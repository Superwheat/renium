use std::borrow::Cow;
use std::cell::Cell;
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use mp4::{AvcConfig, FourCC, Mp4Config, Mp4Sample, Mp4Writer, TrackConfig};
use openh264::OpenH264API;
use openh264::encoder::{
    Complexity, Encoder, EncoderConfig, FrameRate, FrameType, IntraFramePeriod, Profile, QpRange,
    RateControlMode, UsageType, VuiConfig,
};
use openh264::formats::{BgraSliceU8, RgbaSliceU8, YUVBuffer, YUVSource};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{BridgeServer, BridgeTarget, studio_capture_status, wait_for_player_bridge};
#[cfg(any(windows, target_os = "macos"))]
use super::{client_viewport_size, recover_client_viewport};
use crate::app::output::automation_token;
use crate::studio::input as input_inject;
use crate::system::LockRecover;
use crate::system::files::{
    cleanup_stale_sibling_temps, replace_file_with_backup, sibling_temp_path,
};

static ACTIVE: Mutex<Vec<ActiveRecording>> = Mutex::new(Vec::new());

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StartRequest {
    player: Option<String>,
    #[serde(default)]
    studio: bool,
    #[serde(default)]
    client: bool,
    output: Option<PathBuf>,
    /// Where a default-named recording goes: the directory the command ran
    /// in, with the project root for callers that send none.
    directory: Option<PathBuf>,
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
    fps: f64,
    stop: Arc<AtomicBool>,
    worker: JoinHandle<Result<FinishedRecording>>,
}

/// Frames per second the file actually holds, and whether the recording fell
/// well short of the request: rendering, capture or encoding was slower, as
/// in a minimized or fully covered window, which Windows and Roblox throttle.
fn capture_rate(requested_fps: f64, frames: usize, duration_ms: u64) -> (f64, bool) {
    if duration_ms == 0 || frames < 2 {
        return (0.0, false);
    }
    let achieved = frames as f64 * 1000.0 / duration_ms as f64;
    let achieved = (achieved * 10.0).round() / 10.0;
    (
        achieved,
        duration_ms >= 1000 && achieved < requested_fps * 0.75,
    )
}

struct FinishedRecording {
    capture_fallback: Option<String>,
    width: u32,
    height: u32,
    frames: usize,
    duration_ms: u64,
    resized_frames: usize,
    encode_ms: f64,
    capture_ms: Option<f64>,
    encoder: &'static str,
    serial: bool,
    held_back: bool,
}

/// A picture as a capture hands it over: width, height and four bytes a pixel.
type Frame = (u32, u32, Vec<u8>);

type Timed = (Frame, Instant);

#[derive(Clone, Copy, PartialEq)]
enum Take {
    Next,
    Held,
}

#[cfg(any(windows, test))]
const QUEUE_BYTES: usize = 256 << 20;
const SLICE_BYTES: u32 = 250_000;
const FRESH_FRAME_TRIES: usize = 4;
const BACKGROUND_FPS: f64 = 15.0;
#[cfg(windows)]
const UNLOCK_CHECK_FRAMES: u64 = 30;

#[cfg(any(windows, test))]
fn queue_depth(frame: &Frame) -> usize {
    let bytes = (frame.0 as usize * frame.1 as usize * 4).max(1);
    (QUEUE_BYTES / bytes).clamp(1, 120)
}

#[derive(Default)]
struct CaptureCost {
    micros: AtomicU64,
    frames: AtomicU64,
    watched: AtomicU64,
    held: AtomicU64,
    #[cfg_attr(not(windows), allow(dead_code))]
    unlocked: AtomicBool,
}

impl CaptureCost {
    fn add(&self, started: Instant) {
        let micros = started.elapsed().as_micros().min(u128::from(u64::MAX)) as u64;
        self.micros.fetch_add(micros, Ordering::Relaxed);
        self.frames.fetch_add(1, Ordering::Relaxed);
    }

    #[cfg(any(windows, test))]
    fn restart(&self) {
        self.micros.store(0, Ordering::Relaxed);
        self.frames.store(0, Ordering::Relaxed);
    }

    fn mean_ms(&self) -> Option<f64> {
        let frames = self.frames.load(Ordering::Relaxed);
        (frames > 0).then(|| self.micros.load(Ordering::Relaxed) as f64 / frames as f64 / 1000.0)
    }

    #[cfg(windows)]
    fn watch(&self, pid: u32, in_background: bool) {
        if self
            .watched
            .fetch_add(1, Ordering::Relaxed)
            .is_multiple_of(UNLOCK_CHECK_FRAMES)
        {
            self.unlocked
                .store(crate::studio::fps::unlocked(pid), Ordering::Relaxed);
        }
        let held = in_background && !self.unlocked.load(Ordering::Relaxed);
        self.held.fetch_add(u64::from(held), Ordering::Relaxed);
    }

    fn held_back(&self) -> bool {
        self.held.load(Ordering::Relaxed) * 2 > self.watched.load(Ordering::Relaxed)
    }
}

fn slower_than_the_rate(mean_ms: f64, fps: f64) -> bool {
    mean_ms > 900.0 / fps
}

struct RecordingOptions {
    /// Whether the frames are BGRA, as the capture thread hands them over,
    /// and encoded for motion; otherwise they are RGBA.
    bgra: bool,
    #[cfg_attr(not(windows), allow(dead_code))]
    system_encoder: bool,
    cost: Arc<CaptureCost>,
    /// Whether the capture thread keeps the frame rate. Once it does not, the
    /// encoding loop skips the frame times it has missed.
    paced_by_capture: Cell<bool>,
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
    60.0
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
    _probe_target: BridgeTarget,
) -> Result<input_inject::StudioWindow> {
    let pid = bridge.studio_pid_for_selector(BridgeTarget::Edit, None)?;
    input_inject::studio_recording_window(pid)
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
    #[cfg(windows)]
    {
        input_inject::client_recording_window(pid, viewport)
    }
    #[cfg(target_os = "macos")]
    {
        input_inject::window_for_pid(pid, viewport)
    }
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

fn encode_frame(
    encoder: &mut Encoder,
    yuv: &mut YUVBuffer,
    pixels: &[u8],
    bgra: bool,
) -> Result<EncodedFrame> {
    let dimensions = yuv.dimensions();
    if bgra {
        yuv.read_bgra8(BgraSliceU8::new(pixels, dimensions));
    } else {
        yuv.read_rgba8(RgbaSliceU8::new(pixels, dimensions));
    }
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

trait Video {
    fn name(&self) -> &'static str;
    fn add(&mut self, pixels: &[u8], bgra: bool, at: u64) -> Result<()>;
    fn end(self: Box<Self>, duration_ms: u64) -> Result<()>;
}

struct Software {
    encoder: Encoder,
    yuv: YUVBuffer,
    size: (usize, usize),
    path: PathBuf,
    writer: Option<Mp4Writer<BufWriter<File>>>,
    pending: Option<EncodedFrame>,
    pending_at: u64,
}

impl Software {
    fn start(
        width: usize,
        height: usize,
        options: &RecordingOptions,
        temp_path: &Path,
    ) -> Result<Self> {
        let quality = (51.0 - options.quality * 0.41).round().clamp(10.0, 51.0) as u8;
        let config = EncoderConfig::new()
            .max_frame_rate(FrameRate::from_hz(options.fps as f32))
            .usage_type(if options.bgra {
                UsageType::CameraVideoRealTime
            } else {
                UsageType::ScreenContentRealTime
            })
            .rate_control_mode(RateControlMode::Quality)
            .skip_frames(false)
            .profile(Profile::Baseline)
            .complexity(Complexity::Low)
            .qp(QpRange::new(quality, quality))
            .intra_frame_period(IntraFramePeriod::from_num_frames(key_frame_every(options)))
            .vui(VuiConfig::bt601());
        let config = if options.bgra {
            config.max_slice_len(SLICE_BYTES)
        } else {
            config
        };
        Ok(Self {
            encoder: Encoder::with_api_config(OpenH264API::from_source(), config)
                .context("Failed to initialize the H.264 encoder")?,
            yuv: YUVBuffer::new(width, height),
            size: (width, height),
            path: temp_path.to_path_buf(),
            writer: None,
            pending: None,
            pending_at: 0,
        })
    }

    fn file(&self, sps: Vec<u8>, pps: Vec<u8>) -> Result<Mp4Writer<BufWriter<File>>> {
        let file = File::create(&self.path)
            .with_context(|| format!("Failed to create {}", self.path.display()))?;
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
        let (width, height) = self.size;
        writer
            .add_track(&TrackConfig::from(AvcConfig {
                width: u16::try_from(width).context("Recording width exceeds the MP4 limit")?,
                height: u16::try_from(height).context("Recording height exceeds the MP4 limit")?,
                seq_param_set: sps,
                pic_param_set: pps,
            }))
            .context("Failed to initialize the MP4 video track")?;
        Ok(writer)
    }
}

impl Video for Software {
    fn name(&self) -> &'static str {
        "openh264"
    }

    fn add(&mut self, pixels: &[u8], bgra: bool, at: u64) -> Result<()> {
        let mut next = encode_frame(&mut self.encoder, &mut self.yuv, pixels, bgra)?;
        if self.writer.is_none() {
            let sps = next
                .sps
                .take()
                .context("H.264 encoder omitted the sequence parameters")?;
            let pps = next
                .pps
                .take()
                .context("H.264 encoder omitted the picture parameters")?;
            self.writer = Some(self.file(sps, pps)?);
        }
        if let (Some(frame), Some(writer)) = (self.pending.replace(next), self.writer.as_mut()) {
            write_sample(writer, frame, self.pending_at, at)?;
        }
        self.pending_at = at;
        Ok(())
    }

    fn end(self: Box<Self>, duration_ms: u64) -> Result<()> {
        let video = *self;
        let mut writer = video.writer.context("The recording holds no frame")?;
        if let Some(frame) = video.pending {
            write_sample(&mut writer, frame, video.pending_at, duration_ms)?;
        }
        writer
            .write_end()
            .context("Failed to finish the MP4 file")?;
        writer
            .into_writer()
            .flush()
            .context("Failed to write the MP4 file")
    }
}

#[cfg(windows)]
impl Video for super::recording_media::MediaFile {
    fn name(&self) -> &'static str {
        "system"
    }

    fn add(&mut self, pixels: &[u8], bgra: bool, at: u64) -> Result<()> {
        Self::add(self, pixels, bgra, at)
    }

    fn end(self: Box<Self>, duration_ms: u64) -> Result<()> {
        Self::end(*self, duration_ms)
    }
}

fn key_frame_every(options: &RecordingOptions) -> u32 {
    (options.fps.round() as u32).saturating_mul(2).max(1)
}

fn open_video(
    width: usize,
    height: usize,
    options: &RecordingOptions,
    temp_path: &Path,
) -> Result<Box<dyn Video>> {
    #[cfg(windows)]
    if options.system_encoder {
        use super::recording_media::{MediaFile, Settings};
        let settings = Settings {
            width,
            height,
            fps: options.fps,
            quality: options.quality,
            key_frame_every: key_frame_every(options),
        };
        if let Ok(video) = MediaFile::start(temp_path, &settings) {
            return Ok(Box::new(video));
        }
    }
    Ok(Box::new(Software::start(
        width, height, options, temp_path,
    )?))
}

fn record_file(
    mut capture: impl FnMut(Take) -> Result<Option<Timed>>,
    first: Timed,
    options: &RecordingOptions,
    stop: &AtomicBool,
    ready: mpsc::SyncSender<Result<()>>,
    temp_path: &Path,
) -> Result<FinishedRecording> {
    let (_, width, height) = even_rgba(&first.0.2, first.0.0, first.0.1)?;
    let mut video = open_video(width, height, options, temp_path)?;
    let opened = Instant::now();
    let fits = |frame: &Timed| (frame.0.0 & !1, frame.0.1 & !1) == (width as u32, height as u32);
    let mut first = first;
    while let Some(newer) = capture(Take::Held).ok().flatten() {
        let fresh = newer.1 >= opened;
        if fits(&newer) {
            first = newer;
        }
        if fresh {
            break;
        }
    }
    for _ in 0..FRESH_FRAME_TRIES {
        if first.1 >= opened || stop.load(Ordering::Relaxed) {
            break;
        }
        if let Some(newer) = capture(Take::Next)?
            && fits(&newer)
        {
            first = newer;
        }
    }
    let ((source_width, source_height, source_pixels), captured) = first;
    let started = captured.max(opened);
    let (first_pixels, ..) = even_rgba(&source_pixels, source_width, source_height)?;
    video.add(&first_pixels, options.bgra, 0)?;
    let mut encoding = Duration::ZERO;
    drop(first_pixels);
    drop(source_pixels);
    let _ = ready.send(Ok(()));
    let interval = Duration::from_secs_f64(1.0 / options.fps);
    let limit = Duration::from_secs_f64(options.max_seconds);
    let millis = |length: Duration| length.as_millis().min(u128::from(u64::MAX)) as u64;
    let mut next_frame = started + interval;
    let mut pending_at = 0u64;
    let mut frames = 1usize;
    let mut resized_frames = 0usize;
    let mut ended = None;
    loop {
        if ended.is_none() && (stop.load(Ordering::Relaxed) || started.elapsed() >= limit) {
            ended = Some(started.elapsed().min(limit));
        }
        let taken = if ended.is_some() {
            capture(Take::Held).ok().flatten()
        } else {
            let now = Instant::now();
            if now < next_frame {
                thread::sleep((next_frame - now).min(Duration::from_millis(10)));
                continue;
            }
            match capture(Take::Next) {
                Ok(taken) => taken,
                Err(_) if stop.load(Ordering::Relaxed) => continue,
                Err(error) => return Err(error),
            }
        };
        let Some(((frame_width, frame_height, pixels), at)) = taken else {
            if ended.is_some() {
                break;
            }
            continue;
        };
        let at = at.saturating_duration_since(started);
        if ended.is_some_and(|ended| at > ended) {
            break;
        }
        let (pixels, frame_width, frame_height) = even_rgba(&pixels, frame_width, frame_height)?;
        // A document replacement can change the viewport without changing the
        // client window. Keep capturing live frames on the original canvas.
        let pixels = if (frame_width, frame_height) == (width, height) {
            pixels
        } else {
            resized_frames += 1;
            let (canvas_width, canvas_height) = (width, height);
            let scale = (canvas_width as f64 / frame_width as f64)
                .min(canvas_height as f64 / frame_height as f64);
            let fitted_width = ((frame_width as f64 * scale) as usize).max(1);
            let fitted_height = ((frame_height as f64 * scale) as usize).max(1);
            let x = (canvas_width - fitted_width) / 2;
            let y = (canvas_height - fitted_height) / 2;
            let mut canvas = [0, 0, 0, 255].repeat(canvas_width * canvas_height);
            for row in 0..fitted_height {
                let source_row = row * frame_height / fitted_height;
                for column in 0..fitted_width {
                    let source =
                        (source_row * frame_width + column * frame_width / fitted_width) * 4;
                    let target = ((y + row) * canvas_width + x + column) * 4;
                    canvas[target..target + 4].copy_from_slice(&pixels[source..source + 4]);
                }
            }
            Cow::Owned(canvas)
        };
        let timestamp = millis(at).max(pending_at + 1);
        let encode_started = Instant::now();
        video.add(&pixels, options.bgra, timestamp)?;
        encoding += encode_started.elapsed();
        pending_at = timestamp;
        frames += 1;
        next_frame += interval;
        if !options.paced_by_capture.get() {
            while next_frame <= Instant::now() {
                next_frame += interval;
            }
        }
    }
    let duration_ms = millis(ended.unwrap_or_else(|| started.elapsed())).max(pending_at + 1);
    let encoder = video.name();
    video.end(duration_ms)?;
    fs::OpenOptions::new()
        .write(true)
        .open(temp_path)
        .and_then(|file| file.sync_all())
        .with_context(|| format!("Failed to write {}", temp_path.display()))?;
    replace_file_with_backup(temp_path, &options.output, "recording")?;
    Ok(FinishedRecording {
        capture_fallback: None,
        width: width as u32,
        height: height as u32,
        frames,
        duration_ms,
        resized_frames,
        encode_ms: encoding.as_secs_f64() * 1000.0 / frames.saturating_sub(1).max(1) as f64,
        capture_ms: options.cost.mean_ms(),
        encoder,
        serial: !options.paced_by_capture.get(),
        held_back: options.cost.held_back(),
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
    let result = record_window(&window, first, options, &stop, ready, &temp_path);
    if result.is_err() {
        let _ = fs::remove_file(temp_path);
    }
    result
}

fn record_window(
    window: &input_inject::StudioWindow,
    first: Frame,
    options: RecordingOptions,
    stop: &AtomicBool,
    ready: mpsc::SyncSender<Result<()>>,
    temp_path: &Path,
) -> Result<FinishedRecording> {
    let shown = || input_inject::capture_window_rgba(window);
    #[cfg(windows)]
    if options.bgra {
        let interval = Duration::from_secs_f64(1.0 / options.fps);
        let cost = Arc::clone(&options.cost);
        return record_pipelined(
            |frames, done| stream_frames(window, frames, stop, done, interval, &cost),
            shown,
            first,
            options,
            stop,
            ready,
            temp_path,
        );
    }
    record_file(
        |take| match take {
            Take::Next => {
                let started = Instant::now();
                let frame = shown()?;
                options.cost.add(started);
                Ok(Some((frame, Instant::now())))
            }
            Take::Held => Ok(None),
        },
        (first, Instant::now()),
        &options,
        stop,
        ready,
        temp_path,
    )
}

/// Encodes on this thread the frames `produce` captures on another, so a
/// slow capture and a slow encoder overlap. When the producer refuses or
/// ends, the recording goes on with `shown`, the slower capture of the
/// window, and the answer names why.
#[cfg(any(windows, test))]
fn record_pipelined(
    produce: impl FnOnce(mpsc::SyncSender<Result<Timed>>, &AtomicBool) + Send,
    mut shown: impl FnMut() -> Result<Frame>,
    first: Frame,
    mut options: RecordingOptions,
    stop: &AtomicBool,
    ready: mpsc::SyncSender<Result<()>>,
    temp_path: &Path,
) -> Result<FinishedRecording> {
    let done = AtomicBool::new(false);
    thread::scope(|scope| {
        let (sender, receiver) = mpsc::sync_channel(queue_depth(&first));
        let producer_done = &done;
        let producer = scope.spawn(move || produce(sender, producer_done));
        let receive = || receiver.recv().context("Capture worker stopped")?;
        let mut fallback = None;
        let mut held_refusal = None;
        let first = match receive() {
            Ok(timed) => timed,
            Err(error) => {
                fallback = Some(format!("{error:#}"));
                options.bgra = false;
                options.paced_by_capture.set(false);
                (first, Instant::now())
            }
        };
        let result = record_file(
            |take| {
                if take == Take::Held {
                    return Ok(match receiver.try_recv() {
                        Ok(Ok(timed)) => Some(timed),
                        Ok(Err(error)) => {
                            held_refusal = Some(error);
                            None
                        }
                        Err(_) => None,
                    });
                }
                if options.paced_by_capture.get() {
                    let received = match held_refusal.take() {
                        Some(error) => Err(error),
                        None => receive(),
                    };
                    match received {
                        Ok(timed) => return Ok(Some(timed)),
                        Err(error) if stop.load(Ordering::Relaxed) => return Err(error),
                        Err(error) => fallback = Some(format!("{error:#}")),
                    }
                    options.paced_by_capture.set(false);
                    options.cost.restart();
                }
                let capture_started = Instant::now();
                let mut frame = shown()?;
                options.cost.add(capture_started);
                if options.bgra {
                    for pixel in frame.2.chunks_exact_mut(4) {
                        pixel.swap(0, 2);
                    }
                }
                Ok(Some((frame, Instant::now())))
            },
            first,
            &options,
            stop,
            ready,
            temp_path,
        );
        done.store(true, Ordering::Relaxed);
        drop(receiver);
        if producer.join().is_err() {
            fallback = Some("Capture worker panicked".into());
        }
        result.map(|mut finished| {
            finished.capture_fallback = fallback;
            finished
        })
    })
}

/// Notices a capture that returns a blank or frozen picture where the window
/// shows another: its frames are compared with the window's own rendering at
/// the start and while they stand still, ever less often.
#[cfg(any(windows, test))]
struct Freshness {
    rows: u64,
    still_since: Option<Instant>,
    checked: Option<Instant>,
    gap: Duration,
}

#[cfg(any(windows, test))]
impl Freshness {
    const STILL: Duration = Duration::from_millis(500);
    const APART: Duration = Duration::from_secs(2);
    const LONGEST: Duration = Duration::from_secs(16);

    fn new() -> Self {
        Self {
            rows: 0,
            still_since: None,
            checked: None,
            gap: Self::APART,
        }
    }

    /// Whether this frame is to be compared with the window's rendering.
    fn due(&mut self, frame: &Frame, now: Instant) -> bool {
        let rows = sampled_rows(frame);
        if self.checked.is_none() {
            self.rows = rows;
            return true;
        }
        if rows != self.rows {
            self.rows = rows;
            self.still_since = Some(now);
            self.gap = Self::APART;
            return false;
        }
        let since = *self.still_since.get_or_insert(now);
        now.duration_since(since) >= Self::STILL
            && self
                .checked
                .is_some_and(|checked| now.duration_since(checked) >= self.gap)
    }

    /// Whether a BGRA frame shows what the window shows: `shown` is the
    /// window's own RGBA rendering taken after it, `again` the next frame. A
    /// moving picture differs from `shown`, but then differs from `again` as
    /// much; a frozen or blank one differs from `shown` alone. A picture of
    /// one color is held to the window's exactly, since a capture that
    /// painted nothing is one too. Each agreement on a picture that stands
    /// still doubles the wait for the next comparison.
    fn agrees(&mut self, frame: &Frame, shown: &Frame, again: &Frame, now: Instant) -> bool {
        self.checked = Some(now);
        self.still_since = None;
        let blank = difference(frame, &(frame.0, frame.1, first_pixel(frame)), false, 0) == 0.0;
        let tolerance = if blank { 4 } else { 32 };
        let apart = difference(frame, shown, true, tolerance);
        let agreed = apart < 0.02 || difference(frame, again, false, tolerance) * 2.0 > apart;
        self.gap = if agreed && !blank {
            (self.gap * 2).min(Self::LONGEST)
        } else {
            Self::APART
        };
        agreed
    }
}

/// A picture of the frame's size that repeats its first pixel.
#[cfg(any(windows, test))]
fn first_pixel(frame: &Frame) -> Vec<u8> {
    let pixel = frame.2.get(..4).unwrap_or(&[0; 4]);
    pixel.repeat(frame.0 as usize * frame.1 as usize)
}

/// A hash of every fourth row of a frame.
#[cfg(any(windows, test))]
fn sampled_rows(frame: &Frame) -> u64 {
    let (width, height) = (frame.0 as usize, frame.1 as usize);
    let mut hash = (width as u64) << 32 | height as u64;
    for y in (0..height).step_by(4) {
        let Some(row) = frame.2.get(y * width * 4..(y + 1) * width * 4) else {
            break;
        };
        for word in row.chunks_exact(8) {
            let word = u64::from_le_bytes(<[u8; 8]>::try_from(word).unwrap_or_default());
            hash = (hash ^ word).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        }
    }
    hash
}

/// The share of a grid of sample points at which two frames of one size
/// differ by more than `tolerance` in a channel; `swapped` when `other` has
/// red and blue the other way around. Frames of different sizes differ
/// everywhere.
#[cfg(any(windows, test))]
fn difference(frame: &Frame, other: &Frame, swapped: bool, tolerance: u8) -> f64 {
    let (width, height) = (frame.0.min(other.0) as usize, frame.1.min(other.1) as usize);
    if width == 0 || height == 0 || frame.0.abs_diff(other.0) > 1 || frame.1.abs_diff(other.1) > 1 {
        return 1.0;
    }
    let (columns, rows) = (width.min(64), height.min(36));
    let pixel = |of: &Frame, x: usize, y: usize| -> Option<[u8; 3]> {
        let at = (y * of.0 as usize + x) * 4;
        of.2.get(at..at + 3)?.try_into().ok()
    };
    let mut apart = 0usize;
    for row in 0..rows {
        let y = (row * 2 + 1) * height / (rows * 2);
        for column in 0..columns {
            let x = (column * 2 + 1) * width / (columns * 2);
            let alike = pixel(frame, x, y)
                .zip(pixel(other, x, y))
                .is_some_and(|(a, mut b)| {
                    if swapped {
                        b.swap(0, 2);
                    }
                    a.iter().zip(b).all(|(a, b)| a.abs_diff(b) <= tolerance)
                });
            if !alike {
                apart += 1;
            }
        }
    }
    apart as f64 / (columns * rows) as f64
}

/// Captures the window's frames at `interval` and hands each to the
/// recording. A capture refused five times in a row, or one whose picture is
/// not the window's, is handed over as the error that ends this route.
#[cfg(windows)]
fn stream_frames(
    window: &input_inject::StudioWindow,
    sender: mpsc::SyncSender<Result<Timed>>,
    stop: &AtomicBool,
    done: &AtomicBool,
    interval: Duration,
    cost: &CaptureCost,
) {
    let ended = || stop.load(Ordering::Relaxed) || done.load(Ordering::Relaxed);
    let mut capture = match input_inject::recording_capture::Capture::new(window) {
        Ok(capture) => capture,
        Err(error) => {
            let _ = sender.send(Err(error));
            return;
        }
    };
    let mut freshness = Freshness::new();
    let mut refused = 0;
    let mut next = Instant::now();
    while !ended() {
        while Instant::now() < next && !ended() {
            thread::sleep(
                next.saturating_duration_since(Instant::now())
                    .min(Duration::from_millis(10)),
            );
        }
        if ended() {
            break;
        }
        next = (next + interval).max(Instant::now());
        let capture_started = Instant::now();
        let captured = capture.next(stop);
        cost.add(capture_started);
        cost.watch(capture.pid(), capture.in_background());
        let frame = match captured {
            Err(_) if refused < 4 => {
                refused += 1;
                continue;
            }
            Err(error) => Err(error),
            Ok(frame) => {
                refused = 0;
                shown_by_window(window, &mut capture, &mut freshness, frame, stop)
                    .map(|frame| (frame, Instant::now()))
            }
        };
        let failed = frame.is_err();
        if sender.send(frame).is_err() || failed {
            break;
        }
    }
}

/// The frame, or the one after it where it was due for a comparison with the
/// window's own rendering; an error when the capture's picture is not the
/// window's.
#[cfg(windows)]
fn shown_by_window(
    window: &input_inject::StudioWindow,
    capture: &mut input_inject::recording_capture::Capture,
    freshness: &mut Freshness,
    mut frame: Frame,
    stop: &AtomicBool,
) -> Result<Frame> {
    if !freshness.due(&frame, Instant::now()) {
        return Ok(frame);
    }
    for _ in 0..2 {
        let shown = input_inject::capture_window_rgba(window)?;
        let again = capture.next(stop)?;
        if freshness.agrees(&frame, &shown, &again, Instant::now()) {
            return Ok(again);
        }
        frame = again;
    }
    bail!("The fast capture returned a picture the window does not show")
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
    if !request.fps.is_finite() || !(1.0..=60.0).contains(&request.fps) {
        bail!("Invalid record-start fps; expected a number from 1 through 60");
    }
    if !request.max_seconds.is_finite() || !(1.0..=300.0).contains(&request.max_seconds) {
        bail!("Invalid record-start maxSeconds; expected a number from 1 through 300");
    }
    if !request.quality.is_finite() || !(0.0..=100.0).contains(&request.quality) {
        bail!("Invalid record-start quality; expected a number from 0 through 100");
    }
    let mut active = ACTIVE.lock_recover();
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
    if let Some(current) = active
        .iter()
        .find(|recording| recording.window == window.label)
    {
        bail!("Recording conflict: {} is still active", current.id);
    }
    let id = automation_token("recording");
    let base = request.directory.as_deref().unwrap_or(root);
    let output = request
        .output
        .unwrap_or_else(|| PathBuf::from(format!("{id}.mp4")));
    let output = if output.is_absolute() {
        output
    } else {
        base.join(output)
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
        bgra: cfg!(windows),
        system_encoder: cfg!(windows),
        cost: Arc::default(),
        paced_by_capture: Cell::new(cfg!(windows)),
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
    active.push(ActiveRecording {
        id: id.clone(),
        output: output.clone(),
        target,
        window: window_label,
        fps: request.fps,
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
        let mut slot = ACTIVE.lock_recover();
        let ids = || {
            slot.iter()
                .map(|recording| recording.id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        };
        let index = match request.recording_id.as_deref() {
            Some(requested) => match slot.iter().position(|recording| recording.id == requested) {
                Some(index) => index,
                None if slot.is_empty() => bail!("Recording conflict: no recording is active"),
                None => bail!("Recording conflict: {} is active, not {}", ids(), requested),
            },
            None => match slot.len() {
                0 => bail!("Recording conflict: no recording is active"),
                1 => 0,
                _ => bail!(
                    "Recording conflict: several recordings are active ({}); pass the recording id",
                    ids()
                ),
            },
        };
        slot.remove(index)
    };
    active.stop.store(true, Ordering::Relaxed);
    let finished = active
        .worker
        .join()
        .map_err(|_| anyhow!("Recording worker panicked"))??;
    let (achieved_fps, throttled) = capture_rate(active.fps, finished.frames, finished.duration_ms);
    let mut result = json!({
        "recordingId": active.id,
        "path": active.output.display().to_string(),
        "target": active.target,
        "window": active.window,
        "width": finished.width,
        "height": finished.height,
        "frames": finished.frames,
        "durationMs": finished.duration_ms,
        "resizedFrames": finished.resized_frames,
        "requestedFps": active.fps,
        "achievedFps": achieved_fps,
        "mimeType": "video/mp4",
        "audio": false,
    });
    result["encoder"] = json!(finished.encoder);
    if finished.held_back && active.fps > BACKGROUND_FPS {
        result["throttled"] = json!(true);
        result["note"] = json!(
            "The recorded window was in the background and not unlocked, so it rendered one frame in four and the recording repeats frames: run `rbx fps unlock` for it (`--player NAME` for a Play client) and record again"
        );
    } else if throttled {
        result["throttled"] = json!(true);
        let tenths = |ms: f64| (ms * 10.0).round() / 10.0;
        let (width, height, fps) = (finished.width, finished.height, active.fps);
        let allowed = tenths(1000.0 / fps);
        let held =
            format!("The recording holds {achieved_fps} frames per second against {fps} requested");
        let (encode_ms, capture_ms) = (finished.encode_ms, finished.capture_ms.unwrap_or(0.0));
        let slow = |what: &str, took: f64| {
            format!(
                "{held}: {what} a {width}x{height} frame took {} ms and {fps} FPS allows {allowed} ms; record a smaller window",
                tenths(took)
            )
        };
        let encode_slow = slower_than_the_rate(encode_ms, fps);
        let capture_slow = slower_than_the_rate(capture_ms, fps);
        let both_slow = finished.serial && slower_than_the_rate(encode_ms + capture_ms, fps);
        if encode_slow || both_slow {
            result["encodeMs"] = json!(tenths(encode_ms));
        }
        if capture_slow || both_slow {
            result["captureMs"] = json!(tenths(capture_ms));
        }
        result["note"] = json!(if encode_slow {
            slow("encoding", encode_ms)
        } else if capture_slow {
            slow("capturing", capture_ms)
        } else if both_slow {
            slow("capturing and encoding", encode_ms + capture_ms)
        } else {
            format!(
                "{held}: the window rendered fewer. A minimized or fully covered window is throttled by Windows and Roblox; keep the recorded window restored behind your other windows, never minimized, and record again"
            )
        });
    }
    if let Some(reason) = finished.capture_fallback {
        result["captureFallback"] = json!(reason);
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    #[test]
    fn native_bgra_preserves_rgba_colors_and_ignores_unused_alpha() {
        let rgba = [
            [240, 30, 60, 255],
            [20, 210, 80, 255],
            [30, 40, 220, 255],
            [250, 180, 10, 255],
        ]
        .concat()
        .repeat(64);
        let bgra = [
            [60, 30, 240, 0],
            [80, 210, 20, 0],
            [220, 40, 30, 0],
            [10, 180, 250, 0],
        ]
        .concat()
        .repeat(64);
        let encode = |pixels: &[u8], bgra| {
            let mut encoder =
                Encoder::with_api_config(OpenH264API::from_source(), EncoderConfig::new()).unwrap();
            encode_frame(&mut encoder, &mut YUVBuffer::new(16, 16), pixels, bgra).unwrap()
        };
        let expected = encode(&rgba, false);
        let actual = encode(&bgra, true);
        assert_eq!(actual.bytes, expected.bytes);
        assert_eq!(actual.sps, expected.sps);
        assert_eq!(actual.pps, expected.pps);
    }

    fn picture(width: u32, height: u32, color: impl Fn(u32, u32) -> [u8; 4]) -> Frame {
        let mut pixels = Vec::with_capacity((width * height * 4) as usize);
        for y in 0..height {
            for x in 0..width {
                pixels.extend_from_slice(&color(x, y));
            }
        }
        (width, height, pixels)
    }

    #[test]
    fn pictures_differ_by_what_they_show_whatever_their_channel_order() {
        let bars = |shift: u32| move |x: u32, _| [((x + shift) / 8 % 2 * 255) as u8, 90, 200, 255];
        let bgra = picture(96, 64, bars(0));
        let rgba = picture(96, 64, |x, y| {
            let [blue, green, red, alpha] = bars(0)(x, y);
            [red, green, blue, alpha]
        });
        assert_eq!(difference(&bgra, &rgba, true, 32), 0.0);
        assert_eq!(difference(&bgra, &bgra, false, 32), 0.0);
        assert!(difference(&bgra, &rgba, false, 32) > 0.9);
        assert!(difference(&bgra, &picture(96, 64, bars(8)), false, 32) > 0.9);
        assert_eq!(difference(&bgra, &picture(95, 64, bars(0)), false, 32), 0.0);
        assert_eq!(difference(&bgra, &picture(64, 64, bars(0)), false, 32), 1.0);
        assert_eq!(difference(&bgra, &(96, 64, Vec::new()), false, 32), 1.0);
        let near = picture(96, 64, |x, y| {
            let [blue, green, red, alpha] = bars(0)(x, y);
            [blue.saturating_sub(20), green + 20, red, alpha]
        });
        assert_eq!(difference(&bgra, &near, false, 32), 0.0);
        assert_eq!(difference(&bgra, &near, false, 4), 1.0);
    }

    #[test]
    fn a_frozen_or_blank_fast_capture_is_told_from_a_moving_one() {
        let scene =
            |shift: u32| picture(96, 64, move |x, _| [((x + shift) / 8 % 2 * 255) as u8; 4]);
        let blank = picture(96, 64, |_, _| [0, 0, 0, 255]);
        let now = Instant::now();
        let mut freshness = Freshness::new();
        assert!(freshness.agrees(&scene(0), &scene(0), &scene(0), now));
        assert!(freshness.agrees(&scene(0), &scene(8), &scene(8), now));
        assert!(freshness.agrees(&blank, &blank, &blank, now));
        assert!(!freshness.agrees(&scene(0), &scene(8), &scene(0), now));
        assert!(!freshness.agrees(&blank, &scene(0), &blank, now));
        let dim = picture(96, 64, |x, _| [6 + (x % 16) as u8; 4]);
        assert!(
            !freshness.agrees(&blank, &dim, &blank, now),
            "a dark scene is not a blank one"
        );
        assert!(freshness.agrees(&dim, &dim, &dim, now));
        let mut frame = scene(0);
        frame.2[..2400].fill(7);
        assert!(!freshness.agrees(&scene(0), &scene(8), &frame, now));
    }

    #[test]
    fn the_fast_capture_is_compared_at_the_start_and_when_it_stops_changing() {
        let scene =
            |shift: u32| picture(96, 64, move |x, _| [((x + shift) / 8 % 2 * 255) as u8; 4]);
        let start = Instant::now();
        let at = |millis: u64| start + Duration::from_millis(millis);
        let mut freshness = Freshness::new();
        assert!(freshness.due(&scene(0), at(0)));
        assert!(
            freshness.due(&scene(0), at(16)),
            "until a comparison is made"
        );
        assert!(freshness.agrees(&scene(0), &scene(0), &scene(0), at(20)));
        for (frame, millis) in (1..200).map(|frame| (frame, 20 + frame * 16)) {
            assert!(
                !freshness.due(&scene(frame as u32), at(millis)),
                "a moving picture"
            );
        }
        let still = scene(3);
        assert!(!freshness.due(&still, at(4000)));
        assert!(
            !freshness.due(&still, at(4400)),
            "still for less than half a second"
        );
        assert!(freshness.due(&still, at(4600)));
        assert!(freshness.agrees(&still, &still, &still, at(4610)));
        assert!(
            !freshness.due(&still, at(5200)),
            "compared less than two seconds ago"
        );
        assert!(!freshness.due(&still, at(6500)));
        assert!(
            !freshness.due(&still, at(6700)),
            "the wait doubles while it stands still"
        );
        assert!(freshness.due(&still, at(8700)));
        assert!(freshness.agrees(&still, &still, &still, at(8710)));
        assert!(!freshness.due(&scene(4), at(9000)), "a changed picture");
        assert!(!freshness.due(&scene(4), at(10000)));
        assert!(freshness.due(&scene(4), at(10800)), "the wait starts over");
    }

    struct Pipelined {
        root: PathBuf,
        stop: AtomicBool,
    }

    impl Pipelined {
        fn new(name: &str) -> Self {
            let root = std::env::temp_dir().join(automation_token(name));
            fs::create_dir_all(&root).unwrap();
            Self {
                root,
                stop: AtomicBool::new(false),
            }
        }

        fn record(
            &self,
            produce: impl FnOnce(mpsc::SyncSender<Result<Timed>>, &AtomicBool) + Send,
            shown: impl FnMut() -> Result<Frame>,
        ) -> Result<FinishedRecording> {
            let options = RecordingOptions {
                bgra: true,
                system_encoder: false,
                cost: Arc::default(),
                paced_by_capture: Cell::new(true),
                output: self.root.join("clip.mp4"),
                fps: 60.0,
                max_seconds: 5.0,
                quality: 80.0,
            };
            let (ready, _ready) = mpsc::sync_channel(1);
            let temp = sibling_temp_path(&options.output);
            let first = picture(64, 32, |_, _| [9, 9, 9, 255]);
            record_pipelined(produce, shown, first, options, &self.stop, ready, &temp)
        }
    }

    impl Drop for Pipelined {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn a_pipelined_recording_ends_with_its_capture_thread_when_it_is_stopped() {
        let run = Pipelined::new("recording-pipelined-stop");
        let sent = std::sync::atomic::AtomicUsize::new(0);
        let finished = run
            .record(
                |frames, done| {
                    while !done.load(Ordering::Relaxed) {
                        if sent.fetch_add(1, Ordering::Relaxed) == 4 {
                            run.stop.store(true, Ordering::Relaxed);
                        }
                        let frame = picture(64, 32, |x, _| [x as u8, 0, 0, 255]);
                        if frames.send(Ok((frame, Instant::now()))).is_err() {
                            break;
                        }
                    }
                },
                || bail!("the window is not captured while the fast capture runs"),
            )
            .unwrap();
        assert_eq!(finished.capture_fallback, None);
        assert!(finished.frames >= 2);
        assert_eq!((finished.width, finished.height), (64, 32));
        assert!(run.root.join("clip.mp4").is_file());
    }

    #[test]
    fn a_refused_fast_capture_records_the_window_and_names_why() {
        for (name, good, panics, reason) in [
            ("refused", 0, false, "the viewport is not Studio's"),
            ("ended", 3, false, "the viewport is not Studio's"),
            ("panicked", 3, true, "Capture worker panicked"),
        ] {
            let run = Pipelined::new(&format!("recording-pipelined-{name}"));
            let mut shown = 0;
            let finished = run
                .record(
                    |frames, _| {
                        for _ in 0..good {
                            let frame = picture(64, 32, |_, _| [200, 80, 20, 255]);
                            frames.send(Ok((frame, Instant::now()))).unwrap();
                        }
                        if panics {
                            panic!("capture worker test panic");
                        }
                        let _ = frames.send(Err(anyhow!("the viewport is not Studio's")));
                    },
                    || {
                        shown += 1;
                        if shown == 3 {
                            run.stop.store(true, Ordering::Relaxed);
                        }
                        Ok(picture(64, 32, |_, _| [20, 80, 200, 255]))
                    },
                )
                .unwrap();
            assert_eq!(finished.capture_fallback.as_deref(), Some(reason), "{name}");
            assert!(finished.frames >= 2, "{name}: {}", finished.frames);
            assert!(shown >= 2, "{name}");
            assert!(run.root.join("clip.mp4").is_file(), "{name}");
        }
    }

    #[test]
    fn capture_rate_reports_the_file_rate_and_flags_a_throttled_source() {
        assert_eq!(capture_rate(60.0, 600, 10_000), (60.0, false));
        assert_eq!(capture_rate(24.0, 120, 10_000), (12.0, true));
        assert_eq!(capture_rate(60.0, 20, 400), (50.0, false));
        assert_eq!(capture_rate(60.0, 1, 1_000), (0.0, false));
    }

    use super::*;

    #[test]
    fn recording_end_saves_video_reviews_and_preserves_it_when_review_fails() {
        let root = std::env::temp_dir().join(automation_token("recording-end"));
        fs::create_dir_all(&root).unwrap();
        for (name, review, block_review) in [
            ("normal", true, false),
            ("system", true, false),
            ("video-only", false, false),
            ("blocked", true, true),
        ] {
            let output = root.join(format!("{name}.mp4"));
            if block_review {
                fs::write(root.join(format!("{name}.mp4.review")), b"not a directory").unwrap();
            }
            let stop = Arc::new(AtomicBool::new(false));
            let thread_stop = Arc::clone(&stop);
            let options = RecordingOptions {
                bgra: name == "normal",
                system_encoder: name == "system",
                cost: Arc::default(),
                paced_by_capture: Cell::new(name == "normal"),
                output: output.clone(),
                fps: 30.0,
                max_seconds: 5.0,
                quality: 80.0,
            };
            let (ready_tx, ready_rx) = mpsc::sync_channel(1);
            let (done_tx, done_rx) = mpsc::sync_channel(1);
            let worker = thread::spawn(move || {
                let mut frames = 0;
                let color = if options.bgra {
                    [200, 80, 20, 255]
                } else {
                    [20, 80, 200, 255]
                };
                let mut frame = |take| {
                    if take == Take::Held {
                        return Ok(None);
                    }
                    frames += 1;
                    if frames == 5 {
                        thread_stop.store(true, Ordering::Relaxed);
                    }
                    let (width, height) = if frames == 4 { (97, 65) } else { (65, 33) };
                    let pixels = color.repeat((width * height) as usize);
                    Ok(Some(((width, height, pixels), Instant::now())))
                };
                let first = frame(Take::Next)?.unwrap();
                let temp = sibling_temp_path(&options.output);
                let result = record_file(frame, first, &options, &thread_stop, ready_tx, &temp);
                let _ = done_tx.send(());
                result
            });
            ready_rx.recv().unwrap().unwrap();
            ACTIVE.lock().unwrap().push(ActiveRecording {
                id: name.into(),
                output: output.clone(),
                target: "test",
                window: "synthetic capture".into(),
                fps: 30.0,
                stop,
                worker,
            });
            assert!(end(&json!({"recordingId":"wrong"})).is_err());
            done_rx.recv().unwrap();
            let mut result = end(&json!({"recordingId":name})).unwrap();
            if review {
                super::super::recording_review::attach_overview(&mut result);
            }
            assert_eq!(result["width"], 64);
            assert_eq!(result["height"], 32);
            assert_eq!(result["resizedFrames"], 1);
            assert!(output.is_file());
            if review && !block_review {
                assert_eq!(result["review"]["totalFrames"], result["frames"]);
                assert!(Path::new(result["review"]["path"].as_str().unwrap()).is_file());
            } else if block_review {
                assert!(result["reviewError"].is_string());
            } else {
                assert!(result.get("review").is_none());
            }
            assert!(end(&json!({})).is_err());
        }
        fs::remove_dir_all(root).unwrap();
    }
}
