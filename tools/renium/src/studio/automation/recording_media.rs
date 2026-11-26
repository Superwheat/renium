use std::ffi::{CStr, c_void};
use std::path::Path;
use std::sync::{Mutex, OnceLock};

use crate::system::LockRecover;
use anyhow::{Context, Result, bail};
use rayon::prelude::*;
use windows::Win32::Media::MediaFoundation::{
    CODECAPI_AVEncCommonQuality, CODECAPI_AVEncCommonRateControlMode,
    CODECAPI_AVEncMPVDefaultBPictureCount, CODECAPI_AVEncMPVGOPSize, IMFAttributes, IMFMediaBuffer,
    IMFMediaType, IMFSample, IMFSinkWriter, MF_LOW_LATENCY,
    MF_MT_AAC_AUDIO_PROFILE_LEVEL_INDICATION, MF_MT_AAC_PAYLOAD_TYPE,
    MF_MT_AUDIO_AVG_BYTES_PER_SECOND, MF_MT_AUDIO_BITS_PER_SAMPLE, MF_MT_AUDIO_BLOCK_ALIGNMENT,
    MF_MT_AUDIO_NUM_CHANNELS, MF_MT_AUDIO_SAMPLES_PER_SECOND, MF_MT_AVG_BITRATE, MF_MT_FRAME_RATE,
    MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE, MF_MT_MPEG2_PROFILE,
    MF_MT_PIXEL_ASPECT_RATIO, MF_MT_SUBTYPE, MF_MT_TRANSFER_FUNCTION, MF_MT_VIDEO_NOMINAL_RANGE,
    MF_MT_VIDEO_PRIMARIES, MF_MT_YUV_MATRIX, MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS,
    MF_TRANSCODE_CONTAINERTYPE, MF_VERSION, MFAudioFormat_AAC, MFAudioFormat_PCM,
    MFMediaType_Audio, MFMediaType_Video, MFNominalRange_16_235, MFTranscodeContainerType_MPEG4,
    MFVideoFormat_H264, MFVideoFormat_NV12, MFVideoInterlace_Progressive,
    MFVideoPrimaries_SMPTE170M, MFVideoTransFunc_709, MFVideoTransferMatrix_BT601,
    eAVEncCommonRateControlMode_Quality, eAVEncH264VProfile_Main,
};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize};
use windows::core::{HRESULT, HSTRING, Interface, PCWSTR};
use windows_sys::Win32::Foundation::HMODULE;
use windows_sys::Win32::System::LibraryLoader::{
    GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW,
};

const TICKS_PER_MILLISECOND: i64 = 10_000;
const TICKS_PER_SECOND: u64 = 10_000_000;
const AUDIO_SAMPLE_RATE: u32 = 48_000;
const AUDIO_CHANNELS: u32 = 2;
const AUDIO_BITS_PER_SAMPLE: u32 = 16;
const AUDIO_BITRATE_BYTES_PER_SEC: u32 = 24_000;
type ProvenProfile = (usize, usize, u32, bool, bool);
static PROVEN: Mutex<Vec<ProvenProfile>> = Mutex::new(Vec::new());
const BITS_PER_PIXEL: f64 = 0.4;

struct Api {
    startup: unsafe extern "system" fn(u32, u32) -> HRESULT,
    shutdown: unsafe extern "system" fn() -> HRESULT,
    attributes: unsafe extern "system" fn(*mut Option<IMFAttributes>, u32) -> HRESULT,
    media_type: unsafe extern "system" fn(*mut Option<IMFMediaType>) -> HRESULT,
    memory_buffer: unsafe extern "system" fn(u32, *mut Option<IMFMediaBuffer>) -> HRESULT,
    sample: unsafe extern "system" fn(*mut Option<IMFSample>) -> HRESULT,
    sink_writer: unsafe extern "system" fn(
        PCWSTR,
        *mut c_void,
        *mut c_void,
        *mut Option<IMFSinkWriter>,
    ) -> HRESULT,
}

fn library(name: &str) -> Option<HMODULE> {
    let name: Vec<u16> = name.encode_utf16().chain([0]).collect();
    let module = unsafe {
        LoadLibraryExW(
            name.as_ptr(),
            std::ptr::null_mut(),
            LOAD_LIBRARY_SEARCH_SYSTEM32,
        )
    };
    (!module.is_null()).then_some(module)
}

fn entry<T: Copy>(module: HMODULE, name: &CStr) -> Option<T> {
    let found = unsafe { GetProcAddress(module, name.as_ptr().cast()) }?;
    assert_eq!(size_of::<T>(), size_of_val(&found));
    Some(unsafe { std::mem::transmute_copy(&found) })
}

fn api() -> Option<&'static Api> {
    static API: OnceLock<Option<Api>> = OnceLock::new();
    API.get_or_init(|| {
        let platform = library("mfplat.dll")?;
        let readwrite = library("mfreadwrite.dll")?;
        Some(Api {
            startup: entry(platform, c"MFStartup")?,
            shutdown: entry(platform, c"MFShutdown")?,
            attributes: entry(platform, c"MFCreateAttributes")?,
            media_type: entry(platform, c"MFCreateMediaType")?,
            memory_buffer: entry(platform, c"MFCreateMemoryBuffer")?,
            sample: entry(platform, c"MFCreateSample")?,
            sink_writer: entry(readwrite, c"MFCreateSinkWriterFromURL")?,
        })
    })
    .as_ref()
}

fn made<T>(make: impl FnOnce(*mut Option<T>) -> HRESULT) -> Result<T> {
    let mut value = None;
    make(&mut value).ok()?;
    value.context("Media Foundation returned no object")
}

impl Api {
    fn new_attributes(&self, size: u32) -> Result<IMFAttributes> {
        made(|value| unsafe { (self.attributes)(value, size) })
    }

    fn new_media_type(&self) -> Result<IMFMediaType> {
        made(|value| unsafe { (self.media_type)(value) })
    }
}

fn pair(high: u32, low: u32) -> u64 {
    u64::from(high) << 32 | u64::from(low)
}

pub(super) fn nv12(pixels: &[u8], width: usize, height: usize, bgra: bool, out: &mut [u8]) {
    let (red, blue) = if bgra { (2, 0) } else { (0, 2) };
    let (luma, chroma) = out.split_at_mut(width * height);
    luma.par_chunks_mut(width * 2)
        .zip(chroma.par_chunks_mut(width))
        .zip(pixels.par_chunks(width * 8))
        .for_each(|((luma, chroma), rows)| {
            let (top, bottom) = rows.split_at(width * 4);
            let (luma_top, luma_bottom) = luma.split_at_mut(width);
            for left in (0..width).step_by(2) {
                let mut sum = [0i32; 3];
                for (row, luma) in [(top, &mut *luma_top), (bottom, &mut *luma_bottom)] {
                    for x in left..left + 2 {
                        let pixel = &row[x * 4..x * 4 + 4];
                        let (r, g, b) = (
                            i32::from(pixel[red]),
                            i32::from(pixel[1]),
                            i32::from(pixel[blue]),
                        );
                        luma[x] = (((66 * r + 129 * g + 25 * b + 128) >> 8) + 16) as u8;
                        sum[0] += r;
                        sum[1] += g;
                        sum[2] += b;
                    }
                }
                let [r, g, b] = sum;
                chroma[left] = (((-38 * r - 74 * g + 112 * b + 512) >> 10) + 128) as u8;
                chroma[left + 1] = (((112 * r - 94 * g - 18 * b + 512) >> 10) + 128) as u8;
            }
        });
}

pub(super) struct Settings {
    pub(super) width: usize,
    pub(super) height: usize,
    pub(super) fps: f64,
    pub(super) quality: f32,
    pub(super) key_frame_every: u32,
    pub(super) audio: bool,
}

pub(super) struct MediaFile {
    api: &'static Api,
    writer: Option<IMFSinkWriter>,
    stream: u32,
    audio_stream: Option<u32>,
    audio_frames_written: u64,
    audio_failed: bool,
    width: usize,
    height: usize,
    pending: Option<(IMFSample, u64)>,
    started: bool,
    apartment: bool,
}

impl MediaFile {
    pub(super) fn start(path: &Path, settings: &Settings) -> Result<Self> {
        let api = api().context("Media Foundation is not installed")?;
        let apartment = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok();
        let mut file = Self {
            api,
            writer: None,
            stream: 0,
            audio_stream: None,
            audio_frames_written: 0,
            audio_failed: false,
            width: settings.width,
            height: settings.height,
            pending: None,
            started: false,
            apartment,
        };
        unsafe { (api.startup)(MF_VERSION, 0) }.ok()?;
        file.started = true;
        for hardware in [true, false] {
            let rate = (settings.fps * 1000.0).round() as u32;
            let size = (
                settings.width,
                settings.height,
                rate,
                hardware,
                settings.audio,
            );
            let known = PROVEN.lock_recover().contains(&size);
            let opened = if known {
                file.open(path, settings, hardware)
            } else {
                file.prove(path, settings, hardware)
            };
            match opened {
                Ok(()) if known => return Ok(file),
                Ok(()) => {
                    PROVEN.lock_recover().push(size);
                    return Ok(file);
                }
                Err(_) => {
                    file.writer = None;
                    file.audio_stream = None;
                    file.audio_frames_written = 0;
                }
            }
        }
        bail!("No system encoder takes this video")
    }

    fn prove(&mut self, path: &Path, settings: &Settings, hardware: bool) -> Result<()> {
        self.pending = None;
        self.writer = None;
        self.audio_stream = None;
        self.audio_frames_written = 0;
        self.open(path, settings, hardware)?;
        self.add(&vec![0; self.width * self.height * 4], true, 0)?;
        self.write(1)?;
        if settings.audio {
            self.add_audio(&[0i16; 2048])?;
        }
        unsafe { self.writer()?.Finalize()? };
        self.writer = None;
        self.audio_stream = None;
        self.audio_frames_written = 0;
        self.open(path, settings, hardware)
    }

    fn open(&mut self, path: &Path, settings: &Settings, hardware: bool) -> Result<()> {
        let api = self.api;
        let (width, height) = (
            u32::try_from(settings.width)?,
            u32::try_from(settings.height)?,
        );
        let rate = pair((settings.fps * 1000.0).round() as u32, 1000);
        let bits = (settings.width * settings.height) as f64 * settings.fps * BITS_PER_PIXEL;
        let colors = |kind: &IMFMediaType| -> Result<()> {
            unsafe {
                kind.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
                kind.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
                kind.SetUINT64(&MF_MT_FRAME_SIZE, pair(width, height))?;
                kind.SetUINT64(&MF_MT_FRAME_RATE, rate)?;
                kind.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pair(1, 1))?;
                kind.SetUINT32(&MF_MT_VIDEO_PRIMARIES, MFVideoPrimaries_SMPTE170M.0 as u32)?;
                kind.SetUINT32(&MF_MT_TRANSFER_FUNCTION, MFVideoTransFunc_709.0 as u32)?;
                kind.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT601.0 as u32)?;
                kind.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32)?;
            }
            Ok(())
        };
        unsafe {
            let options = api.new_attributes(3)?;
            options.SetUINT32(
                &MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS,
                u32::from(hardware),
            )?;
            options.SetUINT32(&MF_LOW_LATENCY, 1)?;
            options.SetGUID(&MF_TRANSCODE_CONTAINERTYPE, &MFTranscodeContainerType_MPEG4)?;
            let path = crate::system::files::strip_extended_prefix(path.to_path_buf());
            let url = HSTRING::from(path.as_os_str());
            let writer = made(|value| {
                (api.sink_writer)(
                    PCWSTR(url.as_ptr()),
                    std::ptr::null_mut(),
                    options.as_raw(),
                    value,
                )
            })?;
            let output = api.new_media_type()?;
            colors(&output)?;
            output.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
            output.SetUINT32(&MF_MT_AVG_BITRATE, bits.min(240_000_000.0) as u32)?;
            output.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_Main.0 as u32)?;
            self.stream = writer.AddStream(&output)?;
            let input = api.new_media_type()?;
            colors(&input)?;
            input.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
            let encoder = api.new_attributes(4)?;
            encoder.SetUINT32(
                &CODECAPI_AVEncCommonRateControlMode,
                eAVEncCommonRateControlMode_Quality.0 as u32,
            )?;
            encoder.SetUINT32(
                &CODECAPI_AVEncCommonQuality,
                settings.quality.round().clamp(1.0, 100.0) as u32,
            )?;
            encoder.SetUINT32(&CODECAPI_AVEncMPVGOPSize, settings.key_frame_every)?;
            encoder.SetUINT32(&CODECAPI_AVEncMPVDefaultBPictureCount, 0)?;
            writer.SetInputMediaType(self.stream, &input, &encoder)?;

            self.audio_stream = if settings.audio {
                let audio_out = api.new_media_type()?;
                audio_out.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
                audio_out.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_AAC)?;
                audio_out.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, AUDIO_BITS_PER_SAMPLE)?;
                audio_out.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, AUDIO_SAMPLE_RATE)?;
                audio_out.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, AUDIO_CHANNELS)?;
                audio_out.SetUINT32(
                    &MF_MT_AUDIO_AVG_BYTES_PER_SECOND,
                    AUDIO_BITRATE_BYTES_PER_SEC,
                )?;
                audio_out.SetUINT32(&MF_MT_AAC_PAYLOAD_TYPE, 0)?;
                audio_out.SetUINT32(&MF_MT_AAC_AUDIO_PROFILE_LEVEL_INDICATION, 0x29)?;
                let audio_stream = writer.AddStream(&audio_out)?;

                let block_align = AUDIO_CHANNELS * (AUDIO_BITS_PER_SAMPLE / 8);
                let audio_in = api.new_media_type()?;
                audio_in.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
                audio_in.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_PCM)?;
                audio_in.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, AUDIO_BITS_PER_SAMPLE)?;
                audio_in.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, AUDIO_SAMPLE_RATE)?;
                audio_in.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, AUDIO_CHANNELS)?;
                audio_in.SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, block_align)?;
                audio_in.SetUINT32(
                    &MF_MT_AUDIO_AVG_BYTES_PER_SECOND,
                    AUDIO_SAMPLE_RATE * block_align,
                )?;
                writer.SetInputMediaType(audio_stream, &audio_in, None)?;
                Some(audio_stream)
            } else {
                None
            };
            self.audio_frames_written = 0;

            writer.BeginWriting()?;
            self.writer = Some(writer);
        }
        Ok(())
    }

    pub(super) fn has_audio(&self) -> bool {
        self.audio_stream.is_some() && !self.audio_failed
    }

    pub(super) fn add_audio(&mut self, pcm_i16: &[i16]) -> Result<()> {
        if self.audio_failed {
            return Ok(());
        }
        let Some(audio_stream) = self.audio_stream else {
            return Ok(());
        };
        if pcm_i16.is_empty() {
            return Ok(());
        }
        let frames = (pcm_i16.len() / AUDIO_CHANNELS as usize) as u64;
        if frames == 0 {
            return Ok(());
        }
        let byte_len = std::mem::size_of_val(pcm_i16);
        let size = u32::try_from(byte_len).context("Recording audio chunk is too large")?;
        let start_ticks =
            self.audio_frames_written * TICKS_PER_SECOND / u64::from(AUDIO_SAMPLE_RATE);
        let next_frames = self.audio_frames_written + frames;
        let end_ticks = next_frames * TICKS_PER_SECOND / u64::from(AUDIO_SAMPLE_RATE);
        let duration_ticks = end_ticks.saturating_sub(start_ticks).max(1);
        let write = (|| -> Result<()> {
            unsafe {
                let buffer = made(|value| (self.api.memory_buffer)(size, value))?;
                let mut bytes = std::ptr::null_mut();
                buffer.Lock(&mut bytes, None, None)?;
                std::ptr::copy_nonoverlapping(pcm_i16.as_ptr().cast::<u8>(), bytes, byte_len);
                buffer.Unlock()?;
                buffer.SetCurrentLength(size)?;
                let sample = made(|value| (self.api.sample)(value))?;
                sample.AddBuffer(&buffer)?;
                sample.SetSampleTime(i64::try_from(start_ticks)?)?;
                sample.SetSampleDuration(i64::try_from(duration_ticks)?)?;
                self.writer()?.WriteSample(audio_stream, &sample)?;
            }
            Ok(())
        })();
        if let Err(error) = write {
            self.audio_failed = true;
            return Err(error);
        }
        self.audio_frames_written = next_frames;
        Ok(())
    }

    fn writer(&self) -> Result<&IMFSinkWriter> {
        self.writer.as_ref().context("The video file is not open")
    }

    fn write(&mut self, until: u64) -> Result<()> {
        let Some((sample, from)) = self.pending.take() else {
            return Ok(());
        };
        let length = i64::try_from(until.saturating_sub(from).max(1))?;
        unsafe {
            sample.SetSampleDuration(length * TICKS_PER_MILLISECOND)?;
            self.writer()?.WriteSample(self.stream, &sample)?;
        }
        Ok(())
    }

    pub(super) fn add(&mut self, pixels: &[u8], bgra: bool, at: u64) -> Result<()> {
        if pixels.len() != self.width * self.height * 4 {
            bail!("Recording frame does not fit the video");
        }
        let length = self.width * self.height * 3 / 2;
        let size = u32::try_from(length).context("Recording frame is too large")?;
        let sample = unsafe {
            let buffer = made(|value| (self.api.memory_buffer)(size, value))?;
            let mut bytes = std::ptr::null_mut();
            buffer.Lock(&mut bytes, None, None)?;
            nv12(
                pixels,
                self.width,
                self.height,
                bgra,
                std::slice::from_raw_parts_mut(bytes, length),
            );
            buffer.Unlock()?;
            buffer.SetCurrentLength(size)?;
            let sample = made(|value| (self.api.sample)(value))?;
            sample.AddBuffer(&buffer)?;
            sample.SetSampleTime(i64::try_from(at)? * TICKS_PER_MILLISECOND)?;
            sample
        };
        self.write(at)?;
        self.pending = Some((sample, at));
        Ok(())
    }

    pub(super) fn end(mut self, duration_ms: u64) -> Result<()> {
        self.write(duration_ms)?;
        unsafe { self.writer()?.Finalize()? };
        Ok(())
    }
}

impl Drop for MediaFile {
    fn drop(&mut self) {
        self.pending = None;
        self.writer = None;
        unsafe {
            if self.started {
                let _ = (self.api.shutdown)();
            }
            if self.apartment {
                CoUninitialize();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pictures_are_converted_in_either_channel_order() {
        let colors = [[255u8, 255, 255], [0, 0, 0], [255, 0, 0], [0, 0, 255]];
        let expected = [
            (235u8, 128u8, 128u8),
            (16, 128, 128),
            (82, 90, 240),
            (41, 240, 110),
        ];
        for bgra in [false, true] {
            for (color, (y, u, v)) in colors.iter().zip(expected) {
                let pixel = if bgra {
                    [color[2], color[1], color[0], 0]
                } else {
                    [color[0], color[1], color[2], 255]
                };
                let mut out = [0u8; 24];
                nv12(&pixel.repeat(16), 4, 4, bgra, &mut out);
                assert_eq!(out[..16], [y; 16], "{color:?}");
                assert_eq!(out[16..], [u, v].repeat(4), "{color:?}");
            }
        }
    }
}
