//! Recorder graph actions and the small amount of UI state needed around the
//! backend's asynchronous writer.

use super::app::Application;
use pw_graph_backend::{
    AudioFormat, RecorderCreateRequest, RecorderId, RecorderInstance, RecorderResult,
    RecorderState, RecorderStatus,
};
use pw_graph_config::{config_dir, RecordingSaveMode};
use pw_graph_core::NodeId;
use rfd::FileDialog;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

const APP_NAME: &str = "qpwgraph-rs";

/// The pending directory lives below the application's normal data/config
/// directory when the user has not selected a save folder yet. A configured
/// folder is also passed to the backend so a crash leaves the `.part` beside
/// the user's recordings rather than in an arbitrary process directory.
pub(crate) fn recording_directory(application: &Application) -> PathBuf {
    application
        .config
        .recording_dir
        .clone()
        .unwrap_or_else(|| config_dir(APP_NAME).join("recordings"))
}

fn format_from_config(application: &Application) -> Result<AudioFormat, String> {
    if application.config.recording_format.trim().is_empty()
        || application.config.recording_format == "wav-f32"
    {
        // The first format is deliberately fixed. Keeping this validation at
        // the UI boundary makes a hand-edited future value fail visibly rather
        // than silently recording with a different codec.
        Ok(AudioFormat::new(48_000, 2))
    } else {
        Err(application.tf(
            "status.recorder_format_unsupported",
            &[("format", application.config.recording_format.clone())],
        ))
    }
}

pub(crate) fn create_recorder(application: &mut Application) {
    if !application.source.supports_recorders() {
        application.status = application.t("status.recorders_unavailable");
        return;
    }
    let format = match format_from_config(application) {
        Ok(format) => format,
        Err(error) => {
            application.status = error;
            return;
        }
    };
    let request = RecorderCreateRequest {
        name: "Recorder".into(),
        format,
        position: next_recorder_position(application),
        recording_dir: Some(recording_directory(application)),
        capacity_frames: RecorderCreateRequest::default().capacity_frames,
    };
    match application.source.create_recorder(request) {
        Ok(instance) => {
            let id = instance.id;
            application.recorders.insert(id, instance);
            application.status =
                application.tf("status.recorder_created", &[("id", id.to_string())]);
            application.mark_patchbay_graph_dirty();
        }
        Err(error) => {
            application.status = application.tf("status.recorder_failed", &[("error", error)]);
        }
    }
}

pub(crate) fn choose_recording_directory(application: &mut Application) {
    let directory = application
        .config
        .recording_dir
        .clone()
        .unwrap_or_else(|| recording_directory(application));
    match FileDialog::new().set_directory(directory).pick_folder() {
        Some(path) => {
            application.config.recording_dir = Some(path.clone());
            application.status = application.tf(
                "status.recorder_directory_changed",
                &[("path", path.display().to_string())],
            );
        }
        None => {
            application.status = application.t("status.recorder_directory_cancelled");
        }
    }
}

fn next_recorder_position(application: &Application) -> [f32; 2] {
    let count = application.recorders.len() as f32;
    let x = 260.0 + (count % 4.0) * 280.0;
    let y = 180.0 + (count / 4.0).floor() * 180.0;
    [x, y]
}

pub(crate) fn recorder_id_for_node(
    application: &Application,
    rendered_id: i32,
) -> Option<RecorderId> {
    let node_id = application.view.ids.node_id(rendered_id)?;
    application
        .recorders
        .values()
        .find(|recorder| recorder.node_id == node_id)
        .map(|recorder| recorder.id)
}

pub(crate) fn recorder_statuses_by_node(
    application: &Application,
) -> BTreeMap<NodeId, RecorderStatus> {
    application
        .recorders
        .values()
        .map(|recorder| (recorder.node_id, recorder.status.clone()))
        .collect()
}

/// Refresh non-realtime writer diagnostics. Returns true when a rendered row
/// needs rebuilding, including when an asynchronous stop has completed.
pub(crate) fn poll_recordings(application: &mut Application) -> bool {
    let ids: Vec<_> = application.recorders.keys().copied().collect();
    let mut changed = false;
    for id in ids {
        match application.source.poll_recording(id) {
            Ok(Some(result)) => {
                update_instance_from_result(application, id, &result);
                changed = true;
                if application.pending_recorder_stops.remove(&id) {
                    finish_recording(application, id);
                }
            }
            Ok(None) => {
                if let Ok(status) = application.source.recorder_status(id) {
                    changed |= update_instance_status(application, id, status);
                }
            }
            Err(error) => {
                if let Some(instance) = application.recorders.get_mut(&id) {
                    instance.status.state = RecorderState::Error;
                    instance.status.error = Some(error.clone());
                }
                application.pending_recorder_stops.remove(&id);
                application.status = application.tf("status.recorder_failed", &[("error", error)]);
                changed = true;
            }
        }
    }
    changed
}

pub(crate) fn recovered_recording_rows(application: &Application) -> Vec<String> {
    application
        .recovered_recordings
        .iter()
        .map(|recording| {
            let name = recording
                .path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("Recovered recording");
            let duration = format_elapsed(recording.header.frames, recording.header.sample_rate);
            let size = format_bytes(recording.header.file_bytes);
            application.i18n.format(
                "recorder.recovered_item",
                &[
                    ("name", name.to_owned()),
                    ("duration", duration),
                    ("size", size),
                ],
            )
        })
        .collect()
}

pub(crate) fn close_recovery_dialog(application: &mut Application) {
    application.recovery_dialog_visible = false;
    application.status = application.t("status.recorder_recovery_deferred");
}

pub(crate) fn save_recovered_recording(application: &mut Application, index: usize) {
    let Some(recording) = application.recovered_recordings.get(index).cloned() else {
        return;
    };
    let directory = application
        .config
        .recording_dir
        .clone()
        .unwrap_or_else(|| recording_directory(application));
    let default_name = recording
        .path
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_suffix(".part"))
        .map(str::to_owned)
        .unwrap_or_else(|| "Recovered recording.wav".into());
    let selected = FileDialog::new()
        .set_directory(directory)
        .set_file_name(default_name)
        .add_filter("WAV audio", &["wav"])
        .save_file();
    let Some(destination) = selected.map(ensure_wav_extension) else {
        application.status = application.t("status.recorder_recovery_save_cancelled");
        return;
    };
    match pw_graph_backend::router::copy_recording(&recording.path, &destination) {
        Ok(()) => {
            application
                .recovered_recordings
                .retain(|candidate| candidate.path != recording.path);
            if let Some(parent) = destination.parent() {
                application.config.recording_dir = Some(parent.to_owned());
            }
            application.status = application.tf(
                "status.recorder_saved",
                &[("path", destination.display().to_string())],
            );
            application.recovery_dialog_visible = !application.recovered_recordings.is_empty();
        }
        Err(error) => {
            application.status = application.tf(
                "status.recorder_save_failed",
                &[("error", error.to_string())],
            );
        }
    }
}

pub(crate) fn discard_recovered_recording(application: &mut Application, index: usize) {
    let Some(recording) = application.recovered_recordings.get(index).cloned() else {
        return;
    };
    match fs::remove_file(&recording.path) {
        Ok(()) => {
            application
                .recovered_recordings
                .retain(|candidate| candidate.path != recording.path);
            application.status = application.t("status.recorder_discarded");
            application.recovery_dialog_visible = !application.recovered_recordings.is_empty();
        }
        Err(error) => {
            application.status =
                application.tf("status.recorder_failed", &[("error", error.to_string())]);
        }
    }
}

pub(crate) fn record_for_node(application: &mut Application, rendered_id: i32) {
    let Some(id) = recorder_id_for_node(application, rendered_id) else {
        return;
    };
    match application.source.start_recording(id) {
        Ok(()) => {
            if let Some(instance) = application.recorders.get_mut(&id) {
                instance.status.state = RecorderState::Recording;
                instance.status.error = None;
            }
            application.status =
                application.tf("status.recorder_started", &[("id", id.to_string())]);
        }
        Err(error) => {
            if let Some(instance) = application.recorders.get_mut(&id) {
                instance.status.state = RecorderState::Error;
                instance.status.error = Some(error.clone());
            }
            application.status = application.tf("status.recorder_failed", &[("error", error)]);
        }
    }
}

pub(crate) fn stop_for_node(application: &mut Application, rendered_id: i32) {
    let Some(id) = recorder_id_for_node(application, rendered_id) else {
        return;
    };
    if application.pending_recorder_stops.contains(&id) {
        return;
    }
    match application.source.request_stop_recording(id) {
        Ok(()) => {
            application.pending_recorder_stops.insert(id);
            application.status =
                application.tf("status.recorder_stopping", &[("id", id.to_string())]);
        }
        Err(error) => {
            application.status = application.tf("status.recorder_failed", &[("error", error)]);
        }
    }
}

pub(crate) fn save_for_node(application: &mut Application, rendered_id: i32) {
    let Some(id) = recorder_id_for_node(application, rendered_id) else {
        return;
    };
    save_recording(application, id);
}

pub(crate) fn discard_for_node(application: &mut Application, rendered_id: i32) {
    let Some(id) = recorder_id_for_node(application, rendered_id) else {
        return;
    };
    discard_recording(application, id);
}

fn finish_recording(application: &mut Application, id: RecorderId) {
    if matches!(
        RecordingSaveMode::parse(&application.config.recording_save_mode),
        RecordingSaveMode::AutoSave
    ) {
        let Some(directory) = application.config.recording_dir.clone() else {
            application.status = application.t("status.recorder_directory_required");
            return;
        };
        let stem = pw_graph_backend::router::render_recording_filename(
            &application.config.recording_filename_template,
            SystemTime::now(),
        );
        match pw_graph_backend::router::unique_recording_path(&directory, &stem, "wav") {
            Ok(path) => {
                if let Err(error) = save_recording_to(application, id, &path) {
                    application.status =
                        application.tf("status.recorder_save_failed", &[("error", error)]);
                }
            }
            Err(error) => {
                application.status = application.tf(
                    "status.recorder_save_failed",
                    &[("error", error.to_string())],
                );
            }
        }
    } else {
        let directory = application
            .config
            .recording_dir
            .clone()
            .unwrap_or_else(|| recording_directory(application));
        let filename = pw_graph_backend::router::render_recording_filename(
            &application.config.recording_filename_template,
            SystemTime::now(),
        );
        let selected = FileDialog::new()
            .set_directory(directory)
            .set_file_name(format!("{filename}.wav"))
            .add_filter("WAV audio", &["wav"])
            .save_file();
        match selected {
            Some(path) => {
                if let Err(error) = save_recording_to(application, id, &ensure_wav_extension(path))
                {
                    application.status =
                        application.tf("status.recorder_save_failed", &[("error", error)]);
                }
            }
            None => {
                application.status = application.t("status.recorder_save_cancelled");
            }
        }
    }
}

fn ensure_wav_extension(path: PathBuf) -> PathBuf {
    if path.extension().is_some() {
        path
    } else {
        path.with_extension("wav")
    }
}

fn save_recording_to(
    application: &mut Application,
    id: RecorderId,
    destination: &Path,
) -> Result<(), String> {
    let destination = destination.to_owned();
    application.source.save_recording(id, &destination)?;
    if let Some(instance) = application.recorders.get_mut(&id) {
        instance.status.state = RecorderState::Idle;
        instance.status.final_path = Some(destination.clone());
        instance.status.temporary_path = None;
        instance.status.error = None;
    }
    if let Some(parent) = destination.parent() {
        application.config.recording_dir = Some(parent.to_owned());
    }
    application.status = application.tf(
        "status.recorder_saved",
        &[("path", destination.display().to_string())],
    );
    Ok(())
}

fn save_recording(application: &mut Application, id: RecorderId) {
    let Some(default_name) = application
        .recorders
        .get(&id)
        .and_then(|recorder| recorder.status.temporary_path.as_ref())
        .and_then(|path| path.file_name())
        .and_then(|name| name.to_str())
        .map(|name| name.strip_suffix(".part").unwrap_or(name).to_owned())
    else {
        application.status = application.t("status.recorder_no_pending_file");
        return;
    };
    let directory = application
        .config
        .recording_dir
        .clone()
        .unwrap_or_else(|| recording_directory(application));
    let selected = FileDialog::new()
        .set_directory(directory)
        .set_file_name(default_name)
        .add_filter("WAV audio", &["wav"])
        .save_file();
    if let Some(path) = selected {
        if let Err(error) = save_recording_to(application, id, &ensure_wav_extension(path)) {
            application.status = application.tf("status.recorder_save_failed", &[("error", error)]);
        }
    } else {
        application.status = application.t("status.recorder_save_cancelled");
    }
}

fn discard_recording(application: &mut Application, id: RecorderId) {
    match application.source.discard_recording(id) {
        Ok(()) => {
            application.pending_recorder_stops.remove(&id);
            let still_exists = application.recorders.get(&id).is_some_and(|recorder| {
                application.source.graph().node(recorder.node_id).is_some()
            });
            if still_exists {
                if let Some(instance) = application.recorders.get_mut(&id) {
                    instance.status.state = RecorderState::Idle;
                    instance.status.temporary_path = None;
                    instance.status.error = None;
                }
            } else {
                application.recorders.remove(&id);
            }
            application.status = application.t("status.recorder_discarded");
            application.mark_patchbay_graph_dirty();
        }
        Err(error) => {
            application.status = application.tf("status.recorder_failed", &[("error", error)]);
        }
    }
}

fn update_instance_from_result(
    application: &mut Application,
    id: RecorderId,
    result: &RecorderResult,
) {
    if let Some(instance) = application.recorders.get_mut(&id) {
        instance.status.state = RecorderState::Unsaved;
        instance.status.elapsed_frames = result.elapsed_frames;
        instance.status.frames_written = result.frames_written;
        instance.status.dropped_frames = result.dropped_frames;
        instance.status.file_bytes = result.file_bytes;
        instance.status.sample_rate = result.sample_rate;
        instance.status.channels = result.channels;
        instance.status.temporary_path = Some(result.temporary_path.clone());
        instance.status.error = None;
    }
}

fn update_instance_status(
    application: &mut Application,
    id: RecorderId,
    status: RecorderStatus,
) -> bool {
    let Some(instance) = application.recorders.get_mut(&id) else {
        return false;
    };
    if instance.status == status {
        return false;
    }
    instance.status = status;
    true
}

/// Keep the cached instance snapshot useful to the model and to node-removal
/// code. The backend remains authoritative; this is only a UI-side copy.
#[allow(dead_code)]
pub(crate) fn recorder_instance(
    application: &Application,
    id: RecorderId,
) -> Option<&RecorderInstance> {
    application.recorders.get(&id)
}

/// Used by tests and future recovery UI to turn a status into a stable label.
pub(crate) fn recorder_state_slug(state: RecorderState) -> &'static str {
    match state {
        RecorderState::Idle => "idle",
        RecorderState::Recording => "recording",
        RecorderState::Unsaved => "unsaved",
        RecorderState::Error => "error",
    }
}

fn format_elapsed(frames: u64, sample_rate: u32) -> String {
    if sample_rate == 0 {
        return "00:00:00".into();
    }
    let seconds = frames / u64::from(sample_rate);
    format!(
        "{:02}:{:02}:{:02}",
        seconds / 3_600,
        (seconds % 3_600) / 60,
        seconds % 60
    )
}

fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}
