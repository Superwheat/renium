use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use super::*;
use crate::studio::audio::Action;

const HELPER: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/renium-audio.dll"));
const SIZE: usize = 320;
const TAP_SIZE: usize = 568;
const TAP_MAGIC: u32 = 0x5241_5544;
const TAP_VERSION: u32 = 4;
const TAP_RING_CAPACITY: u32 = 4 * 1024 * 1024;
const TAP_PACKET_HEADER_SIZE: usize = 40;
const TAP_READY_OFFSET: u64 = 36;

static TAP_COUNTER: AtomicU64 = AtomicU64::new(1);
static LOCAL_RENDER_VTABLE: OnceLock<Option<([u32; 3], usize)>> = OnceLock::new();

#[repr(C)]
struct TapRingHeader {
    magic: u32,
    version: u32,
    capacity: u32,
    active: AtomicU32,
    write_pos: AtomicU64,
    read_pos: AtomicU64,
    dropped: AtomicU64,
    skipped_unknown: AtomicU64,
    read_faults: AtomicU64,
    reserved: u64,
}

const _: () = assert!(std::mem::size_of::<TapRingHeader>() == 64);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AudioSampleFormat {
    I16,
    I24,
    I32,
    F32,
}

pub(crate) struct AudioPacket {
    pub(crate) qpc_ticks: i64,
    pub(crate) stream_id: u64,
    pub(crate) sample_rate: u32,
    pub(crate) frames: u32,
    pub(crate) channels: u16,
    pub(crate) format: AudioSampleFormat,
    pub(crate) silent: bool,
    pub(crate) payload: Vec<u8>,
}

fn ensure_audio_helper_path() -> Result<PathBuf> {
    let directory = crate::app::update::user_data_dir()?.join("native");
    fs::create_dir_all(&directory)?;
    let path = directory.join(format!("renium-audio-{:016x}.dll", fnv1a(HELPER)));
    if fs::read(&path).ok().as_deref() != Some(HELPER) {
        atomic_write_file(&path, HELPER)?;
    }
    Ok(path)
}

fn helper_export_rva(path: &Path, name: &std::ffi::CStr) -> Result<usize> {
    use windows_sys::Win32::Foundation::FreeLibrary;
    use windows_sys::Win32::System::LibraryLoader::{DONT_RESOLVE_DLL_REFERENCES, LoadLibraryExW};
    let local = unsafe {
        LoadLibraryExW(
            wide(path.as_os_str()).as_ptr(),
            null_mut(),
            DONT_RESOLVE_DLL_REFERENCES,
        )
    };
    anyhow::ensure!(!local.is_null(), "Could not read the Studio audio helper");
    let entry = unsafe { GetProcAddress(local, name.as_ptr().cast()) }
        .map(|address| address as usize - local as usize);
    unsafe {
        FreeLibrary(local);
    }
    entry.context("Studio audio helper has no entry point")
}

fn local_render_vtable_rva() -> Option<([u32; 3], usize)> {
    *LOCAL_RENDER_VTABLE.get_or_init(|| {
        use windows::Win32::Media::Audio::{
            AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_NOPERSIST, IAudioClient,
            IAudioRenderClient, IMMDeviceEnumerator, MMDeviceEnumerator, eConsole, eRender,
        };
        use windows::Win32::System::Com::{
            CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
            CoUninitialize,
        };
        use windows::core::Interface;

        let com = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        let result = (|| -> Result<([u32; 3], usize)> {
            let enumerator: IMMDeviceEnumerator =
                unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }?;
            let device = unsafe { enumerator.GetDefaultAudioEndpoint(eRender, eConsole) }?;
            let client: IAudioClient = unsafe { device.Activate(CLSCTX_ALL, None) }?;
            let mix = unsafe { client.GetMixFormat() }?;
            let init = unsafe {
                client.Initialize(
                    AUDCLNT_SHAREMODE_SHARED,
                    AUDCLNT_STREAMFLAGS_NOPERSIST,
                    1_000_000,
                    0,
                    mix,
                    None,
                )
            };
            unsafe { CoTaskMemFree(Some(mix.cast())) };
            init?;
            let render: IAudioRenderClient = unsafe { client.GetService() }?;
            let vtbl = unsafe { *(Interface::as_raw(&render) as *const usize) };
            let local_mods = modules(std::process::id())?;
            let audio = local_mods
                .iter()
                .find(|m| m.name.eq_ignore_ascii_case("audioses.dll"))
                .context("Local audioses.dll not loaded")?;
            anyhow::ensure!(
                vtbl >= audio.base && vtbl + 40 <= audio.base + audio.size,
                "Render vtable lies outside audioses.dll"
            );
            let mapped = ImageView::open(&audio.path)?;
            let image = PeImage::parse(mapped.bytes())?;
            let self_mem = ProcessMemory::open_read_only(std::process::id())?;
            verify_loaded_image(&self_mem, audio, image.image_stamp)?;
            Ok((image.image_stamp, vtbl - audio.base))
        })();
        if com.is_ok() {
            unsafe { CoUninitialize() };
        }
        result.ok()
    })
}

fn resolve_audioses_render_originals(
    process: &ProcessMemory,
    loaded: &[ModuleEntry],
) -> (u64, u64, u64) {
    let resolved = (|| -> Result<(u64, u64, u64)> {
        let audio = loaded
            .iter()
            .find(|m| m.name.eq_ignore_ascii_case("audioses.dll"))
            .context("Target audioses.dll not loaded")?;
        let mapped = ImageView::open(&audio.path)?;
        let image = PeImage::parse(mapped.bytes())?;
        verify_loaded_image(process, audio, image.image_stamp)?;
        let (local_stamp, vtbl_rva) =
            local_render_vtable_rva().context("Local render vtable RVA unavailable")?;
        anyhow::ensure!(
            local_stamp == image.image_stamp,
            "Local and target audioses.dll builds differ"
        );
        let get_va = read_u64(image.bytes, image.rva_to_offset(vtbl_rva + 24)?)? as usize;
        let rel_va = read_u64(image.bytes, image.rva_to_offset(vtbl_rva + 32)?)? as usize;
        let get_rva = get_va
            .checked_sub(image.image_base)
            .context("GetBuffer VA below ImageBase")?;
        let rel_rva = rel_va
            .checked_sub(image.image_base)
            .context("ReleaseBuffer VA below ImageBase")?;
        image.require_executable_rva(get_rva)?;
        image.require_executable_rva(rel_rva)?;
        Ok((
            (audio.base + vtbl_rva) as u64,
            (audio.base + get_rva) as u64,
            (audio.base + rel_rva) as u64,
        ))
    })();
    resolved.unwrap_or((0, 0, 0))
}

pub(crate) struct AudioOutputGate {
    process: ProcessMemory,
    entry: usize,
    render_vtbl: u64,
    system_release: u64,
}

pub(crate) struct OutputState {
    pub(crate) focused: bool,
    pub(crate) muted: bool,
    pub(crate) hooks: u32,
    pub(crate) buffers: u64,
    pub(crate) silenced: u64,
}

impl AudioOutputGate {
    pub(crate) fn new(pid: u32) -> Result<Self> {
        let path = ensure_audio_helper_path()?;
        let entry = helper_export_rva(&path, c"ReniumAudioStep")?;
        let process = ProcessMemory::open_for_windows_audio(pid)?;
        let loaded = modules(pid)?;
        let base = ensure_library_loaded(pid, &process, &loaded, 5_000, &path)?;
        let (render_vtbl, _, system_release) = resolve_audioses_render_originals(&process, &loaded);
        Ok(Self {
            process,
            entry: base + entry,
            render_vtbl,
            system_release,
        })
    }

    pub(crate) fn step(&self, action: Action, refresh: bool) -> Result<OutputState> {
        let mut bytes = [0; SIZE];
        put_u32(&mut bytes, 0, SIZE as u32);
        put_u32(&mut bytes, 4, 2);
        put_u32(
            &mut bytes,
            8,
            match action {
                Action::Status => 0,
                Action::Off => 1,
                Action::Mute => 2,
                Action::Unmute => 3,
                Action::Auto => 4,
            },
        );
        put_u32(&mut bytes, 12, u32::from(refresh));
        bytes[304..312].copy_from_slice(&self.render_vtbl.to_le_bytes());
        bytes[312..320].copy_from_slice(&self.system_release.to_le_bytes());
        let mut remote = self.process.allocate(SIZE)?;
        self.process.write(remote.address, &bytes)?;
        let result = remote.run(self.entry, 3_000)?;
        anyhow::ensure!(
            result == 0,
            "Studio audio helper rejected its request ({result})"
        );
        self.process.read(remote.address, &mut bytes)?;
        anyhow::ensure!(
            read_u32(&bytes, 28)? == 1,
            "{}",
            String::from_utf8_lossy(&bytes[48..304]).trim_end_matches('\0')
        );
        Ok(OutputState {
            focused: read_u32(&bytes, 16)? != 0,
            muted: read_u32(&bytes, 20)? != 0,
            hooks: read_u32(&bytes, 24)?,
            buffers: read_u64(&bytes, 32)?,
            silenced: read_u64(&bytes, 40)?,
        })
    }
}

impl Drop for AudioOutputGate {
    fn drop(&mut self) {
        let _ = self.step(Action::Off, false);
    }
}

fn ring_read(data: *const u8, capacity: u32, pos: u64, dst: &mut [u8]) {
    if dst.is_empty() {
        return;
    }
    let start = (pos as u32 & (capacity - 1)) as usize;
    let first = dst.len().min(capacity as usize - start);
    unsafe {
        std::ptr::copy_nonoverlapping(data.add(start), dst.as_mut_ptr(), first);
        if first < dst.len() {
            std::ptr::copy_nonoverlapping(data, dst.as_mut_ptr().add(first), dst.len() - first);
        }
    }
}

fn ring_zero(data: *mut u8, capacity: u32, pos: u64, len: usize) {
    if len == 0 {
        return;
    }
    let start = (pos as u32 & (capacity - 1)) as usize;
    let first = len.min(capacity as usize - start);
    unsafe {
        std::ptr::write_bytes(data.add(start), 0, first);
        if first < len {
            std::ptr::write_bytes(data, 0, len - first);
        }
    }
}

fn parse_sample_format(code: u8) -> Option<AudioSampleFormat> {
    match code {
        1 => Some(AudioSampleFormat::I16),
        2 => Some(AudioSampleFormat::I24),
        3 => Some(AudioSampleFormat::I32),
        4 => Some(AudioSampleFormat::F32),
        _ => None,
    }
}

unsafe fn drain_ring_raw(view: *mut TapRingHeader, out: &mut Vec<AudioPacket>) {
    let ring = unsafe { &*view };
    let capacity = ring.capacity;
    if ring.magic != TAP_MAGIC
        || ring.version != TAP_VERSION
        || capacity < 64
        || !capacity.is_power_of_two()
    {
        return;
    }
    let data = unsafe { view.add(1).cast::<u8>() };
    let mask = capacity - 1;
    let write_pos = ring.write_pos.load(Ordering::Acquire);
    let mut read_pos = ring.read_pos.load(Ordering::Relaxed);
    let mut hdr = [0u8; TAP_PACKET_HEADER_SIZE];
    while write_pos.saturating_sub(read_pos) >= TAP_PACKET_HEADER_SIZE as u64 {
        let ready_off = ((read_pos + TAP_READY_OFFSET) as u32 & mask) as usize;
        let ready = unsafe { (&*data.add(ready_off).cast::<AtomicU32>()).load(Ordering::Acquire) };
        if ready == 0 {
            break;
        }
        ring_read(data, capacity, read_pos, &mut hdr);
        let packet_bytes = u32::from_le_bytes(hdr[0..4].try_into().unwrap());
        let payload_bytes = u32::from_le_bytes(hdr[4..8].try_into().unwrap());
        if (packet_bytes as usize) < TAP_PACKET_HEADER_SIZE
            || packet_bytes > capacity
            || u64::from(packet_bytes) > write_pos.saturating_sub(read_pos)
            || (payload_bytes as usize) > (packet_bytes as usize - TAP_PACKET_HEADER_SIZE)
        {
            let span = (write_pos.saturating_sub(read_pos)).min(u64::from(capacity)) as usize;
            ring_zero(data, capacity, read_pos, span);
            read_pos = write_pos;
            break;
        }
        if let Some(format) = parse_sample_format(hdr[34]) {
            let mut payload = vec![0u8; payload_bytes as usize];
            if payload_bytes > 0 {
                ring_read(
                    data,
                    capacity,
                    read_pos + TAP_PACKET_HEADER_SIZE as u64,
                    &mut payload,
                );
            }
            out.push(AudioPacket {
                qpc_ticks: i64::from_le_bytes(hdr[8..16].try_into().unwrap()),
                stream_id: u64::from_le_bytes(hdr[16..24].try_into().unwrap()),
                sample_rate: u32::from_le_bytes(hdr[24..28].try_into().unwrap()),
                frames: u32::from_le_bytes(hdr[28..32].try_into().unwrap()),
                channels: u16::from_le_bytes(hdr[32..34].try_into().unwrap()),
                format,
                silent: (hdr[35] & 1) != 0,
                payload,
            });
        }
        ring_zero(data, capacity, read_pos, packet_bytes as usize);
        read_pos = read_pos.saturating_add(u64::from(packet_bytes));
    }
    ring.read_pos.store(read_pos, Ordering::Release);
}

pub(crate) fn drain_ring_bytes(ring_bytes: &mut [u8], out: &mut Vec<AudioPacket>) {
    let header_len = std::mem::size_of::<TapRingHeader>();
    if ring_bytes.len() < header_len
        || !(ring_bytes.as_ptr() as usize).is_multiple_of(std::mem::align_of::<TapRingHeader>())
    {
        return;
    }
    let view = ring_bytes.as_mut_ptr().cast::<TapRingHeader>();
    let capacity = unsafe { (*view).capacity } as usize;
    if ring_bytes.len() < header_len.saturating_add(capacity) {
        return;
    }
    unsafe { drain_ring_raw(view, out) };
}

pub(crate) struct AudioTap {
    process: ProcessMemory,
    entry: usize,
    tap_id: u64,
    mapping: HANDLE,
    view: *mut TapRingHeader,
    stopped: bool,
}

// SAFETY: AudioTap owns its process handle, file mapping handle, and mapped view exclusively.
unsafe impl Send for AudioTap {}

impl AudioTap {
    pub(crate) fn start(pid: u32) -> Result<Self> {
        use windows_sys::Win32::System::Memory::{
            CreateFileMappingW, FILE_MAP_ALL_ACCESS, MapViewOfFile, UnmapViewOfFile,
        };
        let path = ensure_audio_helper_path()?;
        let entry = helper_export_rva(&path, c"ReniumAudioTap")?;
        let process = ProcessMemory::open_for_windows_audio(pid)?;
        let loaded = modules(pid)?;
        let base = ensure_library_loaded(pid, &process, &loaded, 5_000, &path)?;
        let originals = resolve_audioses_render_originals(&process, &loaded);

        let seq = TAP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let tap_id = (u64::from(std::process::id()) << 32) | (seq & 0xFFFF_FFFF);
        let name = format!("Local\\renium-audio-tap-{}-{pid}-{seq}", std::process::id());
        let wide_name: Vec<u16> = name.encode_utf16().chain([0]).collect();
        anyhow::ensure!(wide_name.len() <= 128, "Audio tap mapping name is too long");

        let total_bytes = (std::mem::size_of::<TapRingHeader>() as u32)
            .checked_add(TAP_RING_CAPACITY)
            .context("Audio tap buffer size overflow")?;
        let mapping = unsafe {
            CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                null(),
                PAGE_READWRITE,
                0,
                total_bytes,
                wide_name.as_ptr(),
            )
        };
        anyhow::ensure!(
            !mapping.is_null(),
            "Could not create audio capture buffer: {}",
            std::io::Error::last_os_error()
        );
        let raw_view =
            unsafe { MapViewOfFile(mapping, FILE_MAP_ALL_ACCESS, 0, 0, total_bytes as usize) };
        if raw_view.Value.is_null() {
            let err = std::io::Error::last_os_error();
            unsafe { CloseHandle(mapping) };
            bail!("Could not map audio capture buffer: {err}");
        }
        let view = raw_view.Value.cast::<TapRingHeader>();
        unsafe {
            std::ptr::write_bytes(view.cast::<u8>(), 0, std::mem::size_of::<TapRingHeader>());
            (*view).magic = TAP_MAGIC;
            (*view).version = TAP_VERSION;
            (*view).capacity = TAP_RING_CAPACITY;
            (*view).active.store(1, Ordering::Relaxed);
        }

        let mut tap = Self {
            process,
            entry: base + entry,
            tap_id,
            mapping,
            view,
            stopped: false,
        };
        if let Err(error) = tap.invoke(1, Some(&wide_name), originals) {
            tap.stopped = true;
            unsafe {
                UnmapViewOfFile(
                    windows_sys::Win32::System::Memory::MEMORY_MAPPED_VIEW_ADDRESS {
                        Value: tap.view.cast(),
                    },
                );
                CloseHandle(tap.mapping);
            }
            tap.view = null_mut();
            tap.mapping = null_mut();
            return Err(error);
        }
        Ok(tap)
    }

    fn invoke(
        &self,
        action: u32,
        wide_name: Option<&[u16]>,
        originals: (u64, u64, u64),
    ) -> Result<()> {
        let mut bytes = [0u8; TAP_SIZE];
        put_u32(&mut bytes, 0, TAP_SIZE as u32);
        put_u32(&mut bytes, 4, TAP_VERSION);
        put_u32(&mut bytes, 8, action);
        put_u32(&mut bytes, 16, std::process::id());
        bytes[24..32].copy_from_slice(&self.tap_id.to_le_bytes());
        bytes[32..40].copy_from_slice(&originals.0.to_le_bytes());
        bytes[40..48].copy_from_slice(&originals.1.to_le_bytes());
        bytes[48..56].copy_from_slice(&originals.2.to_le_bytes());
        if let Some(name) = wide_name {
            for (index, &unit) in name.iter().take(127).enumerate() {
                bytes[56 + index * 2..56 + index * 2 + 2].copy_from_slice(&unit.to_le_bytes());
            }
        }
        let mut remote = self.process.allocate(TAP_SIZE)?;
        self.process.write(remote.address, &bytes)?;
        let result = remote.run(self.entry, 3_000)?;
        anyhow::ensure!(
            result == 0,
            "Studio audio capture helper rejected its request ({result})"
        );
        self.process.read(remote.address, &mut bytes)?;
        anyhow::ensure!(
            read_u32(&bytes, 12)? == 1,
            "{}",
            String::from_utf8_lossy(&bytes[312..]).trim_end_matches('\0')
        );
        Ok(())
    }

    pub(crate) fn is_active(&self) -> bool {
        if self.view.is_null() {
            return false;
        }
        let ring = unsafe { &*self.view };
        let running = unsafe { WaitForSingleObject(self.process.handle, 0) } == WAIT_TIMEOUT;
        running && ring.active.load(Ordering::Acquire) != 0
    }

    pub(crate) fn dropped(&self) -> u64 {
        if self.view.is_null() {
            return 0;
        }
        unsafe { (*self.view).dropped.load(Ordering::Acquire) }
    }

    pub(crate) fn skipped_unknown(&self) -> u64 {
        if self.view.is_null() {
            return 0;
        }
        unsafe { (*self.view).skipped_unknown.load(Ordering::Acquire) }
    }

    pub(crate) fn read_faults(&self) -> u64 {
        if self.view.is_null() {
            return 0;
        }
        unsafe { (*self.view).read_faults.load(Ordering::Acquire) }
    }

    pub(crate) fn deactivate(&self) {
        if !self.view.is_null() {
            unsafe {
                (*self.view).active.store(0, Ordering::Release);
            }
        }
    }

    pub(crate) fn drain(&mut self, out: &mut Vec<AudioPacket>) {
        if !self.view.is_null() {
            let total = std::mem::size_of::<TapRingHeader>() + TAP_RING_CAPACITY as usize;
            let slice = unsafe { std::slice::from_raw_parts_mut(self.view.cast::<u8>(), total) };
            drain_ring_bytes(slice, out);
        }
    }

    pub(crate) fn stop(&mut self) {
        if self.stopped {
            return;
        }
        self.stopped = true;
        self.deactivate();
        let _ = self.invoke(2, None, (0, 0, 0));
    }
}

impl Drop for AudioTap {
    fn drop(&mut self) {
        use windows_sys::Win32::System::Memory::UnmapViewOfFile;
        self.stop();
        if !self.view.is_null() {
            unsafe {
                UnmapViewOfFile(
                    windows_sys::Win32::System::Memory::MEMORY_MAPPED_VIEW_ADDRESS {
                        Value: self.view.cast(),
                    },
                );
            }
            self.view = null_mut();
        }
        if !self.mapping.is_null() {
            unsafe { CloseHandle(self.mapping) };
            self.mapping = null_mut();
        }
    }
}

#[test]
fn output_gate_preserves_forwarding_focus_and_crash_expiry() {
    assert!(
        crate::system::tools::hidden_command(
            Path::new(env!("OUT_DIR")).join("renium-audio-test.exe")
        )
        .status()
        .expect("run audio output regression")
        .success()
    );
}

#[test]
fn drain_ring_bytes_reads_wrapping_packets_and_advances_cursor() {
    let cap = 128u32;
    let mut backing = vec![0u64; (64 + cap as usize) / 8];
    let bytes = unsafe {
        std::slice::from_raw_parts_mut(backing.as_mut_ptr().cast::<u8>(), backing.len() * 8)
    };
    bytes[0..4].copy_from_slice(&TAP_MAGIC.to_le_bytes());
    bytes[4..8].copy_from_slice(&TAP_VERSION.to_le_bytes());
    bytes[8..12].copy_from_slice(&cap.to_le_bytes());
    bytes[12..16].copy_from_slice(&1u32.to_le_bytes());
    bytes[16..24].copy_from_slice(&48u64.to_le_bytes());
    let pkt = &mut bytes[64..64 + 48];
    pkt[0..4].copy_from_slice(&48u32.to_le_bytes());
    pkt[4..8].copy_from_slice(&8u32.to_le_bytes());
    pkt[8..16].copy_from_slice(&123_456i64.to_le_bytes());
    pkt[16..24].copy_from_slice(&7u64.to_le_bytes());
    pkt[24..28].copy_from_slice(&48_000u32.to_le_bytes());
    pkt[28..32].copy_from_slice(&1u32.to_le_bytes());
    pkt[32..34].copy_from_slice(&2u16.to_le_bytes());
    pkt[34] = 4;
    pkt[35] = 0;
    pkt[36..40].copy_from_slice(&1u32.to_le_bytes());
    pkt[40..44].copy_from_slice(&0.5f32.to_le_bytes());
    pkt[44..48].copy_from_slice(&(-0.5f32).to_le_bytes());
    let mut packets = Vec::new();
    drain_ring_bytes(bytes, &mut packets);
    assert_eq!(packets.len(), 1);
    assert_eq!(packets[0].qpc_ticks, 123_456);
    assert_eq!(packets[0].format, AudioSampleFormat::F32);
    assert_eq!(packets[0].payload.len(), 8);
    assert_eq!(u64::from_le_bytes(bytes[24..32].try_into().unwrap()), 48);
}
