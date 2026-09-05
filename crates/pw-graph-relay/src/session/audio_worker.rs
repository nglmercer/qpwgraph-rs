//! The audio workers. `run_rx`/`run_tcp_rx` converge on `run_rx_source` and the
//! transmit paths on `run_tx_source`, so the datagram handling is written once
//! and the transport only decides how bytes arrive and leave.

use super::*;

/// Build this session's transmit converter with every buffer already grown
/// for the largest quantum a realtime callback can present.
///
/// `broadcast_capture` runs on the PipeWire process thread. A converter built
/// by `Converter::new` allocates inside its first `convert` — and again on any
/// quantum larger than one it has already seen — which is exactly what the
/// realtime contract forbids. Sizing here, on the session-setup thread, is
/// what makes `try_push_capture` allocation-free from the very first callback
/// rather than only "after warm-up".
pub(super) fn prepared_capture_converter(
    local: AudioFormat,
    wire: AudioFormat,
) -> (Converter, Vec<f32>) {
    let max_input = crate::MAX_REALTIME_QUANTUM_SAMPLES;
    let converter = Converter::with_capacity(
        local.sample_rate,
        local.channels,
        wire.sample_rate,
        wire.channels,
        max_input,
    );
    // The identity path pushes `samples` straight through and never touches
    // this buffer, but a geometry change writes the full converted quantum
    // into it, so it is sized for the worst supported expansion.
    let out = Vec::with_capacity(converter.output_capacity_for(max_input));
    (converter, out)
}

/// Whether an authenticated audio packet's header agrees with what the
/// session negotiated.
///
/// Only applies to packets carrying audio: an announce packet's header is
/// filler (see [`crate::audio::announce_packet`]) and is filtered out by its
/// empty payload before this is consulted.
pub(super) fn packet_matches_negotiation(
    packet: &AudioPacket<'_>,
    codec: CodecKind,
    format: AudioFormat,
) -> bool {
    packet.codec == codec && packet.stereo == format.is_stereo()
}

pub(super) fn validate_negotiation(
    codec: CodecKind,
    frame_ms: u16,
    sample_rate: u32,
    channels: u16,
) -> RelayResult<()> {
    if !is_supported_frame_ms(frame_ms) {
        return Err(RelayError::Protocol(format!(
            "unsupported frame duration {frame_ms} ms"
        )));
    }
    if !crate::is_supported_sample_rate(sample_rate) {
        return Err(RelayError::Protocol(format!(
            "unsupported sample rate {sample_rate} Hz"
        )));
    }
    if !crate::is_supported_channels(channels) {
        return Err(RelayError::Protocol(format!(
            "unsupported channel count {channels}"
        )));
    }
    let _ = codec; // both Pcm and Opus are supported
    Ok(())
}

pub(super) fn is_timeout(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
    )
}

pub(super) fn is_recoverable_tcp_audio_error(error: &std::io::Error) -> bool {
    let kind_is_recoverable = matches!(
        error.kind(),
        std::io::ErrorKind::BrokenPipe
            | std::io::ErrorKind::ConnectionReset
            | std::io::ErrorKind::ConnectionAborted
            | std::io::ErrorKind::NotConnected
            | std::io::ErrorKind::UnexpectedEof
            | std::io::ErrorKind::TimedOut
            | std::io::ErrorKind::WouldBlock
    );

    // Winsock reports a local `shutdown(SD_BOTH)` as WSAESHUTDOWN (10058),
    // which Rust currently exposes as `ErrorKind::Other`.  It is still a
    // replaceable transport failure, not a fatal protocol error; treating it
    // as recoverable keeps the reconnect path consistent with BrokenPipe and
    // ConnectionReset on the other platforms.
    kind_is_recoverable || cfg!(windows) && error.raw_os_error() == Some(10058)
}

/// Receive loop: datagrams → authenticate → jitter buffer → decoder →
/// convert to the local format → this session's playback queue.
///
/// Every fatal error tears the session down. Leaving the session registered
/// after its audio path has permanently died — as an earlier version did for
/// socket and encoder failures — makes the engine report a healthy, silent
/// connection that will never carry audio again.
pub(super) fn run_rx(
    inner: Arc<EngineInner>,
    record: Arc<SessionRecord>,
    socket: Arc<UdpAudioSlot>,
    host_side: bool,
    ready: Option<SyncSender<Result<(), String>>>,
) {
    run_rx_source(inner, record, host_side, ready, move |datagram| {
        let Some(socket) = socket.current() else {
            return Ok(None);
        };
        match socket.recv_from(datagram) {
            Ok((len, addr)) => Ok(Some((len, Some(addr)))),
            Err(error) if is_timeout(&error) => Ok(None),
            Err(error) => Err(error),
        }
    });
}

pub(super) fn read_tcp_audio_frame(
    stream: &mut impl Read,
    output: &mut [u8],
) -> std::io::Result<usize> {
    let mut length = [0u8; 4];
    stream.read_exact(&mut length)?;
    let length = u32::from_le_bytes(length) as usize;
    if length == 0 || length > output.len() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "TCP audio frame is out of range",
        ));
    }
    stream.read_exact(&mut output[..length])?;
    Ok(length)
}

pub(super) fn write_tcp_audio_frame(
    stream: &mut impl Write,
    datagram: &[u8],
) -> std::io::Result<()> {
    if datagram.is_empty() || datagram.len() > MAX_DATAGRAM {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "TCP audio frame is out of range",
        ));
    }
    let length = u32::try_from(datagram.len()).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "TCP audio frame is too large",
        )
    })?;
    stream.write_all(&length.to_le_bytes())?;
    stream.write_all(datagram)?;
    stream.flush()
}

/// authenticated control session. Only this one supervisor may dial for a
/// session; the slot gate also serializes a control-resume race.
pub(super) fn run_tcp_audio_supervisor(
    inner: Arc<EngineInner>,
    record: Arc<SessionRecord>,
    audio: Arc<TcpAudioSlot>,
    target: SocketAddr,
) {
    const MAX_FAILURES: u32 = 8;
    let mut failures = 0u32;
    let mut backoff = Duration::from_millis(250);
    loop {
        if !inner.session_alive(record.id) || record.stop.load(Ordering::Relaxed) {
            return;
        }
        if audio.current().is_some() {
            failures = 0;
            backoff = Duration::from_millis(250);
            std::thread::sleep(Duration::from_millis(250));
            continue;
        }
        match open_tcp_audio(&inner, &record, &audio, target) {
            Ok(()) => {
                failures = 0;
                backoff = Duration::from_millis(250);
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(error) => {
                failures = failures.saturating_add(1);
                if failures >= MAX_FAILURES {
                    let reason = if error.kind() == std::io::ErrorKind::PermissionDenied {
                        "ADB audio authentication was rejected; recreate forwarding and pair again"
                    } else {
                        "ADB audio forwarding is not reachable; create the adb reverse/forward rule and retry"
                    };
                    inner.emit(RelayEvent::Error {
                        message: reason.into(),
                    });
                    teardown(&inner, record.id, reason.into());
                    return;
                }
                std::thread::sleep(backoff);
                backoff = (backoff * 2).min(Duration::from_secs(4));
            }
        }
    }
}

pub(super) fn run_tcp_rx(
    inner: Arc<EngineInner>,
    record: Arc<SessionRecord>,
    audio: Arc<TcpAudioSlot>,
    ready: Option<SyncSender<Result<(), String>>>,
) {
    let source_record = Arc::clone(&record);
    run_rx_source(inner, record, false, ready, move |datagram| loop {
        let Some(connection) = audio.wait(&source_record.stop) else {
            return Ok(None);
        };
        let result = connection
            .reader
            .lock()
            .map_err(|_| std::io::Error::other("TCP audio reader is poisoned"))
            .and_then(|mut reader| read_tcp_audio_frame(&mut *reader, datagram));
        match result {
            Ok(len) => return Ok(Some((len, None))),
            Err(error) if is_timeout(&error) => return Ok(None),
            Err(_) => {
                audio.clear(&connection);
                continue;
            }
        }
    });
}

pub(super) fn run_rx_source(
    inner: Arc<EngineInner>,
    record: Arc<SessionRecord>,
    host_side: bool,
    ready: Option<SyncSender<Result<(), String>>>,
    mut receive: impl FnMut(&mut [u8]) -> std::io::Result<Option<(usize, Option<SocketAddr>)>>,
) {
    request_realtime_thread();
    let local = inner.config().local_format();
    // Bound what a stalled playback consumer can turn into standing delay:
    // decoded audio waiting here is latency, not safety. The depth is this
    // session's own, in local-format samples.
    let local_frame_samples = (local.sample_rate as usize / 1000)
        * record.format.frame_ms as usize
        * local.channels as usize;
    let incoming_frame = local_frame_samples.max(1);
    record.incoming.set_frame_align(incoming_frame);
    record
        .incoming
        .set_target_depth(incoming_frame * crate::PLAYBACK_DEPTH_FRAMES);
    let mut decoder = match make_decoder(record.codec, record.format) {
        Ok(decoder) => decoder,
        Err(error) => {
            let reason = format!("decoder init failed: {error}");
            report_worker_startup(ready, Err(reason.clone()));
            fail_session(&inner, &record, reason);
            return;
        }
    };
    // Decode output is exactly one frame per call, so the receive converter
    // only ever needs that much. Sizing it here keeps the steady state free of
    // reallocation even though this thread is not itself realtime.
    let mut converter = Converter::with_capacity(
        record.format.sample_rate,
        record.format.channels,
        local.sample_rate,
        local.channels,
        record.format.frame_samples(),
    );

    let mut jitter = JitterBuffer::new(JITTER_DEPTH_FRAMES);
    let mut datagram = vec![0u8; MAX_DATAGRAM];
    let mut frame_buf = vec![0.0f32; record.format.frame_samples()];
    let mut converted =
        Vec::with_capacity(converter.output_capacity_for(record.format.frame_samples()));
    report_worker_startup(ready, Ok(()));
    let mut sumsq = 0.0f64;
    let mut level_samples = 0usize;
    let mut frames_since_level = 0u32;

    loop {
        if !inner.session_alive(record.id) {
            break;
        }
        if !record.local_roles().receive {
            // A flow switch can turn a receive path off while the worker is
            // blocked in a transport read. The transport functions all have
            // bounded waits, so this gate becomes visible promptly without
            // consuming packets into the old route.
            std::thread::sleep(Duration::from_millis(10));
            continue;
        }
        let Some((len, addr)) = (match receive(&mut datagram) {
            Ok(result) => result,
            Err(error) => {
                fail_session(&inner, &record, format!("audio socket failed: {error}"));
                return;
            }
        }) else {
            continue;
        };
        let Some(packet) = AudioPacket::parse(&datagram[..len]) else {
            continue;
        };
        // Authenticate *before* anything else observes the datagram. A packet
        // that does not open never reaches the address bookkeeping, the
        // jitter buffer, or the decoder, so a stranger who can reach this
        // port cannot inject audio or redirect ours.
        let payload = {
            let Ok(mut opener) = record.audio_opener.lock() else {
                break;
            };
            match packet.open(&mut opener) {
                Ok(payload) => payload,
                Err(_) => continue,
            }
        };

        if host_side {
            // Keep the peer's audio address current: after a link roam the
            // client may return from a different source address. Only an
            // authenticated datagram gets to move it.
            if let Some(addr) = addr {
                if let Ok(mut slot) = record.peer_audio_addr.lock() {
                    if *slot != Some(addr) {
                        *slot = Some(addr);
                    }
                }
            }
        }
        if payload.is_empty() {
            // An announce packet: address bookkeeping only. Its header carries
            // no meaningful geometry (the sender hardcodes mono), so the
            // metadata check below deliberately sits after this.
            continue;
        }
        // Authentication proves *who* sent the datagram, not that they are
        // still speaking the format this session negotiated. A paired but
        // buggy or hostile peer that flips its codec id or stereo flag
        // mid-stream would otherwise feed the decoder and the jitter buffer
        // frames they cannot interpret — a decode error per packet at best,
        // and silently mis-framed audio at worst. The negotiated format is
        // the authority; a packet that disagrees with it is dropped before
        // it reaches any stateful audio machinery.
        if !packet_matches_negotiation(&packet, record.codec, record.format) {
            continue;
        }
        if packet.keyframe {
            jitter.set_anchor(packet.sequence);
        }
        if !jitter.push(packet.sequence, payload) {
            continue;
        }

        loop {
            let decoded = match jitter.pop() {
                JitterPop::Buffering => break,
                JitterPop::Frame(payload) => match decoder.decode(&payload, &mut frame_buf) {
                    Ok(samples) => {
                        for sample in &frame_buf[..samples] {
                            sumsq += (*sample as f64) * (*sample as f64);
                        }
                        level_samples += samples;
                        Some(samples)
                    }
                    Err(error) => {
                        inner.emit(RelayEvent::Error {
                            message: format!("audio decode failed: {error}"),
                        });
                        None
                    }
                },
                JitterPop::Lost => decoder.conceal(&mut frame_buf).ok(),
            };
            let Some(samples) = decoded else {
                continue;
            };
            converter.convert(&frame_buf[..samples], &mut converted);
            record.incoming.push(&converted);
        }

        frames_since_level += 1;
        if frames_since_level >= 25 {
            let rms = if level_samples == 0 {
                0.0
            } else {
                (sumsq / level_samples as f64).sqrt() as f32
            };
            inner.emit(RelayEvent::AudioLevel {
                id: record.id,
                rms: rms.min(1.0),
            });
            frames_since_level = 0;
            sumsq = 0.0;
            level_samples = 0;
        }
    }
}

/// Send loop: outgoing queue → encoder → sealed datagrams.
///
/// Pacing comes from the capture side filling the queue in real time. The
/// thread parks on the queue's condvar rather than polling, so a completed
/// frame is encoded and sent the moment the capture callback delivers it
/// instead of up to a poll interval later; the wait timeout exists only so
/// teardown is noticed promptly. The peer address is re-read per frame so a
/// roaming client (new source address) is followed without a restart.
pub(super) fn run_tx(
    inner: Arc<EngineInner>,
    record: Arc<SessionRecord>,
    socket: Arc<UdpAudioSlot>,
    ready: Option<SyncSender<Result<(), String>>>,
) {
    let ready_record = Arc::clone(&record);
    let address_record = Arc::clone(&record);
    run_tx_source(
        inner,
        record,
        ready,
        move || {
            ready_record
                .peer_audio_addr
                .lock()
                .ok()
                .and_then(|slot| *slot)
                .is_some()
        },
        move |datagram| {
            let address = address_record
                .peer_audio_addr
                .lock()
                .ok()
                .and_then(|slot| *slot)
                .ok_or_else(|| {
                    std::io::Error::new(
                        std::io::ErrorKind::WouldBlock,
                        "peer audio address unknown",
                    )
                })?;
            let Some(socket) = socket.current() else {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::WouldBlock,
                    "UDP audio socket is not ready",
                ));
            };
            socket.send_to(datagram, address).map(|_| ())
        },
    );
}

pub(super) fn send_tcp_audio_datagram(
    audio: &Arc<TcpAudioSlot>,
    datagram: &[u8],
) -> std::io::Result<()> {
    let Some(connection) = audio.current() else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::WouldBlock,
            "TCP audio connection is not ready",
        ));
    };
    let result = connection
        .writer
        .lock()
        .map_err(|_| std::io::Error::other("TCP audio writer is poisoned"))
        .and_then(|mut writer| write_tcp_audio_frame(&mut *writer, datagram));
    match result {
        Ok(()) => Ok(()),
        Err(error) if is_recoverable_tcp_audio_error(&error) => {
            // ADB audio is a replaceable secondary stream. Its loss must wake
            // the supervisor without being interpreted by run_tx_source as a
            // fatal session failure.
            audio.clear(&connection);
            Err(std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                "ADB audio connection is reconnecting",
            ))
        }
        Err(error) => Err(error),
    }
}

pub(super) fn run_tcp_tx(
    inner: Arc<EngineInner>,
    record: Arc<SessionRecord>,
    audio: Arc<TcpAudioSlot>,
    ready: Option<SyncSender<Result<(), String>>>,
) {
    let send_audio = Arc::clone(&audio);
    run_tx_source(
        inner,
        record,
        ready,
        move || send_audio.current().is_some(),
        move |datagram| send_tcp_audio_datagram(&audio, datagram),
    );
}

pub(super) fn run_tx_source(
    inner: Arc<EngineInner>,
    record: Arc<SessionRecord>,
    ready: Option<SyncSender<Result<(), String>>>,
    mut transport_ready: impl FnMut() -> bool,
    mut send_datagram: impl FnMut(&[u8]) -> std::io::Result<()>,
) {
    request_realtime_thread();
    // Same reasoning as the receive side: captured audio that cannot be sent
    // promptly is better dropped than delivered late.
    let outgoing_frame = record.format.frame_samples();
    record.outgoing.set_frame_align(outgoing_frame);
    record
        .outgoing
        .set_target_depth(outgoing_frame * crate::CAPTURE_DEPTH_FRAMES);
    let mut encoder = match make_encoder(record.codec, record.format) {
        Ok(encoder) => encoder,
        Err(error) => {
            let reason = format!("encoder init failed: {error}");
            report_worker_startup(ready, Err(reason.clone()));
            fail_session(&inner, &record, reason);
            return;
        }
    };

    let frame_samples = record.format.frame_samples();
    report_worker_startup(ready, Ok(()));
    let mut sequence = 0u32;
    let mut timestamp_ms = 0u32;
    let mut payload = Vec::with_capacity(4096);
    // A send-only session decodes nothing, so the receive path never reports a
    // level for it and its meter would sit at zero however loud the transmitted
    // audio is. Meter the outgoing frames instead, and leave the meter to the
    // receive path whenever this session also receives: both directions share
    // one `AudioLevel` per session and the incoming level is the one the UI
    // documents.
    let mut frames_since_level = 0u32;
    let mut sumsq = 0f64;
    let mut level_samples = 0usize;

    loop {
        if !inner.session_alive(record.id) {
            break;
        }
        if !record.local_roles().emit {
            // Leave the sender worker installed so a later authenticated
            // flow switch can enable it immediately. Do not drain the
            // capture queue while this side is receive-only: the queue is
            // session-owned and must not turn a role switch into stale audio.
            std::thread::sleep(Duration::from_millis(10));
            continue;
        }
        // The host learns the peer address from an authenticated announce;
        // TCP audio waits for the authenticated secondary stream. Either
        // transport can become ready again after a link migration.
        if !transport_ready() {
            std::thread::sleep(Duration::from_millis(10));
            continue;
        }
        let Some(samples) = record
            .outgoing
            .pop_exact_timeout(frame_samples, FRAME_WAIT_TIMEOUT)
        else {
            continue;
        };
        if !record.local_roles().receive {
            for sample in &samples {
                sumsq += (*sample as f64) * (*sample as f64);
            }
            level_samples += samples.len();
            frames_since_level += 1;
            if frames_since_level >= 25 {
                let rms = if level_samples == 0 {
                    0.0
                } else {
                    (sumsq / level_samples as f64).sqrt() as f32
                };
                inner.emit(RelayEvent::AudioLevel {
                    id: record.id,
                    rms: rms.min(1.0),
                });
                frames_since_level = 0;
                sumsq = 0.0;
                level_samples = 0;
            }
        }
        match encoder.encode(&samples, &mut payload) {
            Ok(0) => continue,
            Ok(_) => {
                let header = AudioHeader {
                    stereo: record.format.is_stereo(),
                    keyframe: sequence == 0,
                    codec: record.codec,
                    sequence,
                    timestamp_ms,
                };
                let datagram = {
                    let Ok(mut sealer) = record.audio_sealer.lock() else {
                        fail_session(&inner, &record, "audio sealer is poisoned".into());
                        return;
                    };
                    match seal_datagram(&mut sealer, &header, &payload) {
                        Ok(datagram) => datagram,
                        Err(error) => {
                            fail_session(&inner, &record, format!("audio seal failed: {error}"));
                            return;
                        }
                    }
                };
                if let Err(error) = send_datagram(&datagram) {
                    if is_timeout(&error) {
                        // The datagram was already sealed, so its AEAD
                        // counter was consumed even though the transport
                        // dropped it. Keep the wire timeline monotonic too;
                        // the next frame must not reuse its sequence number.
                        sequence = sequence.wrapping_add(1);
                        timestamp_ms = timestamp_ms.wrapping_add(record.format.frame_ms as u32);
                        continue;
                    }
                    fail_session(&inner, &record, format!("audio send failed: {error}"));
                    return;
                }
                sequence = sequence.wrapping_add(1);
                timestamp_ms = timestamp_ms.wrapping_add(record.format.frame_ms as u32);
            }
            Err(error) => {
                fail_session(&inner, &record, format!("audio encode failed: {error}"));
                return;
            }
        }
    }
}
