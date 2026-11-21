use super::*;
use crate::studio::native::serializer::functions::YieldContract;
use std::collections::{BTreeMap, VecDeque};

#[derive(Clone)]
struct Binding {
    field: usize,
    table: Vec<u8>,
    code: Vec<(usize, Vec<u8>)>,
}

type BindingKey = ([u32; 3], usize, bool, Option<usize>);
static BINDINGS: OnceLock<Mutex<HashMap<BindingKey, Binding>>> = OnceLock::new();

#[path = "windows_functions_yield.rs"]
mod yield_abi;
pub(in super::super) use yield_abi::ContinuationAbi;

pub(super) fn qualify_yield(
    memory: &ProcessMemory,
    studio: &ModuleEntry,
    layout: &PackageLayout,
    table: usize,
    binding: (usize, usize),
    mode: u32,
) -> Result<YieldContract> {
    let methods = properties::vtable_methods(memory, studio, layout, table)?;
    let copies = byte_copy_imports(memory, studio)?;
    let mut cache = HashMap::new();
    yield_abi::qualify(
        mode,
        binding.0,
        binding.1 as u64,
        &methods.into_iter().map(|at| at as u64).collect::<Vec<_>>(),
        &mut |at| {
            cache
                .entry(at)
                .or_insert_with(|| dispatch_code(memory, studio, layout, at as usize).ok())
                .clone()
        },
        &mut |at| memory.read_u64(at as usize).ok(),
        &mut |at| {
            let Ok(bytes) = verified_code(memory, studio, layout, at as usize, 6) else {
                return false;
            };
            let i = Decoder::with_ip(64, &bytes, at, DecoderOptions::NONE).decode();
            i.flow_control() == FlowControl::IndirectBranch
                && i.is_ip_rel_memory_operand()
                && copies
                    .get(&(i.ip_rel_memory_address() as usize))
                    .is_some_and(|target| {
                        memory.read_u64(i.ip_rel_memory_address() as usize).ok()
                            == Some(*target as u64)
                    })
        },
    )
}

fn byte_copy_imports(
    memory: &ProcessMemory,
    studio: &ModuleEntry,
) -> Result<HashMap<usize, usize>> {
    let mapped = ImageView::open(&studio.path)?;
    let image = PeImage::parse(mapped.bytes())?;
    let (rva, size) = probes::directory(&image, 1)?;
    let table = image.rva_to_offset(rva)?;
    let loaded = modules(memory.pid)?;
    let mut copies = HashMap::new();
    for offset in (0..size.saturating_sub(19)).step_by(20) {
        let row = table + offset;
        let name = read_u32(image.bytes, row + 12)? as usize;
        if name == 0 {
            break;
        }
        let dll = probes::image_string(&image, name)?;
        if !dll.to_ascii_lowercase().starts_with("vcruntime")
            && !dll.eq_ignore_ascii_case("ucrtbase.dll")
        {
            continue;
        }
        let names = image.rva_to_offset(read_u32(image.bytes, row)? as usize)?;
        let iat = read_u32(image.bytes, row + 16)? as usize;
        for slot in 0..4096 {
            let entry = read_u64(image.bytes, names + slot * 8)?;
            if entry == 0 {
                break;
            }
            if entry >> 63 != 0 {
                continue;
            }
            let name = probes::image_string(&image, entry as usize + 2)?;
            if !matches!(name, "memcpy" | "memmove") {
                continue;
            }
            let module = loaded
                .iter()
                .find(|m| m.name.eq_ignore_ascii_case(dll))
                .context("yield-payload-string: byte-copy module missing")?;
            let mapped = ImageView::open(&module.path)?;
            let imported = PeImage::parse(mapped.bytes())?;
            verify_loaded_image(memory, module, imported.image_stamp)?;
            let target = probes::exported_rva(&imported, name)?;
            // The export and each routine it jumps to are the file's code.
            let (mut at, mut proved) = (target, false);
            for _ in 0..3 {
                let live = memory.read_vec(module.base + at, 32)?;
                let file = (imported.require_executable_rva(at).is_ok()
                    && imported.require_executable_rva(at + 31).is_ok())
                .then(|| imported.rva_to_offset(at).ok())
                .flatten()
                .and_then(|offset| slice(imported.bytes, offset, 32).ok());
                match file.and_then(|file| copy_entry(&live, file, at)) {
                    Some(next) if next == at => {
                        proved = true;
                        break;
                    }
                    Some(next) => at = next,
                    None => break,
                }
            }
            anyhow::ensure!(proved, "yield-payload-string: byte-copy export changed");
            copies.insert(studio.base + iat + slot * 8, module.base + target);
        }
    }
    Ok(copies)
}

// Where the loaded bytes at `rva` of a copy export lead: `rva` itself where
// they are the file's code, the RVA they jump to where the file holds a jump.
// The C runtime points its memcpy export at the routine it picks for the CPU
// when it loads: the export stays a five-byte jump with another displacement.
// Nothing after a jump runs, so a neighbor the runtime moved as well does not
// count against it.
fn copy_entry(live: &[u8], file: &[u8], rva: usize) -> Option<usize> {
    let jump = |code: &[u8]| {
        (code.first() == Some(&0xe9))
            .then(|| code.get(1..5)?.try_into().ok().map(i32::from_le_bytes))
            .flatten()
    };
    if jump(file).is_none() {
        return (live == file).then_some(rva);
    }
    let next = usize::try_from(rva as i64 + 5 + jump(live)? as i64).ok()?;
    (next != rva).then_some(next)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Storage {
    Descriptor,
    Stack,
    Capture,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Value {
    ImagePointer(u64),
    Continuation(u8), // Symbolic incoming callback, used only by yield qualification.
    Integer(u64),
    TaggedPointer(u64, u8),
    Pointer(Storage, i64),
    Bytes(Storage, i64, usize),
    Instance,
    InstanceInput(i64),
    Lua,
    Adjustment(i64),
    AdjustedInstance(i64),
    VbTable(i64),
    VirtualAdjustment(i64, i64),
    VirtualInstance(i64, i64),
    VbOffset(i64, i64),
}

impl Value {
    fn slice(self, offset: usize, width: usize) -> Option<Self> {
        match self {
            Self::Bytes(storage, start, size) if offset.checked_add(width)? <= size => {
                Some(Self::Bytes(storage, start + offset as i64, width))
            }
            Self::Integer(value) if offset == 0 && width <= 8 => Some(Self::Integer(
                value & u64::MAX.checked_shr((64 - width * 8) as u32).unwrap_or(0),
            )),
            _ if offset == 0 && width == 8 => Some(self),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct State {
    registers: HashMap<Register, Value>,
    stack: BTreeMap<i64, (usize, Value)>,
    lua_callback: bool,
    zero: Option<bool>,
    complete_receiver: bool,
}

impl State {
    fn register(&self, reg: Register) -> Option<Value> {
        self.registers.get(&reg.full_register()).copied()
    }
    fn address(&self, i: &Instruction) -> Option<(Storage, i64)> {
        if i.memory_index() != Register::None {
            return None;
        }
        let Value::Pointer(storage, offset) = self.register(i.memory_base())? else {
            return None;
        };
        Some((
            storage,
            offset.checked_add(i.memory_displacement64() as i64)?,
        ))
    }
    fn read(&self, i: &Instruction, operand: u32, width: usize) -> Option<Value> {
        match i.op_kind(operand) {
            OpKind::Register if width <= i.op_register(operand).size() => {
                self.register(i.op_register(operand))?.slice(0, width)
            }
            OpKind::Memory => {
                if i.memory_index() == Register::None
                    && width == 8
                    && self.register(i.memory_base()) == Some(Value::Instance)
                {
                    return Some(Value::VbTable(i.memory_displacement64() as i64));
                }
                if width == 4
                    && i.memory_index_scale() == 1
                    && i.memory_displacement64() == 0
                    && let (Some(Value::Adjustment(field)), Some(Value::VbTable(offset))) = (
                        self.register(i.memory_base()),
                        self.register(i.memory_index()),
                    )
                {
                    return Some(Value::VirtualAdjustment(field, offset));
                }
                let (storage, offset) = self.address(i)?;
                match storage {
                    Storage::Descriptor if (8..=65520).contains(&offset) => {
                        Some(Value::Bytes(storage, offset, width))
                    }
                    Storage::Stack | Storage::Capture => {
                        let (start, (size, value)) = self.stack.range(..=offset).next_back()?;
                        let relative = usize::try_from(offset - start).ok()?;
                        (relative.checked_add(width)? <= *size)
                            .then(|| value.slice(relative, width))
                            .flatten()
                    }
                    _ => None,
                }
            }
            OpKind::Immediate8
            | OpKind::Immediate16
            | OpKind::Immediate32
            | OpKind::Immediate64
            | OpKind::Immediate8to16
            | OpKind::Immediate8to32
            | OpKind::Immediate8to64
            | OpKind::Immediate32to64 => Some(Value::Integer(i.immediate(operand))),
            _ => None,
        }
    }
    fn store(&mut self, address: i64, width: usize, value: Option<Value>) {
        self.stack.retain(|start, (size, _)| {
            *start + *size as i64 <= address || *start >= address + width as i64
        });
        if let Some(value) = value {
            self.stack.insert(address, (width, value));
        }
    }
    fn merge(&mut self, other: &Self) -> bool {
        let before = self.clone();
        if self.zero != other.zero {
            self.zero = None;
        }
        self.registers
            .retain(|key, value| other.registers.get(key) == Some(value));
        for (reg, value) in &before.registers {
            if matches!(value, Value::Continuation(0 | 1 | 4))
                && matches!(
                    other.registers.get(reg),
                    Some(Value::Continuation(0 | 1 | 4))
                )
                && other.registers.get(reg) != Some(value)
            {
                self.registers.insert(*reg, Value::Continuation(4));
            }
        }
        // Arity hazards are may-read facts: an unprovided input surviving on
        // either branch must not disappear when ordinary must-facts merge.
        for (reg, value) in before.registers.iter().chain(other.registers.iter()) {
            if matches!(value, Value::Continuation(2..)) {
                self.registers.insert(*reg, *value);
            }
        }
        self.stack
            .retain(|key, value| other.stack.get(key) == Some(value));
        *self != before
    }
    fn called_field(&self, i: &Instruction) -> Option<usize> {
        if self.lua_callback && self.register(Register::RDX) != Some(Value::Lua) {
            return None;
        }
        let Value::Bytes(Storage::Descriptor, field, 8) = self.read(i, 0, 8)? else {
            return None;
        };
        (field % 8 == 0 && self.register(Register::RCX) == Some(Value::AdjustedInstance(field + 8)))
            .then(|| usize::try_from(field).ok())
            .flatten()
    }
    fn passed_origins(&self) -> bool {
        let mut values = Vec::new();
        for reg in [Register::RCX, Register::RDX, Register::R8, Register::R9] {
            if let Some(value) = self.register(reg) {
                values.push(value);
                if let Value::Pointer(Storage::Stack, offset) = value {
                    values.extend(self.stack.get(&offset).map(|(_, value)| *value));
                }
            }
        }
        values
            .iter()
            .any(|v| matches!(v, Value::Instance | Value::InstanceInput(_)))
            && values.iter().any(|value| {
                matches!(
                    value,
                    Value::Bytes(Storage::Descriptor, ..) | Value::Pointer(Storage::Descriptor, _)
                )
            })
    }
    fn moved_value(&self, i: &Instruction, width: usize) -> Option<Value> {
        let value = self.read(i, 1, width)?;
        if i.mnemonic() == Mnemonic::Movsxd
            && i.op0_register().size() == 8
            && let Value::Bytes(Storage::Descriptor, field, 4) = value
        {
            Some(Value::Adjustment(field))
        } else {
            Some(value)
        }
    }

    fn pop(&mut self, dst: Register) {
        self.registers.remove(&dst);
        if let Some(Value::Pointer(Storage::Stack, offset)) = self.register(Register::RSP) {
            if let Some((_, value)) = self.stack.remove(&offset) {
                self.registers.insert(dst, value);
            }
            self.registers
                .insert(Register::RSP, Value::Pointer(Storage::Stack, offset + 8));
        }
    }

    fn arithmetic_value(&self, i: &Instruction, dst: Register) -> Option<Value> {
        let left = self.register(dst);
        let right = self.read(i, 1, 8);
        match (left, right, i.mnemonic()) {
            (Some(Value::Adjustment(offset)), Some(Value::Instance), Mnemonic::Add)
            | (Some(Value::Instance), Some(Value::Adjustment(offset)), Mnemonic::Add) => {
                Some(Value::AdjustedInstance(offset))
            }
            (
                Some(Value::VirtualAdjustment(field, offset)),
                Some(Value::Instance),
                Mnemonic::Add,
            )
            | (
                Some(Value::Instance),
                Some(Value::VirtualAdjustment(field, offset)),
                Mnemonic::Add,
            ) => Some(Value::VirtualInstance(field, offset)),
            (
                Some(Value::VbOffset(field, offset)),
                Some(Value::VirtualInstance(high, base)),
                Mnemonic::Add,
            )
            | (
                Some(Value::VirtualInstance(high, base)),
                Some(Value::VbOffset(field, offset)),
                Mnemonic::Add,
            ) if high == field + 4 && base == offset => Some(Value::AdjustedInstance(field)),
            (Some(Value::Adjustment(field)), _, Mnemonic::Add)
                if matches!(
                    i.op1_kind(),
                    OpKind::Immediate8to64 | OpKind::Immediate32to64
                ) =>
            {
                Some(Value::VbOffset(field, i.immediate(1) as i64))
            }
            (Some(Value::Pointer(storage, offset)), _, _)
                if matches!(
                    i.op1_kind(),
                    OpKind::Immediate8to64 | OpKind::Immediate32to64
                ) =>
            {
                let delta = i.immediate(1) as i64;
                offset
                    .checked_add(if i.mnemonic() == Mnemonic::Add {
                        delta
                    } else {
                        -delta
                    })
                    .map(|offset| Value::Pointer(storage, offset))
            }
            _ => None,
        }
    }

    fn move_instruction(&mut self, i: &Instruction, width: usize, dst: Register) {
        let width = if matches!(i.mnemonic(), Mnemonic::Movd | Mnemonic::Movsxd) {
            4
        } else if i.mnemonic() == Mnemonic::Movq {
            8
        } else {
            width
        };
        let value = self.moved_value(i, width);
        if i.op0_kind() == OpKind::Register {
            self.registers.remove(&dst);
            if let Some(value) = value {
                self.registers.insert(dst, value);
            }
        } else if let Some((Storage::Stack | Storage::Capture, offset)) = self.address(i) {
            self.store(offset, width, value);
        }
    }

    fn nonnull_test(&self, i: &Instruction) -> bool {
        self.complete_receiver
            && i.op0_kind() == OpKind::Register
            && i.op1_kind() == OpKind::Register
            && i.op0_register() == i.op1_register()
            && i.op0_register().size() == 8
            && matches!(
                self.register(i.op0_register()),
                Some(Value::Instance | Value::InstanceInput(_))
            )
    }

    fn lea_value(&self, i: &Instruction, width: usize) -> Option<Value> {
        if width != 8 {
            return None;
        }
        if i.is_ip_rel_memory_operand() {
            return Some(Value::ImagePointer(i.ip_rel_memory_address()));
        }
        if i.memory_index() == Register::None
            && matches!(self.register(i.memory_base()), Some(Value::InstanceInput(offset)) if offset.checked_add(i.memory_displacement64() as i64) == Some(0))
        {
            return Some(Value::Instance);
        }
        self.address(i)
            .map(|(storage, offset)| Value::Pointer(storage, offset))
    }

    fn step(&mut self, i: &Instruction) {
        let dst = i.op0_register().full_register();
        let width = if i.op0_kind() == OpKind::Memory {
            i.memory_size().size()
        } else {
            i.op0_register().size()
        };
        if i.rflags_modified() != 0 {
            self.zero = None;
        }
        match i.mnemonic() {
            Mnemonic::Xor | Mnemonic::Xorps | Mnemonic::Pxor
                if i.op0_kind() == OpKind::Register
                    && i.op1_kind() == OpKind::Register
                    && i.op0_register() == i.op1_register() =>
            {
                self.registers.insert(dst, Value::Integer(0));
            }
            Mnemonic::Test if self.nonnull_test(i) => {
                self.zero = Some(false);
            }
            Mnemonic::Cmove if self.zero == Some(false) => {}
            Mnemonic::Mov
            | Mnemonic::Movaps
            | Mnemonic::Movups
            | Mnemonic::Movdqa
            | Mnemonic::Movdqu
            | Mnemonic::Movd
            | Mnemonic::Movq
            | Mnemonic::Movsxd => {
                self.move_instruction(i, width, dst);
            }
            Mnemonic::Lea => {
                let value = self.lea_value(i, width);
                self.registers.remove(&dst);
                if let Some(value) = value {
                    self.registers.insert(dst, value);
                }
            }
            Mnemonic::Or | Mnemonic::And if i.op0_kind() == OpKind::Register && width == 8 => {
                let value = match (self.register(dst), i.op1_kind(), i.mnemonic()) {
                    (Some(Value::ImagePointer(at)), OpKind::Immediate8to64, Mnemonic::Or)
                        if matches!(i.immediate8to64(), 2 | 6) && at % 8 == 0 =>
                    {
                        Some(Value::TaggedPointer(at, i.immediate8to64() as u8))
                    }
                    (Some(Value::ImagePointer(at)), OpKind::Immediate8to64, Mnemonic::And)
                        if i.immediate8to64() == -5 && at % 8 == 0 =>
                    {
                        Some(Value::ImagePointer(at))
                    }
                    _ => None,
                };
                self.registers.remove(&dst);
                if let Some(value) = value {
                    self.registers.insert(dst, value);
                }
            }
            Mnemonic::Add | Mnemonic::Sub if i.op0_kind() == OpKind::Register && width == 8 => {
                let value = self.arithmetic_value(i, dst);
                self.registers.remove(&dst);
                if let Some(value) = value {
                    self.registers.insert(dst, value);
                }
            }
            Mnemonic::Shr if i.op1_kind() == OpKind::Immediate8 && i.immediate8() == 32 => {
                let value = self.register(dst).and_then(|value| value.slice(4, 4));
                self.registers.remove(&dst);
                if let Some(value) = value {
                    self.registers.insert(dst, value);
                }
            }
            Mnemonic::Psrldq => {
                let value = self.register(dst).and_then(|value| {
                    value.slice(
                        i.immediate8() as usize,
                        16usize.saturating_sub(i.immediate8() as usize),
                    )
                });
                self.registers.remove(&dst);
                if let Some(value) = value {
                    self.registers.insert(dst, value);
                }
            }
            Mnemonic::Push => {
                if let Some(Value::Pointer(Storage::Stack, offset)) = self.register(Register::RSP) {
                    self.store(offset - 8, 8, self.read(i, 0, 8));
                    self.registers
                        .insert(Register::RSP, Value::Pointer(Storage::Stack, offset - 8));
                }
            }
            Mnemonic::Pop => self.pop(dst),
            Mnemonic::Call => {
                self.zero = None;
                self.invalidate_call_arguments();
                for reg in [
                    Register::RAX,
                    Register::RCX,
                    Register::RDX,
                    Register::R8,
                    Register::R9,
                    Register::R10,
                    Register::R11,
                    Register::XMM0,
                    Register::XMM1,
                    Register::XMM2,
                    Register::XMM3,
                    Register::XMM4,
                    Register::XMM5,
                ] {
                    self.registers.remove(&reg.full_register());
                }
            }
            _ => self.forget_written(i),
        }
    }

    // An instruction nothing above models: what it writes is no longer known.
    fn forget_written(&mut self, i: &Instruction) {
        for reg in InstructionInfoFactory::new().info(i).used_registers() {
            if matches!(
                reg.access(),
                OpAccess::Write
                    | OpAccess::CondWrite
                    | OpAccess::ReadWrite
                    | OpAccess::ReadCondWrite
            ) {
                self.registers.remove(&reg.register().full_register());
            }
        }
        if i.op0_kind() == OpKind::Memory
            && !matches!(i.mnemonic(), Mnemonic::Cmp | Mnemonic::Test)
            && let Some((Storage::Stack | Storage::Capture, offset)) = self.address(i)
        {
            self.store(offset, i.memory_size().size(), None);
        }
    }

    fn invalidate_call_arguments(&mut self) {
        // A pointer into the capture is just as capable of mutating ownership
        // as one to the callable's start. Outgoing stack slots are not arguments
        // unless the callee's arity is known; they can contain stale pointers.
        let passed = [Register::RCX, Register::RDX, Register::R8, Register::R9]
            .iter()
            .filter_map(|reg| self.register(*reg))
            .collect::<Vec<_>>();
        let ranges = self
            .stack
            .iter()
            .filter_map(|(at, (width, value))| match value {
                Value::ImagePointer(_)
                    if self.stack.get(&(at + 56))
                        == Some(&(8, Value::Pointer(Storage::Stack, *at))) =>
                {
                    Some((*at, 64))
                }
                Value::TaggedPointer(..) => Some((*at - 24, 32)),
                Value::Continuation(0 | 1 | 4) if *width == 32 => Some((*at, 32)),
                _ => None,
            })
            .collect::<Vec<_>>();
        for (start, size) in ranges {
            if passed.iter().any(|value| matches!(value, Value::Pointer(Storage::Stack | Storage::Capture, at) if *at >= start && *at < start + size as i64)) {
                self.store(start, size, None);
            }
        }
    }
}

struct Flow {
    instructions: HashMap<u64, Instruction>,
    states: HashMap<u64, State>,
}

// `State::step` of a call, with what the callee does to the callables it is handed.
fn step_call(
    state: &mut State,
    i: &Instruction,
    code: &mut dyn FnMut(u64, bool) -> Option<Vec<u8>>,
) {
    let target = i.near_branch_target();
    let constructed = yield_abi::constructor(state, target, &mut |at| code(at, false));
    let copied = yield_abi::copied_origin(state, target, &mut |at| code(at, false));
    let copied_fields = if copied.is_none() {
        yield_abi::captures::copied_fields(state, target, &mut |at| code(at, false), 0)
    } else {
        None
    };
    let heap = yield_abi::captures::heap_fields(state, target, &mut |at| code(at, false));
    let source = state.register(Register::RDX);
    state.step(i);
    if let Some((object, origin, preserves)) = copied {
        if preserves && let Some(Value::Pointer(Storage::Stack | Storage::Capture, source)) = source
        {
            state.store(source, 32, Some(Value::Continuation(origin)));
        }
        state.store(object, 32, Some(Value::Continuation(origin)));
    }
    if let Some(updates) = copied_fields {
        for at in updates.consumed {
            state.store(at, 32, None);
        }
        for (at, width, value) in updates.fields {
            state.store(at, width, Some(value));
        }
        if let Some(value) = updates.returned {
            state.registers.insert(Register::RAX, value);
        }
    }
    if let Some((_, _, updates)) = heap {
        for at in updates.consumed {
            state.store(at, 32, None);
        }
    }
    if let Some((object, table)) = constructed {
        let value = Value::Pointer(Storage::Stack, object);
        state.store(object, 8, Some(Value::ImagePointer(table)));
        state.store(object + 56, 8, Some(value));
        state.registers.insert(Register::RAX, value);
    }
}

fn flow(
    entry: u64,
    initial: State,
    code: &mut dyn FnMut(u64, bool) -> Option<Vec<u8>>,
    budget: &mut usize,
) -> Option<Flow> {
    let bytes = code(entry, true)?;
    if bytes.len() > 65536 {
        return None;
    }
    let mut code_budget = 65536usize.checked_sub(bytes.len())?;
    let mut instructions = Decoder::with_ip(64, &bytes, entry, DecoderOptions::NONE)
        .into_iter()
        .map(|i| (i.ip(), i))
        .collect::<HashMap<_, _>>();
    let mut states = HashMap::from([(entry, initial)]);
    let mut queue = VecDeque::from([entry]);
    while let Some(pc) = queue.pop_front() {
        *budget = budget.checked_sub(1)?;
        if !instructions.contains_key(&pc) {
            // MSVC can split a single control-flow graph into several unwind
            // ranges. Verify every reached fragment instead of either skipping
            // the method or assuming adjacent executable bytes belong to it.
            let bytes = code(pc, true)?;
            code_budget = code_budget.checked_sub(bytes.len())?;
            for instruction in Decoder::with_ip(64, &bytes, pc, DecoderOptions::NONE) {
                if let Some(previous) = instructions.insert(instruction.ip(), instruction)
                    && previous != instruction
                {
                    return None;
                }
            }
        }
        let i = instructions.get(&pc)?;
        if i.is_invalid() {
            return None;
        }
        let mut state = states[&pc].clone();
        if i.flow_control() == FlowControl::Call {
            step_call(&mut state, i, code);
        } else {
            state.step(i);
        }
        let successors = match i.flow_control() {
            FlowControl::Return
            | FlowControl::Exception
            | FlowControl::Interrupt
            | FlowControl::IndirectBranch => vec![],
            FlowControl::UnconditionalBranch
                if instructions.contains_key(&i.near_branch_target()) =>
            {
                vec![i.near_branch_target()]
            }
            FlowControl::UnconditionalBranch => vec![],
            FlowControl::ConditionalBranch => vec![i.next_ip(), i.near_branch_target()],
            _ => vec![i.next_ip()],
        };
        for next in successors {
            let changed = if let Some(existing) = states.get_mut(&next) {
                existing.merge(&state)
            } else {
                states.insert(next, state.clone());
                true
            };
            if changed {
                queue.push_back(next);
            }
        }
    }
    Some(Flow {
        instructions,
        states,
    })
}

fn dispatch_fields(
    entry: u64,
    initial: State,
    code: &mut impl FnMut(u64, bool) -> Option<Vec<u8>>,
    budget: &mut usize,
    depth: usize,
    accept: &mut impl FnMut(usize, &State) -> bool,
) -> Option<HashSet<usize>> {
    if depth > 8 {
        return None;
    }
    let Flow {
        instructions,
        states,
    } = flow(entry, initial, code, budget)?;
    let mut fields = HashSet::new();
    for (pc, mut state) in states {
        let i = &instructions[&pc];
        if matches!(
            i.flow_control(),
            FlowControl::IndirectCall | FlowControl::IndirectBranch
        ) {
            if let Some(field) = state.called_field(i) {
                if !accept(field, &state) {
                    return None;
                }
                fields.insert(field);
            }
        } else if (i.flow_control() == FlowControl::Call
            || i.flow_control() == FlowControl::UnconditionalBranch
                && !instructions.contains_key(&i.near_branch_target()))
            && state.passed_origins()
        {
            state.registers.retain(|reg, _| {
                matches!(
                    reg,
                    Register::RCX | Register::RDX | Register::R8 | Register::R9 | Register::RSP
                )
            });
            if i.flow_control() == FlowControl::Call
                && let Some(Value::Pointer(Storage::Stack, offset)) = state.register(Register::RSP)
            {
                state
                    .registers
                    .insert(Register::RSP, Value::Pointer(Storage::Stack, offset - 8));
                state.store(offset - 8, 8, None);
            }
            fields.extend(dispatch_fields(
                i.near_branch_target(),
                state,
                code,
                budget,
                depth + 1,
                accept,
            )?);
        }
    }
    Some(fields)
}

fn dispatch_field(entry: u64, mut code: impl FnMut(u64) -> Option<Vec<u8>>) -> Option<usize> {
    dispatch_field_for(entry, &mut code, false)
}

fn dispatch_field_for(
    entry: u64,
    mut code: impl FnMut(u64) -> Option<Vec<u8>>,
    lua_callback: bool,
) -> Option<usize> {
    dispatch_field_checked(entry, &mut code, lua_callback, &mut |_, _| true)
}

fn dispatch_field_checked(
    entry: u64,
    code: &mut impl FnMut(u64) -> Option<Vec<u8>>,
    lua_callback: bool,
    accept: &mut impl FnMut(usize, &State) -> bool,
) -> Option<usize> {
    dispatch_field_fetch(entry, &mut |at, _| code(at), lua_callback, None, accept)
}

fn dispatch_field_fetch(
    entry: u64,
    code: &mut impl FnMut(u64, bool) -> Option<Vec<u8>>,
    lua_callback: bool,
    receiver_offset: impl Into<Option<usize>>,
    accept: &mut impl FnMut(usize, &State) -> bool,
) -> Option<usize> {
    let fields = dispatch_analysis(entry, code, lua_callback, receiver_offset.into(), accept)?;
    (fields.len() == 1).then(|| *fields.iter().next().unwrap())
}

fn dispatch_analysis(
    entry: u64,
    code: &mut impl FnMut(u64, bool) -> Option<Vec<u8>>,
    lua_callback: bool,
    receiver_offset: impl Into<Option<usize>>,
    accept: &mut impl FnMut(usize, &State) -> bool,
) -> Option<HashSet<usize>> {
    let receiver_offset = receiver_offset.into();
    let initial = State {
        registers: HashMap::from([
            (Register::RCX, Value::Pointer(Storage::Descriptor, 0)),
            (
                Register::RDX,
                match receiver_offset {
                    None | Some(0) => Value::Instance,
                    Some(offset) => Value::InstanceInput(i64::try_from(offset).ok()?),
                },
            ),
            (Register::R8, Value::Lua),
            (Register::RSP, Value::Pointer(Storage::Stack, 0)),
        ]),
        stack: BTreeMap::new(),
        lua_callback,
        zero: None,
        complete_receiver: receiver_offset.is_some(),
    };
    dispatch_fields(entry, initial, code, &mut 32768, 0, accept)
}

pub(in super::super) fn binding(
    memory: &ProcessMemory,
    studio: &ModuleEntry,
    layout: &PackageLayout,
    descriptor: usize,
    table: usize,
) -> Result<(usize, usize)> {
    resolve(
        memory,
        studio,
        layout,
        descriptor,
        member_field(memory, studio, layout, descriptor, table)?,
    )
}

pub(in super::super) fn binding_for_receiver(
    memory: &ProcessMemory,
    studio: &ModuleEntry,
    layout: &PackageLayout,
    descriptor: usize,
    table: usize,
    receiver_offset: usize,
) -> Result<(usize, usize)> {
    resolve(
        memory,
        studio,
        layout,
        descriptor,
        member_field_for(memory, studio, layout, table, false, Some(receiver_offset))?,
    )
}

pub(in super::super) fn member_field(
    memory: &ProcessMemory,
    studio: &ModuleEntry,
    layout: &PackageLayout,
    _descriptor: usize,
    table: usize,
) -> Result<usize> {
    member_field_for(memory, studio, layout, table, false, None)
}

fn member_field_for(
    memory: &ProcessMemory,
    studio: &ModuleEntry,
    layout: &PackageLayout,
    table: usize,
    lua_callback: bool,
    receiver_offset: Option<usize>,
) -> Result<usize> {
    let key = (
        layout.image_stamp,
        table.wrapping_sub(studio.base),
        lua_callback,
        receiver_offset,
    );
    let cached = BINDINGS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock_recover()
        .get(&key)
        .cloned();
    if let Some(binding) = cached
        && memory.read_vec(table, binding.table.len()).ok().as_ref() == Some(&binding.table)
        && binding.code.iter().all(|(rva, code)| {
            memory.read_vec(studio.base + rva, code.len()).ok().as_ref() == Some(code)
        })
    {
        return Ok(binding.field);
    }
    let mut candidates = HashSet::new();
    let mut methods = HashMap::new();
    let mut table_bytes = Vec::new();
    let mut terminated = false;
    for slot in (0..2048).step_by(8) {
        let method = memory.read_u64(table + slot)? as usize;
        table_bytes.extend_from_slice(&(method as u64).to_le_bytes());
        let rva = method.wrapping_sub(studio.base);
        if rva
            .checked_sub(layout.text.virtual_address)
            .is_none_or(|offset| offset >= layout.text.raw_size)
        {
            terminated = true;
            break;
        }
        let mut error = None;
        let field = dispatch_field_fetch(
            method as u64,
            &mut |address, required| {
                let address = usize::try_from(address).ok()?;
                let rva = address.checked_sub(studio.base)?;
                if let Some(code) = methods.get(&rva) {
                    return Some(Vec::clone(code));
                }
                match dispatch_code(memory, studio, layout, address) {
                    Ok(code) => {
                        methods.insert(rva, code.clone());
                        Some(code)
                    }
                    Err(message) => {
                        if required {
                            error = Some(message);
                        }
                        None
                    }
                }
            },
            lua_callback,
            receiver_offset,
            &mut |_, _| true,
        );
        if let Some(error) = error {
            let message = format!("Could not verify reflection dispatch: {error:#}");
            if matches!(
                error.downcast_ref::<properties::NativeError>(),
                Some(properties::NativeError::Unsupported(_))
            ) {
                return Err(properties::NativeError::Unsupported(message).into());
            }
            bail!("{message}");
        }
        #[cfg(test)]
        if std::env::var_os("RENIUM_DISPATCH_TRACE").is_some() {
            eprintln!(
                "slot={slot:x} rva={rva:x} field={field:?} spans={:?}",
                methods
                    .iter()
                    .map(|(rva, code)| (rva, code.len()))
                    .collect::<Vec<_>>()
            );
        }
        if let Some(field) = field {
            candidates.insert(field);
        }
    }
    anyhow::ensure!(
        terminated && candidates.len() == 1,
        properties::NativeError::Unsupported(format!(
            "Reflection dispatch resolved {} function bindings; no call was executed",
            candidates.len()
        ))
    );
    let result = *candidates.iter().next().unwrap();
    let mut cache = BINDINGS.get().unwrap().lock_recover();
    if cache.len() >= 128 {
        cache.clear();
    }
    cache.insert(
        key,
        Binding {
            field: result,
            table: table_bytes,
            code: methods.into_iter().collect(),
        },
    );
    Ok(result)
}

pub(super) fn void_dispatch_field(code: &[u8]) -> Option<usize> {
    let mut calls = 0;
    let mut inputs = HashSet::from([Register::R8, Register::R9]);
    for i in Decoder::new(64, code, DecoderOptions::NONE) {
        if i.is_invalid() {
            return None;
        }
        if i.flow_control() == FlowControl::IndirectCall {
            calls += 1;
        } else if !matches!(i.flow_control(), FlowControl::Next | FlowControl::Return) {
            return None;
        }
        let mut info = InstructionInfoFactory::new();
        for reg in info.info(&i).used_registers() {
            let register = reg.register().full_register();
            if inputs.contains(&register)
                && matches!(
                    reg.access(),
                    OpAccess::Read
                        | OpAccess::CondRead
                        | OpAccess::ReadWrite
                        | OpAccess::ReadCondWrite
                )
            {
                return None;
            }
            if reg.access() == OpAccess::Write {
                inputs.remove(&register);
            }
        }
    }
    if calls != 1 {
        return None;
    }
    dispatch_field(0, |address| (address == 0).then(|| code.to_vec()))
}

fn leaf_dispatch_length(bytes: &[u8]) -> Option<usize> {
    let mut boundaries = HashSet::new();
    let mut branches = Vec::new();
    for instruction in Decoder::new(64, bytes, DecoderOptions::NONE) {
        if instruction.is_invalid() {
            return None;
        }
        boundaries.insert(instruction.ip());
        match instruction.flow_control() {
            FlowControl::Return
            | FlowControl::Exception
            | FlowControl::Interrupt
            | FlowControl::IndirectBranch
            | FlowControl::UnconditionalBranch => {
                // A leaf null guard may jump over a receiver conversion to
                // this same tail dispatch. Do not cross the first terminal,
                // accept backward edges, or admit a target inside an operand.
                return branches
                    .iter()
                    .all(|target| boundaries.contains(target))
                    .then_some(instruction.next_ip() as usize);
            }
            FlowControl::ConditionalBranch
                if instruction.op0_kind() == OpKind::NearBranch64
                    && instruction.near_branch_target() >= instruction.next_ip() =>
            {
                branches.push(instruction.near_branch_target());
            }
            FlowControl::Next => {}
            _ => return None,
        }
    }
    None
}

pub(in super::super) fn dispatch_code(
    memory: &ProcessMemory,
    studio: &ModuleEntry,
    layout: &PackageLayout,
    address: usize,
) -> Result<Vec<u8>> {
    let rva = address
        .checked_sub(studio.base)
        .context("Invalid reflection dispatch address")?;
    let index = layout
        .function_ranges
        .partition_point(|(start, _)| *start <= rva);
    if let Some((start, end)) = index
        .checked_sub(1)
        .and_then(|index| layout.function_ranges.get(index))
        .filter(|(start, _)| *start == rva)
    {
        let length = end - start;
        anyhow::ensure!(
            length <= 65536,
            properties::NativeError::Unsupported(
                "Reflection dispatch exceeds its analysis budget".into()
            )
        );
        return verified_code(memory, studio, layout, address, length);
    }
    let bytes = verified_code(memory, studio, layout, address, 256)?;
    if let Some(length) = leaf_dispatch_length(&bytes) {
        return Ok(bytes[..length].to_vec());
    }
    bail!(properties::NativeError::Unsupported(
        "Reflection dispatch has no verified function boundary".into()
    ))
}

fn resolve(
    memory: &ProcessMemory,
    studio: &ModuleEntry,
    layout: &PackageLayout,
    descriptor: usize,
    field: usize,
) -> Result<(usize, usize)> {
    let function = memory.read_u64(descriptor + field)? as usize;
    anyhow::ensure!(
        memory.read_u32(descriptor + field + 8)? == 0,
        properties::NativeError::Unsupported(
            "Function receiver requires an unsupported adjustment".into()
        )
    );
    verified_code(memory, studio, layout, function, 64)?;
    Ok((field, function))
}

#[cfg(test)]
mod tests {
    #[test]
    fn custom_lua_callback_requires_the_original_lua_state() {
        for field in [0xa8i32, 0x300, 0x2000] {
            let mut code = vec![0x48, 0x8b, 0x81];
            code.extend(field.to_le_bytes());
            code.extend([0x48, 0x63, 0x89]);
            code.extend((field + 8).to_le_bytes());
            code.extend([0x48, 0x01, 0xd1, 0x4c, 0x89, 0xc2, 0xff, 0xe0]);
            assert_eq!(
                dispatch_field_for(0, |_| Some(code.clone()), true),
                Some(field as usize)
            );
            // An output buffer substituted for the Lua state must not bind.
            let position = code.len() - 5;
            code.splice(position..position + 3, [0x48, 0x89, 0xe2]);
            assert_eq!(dispatch_field_for(0, |_| Some(code.clone()), true), None);
        }
    }
    #[test]
    fn leaf_dispatch_null_guards_stay_within_verified_instruction_boundaries() {
        let code = [
            0x48, 0x85, 0xc0, // TEST RAX,RAX
            0x74, 0x07, // JE to the tail dispatch
            0x48, 0x8d, 0x88, 0x20, 0x04, 0x00, 0x00, 0x41, 0xff, 0x60, 0x08, // JMP [R8+8]
            0xcc,
        ];
        assert_eq!(leaf_dispatch_length(&code), Some(16));
        for target in [0u8, 1, 6, 8, 0x7f, 0xfb] {
            let mut broken = code;
            broken[4] = target;
            // A zero-length conditional edge remains a valid boundary.
            assert_eq!(leaf_dispatch_length(&broken), (target == 0).then_some(16));
        }
        assert_eq!(leaf_dispatch_length(&[0xe8, 0, 0, 0, 0, 0xc3]), None);
        assert_eq!(leaf_dispatch_length(&[0x90, 0x0f]), None);
        assert_eq!(leaf_dispatch_length(&[0x74, 1, 0xc3, 0xc3]), None);
        // MSVC can put a terminating trap just beyond a .pdata range. It is
        // its own verified fragment, never permission to read the next function.
        assert_eq!(leaf_dispatch_length(&[0xcc, 0xc3]), Some(1));
        assert_eq!(leaf_dispatch_length(&[0x74, 1, 0xcc, 0xc3]), None);
    }

    use super::*;

    #[test]
    fn void_dispatch_preserves_virtual_base_adjustment() {
        let template = vec![
            0x48, 0x83, 0xec, 0x28, 0xf, 0x10, 0x89, 0x90, 0x0, 0x0, 0x0, 0x66, 0xf, 0x6f, 0xc1,
            0x66, 0xf, 0x73, 0xd8, 0x8, 0x66, 0x49, 0xf, 0x7e, 0xc1, 0x49, 0x8b, 0xc1, 0x48, 0xc1,
            0xe8, 0x20, 0x48, 0x63, 0xc8, 0x48, 0x8b, 0x82, 0xd0, 0x0, 0x0, 0x0, 0x48, 0x63, 0x4,
            0x1, 0x48, 0x3, 0xc2, 0x49, 0x63, 0xc9, 0x48, 0x81, 0xc1, 0xd0, 0x0, 0x0, 0x0, 0x48,
            0x3, 0xc8, 0x66, 0x48, 0xf, 0x7e, 0xc8, 0xff, 0xd0, 0x33, 0xc0, 0x48, 0x83, 0xc4, 0x28,
            0xc3,
        ];
        for field in [0x90i32, 0x300, 0x2000] {
            for vbtable in [0xd0i32, 0x180, 0x400] {
                let mut code = template.clone();
                code[7..11].copy_from_slice(&field.to_le_bytes());
                code[38..42].copy_from_slice(&vbtable.to_le_bytes());
                code[55..59].copy_from_slice(&vbtable.to_le_bytes());
                assert_eq!(void_dispatch_field(&code), Some(field as usize));
                // A mismatched adjustment must not certify an instance receiver.
                code[55..59].copy_from_slice(&(vbtable + 8).to_le_bytes());
                assert_eq!(void_dispatch_field(&code), None);
            }
        }
    }

    fn scalar(field: i32, adjustment: i32) -> Vec<u8> {
        let mut code = vec![0x49, 0x89, 0xcb, 0x48, 0x63, 0x89];
        code.extend_from_slice(&adjustment.to_le_bytes());
        code.extend_from_slice(&[0x48, 0x01, 0xd1, 0x49, 0x8b, 0x83]);
        code.extend_from_slice(&field.to_le_bytes());
        code.extend_from_slice(&[0xff, 0xd0, 0xc3]);
        code
    }

    fn vector(field: i32) -> Vec<u8> {
        let mut code = vec![
            0x49, 0x89, 0xca, 0x49, 0x89, 0xd3, 0x48, 0x83, 0xec, 0x38, 0x41, 0x0f, 0x10, 0x82,
        ];
        code.extend_from_slice(&field.to_le_bytes());
        code.extend_from_slice(&[
            0x0f, 0x11, 0x44, 0x24, 0x20, 0x66, 0x0f, 0x73, 0xd8, 8, 0x66, 0x0f, 0x7e, 0xc1, 0x48,
            0x63, 0xc9, 0x4c, 0x01, 0xd9, 0x48, 0x8b, 0x44, 0x24, 0x20, 0xff, 0xd0, 0x48, 0x83,
            0xc4, 0x38, 0xc3,
        ]);
        code
    }

    fn inline(code: &[u8]) -> Option<usize> {
        dispatch_field(0, |address| (address == 0).then(|| code.to_vec()))
    }

    #[test]
    fn relocated_scalar_and_vector_member_bindings() {
        for field in [8, 0x78, 0x80, 0x2f0, 0x2000] {
            assert_eq!(inline(&scalar(field, field + 8)), Some(field as usize));
            assert_eq!(inline(&vector(field)), Some(field as usize));
        }
    }

    #[test]
    fn complete_receiver_binding_requires_the_selected_instance_conversion() {
        for offset in [32i32, 160, 640] {
            let mut code = vec![0x48, 0x85, 0xd2, 0x48, 0x8d, 0x92]; // test rdx,rdx; lea rdx,[rdx-offset]
            code.extend((-offset).to_le_bytes());
            code.extend([0x49, 0x0f, 0x44, 0xd0]); // cmove rdx,r8 (known not taken)
            code.extend(scalar(0x78, 0x80));
            let check = |bytes: &[u8], selected| {
                dispatch_field_fetch(
                    0,
                    &mut |at, _| (at == 0).then(|| bytes.to_vec()),
                    false,
                    selected,
                    &mut |_, _| true,
                )
            };
            assert_eq!(check(&code, offset as usize), Some(0x78));
            assert_eq!(check(&code, offset as usize + 8), None);
            code[2] = 0xc0; // test rax,rax cannot prove the null guard is not taken.
            assert_eq!(check(&code, offset as usize), None);
        }
    }

    #[test]
    fn wrong_receiver_adjustment_width_and_overwritten_spills_are_rejected() {
        assert_eq!(inline(&scalar(0x78, 0x88)), None);
        let mut code = scalar(0x78, 0x80);
        code[10..13].copy_from_slice(&[0x4c, 0x01, 0xd9]);
        assert_eq!(inline(&code), None);
        let mut code = scalar(0x78, 0x80);
        code[3] = 0x40;
        assert_eq!(inline(&code), None);
        let mut code = vector(0x300);
        code.splice(40..40, [0xc7, 0x44, 0x24, 0x20, 0, 0, 0, 0]);
        assert_eq!(inline(&code), None);
    }

    #[test]
    fn dispatch_can_move_to_an_outlined_or_tail_called_helper() {
        for entry in [vec![0xe8, 0xfb, 0, 0, 0, 0xc3], vec![0xe9, 0xfb, 0, 0, 0]] {
            assert_eq!(
                dispatch_field(0, |address| match address {
                    0 => Some(entry.clone()),
                    256 => Some(scalar(0x300, 0x308)),
                    _ => None,
                }),
                Some(0x300)
            );
            assert_eq!(
                dispatch_field(0, |address| (address == 0).then(|| entry.clone())),
                None
            );
        }
    }

    #[test]
    fn unreachable_adjacent_code_and_ambiguous_control_flow_are_not_bindings() {
        let mut code = vec![0xc3];
        code.extend(scalar(0x78, 0x80));
        assert_eq!(inline(&code), None);
        code[0] = 0xcc;
        assert_eq!(inline(&code), None);
        let good = scalar(0x78, 0x80);
        let bad = scalar(0x80, 0x88);
        let mut code = vec![0x74, good.len() as u8];
        code.extend(good);
        code.extend(bad);
        assert_eq!(inline(&code), None);
        assert_eq!(inline(&[0xeb, 0xff]), None);
        assert_eq!(inline(&[0xeb, 0xfe]), None);
        let good = scalar(0x78, 0x80);
        let mut code = vec![0x74, good.len() as u8];
        code.extend(good);
        code.push(0xcc);
        assert_eq!(inline(&code), Some(0x78));
    }

    #[test]
    #[ignore]
    fn captured_dispatch() -> Result<()> {
        let records: serde_json::Value =
            serde_json::from_slice(&fs::read(std::env::var("RENIUM_FUNCTION_INSPECT_OUT")?)?)?;
        for record in records.as_array().unwrap() {
            let mut fields = HashSet::new();
            for method in record["methods"].as_array().unwrap() {
                let code = method["code"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|n| n.as_u64().unwrap() as u8)
                    .collect::<Vec<_>>();
                if let Some(field) = dispatch_field(0, |address| {
                    code.get(address as usize..).map(|bytes| bytes.to_vec())
                }) {
                    fields.insert(field);
                }
            }
            assert_eq!(fields.len(), 1, "{}: {fields:?}", record["name"]);
        }
        Ok(())
    }
}
