//! A separate may-alias analysis: reflection's must-facts deliberately forget
//! differing branch values, which cannot establish the native stack footprint.
//!
//! One thing is taken from the frame layout and not proved: an address formed
//! from one of this frame's own locals and an index, or returned by a callee
//! that was handed such a local, stays inside the locals. Those lie below the
//! return address, so such an address reaches no incoming parameter unless
//! the code overruns its own buffer. A parameter is read at a fixed offset
//! from the stack or frame pointer, and those reads are bounded exactly.
//! Forming the address of a parameter past the last one, in a register or a
//! stack slot, is refused as well:
//! a callee handed that address reads what the caller never supplied.
//!
//! This reads code a compiler made, to notice a member that takes more than
//! it is given after an engine update; it is no defense against code written
//! to mislead it. Taken on trust as well: a callee stores no address it was
//! handed where its caller reads it back, and a repeated copy inside the
//! locals stays inside them.
use super::*;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Origin {
    Other,
    Stack(i64, i64),
    Local,
    Lost,
}
impl Origin {
    fn merge(self, other: Self) -> Self {
        match (self, other) {
            (Self::Lost, _) | (_, Self::Lost) => Self::Lost,
            (Self::Stack(a, b), Self::Stack(c, d)) => Self::Stack(a.min(c), b.max(d)),
            (Self::Local, other) | (other, Self::Local) => {
                if other == Self::Other || other.local() {
                    Self::Local
                } else {
                    Self::Lost
                }
            }
            (Self::Stack(..), _) => self,
            (_, Self::Stack(..)) => other,
            _ => Self::Other,
        }
    }
    fn offset(self, offset: i64) -> Self {
        match self {
            Self::Stack(a, b) => a
                .checked_add(offset)
                .zip(b.checked_add(offset))
                .map_or(Self::Lost, |(a, b)| Self::Stack(a, b)),
            value => value,
        }
    }
    fn transformed(self) -> Self {
        if self == Self::Other {
            self
        } else {
            Self::Lost
        }
    }
    fn local(self) -> bool {
        matches!(self, Self::Local) || matches!(self, Self::Stack(_, high) if high < 0)
    }
    // What a callee may hand back of the pointers it was given.
    fn returned(self) -> Self {
        if self == Self::Other {
            self
        } else if self.local() {
            Self::Local
        } else {
            Self::Lost
        }
    }
}
#[derive(Clone, Default, Eq, PartialEq)]
struct Origins {
    regs: HashMap<Register, Origin>,
    stack: BTreeMap<i64, (usize, Origin)>,
}
impl Origins {
    fn reg(&self, r: Register) -> Origin {
        if matches!(r, Register::None | Register::RIP | Register::EIP) {
            return Origin::Other;
        }
        self.regs
            .get(&r.full_register())
            .copied()
            .unwrap_or(Origin::Lost)
    }
    fn address(&self, base: Register, index: (Register, u32), displacement: i64) -> Origin {
        let base = self.reg(base).offset(displacement);
        if index.0 == Register::None {
            return base;
        }
        match (base, self.reg(index.0)) {
            (Origin::Other, Origin::Other) => Origin::Other,
            (pointer, Origin::Other) if pointer.local() => Origin::Local,
            (Origin::Other, pointer) if pointer.local() && index.1 == 1 => Origin::Local,
            _ => Origin::Lost,
        }
    }
    fn memory(&self, i: &Instruction) -> Origin {
        self.address(
            i.memory_base(),
            (i.memory_index(), i.memory_index_scale()),
            i.memory_displacement64() as i64,
        )
    }
    fn read(&self, i: &Instruction, operand: u32) -> Origin {
        match i.op_kind(operand) {
            OpKind::Register => self.reg(i.op_register(operand)),
            OpKind::Memory => {
                if let Origin::Stack(a, b) = self.memory(i) {
                    if a != b {
                        return Origin::Lost;
                    }
                    return self.slot(a, i.memory_size().size());
                }
                Origin::Other
            }
            _ => Origin::Other,
        }
    }
    // What `width` bytes of the stack at `a` hold: a stored value where they
    // are exactly its slot, and no address where they cover part of one.
    fn slot(&self, a: i64, width: usize) -> Origin {
        self.stack
            .iter()
            .filter(|(at, (size, _))| **at < a + width as i64 && **at + *size as i64 > a)
            .fold(Origin::Other, |origin, (at, (size, value))| {
                origin.merge(if *at == a && *size == width {
                    *value
                } else {
                    value.transformed()
                })
            })
    }
    fn store(&mut self, at: i64, width: usize, value: Origin) {
        self.stack
            .retain(|start, (size, _)| *start + *size as i64 <= at || *start >= at + width as i64);
        self.stack.insert(at, (width, value));
    }
    fn merge(&mut self, other: &Self) -> bool {
        let before = self.clone();
        for (reg, value) in &other.regs {
            self.regs.insert(*reg, self.reg(*reg).merge(*value));
        }
        // A register only one path wrote holds anything on the other.
        self.regs.retain(|reg, _| other.regs.contains_key(reg));
        for (at, (width, value)) in &other.stack {
            let old = self.stack.get(at).map_or(Origin::Other, |(_, old)| *old);
            self.stack.insert(*at, (*width, old.merge(*value)));
        }
        *self != before
    }
    fn computed(&self, i: &Instruction) -> Origin {
        match i.mnemonic() {
            Mnemonic::Mov
            | Mnemonic::Movaps
            | Mnemonic::Movups
            | Mnemonic::Movdqa
            | Mnemonic::Movdqu
            | Mnemonic::Movq
            | Mnemonic::Movd => self.read(i, 1),
            Mnemonic::Lea => self.memory(i),
            Mnemonic::Xor | Mnemonic::Pxor | Mnemonic::Xorps
                if i.op0_kind() == OpKind::Register
                    && i.op1_kind() == OpKind::Register
                    && i.op0_register() == i.op1_register() =>
            {
                Origin::Other
            }
            Mnemonic::Add | Mnemonic::Sub
                if matches!(
                    i.op1_kind(),
                    OpKind::Immediate8to64 | OpKind::Immediate32to64
                ) =>
            {
                let offset = i.immediate(1) as i64;
                self.read(i, 0).offset(if i.mnemonic() == Mnemonic::Sub {
                    offset.wrapping_neg()
                } else {
                    offset
                })
            }
            // What a string instruction loads through RSI is whatever lies there.
            _ if i.is_string_instruction() => {
                if InstructionInfoFactory::new()
                    .info(i)
                    .used_memory()
                    .iter()
                    .any(|memory| memory.base().full_register() == Register::RSI)
                {
                    self.reg(Register::RSI).transformed()
                } else {
                    Origin::Other
                }
            }
            _ => (0..i.op_count())
                .fold(Origin::Other, |value, op| value.merge(self.read(i, op)))
                .transformed(),
        }
    }
    fn step(&mut self, i: &Instruction) -> Result<()> {
        let value = self.computed(i);
        match i.mnemonic() {
            Mnemonic::Push => {
                let Origin::Stack(at, end) = self.reg(Register::RSP) else {
                    anyhow::bail!("lost stack");
                };
                anyhow::ensure!(at == end, "ambiguous stack");
                self.store(at - 8, 8, self.read(i, 0));
                self.regs
                    .insert(Register::RSP, Origin::Stack(at - 8, at - 8));
            }
            Mnemonic::Pop => {
                let Origin::Stack(at, end) = self.reg(Register::RSP) else {
                    anyhow::bail!("lost stack");
                };
                anyhow::ensure!(at == end, "ambiguous stack");
                let value = self.slot(at, 8);
                self.regs.insert(i.op0_register().full_register(), value);
                self.regs
                    .insert(Register::RSP, Origin::Stack(at + 8, at + 8));
            }
            Mnemonic::Call => {
                let aliases = [Register::RCX, Register::RDX, Register::R8, Register::R9]
                    .into_iter()
                    .fold(Origin::Other, |value, r| value.merge(self.reg(r)));
                for r in [
                    Register::RAX,
                    Register::RCX,
                    Register::RDX,
                    Register::R8,
                    Register::R9,
                    Register::R10,
                    Register::R11,
                ] {
                    self.regs.insert(r, Origin::Other);
                }
                // An unmodelled callee may return an alias of a passed local.
                self.regs.insert(Register::RAX, aliases.returned());
            }
            // Stores then run downward, and two registers are stored that
            // are no operand.
            Mnemonic::Std | Mnemonic::Maskmovdqu | Mnemonic::Maskmovq => {
                anyhow::bail!("unmodelled store")
            }
            Mnemonic::Cmpxchg16b | Mnemonic::Cmpxchg8b => {
                anyhow::ensure!(
                    [Register::RBX, Register::RCX]
                        .iter()
                        .all(|r| self.reg(*r) == Origin::Other),
                    "stack pointer escaped through unmodelled memory"
                );
                self.write(i, value)?
            }
            _ if i.is_string_instruction() => self.string(i, value)?,
            _ => self.write(i, value)?,
        }
        Ok(())
    }
    // A string instruction moves RSI and RDI by one element, or by an unknown
    // count when repeated, and may store through RDI.
    fn string(&mut self, i: &Instruction, value: Origin) -> Result<()> {
        let repeated = i.has_rep_prefix() || i.has_repne_prefix();
        let size = i.memory_size().size();
        let mut factory = InstructionInfoFactory::new();
        let info = factory.info(i);
        let stores = info.used_memory().iter().any(|memory| {
            memory.base().full_register() == Register::RDI
                && matches!(
                    memory.access(),
                    OpAccess::Write | OpAccess::CondWrite | OpAccess::ReadWrite
                )
        });
        if stores {
            let stored = if i.op1_kind() == OpKind::Register {
                self.reg(i.op1_register())
            } else {
                value
            };
            // Part of an address is no address, and no plain value either.
            let stored = if size < 8 {
                stored.transformed()
            } else {
                stored
            };
            match self.reg(Register::RDI) {
                Origin::Stack(at, end) if !repeated => {
                    anyhow::ensure!(at == end, "ambiguous stack store");
                    self.store(at, size, stored)
                }
                Origin::Stack(at, _) => {
                    for (start, (width, slot)) in self.stack.iter_mut() {
                        if *start + *width as i64 > at && *start < 0 {
                            *slot = Origin::Lost;
                        }
                    }
                }
                Origin::Local => {
                    for (start, (_, slot)) in self.stack.iter_mut() {
                        if *start < 0 {
                            *slot = Origin::Lost;
                        }
                    }
                }
                _ => anyhow::ensure!(
                    stored == Origin::Other,
                    "stack pointer escaped through unmodelled memory"
                ),
            }
        }
        for r in info.used_registers() {
            if !matches!(
                r.access(),
                OpAccess::Write
                    | OpAccess::ReadWrite
                    | OpAccess::CondWrite
                    | OpAccess::ReadCondWrite
            ) {
                continue;
            }
            let register = r.register().full_register();
            let next = if !matches!(register, Register::RSI | Register::RDI) {
                value
            } else if repeated {
                self.reg(register).returned()
            } else {
                self.reg(register).offset(size as i64)
            };
            self.regs.insert(register, next);
        }
        Ok(())
    }
    fn write(&mut self, i: &Instruction, value: Origin) -> Result<()> {
        if i.op0_kind() == OpKind::Memory && !matches!(i.mnemonic(), Mnemonic::Cmp | Mnemonic::Test)
        {
            if let Origin::Stack(at, end) = self.memory(i) {
                anyhow::ensure!(at == end, "ambiguous stack store");
                self.store(at, i.memory_size().size(), value);
            } else {
                anyhow::ensure!(
                    value == Origin::Other,
                    "stack pointer escaped through unmodelled memory"
                );
            }
        }
        for r in InstructionInfoFactory::new().info(i).used_registers() {
            if matches!(
                r.access(),
                OpAccess::Write
                    | OpAccess::ReadWrite
                    | OpAccess::CondWrite
                    | OpAccess::ReadCondWrite
            ) {
                self.regs.insert(r.register().full_register(), value);
            }
        }
        Ok(())
    }
}
pub(super) fn verify(entry: u64, analysis: &Flow, count: usize) -> Result<()> {
    let end = ((count.max(4) + 1) * 8) as i64;
    let mut initial = Origins {
        regs: HashMap::from([(Register::RSP, Origin::Stack(0, 0))]),
        ..Origins::default()
    };
    for reg in [Register::RCX, Register::RDX, Register::R8, Register::R9]
        .into_iter()
        .take(count)
    {
        initial.regs.insert(reg, Origin::Other);
    }
    let mut states = HashMap::from([(entry, initial)]);
    let mut pending = VecDeque::from([entry]);
    let mut budget = 32768usize;
    while let Some(pc) = pending.pop_front() {
        budget = budget
            .checked_sub(1)
            .context("yield-native-arity: provenance budget exceeded")?;
        let i = analysis
            .instructions
            .get(&pc)
            .context("yield-native-arity: missing instruction")?;
        let mut state = states[&pc].clone();
        anyhow::ensure!(
            matches!(state.reg(Register::RSP), Origin::Stack(a, b) if a == b),
            "yield-native-arity: lost stack provenance"
        );
        for memory in InstructionInfoFactory::new().info(i).used_memory() {
            match state.address(
                memory.base(),
                (memory.index(), memory.scale()),
                memory.displacement() as i64,
            ) {
                Origin::Other | Origin::Local => {}
                Origin::Lost => anyhow::bail!("yield-native-arity: lost memory origin at 0x{pc:x}"),
                // A repeated string instruction covers a length nothing here
                // bounds, so it may only work inside the locals.
                Origin::Stack(_, high) => anyhow::ensure!(
                    high.checked_add(memory.memory_size().size() as i64)
                        .is_some_and(|last| last <= end)
                        && (high < 0 || !(i.has_rep_prefix() || i.has_repne_prefix())),
                    "yield-native-arity: access beyond {count} native parameters at 0x{pc:x}"
                ),
            }
        }
        state.step(i).with_context(|| {
            format!("yield-native-arity: unproved memory provenance at 0x{pc:x}")
        })?;
        anyhow::ensure!(
            !state
                .regs
                .values()
                .chain(state.stack.values().map(|(_, value)| value))
                .any(|value| matches!(value, Origin::Stack(_, high) if *high >= end)),
            "yield-native-arity: address beyond {count} native parameters at 0x{pc:x}"
        );
        let next = match i.flow_control() {
            FlowControl::Return | FlowControl::Exception | FlowControl::Interrupt => vec![],
            FlowControl::UnconditionalBranch => vec![i.near_branch_target()],
            FlowControl::ConditionalBranch => vec![i.next_ip(), i.near_branch_target()],
            FlowControl::IndirectBranch => {
                anyhow::bail!("yield-native-arity: unknown tail transfer")
            }
            _ => vec![i.next_ip()],
        };
        for at in next {
            let changed = if let Some(old) = states.get_mut(&at) {
                old.merge(&state)
            } else {
                states.insert(at, state.clone());
                true
            };
            if changed {
                pending.push_back(at);
            }
        }
    }
    Ok(())
}
