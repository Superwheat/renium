//! Native binary-loader discovery. Only bounded executable-file code is scanned;
//! the boolean-to-flags adapter and its argument flow must agree with the known ABI.
use super::*;
use iced_x86::{Decoder, DecoderOptions, FlowControl, Mnemonic, OpKind, Register};

#[path = "windows_loader_factory.rs"]
mod factory;
#[path = "windows_loader_history.rs"]
mod history;
pub(super) use history::HistoryTrace;

pub(super) fn discover_factory(
    image: &PeImage<'_>,
    bytes: &[u8],
    reader: usize,
) -> Result<FactoryTrace> {
    factory::discover(image, bytes, reader)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct FactoryTrace {
    pub lookup: usize,
    pub origin: usize,
    pub intern_name: usize,
    pub instance_reader: usize,
    pub context_bytes: usize,
}

struct CachedLoader {
    len: u64,
    modified: Option<SystemTime>,
    result: std::result::Result<PreparedLoader, String>,
}
static LOADERS: OnceLock<Mutex<HashMap<PathBuf, CachedLoader>>> = OnceLock::new();

pub(super) fn prepare(path: &Path) -> Result<LoaderTrace> {
    Ok(prepared(path)?.trace)
}

#[derive(Clone)]
struct PreparedLoader {
    trace: LoaderTrace,
    history: HistoryTrace,
    history_table: std::sync::Arc<Vec<usize>>,
    code: std::sync::Arc<Vec<(usize, Vec<u8>)>>,
}

pub(super) fn verify_loaded(
    memory: &ProcessMemory,
    studio: &ModuleEntry,
) -> Result<(LoaderTrace, HistoryTrace)> {
    let prepared = prepared(&studio.path)?;
    for (rva, expected) in prepared.code.iter() {
        anyhow::ensure!(
            memory.read_vec(studio.base + rva, expected.len())? == *expected,
            "Loaded native reader contract changed"
        );
    }
    let table = memory.read_vec(
        studio.base + prepared.history.table - 8,
        prepared.history_table.len() * 8,
    )?;
    anyhow::ensure!(
        table
            .chunks_exact(8)
            .zip(prepared.history_table.iter())
            .all(|(value, rva)| {
                u64::from_le_bytes(value.try_into().unwrap()) as usize == studio.base + rva
            }),
        "Loaded Studio history table changed"
    );
    Ok((prepared.trace, prepared.history))
}

fn prepared(path: &Path) -> Result<PreparedLoader> {
    let metadata = fs::metadata(path)?;
    let modified = metadata.modified().ok();
    let cache = LOADERS.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(cached) = cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(path)
        .filter(|entry| entry.len == metadata.len() && entry.modified == modified)
    {
        return cached.result.clone().map_err(anyhow::Error::msg);
    }
    let result = fs::read(path)
        .context("Cannot read Studio binary-loader image")
        .and_then(|bytes| {
            let trace = trace_loader(&bytes)?;
            let image = PeImage::parse(&bytes)?;
            let (history, history_code) = history::discover(&image)?;
            let table = image.rva_to_offset(history.table - 8)?;
            let history_table = (0..=history.slots)
                .map(|index| {
                    (read_u64(&bytes, table + index * 8)? as usize)
                        .checked_sub(image.image_base)
                        .context("Studio history table points outside its image")
                })
                .collect::<Result<Vec<_>>>()?;
            let mut code = Vec::new();
            for rva in [
                trace.loader,
                trace.reader,
                trace.factory.lookup,
                trace.factory.intern_name,
                trace.factory.instance_reader,
            ]
            .into_iter()
            .chain(history_code)
            {
                if code.iter().any(|(address, _)| *address == rva) {
                    continue;
                }
                let offset = image.rva_to_offset(rva)?;
                let (begin, end) = image.function_bounds(offset)?;
                anyhow::ensure!(
                    begin == offset,
                    "Native reader contract is not a complete function"
                );
                code.push((rva, bytes[begin..end].to_vec()));
            }
            Ok(PreparedLoader {
                trace,
                history,
                history_table: std::sync::Arc::new(history_table),
                code: std::sync::Arc::new(code),
            })
        })
        .map_err(|error| format!("{error:#}"));
    cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(
            path.to_owned(),
            CachedLoader {
                len: metadata.len(),
                modified,
                result: result.clone(),
            },
        );
    result.map_err(anyhow::Error::msg)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Input {
    Context,
    Stream,
    DataModel,
    Boolean,
    InvertedBoolean,
    Flags,
    Instance(usize),
    Stack(i64),
    Extra5,
    Extra6,
    Zero,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct LoaderTrace {
    pub loader: usize,
    pub reader: usize,
    pub instance_offset: usize,
    pub factory: FactoryTrace,
}

// Volatile argument registers cannot carry provenance across an engine call.
fn clear_volatile(registers: &mut HashMap<Register, Input>) {
    for register in [
        Register::RAX,
        Register::RCX,
        Register::RDX,
        Register::R8,
        Register::R9,
        Register::R10,
        Register::R11,
    ] {
        registers.remove(&register);
    }
}

fn loader_calls(bytes: &[u8], rva: usize) -> Vec<(usize, usize)> {
    use Input::*;
    let mut registers = HashMap::from([
        (Register::RCX, Context),
        (Register::RDX, Stream),
        (Register::R8, DataModel),
        (Register::R9, Boolean),
    ]);
    let mut result = Vec::new();
    let mut stack_zero = false;
    for i in Decoder::with_ip(64, bytes, rva as u64, DecoderOptions::NONE) {
        if i.is_invalid() {
            return Vec::new();
        }
        let destination = i.op0_register().full_register();
        match i.mnemonic() {
            Mnemonic::Call => {
                if i.op0_kind() == OpKind::NearBranch64
                    && stack_zero
                    && registers.get(&Register::RCX) == Some(&Context)
                    && registers.get(&Register::RDX) == Some(&Stream)
                    && registers.get(&Register::R9) == Some(&Flags)
                    && let Some(Instance(offset)) = registers.get(&Register::R8)
                {
                    result.push((i.near_branch_target() as usize, *offset));
                }
                clear_volatile(&mut registers);
                stack_zero = false;
            }
            Mnemonic::Mov | Mnemonic::Movzx | Mnemonic::Lea if i.op0_kind() == OpKind::Register => {
                let value = if i.op1_kind() == OpKind::Register {
                    registers.get(&i.op1_register().full_register()).copied()
                } else if i.mnemonic() == Mnemonic::Lea
                    && i.op1_kind() == OpKind::Memory
                    && i.memory_index() == Register::None
                    && registers.get(&i.memory_base().full_register()) == Some(&DataModel)
                    && i.memory_displacement64() <= 0x800
                    && i.memory_displacement64() % 8 == 0
                {
                    Some(Instance(i.memory_displacement64() as usize))
                } else {
                    None
                };
                registers.remove(&destination);
                if let Some(value) = value {
                    registers.insert(destination, value);
                }
            }
            Mnemonic::Xor if i.op0_kind() == OpKind::Register => {
                let prior = registers.remove(&destination);
                if i.op1_kind() == OpKind::Register
                    && destination == i.op1_register().full_register()
                {
                    registers.insert(destination, Zero);
                } else if prior == Some(Boolean)
                    && i.op1_kind() == OpKind::Immediate8to32
                    && i.immediate8to32() == 1
                {
                    registers.insert(destination, InvertedBoolean);
                }
            }
            Mnemonic::Shl
                if i.op0_kind() == OpKind::Register
                    && registers.get(&destination) == Some(&InvertedBoolean)
                    && i.op1_kind() == OpKind::Immediate8
                    && i.immediate8() == 2 =>
            {
                registers.insert(destination, Flags);
            }
            // The adapter passes null only when its original model was null.
            Mnemonic::Cmove
                if i.op0_kind() == OpKind::Register
                    && matches!(registers.get(&destination), Some(Instance(_)))
                    && registers.get(&i.op1_register().full_register()) == Some(&Zero) => {}
            Mnemonic::Mov
                if i.op0_kind() == OpKind::Memory
                    && i.memory_base() == Register::RSP
                    && i.memory_index() == Register::None
                    && i.memory_displacement64() == 0x20 =>
            {
                stack_zero = i.op1_kind() == OpKind::Register
                    && registers.get(&i.op1_register().full_register()) == Some(&Zero);
            }
            _ if i.op0_kind() == OpKind::Register
                && !matches!(
                    i.mnemonic(),
                    Mnemonic::Cmp | Mnemonic::Test | Mnemonic::Push
                ) =>
            {
                registers.remove(&destination);
            }
            _ => {}
        }
        // No provenance flows through an unconditional jump into another block.
        if matches!(
            i.flow_control(),
            FlowControl::UnconditionalBranch | FlowControl::Return
        ) {
            registers.clear();
            stack_zero = false;
        }
    }
    result
}

// The outer loader supplies a result object and twelve arguments to the binary
// reader. Trace those relationships, not the first CALL or a build-specific RVA.
fn reader_call(bytes: &[u8], rva: usize) -> Option<usize> {
    use Input::*;
    let mut registers = HashMap::from([
        (Register::RCX, Context),
        (Register::RDX, Stream),
        (Register::R8, Instance(0)),
        (Register::R9, Flags),
        (Register::RSP, Stack(0)),
    ]);
    let mut stack = HashMap::from([(0x28, (Extra5, 8)), (0x30, (Extra6, 8))]);
    let address = |i: &iced_x86::Instruction, registers: &HashMap<Register, Input>| {
        if i.memory_index() != Register::None {
            return None;
        }
        let Stack(base) = registers.get(&i.memory_base().full_register())? else {
            return None;
        };
        base.checked_add(i.memory_displacement64() as i64)
            .filter(|offset| (-4096..=4096).contains(offset))
    };
    let mut decoder = Decoder::with_ip(64, bytes, rva as u64, DecoderOptions::NONE);
    for _ in 0..64 {
        let i = decoder.decode();
        let destination = i.op0_register().full_register();
        match i.mnemonic() {
            Mnemonic::Mov | Mnemonic::Lea if i.op0_kind() == OpKind::Register => {
                let value = match i.op1_kind() {
                    OpKind::Register if i.mnemonic() == Mnemonic::Mov => {
                        *registers.get(&i.op1_register().full_register())?
                    }
                    OpKind::Memory => {
                        let offset = address(&i, &registers)?;
                        if i.mnemonic() == Mnemonic::Lea {
                            Stack(offset)
                        } else {
                            let (value, size) = stack.get(&offset)?;
                            if *size != i.memory_size().size() {
                                return None;
                            }
                            *value
                        }
                    }
                    _ => return None,
                };
                if i.op0_register().size() != 8 && !matches!(value, Flags | Zero) {
                    return None;
                }
                registers.insert(destination, value);
            }
            Mnemonic::Mov if i.op0_kind() == OpKind::Memory => {
                let offset = address(&i, &registers)?;
                let value = *registers.get(&i.op1_register().full_register())?;
                let size = i.memory_size().size();
                // Partial overwrites invalidate previous stack provenance.
                stack.retain(|at, (_, prior_size)| {
                    *at + *prior_size as i64 <= offset || *at >= offset + size as i64
                });
                stack.insert(offset, (value, size));
            }
            Mnemonic::Xor
                if i.op0_kind() == OpKind::Register
                    && i.op1_kind() == OpKind::Register
                    && i.op0_register() == i.op1_register() =>
            {
                registers.insert(destination, Zero);
            }
            Mnemonic::Sub
                if i.op0_register() == Register::RSP && i.op1_kind() == OpKind::Immediate32to64 =>
            {
                let Stack(offset) = *registers.get(&Register::RSP)? else {
                    return None;
                };
                let next = offset.checked_sub(i.immediate32to64())?;
                if !(-4096..0).contains(&next) {
                    return None;
                }
                registers.insert(Register::RSP, Stack(next));
            }
            Mnemonic::Call if i.op0_kind() == OpKind::NearBranch64 => {
                let Stack(sp) = *registers.get(&Register::RSP)? else {
                    return None;
                };
                let Stack(result) = *registers.get(&Register::RCX)? else {
                    return None;
                };
                let (Stack(limits), 8) = *stack.get(&(sp + 0x50))? else {
                    return None;
                };
                if registers.get(&Register::RDX) != Some(&Context)
                    || registers.get(&Register::R8) != Some(&Stream)
                    || registers.get(&Register::R9) != Some(&Instance(0))
                    || stack.get(&limits) != Some(&(Zero, 8))
                    || stack.get(&(limits + 8)) != Some(&(Zero, 8))
                    || result < sp + 0x60
                    || result + 48 > 0
                    || limits < sp + 0x60
                    || limits + 16 > 0
                    || (result < limits + 16 && limits < result + 48)
                    || ![
                        (0x20, Zero, 8),
                        (0x28, Zero, 8),
                        (0x30, Flags, 4),
                        (0x38, Extra5, 8),
                        (0x40, Zero, 8),
                        (0x48, Zero, 4),
                        (0x58, Extra6, 8),
                    ]
                    .iter()
                    .all(|(at, value, size)| stack.get(&(sp + at)) == Some(&(*value, *size)))
                {
                    return None;
                }
                // Confirm the caller tests the result's error-discriminant byte.
                let mut check = decoder.decode();
                if check.mnemonic() == Mnemonic::Nop {
                    check = decoder.decode();
                }
                return (check.mnemonic() == Mnemonic::Cmp
                    && check.op0_kind() == OpKind::Memory
                    && check.memory_size().size() == 1
                    && address(&check, &registers) == Some(result + 40)
                    && check.op1_kind() == OpKind::Immediate8
                    && check.immediate8() == 0
                    && decoder.decode().mnemonic() == Mnemonic::Je)
                    .then_some(i.near_branch_target() as usize);
            }
            Mnemonic::Nop => {}
            _ => return None,
        }
    }
    None
}

pub(super) fn trace_loader(bytes: &[u8]) -> Result<LoaderTrace> {
    let image = PeImage::parse(bytes)?;
    let text = image.section(b".text")?;
    let strings = image.section(b".rdata")?;
    let needle = b"loadContent re-entrant\0";
    let string_rvas = memmem::find_iter(
        &bytes[strings.raw_offset..strings.raw_offset + strings.raw_size],
        needle,
    )
    .map(|offset| strings.virtual_address + offset)
    .collect::<HashSet<_>>();
    anyhow::ensure!(
        !string_rvas.is_empty(),
        "Studio binary-loader anchor changed; update Renium's finder"
    );
    let mut anchors = HashSet::new();
    let text_bytes = &bytes[text.raw_offset..text.raw_offset + text.raw_size];
    for at in memchr::memchr2_iter(0x48, 0x4c, text_bytes) {
        let Some(code) = text_bytes.get(at..at + 7) else {
            continue;
        };
        if code[1] != 0x8d || code[2] & 0xc7 != 5 {
            continue;
        }
        let i = Decoder::with_ip(
            64,
            code,
            (text.virtual_address + at) as u64,
            DecoderOptions::NONE,
        )
        .decode();
        if i.mnemonic() == Mnemonic::Lea
            && i.is_ip_rel_memory_operand()
            && string_rvas.contains(&(i.ip_rel_memory_address() as usize))
        {
            anchors.insert(image.function_bounds(text.raw_offset + at)?.0);
        }
    }
    anyhow::ensure!(
        !anchors.is_empty() && anchors.len() <= 8,
        "Studio binary-loader anchor has {} callers; update Renium's finder",
        anchors.len()
    );
    let mut visited = HashSet::new();
    let mut layer = anchors;
    let mut matches = HashSet::new();
    for _ in 0..3 {
        let mut next = HashSet::new();
        for start in layer {
            if !visited.insert(start) {
                continue;
            }
            anyhow::ensure!(
                visited.len() <= 256,
                "Studio binary-loader call graph exceeded its bounded search"
            );
            let Ok((begin, end)) = image.function_bounds(start) else {
                continue;
            };
            if start != begin || end - begin > 0x2000 {
                continue;
            }
            let rva = image.offset_to_rva(begin)?;
            let code = &bytes[begin..end];
            for candidate in loader_calls(code, rva) {
                image.require_executable_rva(candidate.0)?;
                let target = image.rva_to_offset(candidate.0)?;
                anyhow::ensure!(
                    image.function_bounds(target)?.0 == target,
                    "Studio binary loader does not begin at a function boundary"
                );
                matches.insert(candidate);
            }
            for i in Decoder::with_ip(64, code, rva as u64, DecoderOptions::NONE) {
                if i.mnemonic() == Mnemonic::Call
                    && i.op0_kind() == OpKind::NearBranch64
                    && let Ok(offset) = image.rva_to_offset(i.near_branch_target() as usize)
                {
                    next.insert(offset);
                }
            }
        }
        layer = next;
    }
    anyhow::ensure!(
        matches.len() == 1,
        "Studio binary-loader ABI matched {} functions; update Renium's finder",
        matches.len()
    );
    let (loader, instance_offset) = matches.into_iter().next().unwrap();
    let offset = image.rva_to_offset(loader)?;
    let (_, end) = image.function_bounds(offset)?;
    let reader = reader_call(&bytes[offset..end.min(offset + 512)], loader)
        .context("Studio binary-reader argument layout changed; update Renium's finder")?;
    image.require_executable_rva(reader)?;
    let reader_offset = image.rva_to_offset(reader)?;
    anyhow::ensure!(
        image.function_bounds(reader_offset)?.0 == reader_offset,
        "Studio binary reader does not begin at a function boundary"
    );
    Ok(LoaderTrace {
        loader,
        reader,
        instance_offset,
        factory: discover_factory(&image, bytes, reader)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reader_requires_complete_stack_and_result_provenance() -> Result<()> {
        let original = "4C8BDC4881ECB80000004533D24D8953A84D8953B0488B8424E8000000498943A0498D43A849894398458953904D895388488B8424E00000004989438044894C24304C895424284C895424204D8BC84C8BC2488BD1498D4BC8E842D0FFFF9080BC24A8000000007421";
        let code = hex(original)?;
        assert_eq!(reader_call(&code, 0x10000), Some(0xd0a0));
        // Compiler-local result placement is not an ABI requirement.
        let relocated = hex(&original
            .replace("498D4BC8", "498D4BC0")
            .replace("80BC24A800000000", "80BC24A000000000"))?;
        assert_eq!(reader_call(&relocated, 0x10000), Some(0xd0a0));
        for (from, to) in [
            ("4D8953A8", "4D895BA8"),                 // nonzero limits
            ("44894C2430", "4489442430"),             // target instead of flags
            ("4C89542420", "4C895C2420"),             // nonnull root output
            ("488BD1", "488BD0"),                     // wrong context
            ("4C8BC2", "4C8BC1"),                     // wrong stream
            ("4D8BC8", "4D8BC9"),                     // wrong target
            ("488B8424E0000000", "488B8424E8000000"), // wrong extra argument
            ("80BC24A800000000", "80BC24A000000000"), // wrong result field
            ("007421", "007521"),                     // reversed success branch
        ] {
            // The register mutations must affect an actual instruction.
            assert!(original.contains(from));
            assert!(
                reader_call(&hex(&original.replace(from, to))?, 0x10000).is_none(),
                "accepted {from}"
            );
        }
        for end in [0, 20, 88, code.len() - 1] {
            assert!(reader_call(&code[..end], 0x10000).is_none());
        }
        Ok(())
    }

    #[test]
    fn loader_requires_flags_and_all_argument_relationships() -> Result<()> {
        // Preserve the four arguments, transform (!bool)<<2, adjust model to
        // Instance, and pass a null fifth argument. Offsets/registers are data.
        let code = hex(
            "410FB6D9488BE983F301488BCAC1E302498BF0488BFAE8000000004C8D86F001000033C948894C2420448BCB488BD74C0F44C1488BCDE800000000C3",
        )?;
        let found = loader_calls(&code, 0x1000);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].1, 0x1f0);
        for (offset, replacement) in [(9, 2), (15, 1), (30, 0xf1), (46, 0xee)] {
            let mut changed = code.clone();
            changed[offset] = replacement;
            assert!(
                loader_calls(&changed, 0x1000).is_empty(),
                "accepted mutation {offset}"
            );
        }
        Ok(())
    }

    #[test]
    #[ignore = "Read-only loader finder measurement; requires an explicit Studio executable"]
    fn find_installed_loader_without_opening_studio() -> Result<()> {
        let path = PathBuf::from(
            std::env::var_os("RENIUM_LOADER_PROBE_EXE").context("Missing Studio image")?,
        );
        for sample in 0..3 {
            let started = Instant::now();
            let trace = prepare(&path)?;
            println!(
                "sample={sample} ms={:.3} rva={:#x} reader={:#x} instance_offset={:#x}",
                started.elapsed().as_secs_f64() * 1000.,
                trace.loader,
                trace.reader,
                trace.instance_offset
            );
        }
        Ok(())
    }
}
