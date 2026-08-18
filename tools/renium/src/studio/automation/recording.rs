use std::borrow::Cow;
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use mp4::{AvcConfig, FourCC, Mp4Config, Mp4Sample, Mp4Writer, TrackConfig};
use openh264::OpenH264API;
use openh264::encoder::{
    Complexity, Encoder, EncoderConfig, FrameRate, FrameType, IntraFramePeriod, Profile, QpRange,
    RateControlMode, UsageType, VuiConfig,
};
use openh264::formats::{RgbaSliceU8, YUVBuffer, YUVSource};
use serde::Deserialize;
use serde_json::{Value, json};

#[cfg(windows)]
use super::set_capture_probe_phase;
use super::{BridgeServer, BridgeTarget, studio_capture_status, wait_for_player_bridge};
#[cfg(any(windows, target_os = "macos"))]
use super::{client_viewport_size, recover_client_viewport};
use crate::app::output::automation_token;
use crate::studio::input as input_inject;
use crate::system::files::{
    cleanup_stale_sibling_temps, replace_file_with_backup, sibling_temp_path,
};

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
    recording_id: Option<String>,
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

struct EncodedFrame {
    bytes: Vec<u8>,
    is_sync: bool,
    sps: Option<Vec<u8>>,
    pps: Option<Vec<u8>>,
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
    input_inject::verified_studio_window_for_pid(pid, |phase, colors| {
        set_capture_probe_phase(bridge, probe_target, phase, colors)
    })
}

#[cfg(target_os = "macos")]
fn edit_window(
    bridge: &BridgeServer,
    _probe_target: BridgeTarget,
) -> Result<input_inject::StudioWindow> {
    let pid = bridge.studio_pid_for_selector(BridgeTarget::Edit, None)?;
    input_inject::window_for_pid(pid, None)
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
    let viewport = client_viewport_size(bridge, player);
    let viewport = recover_client_viewport(bridge, player, pid, viewport)?;
    input_inject::window_for_pid(pid, viewport)
}

#[cfg(not(any(windows, target_os = "macos")))]
fn client_window(
    _bridge: &BridgeServer,
    _player: Option<&str>,
) -> Result<input_inject::StudioWindow> {
    bail!("Studio recording is only supported on Windows and macOS")
}

fn even_rgba(pixels: &[u8], width: u32, height: u32) -> Result<(Cow<'_, [u8]>, usize, usize)> {
    let width = usize::try_from(width).context("Recording width is out of range")?;
    let height = usize::try_from(height).context("Recording height is out of range")?;
    let expected = width
        .checked_mul(height)
        .and_then(|value| value.checked_mul(4))
        .context("Recording frame is too large")?;
    if pixels.len() != expected {
        bail!(
            "Recording frame has {} bytes; expected {expected}",
            pixels.len()
        );
    }
    let even_width = width & !1;
    let even_height = height & !1;
    if even_width == 0 || even_height == 0 {
        bail!("Recording window is too small");
    }
    if (even_width, even_height) == (width, height) {
        return Ok((Cow::Borrowed(pixels), width, height));
    }
    let row_bytes = width * 4;
    let encoded_row_bytes = even_width * 4;
    let mut cropped = Vec::with_capacity(encoded_row_bytes * even_height);
    for row in pixels.chunks_exact(row_bytes).take(even_height) {
        cropped.extend_from_slice(&row[..encoded_row_bytes]);
    }
    Ok((Cow::Owned(cropped), even_width, even_height))
}

fn annex_b_payload(nal: &[u8]) -> Result<&[u8]> {
    match nal {
        [0, 0, 0, 1, payload @ ..] | [0, 0, 1, payload @ ..] if !payload.is_empty() => Ok(payload),
        _ => bail!("H.264 encoder returned a malformed NAL unit"),
    }
}

fn encode_frame(encoder: &mut Encoder, yuv: &mut YUVBuffer, rgba: &[u8]) -> Result<EncodedFrame> {
    let dimensions = yuv.dimensions();
    yuv.read_rgba8(RgbaSliceU8::new(rgba, dimensions));
    let bitstream = encoder
        .encode(yuv)
        .context("Failed to encode a recording frame")?;
    let mut bytes = Vec::new();
    let mut sps = None;
    let mut pps = None;
    for layer_index in 0..bitstream.num_layers() {
        let layer = bitstream
            .layer(layer_index)
            .context("H.264 encoder omitted a layer")?;
        for nal_index in 0..layer.nal_count() {
            let payload = annex_b_payload(
                layer
                    .nal_unit(nal_index)
                    .context("H.264 encoder omitted a NAL unit")?,
            )?;
            match payload[0] & 0x1f {
                7 => sps = Some(payload.to_vec()),
                8 => pps = Some(payload.to_vec()),
                _ => {
                    let length = u32::try_from(payload.len())
                        .context("Encoded recording frame is too large")?;
                    bytes.extend_from_slice(&length.to_be_bytes());
                    bytes.extend_from_slice(payload);
                }
            }
        }
    }
    if bytes.is_empty() {
        bail!("H.264 encoder returned an empty frame");
    }
    Ok(EncodedFrame {
        bytes,
        is_sync: matches!(bitstream.frame_type(), FrameType::IDR | FrameType::I),
        sps,
        pps,
    })
}

fn fourcc(value: &str) -> Result<FourCC> {
    value
        .parse()
        .with_context(|| format!("Invalid MP4 brand {value}"))
}

fn write_sample(
    writer: &mut Mp4Writer<BufWriter<File>>,
    frame: EncodedFrame,
    start_time: u64,
    end_time: u64,
) -> Result<()> {
    let duration = u32::try_from(end_time.saturating_sub(start_time).max(1))
        .context("Recording sample duration is out of range")?;
    writer
        .write_sample(
            1,
            &Mp4Sample {
                start_time,
                duration,
                rendering_offset: 0,
                is_sync: frame.is_sync,
                bytes: frame.bytes.into(),
            },
        )
        .context("Failed to write an MP4 frame")
}

fn record_file(
    window: &input_inject::StudioWindow,
    first: (u32, u32, Vec<u8>),
    options: &RecordingOptions,
    stop: &AtomicBool,
    ready: mpsc::SyncSender<Result<()>>,
    temp_path: &Path,
) -> Result<FinishedRecording> {
    let (source_width, source_height, first_pixels) = first;
    let (first_pixels, width, height) = even_rgba(&first_pixels, source_width, source_height)?;
    let quality = (51.0 - options.quality * 0.41).round().clamp(10.0, 51.0) as u8;
    let frame_rate = options.fps as f32;
    let intra_period = (options.fps.round() as u32).saturating_mul(2).max(1);
    let config = EncoderConfig::new()
        .max_frame_rate(FrameRate::from_hz(frame_rate))
        .usage_type(UsageType::ScreenContentRealTime)
        .rate_control_mode(RateControlMode::Quality)
        .skip_frames(false)
        .profile(Profile::Baseline)
        .complexity(Complexity::Low)
        .qp(QpRange::new(quality, quality))
        .intra_frame_period(IntraFramePeriod::from_num_frames(intra_period))
        .vui(VuiConfig::bt709_full());
    let mut encoder = Encoder::with_api_config(OpenH264API::from_source(), config)
        .context("Failed to initialize the H.264 encoder")?;
    let mut yuv = YUVBuffer::new(width, height);
    let mut pending = encode_frame(&mut encoder, &mut yuv, &first_pixels)?;
    let sps = pending
        .sps
        .take()
        .context("H.264 encoder omitted the sequence parameters")?;
    let pps = pending
        .pps
        .take()
        .context("H.264 encoder omitted the picture parameters")?;
    let width = u16::try_from(width).context("Recording width exceeds the MP4 limit")?;
    let height = u16::try_from(height).context("Recording height exceeds the MP4 limit")?;
    let file = File::create(temp_path)
        .with_context(|| format!("Failed to create {}", temp_path.display()))?;
    let mut writer = Mp4Writer::write_start(
        BufWriter::new(file),
        &Mp4Config {
            major_brand: fourcc("isom")?,
            minor_version: 512,
            compatible_brands: vec![
                fourcc("isom")?,
                fourcc("iso2")?,
                fourcc("avc1")?,
                fourcc("mp41")?,
            ],
            timescale: 1000,
        },
    )
    .context("Failed to initialize the MP4 file")?;
    writer
        .add_track(&TrackConfig::from(AvcConfig {
            width,
            height,
            seq_param_set: sps,
            pic_param_set: pps,
        }))
        .context("Failed to initialize the MP4 video track")?;
    let _ = ready.send(Ok(()));
    let started = Instant::now();
    let interval = Duration::from_secs_f64(1.0 / options.fps);
    let limit = Duration::from_secs_f64(options.max_seconds);
    let mut next_frame = started + interval;
    let mut pending_at = 0u64;
    let mut frames = 1usize;
    while !stop.load(Ordering::Relaxed) && started.elapsed() < limit {
        while !stop.load(Ordering::Relaxed) && started.elapsed() < limit {
            let now = Instant::now();
            if now >= next_frame {
                break;
            }
            thread::sleep((next_frame - now).min(Duration::from_millis(10)));
        }
        if stop.load(Ordering::Relaxed) || started.elapsed() >= limit {
            break;
        }
        let (frame_width, frame_height, pixels) = input_inject::capture_window_rgba(window)?;
        if (frame_width, frame_height) != (source_width, source_height) {
            bail!(
                "The recorded window changed from {source_width}x{source_height} to {frame_width}x{frame_height}"
            );
        }
        let (pixels, frame_width, frame_height) = even_rgba(&pixels, frame_width, frame_height)?;
        if (frame_width, frame_height) != (usize::from(width), usize::from(height)) {
            bail!("The encoded recording dimensions changed");
        }
        let timestamp = started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
        let next = encode_frame(&mut encoder, &mut yuv, &pixels)?;
        write_sample(&mut writer, pending, pending_at, timestamp)?;
        pending = next;
        pending_at = timestamp;
        frames += 1;
        next_frame += interval;
        while next_frame <= Instant::now() {
            next_frame += interval;
        }
    }
    let duration_ms = started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
    write_sample(&mut writer, pending, pending_at, duration_ms)?;
    writer
        .write_end()
        .context("Failed to finish the MP4 file")?;
    let mut file = writer.into_writer();
    file.flush()
        .with_context(|| format!("Failed to write {}", temp_path.display()))?;
    file.get_ref()
        .sync_all()
        .with_context(|| format!("Failed to write {}", temp_path.display()))?;
    drop(file);
    replace_file_with_backup(temp_path, &options.output, "recording")?;
    Ok(FinishedRecording {
        width: u32::from(width),
        height: u32::from(height),
        frames,
        duration_ms,
    })
}

fn record(
    window: input_inject::StudioWindow,
    first: (u32, u32, Vec<u8>),
    options: RecordingOptions,
    stop: Arc<AtomicBool>,
    ready: mpsc::SyncSender<Result<()>>,
) -> Result<FinishedRecording> {
    cleanup_stale_sibling_temps(&options.output);
    let temp_path = sibling_temp_path(&options.output);
    let result = record_file(&window, first, &options, &stop, ready, &temp_path);
    if result.is_err() {
        let _ = fs::remove_file(temp_path);
    }
    result
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
        .unwrap_or_else(|| PathBuf::from(format!("{id}.mp4")));
    let output = if output.is_absolute() {
        output
    } else {
        root.join(output)
    };
    if !output
        .extension()
        .and_then(|value| value.to_str())
        .is_some_and(|value| value.eq_ignore_ascii_case("mp4"))
    {
        bail!("Invalid record-start output; expected a .mp4 path");
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
        "mimeType": "video/mp4",
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
        if request
            .recording_id
            .as_ref()
            .is_some_and(|id| recording.id != *id)
        {
            let current = recording.id.clone();
            let requested = request.recording_id.as_deref().unwrap_or_default();
            *slot = Some(recording);
            bail!(
                "Recording conflict: {} is active, not {}",
                current,
                requested
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
        "mimeType": "video/mp4",
        "audio": false,
    }))
}
