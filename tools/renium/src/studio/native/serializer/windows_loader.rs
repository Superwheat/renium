//! Native binary-loader discovery. Only bounded executable-file code is scanned;
//! the boolean-to-flags adapter and its argument flow must agree with the known ABI.
use super::*;
use crate::system::LockRecover;
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
    metadata: history::Evidence,
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
    for &(rva, target) in &prepared.metadata.pointers {
        anyhow::ensure!(
            memory.read_u64(studio.base + rva)? as usize == studio.base + target,
            "Loaded Studio reflection metadata changed; native import is unavailable"
        );
    }
    for &rva in &prepared.metadata.names {
        anyhow::ensure!(
            memory.read_vec(studio.base + rva, 19)? == b"LuaSourceContainer\0",
            "Loaded Studio reflection class name changed; native import is unavailable"
        );
    }
    Ok((prepared.trace, prepared.history))
}

fn prepared(path: &Path) -> Result<PreparedLoader> {
    let metadata = fs::metadata(path)?;
    let modified = metadata.modified().ok();
    let cache = LOADERS.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(cached) = cache
        .lock_recover()
        .get(path)
        .filter(|entry| entry.len == metadata.len() && entry.modified == modified)
    {
        return cached.result.clone().map_err(anyhow::Error::msg);
    }
    // Sharing/mapping failures are transient, not an unsupported reader ABI.
    let mapped = ImageView::open(path).context("Cannot read Studio binary-loader image")?;
    let result = (|| -> Result<PreparedLoader> {
        let bytes = mapped.bytes();
        let trace = trace_loader(bytes)?;
        let image = PeImage::parse(bytes)?;
        let discovered = history::discover(&image)?;
        let history = discovered.trace;
        let table = image.rva_to_offset(history.table - 8)?;
        let history_table = (0..=history.slots)
            .map(|index| {
                (read_u64(bytes, table + index * 8)? as usize)
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
        .chain(discovered.code)
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
            metadata: discovered.metadata,
            code: std::sync::Arc::new(code),
        })
    })()
    .map_err(|error| format!("{error:#}"));
    cache.lock_recover().insert(
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
    ExtraFlags,
    ExtraFlagsWithBit,
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
        (Register::RSP, Stack(0)),
    ]);
    let mut result = Vec::new();
    let mut stack_zero = false;
    let mut boolean_test = false;
    let mut extra_flags_valid = true;
    for i in Decoder::with_ip(64, bytes, rva as u64, DecoderOptions::NONE) {
        if i.is_invalid() {
            return Vec::new();
        }
        let destination = i.op0_register().full_register();
        let tested_boolean = boolean_test;
        if i.rflags_modified() != 0 {
            boolean_test = false;
        }
        match i.mnemonic() {
            Mnemonic::Call => {
                if let Some(candidate) = loader_candidate(&i, &registers, stack_zero) {
                    result.push(candidate);
                }
                clear_volatile(&mut registers);
                stack_zero = false;
                boolean_test = false;
                extra_flags_valid = false;
            }
            Mnemonic::Mov | Mnemonic::Movzx | Mnemonic::Lea if i.op0_kind() == OpKind::Register => {
                let value = loader_register_move(&i, &registers, extra_flags_valid);
                registers.remove(&destination);
                if let Some(value) = value {
                    registers.insert(destination, value);
                }
            }
            Mnemonic::Xor if i.op0_kind() == OpKind::Register => {
                let prior = registers.remove(&destination);
                if let Some(value) = loader_xor(&i, prior) {
                    registers.insert(destination, value);
                }
            }
            Mnemonic::Shl | Mnemonic::Or | Mnemonic::Cmovne => {
                let value = loader_flag(&i, &registers, tested_boolean, extra_flags_valid);
                registers.remove(&destination);
                if let Some(value) = value {
                    registers.insert(destination, value);
                }
            }
            Mnemonic::Test
                if i.op0_kind() == OpKind::Register
                    && i.op1_kind() == OpKind::Register
                    && i.op0_register() == i.op1_register()
                    && registers.get(&destination) == Some(&Boolean) =>
            {
                boolean_test = true;
            }
            Mnemonic::Push if i.op0_kind() == OpKind::Register => {
                loader_move_stack(&mut registers, -8);
            }
            Mnemonic::Sub
                if i.op0_register() == Register::RSP
                    && matches!(
                        i.op1_kind(),
                        OpKind::Immediate32to64 | OpKind::Immediate8to64
                    ) =>
            {
                loader_move_stack(&mut registers, -(i.immediate(1) as i64));
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
                    && i.memory_size().size() == 8
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
        if i.op0_kind() == OpKind::Memory
            && !matches!(i.mnemonic(), Mnemonic::Cmp | Mnemonic::Test)
            && let Some(at) = loader_stack_address(&i, &registers)
            && at < 0x34
            && at + i.memory_size().size() as i64 > 0x30
        {
            extra_flags_valid = false;
        }
        if i.flow_control() == FlowControl::ConditionalBranch {
            boolean_test = false;
            registers.retain(|_, value| !matches!(value, ExtraFlags | ExtraFlagsWithBit));
        }
        // No provenance flows through an unconditional jump into another block.
        if matches!(
            i.flow_control(),
            FlowControl::UnconditionalBranch | FlowControl::Return
        ) {
            registers.clear();
            stack_zero = false;
            boolean_test = false;
            extra_flags_valid = false;
        }
    }
    result
}

fn loader_flag(
    i: &iced_x86::Instruction,
    registers: &HashMap<Register, Input>,
    tested_boolean: bool,
    extra_flags_valid: bool,
) -> Option<Input> {
    use Input::*;
    let prior = registers.get(&i.op0_register().full_register());
    match i.mnemonic() {
        Mnemonic::Shl
            if i.op0_kind() == OpKind::Register
                && prior == Some(&InvertedBoolean)
                && i.op1_kind() == OpKind::Immediate8
                && i.immediate8() == 2 =>
        {
            Some(Flags)
        }
        // base | (boolean ? 0 : 4), with the base from the sixth ABI argument.
        Mnemonic::Or
            if i.op0_register().size() == 4
                && prior == Some(&ExtraFlags)
                && i.op1_kind() == OpKind::Immediate8to32
                && i.immediate8to32() == 4 =>
        {
            Some(ExtraFlagsWithBit)
        }
        Mnemonic::Cmovne
            if tested_boolean
                && extra_flags_valid
                && i.op0_register().size() == 4
                && prior == Some(&ExtraFlagsWithBit)
                && loader_extra_flags(i, registers) == Some(ExtraFlags) =>
        {
            Some(Flags)
        }
        _ => None,
    }
}

fn loader_candidate(
    i: &iced_x86::Instruction,
    registers: &HashMap<Register, Input>,
    stack_zero: bool,
) -> Option<(usize, usize)> {
    let Input::Instance(offset) = registers.get(&Register::R8)? else {
        return None;
    };
    (i.op0_kind() == OpKind::NearBranch64
        && stack_zero
        && registers.get(&Register::RCX) == Some(&Input::Context)
        && registers.get(&Register::RDX) == Some(&Input::Stream)
        && registers.get(&Register::R9) == Some(&Input::Flags))
    .then_some((i.near_branch_target() as usize, *offset))
}

fn loader_xor(i: &iced_x86::Instruction, prior: Option<Input>) -> Option<Input> {
    if i.op1_kind() == OpKind::Register
        && i.op0_register() == i.op1_register()
        && i.op0_register().size() >= 4
    {
        Some(Input::Zero)
    } else if prior == Some(Input::Boolean)
        && i.op1_kind() == OpKind::Immediate8to32
        && i.immediate8to32() == 1
    {
        Some(Input::InvertedBoolean)
    } else {
        None
    }
}

fn loader_register_move(
    i: &iced_x86::Instruction,
    registers: &HashMap<Register, Input>,
    extra_flags_valid: bool,
) -> Option<Input> {
    if i.op1_kind() == OpKind::Register {
        let value = *registers.get(&i.op1_register().full_register())?;
        let pointer = matches!(
            value,
            Input::Context
                | Input::Stream
                | Input::DataModel
                | Input::Instance(_)
                | Input::Stack(_)
        );
        (!pointer || i.op0_register().size() == 8 && i.op1_register().size() == 8).then_some(value)
    } else if i.mnemonic() == Mnemonic::Lea
        && i.op0_register().size() == 8
        && i.op1_kind() == OpKind::Memory
        && i.memory_index() == Register::None
        && registers.get(&i.memory_base().full_register()) == Some(&Input::DataModel)
        && i.memory_displacement64() <= 0x800
        && i.memory_displacement64().is_multiple_of(8)
    {
        Some(Input::Instance(i.memory_displacement64() as usize))
    } else if i.mnemonic() == Mnemonic::Lea && i.op0_register().size() == 8 {
        loader_stack_address(i, registers).map(Input::Stack)
    } else if extra_flags_valid && i.op0_register().size() == 4 {
        loader_extra_flags(i, registers)
    } else {
        None
    }
}

fn loader_stack_address(
    i: &iced_x86::Instruction,
    registers: &HashMap<Register, Input>,
) -> Option<i64> {
    if i.memory_index() != Register::None {
        return None;
    }
    let Input::Stack(base) = registers.get(&i.memory_base().full_register())? else {
        return None;
    };
    base.checked_add(i.memory_displacement64() as i64)
}

fn loader_extra_flags(
    i: &iced_x86::Instruction,
    registers: &HashMap<Register, Input>,
) -> Option<Input> {
    // Windows x64's sixth argument is entry RSP+0x30, independent of the
    // compiler's local frame or choice of frame register.
    (i.op1_kind() == OpKind::Memory
        && i.memory_size().size() == 4
        && loader_stack_address(i, registers) == Some(0x30))
    .then_some(Input::ExtraFlags)
}

fn loader_move_stack(registers: &mut HashMap<Register, Input>, delta: i64) {
    let next = match registers.remove(&Register::RSP) {
        Some(Input::Stack(at)) => at.checked_add(delta).filter(|at| (-4096..=0).contains(at)),
        _ => None,
    };
    if let Some(at) = next {
        registers.insert(Register::RSP, Input::Stack(at));
    }
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
            Mnemonic::Mov | Mnemonic::Movdqu | Mnemonic::Movdqa
                if i.op0_kind() == OpKind::Memory && i.op1_kind() == OpKind::Register =>
            {
                let offset = address(&i, &registers)?;
                let value = registers.get(&i.op1_register().full_register()).copied();
                let size = i.memory_size().size();
                reader_stack_write(&mut stack, offset, value, size);
            }
            Mnemonic::Xor | Mnemonic::Xorps | Mnemonic::Pxor
                if i.op0_kind() == OpKind::Register
                    && i.op1_kind() == OpKind::Register
                    && i.op0_register() == i.op1_register()
                    && i.op0_register().size() >= 4 =>
            {
                registers.insert(destination, Zero);
            }
            Mnemonic::Push if i.op0_kind() == OpKind::Register && i.op0_register().size() == 8 => {
                let Stack(offset) = *registers.get(&Register::RSP)? else {
                    return None;
                };
                let next = offset.checked_sub(8)?;
                registers.insert(Register::RSP, Stack(next));
                reader_stack_write(&mut stack, next, registers.get(&destination).copied(), 8);
            }
            Mnemonic::Sub
                if i.op0_register() == Register::RSP
                    && matches!(
                        i.op1_kind(),
                        OpKind::Immediate32to64 | OpKind::Immediate8to64
                    ) =>
            {
                let Stack(offset) = *registers.get(&Register::RSP)? else {
                    return None;
                };
                let next = offset.checked_sub(i.immediate(1) as i64)?;
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

fn reader_stack_write(
    stack: &mut HashMap<i64, (Input, usize)>,
    offset: i64,
    value: Option<Input>,
    size: usize,
) {
    // Callee-saved registers need no input provenance, but even an unknown or
    // partial write must invalidate any argument it overlaps. Normalize SIMD
    // zero stores to words so both initialization forms prove the same range.
    stack.retain(|at, (_, prior_size)| {
        *at + *prior_size as i64 <= offset || *at >= offset + size as i64
    });
    if let Some(value) = value {
        if value == Input::Zero && size == 16 {
            stack.insert(offset, (value, 8));
            stack.insert(offset + 8, (value, 8));
        } else {
            stack.insert(offset, (value, size));
        }
    }
}

// Search for the LEA opcode before checking its REX prefix. Those prefixes
// occur throughout .text; only RIP-relative LEAs can name the loader anchor.
fn rip_relative_leas(bytes: &[u8]) -> impl Iterator<Item = usize> + '_ {
    memchr::memchr_iter(0x8d, bytes).filter_map(|opcode| {
        let at = opcode.checked_sub(1)?;
        let code = bytes.get(at..at + 7)?;
        (matches!(code[0], 0x48 | 0x4c) && code[2] & 0xc7 == 5).then_some(at)
    })
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
    for at in rip_relative_leas(text_bytes) {
        let code = &text_bytes[at..at + 7];
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
    fn loader_anchor_candidates_keep_both_rex_prefixes_and_reject_truncation() -> Result<()> {
        let bytes = hex(
            "8D90488D05000000004C8D3D00000000408D0500000000488B0500000000488D0400000000488D05",
        )?;
        assert_eq!(rip_relative_leas(&bytes).collect::<Vec<_>>(), [2, 9]);
        assert_eq!(rip_relative_leas(&[]).count(), 0);
        Ok(())
    }

    #[test]
    fn reader_tracks_saved_registers_and_vector_zero_initialization() -> Result<()> {
        let original = "4C8BDC49895B08574881ECC00000000F57C0F30F7F442460488B8424F800000049894390498D4398498943884533D2458953804C89542440488B8424F0000000488944243844894C24304C895424284C895424204D8BC84C8BC2488BD1498D4BC0E81AD1FFFF9080BC24B0000000007421";
        let code = hex(original)?;
        assert_eq!(reader_call(&code, 0x10000), Some(0xd180));
        // Equivalent stack allocation with signed, compact immediates.
        let compact = hex(&original.replace("4881ECC0000000", "4883EC604883EC60"))?;
        assert_eq!(reader_call(&compact, 0x10000), Some(0xd181));
        assert!(
            reader_call(
                &hex(&original.replace("4881ECC0000000", "4883ECC0"))?,
                0x10000
            )
            .is_none()
        );
        // Relocate the local frame, use a different saved register and XMM
        // register, and keep the incoming arguments/result relationships.
        let relocated = original
            .replace("49895B0857", "4989730853")
            .replace("ECC0000000", "ECD0000000")
            .replace("0F57C0F30F7F442460", "660FEFC9660F7F4C2470")
            .replace("24F8000000", "2408010000")
            .replace("24F0000000", "2400010000")
            .replace("24B0000000", "24C0000000")
            .replace("49894390", "49894380")
            .replace("49894388", "49898378FFFFFF")
            .replace("45895380", "45899370FFFFFF");
        // Instruction lengths may change; relocate the relative call as well.
        let mut moved = hex(&relocated)?;
        let call = moved.len() - 16;
        assert_eq!(moved[call], 0xe8);
        let target = 0x23456usize;
        moved[call + 1..call + 5]
            .copy_from_slice(&((target as i64 - (0x20000 + call + 5) as i64) as i32).to_le_bytes());
        assert_eq!(reader_call(&moved, 0x20000), Some(target));
        for (from, to) in [
            ("0F57C0", "0F57C1"),         // vector is not zero
            ("7F442460", "7F4C2460"),     // different vector source
            ("7F442460", "7F442468"),     // incomplete limits range
            ("57", "90"),                 // missing stack adjustment
            ("498D4398", "498D4390"),     // wrong limits address
            ("44894C2430", "44895C2430"), // unknown saved register as flags
            ("4533D2", "4532D2"),         // byte clear cannot prove a pointer
            ("4D8BC8", "49895B984D8BC8"), // unknown overwrite of limits
            ("4D8BC8", "4589539C4D8BC8"), // partial overwrite of limits
            ("80BC24B000000000", "80BC24A800000000"),
        ] {
            assert!(original.contains(from));
            assert!(
                reader_call(&hex(&original.replacen(from, to, 1))?, 0x10000).is_none(),
                "accepted {from} -> {to}"
            );
        }
        for end in 0..code.len() {
            assert!(
                reader_call(&code[..end], 0x10000).is_none(),
                "accepted truncated reader at {end}"
            );
        }
        Ok(())
    }

    /// Opt-in, file-only check: no running Studio or helper is needed.
    #[test]
    #[ignore]
    fn installed_studio_builds_expose_the_reader_contract() -> Result<()> {
        for path in std::env::var("RENIUM_STUDIO_EXES")?.split(';') {
            let prepared = prepared(Path::new(path))?;
            println!(
                "{path}: {:?}; history {:?}",
                prepared.trace, prepared.history
            );
        }
        Ok(())
    }
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
    fn loader_accepts_conditional_extra_flags_only_with_proven_stack_and_condition() -> Result<()> {
        let original = "574883EC308B742468488BE983CE04488BCA4584C9498BF8488BDA0F45742468E8000000004C8D87F001000033C948894C2420448BCE488BD34C0F44C1488BCDE800000000C3";
        let code = hex(original)?;
        let found = loader_calls(&code, 0x1000);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].1, 0x1f0);
        // The incoming argument stays at entry RSP+0x30 when the compiler
        // changes its frame size, register allocation and model subobject.
        let relocated = original
            .replace("EC30", "EC40")
            .replace("2468", "2478")
            .replace("8B7424", "8B5C24")
            .replace("83CE04", "83CB04")
            .replace("488BDA", "488BF2")
            .replace("457424", "455C24")
            .replace("448BCE488BD3", "448BCB488BD6")
            .replace("87F0010000", "8710020000");
        let found = loader_calls(&hex(&relocated)?, 0x2000);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].1, 0x210);
        for (from, to) in [
            ("EC30", "EC20"),                     // wrong incoming argument
            ("8B742468", "8B742460"),             // fifth argument is not flags
            ("8B742468", "668B742468"),           // partial load
            ("83CE04", "83CE08"),                 // wrong flag
            ("33C9", "32C9"),                     // byte clear cannot prove a null pointer
            ("48894C2420", "894C2420"),           // partial fifth argument
            ("488BE9", "8BE990"),                 // truncated context pointer
            ("4584C9", "4584C0"),                 // different boolean
            ("4584C9", "4584C94885C0"),           // condition overwritten
            ("0F45742468", "0F44742468"),         // reversed choice
            ("0F45742468", "0F45742460"),         // different source
            ("0F45742468", "74000F45742468"),     // intervening branch
            ("4584C9", "C7442468000000004584C9"), // incoming flags overwritten
        ] {
            assert!(original.contains(from));
            assert!(
                loader_calls(&hex(&original.replace(from, to))?, 0x1000).is_empty(),
                "accepted {from} -> {to}"
            );
        }
        for end in 0..code.len() - 2 {
            assert!(loader_calls(&code[..end], 0x1000).is_empty());
        }
        Ok(())
    }
}
