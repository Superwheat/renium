use std::collections::HashMap;
use std::ffi::{OsStr, c_void};
use std::fs;
use std::mem::{size_of, transmute, zeroed};
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};
use std::sync::{Mutex, OnceLock};
use std::time::{Instant, SystemTime};

use anyhow::{Context, Result, bail};
use memchr::memmem;
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0};
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
const DATA_MODEL_INSTANCE_OFFSET: usize = 0x1c8;
const INSTANCE_CLASS_DESCRIPTOR_OFFSET: usize = 0x18;
const INSTANCE_CHILDREN_OFFSET: usize = 0x70;
const INSTANCE_NAME_OFFSET: usize = 0x98;
const REMOTE_TIMEOUT: u32 = 30_000;

static TRACES: OnceLock<Mutex<HashMap<PathBuf, CachedTrace>>> = OnceLock::new();
static LAYOUTS: OnceLock<Mutex<HashMap<PathBuf, CachedLayout>>> = OnceLock::new();
static DATA_MODELS: OnceLock<Mutex<HashMap<u32, CachedDataModel>>> = OnceLock::new();
static HELPER_EXPORT_RVA: OnceLock<usize> = OnceLock::new();
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
#[derive(Clone)]
struct CachedDataModel {
    title: String,
    outer: usize,
    owner: usize,
}

#[derive(Clone, Copy)]
struct SerializerTrace {
    serializer: usize,
    context_builder: usize,
    context_destroy: usize,
    root_collector: usize,
    deallocator: usize,
}

#[derive(Clone, Copy)]
struct SharedEntry {
    instance: usize,
    owner: usize,
}

struct ActiveDataModel {
    outer: usize,
    owner: usize,
    roots: Vec<SharedEntry>,
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

    fn run(&self, address: usize, parameter: usize, timeout: u32) -> Result<u32> {
        let start = Some(unsafe {
            transmute::<usize, unsafe extern "system" fn(*mut c_void) -> u32>(address)
        });
        let thread = unsafe {
            CreateRemoteThread(
                self.handle,
                null(),
                0,
                start,
                parameter as *const c_void,
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
        let timed_out = waited != WAIT_OBJECT_0;
        if timed_out {
            let completed = unsafe { WaitForSingleObject(thread, u32::MAX) };
            if completed != WAIT_OBJECT_0 {
                unsafe {
                    CloseHandle(thread);
                }
                bail!(
                    "Could not confirm Studio helper completion after waiting {timeout} ms: {}",
                    std::io::Error::last_os_error()
                );
            }
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
        if timed_out {
            bail!("Studio helper exceeded its {timeout} ms deadline and finished afterward");
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

fn read_instance_class(memory: &ProcessMemory, instance: usize) -> Option<String> {
    let descriptor = memory
        .read_u64(instance + INSTANCE_CLASS_DESCRIPTOR_OFFSET)
        .ok()? as usize;
    let name = memory.read_u64(descriptor + 8).ok()? as usize;
    read_msvc_string(memory, name)
}

fn read_instance_name(memory: &ProcessMemory, instance: usize) -> Option<String> {
    let name = memory.read_u64(instance + INSTANCE_NAME_OFFSET).ok()? as usize;
    read_msvc_string(memory, name)
}

fn likely_pointer(value: usize) -> bool {
    (0x10000..0x0000_8000_0000_0000).contains(&value)
}

fn read_children(memory: &ProcessMemory, instance: usize) -> Option<Vec<SharedEntry>> {
    let vector = memory.read_u64(instance + INSTANCE_CHILDREN_OFFSET).ok()? as usize;
    if vector == 0 {
        return Some(Vec::new());
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
    let bytes = memory.read_vec(begin, count * 16).ok()?;
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
    let Ok(uses) = memory.read_u32(owner + 8) else {
        return false;
    };
    let Ok(weaks) = memory.read_u32(owner + 12) else {
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

fn has_required_data_model_roots(memory: &ProcessMemory, roots: &[SharedEntry]) -> bool {
    let mut found = 0u8;
    for root in roots {
        found |= match read_instance_class(memory, root.instance).as_deref() {
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
        let instance = outer + DATA_MODEL_INSTANCE_OFFSET;
        if memory
            .read_u64(instance + 8)
            .ok()
            .map(|value| value as usize)
            != Some(instance)
            || read_instance_class(memory, instance).as_deref() != Some("DataModel")
        {
            continue;
        }
        let Some(name) = read_instance_name(memory, instance) else {
            continue;
        };
        let Some(roots) = read_children(memory, instance) else {
            continue;
        };
        if roots.is_empty() || roots.len() > MAX_ROOTS {
            continue;
        }
        if !has_required_data_model_roots(memory, &roots) {
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
        if roots
            .iter()
            .any(|root| !valid_owner(memory, root.owner, module.base, module.size))
        {
            continue;
        }
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
            },
        ));
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
    let instance = cached.outer.checked_add(DATA_MODEL_INSTANCE_OFFSET)?;
    if memory
        .read_u64(instance + 8)
        .ok()
        .map(|value| value as usize)
        != Some(instance)
        || read_instance_class(memory, instance).as_deref() != Some("DataModel")
        || !valid_owner(memory, cached.owner, module.base, module.size)
    {
        return None;
    }
    let roots = read_children(memory, instance)?;
    if roots.is_empty()
        || roots.len() > MAX_ROOTS
        || roots
            .iter()
            .any(|root| !valid_owner(memory, root.owner, module.base, module.size))
    {
        return None;
    }
    if !has_required_data_model_roots(memory, &roots) {
        return None;
    }
    Some(ActiveDataModel {
        outer: cached.outer,
        owner: cached.owner,
        roots,
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
        if read_instance_class(memory, root.instance).as_deref() == Some(service)
            || read_instance_name(memory, root.instance).as_deref() == Some(service)
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
    let remote_path = memory.allocate(path_bytes.len() * 2)?;
    let bytes = path_bytes
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect::<Vec<_>>();
    memory.write(remote_path.address, &bytes)?;
    memory.run(load_library, remote_path.address, REMOTE_TIMEOUT)?;
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

fn helper_export_rva() -> Result<usize> {
    if let Some(rva) = HELPER_EXPORT_RVA.get() {
        return Ok(*rva);
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
    for index in 0..name_count {
        let name_rva = read_u32(HELPER_BYTES, names + index * 4)? as usize;
        let name_offset = image.rva_to_offset(name_rva)?;
        let end = HELPER_BYTES[name_offset..]
            .iter()
            .position(|byte| *byte == 0)
            .context("Studio helper export name is unterminated")?;
        if &HELPER_BYTES[name_offset..name_offset + end] != b"ReniumRun" {
            continue;
        }
        let ordinal = read_u16(HELPER_BYTES, ordinals + index * 2)? as usize;
        if ordinal >= function_count {
            bail!("Studio helper export ordinal is invalid");
        }
        let rva = read_u32(HELPER_BYTES, functions + ordinal * 4)? as usize;
        let _ = HELPER_EXPORT_RVA.set(rva);
        return Ok(rva);
    }
    bail!("Studio helper is missing ReniumRun")
}

fn put_u32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn put_u64(bytes: &mut [u8], offset: usize, value: usize) {
    bytes[offset..offset + 8].copy_from_slice(&(value as u64).to_le_bytes());
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
        .checked_add(helper_export_rva()?)
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
        let remote = memory.allocate(parameters.len())?;
        memory.write(remote.address, &parameters)?;
        let invoke_started = Instant::now();
        let exit_code = memory.run(helper_run, remote.address, REMOTE_TIMEOUT)?;
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
