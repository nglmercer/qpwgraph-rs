//! Persistent connection sets and activation policies.
//!
//! The native qpwgraph format is XML and resolves rules by node/port names.
//! JSON remains supported as a convenient machine-readable format for tooling
//! and for compatibility with the first Rust prototype.

use super::error::PatchbayError;
use super::model::{ActivationReport, Patchbay, PatchbayResolution};
use pw_graph_backend::{BackendError, GraphDriver};
use pw_graph_core::PortKey;

/// Undo the complete mutation made by one activation. Created links are
/// removed first, in reverse order; links removed by exclusive or
/// auto-disconnect policy are then restored, also in reverse order. Both loops
/// deliberately continue after an error so the final report says which side
/// of the original graph remains stranded.
fn rollback_activation(
    driver: &mut dyn GraphDriver,
    created_by_activation: &[(PortKey, PortKey)],
    removed_by_activation: &[(PortKey, PortKey)],
    cause: BackendError,
) -> PatchbayError {
    let mut created_links_left = 0;
    for (output, input) in created_by_activation.iter().rev() {
        match driver.disconnect_by_key_if_present_without_suppression(output, input) {
            Ok(Some(_)) => driver.allow_connection(output, input),
            Ok(None) => {}
            Err(_) => created_links_left += 1,
        }
    }

    let mut removed_links_not_restored = 0;
    for (output, input) in removed_by_activation.iter().rev() {
        if driver.connect_by_key_if_missing(output, input).is_err() {
            removed_links_not_restored += 1;
        }
    }
    if created_links_left == 0 && removed_links_not_restored == 0 {
        PatchbayError::Backend(cause)
    } else {
        PatchbayError::ActivationNotRolledBack {
            cause: cause.to_string(),
            created_links_left,
            removed_links_not_restored,
        }
    }
}

/// Record a successful removal in the activation transaction. A route that
/// this same activation created and then removed is not part of the original
/// graph, so it must be forgotten from the created set rather than restored as
/// though it had existed before activation.
fn record_activation_removal(
    removed_by_activation: &mut Vec<(PortKey, PortKey)>,
    created_by_activation: &mut Vec<(PortKey, PortKey)>,
    output: PortKey,
    input: PortKey,
) {
    if let Some(index) = created_by_activation
        .iter()
        .position(|(created_output, created_input)| {
            created_output == &output && created_input == &input
        })
    {
        created_by_activation.remove(index);
    } else {
        removed_by_activation.push((output, input));
    }
}

impl Patchbay {
    /// Connect all saved edges. Name-based rules are resolved against the
    /// current registry snapshot, allowing IDs to change between sessions.
    /// Backend mutations are atomic: a fatal connect/disconnect error rolls
    /// back every mutation made by this invocation. Unresolved rules are
    /// skipped as before and are not mutations.
    pub fn activate(
        &self,
        driver: &mut dyn GraphDriver,
        exclusive: bool,
        auto_disconnect: bool,
    ) -> Result<ActivationReport, PatchbayError> {
        driver.refresh()?;
        let mut report = ActivationReport::default();
        let capabilities = driver.capabilities();
        if !self.connections.is_empty() && !capabilities.connect {
            report
                .failed
                .push("connection activation is not supported by this backend".into());
            return Ok(report);
        }
        if (exclusive || auto_disconnect) && !capabilities.disconnect {
            report
                .failed
                .push("connection removal is not supported by this backend".into());
            return Ok(report);
        }
        let mut resolved = Vec::new();
        for (rule_index, _) in self.connections.iter().enumerate() {
            match self.resolve_rule(driver.graph(), rule_index) {
                PatchbayResolution::Resolved { output, input } => {
                    let Some(output) = driver.graph().port_key(output) else {
                        continue;
                    };
                    let Some(input) = driver.graph().port_key(input) else {
                        continue;
                    };
                    resolved.push((output, input));
                }
                PatchbayResolution::WaitingForEndpoint { detail } => {
                    report.waiting.push(format!("rule {rule_index}: {detail}"));
                }
                PatchbayResolution::Ambiguous { detail } => {
                    report
                        .ambiguous
                        .push(format!("rule {rule_index}: {detail}"));
                }
            }
        }

        let mut removed_by_activation: Vec<(PortKey, PortKey)> = Vec::new();
        let mut created_by_activation: Vec<(PortKey, PortKey)> = Vec::new();

        // Exclusive mode is only safe once every saved selector has resolved.
        // A dynamic application can legitimately be between its node and port
        // registry events; removing the currently-live links from that partial
        // snapshot would make a transient disappearance destructive.
        if exclusive && report.waiting.is_empty() && report.ambiguous.is_empty() {
            let live: Vec<_> = driver
                .graph()
                .links
                .values()
                // Observed relationships are not the patchbay's to remove.
                // A composite backend can expose immutable Core Audio session
                // links next to mutable MIDI ones and still report that it
                // supports disconnection overall.
                .filter(|link| driver.is_link_mutable(link.id))
                .filter_map(|link| {
                    Some((
                        driver.graph().port_key(link.output_port)?,
                        driver.graph().port_key(link.input_port)?,
                    ))
                })
                .collect();
            for (live_output, live_input) in live {
                let saved = resolved
                    .iter()
                    .any(|(output, input)| output == &live_output && input == &live_input);
                if saved {
                    continue;
                }
                match driver
                    .disconnect_by_key_if_present_without_suppression(&live_output, &live_input)
                {
                    Ok(Some(_)) => {
                        report.disconnected += 1;
                        record_activation_removal(
                            &mut removed_by_activation,
                            &mut created_by_activation,
                            live_output,
                            live_input,
                        );
                    }
                    Ok(None) => {}
                    Err(error) => {
                        return Err(rollback_activation(
                            driver,
                            &created_by_activation,
                            &removed_by_activation,
                            error,
                        ));
                    }
                }
            }
        }

        for (output, input) in resolved {
            if driver.graph().find_link_by_keys(&output, &input).is_some() {
                report.already_present += 1;
                continue;
            }

            if auto_disconnect {
                let Some(input_port) = driver.graph().resolve_port_key(&input) else {
                    continue;
                };
                let stale: Vec<(PortKey, PortKey)> = driver
                    .graph()
                    .links_for_port(input_port)
                    .filter(|link| link.input_port == input_port)
                    .filter(|link| driver.is_link_mutable(link.id))
                    .filter_map(|link| {
                        Some((
                            driver.graph().port_key(link.output_port)?,
                            driver.graph().port_key(link.input_port)?,
                        ))
                    })
                    .collect();
                for (stale_output, stale_input) in stale {
                    match driver.disconnect_by_key_if_present_without_suppression(
                        &stale_output,
                        &stale_input,
                    ) {
                        Ok(Some(_)) => {
                            report.disconnected += 1;
                            record_activation_removal(
                                &mut removed_by_activation,
                                &mut created_by_activation,
                                stale_output,
                                stale_input,
                            );
                        }
                        Ok(None) => {}
                        Err(error) => {
                            return Err(rollback_activation(
                                driver,
                                &created_by_activation,
                                &removed_by_activation,
                                error,
                            ));
                        }
                    }
                }
            }

            match driver.connect_by_key_if_missing(&output, &input) {
                Ok(Some(_)) => {
                    report.connected += 1;
                    created_by_activation.push((output, input));
                }
                Ok(None) => report.already_present += 1,
                Err(error) => {
                    return Err(rollback_activation(
                        driver,
                        &created_by_activation,
                        &removed_by_activation,
                        error,
                    ));
                }
            }
        }
        Ok(report)
    }
}
