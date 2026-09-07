//! macOS reflection discovery lives in the Rust host. The in-process helper
//! performs bounded memory copies and owns only the engine's C++ ABI calls.
use super::*;
use std::collections::HashSet;
use std::io::{Seek, SeekFrom};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Argument {
    Descriptor,
    DescriptorField,
    Instance,
    Output,
    Input,
    Parsed,
    Scratch,
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
        && (kind != CallKind::Setter || registers[2] == Some(Scratch)))
    .then_some((field, slot))
}

fn parses_input(registers: &[Option<Argument>; 32]) -> bool {
    use Argument::*;
    registers[..2] == [Some(Input), Some(Scratch)]
        || registers[..3] == [Some(DescriptorField), Some(Input), Some(Scratch)]
}

// ARM64 instructions are fixed-width. Track arguments through register moves
// and field loads; relocated fields, vtable slots and scratch registers are not
// signatures. A call also needs its expected result/argument use to validate.
fn binding_call(code: &[u8], offsets: &[usize], kind: CallKind) -> Option<(usize, usize)> {
    use Argument::*;
    let mut registers = [None; 32];
    registers[0] = Some(Descriptor);
    registers[1] = Some(Instance);
    if kind == CallKind::Text {
        registers[8] = Some(Output);
    } else if kind == CallKind::Setter {
        registers[2] = Some(Input);
    }
    let mut found = None;
    let mut result_verified = false;
    for bytes in code.as_chunks::<4>().0 {
        let word = read_u32(bytes, 0)?;
        let dst = (word & 31) as usize;
        let base = ((word >> 5) & 31) as usize;
        if word == 0xd65f03c0 {
            // ret
            return (result_verified || kind == CallKind::Setter && registers[0] == Some(Parsed))
                .then_some(found)
                .flatten();
        } else if word & 0xffe0ffe0 == 0xaa0003e0 {
            // mov Xd, Xn
            registers[dst] = registers[((word >> 16) & 31) as usize];
        } else if word & 0xffc00000 == 0xf9400000 {
            // ldr Xd, [Xn,#unsigned]
            let offset = ((word >> 10) & 4095) as usize * 8;
            registers[dst] = loaded_argument(registers[base], offset, offsets);
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
            registers[dst] = if base == 31 || base == 29 {
                Some(Scratch)
            } else if registers[base] == Some(Instance) {
                Some(Instance)
            } else {
                None
            };
        } else if word & 0xfffffc1f == 0xd63f0000 {
            // blr Xn
            let call = indirect_binding(&registers, base, kind);
            // A second different binding call cannot prove this ABI.
            if found.is_some() && call.is_some() && found != call {
                return None;
            }
            found = call.or(found);
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

struct Memory {
    pid: u32,
    trace: PackageActionTrace,
    deadline: Instant,
    base: u64,
    executable: PathBuf,
}

#[derive(Clone)]
struct MemberEntry {
    header: Vec<u8>,
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

impl Memory {
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
        request.extend_from_slice(&(remaining.as_millis() as u64).to_le_bytes());
        request.extend_from_slice(&self.trace.submit_rva.to_le_bytes());
        request.extend_from_slice(&self.trace.image_uuid);
        request.extend_from_slice(payload);
        request.extend_from_slice(title.as_bytes());
        stream.write_all(&request)?;
        let mut response = [0; RESPONSE_SIZE];
        stream.read_exact(&mut response)?;
        if read_u32(&response, 0) != Some(REQUEST_MAGIC) || read_u32(&response, 4) != Some(0) {
            let error = &response[24..];
            let end = error.iter().position(|b| *b == 0).unwrap_or(error.len());
            bail!(
                "Studio reflection transport: {}",
                String::from_utf8_lossy(&error[..end])
            );
        }
        let length = read_u64(&response, 8)
            .filter(|length| *length <= 1024 * 1024)
            .context("Invalid reflection response length")? as usize;
        let mut data = vec![0; length];
        stream.read_exact(&mut data)?;
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
            && entry.header == class
            && self
                .pointer(entry.descriptor + 8)
                .and_then(|p| self.name(p))
                .as_deref()
                .ok()
                == Some(wanted)
        {
            return Ok(entry.descriptor);
        }
        let mut matches = HashSet::new();
        for offset in (0..=0x200).step_by(8) {
            let entries = read_u64(&class, offset).unwrap();
            let count = read_u64(&class, offset + 8).unwrap();
            let capacity = read_u64(&class, offset + 16).unwrap();
            if entries < 0x10000 || count == 0 || count > 512 || capacity < count || capacity > 1024
            {
                continue;
            }
            let Ok(bytes) = self.read(entries, count as usize * 16) else {
                continue;
            };
            for row in bytes.as_chunks::<16>().0 {
                let descriptor = read_u64(row, 0).unwrap();
                let name = self.pointer(descriptor + 8).and_then(|p| self.name(p));
                if name.as_deref().ok() == Some(wanted) {
                    matches.insert(descriptor);
                }
            }
        }
        anyhow::ensure!(
            matches.len() == 1,
            "Reflection member {wanted} resolved {} candidates",
            matches.len()
        );
        let descriptor = *matches.iter().next().unwrap();
        let mut cache = MEMBERS
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if cache.len() >= 1024 {
            cache.clear();
        }
        cache.insert(
            key,
            MemberEntry {
                header: class,
                descriptor,
            },
        );
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
            "This engine property has no supported setter"
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
        let timeout = self.remaining()?.as_millis().clamp(1, 3000) as u32;
        put32(&mut self.parameters, 132, operation);
        put32(&mut self.parameters, 140, timeout);
        let output = self.memory.request(2, &self.parameters, &self.title)?;
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
        let names = memory.instance_names(&children, name_offset)?;
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

pub(crate) fn prepare_property(
    pid: u32,
    title: &str,
    segments: &[String],
    ordinals: &[usize],
    property: &str,
    timeout: Duration,
) -> Result<NativeProperty> {
    let deadline = Instant::now() + timeout;
    let executable = process_executable_path(pid)?;
    let mut memory = Memory {
        pid,
        trace: trace_package_action(&executable)?,
        deadline,
        base: 0,
        executable,
    };
    let context = memory.request(0, &[0], title)?;
    anyhow::ensure!(
        context.len() == 64,
        "Unsupported reflection context; install the matching helper"
    );
    memory.base = read_u64(&context, 0).unwrap();
    let ancestors = resolve_path(&memory, &context, segments, ordinals)?;
    let (instance, owner) = *ancestors.last().unwrap();
    let class_offset = read_u64(&context, 40).unwrap();
    let class_descriptor = memory.pointer(instance + class_offset)?;
    let class_name = memory.name(memory.pointer(class_descriptor + 8)?)?;
    let descriptor = memory.member(instance, class_offset, property)?;
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
    let mut prepared = NativeProperty {
        class_name,
        instance_id: String::new(),
        property: property.into(),
        title: title.into(),
        memory,
        parameters,
        writable: setter != 0,
    };
    let identity = prepared.invoke(0)?;
    anyhow::ensure!(
        identity.len() == 16 && identity != [0; 16],
        "Studio target has no stable identity"
    );
    prepared.instance_id = identity.iter().map(|b| format!("{b:02x}")).collect();
    prepared.parameters[664..680].copy_from_slice(&identity);
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
