use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use serde_json::{Value, json};
use webp_animation::prelude::{Encoder, EncoderOptions, EncodingConfig};

#[cfg(any(windows, target_os = "macos"))]
use super::client_viewport_size;
#[cfg(windows)]
use super::set_capture_probe_phase;
use super::{BridgeServer, BridgeTarget, studio_capture_status, wait_for_player_bridge};
use crate::app::output::automation_token;
use crate::studio::input as input_inject;

static ACTIVE: Mutex<Option<ActiveRecording>> = Mutex::new(None);

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StartRequest {
    player: Option<String>,
    #[serde(default)]
    studio: bool,
    #[serde(default)]
    client: bool,
    output: Option<PathBuf>,
    #[serde(default = "default_fps")]
    fps: f64,
    #[serde(default = "default_max_seconds")]
    max_seconds: f64,
    #[serde(default = "default_quality")]
    quality: f32,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct EndRequest {
    recording_id: String,
}

struct ActiveRecording {
    id: String,
    output: PathBuf,
    target: &'static str,
    window: String,
    stop: Arc<AtomicBool>,
    worker: JoinHandle<Result<FinishedRecording>>,
}

struct FinishedRecording {
    width: u32,
    height: u32,
    frames: usize,
    duration_ms: u64,
}

struct RecordingOptions {
    output: PathBuf,
    fps: f64,
    max_seconds: f64,
    quality: f32,
}

fn default_fps() -> f64 {
    12.0
}

fn default_max_seconds() -> f64 {
    60.0
}

fn default_quality() -> f32 {
    80.0
}

#[cfg(windows)]
fn edit_window(
    bridge: &BridgeServer,
    probe_target: BridgeTarget,
) -> Result<input_inject::StudioWindow> {
    let pid = bridge.studio_pid_for_selector(BridgeTarget::Edit, None)?;
    input_inject::verified_recording_window_for_pid(pid, |phase, colors| {
        set_capture_probe_phase(bridge, probe_target, phase, colors)
    })
}

#[cfg(target_os = "macos")]
fn edit_window(
    bridge: &BridgeServer,
    _probe_target: BridgeTarget,
) -> Result<input_inject::StudioWindow> {
    let pid = bridge.studio_pid_for_selector(BridgeTarget::Edit, None)?;
    input_inject::recording_window_for_pid(pid, None)
}

#[cfg(not(any(windows, target_os = "macos")))]
fn edit_window(
    _bridge: &BridgeServer,
    _probe_target: BridgeTarget,
) -> Result<input_inject::StudioWindow> {
    bail!("Studio recording is only supported on Windows and macOS")
}

#[cfg(any(windows, target_os = "macos"))]
fn client_window(
    bridge: &BridgeServer,
    player: Option<&str>,
) -> Result<input_inject::StudioWindow> {
    let pid = bridge.studio_pid_for_selector(BridgeTarget::Client, player)?;
    input_inject::recording_window_for_pid(pid, client_viewport_size(bridge, player))
}

#[cfg(not(any(windows, target_os = "macos")))]
fn client_window(
    _bridge: &BridgeServer,
    _player: Option<&str>,
) -> Result<input_inject::StudioWindow> {
    bail!("Studio recording is only supported on Windows and macOS")
}

fn record(
    window: input_inject::StudioWindow,
    first: (u32, u32, Vec<u8>),
    options: RecordingOptions,
    stop: Arc<AtomicBool>,
    ready: mpsc::SyncSender<Result<()>>,
) -> Result<FinishedRecording> {
    let (width, height, pixels) = first;
    let encoder_options = EncoderOptions {
        encoding_config: Some(EncodingConfig::new_lossy(options.quality)),
        ..EncoderOptions::default()
    };
    let mut encoder = match Encoder::new_with_options((width, height), encoder_options) {
        Ok(encoder) => encoder,
        Err(error) => {
            let message = format!("Failed to initialize the WebP encoder: {error}");
            let _ = ready.send(Err(anyhow!(message.clone())));
            bail!(message);
        }
    };
    if let Err(error) = encoder.add_frame(&pixels, 0) {
        let message = format!("Failed to encode the first recording frame: {error}");
        let _ = ready.send(Err(anyhow!(message.clone())));
        bail!(message);
    }
    let _ = ready.send(Ok(()));
    let started = Instant::now();
    let interval = Duration::from_secs_f64(1.0 / options.fps);
    let limit = Duration::from_secs_f64(options.max_seconds);
    let mut frames = 1usize;
    let mut last_timestamp = 0i32;
    while !stop.load(Ordering::Relaxed) && started.elapsed() < limit {
        let wait_until = Instant::now() + interval;
        while !stop.load(Ordering::Relaxed) && started.elapsed() < limit {
            let now = Instant::now();
            if now >= wait_until {
                break;
            }
            thread::sleep((wait_until - now).min(Duration::from_millis(10)));
        }
        if stop.load(Ordering::Relaxed) || started.elapsed() >= limit {
            break;
        }
        let (frame_width, frame_height, pixels) = input_inject::capture_window_rgba(&window)?;
        if (frame_width, frame_height) != (width, height) {
            bail!(
                "The recorded window changed from {width}x{height} to {frame_width}x{frame_height}"
            );
        }
        let timestamp = i32::try_from(started.elapsed().as_millis())
            .context("Recording duration exceeded the WebP timestamp range")?
            .max(last_timestamp + 1);
        encoder
            .add_frame(&pixels, timestamp)
            .context("Failed to encode a recording frame")?;
        last_timestamp = timestamp;
        frames += 1;
    }
    let duration_ms = started.elapsed().as_millis().min(u64::MAX as u128) as u64;
    let final_timestamp = i32::try_from(duration_ms)
        .context("Recording duration exceeded the WebP timestamp range")?
        .max(last_timestamp + 1);
    let data = encoder
        .finalize(final_timestamp)
        .context("Failed to finish the WebP recording")?;
    fs::write(&options.output, data.as_ref())
        .with_context(|| format!("Failed to write {}", options.output.display()))?;
    Ok(FinishedRecording {
        width,
        height,
        frames,
        duration_ms,
    })
}

pub(crate) fn start(
    parameters: &Value,
    bridge: &BridgeServer,
    root: &Path,
    wait_seconds: f64,
) -> Result<Value> {
    let request: StartRequest =
        serde_json::from_value(parameters.clone()).context("Invalid record-start payload")?;
    if request.studio && (request.client || request.player.is_some()) {
        bail!("record-start studio cannot be combined with client or player");
    }
    if !request.fps.is_finite() || !(1.0..=30.0).contains(&request.fps) {
        bail!("Invalid record-start fps; expected a number from 1 through 30");
    }
    if !request.max_seconds.is_finite() || !(1.0..=300.0).contains(&request.max_seconds) {
        bail!("Invalid record-start maxSeconds; expected a number from 1 through 300");
    }
    if !request.quality.is_finite() || !(0.0..=100.0).contains(&request.quality) {
        bail!("Invalid record-start quality; expected a number from 0 through 100");
    }
    let mut active = ACTIVE.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(current) = active.as_ref() {
        bail!("Recording conflict: {} is still active", current.id);
    }
    let player = request.player.as_deref();
    if let Some(player) = player {
        wait_for_player_bridge(bridge, player, wait_seconds)?;
    }
    let status = if player.is_none() && !request.client {
        studio_capture_status(bridge)
    } else {
        None
    };
    let simulated = status
        .as_ref()
        .and_then(|value| value.get("simulating"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let client_ready =
        bridge.channel_count_for_target(BridgeTarget::Client) >= bridge.expected_channel_count();
    let use_studio =
        request.studio || (player.is_none() && !request.client && (simulated || !client_ready));
    let probe_target = if simulated && client_ready {
        BridgeTarget::Client
    } else {
        BridgeTarget::Edit
    };
    let (window, target) = if use_studio {
        (edit_window(bridge, probe_target)?, "studio")
    } else {
        (client_window(bridge, player)?, "play-client")
    };
    let id = automation_token("recording");
    let output = request
        .output
        .unwrap_or_else(|| PathBuf::from(format!("{id}.webp")));
    let output = if output.is_absolute() {
        output
    } else {
        root.join(output)
    };
    if !output
        .extension()
        .and_then(|value| value.to_str())
        .is_some_and(|value| value.eq_ignore_ascii_case("webp"))
    {
        bail!("Invalid record-start output; expected a .webp path");
    }
    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create {}", parent.display()))?;
    }
    let first = input_inject::capture_window_rgba(&window)?;
    let window_label = window.label.clone();
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = Arc::clone(&stop);
    let recording_options = RecordingOptions {
        output: output.clone(),
        fps: request.fps,
        max_seconds: request.max_seconds,
        quality: request.quality,
    };
    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    let worker =
        thread::spawn(move || record(window, first, recording_options, thread_stop, ready_tx));
    match ready_rx.recv() {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            let _ = worker.join();
            return Err(error);
        }
        Err(_) => {
            let result = worker
                .join()
                .map_err(|_| anyhow!("Recording worker panicked"))?;
            result?;
            bail!("Recording worker stopped before it was ready");
        }
    }
    *active = Some(ActiveRecording {
        id: id.clone(),
        output: output.clone(),
        target,
        window: window_label,
        stop,
        worker,
    });
    Ok(json!({
        "recordingId": id,
        "path": output.display().to_string(),
        "target": target,
        "fps": request.fps,
        "maxSeconds": request.max_seconds,
        "mimeType": "image/webp",
    }))
}

pub(crate) fn end(parameters: &Value) -> Result<Value> {
    let request: EndRequest =
        serde_json::from_value(parameters.clone()).context("Invalid record-end payload")?;
    let active = {
        let mut slot = ACTIVE.lock().unwrap_or_else(PoisonError::into_inner);
        let recording = slot
            .take()
            .context("Recording conflict: no recording is active")?;
        if recording.id != request.recording_id {
            let current = recording.id.clone();
            *slot = Some(recording);
            bail!(
                "Recording conflict: {} is active, not {}",
                current,
                request.recording_id
            );
        }
        recording
    };
    active.stop.store(true, Ordering::Relaxed);
    let finished = active
        .worker
        .join()
        .map_err(|_| anyhow!("Recording worker panicked"))??;
    Ok(json!({
        "recordingId": active.id,
        "path": active.output.display().to_string(),
        "target": active.target,
        "window": active.window,
        "width": finished.width,
        "height": finished.height,
        "frames": finished.frames,
        "durationMs": finished.duration_ms,
        "mimeType": "image/webp",
        "audio": false,
    }))
}
