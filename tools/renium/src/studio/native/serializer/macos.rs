use std::collections::HashMap;
use std::ffi::c_void;
use std::fs::{self, File};
use std::io::{BufReader, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, bail};

use crate::app::timing::current_millis;
use crate::studio::native::snapshot::{
    NativeSnapshot, NativeSnapshotRoots, finalize_native_snapshot, temporary_output_path,
};
use crate::system::files::{atomic_write_file, sha256_hex};

const HELPER_BYTES: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/renium-studio-helper.dylib"));
const LAUNCHER_BYTES: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/renium-studio-launcher"));
const REQUEST_MAGIC: u32 = 0x4d4e4552;
const REQUEST_VERSION: u32 = 3;
const RESPONSE_SIZE: usize = 536;
const MACH_HEADER_64_SIZE: usize = 32;
const SEGMENT_COMMAND_64_SIZE: usize = 72;
const SECTION_64_SIZE: usize = 80;
const LC_SEGMENT_64: u32 = 0x19;
const LC_UUID: u32 = 0x1b;
const CPU_TYPE_X86_64: u32 = 0x0100_0007;
const CPU_TYPE_ARM64: u32 = 0x0100_000c;

static TRACES: OnceLock<Mutex<HashMap<PathBuf, CachedTrace>>> = OnceLock::new();

#[derive(Clone, Copy)]
struct SerializerTrace {
    factory_rva: u64,
    execute_rva: u64,
    image_uuid: [u8; 16],
}
struct CachedTrace {
    len: u64,
    modified: Option<SystemTime>,
    trace: SerializerTrace,
}

#[derive(Clone, Copy)]
struct MachSection {
    address: u64,
    size: u64,
    offset: usize,
}

struct MachImage<'a> {
    bytes: &'a [u8],
    cpu: u32,
    image_base: u64,
    image_uuid: [u8; 16],
    text: MachSection,
    sections: Vec<MachSection>,
}

fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        bytes.get(offset..offset + 4)?.try_into().ok()?,
    ))
}

fn read_i32(bytes: &[u8], offset: usize) -> Option<i32> {
    Some(i32::from_le_bytes(
        bytes.get(offset..offset + 4)?.try_into().ok()?,
    ))
}

fn read_u64(bytes: &[u8], offset: usize) -> Option<u64> {
    Some(u64::from_le_bytes(
        bytes.get(offset..offset + 8)?.try_into().ok()?,
    ))
}

fn fixed_name(bytes: &[u8]) -> Result<&str> {
    let end = bytes
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(bytes.len());
    std::str::from_utf8(&bytes[..end]).context("Studio Mach-O contains a non-UTF-8 name")
}

impl<'a> MachImage<'a> {
    fn parse(bytes: &'a [u8]) -> Result<Self> {
        if read_u32(bytes, 0) != Some(0xfeed_facf) {
            bail!("Studio executable is not a supported 64-bit Mach-O");
        }
        let cpu = read_u32(bytes, 4).context("Studio Mach-O header is truncated")?;
        if !matches!(cpu, CPU_TYPE_ARM64 | CPU_TYPE_X86_64) {
            bail!("Studio executable uses unsupported CPU type 0x{cpu:X}");
        }
        let command_count =
            read_u32(bytes, 16).context("Studio Mach-O load commands are truncated")? as usize;
        let mut cursor = MACH_HEADER_64_SIZE;
        let mut sections = Vec::new();
        let mut text = None;
        let mut image_base = None;
        let mut image_uuid = None;
        for _ in 0..command_count {
            let command =
                read_u32(bytes, cursor).context("Studio Mach-O load command is truncated")?;
            let command_size = read_u32(bytes, cursor + 4)
                .context("Studio Mach-O load command size is truncated")?
                as usize;
            let command_end = cursor
                .checked_add(command_size)
                .filter(|end| *end <= bytes.len());
            if command_size < 8 || command_end.is_none() {
                bail!("Studio Mach-O contains an invalid load command");
            }
            if command == LC_SEGMENT_64 {
                if command_size < SEGMENT_COMMAND_64_SIZE {
                    bail!("Studio Mach-O contains a truncated segment");
                }
                let segment_name = fixed_name(&bytes[cursor + 8..cursor + 24])?;
                if segment_name == "__TEXT" {
                    image_base = Some(
                        read_u64(bytes, cursor + 24)
                            .context("Studio __TEXT address is truncated")?,
                    );
                }
                let section_count = read_u32(bytes, cursor + 64)
                    .context("Studio Mach-O segment section count is truncated")?
                    as usize;
                let sections_end = SEGMENT_COMMAND_64_SIZE
                    .checked_add(
                        section_count
                            .checked_mul(SECTION_64_SIZE)
                            .context("Studio Mach-O section count overflowed")?,
                    )
                    .context("Studio Mach-O section count overflowed")?;
                if sections_end > command_size {
                    bail!("Studio Mach-O section table is truncated");
                }
                for index in 0..section_count {
                    let section_offset = cursor + SEGMENT_COMMAND_64_SIZE + index * SECTION_64_SIZE;
                    let section_name = fixed_name(&bytes[section_offset..section_offset + 16])?;
                    let address = read_u64(bytes, section_offset + 32)
                        .context("Studio Mach-O section address is truncated")?;
                    let size = read_u64(bytes, section_offset + 40)
                        .context("Studio Mach-O section size is truncated")?;
                    let offset = read_u32(bytes, section_offset + 48)
                        .context("Studio Mach-O section offset is truncated")?
                        as usize;
                    let section = MachSection {
                        address,
                        size,
                        offset,
                    };
                    if segment_name == "__TEXT" && section_name == "__text" {
                        text = Some(section);
                    }
                    if offset > 0
                        && size > 0
                        && usize::try_from(size)
                            .ok()
                            .and_then(|size| offset.checked_add(size))
                            .is_some_and(|end| end <= bytes.len())
                    {
                        sections.push(section);
                    }
                }
            } else if command == LC_UUID {
                if command_size < 24 {
                    bail!("Studio Mach-O contains a truncated UUID command");
                }
                let uuid: [u8; 16] = bytes[cursor + 8..cursor + 24]
                    .try_into()
                    .expect("UUID command bounds were validated");
                if image_uuid.replace(uuid).is_some() {
                    bail!("Studio Mach-O contains multiple UUID commands");
                }
            }
            cursor = command_end.expect("load command bounds were validated");
        }
        let text = text.context("Studio Mach-O is missing __TEXT,__text")?;
        let image_base = image_base.context("Studio Mach-O is missing __TEXT")?;
        let image_uuid = image_uuid.context("Studio Mach-O is missing LC_UUID")?;
        Ok(Self {
            bytes,
            cpu,
            image_base,
            image_uuid,
            text,
            sections,
        })
    }

    fn address_for_offset(&self, offset: usize) -> Option<u64> {
        self.sections.iter().find_map(|section| {
            let relative = offset.checked_sub(section.offset)?;
            (relative < section.size as usize).then_some(section.address + relative as u64)
        })
    }

    fn text_bytes(&self) -> Result<&[u8]> {
        let end = self
            .text
            .offset
            .checked_add(self.text.size as usize)
            .context("Studio text section size overflowed")?;
        self.bytes
            .get(self.text.offset..end)
            .context("Studio text section is truncated")
    }

    fn text_offset_for_address(&self, address: u64) -> Option<usize> {
        let relative = address.checked_sub(self.text.address)?;
        (relative < self.text.size)
            .then(|| self.text.offset.checked_add(relative as usize))
            .flatten()
    }

    fn text_address_for_offset(&self, offset: usize) -> Option<u64> {
        let relative = offset.checked_sub(self.text.offset)?;
        (relative < self.text.size as usize).then_some(self.text.address + relative as u64)
    }
}

fn find_unique_bytes(haystack: &[u8], needle: &[u8], label: &str) -> Result<usize> {
    let mut matches = memchr::memmem::find_iter(haystack, needle);
    let first = matches
        .next()
        .with_context(|| format!("Studio serializer {label} was not found"))?;
    if matches.next().is_some() {
        bail!("Studio serializer {label} is ambiguous");
    }
    Ok(first)
}

fn arm64_string_xref(image: &MachImage<'_>, string_address: u64) -> Result<u64> {
    let text = image.text_bytes()?;
    let mut hits = Vec::new();
    for offset in (0..text.len().saturating_sub(28)).step_by(4) {
        let instruction = read_u32(text, offset).expect("ARM64 instruction range was bounded");
        if instruction & 0x9f00_0000 != 0x9000_0000 {
            continue;
        }
        let register = instruction & 31;
        let immediate_low = (instruction >> 29) & 3;
        let immediate_high = (instruction >> 5) & 0x7ffff;
        let mut immediate = ((immediate_high << 2) | immediate_low) as i64;
        if immediate & 0x10_0000 != 0 {
            immediate -= 0x20_0000;
        }
        let address = image.text.address + offset as u64;
        let page = (address & !0xfff).wrapping_add_signed(immediate << 12);
        for next in (offset + 4..=offset + 24).step_by(4) {
            let add = read_u32(text, next).expect("ARM64 lookahead range was bounded");
            if add & 0xff00_0000 != 0x9100_0000 || (add >> 5) & 31 != register {
                continue;
            }
            let shift = if add & (1 << 22) == 0 { 0 } else { 12 };
            let value = page + ((((add >> 10) & 0xfff) as u64) << shift);
            if value == string_address {
                hits.push(address);
            }
        }
    }
    if hits.len() != 1 {
        bail!(
            "Studio serializer log reference count changed from one to {}",
            hits.len()
        );
    }
    Ok(hits[0])
}

fn x86_string_xref(image: &MachImage<'_>, string_address: u64) -> Result<u64> {
    let text = image.text_bytes()?;
    let mut hits = Vec::new();
    for offset in 0..text.len().saturating_sub(7) {
        let rex = text[offset];
        let opcode = text[offset + 1];
        let modrm = text[offset + 2];
        if rex & 0xf0 != 0x40 || opcode != 0x8d || modrm & 0xc7 != 0x05 {
            continue;
        }
        let displacement =
            read_i32(text, offset + 3).expect("x86 instruction range was bounded") as i64;
        let address = image.text.address + offset as u64;
        if address.wrapping_add(7).wrapping_add_signed(displacement) == string_address {
            hits.push(address);
        }
    }
    if hits.len() != 1 {
        bail!(
            "Studio serializer log reference count changed from one to {}",
            hits.len()
        );
    }
    Ok(hits[0])
}

fn find_pattern_in_address_range(
    image: &MachImage<'_>,
    start: u64,
    end: u64,
    pattern: &[u8],
    label: &str,
) -> Result<u64> {
    let start_offset = image
        .text_offset_for_address(start.max(image.text.address))
        .context("Studio serializer search begins outside __text")?;
    let text_end = image.text.address + image.text.size;
    let end_address = end.min(text_end);
    let end_offset = image
        .text_offset_for_address(end_address.saturating_sub(1))
        .map(|offset| offset + 1)
        .context("Studio serializer search ends outside __text")?;
    let relative = find_unique_bytes(
        image
            .bytes
            .get(start_offset..end_offset)
            .context("Studio serializer search range is invalid")?,
        pattern,
        label,
    )?;
    image
        .text_address_for_offset(start_offset + relative)
        .context("Studio serializer pattern address is invalid")
}

fn trace_arm64(image: &MachImage<'_>, log_xref: u64) -> Result<SerializerTrace> {
    let execute_pattern = [
        0xf4, 0x4f, 0xbe, 0xa9, 0xfd, 0x7b, 0x01, 0xa9, 0xfd, 0x43, 0x00, 0x91, 0xf3, 0x03, 0x00,
        0xaa, 0x00, 0xe0, 0x05, 0x91,
    ];
    let execute = find_pattern_in_address_range(
        image,
        log_xref.saturating_sub(0x3000),
        log_xref,
        &execute_pattern,
        "execution entry",
    )?;
    let allocation_pattern = [0x00, 0x3b, 0x80, 0x52];
    let allocation = find_pattern_in_address_range(
        image,
        execute + 0x1000,
        execute + 0x4000,
        &allocation_pattern,
        "state allocation",
    )?;
    let factory_pattern = [
        0xf8, 0x5f, 0xbc, 0xa9, 0xf6, 0x57, 0x01, 0xa9, 0xf4, 0x4f, 0x02, 0xa9, 0xfd, 0x7b, 0x03,
        0xa9,
    ];
    let factory = find_pattern_in_address_range(
        image,
        allocation.saturating_sub(0x80),
        allocation + 0x20,
        &factory_pattern,
        "state factory",
    )?;
    let factory_offset = image
        .text_offset_for_address(factory)
        .context("Studio serializer state factory is outside __text")?;
    let factory_bytes = image
        .bytes
        .get(factory_offset..factory_offset + 128)
        .context("Studio serializer state factory is truncated")?;
    if !factory_bytes
        .windows(8)
        .any(|window| window == [0x81, 0x62, 0x00, 0x91, 0x61, 0x52, 0x00, 0xa9])
    {
        bail!("Studio serializer state factory result shape changed");
    }
    Ok(SerializerTrace {
        factory_rva: factory
            .checked_sub(image.image_base)
            .context("Studio serializer state factory precedes __TEXT")?,
        execute_rva: execute
            .checked_sub(image.image_base)
            .context("Studio serializer execution entry precedes __TEXT")?,
        image_uuid: image.image_uuid,
    })
}

fn trace_x86(image: &MachImage<'_>, log_xref: u64) -> Result<SerializerTrace> {
    let execute_pattern = [
        0x55, 0x48, 0x89, 0xe5, 0x41, 0x56, 0x53, 0x49, 0x89, 0xfe, 0x48, 0x8d, 0x9f, 0x78, 0x01,
        0x00, 0x00,
    ];
    let execute = find_pattern_in_address_range(
        image,
        log_xref.saturating_sub(0x3000),
        log_xref,
        &execute_pattern,
        "execution entry",
    )?;
    let factory_pattern = [
        0x55, 0x48, 0x89, 0xe5, 0x41, 0x57, 0x41, 0x56, 0x41, 0x55, 0x41, 0x54, 0x53, 0x50,
    ];
    let start_offset = image
        .text_offset_for_address(execute + 0x1000)
        .context("Studio serializer factory search begins outside __text")?;
    let end_offset = image
        .text_offset_for_address(execute + 0x4000)
        .context("Studio serializer factory search ends outside __text")?;
    let area = image
        .bytes
        .get(start_offset..end_offset)
        .context("Studio serializer factory search range is invalid")?;
    let mut factories = Vec::new();
    for relative in memchr::memmem::find_iter(area, &factory_pattern) {
        let Some(body) = area.get(relative..relative + 128) else {
            continue;
        };
        if body
            .windows(5)
            .any(|window| window == [0xbf, 0xd8, 0x01, 0x00, 0x00])
            && body.windows(11).any(|window| {
                window
                    == [
                        0x4c, 0x89, 0xf2, 0x48, 0x83, 0xc2, 0x18, 0x48, 0x89, 0x13, 0x4c,
                    ]
            })
        {
            factories.push(
                image
                    .text_address_for_offset(start_offset + relative)
                    .context("Studio serializer factory address is invalid")?,
            );
        }
    }
    if factories.len() != 1 {
        bail!(
            "Studio serializer state factory count changed from one to {}",
            factories.len()
        );
    }
    let factory = factories[0];
    Ok(SerializerTrace {
        factory_rva: factory
            .checked_sub(image.image_base)
            .context("Studio serializer state factory precedes __TEXT")?,
        execute_rva: execute
            .checked_sub(image.image_base)
            .context("Studio serializer execution entry precedes __TEXT")?,
        image_uuid: image.image_uuid,
    })
}

fn trace_studio(path: &Path) -> Result<SerializerTrace> {
    let metadata =
        fs::metadata(path).with_context(|| format!("Could not inspect {}", path.display()))?;
    let modified = metadata.modified().ok();
    let cache = TRACES.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(trace) = cache
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .get(path)
        .filter(|cached| cached.len == metadata.len() && cached.modified == modified)
        .map(|cached| cached.trace)
    {
        return Ok(trace);
    }
    let bytes = fs::read(path).with_context(|| format!("Could not read {}", path.display()))?;
    let image = MachImage::parse(&bytes)?;
    let log_offset = memchr::memmem::find(&bytes, b"serializeDataModel() took %.2f s\0")
        .context("Studio local serializer log marker was not found")?;
    if memchr::memmem::find(
        &bytes[log_offset + 1..],
        b"serializeDataModel() took %.2f s\0",
    )
    .is_some()
    {
        bail!("Studio local serializer log marker is ambiguous");
    }
    let log_address = image
        .address_for_offset(log_offset)
        .context("Studio local serializer log marker has no virtual address")?;
    let trace = match image.cpu {
        CPU_TYPE_ARM64 => trace_arm64(&image, arm64_string_xref(&image, log_address)?)?,
        CPU_TYPE_X86_64 => trace_x86(&image, x86_string_xref(&image, log_address)?)?,
        _ => unreachable!(),
    };
    cache
        .lock()
        .unwrap_or_else(|error| error.into_inner())
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

#[link(name = "proc")]
unsafe extern "C" {
    fn proc_pidpath(pid: i32, buffer: *mut c_void, buffer_size: u32) -> i32;
}

fn process_executable_path(pid: u32) -> Result<PathBuf> {
    let mut bytes = vec![0u8; 4096];
    let length = unsafe { proc_pidpath(pid as i32, bytes.as_mut_ptr().cast(), bytes.len() as u32) };
    if length <= 0 {
        bail!(
            "Could not locate Studio process {pid}: {}",
            std::io::Error::last_os_error()
        );
    }
    bytes.truncate(length as usize);
    Ok(PathBuf::from(
        String::from_utf8(bytes).context("Studio executable path is not valid UTF-8")?,
    ))
}

fn invoke_helper(
    pid: u32,
    trace: SerializerTrace,
    output: &Path,
    studio_title: &str,
) -> Result<(u64, f64)> {
    let path = output
        .to_str()
        .context("Native snapshot path is not valid UTF-8")?;
    if !output.is_absolute() {
        bail!("Native snapshot path must be absolute");
    }
    let path_bytes = path.as_bytes();
    let path_length =
        u32::try_from(path_bytes.len()).context("Native snapshot path is too long")?;
    let title_bytes = studio_title.as_bytes();
    let title_length =
        u32::try_from(title_bytes.len()).context("Roblox Studio title is too long")?;
    let socket_path = PathBuf::from(format!("/tmp/renium-studio-{pid}.sock"));
    let mut socket = UnixStream::connect(&socket_path).with_context(|| {
        format!(
            "Studio process {pid} was not launched with Renium's native helper; restart Roblox Studio"
        )
    })?;
    socket.set_read_timeout(Some(Duration::from_secs(30)))?;
    socket.set_write_timeout(Some(Duration::from_secs(5)))?;
    let mut request = Vec::with_capacity(56 + path_bytes.len() + title_bytes.len());
    request.extend_from_slice(&REQUEST_MAGIC.to_le_bytes());
    request.extend_from_slice(&REQUEST_VERSION.to_le_bytes());
    request.extend_from_slice(&1u32.to_le_bytes());
    request.extend_from_slice(&path_length.to_le_bytes());
    request.extend_from_slice(&title_length.to_le_bytes());
    request.extend_from_slice(&0u32.to_le_bytes());
    request.extend_from_slice(&trace.factory_rva.to_le_bytes());
    request.extend_from_slice(&trace.execute_rva.to_le_bytes());
    request.extend_from_slice(&trace.image_uuid);
    request.extend_from_slice(path_bytes);
    request.extend_from_slice(title_bytes);
    socket
        .write_all(&request)
        .context("Could not send the native snapshot request to Studio")?;
    let mut response = [0u8; RESPONSE_SIZE];
    socket
        .read_exact(&mut response)
        .context("Studio native serializer closed without a complete response")?;
    if read_u32(&response, 0) != Some(REQUEST_MAGIC) {
        bail!("Studio native serializer returned an invalid response");
    }
    let status = read_u32(&response, 4).unwrap_or(u32::MAX);
    let output_size = read_u64(&response, 8).unwrap_or(0);
    let elapsed_ms = read_u64(&response, 16).unwrap_or(0) as f64 / 1000.0;
    if status != 0 {
        let error_bytes = &response[24..];
        let end = error_bytes
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(error_bytes.len());
        let error = String::from_utf8_lossy(&error_bytes[..end]);
        bail!("Studio native serializer failed with status {status}: {error}");
    }
    Ok((output_size, elapsed_ms))
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
    let executable = process_executable_path(pid)?;
    let trace_started = Instant::now();
    let trace = trace_studio(&executable)?;
    let trace_ms = trace_started.elapsed().as_secs_f64() * 1000.0;
    let temporary = temporary_output_path(output, pid)?;
    let result = (|| -> Result<NativeSnapshot> {
        let invoke_started = Instant::now();
        let (reported_size, serialize_ms) = invoke_helper(pid, trace, &temporary, studio_title)?;
        let invoke_ms = invoke_started.elapsed().as_secs_f64() * 1000.0;
        let expected_roots = NativeSnapshotRoots {
            exact_service: None,
            containing_service: service,
        };
        let (instance_count, validate_ms) =
            finalize_native_snapshot(&temporary, output, reported_size, expected_roots)?;
        Ok(NativeSnapshot {
            instance_count,
            output_size: reported_size,
            trace_ms,
            discover_ms: 0.0,
            helper_ms: 0.0,
            invoke_ms,
            validate_ms,
            context_ms: 0.0,
            collect_ms: 0.0,
            serialize_ms,
            write_ms: 0.0,
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

fn command_output(command: &mut Command, label: &str) -> Result<Output> {
    command
        .output()
        .with_context(|| format!("Failed to run {label}"))
}

fn run_command(command: &mut Command, label: &str) -> Result<()> {
    let output = command_output(command, label)?;
    if !output.status.success() {
        let error = String::from_utf8_lossy(&output.stderr);
        bail!("{label} failed: {}", error.trim());
    }
    Ok(())
}

fn studio_bundle_version(studio: &Path) -> Result<u64> {
    let info = studio.join("Contents/Info.plist");
    let output = command_output(
        Command::new("plutil")
            .args(["-extract", "CFBundleVersion", "raw", "-o", "-"])
            .arg(&info),
        "Roblox Studio version check",
    )?;
    if !output.status.success() {
        bail!(
            "Could not read the Roblox Studio version from {}: {}",
            info.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse::<u64>()
        .with_context(|| format!("Invalid Roblox Studio version in {}", info.display()))
}

fn installed_studio_paths() -> Result<[PathBuf; 2]> {
    let home = std::env::var("HOME").context("HOME is not set")?;
    Ok([
        PathBuf::from("/Applications/RobloxStudio.app"),
        PathBuf::from(&home)
            .join("Applications")
            .join("RobloxStudio.app"),
    ])
}

fn studio_engine_executable(studio: &Path) -> PathBuf {
    let patched = studio.join("Contents/MacOS/RobloxStudio.bin");
    if patched.is_file() {
        patched
    } else {
        studio.join("Contents/MacOS/RobloxStudio")
    }
}

fn source_studio_path() -> Result<PathBuf> {
    let candidates = installed_studio_paths()?
        .into_iter()
        .filter(|path| {
            let macos = path.join("Contents/MacOS");
            macos.join("RobloxStudio").is_file() || macos.join("RobloxStudio.bin").is_file()
        })
        .map(|path| studio_bundle_version(&path).map(|version| (version, path)))
        .collect::<Result<Vec<_>>>()?;
    candidates
        .into_iter()
        .max_by_key(|(version, _)| *version)
        .map(|(_, path)| path)
        .context("RobloxStudio.app was not found in /Applications or ~/Applications")
}

pub fn source_studio_platform_key() -> Result<String> {
    let executable = studio_engine_executable(&source_studio_path()?);
    let output = Command::new("lipo")
        .arg("-archs")
        .arg(&executable)
        .output()
        .context("Failed to inspect the Roblox Studio architecture")?;
    if !output.status.success() {
        bail!("lipo could not inspect {}", executable.display());
    }
    let architectures = String::from_utf8_lossy(&output.stdout);
    let has_architecture = |wanted| {
        architectures
            .split_whitespace()
            .any(|value| value == wanted)
    };
    let has_arm64 = has_architecture("arm64");
    let has_x86_64 = has_architecture("x86_64");
    let current = std::env::consts::ARCH;
    let architecture = if (current == "aarch64" && has_arm64) || (current == "x86_64" && has_x86_64)
    {
        current
    } else if has_arm64 && !has_x86_64 {
        "aarch64"
    } else if has_x86_64 && !has_arm64 {
        "x86_64"
    } else {
        bail!(
            "{} does not contain the {current} architecture required by this Renium build",
            executable.display()
        );
    };
    Ok(format!("macos-{architecture}"))
}

fn legacy_managed_studio_path() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join("Applications/Renium Studio.app"))
}

pub fn patched_studio_path() -> Result<PathBuf> {
    source_studio_path()
}

const PATCH_FORMAT: u32 = 2;
const PATCH_MARKER_NAME: &str = "ReniumStudioPatch.version";
const PATCH_HELPER_NAME: &str = "ReniumStudioHelper.dylib";

#[derive(Clone, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct StudioPatchState {
    format: u32,
    studio: PathBuf,
    bundle_version: u64,
    original_sha256: String,
    #[serde(default)]
    patched_launcher_sha256: String,
}

fn studio_patch_root() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home)
        .join("Library/Application Support/Renium")
        .join("studio-patch"))
}

fn studio_patch_baseline() -> Result<PathBuf> {
    Ok(studio_patch_root()?.join("baseline"))
}

fn studio_patch_journal() -> Result<PathBuf> {
    Ok(studio_patch_root()?.join("journal.json"))
}

fn studio_patch_state_path() -> Result<PathBuf> {
    Ok(studio_patch_baseline()?.join("state.json"))
}

fn studio_native_root() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home)
        .join("Library/Application Support/Renium")
        .join("native"))
}

fn studio_external_helper() -> Result<PathBuf> {
    Ok(studio_native_root()?.join(PATCH_HELPER_NAME))
}

fn studio_external_helper_state() -> Result<PathBuf> {
    Ok(studio_native_root()?.join("helper.version"))
}

fn studio_original_backup() -> Result<PathBuf> {
    Ok(studio_patch_baseline()?.join("RobloxStudio"))
}

fn studio_signature_backup() -> Result<PathBuf> {
    Ok(studio_patch_baseline()?.join("_CodeSignature"))
}

fn studio_launcher(studio: &Path) -> PathBuf {
    studio.join("Contents/MacOS/RobloxStudio")
}

fn studio_patched_engine(studio: &Path) -> PathBuf {
    studio.join("Contents/MacOS/RobloxStudio.bin")
}

fn studio_legacy_patch_helper(studio: &Path) -> PathBuf {
    studio.join("Contents/Frameworks").join(PATCH_HELPER_NAME)
}

fn studio_patch_marker(studio: &Path) -> PathBuf {
    studio.join("Contents/Resources").join(PATCH_MARKER_NAME)
}

fn sha256_file(path: &Path) -> Result<String> {
    use sha2::{Digest, Sha256};

    let file = File::open(path).with_context(|| format!("Could not read {}", path.display()))?;
    let mut reader = BufReader::new(file);
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 1024 * 1024];
    loop {
        let length = reader
            .read(&mut buffer)
            .with_context(|| format!("Could not read {}", path.display()))?;
        if length == 0 {
            break;
        }
        hasher.update(&buffer[..length]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn expected_patch_signature(state: &StudioPatchState) -> String {
    format!(
        "{}:{}:{}:{}",
        state.format,
        state.bundle_version,
        state.original_sha256,
        sha256_hex(LAUNCHER_BYTES)
    )
}

fn external_helper_is_current() -> bool {
    let Ok(path) = studio_external_helper() else {
        return false;
    };
    let Ok(state_path) = studio_external_helper_state() else {
        return false;
    };
    let Ok(state) = fs::read_to_string(state_path) else {
        return false;
    };
    let Some((source_hash, installed_hash)) = state.trim().split_once(':') else {
        return false;
    };
    source_hash == sha256_hex(HELPER_BYTES)
        && sha256_file(&path).ok().as_deref() == Some(installed_hash)
}

fn install_external_helper() -> Result<()> {
    if external_helper_is_current() {
        return Ok(());
    }
    let root = studio_native_root()?;
    fs::create_dir_all(&root).with_context(|| format!("Could not create {}", root.display()))?;
    let helper = studio_external_helper()?;
    let next = root.join(format!(
        ".{PATCH_HELPER_NAME}.next-{}-{}",
        std::process::id(),
        current_millis()
    ));
    let result = (|| -> Result<()> {
        atomic_write_file(&next, HELPER_BYTES)?;
        fs::set_permissions(&next, fs::Permissions::from_mode(0o755))?;
        run_command(
            Command::new("codesign")
                .args(["--force", "--sign", "-", "--timestamp=none"])
                .arg(&next),
            "Renium Studio helper signing",
        )?;
        run_command(
            Command::new("codesign")
                .args(["--verify", "--strict"])
                .arg(&next),
            "Renium Studio helper signature verification",
        )?;
        fs::rename(&next, &helper)
            .with_context(|| format!("Could not install {}", helper.display()))?;
        let state = format!("{}:{}\n", sha256_hex(HELPER_BYTES), sha256_file(&helper)?);
        atomic_write_file(&studio_external_helper_state()?, state.as_bytes())
    })();
    if result.is_err() {
        let _ = remove_file_if_present(&next);
    }
    result
}

fn read_patch_state() -> Result<Option<StudioPatchState>> {
    let path = studio_patch_state_path()?;
    if !path.is_file() {
        return Ok(None);
    }
    let bytes = fs::read(&path).with_context(|| format!("Could not read {}", path.display()))?;
    let state = serde_json::from_slice(&bytes)
        .with_context(|| format!("Invalid Renium Studio patch state at {}", path.display()))?;
    Ok(Some(state))
}

fn write_patch_state(path: &Path, state: &StudioPatchState) -> Result<()> {
    let mut bytes = serde_json::to_vec_pretty(state)?;
    bytes.push(b'\n');
    atomic_write_file(path, &bytes)
}

fn remove_file_if_present(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("Could not remove {}", path.display())),
    }
}

fn remove_directory_if_present(path: &Path) -> Result<()> {
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("Could not remove {}", path.display())),
    }
}

fn copy_directory(source: &Path, destination: &Path, label: &str) -> Result<()> {
    run_command(Command::new("ditto").arg(source).arg(destination), label)
}

fn studio_bundle_identifier(studio: &Path) -> Result<String> {
    let info = studio.join("Contents/Info.plist");
    let output = command_output(
        Command::new("plutil")
            .args(["-extract", "CFBundleIdentifier", "raw", "-o", "-"])
            .arg(&info),
        "Roblox Studio identifier check",
    )?;
    if !output.status.success() {
        bail!(
            "Could not read the Roblox Studio identifier from {}: {}",
            info.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn verify_unmodified_studio(studio: &Path) -> Result<()> {
    if studio_bundle_identifier(studio)? != "com.Roblox.RobloxStudio" {
        bail!(
            "{} is not an official Roblox Studio bundle",
            studio.display()
        );
    }
    run_command(
        Command::new("codesign")
            .args(["--verify", "--deep", "--strict"])
            .arg(studio),
        "Roblox Studio signature verification",
    )
}

fn patch_artifacts_present(studio: &Path) -> bool {
    studio_patched_engine(studio).exists()
        || studio_legacy_patch_helper(studio).exists()
        || studio_patch_marker(studio).exists()
        || fs::read(studio_launcher(studio))
            .ok()
            .as_deref()
            .is_some_and(|bytes| bytes == LAUNCHER_BYTES)
}

fn patch_is_current(studio: &Path, state: &StudioPatchState) -> bool {
    state.format == PATCH_FORMAT
        && state.studio == studio
        && studio_bundle_version(studio).ok() == Some(state.bundle_version)
        && fs::read(studio_patch_marker(studio))
            .ok()
            .is_some_and(|bytes| bytes == expected_patch_signature(state).as_bytes())
        && !state.patched_launcher_sha256.is_empty()
        && sha256_file(&studio_launcher(studio)).ok().as_deref()
            == Some(state.patched_launcher_sha256.as_str())
        && studio_patched_engine(studio).is_file()
        && studio_original_backup()
            .ok()
            .and_then(|path| sha256_file(&path).ok())
            .as_deref()
            == Some(state.original_sha256.as_str())
}

fn ensure_studio_closed(studio: &Path) -> Result<()> {
    let output = command_output(
        Command::new("ps").args(["-axo", "command="]),
        "Roblox Studio process check",
    )?;
    if !output.status.success() {
        bail!("Could not inspect running Roblox Studio processes");
    }
    let executables = [
        studio_launcher(studio),
        studio_patched_engine(studio),
        studio.join("Contents/MacOS/ReniumStudio"),
        studio.join("Contents/MacOS/RobloxStudio.app/Contents/MacOS/RobloxStudio"),
        studio
            .join("Contents/MacOS/RobloxStudioInstaller.app/Contents/MacOS/RobloxStudioInstaller"),
    ];
    if String::from_utf8_lossy(&output.stdout).lines().any(|line| {
        let line = line.trim_start();
        executables
            .iter()
            .any(|executable| line.starts_with(executable.to_string_lossy().as_ref()))
    }) {
        bail!("Close Roblox Studio before Renium updates its native helper");
    }
    Ok(())
}

fn capture_studio_baseline(studio: &Path) -> Result<StudioPatchState> {
    verify_unmodified_studio(studio)?;
    if patch_artifacts_present(studio) {
        bail!(
            "{} contains an unrecognized partial Renium patch",
            studio.display()
        );
    }
    let root = studio_patch_root()?;
    fs::create_dir_all(&root).with_context(|| format!("Could not create {}", root.display()))?;
    let next = root.join(format!(
        "baseline.next-{}-{}",
        std::process::id(),
        current_millis()
    ));
    fs::create_dir(&next).with_context(|| format!("Could not create {}", next.display()))?;
    let state = StudioPatchState {
        format: PATCH_FORMAT,
        studio: studio.to_path_buf(),
        bundle_version: studio_bundle_version(studio)?,
        original_sha256: sha256_file(&studio_launcher(studio))?,
        patched_launcher_sha256: String::new(),
    };
    let result = (|| -> Result<()> {
        fs::copy(studio_launcher(studio), next.join("RobloxStudio"))
            .with_context(|| "Could not back up the Roblox Studio executable")?;
        copy_directory(
            &studio.join("Contents/_CodeSignature"),
            &next.join("_CodeSignature"),
            "Roblox Studio signature backup",
        )?;
        write_patch_state(&next.join("state.json"), &state)?;
        if sha256_file(&next.join("RobloxStudio"))? != state.original_sha256 {
            bail!("Roblox Studio executable backup verification failed");
        }
        Ok(())
    })();
    if let Err(error) = result {
        let _ = fs::remove_dir_all(&next);
        return Err(error);
    }
    let baseline = studio_patch_baseline()?;
    let previous = root.join("baseline.previous");
    remove_directory_if_present(&previous)?;
    if baseline.exists() {
        fs::rename(&baseline, &previous).with_context(|| {
            format!(
                "Could not move {} to {}",
                baseline.display(),
                previous.display()
            )
        })?;
    }
    if let Err(error) = fs::rename(&next, &baseline) {
        if previous.exists() {
            let _ = fs::rename(&previous, &baseline);
        }
        return Err(error).with_context(|| format!("Could not publish {}", baseline.display()));
    }
    remove_directory_if_present(&previous)?;
    Ok(state)
}

fn extracted_entitlements(executable: &Path) -> Result<String> {
    let output = command_output(
        Command::new("codesign")
            .args(["-d", "--entitlements", ":-"])
            .arg(executable),
        "codesign entitlement extraction",
    )?;
    if !output.status.success() {
        let error = String::from_utf8_lossy(&output.stderr);
        bail!("codesign entitlement extraction failed: {}", error.trim());
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    [stdout.as_ref(), stderr.as_ref()]
        .into_iter()
        .find_map(|text| {
            let start = text.find("<?xml").or_else(|| text.find("<plist"))?;
            let relative_end = text[start..].find("</plist>")?;
            let end = start + relative_end + "</plist>".len();
            Some(text[start..end].to_string())
        })
        .context("codesign did not return a readable entitlement property list")
}

fn add_entitlement(mut plist: String, key: &str) -> Result<String> {
    let marker = format!("<key>{key}</key>");
    if let Some(key_start) = plist.find(&marker) {
        let value_start = key_start + marker.len();
        if let Some(relative) = plist[value_start..].find("<false/>") {
            let start = value_start + relative;
            plist.replace_range(start..start + "<false/>".len(), "<true/>");
        }
        return Ok(plist);
    }
    let insert = plist
        .rfind("</dict>")
        .context("Extracted Studio entitlements are not a property-list dictionary")?;
    plist.insert_str(insert, &format!("<key>{key}</key><true/>\n"));
    Ok(plist)
}

fn restore_unmodified_studio(state: &StudioPatchState) -> Result<()> {
    let studio = &state.studio;
    if !studio.is_dir() {
        bail!(
            "Roblox Studio is no longer installed at {}",
            studio.display()
        );
    }
    if studio_bundle_version(studio)? != state.bundle_version {
        bail!("Refusing to restore an older Roblox Studio executable over a newer installation");
    }
    let original = studio_original_backup()?;
    if sha256_file(&original)? != state.original_sha256 {
        bail!("The Roblox Studio rollback executable no longer matches its recorded hash");
    }
    ensure_studio_closed(studio)?;
    let launcher = studio_launcher(studio);
    let temporary = launcher.with_extension("renium-restore");
    remove_file_if_present(&temporary)?;
    fs::copy(&original, &temporary)
        .with_context(|| format!("Could not restore {}", launcher.display()))?;
    fs::set_permissions(&temporary, fs::metadata(&original)?.permissions())?;
    remove_file_if_present(&launcher)?;
    fs::rename(&temporary, &launcher)
        .with_context(|| format!("Could not restore {}", launcher.display()))?;
    remove_file_if_present(&studio_patched_engine(studio))?;
    remove_file_if_present(&studio_legacy_patch_helper(studio))?;
    remove_file_if_present(&studio_patch_marker(studio))?;
    let signature = studio.join("Contents/_CodeSignature");
    remove_directory_if_present(&signature)?;
    copy_directory(
        &studio_signature_backup()?,
        &signature,
        "Roblox Studio signature restore",
    )?;
    verify_unmodified_studio(studio)
}

fn remove_stale_patch_from_updated_studio(state: &StudioPatchState) -> Result<bool> {
    let studio = &state.studio;
    if !studio.is_dir()
        || studio_bundle_version(studio).ok() == Some(state.bundle_version)
        || fs::read(studio_launcher(studio)).ok().as_deref() == Some(LAUNCHER_BYTES)
    {
        return Ok(false);
    }
    ensure_studio_closed(studio)?;
    remove_file_if_present(&studio_patched_engine(studio))?;
    remove_file_if_present(&studio_legacy_patch_helper(studio))?;
    remove_file_if_present(&studio_patch_marker(studio))?;
    verify_unmodified_studio(studio)?;
    remove_directory_if_present(&studio_patch_baseline()?)?;
    remove_file_if_present(&studio_patch_journal()?)?;
    Ok(true)
}

pub fn recover_studio_patch_install() -> Result<()> {
    let journal = studio_patch_journal()?;
    if !journal.is_file() {
        return Ok(());
    }
    let Some(state) = read_patch_state()? else {
        bail!("Renium Studio patch recovery is missing its rollback baseline");
    };
    if remove_stale_patch_from_updated_studio(&state)? {
        return Ok(());
    }
    if patch_is_current(&state.studio, &state) {
        remove_file_if_present(&journal)?;
        return Ok(());
    }
    restore_unmodified_studio(&state)?;
    remove_file_if_present(&journal)
}

fn install_studio_patch(studio: &Path, state: &StudioPatchState) -> Result<()> {
    ensure_studio_closed(studio)?;
    let root = studio_patch_root()?;
    fs::create_dir_all(&root).with_context(|| format!("Could not create {}", root.display()))?;
    write_patch_state(&studio_patch_journal()?, state)?;
    let entitlements = root.join("entitlements.plist");
    let result = (|| -> Result<()> {
        let entitlement_text = add_entitlement(
            add_entitlement(
                extracted_entitlements(&studio_original_backup()?)?,
                "com.apple.security.cs.allow-dyld-environment-variables",
            )?,
            "com.apple.security.cs.disable-library-validation",
        )?;
        atomic_write_file(&entitlements, entitlement_text.as_bytes())?;
        fs::rename(studio_launcher(studio), studio_patched_engine(studio))
            .with_context(|| format!("Could not prepare {} for Renium", studio.display()))?;
        atomic_write_file(&studio_launcher(studio), LAUNCHER_BYTES)?;
        remove_file_if_present(&studio_legacy_patch_helper(studio))?;
        fs::set_permissions(studio_launcher(studio), fs::Permissions::from_mode(0o755))?;
        atomic_write_file(
            &studio_patch_marker(studio),
            expected_patch_signature(state).as_bytes(),
        )?;
        run_command(
            Command::new("codesign")
                .args(["--force", "--sign", "-"])
                .arg(studio_launcher(studio)),
            "Renium Studio launcher signing",
        )?;
        run_command(
            Command::new("codesign")
                .args([
                    "--force",
                    "--sign",
                    "-",
                    "--options",
                    "runtime",
                    "--entitlements",
                ])
                .arg(&entitlements)
                .arg(studio_patched_engine(studio)),
            "Roblox Studio executable signing",
        )?;
        run_command(
            Command::new("codesign")
                .args([
                    "--force",
                    "--sign",
                    "-",
                    "--options",
                    "runtime",
                    "--entitlements",
                ])
                .arg(&entitlements)
                .arg(studio),
            "Roblox Studio app signing",
        )?;
        run_command(
            Command::new("codesign")
                .args(["--verify", "--deep", "--strict"])
                .arg(studio),
            "Patched Roblox Studio signature verification",
        )?;
        let mut installed_state = state.clone();
        installed_state.patched_launcher_sha256 = sha256_file(&studio_launcher(studio))?;
        write_patch_state(&studio_patch_state_path()?, &installed_state)?;
        Ok(())
    })();
    let _ = remove_file_if_present(&entitlements);
    if let Err(error) = result {
        return match restore_unmodified_studio(state) {
            Ok(()) => Err(error).context("The original Roblox Studio app was restored"),
            Err(rollback) => {
                Err(error).context(format!("Roblox Studio patch rollback failed: {rollback:#}"))
            }
        };
    }
    remove_file_if_present(&studio_patch_journal()?)
}

fn remove_legacy_managed_studio() -> Result<()> {
    let legacy = legacy_managed_studio_path()?;
    if legacy.join("Contents/MacOS/ReniumStudio").is_file()
        && legacy
            .join("Contents/Resources/ReniumStudio.version")
            .is_file()
    {
        ensure_studio_closed(&legacy)?;
        remove_directory_if_present(&legacy)?;
    }
    let parent = legacy
        .parent()
        .context("Legacy Renium Studio path has no parent")?;
    if parent.is_dir() {
        for entry in fs::read_dir(parent)? {
            let entry = entry?;
            if entry.file_type()?.is_dir()
                && entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| name.starts_with(".Renium Studio.transaction-"))
            {
                remove_directory_if_present(&entry.path())?;
            }
        }
    }
    Ok(())
}

fn clear_studio_patch_state() -> Result<()> {
    let patch_root = studio_patch_root()?;
    remove_directory_if_present(&patch_root)?;
    if let Some(renium_root) = patch_root.parent() {
        let _ = fs::remove_dir(renium_root);
    }
    Ok(())
}

fn remove_external_helper() -> Result<()> {
    let native_root = studio_native_root()?;
    remove_directory_if_present(&native_root)?;
    if let Some(renium_root) = native_root.parent() {
        let _ = fs::remove_dir(renium_root);
    }
    Ok(())
}

pub struct StudioPatchRemoval {
    active: bool,
}

impl StudioPatchRemoval {
    pub fn rollback(self) -> Result<()> {
        if self.active {
            setup_studio_patch(false)?;
        }
        Ok(())
    }

    pub fn commit(self) -> Result<()> {
        if self.active {
            clear_studio_patch_state()?;
        }
        remove_external_helper()?;
        remove_legacy_managed_studio()
    }
}

pub fn begin_studio_patch_removal() -> Result<StudioPatchRemoval> {
    recover_studio_patch_install()?;
    let state = read_patch_state()?;
    if let Some(state) = state.as_ref() {
        if remove_stale_patch_from_updated_studio(state)? {
            return Ok(StudioPatchRemoval { active: false });
        }
        if patch_artifacts_present(&state.studio) {
            write_patch_state(&studio_patch_journal()?, state)?;
            restore_unmodified_studio(state)?;
            remove_file_if_present(&studio_patch_journal()?)?;
        }
    }
    Ok(StudioPatchRemoval {
        active: state.is_some(),
    })
}

pub fn setup_studio_patch(dry_run: bool) -> Result<PathBuf> {
    if HELPER_BYTES.is_empty() || LAUNCHER_BYTES.is_empty() {
        bail!("This Renium build does not contain macOS Studio helper artifacts");
    }
    let studio = source_studio_path()?;
    if dry_run {
        return Ok(studio);
    }
    recover_studio_patch_install()?;
    install_external_helper()?;
    if let Some(state) = read_patch_state()? {
        if patch_is_current(&studio, &state) {
            return Ok(studio);
        }
        if state.studio != studio && state.studio.is_dir() && patch_artifacts_present(&state.studio)
        {
            write_patch_state(&studio_patch_journal()?, &state)?;
            restore_unmodified_studio(&state)?;
            clear_studio_patch_state()?;
        } else if remove_stale_patch_from_updated_studio(&state)? {
        } else if state.studio == studio && patch_artifacts_present(&studio) {
            write_patch_state(&studio_patch_journal()?, &state)?;
            restore_unmodified_studio(&state)?;
        }
    }
    let state = capture_studio_baseline(&studio)?;
    install_studio_patch(&studio, &state)?;
    remove_legacy_managed_studio()?;
    Ok(studio)
}
