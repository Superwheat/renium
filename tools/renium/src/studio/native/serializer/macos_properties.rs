//! macOS reflection discovery lives in the Rust host. The in-process helper
//! performs bounded memory copies and owns only the engine's C++ ABI calls.
use super::*;
use std::collections::HashSet;
use std::io::{Seek, SeekFrom};
use std::sync::Arc;

#[path = "macos_history.rs"]
mod history;

#[path = "macos_terrain.rs"]
mod terrain;
pub(crate) use terrain::prepare_terrain;
#[path = "macos_terrain_observation.rs"]
mod terrain_observation;
pub(crate) use terrain_observation::observe_terrain;

pub(crate) fn register_history(pid: u32, title: &str, token: &str) -> Result<()> {
    anyhow::ensure!(
        !token.is_empty() && token.len() <= 256,
        "Invalid Studio recording token"
    );
    let mut prepared = prepare_property(
        pid,
        title,
        &["ChangeHistoryService".into()],
        &[],
        "Name",
        Duration::from_secs(3),
    )?;
    let mut binding = prepared.invoke(6)?[16..].to_vec();
    if binding.iter().all(|byte| *byte == 0) {
        binding = history::binding(&prepared)?;
    }
    binding.extend_from_slice(token.as_bytes());
    put32(&mut prepared.parameters, 136, binding.len() as u32);
    prepared.parameters[680..680 + binding.len()].copy_from_slice(&binding);
    prepared.invoke(6)?;
    Ok(())
}

#[cfg(target_arch = "aarch64")]
#[path = "macos_identity.rs"]
mod identity;
#[cfg(target_arch = "aarch64")]
pub(crate) use identity::capture_identities;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Argument {
    Descriptor,
    DescriptorField,
    Instance,
    Output,
    Input,
    Parsed,
    Scratch(i64),
    TextBytes(i64, usize),
    Binding(usize),
    Vtable(usize),
    Function(usize, usize),
    Low,
    High,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum CallKind {
    Text,
    Identity,
    Setter,
}

fn loaded_argument(base: Option<Argument>, offset: usize, bindings: &[usize]) -> Option<Argument> {
    use Argument::*;
    match base {
        Some(Descriptor) if bindings.contains(&offset) => Some(Binding(offset)),
        Some(Descriptor) => Some(DescriptorField),
        Some(Input) if offset == 0 => Some(Input),
        Some(Binding(field)) if offset == 0 => Some(Vtable(field)),
        Some(Vtable(field)) => Some(Function(field, offset)),
        _ => None,
    }
}

fn indirect_binding(
    registers: &[Option<Argument>; 32],
    base: usize,
    kind: CallKind,
) -> Option<(usize, usize)> {
    use Argument::*;
    let Function(field, slot) = registers[base]? else {
        return None;
    };
    (registers[0] == Some(Binding(field))
        && registers[1] == Some(Instance)
        && (kind != CallKind::Setter || matches!(registers[2], Some(Scratch(_)))))
    .then_some((field, slot))
}

fn parses_input(registers: &[Option<Argument>; 32]) -> bool {
    use Argument::*;
    registers[0] == Some(Input) && matches!(registers[1], Some(Scratch(_)))
        || registers[..2] == [Some(DescriptorField), Some(Input)]
            && matches!(registers[2], Some(Scratch(_)))
}

fn copy_returned_text(
    bytes: &mut [Option<i64>; 24],
    offset: usize,
    source: Option<Argument>,
    temporary: Option<i64>,
) -> bool {
    let Some(Argument::TextBytes(source, length)) = source else {
        return false;
    };
    let Some(output) = bytes.get_mut(offset..offset + length) else {
        return false;
    };
    for (index, byte) in output.iter_mut().enumerate() {
        *byte = Some(source + index as i64);
    }
    bytes[0].is_some_and(|first| {
        // A returned string or the payload after ContentId's eight-byte header.
        temporary.is_some_and(|start| first == start || first == start + 8)
            && bytes
                .iter()
                .enumerate()
                .all(|(index, byte)| *byte == Some(first + index as i64))
    })
}

fn adjusted_argument(base: Option<Argument>, word: u32) -> Option<Argument> {
    let offset = (((word >> 10) & 4095) as i64) << if word & (1 << 22) != 0 { 12 } else { 0 };
    match base {
        Some(Argument::Scratch(address)) => {
            Some(Argument::Scratch(if word & 0xff000000 == 0xd1000000 {
                address - offset
            } else {
                address + offset
            }))
        }
        Some(Argument::Instance) => Some(Argument::Instance),
        _ => None,
    }
}

// ARM64 instructions are fixed-width. Track arguments through register moves
// and field loads; relocated fields, vtable slots and scratch registers are not
// signatures. A call also needs its expected result/argument use to validate.
fn data_instruction(
    word: u32,
    offsets: &[usize],
    registers: &mut [Option<Argument>; 32],
    vectors: &mut [Option<Argument>; 32],
    copied_text: &mut [Option<i64>; 24],
    temporary_result: Option<i64>,
) -> Option<bool> {
    use Argument::*;
    let dst = (word & 31) as usize;
    let base = ((word >> 5) & 31) as usize;
    let mut copied = false;
    if word & 0xffe0ffe0 == 0xaa0003e0 {
        // mov Xd, Xn
        registers[dst] = registers[((word >> 16) & 31) as usize];
    } else if word & 0xffc00000 == 0xf9400000 {
        // ldr Xd, [Xn,#unsigned]
        let offset = ((word >> 10) & 4095) as usize * 8;
        registers[dst] = returned_text_bytes(registers[base], offset as i64, 8, temporary_result)
            .or_else(|| loaded_argument(registers[base], offset, offsets));
    } else if word & 0xffe00c00 == 0xf8400000 {
        // ldur Xd,[Xn,#signed]
        let offset = ((word << 11) as i32 >> 23) as i64;
        registers[dst] = returned_text_bytes(registers[base], offset, 8, temporary_result);
    } else if word & 0xffe00c00 == 0x3cc00000 {
        // ldur Qd,[Xn,#signed]: libc++ can inline its short-string copy.
        let offset = ((word << 11) as i32 >> 23) as i64;
        vectors[dst] = returned_text_bytes(registers[base], offset, 16, temporary_result);
    } else if word & 0xffc00000 == 0x3dc00000 {
        vectors[dst] = returned_text_bytes(
            registers[base],
            ((word >> 10) & 4095) as i64 * 16,
            16,
            temporary_result,
        );
    } else if word & 0xffc00000 == 0xf9000000 || word & 0xffc00000 == 0x3d800000 {
        let vector = word & 0xffc00000 == 0x3d800000;
        let offset = ((word >> 10) & 4095) as usize * if vector { 16 } else { 8 };
        if registers[base] == Some(Output) {
            copied |= copy_returned_text(
                copied_text,
                offset,
                if vector { vectors[dst] } else { registers[dst] },
                temporary_result,
            );
        }
    } else if word & 0xffc00000 == 0xb9000000 {
        // str Wt,[Xn,#unsigned] writes memory, not Wt or SP. Vector parsers
        // initialize their stack result with STR WZR before receiving input.
    } else if word & 0xffe00c00 == 0x9a800000 {
        // csel: the short/long string arms must both denote input text.
        let other = ((word >> 16) & 31) as usize;
        registers[dst] = if registers[base] == registers[other] {
            registers[base]
        } else {
            None
        };
    } else if matches!(word & 0xff000000, 0x91000000 | 0xd1000000) {
        // add immediate (including SP)
        registers[dst] = adjusted_argument(registers[base], word);
    } else {
        return None;
    }
    Some(copied)
}

fn binding_call(code: &[u8], offsets: &[usize], kind: CallKind) -> Option<(usize, usize)> {
    use Argument::*;
    let mut registers = [None; 32];
    registers[31] = Some(Scratch(0));
    registers[0] = Some(Descriptor);
    registers[1] = Some(Instance);
    if kind == CallKind::Text {
        registers[8] = Some(Output);
    } else if kind == CallKind::Setter {
        registers[2] = Some(Input);
    }
    let mut found = None;
    let mut result_verified = false;
    let mut temporary_result = None;
    let mut vectors = [None; 32];
    let mut copied_text = [None; 24];
    for bytes in code.as_chunks::<4>().0 {
        let word = read_u32(bytes, 0)?;
        let dst = (word & 31) as usize;
        let base = ((word >> 5) & 31) as usize;
        if word & 0x3f00001f == 0x3100001f {
            // ADDS/SUBS immediate with Rd=31 (CMN/CMP) update flags only.
            // This encoding denotes the zero register, not the stack pointer.
            continue;
        }
        if word == 0xd65f03c0 {
            // ret
            return (result_verified || kind == CallKind::Setter && registers[0] == Some(Parsed))
                .then_some(found)
                .flatten();
        } else if let Some(copied) = data_instruction(
            word,
            offsets,
            &mut registers,
            &mut vectors,
            &mut copied_text,
            temporary_result,
        ) {
            result_verified |= copied;
        } else if word & 0xfffffc1f == 0xd63f0000 {
            // blr Xn
            let call = indirect_binding(&registers, base, kind);
            // A second different binding call cannot prove this ABI.
            if found.is_some() && call.is_some() && found != call {
                return None;
            }
            found = call.or(found);
            if kind == CallKind::Text && call.is_some() {
                temporary_result = match registers[8] {
                    Some(Scratch(address)) => Some(address),
                    _ => None,
                };
            }
            registers[..19].fill(None);
            if kind == CallKind::Identity && call.is_some() {
                registers[0] = Some(Low);
                registers[1] = Some(High);
            }
        } else if word & 0xfc000000 == 0x94000000 {
            // bl direct
            result_verified |=
                kind == CallKind::Text && found.is_some() && registers[8] == Some(Output);
            let parsing = kind == CallKind::Setter && parses_input(&registers);
            registers[..19].fill(None);
            if parsing {
                registers[0] = Some(Parsed);
            }
        } else if word & 0xfc000000 == 0x14000000 {
            // tail call
            if kind == CallKind::Text && result_verified {
                // A complete copied return value needs no conversion tail call.
                // Continue through the common cleanup/return epilogue.
                continue;
            }
            return (kind == CallKind::Text && found.is_some() && registers[8] == Some(Output))
                .then_some(found)
                .flatten();
        } else if word & 0xff800000 == 0x52800000 {
            // mov Wd,#imm
            result_verified |= kind == CallKind::Setter
                && found.is_some()
                && dst == 0
                && ((word >> 5) & 65535) == 1;
            registers[dst] = None;
        } else if word & 0xffc00000 == 0xa9000000 {
            // stp Xd,Xt2,[Xn,#imm]
            result_verified |= kind == CallKind::Identity
                && found.is_some()
                && registers[dst] == Some(Low)
                && registers[((word >> 10) & 31) as usize] == Some(High);
        } else if word & 0x3b000000 != 0x29000000 {
            // remaining paired stack saves/restores
            // Conditional branches/tests do not assign registers. Other
            // unrecognized arithmetic cannot establish an argument identity.
            if word & 0xff000010 != 0x54000000
                && word & 0x7e000000 != 0x34000000
                && word & 0x7e000000 != 0x36000000
            {
                registers[dst] = None;
            }
        }
    }
    None
}

fn returned_text_bytes(
    base: Option<Argument>,
    offset: i64,
    length: usize,
    temporary: Option<i64>,
) -> Option<Argument> {
    let Argument::Scratch(base) = base? else {
        return None;
    };
    let source = base + offset;
    let relative = source.checked_sub(temporary?)?;
    (relative >= 0 && relative + length as i64 <= 256)
        .then_some(Argument::TextBytes(source, length))
}

struct Memory {
    pid: u32,
    trace: PackageActionTrace,
    deadline: Instant,
    base: u64,
    executable: PathBuf,
}

#[derive(Clone)]
struct MemberEntry {
    header: Arc<[u8]>,
    descriptor: u64,
}
type MemberKey = (u32, [u8; 16], u64, String);
static MEMBERS: OnceLock<Mutex<HashMap<MemberKey, MemberEntry>>> = OnceLock::new();

#[derive(Clone)]
struct ValidatedAbi {
    field: usize,
    getter_slot: usize,
    setter_slot: usize,
    // Descriptor vtable offset, executable RVA, and verified loaded code.
    functions: Vec<(usize, u64, Vec<u8>)>,
}
type AbiKey = ([u8; 16], u64, CallKind);
static ABIS: OnceLock<Mutex<HashMap<AbiKey, ValidatedAbi>>> = OnceLock::new();

enum StringSource {
    Inline(Vec<u8>),
    Indirect(u64, usize),
}

fn string_source(bytes: &[u8]) -> Option<StringSource> {
    let tag = *bytes.get(23)?;
    if tag & 0x80 == 0 {
        return (tag < 24).then(|| StringSource::Inline(bytes[..usize::from(tag)].to_vec()));
    }
    let length = read_u64(bytes, 8).filter(|size| *size <= 4096)? as usize;
    if length == 0 {
        return Some(StringSource::Inline(Vec::new()));
    }
    let address = read_u64(bytes, 0).filter(|address| *address >= 0x10000)?;
    Some(StringSource::Indirect(address, length))
}

fn name_text(bytes: Vec<u8>) -> Result<String> {
    let text = String::from_utf8(bytes)?;
    anyhow::ensure!(
        !text.bytes().any(|b| b == 0 || b < 9 || (b > 13 && b < 32)),
        "Invalid reflection name text"
    );
    Ok(text)
}

fn member_descriptors(
    class: &[u8],
    mut read: impl FnMut(&[(u64, usize)]) -> Result<Vec<Option<Vec<u8>>>>,
) -> Result<HashMap<String, HashSet<u64>>> {
    anyhow::ensure!(class.len() == 0x218, "Invalid reflection class header");
    let vectors = (0..=0x200)
        .step_by(8)
        .filter_map(|offset| {
            let entries = read_u64(class, offset)?;
            let count = read_u64(class, offset + 8)?;
            let capacity = read_u64(class, offset + 16)?;
            (entries >= 0x10000
                && count > 0
                && count <= 512
                && capacity >= count
                && capacity <= 1024
                && entries.checked_add(count * 16).is_some())
            .then(|| (entries, count as usize * 16))
        })
        .collect::<Vec<_>>();
    let mut descriptors = HashSet::new();
    for bytes in read(&vectors)?.into_iter().flatten() {
        descriptors.extend(bytes.as_chunks::<16>().0.iter().filter_map(|row| {
            read_u64(row, 0).filter(|p| *p >= 0x10000 && p.checked_add(16).is_some())
        }));
    }
    let descriptors = descriptors.into_iter().collect::<Vec<_>>();
    let pointers = read(&descriptors.iter().map(|p| (p + 8, 8)).collect::<Vec<_>>())?;
    let named = descriptors
        .into_iter()
        .zip(pointers)
        .filter_map(|(descriptor, bytes)| {
            let pointer = read_u64(bytes.as_deref()?, 0)?;
            (pointer >= 0x10000 && pointer.checked_add(32).is_some())
                .then_some((descriptor, pointer))
        })
        .collect::<Vec<_>>();
    // Name objects occur at offsets 0 or 8. Read each string header separately:
    // a valid first header must survive an unreadable alternate layout.
    let headers = read(
        &named
            .iter()
            .flat_map(|(_, p)| [(*p, 24), (p + 8, 24)])
            .collect::<Vec<_>>(),
    )?;
    let mut pending = Vec::new();
    let candidates = headers
        .chunks_exact(2)
        .map(|headers| {
            headers
                .iter()
                .filter_map(|bytes| string_source(bytes.as_deref()?))
                .filter_map(|source| match source {
                    StringSource::Inline(bytes) => Some(Ok(bytes)),
                    StringSource::Indirect(address, size)
                        if address.checked_add(size as u64).is_some() =>
                    {
                        pending.push((address, size));
                        Some(Err(pending.len() - 1))
                    }
                    _ => None,
                })
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    let bodies = read(&pending)?;
    let mut members: HashMap<String, HashSet<u64>> = HashMap::new();
    for ((descriptor, _), choices) in named.into_iter().zip(candidates) {
        let name = choices.into_iter().find_map(|choice| {
            let bytes = match choice {
                Ok(bytes) => bytes,
                Err(index) => bodies[index].clone()?,
            };
            name_text(bytes).ok().filter(|name| !name.is_empty())
        });
        if let Some(name) = name {
            members.entry(name).or_default().insert(descriptor);
        }
    }
    Ok(members)
}

impl Memory {
    fn for_process(pid: u32, timeout: Duration) -> Result<Self> {
        let deadline = Instant::now() + timeout;
        let executable = process_executable_path(pid)?;
        Ok(Self {
            pid,
            trace: trace_package_action(&executable)?,
            deadline,
            base: 0,
            executable,
        })
    }

    fn abi_key(&self, table: u64, kind: CallKind) -> Result<AbiKey> {
        Ok((
            self.trace.image_uuid,
            table
                .checked_sub(self.base)
                .context("Reflection vtable is outside Studio")?,
            kind,
        ))
    }

    fn cached_abi(&self, table: u64, kind: CallKind) -> Option<ValidatedAbi> {
        let key = self.abi_key(table, kind).ok()?;
        let entry = ABIS
            .get()?
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&key)?
            .clone();
        if entry.functions.iter().all(|(slot, rva, code)| {
            self.pointer(table + *slot as u64).ok() == Some(self.base + rva)
                && self.read(self.base + rva, code.len()).ok().as_ref() == Some(code)
        }) {
            return Some(entry);
        }
        ABIS.get()?
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&key);
        None
    }

    fn cache_abi(&self, table: u64, kind: CallKind, entry: ValidatedAbi) -> Result<()> {
        let key = self.abi_key(table, kind)?;
        let mut cache = ABIS
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if cache.len() >= 1024 {
            cache.clear();
        }
        cache.insert(key, entry);
        Ok(())
    }

    fn code(&self, address: u64, length: usize) -> Result<Vec<u8>> {
        let start = self.base + self.trace.text.address - self.trace.image_base;
        let relative = address
            .checked_sub(start)
            .filter(|relative| {
                relative
                    .checked_add(length as u64)
                    .is_some_and(|end| end <= self.trace.text.size)
            })
            .context("Reflection function is outside Studio executable code")?;
        let mut file = File::open(&self.executable)?;
        file.seek(SeekFrom::Start(self.trace.text.offset as u64 + relative))?;
        let mut expected = vec![0; length];
        file.read_exact(&mut expected)?;
        anyhow::ensure!(
            self.read(address, length)? == expected,
            "Studio reflection code differs from its executable; refusing the call"
        );
        Ok(expected)
    }

    fn bindings(&self, descriptor: u64) -> Result<Vec<usize>> {
        let bytes = self.read(descriptor, 0x200)?;
        Ok((0x40..0x200)
            .step_by(8)
            .filter(|offset| {
                let ptr = read_u64(&bytes, *offset).unwrap();
                self.rtti(ptr).is_ok_and(|kind| {
                    kind.contains("PropDescriptor")
                        && (kind.contains("10GetSetImplI") || kind.contains("7GetImplI"))
                })
            })
            .collect())
    }

    fn text_functions(&self, descriptor: u64) -> Result<(usize, usize, usize)> {
        let vtable = self.pointer(descriptor)?;
        if let Some(abi) = self.cached_abi(vtable, CallKind::Text) {
            return Ok((abi.field, abi.getter_slot, abi.setter_slot));
        }
        let offsets = self.bindings(descriptor)?;
        let functions = self.read(vtable, 32 * 8)?;
        let mut matches = Vec::new();
        for slot in 16..30 {
            let capability = read_u64(&functions, slot * 8).unwrap();
            if self.code(capability, 8).ok().as_deref()
                != Some(&[0x20, 0, 0x80, 0x52, 0xc0, 3, 0x5f, 0xd6])
            {
                continue;
            }
            let getter = read_u64(&functions, (slot + 1) * 8).unwrap();
            let setter = read_u64(&functions, (slot + 2) * 8).unwrap();
            let getter_code = self.code(getter, 256)?;
            if let Some((field, _)) = binding_call(&getter_code, &offsets, CallKind::Text) {
                let setter_code = self.code(setter, 512)?;
                let setter_slot = if binding_call(&setter_code, &offsets, CallKind::Setter)
                    .is_some_and(|(setter_field, _)| field == setter_field)
                {
                    (slot + 2) * 8
                } else {
                    0
                };
                matches.push(ValidatedAbi {
                    field,
                    getter_slot: (slot + 1) * 8,
                    setter_slot,
                    functions: vec![
                        ((slot + 1) * 8, getter - self.base, getter_code),
                        ((slot + 2) * 8, setter - self.base, setter_code),
                    ],
                });
            }
        }
        anyhow::ensure!(
            matches.len() == 1,
            "Reflection text ABI has {} validated candidates; Renium's detector needs updating if Studio changed this layout",
            matches.len()
        );
        let entry = matches.remove(0);
        let result = (entry.field, entry.getter_slot, entry.setter_slot);
        self.cache_abi(vtable, CallKind::Text, entry)?;
        Ok(result)
    }
    fn request(&self, operation: u32, payload: &[u8], title: &str) -> Result<Vec<u8>> {
        let started = Instant::now();
        let remaining = self
            .deadline
            .checked_duration_since(Instant::now())
            .context("Protected property discovery exceeded its deadline")?;
        let mut stream = UnixStream::connect(format!("/tmp/renium-studio-{}.sock", self.pid))
            .context(
                "Studio needs the matching Renium helper; restart the selected Studio window",
            )?;
        stream.set_read_timeout(Some(remaining))?;
        stream.set_write_timeout(Some(remaining))?;
        let mut request = Vec::with_capacity(56 + payload.len() + title.len());
        for value in [
            REQUEST_MAGIC,
            REQUEST_VERSION,
            3,
            payload.len() as u32,
            title.len() as u32,
            operation,
        ] {
            request.extend_from_slice(&value.to_le_bytes());
        }
        // The helper's command-3 factory field carries the budget, not an RVA.
        // Old helpers ignore the opt-in high bit; untraced calls pay no
        // per-candidate timing cost during cold discovery.
        let phase_timing = if crate::app::output::global_log_enabled(5) {
            1_u64 << 63
        } else {
            0
        };
        request.extend_from_slice(&((remaining.as_millis() as u64) | phase_timing).to_le_bytes());
        request.extend_from_slice(&self.trace.submit_rva.to_le_bytes());
        request.extend_from_slice(&self.trace.image_uuid);
        request.extend_from_slice(payload);
        request.extend_from_slice(title.as_bytes());
        stream.write_all(&request)?;
        let mut response = [0; RESPONSE_SIZE];
        stream.read_exact(&mut response).with_context(|| {
            let recovery = if operation == 2 && matches!(read_u32(payload, 132), Some(2 | 5)) {
                "; a requested write may still finish, so read the property before retrying"
            } else {
                ""
            };
            format!(
                "Studio reflection operation {operation} (PID {}, {} ms remaining) did not return a response before the connection ended or timed out{recovery}",
                self.pid,
                remaining.as_millis(),
            )
        })?;
        if matches!(operation, 0 | 2 | 4) {
            let details = &response[24..];
            let end = details
                .iter()
                .position(|b| *b == 0)
                .unwrap_or(details.len());
            crate::app::output::log_global(
                5,
                format_args!(
                    "[renium] native property transport: pid={} op={operation} title={title:?} wall_us={} helper_us={} status={:?} details={:?}",
                    self.pid,
                    started.elapsed().as_micros(),
                    read_u64(&response, 16).unwrap_or(0),
                    read_u32(&response, 4),
                    String::from_utf8_lossy(&details[..end]),
                ),
            );
        }
        if read_u32(&response, 0) != Some(REQUEST_MAGIC) || read_u32(&response, 4) != Some(0) {
            let error = &response[24..];
            let end = error.iter().position(|b| *b == 0).unwrap_or(error.len());
            bail!(
                "Studio reflection transport: {}",
                String::from_utf8_lossy(&error[..end])
            );
        }
        let limit = if operation == 2 && read_u32(payload, 132) == Some(4) {
            64 * 1024 * 1024
        } else {
            1024 * 1024
        };
        let length = read_u64(&response, 8)
            .filter(|length| *length <= limit)
            .context("Invalid reflection response length")? as usize;
        let mut data = vec![0; length];
        stream
            .read_exact(&mut data)
            .context("Studio reflection response was interrupted")?;
        Ok(data)
    }

    fn read(&self, address: u64, size: usize) -> Result<Vec<u8>> {
        if address < 0x10000 || size == 0 || size > 1024 * 1024 {
            bail!("Invalid bounded reflection address/length");
        }
        let mut payload = address.to_le_bytes().to_vec();
        payload.extend_from_slice(&(size as u32).to_le_bytes());
        let bytes = self.request(1, &payload, "")?;
        anyhow::ensure!(bytes.len() == size, "Incomplete reflection read");
        Ok(bytes)
    }

    fn read_many(&self, requests: &[(u64, usize)]) -> Result<Vec<Option<Vec<u8>>>> {
        let mut results = Vec::with_capacity(requests.len());
        let mut index = 0;
        while index < requests.len() {
            let begin = index;
            let mut size = 0;
            let mut payload = Vec::new();
            for (address, length) in &requests[begin..] {
                anyhow::ensure!(
                    *length > 0 && *length < 1024 * 1024,
                    "Invalid reflection batch length"
                );
                if index - begin == 4096 || size + length + 1 > 1024 * 1024 {
                    break;
                }
                payload.extend_from_slice(&address.to_le_bytes());
                payload.extend_from_slice(&(*length as u32).to_le_bytes());
                size += length + 1;
                index += 1;
            }
            let bytes = self.request(3, &payload, "")?;
            anyhow::ensure!(bytes.len() == size, "Incomplete reflection batch");
            let mut cursor = 0;
            for (_, length) in &requests[begin..index] {
                results.push(
                    (bytes[cursor] == 1).then(|| bytes[cursor + 1..cursor + 1 + length].to_vec()),
                );
                cursor += length + 1;
            }
        }
        Ok(results)
    }

    fn pointer(&self, address: u64) -> Result<u64> {
        read_u64(&self.read(address, 8)?, 0).context("Invalid reflection pointer")
    }

    fn cstring(&self, address: u64) -> Result<String> {
        let mut bytes = Vec::new();
        for offset in (0..512).step_by(64) {
            let block = self.read(address + offset, 64)?;
            if let Some(end) = block.iter().position(|b| *b == 0) {
                bytes.extend_from_slice(&block[..end]);
                return String::from_utf8(bytes).context("Non-UTF8 reflection name");
            }
            bytes.extend_from_slice(&block);
        }
        bail!("Reflection name exceeds 512 bytes")
    }

    fn rtti(&self, object: u64) -> Result<String> {
        let vtable = self.pointer(object)?;
        let info = self.pointer(vtable.checked_sub(8).context("Invalid vtable")?)?;
        self.cstring(self.pointer(info + 8)?)
    }

    fn string(&self, address: u64) -> Result<String> {
        let bytes = self.read(address, 24)?;
        let text = match string_source(&bytes).context("Invalid reflection string layout")? {
            StringSource::Inline(bytes) => bytes,
            StringSource::Indirect(address, size) => self.read(address, size)?,
        };
        name_text(text)
    }

    fn name(&self, address: u64) -> Result<String> {
        for offset in [0, 8] {
            if let Ok(name) = self.string(address + offset)
                && !name.is_empty()
            {
                return Ok(name);
            }
        }
        bail!("Reflection name is unavailable")
    }

    fn instance_classes(&self, instances: &[(u64, u64)], offset: u64) -> Result<Vec<String>> {
        let descriptors = self.read_many(
            &instances
                .iter()
                .map(|(p, _)| (p + offset, 8))
                .collect::<Vec<_>>(),
        )?;
        let descriptors = descriptors
            .into_iter()
            .map(|bytes| {
                let bytes = bytes.context("Studio hierarchy changed while reading classes")?;
                Ok((read_u64(&bytes, 0).unwrap(), 0))
            })
            .collect::<Result<Vec<_>>>()?;
        self.instance_names(&descriptors, 8)
    }

    fn instance_names(&self, instances: &[(u64, u64)], offset: u64) -> Result<Vec<String>> {
        let pointers = self.read_many(
            &instances
                .iter()
                .map(|(p, _)| (p + offset, 8))
                .collect::<Vec<_>>(),
        )?;
        let heads = pointers
            .into_iter()
            .map(|bytes| {
                let bytes = bytes.context("Studio hierarchy changed while reading names")?;
                Ok((read_u64(&bytes, 0).unwrap(), 32))
            })
            .collect::<Result<Vec<_>>>()?;
        let headers = self.read_many(&heads)?;
        let mut pending = Vec::new();
        let mut candidates = Vec::with_capacity(headers.len());
        for bytes in headers {
            let bytes = bytes.context("Studio hierarchy changed while reading names")?;
            let mut choices = Vec::new();
            for offset in [0, 8] {
                if let Some(source) = string_source(&bytes[offset..]) {
                    let choice = match source {
                        StringSource::Inline(bytes) => Ok(bytes),
                        StringSource::Indirect(address, size) => {
                            pending.push((address, size));
                            Err(pending.len() - 1)
                        }
                    };
                    choices.push(choice);
                }
            }
            candidates.push(choices);
        }
        let bodies = self.read_many(&pending)?;
        candidates
            .into_iter()
            .map(|choices| {
                let mut empty = false;
                for choice in choices {
                    let bytes = match choice {
                        Ok(bytes) => Some(bytes),
                        Err(index) => bodies[index].clone(),
                    };
                    if let Some(text) = bytes.and_then(|bytes| String::from_utf8(bytes).ok()) {
                        if !text.is_empty() {
                            return Ok(text);
                        }
                        empty = true;
                    }
                }
                if empty {
                    return Ok(String::new());
                }
                bail!("Studio instance names changed or use an unsupported layout; retry the query")
            })
            .collect()
    }

    fn children(&self, instance: u64, offset: u64) -> Result<Vec<(u64, u64)>> {
        let vector = self.read(self.pointer(instance + offset)?, 24)?;
        let begin = read_u64(&vector, 0).unwrap();
        let end = read_u64(&vector, 8).unwrap();
        let capacity = read_u64(&vector, 16).unwrap();
        let size = end
            .checked_sub(begin)
            .filter(|size| size.is_multiple_of(16) && *size <= 16 * 1_000_000 && end <= capacity)
            .context("Invalid or oversized reflection children vector")?
            as usize;
        let mut children = Vec::with_capacity(size / 16);
        for offset in (0..size).step_by(1024 * 1024) {
            let bytes = self.read(begin + offset as u64, (size - offset).min(1024 * 1024))?;
            children.extend(
                bytes
                    .as_chunks::<16>()
                    .0
                    .iter()
                    .map(|b| (read_u64(b, 0).unwrap(), read_u64(b, 8).unwrap())),
            );
        }
        Ok(children)
    }

    fn member(&self, instance: u64, class_offset: u64, wanted: &str) -> Result<u64> {
        let class_descriptor = self.pointer(instance + class_offset)?;
        let class = self.read(class_descriptor, 0x218)?;
        let key = (
            self.pid,
            self.trace.image_uuid,
            class_descriptor,
            wanted.to_owned(),
        );
        let cached = MEMBERS.get().and_then(|cache| {
            cache
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(&key)
                .cloned()
        });
        if let Some(entry) = cached
            && entry.header.as_ref() == class
            && self
                .pointer(entry.descriptor + 8)
                .and_then(|p| self.name(p))
                .as_deref()
                .ok()
                == Some(wanted)
        {
            return Ok(entry.descriptor);
        }
        let members = member_descriptors(&class, |requests| self.read_many(requests))?;
        let matches = members.get(wanted);
        anyhow::ensure!(
            matches.is_some_and(|values| values.len() == 1),
            "Reflection member {wanted} resolved {} candidates",
            matches.map_or(0, HashSet::len)
        );
        let descriptor = *matches.unwrap().iter().next().unwrap();
        let mut cache = MEMBERS
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if cache.len() + members.len() > 1024 {
            cache.clear();
        }
        let header: Arc<[u8]> = Arc::from(class);
        // The scan already resolved every name. Share its class header across
        // unique members instead of rescanning it for the next property/ID.
        for (name, matches) in members.into_iter().take(1024 - cache.len()) {
            if matches.len() == 1 {
                cache.insert(
                    (self.pid, self.trace.image_uuid, class_descriptor, name),
                    MemberEntry {
                        header: header.clone(),
                        descriptor: *matches.iter().next().unwrap(),
                    },
                );
            }
        }
        Ok(descriptor)
    }
}

pub(crate) struct NativeProperty {
    pub(crate) class_name: String,
    pub(crate) instance_id: String,
    property: String,
    title: String,
    memory: Memory,
    parameters: Vec<u8>,
    writable: bool,
}

impl NativeProperty {
    pub(crate) fn ensure_writable(&self) -> Result<()> {
        anyhow::ensure!(
            self.writable,
            "{}.{} has no supported native setter",
            self.class_name,
            self.property
        );
        Ok(())
    }
    pub(crate) fn remaining(&self) -> Result<Duration> {
        self.memory
            .deadline
            .checked_duration_since(Instant::now())
            .context("Protected property operation exceeded its deadline")
    }
    fn invoke(&mut self, operation: u32) -> Result<Vec<u8>> {
        let started = Instant::now();
        crate::app::output::log_global(
            5,
            format_args!(
                "[renium] native property invoke: pid={} operation={operation} {}.{} title={:?}",
                self.memory.pid, self.class_name, self.property, self.title,
            ),
        );
        let timeout = self.remaining()?.as_millis().clamp(1, 3000) as u32;
        put32(&mut self.parameters, 132, operation);
        put32(&mut self.parameters, 140, timeout);
        let output = self
            .memory
            .request(2, &self.parameters, &self.title)
            .with_context(|| {
                let action = match operation {
                    0 => "identify",
                    1 | 3 | 4 => "read",
                    _ => "write",
                };
                format!(
                    "Could not {action} {}.{} in {}",
                    self.class_name, self.property, self.title
                )
            })?;
        crate::app::output::log_global(
            5,
            format_args!(
                "[renium] native property invoke completed: {}.{} operation={operation} {:.1}ms",
                self.class_name,
                self.property,
                started.elapsed().as_secs_f64() * 1000.0,
            ),
        );
        anyhow::ensure!(output.len() >= 16, "Incomplete property identity response");
        Ok(output)
    }
    pub(crate) fn read(&mut self) -> Result<String> {
        String::from_utf8(self.invoke(1)?[16..].to_vec())
            .context("Property needs a binary value codec; it is not UTF-8 text")
    }
    pub(crate) fn write(&mut self, text: &str) -> Result<String> {
        self.ensure_writable()?;
        anyhow::ensure!(text.len() <= 65536, "Property value exceeds 64 KiB");
        put32(&mut self.parameters, 136, text.len() as u32);
        self.parameters[680..680 + text.len()].copy_from_slice(text.as_bytes());
        let mut actual = String::from_utf8(self.invoke(2)?[16..].to_vec())?;
        let mut pause = 0;
        while !crate::automation::property_access::property_text_matches(
            &self.class_name,
            &self.property,
            text,
            &actual,
        ) {
            if self.remaining()? <= Duration::from_millis(pause) {
                bail!(
                    "Studio accepted the write but readback did not complete; inspect its value before retrying because it may still finish"
                );
            }
            if pause != 0 {
                std::thread::sleep(Duration::from_millis(pause));
            }
            actual = self.read()?;
            pause = (pause * 2).clamp(1, 8);
        }
        Ok(actual)
    }
}

fn put32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}
fn put64(bytes: &mut [u8], offset: usize, value: u64) {
    bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

fn resolve_path(
    memory: &Memory,
    context: &[u8],
    segments: &[String],
    ordinals: &[usize],
) -> Result<Vec<(u64, u64)>> {
    if segments.is_empty()
        || segments.len() > 64
        || segments.iter().any(String::is_empty)
        || (!ordinals.is_empty() && ordinals.len() != segments.len())
        || ordinals.contains(&0)
    {
        bail!("Property path needs 1–64 nonempty segments and matching positive ordinals");
    }
    let children_offset = read_u64(context, 24).unwrap();
    let name_offset = read_u64(context, 32).unwrap();
    let mut parent = read_u64(context, 8).unwrap();
    let mut ancestors = Vec::new();
    for (index, name) in segments.iter().enumerate() {
        let children = memory.children(parent, children_offset)?;
        // Service paths use class identity throughout the bridge. Their mutable
        // display names must not break native access or select a namesake folder.
        let service_root = index == 0
            && rbx_reflection_database::get()?
                .classes
                .get(name.as_str())
                .is_some_and(|class| class.tags.contains(&rbx_reflection::ClassTag::Service));
        let names = if service_root {
            memory.instance_classes(&children, read_u64(context, 40).unwrap())?
        } else {
            memory.instance_names(&children, name_offset)?
        };
        let matches = children
            .into_iter()
            .zip(names)
            .filter_map(|(entry, child_name)| (child_name == *name).then_some(entry))
            .collect::<Vec<_>>();
        let entry = match ordinals.get(index) {
            Some(ordinal) => matches.get(ordinal - 1).copied(),
            None if matches.len() == 1 => Some(matches[0]),
            _ => None,
        }
        .with_context(|| {
            format!(
                "Property segment '{name}' has {} matches; use --ords for duplicates",
                matches.len()
            )
        })?;
        parent = entry.0;
        ancestors.push(entry);
    }
    Ok(ancestors)
}

fn parent_offset(memory: &Memory, context: &[u8]) -> Result<u64> {
    let model = read_u64(context, 8).unwrap();
    let samples = memory
        .children(model, read_u64(context, 24).unwrap())?
        .into_iter()
        .take(3)
        .map(|(p, _)| memory.read(p, 0x208))
        .collect::<Result<Vec<_>>>()?;
    let offsets = (0..=0x200)
        .step_by(8)
        .filter(|offset| {
            samples.len() == 3 && samples.iter().all(|b| read_u64(b, *offset) == Some(model))
        })
        .collect::<Vec<_>>();
    anyhow::ensure!(
        offsets.len() == 1,
        "Studio parent layout is not uniquely validated; Renium's detector needs updating for this layout"
    );
    Ok(offsets[0] as u64)
}

fn discover_identity_abi(memory: &Memory, descriptor: u64, table: u64) -> Result<ValidatedAbi> {
    let offsets = memory.bindings(descriptor)?;
    let mut found = HashMap::new();
    for slot in 12..24 {
        let address = memory.pointer(table + slot * 8)?;
        if let Ok(code) = memory.code(address, 256)
            && let Some(result) = binding_call(&code, &offsets, CallKind::Identity)
        {
            found.entry(result).or_insert_with(Vec::new).push((
                slot as usize * 8,
                address - memory.base,
                code,
            ));
        }
    }
    anyhow::ensure!(
        found.len() == 1,
        "Studio identity ABI has {} candidates; Renium's detector needs updating for this layout",
        found.len()
    );
    let ((field, getter_slot), functions) = found.into_iter().next().unwrap();
    Ok(ValidatedAbi {
        field,
        getter_slot,
        setter_slot: 0,
        functions,
    })
}

fn identity_function(
    memory: &Memory,
    instance: u64,
    class_offset: u64,
) -> Result<(u64, u64, usize)> {
    let descriptor = memory.member(instance, class_offset, "UniqueId")?;
    anyhow::ensure!(
        memory.rtti(descriptor)?
            == "N3RBX10Reflection14PropDescriptorINS_8InstanceENS_8UniqueIdEEE",
        "Unsupported Studio identity descriptor"
    );
    let table = memory.pointer(descriptor)?;
    let abi = match memory.cached_abi(table, CallKind::Identity) {
        Some(abi) => abi,
        None => {
            let abi = discover_identity_abi(memory, descriptor, table)?;
            memory.cache_abi(table, CallKind::Identity, abi.clone())?;
            abi
        }
    };
    let binding = memory.pointer(descriptor + abi.field as u64)?;
    anyhow::ensure!(
        memory.rtti(binding)?.contains("UniqueIdEE10GetSetImplI"),
        "Studio identity binding has an unsupported type"
    );
    let getter = memory.pointer(memory.pointer(binding)? + abi.getter_slot as u64)?;
    memory.code(getter, 128)?;
    Ok((binding, getter, abi.getter_slot))
}

pub(crate) fn prepare_context(pid: u32, title: &str) -> Result<()> {
    let memory = Memory::for_process(pid, Duration::from_secs(2))?;
    let context = memory.request(0, &[0], title)?;
    anyhow::ensure!(context.len() == 64, "Unsupported Studio reflection context");
    let mut request = Vec::with_capacity(20);
    request.extend_from_slice(&context[56..64]);
    request.extend_from_slice(&context[8..16]);
    let remaining = memory.deadline.saturating_duration_since(Instant::now());
    request.extend_from_slice(&(remaining.as_millis().clamp(1, 3000) as u32).to_le_bytes());
    let response = memory.request(4, &request, title)?;
    anyhow::ensure!(
        response.is_empty(),
        "Unexpected Studio preparation response"
    );
    Ok(())
}

pub(crate) fn prepare_property(
    pid: u32,
    title: &str,
    segments: &[String],
    ordinals: &[usize],
    property: &str,
    timeout: Duration,
) -> Result<NativeProperty> {
    prepare(pid, title, segments, ordinals, property, timeout, None).map(|(prepared, _)| prepared)
}

/// A trusted snapshot read captures identity and value on the same engine task.
/// It does not create a write grant or change the ordinary approval flow.
pub(crate) fn read_property(
    pid: u32,
    title: &str,
    segments: &[String],
    ordinals: &[usize],
    class: &str,
    property: &str,
    timeout: Duration,
) -> Result<String> {
    prepare(
        pid,
        title,
        segments,
        ordinals,
        property,
        timeout,
        Some(class),
    )
    .map(|(_, value)| value)
}

fn prepare(
    pid: u32,
    title: &str,
    segments: &[String],
    ordinals: &[usize],
    property: &str,
    timeout: Duration,
    read_class: Option<&str>,
) -> Result<(NativeProperty, String)> {
    let _trace = crate::app::timing::trace_scope("native.property", "prepare reflected property");
    let mut prepared = discover_property(
        pid, title, segments, ordinals, property, timeout, read_class,
    )?;
    let phase = crate::app::timing::trace_scope(
        "native.property",
        "invoke identity-checked property operation",
    );
    let response = prepared.invoke(if read_class.is_some() { 3 } else { 0 })?;
    drop(phase);
    let identity = response
        .get(..16)
        .context("Studio returned no instance identity")?;
    anyhow::ensure!(identity != [0; 16], "Studio target has no stable identity");
    prepared.instance_id = identity.iter().map(|b| format!("{b:02x}")).collect();
    prepared.parameters[664..680].copy_from_slice(identity);
    Ok((prepared, String::from_utf8(response[16..].to_vec())?))
}

fn discover_property(
    pid: u32,
    title: &str,
    segments: &[String],
    ordinals: &[usize],
    property: &str,
    timeout: Duration,
    read_class: Option<&str>,
) -> Result<NativeProperty> {
    let _trace = crate::app::timing::trace_scope("native.property", "discover reflected property");
    crate::app::output::log_global(
        5,
        format_args!(
            "[renium] native property prepare: pid={pid} path={segments:?} property={property} title={title:?}",
        ),
    );
    let started = Instant::now();
    let phase = crate::app::timing::trace_scope("native.property", "open process");
    let mut memory = Memory::for_process(pid, timeout)?;
    drop(phase);
    let traced = Instant::now();
    let phase = crate::app::timing::trace_scope("native.property", "locate DataModel context");
    let context = memory.request(0, &[0], title)?;
    anyhow::ensure!(
        context.len() == 64,
        "Unsupported reflection context; install the matching helper"
    );
    memory.base = read_u64(&context, 0).unwrap();
    drop(phase);
    let located = Instant::now();
    let phase =
        crate::app::timing::trace_scope("native.property", "resolve instance path and class");
    let ancestors = resolve_path(&memory, &context, segments, ordinals)?;
    let (instance, owner) = *ancestors.last().unwrap();
    let class_offset = read_u64(&context, 40).unwrap();
    let class_descriptor = memory.pointer(instance + class_offset)?;
    let class_name = memory.name(memory.pointer(class_descriptor + 8)?)?;
    anyhow::ensure!(
        read_class.is_none_or(|expected| class_name == expected),
        "Native snapshot target changed class"
    );
    let resolved = Instant::now();
    drop(phase);
    let phase = crate::app::timing::trace_scope("native.property", "resolve property descriptor");
    let descriptor = memory.member(instance, class_offset, property)?;
    drop(phase);
    let member = Instant::now();
    let phase = crate::app::timing::trace_scope(
        "native.property",
        "resolve property codec and identity getter",
    );
    let descriptor_kind = memory.rtti(descriptor)?;
    anyhow::ensure!(
        descriptor_kind.contains("PropDescriptor"),
        "Unsupported protected-property descriptor {descriptor_kind}"
    );
    let (_, getter_slot, setter_slot) = memory.text_functions(descriptor)?;
    let table = memory.pointer(descriptor)?;
    let getter = memory.pointer(table + getter_slot as u64)?;
    let setter = if setter_slot == 0 {
        0
    } else {
        memory.pointer(table + setter_slot as u64)?
    };
    let (identity_binding, identity_getter, identity_slot) =
        identity_function(&memory, instance, class_offset)?;
    let codecs = Instant::now();
    drop(phase);
    let phase = crate::app::timing::trace_scope("native.property", "prepare ABI parameters");
    let mut parameters = vec![0; 66216];
    for (offset, value) in [
        (0, read_u64(&context, 56).unwrap()),
        (8, instance),
        (16, owner),
        (24, descriptor),
        (32, table),
        (40, class_descriptor),
        (48, getter),
        (56, setter),
        (64, identity_binding),
        (72, identity_getter),
        (80, class_offset),
        (88, read_u64(&context, 48).unwrap()),
        (96, parent_offset(&memory, &context)?),
        (104, getter_slot as u64),
        (112, setter_slot as u64),
        (120, identity_slot as u64),
    ] {
        put64(&mut parameters, offset, value);
    }
    put32(&mut parameters, 128, ancestors.len() as u32 + 1);
    for (index, (ptr, _)) in ancestors.iter().rev().enumerate() {
        put64(&mut parameters, 144 + index * 8, *ptr);
    }
    put64(
        &mut parameters,
        144 + ancestors.len() * 8,
        read_u64(&context, 8).unwrap(),
    );
    let prepared = NativeProperty {
        class_name,
        instance_id: String::new(),
        property: property.into(),
        title: title.into(),
        memory,
        parameters,
        writable: setter != 0,
    };
    drop(phase);
    crate::app::output::log_global(
        5,
        format_args!(
            "[renium] native property discovery phases: {}.{} trace_ms={:.3} context_ms={:.3} path_ms={:.3} member_ms={:.3} codec_ms={:.3} params_ms={:.3}",
            prepared.class_name,
            property,
            traced.duration_since(started).as_secs_f64() * 1000.0,
            located.duration_since(traced).as_secs_f64() * 1000.0,
            resolved.duration_since(located).as_secs_f64() * 1000.0,
            member.duration_since(resolved).as_secs_f64() * 1000.0,
            codecs.duration_since(member).as_secs_f64() * 1000.0,
            codecs.elapsed().as_secs_f64() * 1000.0,
        ),
    );
    Ok(prepared)
}

#[test]
#[ignore = "Runs protected reads/writes only against the owned Mac property fixture"]
fn protected_property_live_fixture() -> Result<()> {
    let pid = std::env::var("RENIUM_INSPECT_FIXTURE_PID")?.parse()?;
    let title = "ReniumPropertyTest.rbxl";
    let path = ["Workspace".into(), "ReniumAccessFixture".into()];
    let start = Instant::now();
    let mut property = prepare_property(
        pid,
        title,
        &path,
        &[],
        "CollisionFidelity",
        Duration::from_secs(3),
    )?;
    let initial = property.read()?;
    println!(
        "CollisionFidelity initial={initial} prepare/read ms={}",
        start.elapsed().as_millis()
    );
    let write = Instant::now();
    assert_eq!(property.write("Hull")?, "Hull");
    println!(
        "verified CollisionFidelity write ms={}",
        write.elapsed().as_micros() as f64 / 1000.
    );
    assert_eq!(property.write(&initial)?, initial);
    let mut reads = Vec::new();
    let mut writes = Vec::new();
    for _ in 0..20 {
        let read_start = Instant::now();
        let mut repeat = prepare_property(
            pid,
            title,
            &path,
            &[],
            "CollisionFidelity",
            Duration::from_secs(3),
        )?;
        assert_eq!(repeat.read()?, initial);
        reads.push(read_start.elapsed().as_micros());
        let write_start = Instant::now();
        assert_eq!(repeat.write("Hull")?, "Hull");
        writes.push(write_start.elapsed().as_micros());
        assert_eq!(repeat.write(&initial)?, initial);
    }
    reads.sort_unstable();
    writes.sort_unstable();
    println!(
        "20 cycles, native read us min/median/max={}/{}/{}, write={}/{}/{}",
        reads[0], reads[10], reads[19], writes[0], writes[10], writes[19]
    );
    let mut text = prepare_property(
        pid,
        title,
        &["Workspace".into(), "ReniumAccessText".into()],
        &[],
        "Value",
        Duration::from_secs(3),
    )?;
    assert_eq!(text.read()?, "fidelity-".repeat(1024));
    Ok(())
}

#[test]
#[ignore = "Read-only descriptor/code inspection in the owned Terrain regression fixture"]
fn terrain_descriptor_live_fixture() -> Result<()> {
    let pid = std::env::var("RENIUM_INSPECT_FIXTURE_PID")?.parse()?;
    let mut memory = Memory::for_process(pid, Duration::from_secs(10))?;
    let context = memory.request(0, &[0], "ReniumPropertyPackageTest.rbxl")?;
    anyhow::ensure!(context.len() == 64, "Unexpected reflection context");
    memory.base = read_u64(&context, 0).unwrap();
    let path =
        std::env::var("RENIUM_INSPECT_FIXTURE_PATH").unwrap_or_else(|_| "Workspace.Terrain".into());
    let ancestors = resolve_path(
        &memory,
        &context,
        &path.split('.').map(str::to_owned).collect::<Vec<_>>(),
        &[],
    )?;
    let instance = ancestors.last().context("Missing Terrain")?.0;
    let class_offset = read_u64(&context, 40).unwrap();
    let mut rows = Vec::new();
    let names = std::env::var("RENIUM_INSPECT_FIXTURE_PROPERTIES")
        .unwrap_or_else(|_| "SmoothGrid,PhysicsGrid,CopyRegion,PasteRegion".into());
    if names == "*" {
        let class = memory.read(memory.pointer(instance + class_offset)?, 0x218)?;
        let mut members = member_descriptors(&class, |requests| memory.read_many(requests))?
            .into_keys()
            .collect::<Vec<_>>();
        members.sort();
        fs::write(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../audit/release-readiness/terrain-member-names.json"),
            serde_json::to_vec_pretty(&members)?,
        )?;
        return Ok(());
    }
    for name in names.split(',') {
        let descriptor = memory.member(instance, class_offset, name)?;
        let table = memory.pointer(descriptor)?;
        let slots = memory.read(table, 32 * 8)?;
        let mut functions = Vec::new();
        for slot in 0..32 {
            let function = read_u64(&slots, slot * 8).unwrap();
            if let Ok(code) = memory.code(function, 256) {
                functions.push(serde_json::json!({"slot":slot*8,"rva":function-memory.base,"code":base64::encode(code)}));
            }
        }
        let mut bindings = Vec::new();
        for offset in memory.bindings(descriptor)? {
            let binding = memory.pointer(descriptor + offset as u64)?;
            let vtable = memory.pointer(binding)?;
            let slots = memory.read(vtable, 8 * 8)?;
            let mut methods = Vec::new();
            for slot in 0..8 {
                let function = read_u64(&slots, slot * 8).unwrap();
                if let Ok(code) = memory.code(function, 512) {
                    methods.push(serde_json::json!({"slot":slot*8,"rva":function-memory.base,"code":base64::encode(code)}));
                }
            }
            let fields = memory.read(binding, 48)?;
            let mut targets = Vec::new();
            for offset in [8, 24] {
                let function = read_u64(&fields, offset).unwrap();
                if let Ok(code) = memory.code(function, 2048) {
                    targets.push(serde_json::json!({"offset":offset,"rva":function-memory.base,"code":base64::encode(code)}));
                }
            }
            bindings.push(
                serde_json::json!({"offset":offset,"kind":memory.rtti(binding)?,"methods":methods,"fields":base64::encode(fields),"targets":targets}),
            );
        }
        let text_abi = memory
            .text_functions(descriptor)
            .map_err(|error| error.to_string());
        rows.push(serde_json::json!({"name":name,"base":memory.base,"kind":memory.rtti(descriptor)?,"textAbi":text_abi,"fields":base64::encode(memory.read(descriptor,256)?),"functions":functions,"bindings":bindings}));
    }
    let output = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../audit/release-readiness/terrain-descriptors.json");
    fs::write(output, serde_json::to_vec_pretty(&rows)?)?;
    Ok(())
}

#[cfg(test)]
fn terrain_fixture_binary_property(pid: u32, name: &str) -> Result<NativeProperty> {
    let mut prepared = prepare_property(
        pid,
        "ReniumPropertyPackageTest.rbxl",
        &["Workspace".into(), "Terrain".into()],
        &[],
        "Name",
        Duration::from_secs(3),
    )?;
    anyhow::ensure!(
        prepared.class_name == "Terrain",
        "Not the owned Terrain target"
    );
    let instance = read_u64(&prepared.parameters, 8).unwrap();
    let class_offset = read_u64(&prepared.parameters, 80).unwrap();
    let descriptor = prepared.memory.member(instance, class_offset, name)?;
    anyhow::ensure!(
        prepared.memory.rtti(descriptor)?
            == "N3RBX10Reflection14PropDescriptorINS_19MegaClusterInstanceENS_12BinaryStringEEE",
        "Not a Terrain BinaryString descriptor"
    );
    // Read-only probe of the inspected 0.738 ARM64 ABI, not a production resolver.
    // Its copy method loads this binding, returns 24-byte string storage via x8,
    // passes that storage to the setter, and disposes it as libc++ std::string.
    let binding = prepared.memory.pointer(descriptor + 144)?;
    anyhow::ensure!(
        prepared.memory.rtti(binding)?
            == "N3RBX10Reflection14PropDescriptorINS_19MegaClusterInstanceENS_12BinaryStringEE10GetSetImplIMNS_11TerrainPropEKFS3_vEMS6_FvS3_EEE",
        "Not the inspected Terrain getter binding"
    );
    let table = prepared.memory.pointer(binding)?;
    let getter = prepared.memory.pointer(table + 32)?;
    let code = prepared.memory.code(getter, 36)?;
    anyhow::ensure!(
        read_u32(&code, 0) == Some(0x91070029) && read_u32(&code, 32) == Some(0xd61f0020),
        "The inspected Terrain getter changed"
    );
    for (offset, value) in [
        (24, binding),
        (32, table),
        (48, getter),
        (56, 0),
        (104, 32),
        (112, 0),
    ] {
        put64(&mut prepared.parameters, offset, value);
    }
    prepared.property = name.into();
    prepared.writable = false;
    Ok(prepared)
}

#[test]
#[ignore = "Opt-in history hook qualification in the owned Mac Terrain fixture"]
fn terrain_history_hook_live_fixture() -> Result<()> {
    anyhow::ensure!(
        std::env::var("RENIUM_TERRAIN_WRITE_PROBE").as_deref() == Ok("1"),
        "Explicit write probe opt-in required"
    );
    let pid = std::env::var("RENIUM_INSPECT_FIXTURE_PID")?.parse()?;
    let token = std::env::var("RENIUM_HISTORY_PROBE_TOKEN")?;
    register_history(pid, "ReniumPropertyPackageTest.rbxl", &token)
}

#[test]
#[ignore = "Read-only native Terrain listener interface discovery in the owned Mac fixture"]
fn terrain_native_listener_live_fixture() -> Result<()> {
    fn describe(memory: &Memory, info: u64, depth: usize) -> Result<serde_json::Value> {
        anyhow::ensure!(depth < 20, "Unexpected native inheritance depth");
        let name = memory.cstring(memory.pointer(info + 8)?)?;
        let kind = memory.rtti(info)?;
        let mut bases = Vec::new();
        if kind.contains("__si_class_type_info") {
            bases.push(serde_json::json!({"offsetFlags":0,"type":describe(memory, memory.pointer(info + 16)?, depth + 1)?}));
        } else if kind.contains("__vmi_class_type_info") {
            let header = memory.read(info + 16, 8)?;
            let count = read_u32(&header, 4).unwrap() as usize;
            anyhow::ensure!(count <= 32, "Unexpected native base count");
            let entries = memory.read(info + 24, count * 16)?;
            for entry in entries.chunks_exact(16) {
                bases.push(serde_json::json!({"offsetFlags":read_u64(entry,8).unwrap() as i64,"type":describe(memory,read_u64(entry,0).unwrap(),depth+1)?}));
            }
        }
        Ok(serde_json::json!({"name":name,"kind":kind,"bases":bases}))
    }
    let pid = std::env::var("RENIUM_INSPECT_FIXTURE_PID")?.parse()?;
    let property = terrain_fixture_binary_property(pid, "SmoothGrid")?;
    let instance = read_u64(&property.parameters, 8).unwrap();
    let vtable = property.memory.pointer(instance)?;
    let hierarchy = describe(&property.memory, property.memory.pointer(vtable - 8)?, 0)?;
    fs::write(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../audit/release-readiness/terrain-native-inheritance.json"),
        serde_json::to_vec_pretty(&hierarchy)?,
    )?;
    // The observed non-virtual GridListener base is at +512 in this fixture's
    // build. This is diagnostic data, not a production offset or hook.
    let listener_table = property.memory.pointer(instance + 512)?;
    anyhow::ensure!(
        property.memory.pointer(listener_table - 16)? as i64 == -512,
        "The inspected Terrain GridListener adjustment changed"
    );
    let mut methods = Vec::new();
    for slot in (0..128).step_by(8) {
        let method = property.memory.pointer(listener_table + slot)?;
        let Ok(code) = property.memory.code(method, 1024) else {
            break;
        };
        let mut targets = Vec::new();
        for at in [0, 4] {
            let word = read_u32(&code, at).unwrap();
            if word & 0xfc000000 == 0x14000000 {
                let target = (method as i64 + at as i64 + ((word << 6) as i32 >> 4) as i64) as u64;
                targets.push(serde_json::json!({"rva":target-property.memory.base,"code":base64::encode(property.memory.code(target,4096)?)}));
            }
        }
        methods.push(serde_json::json!({"slot":slot,"rva":method-property.memory.base,"code":base64::encode(code),"targets":targets}));
    }
    fs::write(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../audit/release-readiness/terrain-native-listener-methods.json"),
        serde_json::to_vec_pretty(&methods)?,
    )?;
    // The fully captured SmoothGrid getter in this fixture loads +0x240 and
    // dispatches serialization through +0xf0. Read its actual target only;
    // these offsets are diagnostic evidence, never a production resolver.
    let grid = property.memory.pointer(instance + 0x240)?;
    let grid_table = property.memory.pointer(grid)?;
    let serialize = property.memory.pointer(grid_table + 0xf0)?;
    fs::write(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../audit/release-readiness/terrain-grid-serialize-target.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "kind": property.memory.rtti(grid)?,
            "rva": serialize - property.memory.base,
            "code": base64::encode(property.memory.code(serialize, 256)?),
        }))?,
    )?;
    Ok(())
}

#[test]
#[ignore = "Explicit opt-in conditional Terrain binary write in the owned Mac fixture"]
fn terrain_binary_setter_live_fixture() -> Result<()> {
    anyhow::ensure!(
        std::env::var("RENIUM_TERRAIN_WRITE_PROBE").as_deref() == Ok("1"),
        "Explicit Terrain write probe opt-in required"
    );
    let pid = std::env::var("RENIUM_INSPECT_FIXTURE_PID")?.parse()?;
    let mut property = terrain_fixture_binary_property(pid, "SmoothGrid")?;
    let table = read_u64(&property.parameters, 32).unwrap();
    let setter = property.memory.pointer(table + 40)?;
    let code = property.memory.code(setter, 132)?;
    anyhow::ensure!(
        read_u32(&code, 0) == Some(0xd10103ff)
            && read_u32(&code, 96) == Some(0xd63f0280)
            && read_u32(&code, 128) == Some(0xd65f03c0),
        "The inspected Terrain BinaryString setter changed"
    );
    put64(&mut property.parameters, 56, setter);
    put64(&mut property.parameters, 112, 40);
    let before = property.invoke(1)?[16..].to_vec();
    let audit = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../audit/release-readiness");
    let source: serde_json::Value =
        serde_json::from_slice(&fs::read(audit.join("terrain-native-rock-values.json"))?)?;
    let desired = base64::decode(
        source["SmoothGrid"]["base64"]
            .as_str()
            .context("Missing owned Rock grid")?,
    )?;
    anyhow::ensure!(
        before != desired && !before.is_empty(),
        "Terrain probe needs a distinct initial grid"
    );
    let input_size = 4 + before.len() + desired.len();
    anyhow::ensure!(
        input_size <= 65536,
        "Terrain diagnostic payload exceeds its fixed transport"
    );
    put32(&mut property.parameters, 136, input_size as u32);
    put32(&mut property.parameters, 680, before.len() as u32);
    property.parameters[684..684 + before.len()].copy_from_slice(&before);
    property.parameters[684 + before.len()..680 + input_size].copy_from_slice(&desired);
    property.parameters[684] ^= 1;
    let error = property
        .invoke(5)
        .expect_err("Changed Terrain must reject the stale write");
    anyhow::ensure!(
        format!("{error:#}").contains("changed before the conditional write"),
        "Unexpected rejection: {error:#}"
    );
    assert_eq!(&property.invoke(1)?[16..], before);
    property.parameters[684] ^= 1;
    let after = property.invoke(5)?;
    assert_eq!(&after[16..], desired);
    fs::write(
        audit.join("terrain-conditional-setter.json"),
        serde_json::to_vec_pretty(
            &serde_json::json!({"staleRejected":true,"before":base64::encode(before),"after":base64::encode(&after[16..])}),
        )?,
    )?;
    Ok(())
}

#[test]
#[ignore = "Read-only native BinaryString ABI probe in the owned Mac Terrain fixture"]
fn terrain_binary_getter_live_fixture() -> Result<()> {
    let pid = std::env::var("RENIUM_INSPECT_FIXTURE_PID")?.parse()?;
    let mut values = serde_json::Map::new();
    for name in ["SmoothGrid", "PhysicsGrid", "MaterialColors"] {
        let mut prepared = terrain_fixture_binary_property(pid, name)?;
        let bytes = prepared.invoke(1)?;
        let payload = &bytes[16..];
        values.insert(
            name.into(),
            serde_json::json!({"bytes":payload.len(),"base64":base64::encode(payload)}),
        );
    }
    let output = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../audit/release-readiness/terrain-native-binary-values.json");
    fs::write(output, serde_json::to_vec_pretty(&values)?)?;
    println!("Terrain BinaryString getter probe completed");
    Ok(())
}

#[test]
fn arm_vector_setter_preserves_stack_identity_across_zero_initialization() {
    // Actual Vector3 text setter: parse into a zero-initialized stack value,
    // pass that value to the binding, then return the parser's success flag.
    let words: [u32; 26] = [
        0xd10103ff, 0xa90157f6, 0xa9024ff4, 0xa9037bfd, 0x9100c3fd, 0xaa0103f3, 0xaa0003f5,
        0xf90003ff, 0xb9000bff, 0x910003e1, 0xaa0203e0, 0x95234daa, 0xaa0003f4, 0x340000e0,
        0xf9404aa0, 0xf9400008, 0xf9401508, 0x910003e2, 0xaa1303e1, 0xd63f0100, 0xaa1403e0,
        0xa9437bfd, 0xa9424ff4, 0xa94157f6, 0x910103ff, 0xd65f03c0,
    ];
    let decode = |words: &[u32], field| {
        binding_call(
            &words
                .iter()
                .flat_map(|word| word.to_le_bytes())
                .collect::<Vec<_>>(),
            &[field],
            CallKind::Setter,
        )
    };
    for field in [128, 144, 256] {
        let mut relocated = words;
        relocated[14] = 0xf94002a0 | ((field as u32 / 8) << 10);
        assert_eq!(decode(&relocated, field), Some((field, 40)));
    }
    for (index, replacement) in [
        (8, 0x110023ff),  // ADD WSP actually writes the stack pointer.
        (10, 0xaa0303e0), // Parser receives a different input.
        (17, 0xaa1403e2), // Setter receives the status, not parsed storage.
        (20, 0xaa1603e0), // Function does not return the parse result.
    ] {
        let mut invalid = words;
        invalid[index] = replacement;
        assert_eq!(decode(&invalid, 144), None, "invalid setter at {index}");
    }
}

#[test]
fn arm_enum_setter_preserves_stack_identity_across_flag_only_compares() {
    // Verified LightingStyle text-setter instructions. The enum parser writes
    // stack storage before the binding setter consumes it.
    let words: [u32; 30] = [
        0xd10103ff, 0xa90157f6, 0xa9024ff4, 0xa9037bfd, 0x9100c3fd, 0xaa0103f3, 0xaa0003f4,
        0xf9405400, 0x39c05c48, 0xf9400049, 0x7100011f, // cmp w8,#0 (Rd=31 means WZR, not SP)
        0x9a82b121, 0x910023e2, 0x94a8cd4d, 0xaa0003f5, 0x34000120, 0xb9400be8, 0xb9000fe8,
        0xf9404e80, 0xf9400008, 0xf9401508, 0x910033e2, 0xaa1303e1, 0xd63f0100, 0xaa1503e0,
        0xa9437bfd, 0xa9424ff4, 0xa94157f6, 0x910103ff, 0xd65f03c0,
    ];
    let decode = |words: &[u32], field| {
        binding_call(
            &words
                .iter()
                .flat_map(|word| word.to_le_bytes())
                .collect::<Vec<_>>(),
            &[field],
            CallKind::Setter,
        )
    };
    for field in [128, 152, 256] {
        let mut relocated = words;
        relocated[18] = 0xf9400280 | ((field as u32 / 8) << 10);
        assert_eq!(decode(&relocated, field), Some((field, 40)));
    }
    for (index, replacement) in [
        (10, 0x110003ff), // untracked add to WSP must still invalidate SP
        (11, 0x9a80b121), // one string arm no longer carries the input
        (24, 0xaa1703e0), // returned value is not the parse result
    ] {
        let mut invalid = words;
        invalid[index] = replacement;
        assert_eq!(decode(&invalid, 152), None);
    }
}

#[test]
fn arm_text_getter_accepts_only_a_complete_returned_string_copy() {
    let words: [u32; 15] = [
        0xd10103ff, // sub sp,sp,#64
        0xaa0803f3, // mov x19,x8 (original result)
        0xf9404800, // ldr x0,[x0,#144] (descriptor binding)
        0xf9400008, // ldr x8,[x0]
        0xf9401109, // ldr x9,[x8,#32]
        0x910003f4, // mov x20,sp
        0x910003e8, // mov x8,sp (temporary aggregate result)
        0xd63f0120, // blr x9
        0x3cc08280, // ldur q0,[x20,#8] (ContentId string)
        0x3d800260, // str q0,[x19]
        0xf8418288, // ldur x8,[x20,#24]
        0xf9000a68, // str x8,[x19,#16]
        0x14000002, // b common epilogue
        0x910103ff, // add sp,sp,#64
        0xd65f03c0,
    ];
    let decode = |words: &[u32]| {
        binding_call(
            &words
                .iter()
                .flat_map(|word| word.to_le_bytes())
                .collect::<Vec<_>>(),
            &[144],
            CallKind::Text,
        )
    };
    assert_eq!(decode(&words), Some((144, 32)));
    for (index, replacement) in [
        (1, 0xaa0703f3),  // wrong output
        (5, 0x910023f4),  // copy from a different stack address
        (9, 0x3d800280),  // write into the temporary, not caller output
        (10, 0xf8420288), // noncontiguous source bytes
        (11, 0xf9000e68), // incomplete caller output
    ] {
        let mut invalid = words;
        invalid[index] = replacement;
        assert_eq!(decode(&invalid), None, "invalid copy at {index}");
    }
}

#[test]
fn batched_members_preserve_name_layouts_unreadable_entries_and_ambiguity() {
    for vector_offset in [0x40, 0x120, 0x200] {
        let mut class = vec![0; 0x218];
        put64(&mut class, 0x10, 0x90000);
        put64(&mut class, 0x18, u64::MAX);
        put64(&mut class, 0x20, u64::MAX);
        let mut memory: HashMap<(u64, usize), Vec<u8>> = HashMap::new();
        let mut entries = Vec::new();
        for index in 0..200_u64 {
            let descriptor = 0x20000 + index * 0x100;
            let pointer = 0x40000 + index * 0x100;
            entries.extend_from_slice(&descriptor.to_le_bytes());
            entries.extend_from_slice(&0_u64.to_le_bytes());
            if index == 198 {
                continue;
            } // Removed/unreadable descriptor.
            let pointer = if index == 199 { u64::MAX } else { pointer };
            memory.insert((descriptor + 8, 8), pointer.to_le_bytes().to_vec());
            if index == 199 {
                continue;
            } // Invalid address cannot poison the batch.
            let name = if index < 2 {
                "Ambiguous".to_owned()
            } else if index == 197 {
                "Invalid\0Name".to_owned()
            } else {
                format!("Property{index}")
            };
            let mut header = vec![0; 24];
            if index.is_multiple_of(3) {
                let body = 0x80000 + index * 0x100;
                header[..8].copy_from_slice(&body.to_le_bytes());
                header[8..16].copy_from_slice(&(name.len() as u64).to_le_bytes());
                header[23] = 0x80;
                memory.insert((body, name.len()), name.into_bytes());
            } else {
                header[..name.len()].copy_from_slice(name.as_bytes());
                header[23] = name.len() as u8;
            }
            if index.is_multiple_of(2) {
                // A valid 24-byte first layout with no readable second layout.
                memory.insert((pointer, 24), header);
            } else {
                memory.insert((pointer, 24), vec![0xff; 24]);
                memory.insert((pointer + 8, 24), header);
            }
        }
        // Duplicate occurrences of the same descriptor are not ambiguity.
        entries.extend_from_slice(&0x20200_u64.to_le_bytes());
        entries.extend_from_slice(&0_u64.to_le_bytes());
        put64(&mut class, vector_offset, 0x10000);
        put64(&mut class, vector_offset + 8, 201);
        put64(&mut class, vector_offset + 16, 201);
        memory.insert((0x10000, entries.len()), entries);
        let mut batches = 0;
        let members = member_descriptors(&class, |requests| {
            batches += 1;
            assert!(
                requests
                    .iter()
                    .all(|(p, len)| *p >= 0x10000 && p.checked_add(*len as u64).is_some())
            );
            Ok(requests
                .iter()
                .map(|request| memory.get(request).cloned())
                .collect())
        })
        .unwrap();
        assert_eq!(
            batches, 4,
            "Discovery must batch by stage, not by property count"
        );
        assert_eq!(members.len(), 196);
        assert_eq!(members["Ambiguous"].len(), 2);
        assert_eq!(members["Property2"], HashSet::from([0x20200]));
        assert!(!members.contains_key("Property198"));
        assert!(!members.contains_key("Invalid\0Name"));
    }
}

#[test]
fn arm_reflection_discovery_tracks_relocated_fields_slots_and_registers() {
    let mov = |dst: u32, source: u32| 0xaa0003e0 | source << 16 | dst;
    let ldr = |dst: u32, base: u32, offset: u32| 0xf9400000 | (offset / 8) << 10 | base << 5 | dst;
    for (field, slot, scratch) in [(0x90, 0x20, 9), (0xb0, 0x38, 14), (0x68, 0x48, 16)] {
        let words = [
            mov(19, 8),
            ldr(0, 0, field),
            ldr(scratch, 0, 0),
            ldr(scratch, scratch, slot),
            0xd63f0000 | scratch << 5,
            mov(8, 19),
            0x14000008,
        ];
        let code = words
            .iter()
            .flat_map(|w| w.to_le_bytes())
            .collect::<Vec<_>>();
        assert_eq!(
            binding_call(&code, &[field as usize], CallKind::Text),
            Some((field as usize, slot as usize))
        );
        assert_eq!(binding_call(&code, &[], CallKind::Text), None);
        assert_eq!(
            binding_call(&code[..code.len() - 4], &[field as usize], CallKind::Text),
            None
        );
        let mut wrong = words;
        wrong[1] = ldr(0, 1, field); // Field from instance is not a descriptor binding.
        let code = wrong
            .iter()
            .flat_map(|w| w.to_le_bytes())
            .collect::<Vec<_>>();
        assert_eq!(binding_call(&code, &[field as usize], CallKind::Text), None);
        wrong = words;
        wrong[5] = mov(8, 20); // Result is not returned through the original output argument.
        let code = wrong
            .iter()
            .flat_map(|w| w.to_le_bytes())
            .collect::<Vec<_>>();
        assert_eq!(binding_call(&code, &[field as usize], CallKind::Text), None);
    }
}
