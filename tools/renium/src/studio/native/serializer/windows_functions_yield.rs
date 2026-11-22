//! BoundYieldFuncDesc's RTTI describes the public arguments, not its hidden
//! continuations. Qualify BOTH actual callbacks at the member dispatch.
//! Bound operation tables and incoming native slots are checked on every image.
//! Track the actual continuations through verified copies, rather than accepting
//! any reachable consumer. Both invocation paths must construct an owned byte
//! string through verified byte-copy imports. Nominal C++ type names and HTTP
//! semantics are not encoded in machine code; reads remain guarded and the
//! helper never destroys an engine payload.
use super::*;
#[path = "windows_functions_arity.rs"]
mod arity;
#[path = "windows_functions_captures.rs"]
pub(super) mod captures;
#[path = "windows_functions_strings.rs"]
mod strings;

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize)]
#[repr(u32)]
pub(in super::super::super) enum ContinuationAbi {
    Msvc = 1,
    Tagged = 2,
}

// Execute only the inline/nontrivial path of the helper's callable contract in
// a tiny symbolic address space. These are helper ABI offsets, not engine types.
// Unknown branches/instructions/calls refuse; no machine code is executed.
#[derive(Default)]
struct CallableState {
    regs: HashMap<Register, u64>,
    memory: HashMap<u64, u64>,
    zero: Option<bool>,
}
impl CallableState {
    fn reg(&self, r: Register) -> Option<u64> {
        let value = *self.regs.get(&r.full_register())?;
        Some(value & Self::mask(r.size()))
    }
    fn mask(size: usize) -> u64 {
        u64::MAX
            .checked_shr((64 - size.min(8) * 8) as u32)
            .unwrap_or(0)
    }
    fn address(&self, i: &Instruction) -> Option<u64> {
        (i.memory_index() == Register::None).then_some(())?;
        Some(
            self.reg(i.memory_base())?
                .wrapping_add(i.memory_displacement64()),
        )
    }
    fn value(&self, i: &Instruction, op: u32) -> Option<u64> {
        match i.op_kind(op) {
            OpKind::Register => self.reg(i.op_register(op)),
            OpKind::Memory => {
                Some(*self.memory.get(&self.address(i)?)? & Self::mask(i.memory_size().size()))
            }
            OpKind::Immediate8
            | OpKind::Immediate16
            | OpKind::Immediate32
            | OpKind::Immediate64
            | OpKind::Immediate8to16
            | OpKind::Immediate8to32
            | OpKind::Immediate8to64
            | OpKind::Immediate32to64 => Some(i.immediate(op)),
            _ => None,
        }
    }
    fn set_reg(&mut self, r: Register, value: Option<u64>) {
        let full = r.full_register();
        let value = value.and_then(|v| match r.size() {
            8 => Some(v),
            4 => Some(v & u32::MAX as u64),
            size => Some((self.regs.get(&full)? & !Self::mask(size)) | (v & Self::mask(size))),
        });
        self.regs.remove(&full);
        if let Some(value) = value {
            self.regs.insert(full, value);
        }
    }
    fn write(&mut self, i: &Instruction, value: Option<u64>) {
        if i.op0_kind() == OpKind::Register {
            self.set_reg(i.op0_register(), value);
        } else if let Some(at) = self.address(i) {
            self.memory.retain(|start, _| {
                start.saturating_add(8) <= at
                    || *start >= at.saturating_add(i.memory_size().size() as u64)
            });
            if i.memory_size().size() == 8
                && let Some(value) = value
            {
                self.memory.insert(at, value);
            }
        }
    }
    fn arithmetic(&mut self, i: &Instruction) -> Option<()> {
        let a = self.value(i, 0);
        let b = self.value(i, 1);
        let value = match i.mnemonic() {
            Mnemonic::Not => a.map(|a| !a),
            Mnemonic::Xor
                if i.op0_kind() == OpKind::Register
                    && i.op1_kind() == OpKind::Register
                    && i.op0_register() == i.op1_register() =>
            {
                Some(0)
            }
            _ => a.zip(b).map(|(a, b)| match i.mnemonic() {
                Mnemonic::And | Mnemonic::Test => a & b,
                Mnemonic::Or => a | b,
                Mnemonic::Xor => a ^ b,
                Mnemonic::Add => a.wrapping_add(b),
                Mnemonic::Sub | Mnemonic::Cmp => a.wrapping_sub(b),
                Mnemonic::Shr | Mnemonic::Sar => a >> (b & 63), // Only tested low tag bits matter.
                _ => unreachable!(),
            }),
        };
        if i.rflags_modified() != 0 {
            self.zero = value
                .map(|v| v & Self::mask(i.op0_register().size().max(i.memory_size().size())) == 0);
        }
        if !matches!(i.mnemonic(), Mnemonic::Cmp | Mnemonic::Test) {
            self.write(i, value);
        }
        Some(())
    }
    fn step(&mut self, i: &Instruction) -> Option<()> {
        if (0..i.op_count()).any(|op| {
            matches!(
                i.op_register(op),
                Register::AH | Register::BH | Register::CH | Register::DH
            )
        }) {
            return None;
        }
        match i.mnemonic() {
            Mnemonic::Mov | Mnemonic::Movzx => self.write(i, self.value(i, 1)),
            Mnemonic::Lea => self.write(i, self.address(i)),
            Mnemonic::Push => {
                let at = self.reg(Register::RSP)?.wrapping_sub(8);
                self.regs.insert(Register::RSP, at);
                self.memory.remove(&at);
                if let Some(value) = self.value(i, 0) {
                    self.memory.insert(at, value);
                }
            }
            Mnemonic::Pop => {
                let at = self.reg(Register::RSP)?;
                let value = self.memory.remove(&at);
                self.set_reg(i.op0_register(), value);
                self.regs.insert(Register::RSP, at + 8);
            }
            Mnemonic::Nop => {}
            Mnemonic::Not
            | Mnemonic::Xor
            | Mnemonic::Or
            | Mnemonic::And
            | Mnemonic::Test
            | Mnemonic::Cmp
            | Mnemonic::Add
            | Mnemonic::Sub
            | Mnemonic::Shr
            | Mnemonic::Sar => return self.arithmetic(i),
            _ => return None,
        }
        Some(())
    }
    fn next(&mut self, i: &Instruction) -> Option<u64> {
        match i.flow_control() {
            FlowControl::Next => self.step(i)?,
            FlowControl::UnconditionalBranch => return Some(i.near_branch_target()),
            FlowControl::ConditionalBranch => match (i.mnemonic(), self.zero) {
                (Mnemonic::Je, Some(true)) | (Mnemonic::Jne, Some(false)) => {
                    return Some(i.near_branch_target());
                }
                (Mnemonic::Je, Some(false)) | (Mnemonic::Jne, Some(true)) => {}
                _ => return None,
            },
            _ => return None,
        }
        Some(i.next_ip())
    }
    fn called(&mut self) {
        self.zero = None;
        for r in [
            Register::RAX,
            Register::RCX,
            Register::RDX,
            Register::R8,
            Register::R9,
            Register::R10,
            Register::R11,
        ] {
            self.regs.remove(&r);
        }
    }
}

fn callable_consumer(bytes: &[u8], entry: u64, role: u64) -> bool {
    let mut s = CallableState::default();
    s.regs.extend([
        (Register::RCX, if role == 1 { 0x10000 } else { 0x20000 }),
        (Register::RDX, if role == 1 { 0x20000 } else { 0x30000 }),
        (Register::RSP, 0x100000),
    ]);
    s.memory.extend([
        (0x20018, 0x80006),
        (0x10018, 0),
        (0x80000 + role * 8, 0x90000 + role),
    ]);
    let code: HashMap<_, _> = Decoder::with_ip(64, bytes, entry, DecoderOptions::NONE)
        .into_iter()
        .map(|i| (i.ip(), i))
        .collect();
    let mut pc = entry;
    let mut called = false;
    for _ in 0..256 {
        let Some(i) = code.get(&pc).filter(|i| !i.is_invalid()) else {
            return false;
        };
        pc = i.next_ip();
        match i.flow_control() {
            FlowControl::Return => {
                return called
                    && s.memory.get(&(if role == 1 { 0x10018 } else { 0x20018 }))
                        == Some(&(if role == 1 { 0x80006 } else { 0 }));
            }
            FlowControl::IndirectCall => {
                if called
                    || s.value(i, 0) != Some(0x90000 + role)
                    || s.reg(Register::RCX) != Some(if role == 1 { 0x10000 } else { 0x20000 })
                    || (role != 3
                        && s.reg(Register::RDX) != Some(if role == 1 { 0x20000 } else { 0x30000 }))
                {
                    return false;
                }
                called = true;
                s.called();
            }
            FlowControl::Call if role == 0 && called => {
                // The invocation result must not feed cleanup: the engine
                // passes the original payload to its own cleanup routine.
                // This is ownership evidence, not a proof of its C++ type.
                return s.reg(Register::RCX) == Some(0x30000);
            }
            FlowControl::Next if role == 0 && called => {
                // Do not accept a changed consumer that uses the callback's
                // return value. Only known register transfers may precede cleanup.
                if i.mnemonic() != Mnemonic::Nop
                    && (i.op0_kind() != OpKind::Register || s.value(i, 1).is_none())
                {
                    return false;
                }
                if s.step(i).is_none() {
                    return false;
                }
            }
            _ => match s.next(i) {
                Some(next) => pc = next,
                None => return false,
            },
        }
    }
    false
}

// Qualify copy-assignment's move into an empty destination: a verified copy
// produces its temporary, slot 2 moves that capture, slot 3 destroys the moved
// source, and the caller clears its tag. Do not infer this from the other slots.
fn callable_move(bytes: &[u8], entry: u64, code: &mut impl FnMut(u64) -> Option<Vec<u8>>) -> bool {
    let mut s = CallableState::default();
    s.regs.extend([
        (Register::RCX, 0x10000),
        (Register::RDX, 0x20000),
        (Register::RSP, 0x100000),
    ]);
    s.memory.extend([
        (0x10018, 0),
        (0x20018, 0x80006),
        (0x80010, 0x90002),
        (0x80018, 0x90003),
    ]);
    let instructions: HashMap<_, _> = Decoder::with_ip(64, bytes, entry, DecoderOptions::NONE)
        .into_iter()
        .map(|i| (i.ip(), i))
        .collect();
    let mut pc = entry;
    let mut temporary = None;
    let mut moved = false;
    let mut destroyed = false;
    for _ in 0..256 {
        let Some(i) = instructions.get(&pc).filter(|i| !i.is_invalid()) else {
            return false;
        };
        pc = i.next_ip();
        match i.flow_control() {
            FlowControl::Call => {
                let Some(to) = s
                    .reg(Register::RCX)
                    .filter(|at| (0xf0000..0x100000).contains(at))
                else {
                    return false;
                };
                if temporary.is_some()
                    || s.reg(Register::RDX) != Some(0x20000)
                    || !code(i.near_branch_target())
                        .is_some_and(|bytes| callable_consumer(&bytes, i.near_branch_target(), 1))
                {
                    return false;
                }
                s.memory.insert(to + 24, 0x80006);
                temporary = Some(to);
                s.called();
            }
            FlowControl::IndirectCall => {
                let valid = if !moved {
                    s.value(i, 0) == Some(0x90002)
                        && s.reg(Register::RCX) == Some(0x10000)
                        && temporary.is_some()
                        && s.reg(Register::RDX) == temporary
                } else {
                    !destroyed
                        && s.value(i, 0) == Some(0x90003)
                        && s.reg(Register::RCX) == temporary
                };
                if !valid {
                    return false;
                }
                destroyed = moved;
                moved = true;
                s.called();
            }
            _ => match s.next(i) {
                Some(next) => pc = next,
                None => return false,
            },
        }
        if destroyed
            && temporary.is_some_and(|at| s.memory.get(&(at + 24)) == Some(&0))
            && s.memory.get(&0x10018) == Some(&0x80006)
        {
            return true;
        }
    }
    false
}

fn direct_move(bytes: &[u8], entry: u64) -> bool {
    let mut s = CallableState::default();
    s.regs.extend([
        (Register::RCX, 0x10000),
        (Register::RDX, 0x20000),
        (Register::RSP, 0x100000),
    ]);
    s.memory.extend([
        (0x10018, 0),
        (0x20018, 0x80006),
        (0x80010, 0x90002),
        (0x80018, 0x90003),
    ]);
    let instructions: HashMap<_, _> = Decoder::with_ip(64, bytes, entry, DecoderOptions::NONE)
        .into_iter()
        .map(|i| (i.ip(), i))
        .collect();
    let mut pc = entry;
    let mut moved = false;
    let mut destroyed = false;
    for _ in 0..256 {
        let Some(i) = instructions.get(&pc) else {
            return false;
        };
        match i.flow_control() {
            FlowControl::IndirectCall => {
                let valid = if !moved {
                    s.value(i, 0) == Some(0x90002)
                        && s.reg(Register::RCX) == Some(0x10000)
                        && s.reg(Register::RDX) == Some(0x20000)
                } else {
                    !destroyed
                        && s.value(i, 0) == Some(0x90003)
                        && s.reg(Register::RCX) == Some(0x20000)
                };
                if !valid {
                    return false;
                }
                destroyed = moved;
                moved = true;
                s.called();
                pc = i.next_ip();
            }
            FlowControl::Return => {
                return destroyed
                    && s.memory.get(&0x10018) == Some(&0x80006)
                    && s.memory.get(&0x20018) == Some(&0);
            }
            _ => {
                let Some(next) = s.next(i) else {
                    return false;
                };
                pc = next;
            }
        }
    }
    false
}

fn tagged_table(state: &State, argument: usize) -> Option<u64> {
    let object = callback_object(state, argument)?;
    match state.stack.get(&(object + 24)) {
        Some((8, Value::TaggedPointer(table, 6))) => Some(*table),
        _ => None,
    }
}

fn incoming_ownership(
    entry: u64,
    argument: usize,
    code: &mut impl FnMut(u64) -> Option<Vec<u8>>,
) -> Result<()> {
    let initial = State {
        registers: HashMap::from([(Register::RSP, Value::Pointer(Storage::Stack, 0))]),
        stack: BTreeMap::from([
            (
                ((argument + 1) * 8) as i64,
                (8, Value::Pointer(Storage::Descriptor, 4096)),
            ),
            (
                ((argument + 2) * 8) as i64,
                (8, Value::Pointer(Storage::Descriptor, 8192)),
            ),
        ]),
        lua_callback: false,
        zero: None,
        complete_receiver: false,
    };
    let analysis = flow(entry, initial, &mut |at, _| code(at), &mut 32768)
        .context("yield-callback-ownership: incomplete member control flow")?;
    verify_arity(entry, &analysis, argument + 2)?;
    let mut evidence = HashSet::new();
    for (pc, state) in analysis.states {
        let instruction = &analysis.instructions[&pc];
        if instruction.flow_control() != FlowControl::Call {
            continue;
        }
        for (role, register) in [(1, Register::RDX), (3, Register::RCX)] {
            if let Some(Value::Pointer(Storage::Descriptor, origin @ (4096 | 8192))) =
                state.register(register)
                && let Some(bytes) = code(instruction.near_branch_target())
                && (callable_consumer(&bytes, instruction.near_branch_target(), role)
                    // A member that takes its continuation moves it out of
                    // the argument instead of copying it.
                    || role == 1 && direct_move(&bytes, instruction.near_branch_target()))
            {
                evidence.insert((origin, role));
            }
        }
    }
    let missing: Vec<_> = [
        (4096, 1, "success.copy"),
        (4096, 3, "success.destroy"),
        (8192, 1, "error.copy"),
        (8192, 3, "error.destroy"),
    ]
    .into_iter()
    .filter_map(|(origin, role, name)| (!evidence.contains(&(origin, role))).then_some(name))
    .collect();
    anyhow::ensure!(
        missing.is_empty(),
        "yield-callback-ownership: unproved {missing:?}"
    );
    Ok(())
}

/// A yielding member as the verifier needs it: the place of its success
/// continuation among its arguments, the receiver counted, and the offset of
/// the instance inside the receiver where the member is not the instance's
/// own class's.
#[derive(Clone, Copy)]
pub(in super::super::super) struct Member {
    argument: usize,
    receiver: Option<usize>,
    /// The member's function takes the converted receiver as it is.
    unadjusted: bool,
}

impl Member {
    /// The member a request of `mode` calls.
    pub(in super::super::super) fn of(mode: u32, receiver: Option<usize>) -> Self {
        let argument = match mode {
            1 => 4,
            5 => 5,
            _ => 6,
        };
        Self {
            argument,
            receiver,
            unadjusted: mode == 5,
        }
    }
}

pub(super) fn qualify(
    member: Member,
    field: usize,
    entry: u64,
    methods: &[u64],
    code: &mut impl FnMut(u64) -> Option<Vec<u8>>,
    operation: &mut impl FnMut(u64) -> Option<u64>,
    byte_copy: &mut impl FnMut(u64) -> bool,
) -> Result<YieldContract> {
    let argument = member.argument;
    let mut proved = false;
    let mut tables = HashSet::new();
    for &method in methods {
        let result = dispatch_field_fetch(
            method,
            &mut |at, _| code(at),
            false,
            member.receiver,
            &mut |candidate, state| {
                if candidate != field || state.unadjusted_receiver() != member.unadjusted {
                    return false;
                }
                let (Some(success), Some(error)) = (
                    tagged_table(state, argument),
                    tagged_table(state, argument + 1),
                ) else {
                    return false;
                };
                tables.extend([success, error]);
                true
            },
        );
        proved |= result == Some(field);
    }
    anyhow::ensure!(
        proved,
        "yield-callback-construction: both hidden tagged continuations were not proved"
    );
    bound_invocations(tables, code, operation)?;
    incoming_ownership(entry, argument, code)?;
    traced_consumers(entry, argument, code, operation, byte_copy)
}

// These are the operation tables constructed for THIS member by reflection,
// not an arbitrary callable found near it. Verify both input footprints too;
// a changed hidden return buffer or extra callback input consumes another slot.
fn bound_invocations(
    tables: HashSet<u64>,
    code: &mut impl FnMut(u64) -> Option<Vec<u8>>,
    operation: &mut impl FnMut(u64) -> Option<u64>,
) -> Result<()> {
    for table in tables {
        let invoke = operation(table)
            .context("yield-callback-bound-invoke: missing actual callback operation")?;
        let initial = State {
            registers: HashMap::from([
                (Register::RSP, Value::Pointer(Storage::Stack, 0)),
                (Register::R8, Value::Continuation(2)),
                (Register::R9, Value::Continuation(3)),
                (Register::XMM0.full_register(), Value::Continuation(2)),
                (Register::XMM1.full_register(), Value::Continuation(2)),
                (Register::XMM2.full_register(), Value::Continuation(2)),
                (Register::XMM3.full_register(), Value::Continuation(2)),
            ]),
            stack: BTreeMap::new(),
            lua_callback: false,
            zero: None,
            complete_receiver: false,
        };
        let analysis = flow(invoke, initial, &mut |at, _| code(at), &mut 32768)
            .context("yield-callback-bound-invoke: incomplete actual callback control flow")?;
        verify_arity(invoke, &analysis, 2).context("yield-callback-bound-invoke")?;
    }
    Ok(())
}

fn callback_origin(state: &State, value: Value) -> Option<u8> {
    match value {
        Value::Continuation(origin) => Some(origin),
        Value::Pointer(Storage::Stack | Storage::Capture, at) => match state.stack.get(&at) {
            Some((32, Value::Continuation(origin))) => Some(*origin),
            _ => None,
        },
        _ => None,
    }
}

pub(super) fn copied_origin(
    state: &State,
    entry: u64,
    code: &mut impl FnMut(u64) -> Option<Vec<u8>>,
) -> Option<(i64, u8, bool)> {
    let origin = callback_origin(state, state.register(Register::RDX)?)?;
    let Value::Pointer(Storage::Stack | Storage::Capture, to) = state.register(Register::RCX)?
    else {
        return None;
    };
    let bytes = code(entry)?;
    let preserves = callable_consumer(&bytes, entry, 1) || callable_move(&bytes, entry, code);
    (preserves || direct_move(&bytes, entry)).then_some((to, origin, preserves))
}

// Incoming stack bytes beyond the last supplied argument must never be used.
// Count includes `this`; x64's return address and four home slots are ABI data.
fn verify_arity(entry: u64, analysis: &Flow, count: usize) -> Result<()> {
    for (pc, state) in &analysis.states {
        let i = &analysis.instructions[pc];
        anyhow::ensure!(
            state.register(Register::RSP).is_some(),
            "yield-native-arity: lost stack provenance"
        );
        for reg in InstructionInfoFactory::new().info(i).used_registers() {
            if matches!(
                reg.access(),
                OpAccess::Read | OpAccess::CondRead | OpAccess::ReadWrite | OpAccess::ReadCondWrite
            ) {
                anyhow::ensure!(
                    !matches!(
                        state.register(reg.register()),
                        Some(Value::Continuation(2..))
                    ),
                    "yield-native-arity: additional input register at 0x{pc:x}"
                );
            }
        }
        anyhow::ensure!(
            i.flow_control() != FlowControl::IndirectBranch
                && (i.flow_control() != FlowControl::UnconditionalBranch
                    || analysis.instructions.contains_key(&i.near_branch_target())),
            "yield-native-arity: unproved tail transfer"
        );
    }
    arity::verify(entry, analysis, count)
}

fn record_consumer(
    state: &State,
    target: u64,
    bytes: &[u8],
    code: &mut impl FnMut(u64) -> Option<Vec<u8>>,
    evidence: &mut HashSet<(u8, u64)>,
    payload: bool,
) -> bool {
    let mut matched = false;
    for (role, register) in [
        (0, Register::RCX),
        (1, Register::RDX),
        (2, Register::RDX),
        (3, Register::RCX),
    ] {
        if let Some(origin) = state
            .register(register)
            .and_then(|value| callback_origin(state, value))
            && (if role == 2 {
                callable_move(bytes, target, code) || direct_move(bytes, target)
            } else {
                callable_consumer(bytes, target, role)
            })
        {
            if role == 0 && !payload {
                continue;
            }
            if origin == 4 {
                evidence.extend([(0, role), (1, role)]);
            } else {
                evidence.insert((origin, role));
            }
            matched = true;
        }
    }
    matched
}

fn traced_consumers(
    entry: u64,
    argument: usize,
    code: &mut impl FnMut(u64) -> Option<Vec<u8>>,
    operation: &mut impl FnMut(u64) -> Option<u64>,
    byte_copy: &mut impl FnMut(u64) -> bool,
) -> Result<YieldContract> {
    let initial = State {
        registers: HashMap::from([(Register::RSP, Value::Pointer(Storage::Stack, 0))]),
        stack: BTreeMap::from([
            (((argument + 1) * 8) as i64, (8, Value::Continuation(0))),
            (((argument + 2) * 8) as i64, (8, Value::Continuation(1))),
        ]),
        lua_callback: false,
        zero: None,
        complete_receiver: false,
    };
    let mut queue = VecDeque::from([(entry, initial)]);
    let mut seen = HashSet::new();
    let mut evidence = HashSet::new();
    let mut contract = None;
    let mut sites = Vec::new();
    while let Some((at, initial)) = queue.pop_front() {
        if !seen.insert((at, captures::signature(&initial))) {
            continue;
        }
        // The search ends by itself once every state was seen: about 350 for
        // an HTTP member of Studio 0.748. The limit only bounds the work.
        anyhow::ensure!(
            seen.len() <= 2048,
            "yield-callback-consumers: provenance budget exceeded"
        );
        let Some(analysis) = flow(at, initial, &mut |at, _| code(at), &mut 32768) else {
            continue;
        };
        for (pc, state) in &analysis.states {
            let i = &analysis.instructions[pc];
            let target = match i.flow_control() {
                FlowControl::Call => i.near_branch_target(),
                FlowControl::IndirectCall => match state.read(i, 0, 8) {
                    Some(Value::ImagePointer(target)) => target,
                    _ => continue,
                },
                _ => continue,
            };
            let Some(bytes) = code(target) else {
                continue;
            };
            let payload = if state
                .register(Register::RCX)
                .and_then(|v| callback_origin(state, v))
                .is_some()
                && callable_consumer(&bytes, target, 0)
            {
                strings::payload(&analysis, at, *pc, target, code, byte_copy)
            } else {
                None
            };
            if let Some(payload) = payload {
                anyhow::ensure!(
                    contract.is_none_or(|previous| previous == payload),
                    "yield-payload-string: inconsistent engine layouts"
                );
                contract = Some(payload);
                let returns: Vec<_> = Decoder::with_ip(64, &bytes, target, DecoderOptions::NONE)
                    .into_iter()
                    .filter(|i| i.flow_control() == FlowControl::IndirectCall)
                    .map(|i| i.next_ip())
                    .collect();
                anyhow::ensure!(
                    returns.len() == 1,
                    "yield-callback-site: ambiguous invocation return"
                );
                sites.push([returns[0], i.next_ip()]);
            }
            if record_consumer(
                state,
                target,
                &bytes,
                code,
                &mut evidence,
                payload.is_some(),
            ) {
                continue;
            }
            if let Some(child) = captures::heap_invocation(state, target, code, operation) {
                queue.push_back(child);
            }
            if let Some(child) = captures::forwarded(state) {
                queue.push_back((target, child));
            }
        }
    }
    anyhow::ensure!(
        [0, 1].into_iter().all(|origin| [1, 2, 3]
            .into_iter()
            .all(|role| evidence.contains(&(origin, role))))
            && evidence.contains(&(1, 0))
            && evidence.contains(&(0, 0)),
        "yield-callback-consumers: actual continuation copy/move/destroy or success/error invocation unproved ({evidence:?})"
    );
    sites.sort_unstable();
    sites.dedup();
    Ok(YieldContract {
        layout: contract.context("yield-payload-string: construction unproved")?,
        sites,
    })
}

pub(super) fn callback_object(state: &State, argument: usize) -> Option<i64> {
    let value = match argument {
        0..=3 => {
            state.register([Register::RCX, Register::RDX, Register::R8, Register::R9][argument])
        }
        4..=7 => {
            let Value::Pointer(Storage::Stack, rsp) = state.register(Register::RSP)? else {
                return None;
            };
            state
                .stack
                .get(&(rsp + argument as i64 * 8))
                .map(|(_, value)| *value)
        }
        _ => None,
    }?;
    let Value::Pointer(Storage::Stack, object) = value else {
        return None;
    };
    Some(object)
}

fn callback_table(state: &State, argument: usize) -> Option<u64> {
    let object = callback_object(state, argument)?;
    let value = Value::Pointer(Storage::Stack, object);
    // MSVC's 64-byte std::function stores its inline implementation pointer
    // at +56 and that implementation starts with an untagged RTTI vtable.
    // These are our helper's ABI constants, not Studio layout offsets.
    if state.stack.get(&(object + 56)) != Some(&(8, value)) {
        return None;
    }
    match state.stack.get(&object) {
        Some((8, Value::ImagePointer(table))) => Some(*table),
        _ => None,
    }
}

// Some dispatchers outline the MSVC inline-callable constructor. Interpret all
// its return paths rather than assuming that an arbitrary call returns RCX.
pub(super) fn constructor(
    caller: &State,
    entry: u64,
    code: &mut impl FnMut(u64) -> Option<Vec<u8>>,
) -> Option<(i64, u64)> {
    let Value::Pointer(Storage::Stack, object) = caller.register(Register::RCX)? else {
        return None;
    };
    let Value::Pointer(Storage::Stack, _) = caller.register(Register::RDX)? else {
        return None;
    };
    let Value::Pointer(Storage::Stack, rsp) = caller.register(Register::RSP)? else {
        return None;
    };
    let mut instructions = HashMap::new();
    let mut code_budget = 1024usize;
    let mut initial = caller.clone();
    initial
        .registers
        .retain(|reg, _| matches!(reg, Register::RCX | Register::RDX));
    initial
        .registers
        .insert(Register::RSP, Value::Pointer(Storage::Stack, rsp - 8));
    let mut states = HashMap::from([(entry, initial)]);
    let mut queue = VecDeque::from([entry]);
    let mut budget = 4096usize;
    while let Some(pc) = queue.pop_front() {
        budget = budget.checked_sub(1)?;
        if !instructions.contains_key(&pc) {
            // MSVC splits a constructor at unwind-state transitions. Each
            // reached fragment still needs its own verified code boundary.
            let bytes = code(pc)?;
            code_budget = code_budget.checked_sub(bytes.len())?;
            instructions.extend(
                Decoder::with_ip(64, &bytes, pc, DecoderOptions::NONE)
                    .into_iter()
                    .map(|i| (i.ip(), i)),
            );
        }
        let i = instructions.get(&pc)?;
        if i.is_invalid() || i.flow_control() == FlowControl::Call {
            return None;
        }
        let mut state = states[&pc].clone();
        state.step(i);
        let next = match i.flow_control() {
            FlowControl::Return => vec![],
            FlowControl::ConditionalBranch => vec![i.next_ip(), i.near_branch_target()],
            FlowControl::UnconditionalBranch => vec![i.near_branch_target()],
            FlowControl::Next | FlowControl::IndirectCall => vec![i.next_ip()],
            _ => return None,
        };
        for pc in next {
            let changed = if let Some(existing) = states.get_mut(&pc) {
                existing.merge(&state)
            } else {
                states.insert(pc, state.clone());
                true
            };
            if changed {
                queue.push_back(pc);
            }
        }
    }
    let mut result = None;
    for (pc, mut state) in states {
        if instructions[&pc].flow_control() != FlowControl::Return {
            continue;
        }
        if state.register(Register::RAX) != Some(Value::Pointer(Storage::Stack, object)) {
            return None;
        }
        state
            .registers
            .insert(Register::RCX, Value::Pointer(Storage::Stack, object));
        let table = callback_table(&state, 0)?;
        if result.is_some_and(|previous| previous != table) {
            return None;
        }
        result = Some(table);
    }
    result.map(|table| (object, table))
}

#[cfg(test)]
#[path = "windows_functions_yield_tests.rs"]
mod tests;
