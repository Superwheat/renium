use std::collections::{HashMap, VecDeque};
use std::sync::mpsc;
use std::time::Instant;

use anyhow::Result;
use windows_sys::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};

use crate::studio::native::serializer::{AudioPacket, AudioSampleFormat, AudioTap};

pub(super) const OUTPUT_SAMPLE_RATE: u32 = 48_000;
pub(super) const OUTPUT_CHANNELS: u32 = 2;
const CHUNK_FRAMES: usize = 960;
const EMIT_LAG_MS: u64 = 120;
const CONTINUOUS_PAST_FRAMES: f64 = 12_000.0;
const SINGLE_GAP_RESYNC_FRAMES: f64 = 2_400.0;
const SUSTAINED_LAG_FRAMES: f64 = 960.0;
const SUSTAINED_LAG_PACKETS: u8 = 3;
const LATE_START_THRESHOLD_MS: u64 = 50;

pub(super) trait AudioSource: Send {
    fn is_active(&self) -> bool;
    fn drain(&mut self, out: &mut Vec<AudioPacket>);
    fn deactivate(&self);
    fn stop(&mut self);
    fn dropped(&self) -> u64;
    fn skipped_unknown(&self) -> u64;
    fn read_faults(&self) -> u64;
}

impl AudioSource for AudioTap {
    fn is_active(&self) -> bool {
        Self::is_active(self)
    }
    fn drain(&mut self, out: &mut Vec<AudioPacket>) {
        Self::drain(self, out);
    }
    fn deactivate(&self) {
        Self::deactivate(self);
    }
    fn stop(&mut self) {
        Self::stop(self);
    }
    fn dropped(&self) -> u64 {
        Self::dropped(self)
    }
    fn skipped_unknown(&self) -> u64 {
        Self::skipped_unknown(self)
    }
    fn read_faults(&self) -> u64 {
        Self::read_faults(self)
    }
}

pub(super) fn qpc_now() -> i64 {
    let mut counter = 0i64;
    unsafe { QueryPerformanceCounter(&mut counter) };
    counter
}

pub(super) fn qpc_frequency() -> u64 {
    let mut freq = 0i64;
    unsafe { QueryPerformanceFrequency(&mut freq) };
    freq.max(1) as u64
}

struct StreamState {
    cursor_frames: f64,
    initialized: bool,
    src_rate: u32,
    frac: f64,
    prev: [f32; 2],
    has_prev: bool,
    sustained_lag: u8,
}

impl StreamState {
    fn new(src_rate: u32) -> Self {
        Self {
            cursor_frames: 0.0,
            initialized: false,
            src_rate,
            frac: 0.0,
            prev: [0.0, 0.0],
            has_prev: false,
            sustained_lag: 0,
        }
    }
}

pub(super) struct AudioFinishReport {
    pub(super) had_tap: bool,
    pub(super) got_audio: bool,
    pub(super) late_start_ms: Option<u64>,
    pub(super) tap_error: Option<String>,
    pub(super) dropped: u64,
    pub(super) skipped_unknown: u64,
    pub(super) read_faults: u64,
    pub(super) tap_lost: bool,
}

pub(super) struct AudioMixer {
    qpc_freq: f64,
    start_qpc: i64,
    emitted_frames: i64,
    mix: VecDeque<[f32; 2]>,
    streams: HashMap<u64, StreamState>,
    scratch_stereo: Vec<[f32; 2]>,
    scratch_resampled: Vec<[f32; 2]>,
    pcm_out: Vec<i16>,
}

impl AudioMixer {
    pub(super) fn new(qpc_freq: u64) -> Self {
        Self {
            qpc_freq: (qpc_freq.max(1)) as f64,
            start_qpc: 0,
            emitted_frames: 0,
            mix: VecDeque::with_capacity(OUTPUT_SAMPLE_RATE as usize),
            streams: HashMap::new(),
            scratch_stereo: Vec::new(),
            scratch_resampled: Vec::new(),
            pcm_out: Vec::with_capacity(CHUNK_FRAMES * OUTPUT_CHANNELS as usize),
        }
    }

    pub(super) fn set_start_qpc(&mut self, start_qpc: i64) {
        self.start_qpc = start_qpc;
    }

    fn resolve_placement(stream: &mut StreamState, nominal_pos: f64, emitted_frames: i64) -> f64 {
        if !stream.initialized {
            stream.initialized = true;
            stream.sustained_lag = 0;
            return nominal_pos;
        }
        let diff = nominal_pos - stream.cursor_frames;
        stream.sustained_lag = if diff > SUSTAINED_LAG_FRAMES {
            stream.sustained_lag.saturating_add(1)
        } else {
            0
        };
        let must_resync = !(-CONTINUOUS_PAST_FRAMES..=SINGLE_GAP_RESYNC_FRAMES).contains(&diff)
            || stream.sustained_lag >= SUSTAINED_LAG_PACKETS
            || (stream.cursor_frames < emitted_frames as f64
                && nominal_pos >= emitted_frames as f64);
        if must_resync {
            stream.sustained_lag = 0;
            nominal_pos.max(emitted_frames as f64)
        } else {
            stream.cursor_frames
        }
    }

    pub(super) fn ingest(&mut self, packet: &AudioPacket) {
        if packet.frames == 0 || packet.sample_rate == 0 || packet.channels == 0 {
            return;
        }
        let nominal_pos = (packet.qpc_ticks.saturating_sub(self.start_qpc) as f64 / self.qpc_freq)
            * f64::from(OUTPUT_SAMPLE_RATE);
        let stream = self
            .streams
            .entry(packet.stream_id)
            .or_insert_with(|| StreamState::new(packet.sample_rate));
        if stream.src_rate != packet.sample_rate {
            stream.src_rate = packet.sample_rate;
            stream.frac = 0.0;
            stream.has_prev = false;
        }
        if packet.silent {
            let out_len = f64::from(packet.frames) * f64::from(OUTPUT_SAMPLE_RATE)
                / f64::from(packet.sample_rate);
            stream.cursor_frames =
                Self::resolve_placement(stream, nominal_pos, self.emitted_frames) + out_len;
            stream.frac = 0.0;
            stream.has_prev = false;
            return;
        }
        decode_to_stereo(
            &packet.payload,
            packet.frames as usize,
            packet.channels as usize,
            packet.format,
            &mut self.scratch_stereo,
        );
        if self.scratch_stereo.is_empty() {
            return;
        }
        let samples = if packet.sample_rate == OUTPUT_SAMPLE_RATE {
            &self.scratch_stereo[..]
        } else {
            resample_linear(
                &self.scratch_stereo,
                packet.sample_rate,
                OUTPUT_SAMPLE_RATE,
                &mut stream.frac,
                &mut stream.prev,
                &mut stream.has_prev,
                &mut self.scratch_resampled,
            );
            &self.scratch_resampled[..]
        };
        if samples.is_empty() {
            return;
        }
        let place_at = Self::resolve_placement(stream, nominal_pos, self.emitted_frames);
        stream.cursor_frames = place_at + samples.len() as f64;
        let start_frame = place_at.round() as i64;
        if start_frame + samples.len() as i64 <= self.emitted_frames {
            return;
        }
        let skip = (self.emitted_frames - start_frame).max(0) as usize;
        let dest_offset = (start_frame + skip as i64 - self.emitted_frames) as usize;
        let needed = dest_offset + (samples.len() - skip);
        if self.mix.len() < needed {
            self.mix.resize(needed, [0.0, 0.0]);
        }
        for (i, sample) in samples[skip..].iter().enumerate() {
            self.mix[dest_offset + i][0] += sample[0];
            self.mix[dest_offset + i][1] += sample[1];
        }
    }

    pub(super) fn emit_up_to(
        &mut self,
        target_frame: i64,
        full_chunks_only: bool,
        mut write_chunk: impl FnMut(&[i16]) -> Result<()>,
    ) -> Result<()> {
        let target = target_frame.max(self.emitted_frames);
        while self.emitted_frames < target {
            let remaining = (target - self.emitted_frames) as usize;
            if full_chunks_only && remaining < CHUNK_FRAMES {
                break;
            }
            let count = remaining.min(CHUNK_FRAMES);
            self.pcm_out.clear();
            for _ in 0..count {
                let [l, r] = self.mix.pop_front().unwrap_or([0.0, 0.0]);
                self.pcm_out.push(float_to_i16(l));
                self.pcm_out.push(float_to_i16(r));
            }
            self.emitted_frames += count as i64;
            write_chunk(&self.pcm_out)?;
        }
        Ok(())
    }
}

fn float_to_i16(sample: f32) -> i16 {
    if sample.is_finite() {
        (sample.clamp(-1.0, 1.0) * 32767.0).round() as i16
    } else {
        0
    }
}

fn decode_sample(b: &[u8], format: AudioSampleFormat) -> f32 {
    match format {
        AudioSampleFormat::I16 => i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0,
        AudioSampleFormat::I24 => {
            (((i32::from(b[0]) << 8) | (i32::from(b[1]) << 16) | (i32::from(b[2]) << 24)) >> 8)
                as f32
                / 8_388_608.0
        }
        AudioSampleFormat::I32 => {
            i32::from_le_bytes([b[0], b[1], b[2], b[3]]) as f32 / 2_147_483_648.0
        }
        AudioSampleFormat::F32 => f32::from_le_bytes([b[0], b[1], b[2], b[3]]),
    }
}

fn decode_to_stereo(
    payload: &[u8],
    frames: usize,
    channels: usize,
    format: AudioSampleFormat,
    out: &mut Vec<[f32; 2]>,
) {
    out.clear();
    let sample_bytes = match format {
        AudioSampleFormat::I16 => 2,
        AudioSampleFormat::I24 => 3,
        AudioSampleFormat::I32 | AudioSampleFormat::F32 => 4,
    };
    let frame_bytes = channels.saturating_mul(sample_bytes);
    if frame_bytes == 0 {
        return;
    }
    let available = frames.min(payload.len() / frame_bytes);
    out.reserve(available);
    const SQ2: f32 = std::f32::consts::FRAC_1_SQRT_2;
    const N3: f32 = 1.0 / (1.0 + SQ2);
    const N5: f32 = 1.0 / (1.0 + 2.0 * SQ2);
    const N7: f32 = 1.0 / (1.0 + 2.5 * SQ2);
    const N8: f32 = 1.0 / (1.0 + 3.0 * SQ2);

    for frame in payload.chunks_exact(frame_bytes).take(available) {
        let ch = |i: usize| decode_sample(&frame[i * sample_bytes..(i + 1) * sample_bytes], format);
        let stereo = match channels {
            1 => [ch(0); 2],
            2 => [ch(0), ch(1)],
            3 => [(ch(0) + ch(2) * SQ2) * N3, (ch(1) + ch(2) * SQ2) * N3],
            4 => [(ch(0) + ch(2) * SQ2) * N3, (ch(1) + ch(3) * SQ2) * N3],
            5 => {
                let c = ch(2) * SQ2;
                [
                    (ch(0) + c + ch(3) * SQ2) * N5,
                    (ch(1) + c + ch(4) * SQ2) * N5,
                ]
            }
            6 => {
                let c = ch(2) * SQ2;
                [
                    (ch(0) + c + ch(4) * SQ2) * N5,
                    (ch(1) + c + ch(5) * SQ2) * N5,
                ]
            }
            7 => {
                let c = ch(2) * SQ2 + ch(4) * (0.5 * SQ2);
                [
                    (ch(0) + c + ch(5) * SQ2) * N7,
                    (ch(1) + c + ch(6) * SQ2) * N7,
                ]
            }
            8 => {
                let c = ch(2) * SQ2;
                [
                    (ch(0) + c + (ch(4) + ch(6)) * SQ2) * N8,
                    (ch(1) + c + (ch(5) + ch(7)) * SQ2) * N8,
                ]
            }
            n => {
                let (mut l, mut r) = (0.0f32, 0.0f32);
                for i in 0..n {
                    if i % 2 == 0 {
                        l += ch(i);
                    } else {
                        r += ch(i);
                    }
                }
                [l / n.div_ceil(2) as f32, r / (n / 2).max(1) as f32]
            }
        };
        out.push(stereo);
    }
}

fn resample_linear(
    input: &[[f32; 2]],
    src_rate: u32,
    dst_rate: u32,
    frac: &mut f64,
    prev: &mut [f32; 2],
    has_prev: &mut bool,
    out: &mut Vec<[f32; 2]>,
) {
    out.clear();
    if input.is_empty() {
        return;
    }
    let step = f64::from(src_rate) / f64::from(dst_rate);
    let mut pos = *frac;
    let last = (input.len() - 1) as f64;
    while pos < last {
        let (a, b, t) = if pos < 0.0 {
            (
                if *has_prev { *prev } else { input[0] },
                input[0],
                (pos + 1.0) as f32,
            )
        } else {
            let idx = pos as usize;
            (input[idx], input[idx + 1], (pos - idx as f64) as f32)
        };
        out.push([a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t]);
        pos += step;
    }
    *frac = pos - input.len() as f64;
    *prev = input[input.len() - 1];
    *has_prev = true;
}

pub(super) struct AudioRecorder {
    tap: Option<Box<dyn AudioSource>>,
    pending: Option<mpsc::Receiver<Result<Box<dyn AudioSource>>>>,
    started: Instant,
    late_start_ms: Option<u64>,
    tap_error: Option<String>,
    mixer: AudioMixer,
    packets: Vec<AudioPacket>,
    got_audio: bool,
    tap_lost: bool,
}

impl AudioRecorder {
    pub(super) fn new(
        pending: mpsc::Receiver<Result<Box<dyn AudioSource>>>,
        started: Instant,
    ) -> Self {
        let qpc_freq = qpc_frequency();
        let mut mixer = AudioMixer::new(qpc_freq);
        let delta = Instant::now()
            .saturating_duration_since(started)
            .as_secs_f64();
        let delta_ticks = (delta * mixer.qpc_freq).round() as i64;
        mixer.set_start_qpc(qpc_now().saturating_sub(delta_ticks));
        let mut recorder = Self {
            tap: None,
            pending: Some(pending),
            started,
            late_start_ms: None,
            tap_error: None,
            mixer,
            packets: Vec::new(),
            got_audio: false,
            tap_lost: false,
        };
        recorder.try_attach(false);
        recorder
    }

    fn try_attach(&mut self, record_late: bool) {
        let Some(rx) = self.pending.as_ref() else {
            return;
        };
        match rx.try_recv() {
            Ok(Ok(tap)) => {
                self.tap = Some(tap);
                self.pending = None;
                if record_late {
                    let ms = self.started.elapsed().as_millis() as u64;
                    if ms >= LATE_START_THRESHOLD_MS {
                        self.late_start_ms = Some(ms);
                    }
                }
            }
            Ok(Err(error)) => {
                self.tap_error = Some(format!("Could not capture process audio: {error:#}"));
                self.pending = None;
            }
            Err(mpsc::TryRecvError::Empty) => {}
            Err(mpsc::TryRecvError::Disconnected) => {
                self.tap_error =
                    Some("Process audio capture worker stopped before becoming ready".into());
                self.pending = None;
            }
        }
    }

    pub(super) fn deactivate(&self) {
        if let Some(tap) = self.tap.as_ref() {
            tap.deactivate();
        }
    }

    pub(super) fn poll(
        &mut self,
        elapsed_ms: u64,
        write_chunk: impl FnMut(&[i16]) -> Result<()>,
    ) -> Result<()> {
        self.try_attach(true);
        if let Some(tap) = self.tap.as_mut() {
            if !tap.is_active() {
                self.tap_lost = true;
            }
            self.packets.clear();
            tap.drain(&mut self.packets);
            if !self.packets.is_empty() {
                self.got_audio = true;
            }
            for packet in &self.packets {
                self.mixer.ingest(packet);
            }
        }
        let target =
            elapsed_ms.saturating_sub(EMIT_LAG_MS) as i64 * i64::from(OUTPUT_SAMPLE_RATE) / 1000;
        self.mixer.emit_up_to(target, true, write_chunk)
    }

    pub(super) fn finish(
        mut self,
        duration_ms: u64,
        mut write_chunk: impl FnMut(&[i16]) -> Result<()>,
    ) -> Result<AudioFinishReport> {
        self.try_attach(true);
        let (had_tap, dropped, skipped_unknown, read_faults) = if let Some(tap) = self.tap.as_mut()
        {
            if !tap.is_active() {
                self.tap_lost = true;
            }
            tap.stop();
            self.packets.clear();
            tap.drain(&mut self.packets);
            if !self.packets.is_empty() {
                self.got_audio = true;
            }
            for packet in &self.packets {
                self.mixer.ingest(packet);
            }
            (
                true,
                tap.dropped(),
                tap.skipped_unknown(),
                tap.read_faults(),
            )
        } else {
            (false, 0, 0, 0)
        };
        let target = (duration_ms as i64 * i64::from(OUTPUT_SAMPLE_RATE) + 500) / 1000;
        self.mixer.emit_up_to(target, false, &mut write_chunk)?;
        Ok(AudioFinishReport {
            had_tap,
            got_audio: self.got_audio,
            late_start_ms: self.late_start_ms,
            tap_error: self.tap_error,
            dropped,
            skipped_unknown,
            read_faults,
            tap_lost: self.tap_lost,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine_packet(
        qpc: i64,
        rate: u32,
        ch: u16,
        frames: u32,
        start: u64,
        fmt: AudioSampleFormat,
    ) -> AudioPacket {
        let mut payload = Vec::new();
        for i in 0..frames {
            let t = (start + u64::from(i)) as f64 / f64::from(rate);
            let v = (0.5 * (2.0 * std::f64::consts::PI * 440.0 * t).sin()) as f32;
            for _ in 0..ch {
                match fmt {
                    AudioSampleFormat::F32 => payload.extend_from_slice(&v.to_le_bytes()),
                    AudioSampleFormat::I16 => {
                        payload.extend_from_slice(&((v * 32767.0).round() as i16).to_le_bytes())
                    }
                    AudioSampleFormat::I24 => payload
                        .extend_from_slice(&((v * 8_388_607.0).round() as i32).to_le_bytes()[..3]),
                    AudioSampleFormat::I32 => payload
                        .extend_from_slice(&((v * 2_147_483_647.0).round() as i32).to_le_bytes()),
                }
            }
        }
        AudioPacket {
            qpc_ticks: qpc,
            stream_id: 1,
            sample_rate: rate,
            frames,
            channels: ch,
            format: fmt,
            silent: false,
            payload,
        }
    }

    fn dft_power(samples: &[f64], rate: f64, hz: f64) -> f64 {
        let w = 2.0 * std::f64::consts::PI * hz / rate;
        let (re, im) = samples
            .iter()
            .enumerate()
            .fold((0.0, 0.0), |(r, m), (i, &s)| {
                let a = w * i as f64;
                (r + s * a.cos(), m + s * a.sin())
            });
        re * re + im * im
    }

    #[test]
    fn resampler_and_formats_produce_clean_440_hz() {
        for (rate, ch, fmt, frames, jitter) in [
            (44_100u32, 1u16, AudioSampleFormat::F32, 441u32, true),
            (44_100, 2, AudioSampleFormat::I16, 441, false),
            (48_000, 6, AudioSampleFormat::F32, 480, false),
        ] {
            let mut mixer = AudioMixer::new(10_000_000);
            mixer.set_start_qpc(1_000_000);
            let mut sample_idx = 0u64;
            for step in 0..100u64 {
                let j = if jitter {
                    if step % 2 == 0 { 40_000 } else { -35_000 }
                } else {
                    0
                };
                let qpc = 1_000_000 + (step * 100_000) as i64 + j;
                mixer.ingest(&sine_packet(qpc, rate, ch, frames, sample_idx, fmt));
                sample_idx += u64::from(frames);
            }
            let mut pcm = Vec::new();
            mixer
                .emit_up_to(48_000, false, |c| {
                    pcm.extend_from_slice(c);
                    Ok(())
                })
                .unwrap();
            let left: Vec<f64> = pcm
                .iter()
                .step_by(2)
                .skip(2_400)
                .take(43_200)
                .map(|&s| f64::from(s))
                .collect();
            let p440 = dft_power(&left, 48_000.0, 440.0);
            for sb in [100.0, 340.0, 540.0, 880.0] {
                let db = 10.0 * (dft_power(&left, 48_000.0, sb) / p440).log10();
                assert!(
                    db < -50.0,
                    "{rate}/{ch}/{fmt:?} {sb}Hz sideband: {db:.1} dB"
                );
            }
        }
    }

    #[test]
    fn silent_44100_packets_do_not_drift_cursor() {
        let mut mixer = AudioMixer::new(10_000_000);
        for i in 0..1000u64 {
            mixer.ingest(&AudioPacket {
                qpc_ticks: ((i * 512) as f64 / 44_100.0 * 10_000_000.0) as i64,
                stream_id: 9,
                sample_rate: 44_100,
                frames: 512,
                channels: 2,
                format: AudioSampleFormat::F32,
                silent: true,
                payload: Vec::new(),
            });
        }
        let expected = 1000.0 * 512.0 * 48_000.0 / 44_100.0;
        assert!((mixer.streams[&9].cursor_frames - expected).abs() < 1.0);
    }

    #[test]
    fn audio_returns_to_timestamp_after_mid_clip_gap() {
        for gap_ms in [35u64, 60, 130] {
            let mut mixer = AudioMixer::new(10_000_000);
            let mut pcm = Vec::new();
            let expected_ms = 80 * 10 + gap_ms;
            for step in 0..100u64 {
                let t_ms = step * 10 + if step >= 30 { gap_ms } else { 0 };
                let payload = if step == 80 {
                    [0.8f32.to_le_bytes()].concat().repeat(960)
                } else {
                    Vec::new()
                };
                mixer.ingest(&AudioPacket {
                    qpc_ticks: (t_ms * 10_000) as i64,
                    stream_id: 5,
                    sample_rate: 48_000,
                    frames: 480,
                    channels: 2,
                    format: AudioSampleFormat::F32,
                    silent: step != 80,
                    payload,
                });
                let target = t_ms.saturating_sub(EMIT_LAG_MS) as i64 * 48;
                mixer
                    .emit_up_to(target, true, |c| {
                        pcm.extend_from_slice(c);
                        Ok(())
                    })
                    .unwrap();
            }
            mixer
                .emit_up_to((1000 + gap_ms) as i64 * 48, false, |c| {
                    pcm.extend_from_slice(c);
                    Ok(())
                })
                .unwrap();
            let onset = pcm
                .chunks_exact(2)
                .position(|lr| lr[0].unsigned_abs() > 16_000)
                .unwrap();
            assert!((onset as f64 / 48.0 - expected_ms as f64).abs() < 2.0);
        }
    }

    #[test]
    fn multi_channel_downmix_includes_center_and_never_clips() {
        for ch in 1u16..=8 {
            let mut out = Vec::new();
            let full: Vec<u8> = (0..ch).flat_map(|_| 1.0f32.to_le_bytes()).collect();
            decode_to_stereo(&full, 1, usize::from(ch), AudioSampleFormat::F32, &mut out);
            assert!(out[0][0] <= 1.0001 && out[0][0] > 0.9);
            if matches!(ch, 3 | 5 | 6 | 7 | 8) {
                let mut center = vec![0u8; usize::from(ch) * 4];
                center[8..12].copy_from_slice(&1.0f32.to_le_bytes());
                decode_to_stereo(
                    &center,
                    1,
                    usize::from(ch),
                    AudioSampleFormat::F32,
                    &mut out,
                );
                assert!(out[0][0] > 0.15 && out[0][1] > 0.15);
            }
        }
    }
}
