use std::collections::{HashMap, HashSet};
use std::ffi::{OsStr, c_void};
use std::fs;
use std::mem::{size_of, transmute, zeroed};
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, bail};
use memchr::{memchr_iter, memmem};
use windows_sys::Win32::Foundation::{
    CloseHandle, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::System::Diagnostics::Debug::{ReadProcessMemory, WriteProcessMemory};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, MODULEENTRY32W, Module32FirstW, Module32NextW, TH32CS_SNAPMODULE,
    TH32CS_SNAPMODULE32,
};
use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows_sys::Win32::System::Memory::{
    MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE, VirtualAllocEx, VirtualFreeEx,
};
use windows_sys::Win32::System::Threading::{
    CreateRemoteThread, GetExitCodeThread, OpenProcess, PROCESS_CREATE_THREAD,
    PROCESS_QUERY_INFORMATION, PROCESS_SYNCHRONIZE, PROCESS_VM_OPERATION, PROCESS_VM_READ,
    PROCESS_VM_WRITE, WaitForSingleObject,
};

use crate::studio::native::snapshot::{
    NativeSnapshot, NativeSnapshotRoots, finalize_native_snapshot, temporary_output_path,
};
use crate::system::files::{atomic_write_file, fnv1a};

const HELPER_BYTES: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/renium-studio-helper.dll"));
const PARAM_SIZE: usize = 5792;
const PARAM_STATUS: usize = 68;
const PARAM_OUTPUT_SIZE: usize = 72;
const PARAM_CONTEXT_MICROS: usize = 80;
const PARAM_COLLECT_MICROS: usize = 88;
const PARAM_SERIALIZE_MICROS: usize = 96;
const PARAM_WRITE_MICROS: usize = 104;
const PARAM_REQUESTED_MXCSR: usize = 128;
const PARAM_PLACE_MODE: usize = 136;
const PARAM_ROOTS: usize = 144;
const PARAM_OUTPUT_PATH: usize = 4240;
const PARAM_ERROR: usize = 5280;
const MAX_ROOTS: usize = 256;
const REMOTE_TIMEOUT: u32 = 20_000;
const PACKAGE_UNMODIFIED_STATE: i64 = u32::MAX as i64;

static TRACES: OnceLock<Mutex<HashMap<PathBuf, CachedTrace>>> = OnceLock::new();
static LAYOUTS: OnceLock<Mutex<HashMap<PathBuf, CachedLayout>>> = OnceLock::new();
static DATA_MODELS: OnceLock<Mutex<HashMap<u32, CachedDataModel>>> = OnceLock::new();
static HELPER_EXPORT_RVAS: OnceLock<HashMap<String, usize>> = OnceLock::new();
static PACKAGE_LAYOUTS: OnceLock<Mutex<HashMap<PathBuf, CachedPackageLayout>>> = OnceLock::new();
struct CachedTrace {
    len: u64,
    modified: Option<SystemTime>,
    trace: SerializerTrace,
}
struct CachedLayout {
    len: u64,
    modified: Option<SystemTime>,
    data: PeSection,
    trace: SerializerTrace,
}
struct CachedPackageLayout {
    len: u64,
    modified: Option<SystemTime>,
    layout: PackageLayout,
}
#[derive(Clone)]
struct CachedDataModel {
    title: String,
    outer: usize,
    owner: usize,
    layout: InstanceLayout,
}

#[derive(Clone, Copy)]
struct SerializerTrace {
    serializer: usize,
    context_builder: usize,
    context_destroy: usize,
    root_collector: usize,
    deallocator: usize,
}

#[derive(Clone)]
struct PackageLayout {
    data: PeSection,
    submit_task: usize,
}

#[derive(Clone, Copy)]
struct SharedEntry {
    instance: usize,
    owner: usize,
}

#[derive(Clone, Copy, Eq, Hash, PartialEq)]
struct InstanceLayout {
    data_model_instance: usize,
    self_pointer: usize,
    class_descriptor: usize,
    children: usize,
    name: usize,
}

struct ActiveDataModel {
    outer: usize,
    owner: usize,
    roots: Vec<SharedEntry>,
    layout: InstanceLayout,
}

struct ProcessMemory {
    handle: HANDLE,
}

impl ProcessMemory {
    fn open(pid: u32) -> Result<Self> {
        let access = PROCESS_CREATE_THREAD
            | PROCESS_QUERY_INFORMATION
            | PROCESS_SYNCHRONIZE
            | PROCESS_VM_OPERATION
            | PROCESS_VM_READ
            | PROCESS_VM_WRITE;
        let handle = unsafe { OpenProcess(access, 0, pid) };
        if handle.is_null() {
            bail!(
                "Could not open Studio process {pid}: {}",
                std::io::Error::last_os_error()
            );
        }
        Ok(Self { handle })
    }

    fn read(&self, address: usize, output: &mut [u8]) -> Result<()> {
        let mut read = 0;
        let ok = unsafe {
            ReadProcessMemory(
                self.handle,
                address as *const c_void,
                output.as_mut_ptr().cast(),
                output.len(),
                &mut read,
            )
        };
        if ok == 0 || read != output.len() {
            bail!(
                "Could not read Studio memory at 0x{address:X}: {}",
                std::io::Error::last_os_error()
            );
        }
        Ok(())
    }

    fn read_vec(&self, address: usize, size: usize) -> Result<Vec<u8>> {
        let mut output = vec![0; size];
        self.read(address, &mut output)?;
        Ok(output)
    }

    fn read_u32(&self, address: usize) -> Result<u32> {
        let mut bytes = [0; 4];
        self.read(address, &mut bytes)?;
        Ok(u32::from_le_bytes(bytes))
    }

    fn read_u64(&self, address: usize) -> Result<u64> {
        let mut bytes = [0; 8];
        self.read(address, &mut bytes)?;
        Ok(u64::from_le_bytes(bytes))
    }

    fn write(&self, address: usize, bytes: &[u8]) -> Result<()> {
        let mut written = 0;
        let ok = unsafe {
            WriteProcessMemory(
                self.handle,
                address as *mut c_void,
                bytes.as_ptr().cast(),
                bytes.len(),
                &mut written,
            )
        };
        if ok == 0 || written != bytes.len() {
            bail!(
                "Could not write Studio memory at 0x{address:X}: {}",
                std::io::Error::last_os_error()
            );
        }
        Ok(())
    }

    fn allocate(&self, size: usize) -> Result<RemoteAllocation<'_>> {
        let address = unsafe {
            VirtualAllocEx(
                self.handle,
                null(),
                size,
                MEM_COMMIT | MEM_RESERVE,
                PAGE_READWRITE,
            )
        };
        if address.is_null() {
            bail!(
                "Could not allocate Studio memory: {}",
                std::io::Error::last_os_error()
            );
        }
        Ok(RemoteAllocation {
            memory: self,
            address: address as usize,
        })
    }
}

impl RemoteAllocation<'_> {
    fn run(&mut self, address: usize, timeout: u32) -> Result<u32> {
        let start = Some(unsafe {
            transmute::<usize, unsafe extern "system" fn(*mut c_void) -> u32>(address)
        });
        let thread = unsafe {
            CreateRemoteThread(
                self.memory.handle,
                null(),
                0,
                start,
                self.address as *const c_void,
                0,
                null_mut(),
            )
        };
        if thread.is_null() {
            bail!(
                "Could not start the Studio helper: {}",
                std::io::Error::last_os_error()
            );
        }
        let waited = unsafe { WaitForSingleObject(thread, timeout) };
        if waited != WAIT_OBJECT_0 {
            unsafe {
                CloseHandle(thread);
            }
            // The helper may still reference this allocation. Studio reclaims this small buffer
            // on exit; freeing it here would create a remote use-after-free.
            self.address = 0;
            if waited == WAIT_TIMEOUT {
                bail!("Studio helper exceeded its {timeout}ms deadline");
            }
            bail!(
                "Could not wait for the Studio helper: {}",
                std::io::Error::last_os_error()
            );
        }
        let mut exit_code = 0;
        let ok = unsafe { GetExitCodeThread(thread, &mut exit_code) };
        unsafe {
            CloseHandle(thread);
        }
        if ok == 0 {
            bail!(
                "Could not read the Studio helper result: {}",
                std::io::Error::last_os_error()
            );
        }
        Ok(exit_code)
    }
}

impl Drop for ProcessMemory {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.handle);
        }
    }
}

struct RemoteAllocation<'a> {
    memory: &'a ProcessMemory,
    address: usize,
}

impl Drop for RemoteAllocation<'_> {
    fn drop(&mut self) {
        if self.address == 0 {
            return;
        }
        unsafe {
            VirtualFreeEx(
                self.memory.handle,
                self.address as *mut c_void,
                0,
                MEM_RELEASE,
            );
        }
    }
}
struct ModuleEntry {
    base: usize,
    size: usize,
    name: String,
    path: PathBuf,
}

#[derive(Clone, Copy)]
struct PeSection {
    name: [u8; 8],
    virtual_size: usize,
    virtual_address: usize,
    raw_size: usize,
    raw_offset: usize,
}

struct PeImage<'a> {
    bytes: &'a [u8],
    image_base: usize,
    sections: Vec<PeSection>,
}

impl<'a> PeImage<'a> {
    fn parse(bytes: &'a [u8]) -> Result<Self> {
        if bytes.len() < 0x40 {
            bail!("Studio executable is too small");
        }
        let pe_offset = read_u32(bytes, 0x3c)? as usize;
        if read_u32(bytes, pe_offset)? != 0x4550 {
            bail!("Studio executable is not PE");
        }
        let section_count = read_u16(bytes, pe_offset + 6)? as usize;
        let optional_size = read_u16(bytes, pe_offset + 20)? as usize;
        let optional_offset = pe_offset + 24;
        if read_u16(bytes, optional_offset)? != 0x20b {
            bail!("Studio executable is not PE32+");
        }
        let image_base = read_u64(bytes, optional_offset + 24)? as usize;
        let section_offset = optional_offset + optional_size;
        let mut sections = Vec::with_capacity(section_count);
        for index in 0..section_count {
            let offset = section_offset + index * 40;
            let mut name = [0; 8];
            name.copy_from_slice(slice(bytes, offset, 8)?);
            sections.push(PeSection {
                name,
                virtual_size: read_u32(bytes, offset + 8)? as usize,
                virtual_address: read_u32(bytes, offset + 12)? as usize,
                raw_size: read_u32(bytes, offset + 16)? as usize,
                raw_offset: read_u32(bytes, offset + 20)? as usize,
            });
        }
        Ok(Self {
            bytes,
            image_base,
            sections,
        })
    }

    fn section(&self, name: &[u8]) -> Result<PeSection> {
        self.sections
            .iter()
            .copied()
            .find(|section| {
                let end = section
                    .name
                    .iter()
                    .position(|byte| *byte == 0)
                    .unwrap_or(section.name.len());
                &section.name[..end] == name
            })
            .with_context(|| {
                format!(
                    "Studio executable is missing {}",
                    String::from_utf8_lossy(name)
                )
            })
    }

    fn offset_to_rva(&self, offset: usize) -> Result<usize> {
        for section in &self.sections {
            if offset >= section.raw_offset && offset < section.raw_offset + section.raw_size {
                return Ok(section.virtual_address + offset - section.raw_offset);
            }
        }
        bail!("Studio file offset 0x{offset:X} is not mapped")
    }

    fn rva_to_offset(&self, rva: usize) -> Result<usize> {
        for section in &self.sections {
            if rva >= section.virtual_address
                && rva < section.virtual_address + section.virtual_size.max(section.raw_size)
            {
                return Ok(section.raw_offset + rva - section.virtual_address);
            }
        }
        bail!("Studio RVA 0x{rva:X} is not mapped")
    }

    fn va_to_offset(&self, address: usize) -> Result<usize> {
        self.rva_to_offset(
            address
                .checked_sub(self.image_base)
                .context("Studio address is below its image base")?,
        )
    }

    fn call_target(&self, offset: usize) -> Result<usize> {
        if self.bytes.get(offset) != Some(&0xE8) {
            bail!("Expected a direct Studio call at file offset 0x{offset:X}");
        }
        let displacement = read_i32(self.bytes, offset + 1)? as isize;
        let source = self.offset_to_rva(offset + 5)? as isize;
        let target = source
            .checked_add(displacement)
            .context("Studio call target overflowed")?;
        usize::try_from(target).context("Studio call target was negative")
    }

    fn rip_target(&self, offset: usize, instruction_size: usize) -> Result<usize> {
        let displacement = read_i32(self.bytes, offset + 3)? as isize;
        let source = self.offset_to_rva(offset + instruction_size)? as isize;
        let target = source
            .checked_add(displacement)
            .context("Studio RIP target overflowed")?;
        usize::try_from(target).context("Studio RIP target was negative")
    }
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16> {
    Ok(u16::from_le_bytes(slice(bytes, offset, 2)?.try_into()?))
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32> {
    Ok(u32::from_le_bytes(slice(bytes, offset, 4)?.try_into()?))
}

fn read_i32(bytes: &[u8], offset: usize) -> Result<i32> {
    Ok(i32::from_le_bytes(slice(bytes, offset, 4)?.try_into()?))
}

fn read_u64(bytes: &[u8], offset: usize) -> Result<u64> {
    Ok(u64::from_le_bytes(slice(bytes, offset, 8)?.try_into()?))
}

fn slice(bytes: &[u8], offset: usize, size: usize) -> Result<&[u8]> {
    bytes
        .get(offset..offset.saturating_add(size))
        .context("Studio executable structure is truncated")
}

fn pattern_matches<'a>(
    bytes: &'a [u8],
    pattern: &'a [u8],
    start: usize,
    end: usize,
) -> impl Iterator<Item = usize> + 'a {
    let range = if pattern.is_empty() || end < start || end - start < pattern.len() {
        &bytes[0..0]
    } else {
        &bytes[start..end]
    };
    memmem::find_iter(range, pattern).map(move |offset| start + offset)
}

fn unique_match(matches: impl Iterator<Item = usize>) -> (Option<usize>, usize) {
    matches.fold((None, 0), |(first, count), offset| {
        (first.or(Some(offset)), count + 1)
    })
}

fn trace_serializer(path: &Path, bytes: &[u8]) -> Result<SerializerTrace> {
    let metadata =
        fs::metadata(path).with_context(|| format!("Could not inspect {}", path.display()))?;
    let modified = metadata.modified().ok();
    let cache = TRACES.get_or_init(|| Mutex::new(HashMap::new()));
    let cached = cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(path)
        .filter(|cached| cached.len == metadata.len() && cached.modified == modified)
        .map(|cached| cached.trace);
    if let Some(trace) = cached {
        return Ok(trace);
    }

    let image = PeImage::parse(bytes)?;
    let text = image.section(b".text")?;
    let start = text.raw_offset;
    let end = start + text.raw_size;
    let anchor = hex("4C897DA8498B16488D4D88")?;
    let continuation = hex(
        "E800000000488BD3488D8DD0000000E800000000904C897C24384C897C2430488D4588488944242844897C24204C8D8DD00000004D8B06488D55E0488D8D30010000E8",
    )?;
    let (sequence, sequence_count) =
        unique_match(
            pattern_matches(bytes, &anchor, start, end).filter(|sequence| {
                let offset = sequence + anchor.len();
                let Ok(candidate) = slice(bytes, offset, continuation.len()) else {
                    return false;
                };
                candidate.iter().zip(&continuation).enumerate().all(
                    |(index, (actual, expected))| {
                        matches!(index, 1..=4 | 16..=19) || actual == expected
                    },
                )
            }),
        );
    if sequence_count != 1 {
        bail!(
            "Studio serializer signature matched {} locations",
            sequence_count
        );
    }
    let sequence = sequence.expect("serializer signature count was validated");
    let root_collector = image.call_target(sequence + 11)?;
    let context_builder = image.call_target(sequence + 26)?;
    let wrapper = image.call_target(sequence + 77)?;
    let destroy_pattern = hex("C6853801000000488D8DD0000000E8")?;
    let (destroy_match, destroy_count) = unique_match(pattern_matches(
        bytes,
        &destroy_pattern,
        sequence,
        (sequence + 0x500).min(bytes.len()),
    ));
    if destroy_count != 1 {
        bail!(
            "Studio context cleanup signature matched {} locations",
            destroy_count
        );
    }
    let destroy_match = destroy_match.expect("cleanup signature count was validated");
    let context_destroy = image.call_target(destroy_match + 14)?;
    let deallocator_suffix = hex("0F57C0F30F7F4588")?;
    let (deallocator_call, deallocator_count) = unique_match(
        (destroy_match + destroy_pattern.len()
            ..(destroy_match + destroy_pattern.len() + 0x100).min(bytes.len()))
            .filter(|offset| {
                bytes.get(*offset) == Some(&0xE8)
                    && slice(bytes, offset + 5, deallocator_suffix.len())
                        .is_ok_and(|value| value == deallocator_suffix)
            }),
    );
    if deallocator_count != 1 {
        bail!(
            "Studio deallocator signature matched {} locations",
            deallocator_count
        );
    }
    let deallocator =
        image.call_target(deallocator_call.expect("deallocator signature count was validated"))?;
    let wrapper_offset = image.rva_to_offset(wrapper)?;
    let wrapper_prefix = hex("40534883EC6033C0488BD9")?;
    if slice(bytes, wrapper_offset, wrapper_prefix.len())? != wrapper_prefix {
        bail!("Studio serializer wrapper changed");
    }
    let async_wrapper = image.call_target(wrapper_offset + 0x52)?;
    let async_offset = image.rva_to_offset(async_wrapper)?;
    let async_prefix = hex("4C8BDC534881ECD0000000")?;
    if slice(bytes, async_offset, async_prefix.len())? != async_prefix {
        bail!("Studio asynchronous serializer wrapper changed");
    }
    let vtable_lea = async_offset + 0x9c;
    if slice(bytes, vtable_lea, 3)? != [0x48, 0x8D, 0x0D] {
        bail!("Studio serializer callback moved");
    }
    let vtable = image.rip_target(vtable_lea, 7)?;
    let vtable_offset = image.rva_to_offset(vtable)?;
    let invoke_va = read_u64(bytes, vtable_offset + 16)? as usize;
    let invoke_offset = image.va_to_offset(invoke_va)?;
    let invoke_prefix = hex("4883EC68488B41504C8B49204C8B4118488B5110")?;
    if slice(bytes, invoke_offset, invoke_prefix.len())? != invoke_prefix {
        bail!("Studio serializer callback changed");
    }
    let serializer = image.call_target(invoke_offset + 0x51)?;
    let trace = SerializerTrace {
        serializer,
        context_builder,
        context_destroy,
        root_collector,
        deallocator,
    };
    cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(
            path.to_path_buf(),
            CachedTrace {
                len: metadata.len(),
                modified,
                trace,
            },
        );
    Ok(trace)
}

fn studio_layout(path: &Path) -> Result<(PeSection, SerializerTrace)> {
    let metadata =
        fs::metadata(path).with_context(|| format!("Could not inspect {}", path.display()))?;
    let modified = metadata.modified().ok();
    let cache = LAYOUTS.get_or_init(|| Mutex::new(HashMap::new()));
    let cached = cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(path)
        .filter(|cached| cached.len == metadata.len() && cached.modified == modified)
        .map(|cached| (cached.data, cached.trace));
    if let Some(layout) = cached {
        return Ok(layout);
    }
    let executable =
        fs::read(path).with_context(|| format!("Could not read {}", path.display()))?;
    let image = PeImage::parse(&executable)?;
    let data = image.section(b".data")?;
    let trace = trace_serializer(path, &executable)?;
    cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(
            path.to_path_buf(),
            CachedLayout {
                len: metadata.len(),
                modified,
                data,
                trace,
            },
        );
    Ok((data, trace))
}

fn hex(value: &str) -> Result<Vec<u8>> {
    if !value.len().is_multiple_of(2) {
        bail!("Invalid byte pattern");
    }
    (0..value.len())
        .step_by(2)
        .map(|index| {
            u8::from_str_radix(&value[index..index + 2], 16).context("Invalid byte pattern")
        })
        .collect()
}

fn masked(value: &str) -> Result<Vec<Option<u8>>> {
    value
        .split_ascii_whitespace()
        .map(|token| {
            if token == "??" {
                Ok(None)
            } else {
                u8::from_str_radix(token, 16)
                    .map(Some)
                    .context("Invalid masked byte pattern")
            }
        })
        .collect()
}

fn find_all_masked(bytes: &[u8], pattern: &[Option<u8>], start: usize, end: usize) -> Vec<usize> {
    if pattern.is_empty() || end < start || end - start < pattern.len() {
        return Vec::new();
    }
    let Some((anchor_index, anchor)) = pattern
        .iter()
        .enumerate()
        .find_map(|(index, value)| value.map(|value| (index, value)))
    else {
        return (start..=end - pattern.len()).collect();
    };
    memchr_iter(anchor, &bytes[start + anchor_index..end])
        .filter_map(|matched| {
            (start + anchor_index + matched)
                .checked_sub(anchor_index)
                .filter(|offset| *offset + pattern.len() <= end)
        })
        .filter(|offset| {
            pattern.iter().enumerate().all(|(index, expected)| {
                expected.is_none_or(|expected| bytes[offset + index] == expected)
            })
        })
        .collect()
}

fn renderer_submit_task_rva(executable: &[u8], image: &PeImage<'_>) -> Result<usize> {
    let signature = masked(
        "48 89 5C 24 ?? 48 89 74 24 ?? 57 48 83 EC ?? 41 8B F8 48 8B F2 48 8B D9 \
         0F 57 C0 F3 0F 7F 44 24 ?? 4C 8B 49 30 4D 85 C9",
    )?;
    let text = image.section(b".text")?;
    let matches = find_all_masked(
        executable,
        &signature,
        text.raw_offset,
        text.raw_offset + text.raw_size,
    );
    if matches.len() != 1 {
        bail!(
            "Studio DataModel task submitter signature resolved {} candidates",
            matches.len()
        );
    }
    image.offset_to_rva(matches[0])
}

fn package_layout(path: &Path) -> Result<PackageLayout> {
    let metadata =
        fs::metadata(path).with_context(|| format!("Could not inspect {}", path.display()))?;
    let modified = metadata.modified().ok();
    let cache = PACKAGE_LAYOUTS.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(layout) = cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(path)
        .filter(|cached| cached.len == metadata.len() && cached.modified == modified)
        .map(|cached| cached.layout.clone())
    {
        return Ok(layout);
    }
    let bytes = fs::read(path).with_context(|| format!("Could not read {}", path.display()))?;
    let image = PeImage::parse(&bytes)?;
    let layout = PackageLayout {
        data: image.section(b".data")?,
        submit_task: renderer_submit_task_rva(&bytes, &image)?,
    };
    cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(
            path.to_path_buf(),
            CachedPackageLayout {
                len: metadata.len(),
                modified,
                layout: layout.clone(),
            },
        );
    Ok(layout)
}

fn modules(pid: u32) -> Result<Vec<ModuleEntry>> {
    let snapshot =
        unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPMODULE | TH32CS_SNAPMODULE32, pid) };
    if snapshot == INVALID_HANDLE_VALUE {
        bail!(
            "Could not inspect Studio process {pid}: {}",
            std::io::Error::last_os_error()
        );
    }
    let mut entry: MODULEENTRY32W = unsafe { zeroed() };
    entry.dwSize = size_of::<MODULEENTRY32W>() as u32;
    let mut result = Vec::new();
    let mut ok = unsafe { Module32FirstW(snapshot, &mut entry) };
    while ok != 0 {
        result.push(ModuleEntry {
            base: entry.modBaseAddr as usize,
            size: entry.modBaseSize as usize,
            name: wide_array(&entry.szModule),
            path: PathBuf::from(wide_array(&entry.szExePath)),
        });
        entry.dwSize = size_of::<MODULEENTRY32W>() as u32;
        ok = unsafe { Module32NextW(snapshot, &mut entry) };
    }
    unsafe {
        CloseHandle(snapshot);
    }
    if result.is_empty() {
        bail!("Studio process {pid} has no readable modules");
    }
    Ok(result)
}

fn wide_array(value: &[u16]) -> String {
    let end = value
        .iter()
        .position(|character| *character == 0)
        .unwrap_or(value.len());
    String::from_utf16_lossy(&value[..end])
}

fn read_msvc_string(memory: &ProcessMemory, address: usize) -> Option<String> {
    let bytes = memory.read_vec(address, 32).ok()?;
    let size = u64::from_le_bytes(bytes[16..24].try_into().ok()?) as usize;
    let capacity = u64::from_le_bytes(bytes[24..32].try_into().ok()?) as usize;
    if size > capacity || size > 1024 * 1024 {
        return None;
    }
    let data = if capacity < 16 {
        bytes[..size].to_vec()
    } else {
        let pointer = u64::from_le_bytes(bytes[..8].try_into().ok()?) as usize;
        memory.read_vec(pointer, size).ok()?
    };
    if data
        .iter()
        .any(|byte| *byte == 0 || *byte < 9 || (*byte > 13 && *byte < 32))
    {
        return None;
    }
    String::from_utf8(data).ok()
}

fn read_c_string(memory: &ProcessMemory, address: usize, limit: usize) -> Option<String> {
    let bytes = memory.read_vec(address, limit).ok()?;
    let end = bytes.iter().position(|byte| *byte == 0)?;
    String::from_utf8(bytes[..end].to_vec()).ok()
}

fn read_rtti_type(
    memory: &ProcessMemory,
    object: usize,
    module_base: usize,
    module_size: usize,
) -> Option<String> {
    let module_end = module_base.checked_add(module_size)?;
    let vtable = memory.read_u64(object).ok()? as usize;
    if vtable < module_base + 8 || vtable >= module_end {
        return None;
    }
    let locator = memory.read_u64(vtable - 8).ok()? as usize;
    if locator < module_base || locator >= module_end {
        return None;
    }
    let signature = memory.read_u32(locator).ok()?;
    let type_rva = memory.read_u32(locator + 12).ok()? as usize;
    if signature != 1 || type_rva >= module_size {
        return None;
    }
    let name = read_c_string(memory, module_base + type_rva + 16, 4096)?;
    name.starts_with(".?A").then_some(name)
}

fn read_instance_class_at(
    memory: &ProcessMemory,
    instance: usize,
    offset: usize,
) -> Option<String> {
    let descriptor = memory.read_u64(instance.checked_add(offset)?).ok()? as usize;
    if !likely_pointer(descriptor) {
        return None;
    }
    let name = memory.read_u64(descriptor.checked_add(8)?).ok()? as usize;
    if !likely_pointer(name) {
        return None;
    }
    read_msvc_string(memory, name)
}

fn read_instance_class(
    memory: &ProcessMemory,
    instance: usize,
    layout: InstanceLayout,
) -> Option<String> {
    read_instance_class_at(memory, instance, layout.class_descriptor)
}

fn read_instance_name_at(memory: &ProcessMemory, instance: usize, offset: usize) -> Option<String> {
    let name = memory.read_u64(instance.checked_add(offset)?).ok()? as usize;
    if !likely_pointer(name) {
        return None;
    }
    read_msvc_string(memory, name)
        .filter(|value| !value.is_empty())
        .or_else(|| read_msvc_string(memory, name.checked_add(8)?))
}

fn read_instance_name(
    memory: &ProcessMemory,
    instance: usize,
    layout: InstanceLayout,
) -> Option<String> {
    read_instance_name_at(memory, instance, layout.name)
        .or_else(|| read_instance_class(memory, instance, layout))
}

fn likely_pointer(value: usize) -> bool {
    (0x10000..0x0000_8000_0000_0000).contains(&value)
}

fn read_children_at(
    memory: &ProcessMemory,
    instance: usize,
    offset: usize,
) -> Option<Vec<SharedEntry>> {
    let vector = memory.read_u64(instance.checked_add(offset)?).ok()? as usize;
    if vector == 0 {
        return Some(Vec::new());
    }
    if !likely_pointer(vector) {
        return None;
    }
    let header = memory.read_vec(vector, 24).ok()?;
    let begin = u64::from_le_bytes(header[0..8].try_into().ok()?) as usize;
    let end = u64::from_le_bytes(header[8..16].try_into().ok()?) as usize;
    let capacity = u64::from_le_bytes(header[16..24].try_into().ok()?) as usize;
    if end < begin
        || capacity < end
        || !(end - begin).is_multiple_of(16)
        || end - begin > 16 * 1024 * 1024
    {
        return None;
    }
    let count = (end - begin) / 16;
    if count == 0 {
        return Some(Vec::new());
    }
    let bytes = memory.read_vec(begin, count.checked_mul(16)?).ok()?;
    let mut children = Vec::with_capacity(count);
    for index in 0..count {
        let offset = index * 16;
        let instance = u64::from_le_bytes(bytes[offset..offset + 8].try_into().ok()?) as usize;
        let owner = u64::from_le_bytes(bytes[offset + 8..offset + 16].try_into().ok()?) as usize;
        if likely_pointer(instance) && likely_pointer(owner) {
            children.push(SharedEntry { instance, owner });
        }
    }
    Some(children)
}

fn read_children(
    memory: &ProcessMemory,
    instance: usize,
    layout: InstanceLayout,
) -> Option<Vec<SharedEntry>> {
    read_children_at(memory, instance, layout.children)
}

fn valid_owner(
    memory: &ProcessMemory,
    owner: usize,
    module_base: usize,
    module_size: usize,
) -> bool {
    if !likely_pointer(owner) {
        return false;
    }
    let Ok(vtable) = memory.read_u64(owner).map(|value| value as usize) else {
        return false;
    };
    let Some(uses_address) = owner.checked_add(8) else {
        return false;
    };
    let Ok(uses) = memory.read_u32(uses_address) else {
        return false;
    };
    let Some(weaks_address) = owner.checked_add(12) else {
        return false;
    };
    let Ok(weaks) = memory.read_u32(weaks_address) else {
        return false;
    };
    vtable >= module_base
        && vtable < module_base.saturating_add(module_size)
        && uses > 0
        && uses < 1_000_000
        && weaks > 0
        && weaks < 1_000_000
}

fn expected_data_model_names(title: &str) -> Vec<String> {
    let title = title
        .strip_suffix(" - Roblox Studio")
        .unwrap_or(title)
        .trim();
    let mut values = vec![title.to_string()];
    if let Some(name) = Path::new(title)
        .file_name()
        .and_then(|value| value.to_str())
        && !values.iter().any(|value| value == name)
    {
        values.push(name.to_string());
    }
    values
}

fn has_required_data_model_roots(
    memory: &ProcessMemory,
    roots: &[SharedEntry],
    layout: InstanceLayout,
) -> bool {
    let mut found = 0u8;
    for root in roots {
        found |= match read_instance_class(memory, root.instance, layout).as_deref() {
            Some("Workspace") => 1,
            Some("Players") => 2,
            Some("MaterialService") => 4,
            _ => 0,
        };
        if found == 7 {
            return true;
        }
    }
    false
}

fn discover_instance_layout(
    memory: &ProcessMemory,
    module: &ModuleEntry,
    outer: usize,
) -> Vec<(InstanceLayout, String, Vec<SharedEntry>)> {
    let required_classes = ["Workspace", "Players", "MaterialService"];
    let mut layouts = HashSet::new();
    let mut candidates = Vec::new();
    for data_model_instance in (0..=0x800).step_by(8) {
        let Some(instance) = outer.checked_add(data_model_instance) else {
            continue;
        };
        for self_pointer in (0..=0x80).step_by(8).filter(|offset| {
            instance
                .checked_add(*offset)
                .and_then(|address| memory.read_u64(address).ok())
                .map(|value| value as usize)
                == Some(instance)
        }) {
            for class_descriptor in (0..=0x100).step_by(8).filter(|offset| {
                read_instance_class_at(memory, instance, *offset).as_deref() == Some("DataModel")
            }) {
                for children in (0..=0x180).step_by(8) {
                    let Some(roots) = read_children_at(memory, instance, children) else {
                        continue;
                    };
                    if roots.is_empty()
                        || roots.len() > MAX_ROOTS
                        || roots
                            .iter()
                            .any(|root| !valid_owner(memory, root.owner, module.base, module.size))
                    {
                        continue;
                    }
                    let root_classes = roots
                        .iter()
                        .filter_map(|root| {
                            read_instance_class_at(memory, root.instance, class_descriptor)
                                .map(|class| (root.instance, class))
                        })
                        .collect::<Vec<_>>();
                    if !required_classes
                        .iter()
                        .all(|required| root_classes.iter().any(|(_, class)| class == required))
                    {
                        continue;
                    }
                    let direct_name = (0..=0x180).step_by(8).find_map(|name| {
                        let data_model_name = read_instance_name_at(memory, instance, name)?;
                        (!data_model_name.is_empty()
                            && required_classes.iter().all(|required| {
                                root_classes.iter().any(|(root, class)| {
                                    class == required
                                        && read_instance_name_at(memory, *root, name).as_deref()
                                            == Some(*required)
                                })
                            }))
                        .then_some((name, data_model_name))
                    });
                    let (name, data_model_name) =
                        direct_name.unwrap_or_else(|| (0, "DataModel".to_string()));
                    let layout = InstanceLayout {
                        data_model_instance,
                        self_pointer,
                        class_descriptor,
                        children,
                        name,
                    };
                    if layouts.insert(layout) {
                        candidates.push((layout, data_model_name, roots.clone()));
                    }
                }
            }
        }
    }
    candidates
}

fn find_active_data_model(
    memory: &ProcessMemory,
    module: &ModuleEntry,
    data: PeSection,
    title: &str,
) -> Result<ActiveDataModel> {
    let data_base = module
        .base
        .checked_add(data.virtual_address)
        .context("Studio data section address overflowed")?;
    let bytes = memory
        .read_vec(data_base, data.virtual_size)
        .context("Could not read Studio's data section")?;
    let mut references: HashMap<usize, Vec<usize>> = HashMap::new();
    for offset in (0..=bytes.len().saturating_sub(8)).step_by(8) {
        let value = u64::from_le_bytes(
            bytes[offset..offset + 8]
                .try_into()
                .expect("data-section word is eight bytes"),
        ) as usize;
        if likely_pointer(value) {
            references.entry(value).or_default().push(offset);
        }
    }
    let expected_names = expected_data_model_names(title);
    let mut candidates = Vec::new();
    for (outer, offsets) in references
        .into_iter()
        .filter(|(_, offsets)| offsets.len() >= 2)
    {
        if read_rtti_type(memory, outer, module.base, module.size).as_deref()
            != Some(".?AVDataModel@RBX@@")
        {
            continue;
        }
        let owner = offsets
            .iter()
            .filter_map(|offset| {
                bytes.get(offset + 8..offset + 16).map(|value| {
                    u64::from_le_bytes(
                        value
                            .try_into()
                            .expect("data-section owner word is eight bytes"),
                    ) as usize
                })
            })
            .find(|owner| valid_owner(memory, *owner, module.base, module.size));
        let Some(owner) = owner else {
            continue;
        };
        for (layout, name, roots) in discover_instance_layout(memory, module, outer) {
            let exact_name = expected_names
                .iter()
                .any(|expected| expected.eq_ignore_ascii_case(&name));
            let score = usize::from(exact_name) * 1000
                + usize::from(!name.eq_ignore_ascii_case("Game")) * 100
                + offsets.len().min(20);
            candidates.push((
                score,
                name,
                ActiveDataModel {
                    outer,
                    owner,
                    roots,
                    layout,
                },
            ));
        }
    }
    candidates.sort_by_key(|candidate| std::cmp::Reverse(candidate.0));
    let Some((best_score, best_name, _)) = candidates.first() else {
        bail!("Could not locate the active Studio DataModel");
    };
    if candidates
        .get(1)
        .is_some_and(|candidate| candidate.0 == *best_score)
    {
        bail!(
            "Studio DataModel selection is ambiguous between '{}' and '{}'",
            best_name,
            candidates[1].1
        );
    }
    Ok(candidates.remove(0).2)
}

fn refresh_active_data_model(
    memory: &ProcessMemory,
    module: &ModuleEntry,
    title: &str,
    cached: &CachedDataModel,
) -> Option<ActiveDataModel> {
    if cached.title != title
        || read_rtti_type(memory, cached.outer, module.base, module.size).as_deref()
            != Some(".?AVDataModel@RBX@@")
    {
        return None;
    }
    let instance = cached
        .outer
        .checked_add(cached.layout.data_model_instance)?;
    if memory
        .read_u64(instance + cached.layout.self_pointer)
        .ok()
        .map(|value| value as usize)
        != Some(instance)
        || read_instance_class(memory, instance, cached.layout).as_deref() != Some("DataModel")
        || !valid_owner(memory, cached.owner, module.base, module.size)
    {
        return None;
    }
    let roots = read_children(memory, instance, cached.layout)?;
    if roots.is_empty()
        || roots.len() > MAX_ROOTS
        || roots
            .iter()
            .any(|root| !valid_owner(memory, root.owner, module.base, module.size))
    {
        return None;
    }
    if !has_required_data_model_roots(memory, &roots, cached.layout) {
        return None;
    }
    Some(ActiveDataModel {
        outer: cached.outer,
        owner: cached.owner,
        roots,
        layout: cached.layout,
    })
}

fn active_data_model(
    pid: u32,
    memory: &ProcessMemory,
    module: &ModuleEntry,
    data: PeSection,
    title: &str,
) -> Result<ActiveDataModel> {
    let cache = DATA_MODELS.get_or_init(|| Mutex::new(HashMap::new()));
    let cached = cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&pid)
        .cloned();
    if let Some(data_model) = cached
        .as_ref()
        .and_then(|cached| refresh_active_data_model(memory, module, title, cached))
    {
        return Ok(data_model);
    }
    let data_model = find_active_data_model(memory, module, data, title)?;
    cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(
            pid,
            CachedDataModel {
                title: title.to_string(),
                outer: data_model.outer,
                owner: data_model.owner,
                layout: data_model.layout,
            },
        );
    Ok(data_model)
}

fn select_service_root(
    memory: &ProcessMemory,
    data_model: &mut ActiveDataModel,
    service: &str,
) -> Result<()> {
    let mut selected = None;
    let mut count = 0;
    for root in data_model.roots.iter().copied() {
        if read_instance_class(memory, root.instance, data_model.layout).as_deref() == Some(service)
            || read_instance_name(memory, root.instance, data_model.layout).as_deref()
                == Some(service)
        {
            selected = selected.or(Some(root));
            count += 1;
        }
    }
    if count != 1 {
        bail!(
            "Studio DataModel contains {} roots matching {service}",
            count
        );
    }
    data_model.roots.clear();
    data_model
        .roots
        .push(selected.expect("service root count was validated"));
    Ok(())
}

fn helper_path() -> Result<PathBuf> {
    let hash = fnv1a(HELPER_BYTES);
    let directory = std::env::temp_dir().join("renium-native");
    fs::create_dir_all(&directory)
        .with_context(|| format!("Could not create {}", directory.display()))?;
    let path = directory.join(format!("renium-studio-helper-{hash:016x}.dll"));
    if fs::read(&path).ok().as_deref() != Some(HELPER_BYTES) {
        atomic_write_file(&path, HELPER_BYTES)
            .with_context(|| format!("Could not install Studio helper {}", path.display()))?;
    }
    Ok(path)
}

fn normalized_path(path: &Path) -> String {
    path.to_string_lossy().replace('/', "\\")
}

fn module_path_matches(module: &ModuleEntry, path: &Path) -> bool {
    normalized_path(&module.path).eq_ignore_ascii_case(&normalized_path(path))
}

fn ensure_helper_loaded(
    pid: u32,
    memory: &ProcessMemory,
    current_modules: &[ModuleEntry],
) -> Result<usize> {
    let path = helper_path()?;
    if let Some(module) = current_modules
        .iter()
        .find(|module| module_path_matches(module, &path))
    {
        return Ok(module.base);
    }
    let kernel32 = current_modules
        .iter()
        .find(|module| module.name.eq_ignore_ascii_case("kernel32.dll"))
        .context("Studio process is missing kernel32.dll")?;
    let kernel_name = wide("kernel32.dll");
    let local_kernel = unsafe { GetModuleHandleW(kernel_name.as_ptr()) };
    if local_kernel.is_null() {
        bail!(
            "Could not locate local kernel32.dll: {}",
            std::io::Error::last_os_error()
        );
    }
    let local_load_library =
        unsafe { GetProcAddress(local_kernel, c"LoadLibraryW".as_ptr().cast()) }
            .context("Could not locate LoadLibraryW")? as usize;
    let load_library = kernel32
        .base
        .checked_add(
            local_load_library
                .checked_sub(local_kernel as usize)
                .context("LoadLibraryW is outside kernel32.dll")?,
        )
        .context("Remote LoadLibraryW address overflowed")?;
    let path_bytes = wide(path.as_os_str());
    let mut remote_path = memory.allocate(path_bytes.len() * 2)?;
    let bytes = path_bytes
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect::<Vec<_>>();
    memory.write(remote_path.address, &bytes)?;
    remote_path.run(load_library, REMOTE_TIMEOUT)?;
    let loaded = modules(pid)?
        .into_iter()
        .find(|module| module_path_matches(module, &path))
        .with_context(|| format!("Studio did not load {}", path.display()))?;
    Ok(loaded.base)
}

fn wide(value: impl AsRef<OsStr>) -> Vec<u16> {
    value
        .as_ref()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

fn helper_export_rvas() -> Result<&'static HashMap<String, usize>> {
    if let Some(exports) = HELPER_EXPORT_RVAS.get() {
        return Ok(exports);
    }
    let image = PeImage::parse(HELPER_BYTES)?;
    let pe_offset = read_u32(HELPER_BYTES, 0x3c)? as usize;
    let optional_offset = pe_offset + 24;
    let export_rva = read_u32(HELPER_BYTES, optional_offset + 112)? as usize;
    let export_offset = image.rva_to_offset(export_rva)?;
    let function_count = read_u32(HELPER_BYTES, export_offset + 20)? as usize;
    let name_count = read_u32(HELPER_BYTES, export_offset + 24)? as usize;
    let functions = image.rva_to_offset(read_u32(HELPER_BYTES, export_offset + 28)? as usize)?;
    let names = image.rva_to_offset(read_u32(HELPER_BYTES, export_offset + 32)? as usize)?;
    let ordinals = image.rva_to_offset(read_u32(HELPER_BYTES, export_offset + 36)? as usize)?;
    let mut exports = HashMap::with_capacity(name_count);
    for index in 0..name_count {
        let name_rva = read_u32(HELPER_BYTES, names + index * 4)? as usize;
        let name_offset = image.rva_to_offset(name_rva)?;
        let end = HELPER_BYTES[name_offset..]
            .iter()
            .position(|byte| *byte == 0)
            .context("Studio helper export name is unterminated")?;
        let name = std::str::from_utf8(&HELPER_BYTES[name_offset..name_offset + end])?.to_string();
        let ordinal = read_u16(HELPER_BYTES, ordinals + index * 2)? as usize;
        if ordinal >= function_count {
            bail!("Studio helper export ordinal is invalid");
        }
        let rva = read_u32(HELPER_BYTES, functions + ordinal * 4)? as usize;
        exports.insert(name, rva);
    }
    let _ = HELPER_EXPORT_RVAS.set(exports);
    Ok(HELPER_EXPORT_RVAS
        .get()
        .expect("Studio helper exports were initialized"))
}

fn helper_export_rva(name: &str) -> Result<usize> {
    helper_export_rvas()?
        .get(name)
        .copied()
        .with_context(|| format!("Studio helper is missing {name}"))
}

fn put_u32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn put_u64(bytes: &mut [u8], offset: usize, value: usize) {
    bytes[offset..offset + 8].copy_from_slice(&(value as u64).to_le_bytes());
}

fn find_class_member_descriptor(
    memory: &ProcessMemory,
    instance: usize,
    layout: InstanceLayout,
    member_name: &str,
) -> Result<usize> {
    let class_descriptor = memory.read_u64(instance + layout.class_descriptor)? as usize;
    let mut matches = HashSet::new();
    for offset in (0..=0x200usize).step_by(8) {
        let Ok(entries) = memory
            .read_u64(class_descriptor + offset)
            .map(|value| value as usize)
        else {
            continue;
        };
        let Ok(count) = memory
            .read_u64(class_descriptor + offset + 8)
            .map(|value| value as usize)
        else {
            continue;
        };
        let Ok(capacity) = memory
            .read_u64(class_descriptor + offset + 16)
            .map(|value| value as usize)
        else {
            continue;
        };
        if count == 0
            || count > 512
            || capacity < count
            || capacity > 1024
            || !likely_pointer(entries)
        {
            continue;
        }
        for index in 0..count {
            let Ok(member) = memory
                .read_u64(entries + index * 16)
                .map(|value| value as usize)
            else {
                continue;
            };
            if !likely_pointer(member) {
                continue;
            }
            let Ok(name_object) = memory.read_u64(member + 8).map(|value| value as usize) else {
                continue;
            };
            if read_msvc_string(memory, name_object).as_deref() == Some(member_name) {
                matches.insert(member);
            }
        }
    }
    if matches.len() != 1 {
        bail!(
            "Class member '{member_name}' resolved to {} descriptors",
            matches.len()
        );
    }
    matches
        .into_iter()
        .next()
        .context("Class member descriptor was not found")
}

fn property_class_adjustment(
    memory: &ProcessMemory,
    instance: usize,
    layout: InstanceLayout,
    descriptor: usize,
    binding: usize,
    property_name: &str,
) -> Result<i32> {
    let read_adjustment = |binding: usize| -> Option<i32> {
        let cast = memory.read_u64(binding + 16).ok()? as usize;
        let code = memory.read_vec(cast, 12).ok()?;
        (code[..8] == [0x48, 0x8b, 0xc1, 0x33, 0xd2, 0x48, 0x81, 0xc1])
            .then(|| i32::from_le_bytes([code[8], code[9], code[10], code[11]]))
    };
    let adjustment = read_adjustment(binding)
        .or_else(|| {
            let fallback =
                find_class_member_descriptor(memory, instance, layout, "VersionNumber").ok()?;
            let declaring_class = memory.read_u64(descriptor + 0x30).ok()?;
            (memory.read_u64(fallback + 0x30).ok()? == declaring_class).then_some(())?;
            let fallback_binding = memory.read_u64(fallback + 0x90).ok()? as usize;
            read_adjustment(fallback_binding)
        })
        .with_context(|| format!("Property '{property_name}' cast has an unsupported layout"))?;
    if adjustment >= 0 {
        bail!("Property '{property_name}' resolved an invalid class adjustment");
    }
    Ok(adjustment)
}

#[derive(Clone, Copy)]
struct IntegerPropertyLayout {
    offset: usize,
    width: usize,
}

#[derive(Clone, Copy)]
struct PackageStatusLayout {
    modified: IntegerPropertyLayout,
    has_new_version: IntegerPropertyLayout,
    version: IntegerPropertyLayout,
}

fn integer_property_layout(
    memory: &ProcessMemory,
    instance: usize,
    layout: InstanceLayout,
    property_name: &str,
) -> Result<IntegerPropertyLayout> {
    let descriptor = find_class_member_descriptor(memory, instance, layout, property_name)?;
    let binding = memory.read_u64(descriptor + 0x90)? as usize;
    let getter = memory.read_u64(binding + 8)? as usize;
    let code = memory.read_vec(getter, 8)?;
    let (getter_offset, width) = if code[..3] == [0x48, 0x8b, 0x81] && code[7] == 0xc3 {
        (
            u32::from_le_bytes(code[3..7].try_into().expect("getter offset is four bytes"))
                as usize,
            8,
        )
    } else if code[..2] == [0x8b, 0x81] && code[6] == 0xc3 {
        (
            u32::from_le_bytes(code[2..6].try_into().expect("getter offset is four bytes"))
                as usize,
            4,
        )
    } else if code[..3] == [0x0f, 0xb6, 0x81] && code[7] == 0xc3 {
        (
            u32::from_le_bytes(code[3..7].try_into().expect("getter offset is four bytes"))
                as usize,
            1,
        )
    } else {
        bail!("Property '{property_name}' getter has an unsupported layout");
    };
    let adjustment =
        property_class_adjustment(memory, instance, layout, descriptor, binding, property_name)?;
    let offset = getter_offset
        .checked_add(adjustment.unsigned_abs() as usize)
        .context("Property field offset overflowed")?;
    if offset >= 0x1000 {
        bail!("Property '{property_name}' resolved an invalid field offset");
    }
    Ok(IntegerPropertyLayout { offset, width })
}

fn read_integer_property(
    memory: &ProcessMemory,
    instance: usize,
    layout: IntegerPropertyLayout,
) -> Result<i64> {
    match layout.width {
        8 => Ok(memory.read_u64(instance + layout.offset)? as i64),
        4 => Ok(i64::from(memory.read_u32(instance + layout.offset)?)),
        1 => Ok(i64::from(memory.read_vec(instance + layout.offset, 1)?[0])),
        _ => unreachable!(),
    }
}

fn package_status_layout(
    memory: &ProcessMemory,
    link: usize,
    layout: InstanceLayout,
) -> Result<PackageStatusLayout> {
    Ok(PackageStatusLayout {
        modified: integer_property_layout(memory, link, layout, "ModifiedState")?,
        has_new_version: integer_property_layout(memory, link, layout, "HasNewVersion")?,
        version: integer_property_layout(memory, link, layout, "VersionNumber")?,
    })
}

fn package_status(
    memory: &ProcessMemory,
    link: usize,
    layout: PackageStatusLayout,
) -> Result<(String, i64, i64)> {
    let modified = read_integer_property(memory, link, layout.modified)?;
    let has_new_version = read_integer_property(memory, link, layout.has_new_version)? != 0;
    let version = read_integer_property(memory, link, layout.version)?;
    let status = match (modified != PACKAGE_UNMODIFIED_STATE, has_new_version) {
        (false, false) => "Up To Date",
        (true, false) => "Changed",
        (false, true) => "New Version Available",
        (true, true) => "Changed + New Version Available",
    };
    Ok((status.to_string(), version, modified))
}

struct ResolvedPackage {
    root: SharedEntry,
    link: SharedEntry,
    path: String,
}

fn resolve_package_target(
    memory: &ProcessMemory,
    data_model: &ActiveDataModel,
    target: &super::PackageTarget,
) -> Result<ResolvedPackage> {
    if target.path_segments.len() < 2 {
        bail!("Package target must include a service and package root");
    }
    if !target.path_ordinals.is_empty() && target.path_ordinals.len() != target.path_segments.len()
    {
        bail!("Package path ordinals must match the number of path segments");
    }
    let service = &target.path_segments[0];
    let roots = data_model
        .roots
        .iter()
        .copied()
        .filter(|entry| {
            read_instance_name(memory, entry.instance, data_model.layout).as_deref()
                == Some(service)
                || read_instance_class(memory, entry.instance, data_model.layout).as_deref()
                    == Some(service)
        })
        .collect::<Vec<_>>();
    if roots.len() != 1 {
        bail!(
            "Studio DataModel contains {} roots matching {service}",
            roots.len()
        );
    }
    let mut current = roots[0];
    for (index, name) in target.path_segments.iter().enumerate().skip(1) {
        let children = read_children(memory, current.instance, data_model.layout)
            .context("Package target children changed while resolving its path")?;
        let matches = children
            .into_iter()
            .filter(|entry| {
                read_instance_name(memory, entry.instance, data_model.layout).as_deref()
                    == Some(name)
            })
            .collect::<Vec<_>>();
        let ordinal = target.path_ordinals.get(index).copied();
        current = match ordinal {
            Some(0) => bail!("Package path ordinals must be positive"),
            Some(ordinal) => matches.get(ordinal - 1).copied().with_context(|| {
                format!(
                    "Package path segment '{name}' has {} matches, not ordinal {ordinal}",
                    matches.len()
                )
            })?,
            None if matches.len() == 1 => matches[0],
            None => bail!(
                "Package path segment '{name}' has {} matches; add --ords to select one",
                matches.len()
            ),
        };
    }
    if read_instance_class(memory, current.instance, data_model.layout).as_deref()
        == Some("PackageLink")
    {
        bail!("Target the package root, not its PackageLink child");
    }
    let links = read_children(memory, current.instance, data_model.layout)
        .context("Package root children changed while locating PackageLink")?
        .into_iter()
        .filter(|entry| {
            read_instance_class(memory, entry.instance, data_model.layout).as_deref()
                == Some("PackageLink")
        })
        .collect::<Vec<_>>();
    if links.len() != 1 {
        bail!(
            "Package root {} has {} direct PackageLink children",
            target.path_segments.join("."),
            links.len()
        );
    }
    Ok(ResolvedPackage {
        root: current,
        link: links[0],
        path: target.path_segments.join("."),
    })
}

fn invoke_package_helper(
    pid: u32,
    memory: &ProcessMemory,
    modules: &[ModuleEntry],
    parameters: &mut [u8],
    timeout: u32,
) -> Result<()> {
    put_u32(parameters, 40, timeout);
    let helper = ensure_helper_loaded(pid, memory, modules)?;
    let run = helper
        .checked_add(helper_export_rva("ReniumPackageAction")?)
        .context("Studio package helper address overflowed")?;
    let mut remote = memory.allocate(parameters.len())?;
    memory.write(remote.address, parameters)?;
    let exit_code = remote.run(run, timeout)?;
    memory.read(remote.address, parameters)?;
    let status = read_u32(parameters, 44)?;
    if exit_code != 0 || status != 4 {
        bail!(
            "Studio package action failed with status 0x{status:X}, exit 0x{exit_code:X}, exception 0x{:X}: {}",
            read_u32(parameters, 48)?,
            error_text_at(parameters, 160)
        );
    }
    Ok(())
}

fn error_text_at(parameters: &[u8], offset: usize) -> String {
    let bytes = &parameters[offset..];
    let end = bytes
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

fn package_timeout_ms(started: Instant, timeout: Duration) -> Result<u32> {
    let remaining = timeout
        .checked_sub(started.elapsed())
        .context("Package action exceeded its deadline")?;
    u32::try_from(remaining.as_millis().max(1))
        .context("Package action timeout exceeds the Windows limit")
}

fn data_model_task_context(
    memory: &ProcessMemory,
    studio: &ModuleEntry,
    layout: &PackageLayout,
    data_model: &ActiveDataModel,
) -> Result<usize> {
    let data_model_instance = data_model
        .outer
        .checked_add(data_model.layout.data_model_instance)
        .context("DataModel instance address overflowed")?;
    let task_context = (memory.read_u64(data_model_instance + 0x58)? as usize) & !7;
    if !likely_pointer(task_context) {
        bail!("Studio DataModel task context is not ready");
    }
    let submit = studio
        .base
        .checked_add(layout.submit_task)
        .context("Studio DataModel task submitter address overflowed")?;
    let vtable = memory.read_u64(task_context)? as usize;
    let valid = (0..16usize).any(|index| {
        memory
            .read_u64(vtable + index * 8)
            .map(|value| value as usize)
            .is_ok_and(|method| {
                (studio.base..studio.base + studio.size).contains(&method)
                    && submit >= method
                    && submit - method <= 0x2000
            })
    });
    if !valid {
        bail!("Studio DataModel task submitter failed task-context validation");
    }
    Ok(task_context)
}

#[derive(Clone, Copy)]
struct PackageUiBinding {
    service: usize,
    operation_rva: usize,
}

fn package_ui_binding(
    memory: &ProcessMemory,
    studio: &ModuleEntry,
    data_model: &ActiveDataModel,
    member_name: &str,
) -> Result<PackageUiBinding> {
    let services = data_model
        .roots
        .iter()
        .copied()
        .filter(|entry| {
            read_instance_class(memory, entry.instance, data_model.layout).as_deref()
                == Some("PackageUIService")
        })
        .collect::<Vec<_>>();
    if services.len() != 1 {
        bail!(
            "Active Studio DataModel has {} PackageUIService instances",
            services.len()
        );
    }
    let service = services[0].instance;
    let descriptor = find_class_member_descriptor(memory, service, data_model.layout, member_name)?;
    let descriptor_type = read_rtti_type(memory, descriptor, studio.base, studio.size)
        .with_context(|| format!("PackageUIService.{member_name} descriptor has no Studio RTTI"))?;
    if !descriptor_type.contains("BoundYieldFuncDesc")
        || !descriptor_type.contains("PackageUIService")
        || !descriptor_type.contains("shared_ptr")
    {
        bail!("PackageUIService.{member_name} has an unsupported reflection binding");
    }
    let kind = memory.read_u64(descriptor + 0x28)? as usize;
    if read_msvc_string(memory, kind).as_deref() != Some("YieldFunction") {
        bail!("PackageUIService.{member_name} is not a yielding engine method");
    }
    let thunk = memory.read_u64(descriptor + 0x78)? as usize;
    let code = memory.read_vec(thunk, 9)?;
    if code[..5] != [0x48, 0x8b, 0x01, 0xff, 0xa0] {
        bail!("PackageUIService.{member_name} dispatcher has an unsupported layout");
    }
    let slot = u32::from_le_bytes(
        code[5..9]
            .try_into()
            .expect("virtual dispatch slot is four bytes"),
    ) as usize;
    if slot >= 0x1000 || !slot.is_multiple_of(8) {
        bail!("PackageUIService.{member_name} resolved an invalid virtual slot");
    }
    let vtable = memory.read_u64(service)? as usize;
    let operation = memory.read_u64(vtable + slot)? as usize;
    if !(studio.base..studio.base + studio.size).contains(&operation) {
        bail!("PackageUIService.{member_name} implementation is outside Studio");
    }
    Ok(PackageUiBinding {
        service,
        operation_rva: operation - studio.base,
    })
}

pub(super) fn platform_package_action(
    pid: u32,
    studio_title: &str,
    target: &super::PackageTarget,
    action: super::PackageAction,
    timeout: Duration,
) -> Result<super::PackageActionResult> {
    let started = Instant::now();
    let current_modules = modules(pid)?;
    let studio = current_modules
        .iter()
        .find(|module| module.name.eq_ignore_ascii_case("RobloxStudioBeta.exe"))
        .context("Roblox Studio module was not found")?;
    let layout = package_layout(&studio.path)?;
    let memory = ProcessMemory::open(pid)?;
    let data_model = active_data_model(pid, &memory, studio, layout.data, studio_title)?;
    let package = resolve_package_target(&memory, &data_model, target)?;
    let status_layout = package_status_layout(&memory, package.link.instance, data_model.layout)?;
    let (initial_status, initial_version, initial_modified) =
        package_status(&memory, package.link.instance, status_layout)?;
    if initial_version != target.expected_version {
        bail!(
            "Package '{}' changed while Renium was preparing the action; expected version {}, found {}",
            package.path,
            target.expected_version,
            initial_version
        );
    }
    let changed = match action {
        super::PackageAction::Desync | super::PackageAction::Restore => {
            let target_state = if action == super::PackageAction::Desync {
                1
            } else {
                PACKAGE_UNMODIFIED_STATE
            };
            let target_state_parameter = usize::try_from(target_state)
                .context("PackageLink.ModifiedState exceeds the native parameter width")?;
            if initial_modified != target_state {
                let descriptor = find_class_member_descriptor(
                    &memory,
                    package.link.instance,
                    data_model.layout,
                    "ModifiedState",
                )?;
                let binding = memory.read_u64(descriptor + 0x90)? as usize;
                let setter = memory.read_u64(binding + 16)? as usize;
                if !(studio.base..studio.base + studio.size).contains(&setter) {
                    bail!("PackageLink.ModifiedState setter is outside Studio");
                }
                let adjustment = property_class_adjustment(
                    &memory,
                    package.link.instance,
                    data_model.layout,
                    descriptor,
                    binding,
                    "ModifiedState",
                )?;
                let adjusted_link = package
                    .link
                    .instance
                    .checked_add(adjustment.unsigned_abs() as usize)
                    .context("Adjusted PackageLink address overflowed")?;
                let task_context = data_model_task_context(&memory, studio, &layout, &data_model)?;
                let mut parameters = vec![0; 688];
                put_u64(&mut parameters, 0, task_context);
                put_u64(&mut parameters, 8, adjusted_link);
                put_u64(&mut parameters, 16, package.root.owner);
                put_u64(&mut parameters, 416, data_model.outer);
                put_u64(&mut parameters, 424, data_model.owner);
                put_u64(&mut parameters, 432, studio.base);
                put_u64(&mut parameters, 440, target_state_parameter);
                put_u64(&mut parameters, 448, setter - studio.base);
                put_u64(&mut parameters, 456, layout.submit_task);
                put_u32(&mut parameters, 488, 6);
                invoke_package_helper(
                    pid,
                    &memory,
                    &current_modules,
                    &mut parameters,
                    package_timeout_ms(started, timeout)?,
                )?;
                let current =
                    read_integer_property(&memory, package.link.instance, status_layout.modified)?;
                if current != target_state {
                    bail!(
                        "Studio PackageLink.ModifiedState remained {current}, expected {target_state}"
                    );
                }
                true
            } else {
                false
            }
        }
        super::PackageAction::Publish => {
            if initial_modified == PACKAGE_UNMODIFIED_STATE {
                bail!(
                    "Package '{}' is '{initial_status}'; publishing requires the Changed state",
                    package.path
                );
            }
            let binding = package_ui_binding(&memory, studio, &data_model, "PublishPackage")?;
            let task_context = data_model_task_context(&memory, studio, &layout, &data_model)?;
            let mut parameters = vec![0; 688];
            put_u64(&mut parameters, 0, task_context);
            put_u64(&mut parameters, 8, package.root.instance);
            put_u64(&mut parameters, 16, package.root.owner);
            put_u64(&mut parameters, 432, studio.base);
            put_u64(&mut parameters, 440, binding.service);
            put_u64(&mut parameters, 448, binding.operation_rva);
            put_u64(&mut parameters, 456, layout.submit_task);
            put_u32(&mut parameters, 488, 7);
            invoke_package_helper(
                pid,
                &memory,
                &current_modules,
                &mut parameters,
                package_timeout_ms(started, timeout)?,
            )?;
            loop {
                let (_, version, modified) =
                    package_status(&memory, package.link.instance, status_layout)?;
                if modified == PACKAGE_UNMODIFIED_STATE && version >= initial_version {
                    break version > initial_version;
                }
                if started.elapsed() >= timeout {
                    bail!(
                        "Package '{}' did not finish publishing within {:.1}s",
                        package.path,
                        timeout.as_secs_f64()
                    );
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
        super::PackageAction::Update => {
            if initial_modified == PACKAGE_UNMODIFIED_STATE
                && !initial_status.contains("New Version")
            {
                false
            } else {
                let binding =
                    package_ui_binding(&memory, studio, &data_model, "SetPackageVersion")?;
                let task_context = data_model_task_context(&memory, studio, &layout, &data_model)?;
                let mut parameters = vec![0; 688];
                put_u64(&mut parameters, 0, task_context);
                put_u64(&mut parameters, 8, package.root.instance);
                put_u64(&mut parameters, 16, package.root.owner);
                put_u64(&mut parameters, 432, studio.base);
                put_u64(&mut parameters, 440, binding.service);
                put_u64(&mut parameters, 448, binding.operation_rva);
                put_u64(&mut parameters, 456, layout.submit_task);
                put_u64(
                    &mut parameters,
                    480,
                    usize::try_from(initial_version)
                        .context("Package version exceeds the native parameter range")?,
                );
                put_u32(&mut parameters, 488, 8);
                invoke_package_helper(
                    pid,
                    &memory,
                    &current_modules,
                    &mut parameters,
                    package_timeout_ms(started, timeout)?,
                )?;
                loop {
                    if let Ok(updated) = resolve_package_target(&memory, &data_model, target)
                        && let Ok(updated_layout) =
                            package_status_layout(&memory, updated.link.instance, data_model.layout)
                        && let Ok((status, version, modified)) =
                            package_status(&memory, updated.link.instance, updated_layout)
                        && modified == PACKAGE_UNMODIFIED_STATE
                        && status == "Up To Date"
                        && version >= initial_version
                    {
                        break true;
                    }
                    if started.elapsed() >= timeout {
                        bail!(
                            "Package '{}' did not finish updating within {:.1}s",
                            package.path,
                            timeout.as_secs_f64()
                        );
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
        }
    };
    let current_package = if matches!(action, super::PackageAction::Update) {
        resolve_package_target(&memory, &data_model, target)?
    } else {
        package
    };
    let current_status_layout = if matches!(action, super::PackageAction::Update) {
        package_status_layout(&memory, current_package.link.instance, data_model.layout)?
    } else {
        status_layout
    };
    let (status, version, _) = package_status(
        &memory,
        current_package.link.instance,
        current_status_layout,
    )?;
    Ok(super::PackageActionResult {
        action: match action {
            super::PackageAction::Desync => "desync",
            super::PackageAction::Restore => "restore",
            super::PackageAction::Publish => "publish",
            super::PackageAction::Update => "update",
        },
        changed,
        path: current_package.path,
        status,
        version,
    })
}

fn build_parameters(
    module_base: usize,
    trace: SerializerTrace,
    data_model: &ActiveDataModel,
    output: &Path,
    place_mode: bool,
) -> Result<Vec<u8>> {
    let mut bytes = vec![0; PARAM_SIZE];
    put_u64(&mut bytes, 0, module_base);
    put_u64(&mut bytes, 8, trace.serializer);
    put_u64(&mut bytes, 16, trace.context_builder);
    put_u64(&mut bytes, 24, trace.context_destroy);
    put_u64(&mut bytes, 32, trace.root_collector);
    put_u64(&mut bytes, 40, trace.deallocator);
    put_u64(&mut bytes, 48, data_model.outer);
    put_u64(&mut bytes, 56, data_model.owner);
    put_u32(
        &mut bytes,
        64,
        u32::try_from(data_model.roots.len()).context("Studio root count overflowed")?,
    );
    put_u32(&mut bytes, PARAM_REQUESTED_MXCSR, 0x9fc0);
    put_u32(&mut bytes, PARAM_PLACE_MODE, u32::from(place_mode));
    for (index, root) in data_model.roots.iter().enumerate() {
        let offset = PARAM_ROOTS + index * 16;
        put_u64(&mut bytes, offset, root.instance);
        put_u64(&mut bytes, offset + 8, root.owner);
    }
    let path = wide(output.as_os_str());
    if path.len() > 520 {
        bail!("Native snapshot path is too long: {}", output.display());
    }
    for (index, value) in path.iter().enumerate() {
        bytes[PARAM_OUTPUT_PATH + index * 2..PARAM_OUTPUT_PATH + index * 2 + 2]
            .copy_from_slice(&value.to_le_bytes());
    }
    Ok(bytes)
}

fn error_text(bytes: &[u8]) -> String {
    let value = &bytes[PARAM_ERROR..PARAM_ERROR + 512];
    let end = value
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(value.len());
    String::from_utf8_lossy(&value[..end]).into_owned()
}

fn write_live_snapshot(
    pid: u32,
    studio_title: &str,
    output: &Path,
    service: Option<&str>,
) -> Result<NativeSnapshot> {
    if output.exists() {
        bail!(
            "Refusing to overwrite existing native snapshot {}",
            output.display()
        );
    }
    let started = Instant::now();
    let current_modules = modules(pid)?;
    let studio = current_modules
        .first()
        .context("Studio process has no main module")?;
    let trace_started = Instant::now();
    let (data, trace) = studio_layout(&studio.path)?;
    let trace_ms = trace_started.elapsed().as_secs_f64() * 1000.0;
    let memory = ProcessMemory::open(pid)?;
    let discover_started = Instant::now();
    let mut data_model = active_data_model(pid, &memory, studio, data, studio_title)?;
    if let Some(service) = service {
        select_service_root(&memory, &mut data_model, service)?;
    }
    let discover_ms = discover_started.elapsed().as_secs_f64() * 1000.0;
    let helper_started = Instant::now();
    let helper = ensure_helper_loaded(pid, &memory, &current_modules)?;
    let helper_run = helper
        .checked_add(helper_export_rva("ReniumRun")?)
        .context("Studio helper address overflowed")?;
    let helper_ms = helper_started.elapsed().as_secs_f64() * 1000.0;
    let temporary = temporary_output_path(output, pid)?;
    let result = (|| -> Result<NativeSnapshot> {
        let mut parameters = build_parameters(
            studio.base,
            trace,
            &data_model,
            &temporary,
            service.is_none(),
        )?;
        let mut remote = memory.allocate(parameters.len())?;
        memory.write(remote.address, &parameters)?;
        let invoke_started = Instant::now();
        let exit_code = remote.run(helper_run, REMOTE_TIMEOUT)?;
        let invoke_ms = invoke_started.elapsed().as_secs_f64() * 1000.0;
        memory.read(remote.address, &mut parameters)?;
        let status = read_u32(&parameters, PARAM_STATUS)?;
        if exit_code != 0 || status != 4 {
            let error = error_text(&parameters);
            bail!(
                "Studio native serializer failed with status 0x{status:X}, exit 0x{exit_code:X}: {error}"
            );
        }
        let output_size = read_u64(&parameters, PARAM_OUTPUT_SIZE)?;
        let expected_roots = NativeSnapshotRoots {
            exact_service: service,
            containing_service: None,
        };
        let (instance_count, validate_ms) =
            finalize_native_snapshot(&temporary, output, output_size, expected_roots)?;
        Ok(NativeSnapshot {
            instance_count,
            output_size,
            trace_ms,
            discover_ms,
            helper_ms,
            invoke_ms,
            validate_ms,
            context_ms: read_u64(&parameters, PARAM_CONTEXT_MICROS)? as f64 / 1000.0,
            collect_ms: read_u64(&parameters, PARAM_COLLECT_MICROS)? as f64 / 1000.0,
            serialize_ms: read_u64(&parameters, PARAM_SERIALIZE_MICROS)? as f64 / 1000.0,
            write_ms: read_u64(&parameters, PARAM_WRITE_MICROS)? as f64 / 1000.0,
            elapsed_ms: started.elapsed().as_secs_f64() * 1000.0,
        })
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

pub fn write_live_place(pid: u32, studio_title: &str, output: &Path) -> Result<NativeSnapshot> {
    write_live_snapshot(pid, studio_title, output, None)
}

pub fn write_live_service(
    pid: u32,
    studio_title: &str,
    service: &str,
    output: &Path,
) -> Result<NativeSnapshot> {
    write_live_snapshot(pid, studio_title, output, Some(service))
}
