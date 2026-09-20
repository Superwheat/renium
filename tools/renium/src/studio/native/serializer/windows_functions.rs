use super::*;
use std::collections::{BTreeMap, VecDeque};

#[derive(Clone)]
struct Binding {
    field: usize,
    table: Vec<u8>,
    code: Vec<(usize, Vec<u8>)>,
}

type BindingKey = ([u32; 3], usize);
static BINDINGS: OnceLock<Mutex<HashMap<BindingKey, Binding>>> = OnceLock::new();

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Storage {
    Descriptor,
    Stack,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Value {
    Pointer(Storage, i64),
    Bytes(Storage, i64, usize),
    Instance,
    Adjustment(i64),
    AdjustedInstance(i64),
}

impl Value {
    fn slice(self, offset: usize, width: usize) -> Option<Self> {
        match self {
            Self::Bytes(storage, start, size) if offset.checked_add(width)? <= size => {
                Some(Self::Bytes(storage, start + offset as i64, width))
            }
            _ if offset == 0 && width == 8 => Some(self),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct State {
    registers: HashMap<Register, Value>,
    stack: BTreeMap<i64, (usize, Value)>,
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
                let (storage, offset) = self.address(i)?;
                match storage {
                    Storage::Descriptor if (8..=65520).contains(&offset) => {
                        Some(Value::Bytes(storage, offset, width))
                    }
                    Storage::Stack => {
                        let (start, (size, value)) = self.stack.range(..=offset).next_back()?;
                        let relative = usize::try_from(offset - start).ok()?;
                        (relative.checked_add(width)? <= *size)
                            .then(|| value.slice(relative, width))
                            .flatten()
                    }
                    _ => None,
                }
            }
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
        self.registers
            .retain(|key, value| other.registers.get(key) == Some(value));
        self.stack
            .retain(|key, value| other.stack.get(key) == Some(value));
        *self != before
    }
    fn called_field(&self, i: &Instruction) -> Option<usize> {
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
        values.contains(&Value::Instance)
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

    fn step(&mut self, i: &Instruction) {
        let dst = i.op0_register().full_register();
        let width = if i.op0_kind() == OpKind::Memory {
            i.memory_size().size()
        } else {
            i.op0_register().size()
        };
        match i.mnemonic() {
            Mnemonic::Mov
            | Mnemonic::Movaps
            | Mnemonic::Movups
            | Mnemonic::Movdqa
            | Mnemonic::Movdqu
            | Mnemonic::Movd
            | Mnemonic::Movq
            | Mnemonic::Movsxd => {
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
                } else if let Some((Storage::Stack, offset)) = self.address(i) {
                    self.store(offset, width, value);
                }
            }
            Mnemonic::Lea => {
                let value = self
                    .address(i)
                    .filter(|_| width == 8)
                    .map(|(storage, offset)| Value::Pointer(storage, offset));
                self.registers.remove(&dst);
                if let Some(value) = value {
                    self.registers.insert(dst, value);
                }
            }
            Mnemonic::Add | Mnemonic::Sub if i.op0_kind() == OpKind::Register && width == 8 => {
                let left = self.register(dst);
                let right = self.read(i, 1, 8);
                let value = match (left, right, i.mnemonic()) {
                    (Some(Value::Adjustment(offset)), Some(Value::Instance), Mnemonic::Add)
                    | (Some(Value::Instance), Some(Value::Adjustment(offset)), Mnemonic::Add) => {
                        Some(Value::AdjustedInstance(offset))
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
                };
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
            _ => {
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
                    && let Some((Storage::Stack, offset)) = self.address(i)
                {
                    self.store(offset, i.memory_size().size(), None);
                }
            }
        }
    }
}

fn dispatch_fields(
    entry: u64,
    initial: State,
    code: &mut impl FnMut(u64) -> Option<Vec<u8>>,
    budget: &mut usize,
    depth: usize,
) -> Option<HashSet<usize>> {
    if depth > 8 {
        return None;
    }
    let bytes = code(entry)?;
    if bytes.len() > 65536 {
        return None;
    }
    let instructions = Decoder::with_ip(64, &bytes, entry, DecoderOptions::NONE)
        .into_iter()
        .map(|i| (i.ip(), i))
        .collect::<HashMap<_, _>>();
    let mut states = HashMap::from([(entry, initial)]);
    let mut queue = VecDeque::from([entry]);
    while let Some(pc) = queue.pop_front() {
        *budget = budget.checked_sub(1)?;
        let i = instructions.get(&pc)?;
        if i.is_invalid() {
            return None;
        }
        let mut state = states[&pc].clone();
        state.step(i);
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
            if !instructions.contains_key(&next) {
                return None;
            }
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
    let mut fields = HashSet::new();
    for (pc, mut state) in states {
        let i = &instructions[&pc];
        if matches!(
            i.flow_control(),
            FlowControl::IndirectCall | FlowControl::IndirectBranch
        ) {
            fields.extend(state.called_field(i));
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
            )?);
        }
    }
    Some(fields)
}

fn dispatch_field(entry: u64, mut code: impl FnMut(u64) -> Option<Vec<u8>>) -> Option<usize> {
    let initial = State {
        registers: HashMap::from([
            (Register::RCX, Value::Pointer(Storage::Descriptor, 0)),
            (Register::RDX, Value::Instance),
            (Register::RSP, Value::Pointer(Storage::Stack, 0)),
        ]),
        stack: BTreeMap::new(),
    };
    let fields = dispatch_fields(entry, initial, &mut code, &mut 32768, 0)?;
    (fields.len() == 1).then(|| *fields.iter().next().unwrap())
}

pub(super) fn binding(
    memory: &ProcessMemory,
    studio: &ModuleEntry,
    layout: &PackageLayout,
    descriptor: usize,
    table: usize,
) -> Result<(usize, usize)> {
    let key = (layout.image_stamp, table.wrapping_sub(studio.base));
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
        return resolve(memory, studio, layout, descriptor, binding.field);
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
        let field = dispatch_field(method as u64, |address| {
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
                    error = Some(format!("{message:#}"));
                    None
                }
            }
        });
        if let Some(error) = error {
            bail!("Could not verify reflection dispatch: {error}");
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
            candidates.insert(resolve(memory, studio, layout, descriptor, field)?);
        }
    }
    anyhow::ensure!(
        terminated && candidates.len() == 1,
        "Reflection dispatch resolved {} function bindings; no call was executed",
        candidates.len()
    );
    let result = *candidates.iter().next().unwrap();
    let mut cache = BINDINGS.get().unwrap().lock_recover();
    if cache.len() >= 128 {
        cache.clear();
    }
    cache.insert(
        key,
        Binding {
            field: result.0,
            table: table_bytes,
            code: methods.into_iter().collect(),
        },
    );
    Ok(result)
}

fn dispatch_code(
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
            "Reflection dispatch exceeds its analysis budget"
        );
        return verified_code(memory, studio, layout, address, length);
    }
    let bytes = verified_code(memory, studio, layout, address, 256)?;
    for instruction in Decoder::new(64, &bytes, DecoderOptions::NONE) {
        match instruction.flow_control() {
            FlowControl::Return
            | FlowControl::IndirectBranch
            | FlowControl::UnconditionalBranch => {
                return Ok(bytes[..instruction.next_ip() as usize].to_vec());
            }
            FlowControl::Next => {}
            _ => break,
        }
    }
    bail!("Reflection dispatch has no verified function boundary")
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
        "Function receiver requires an unsupported adjustment"
    );
    verified_code(memory, studio, layout, function, 64)?;
    Ok((field, function))
}

#[cfg(test)]
mod tests {
    use super::*;

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
