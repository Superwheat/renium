//! Validate the native serializer's subtree-exclusion set before using it for
//! retained-container checkpoints. Discovery follows the six argument flows,
//! recursive child collection and the set's lookup algorithm, not an image RVA.
use super::*;
use iced_x86::{
    Decoder, DecoderOptions, Instruction, InstructionInfoFactory, Mnemonic, OpAccess, OpKind,
    Register,
};

#[derive(Clone, Copy, Eq, PartialEq)]
enum Value {
    Roots,
    Flags,
    Exclusions,
    Stack(i64),
}

struct Flow {
    registers: HashMap<Register, Value>,
    stack: HashMap<i64, (Value, usize)>,
    info: InstructionInfoFactory,
}

impl Flow {
    fn new(root: Register) -> Self {
        Self {
            registers: HashMap::from([(root, Value::Roots), (Register::RSP, Value::Stack(0))]),
            stack: HashMap::from([(0x28, (Value::Flags, 4)), (0x30, (Value::Exclusions, 8))]),
            info: InstructionInfoFactory::new(),
        }
    }
    fn address(&self, i: &Instruction) -> Option<i64> {
        if i.memory_index() != Register::None {
            return None;
        }
        let Value::Stack(base) = self.registers.get(&i.memory_base().full_register())? else {
            return None;
        };
        base.checked_add(i.memory_displacement64() as i64)
            .filter(|v| (-8192..=8192).contains(v))
    }
    fn step(&mut self, i: &Instruction) {
        let destination = i.op0_register().full_register();
        let call_stack = if i.mnemonic() == Mnemonic::Call {
            self.registers.get(&Register::RSP).copied()
        } else {
            None
        };
        let value = match i.mnemonic() {
            Mnemonic::Mov if i.op0_kind() == OpKind::Register => match i.op1_kind() {
                OpKind::Register => self
                    .registers
                    .get(&i.op1_register().full_register())
                    .copied(),
                OpKind::Memory => self
                    .address(i)
                    .and_then(|at| self.stack.get(&at))
                    .filter(|(_, size)| *size == i.memory_size().size())
                    .map(|(value, _)| *value),
                _ => None,
            },
            Mnemonic::Lea if i.op0_kind() == OpKind::Register => self.address(i).map(Value::Stack),
            Mnemonic::Sub | Mnemonic::Add if i.op0_register() == Register::RSP => {
                let immediate = match i.op1_kind() {
                    OpKind::Immediate32to64 => Some(i.immediate32to64()),
                    OpKind::Immediate8to64 => Some(i.immediate8to64()),
                    _ => None,
                };
                match (self.registers.get(&Register::RSP), immediate) {
                    (Some(Value::Stack(sp)), Some(amount)) => sp
                        .checked_add(if i.mnemonic() == Mnemonic::Sub {
                            -amount
                        } else {
                            amount
                        })
                        .map(Value::Stack),
                    _ => None,
                }
            }
            _ => None,
        };
        if i.op0_kind() == OpKind::Memory
            && i.mnemonic() == Mnemonic::Mov
            && let Some(at) = self.address(i)
        {
            let size = i.memory_size().size();
            let source = self
                .registers
                .get(&i.op1_register().full_register())
                .copied();
            self.stack
                .retain(|old, (_, length)| *old + *length as i64 <= at || *old >= at + size as i64);
            if let Some(source) = source {
                self.stack.insert(at, (source, size));
            }
        }
        if matches!(i.mnemonic(), Mnemonic::Push | Mnemonic::Pop) {
            if let Some(Value::Stack(sp)) = self.registers.get_mut(&Register::RSP) {
                *sp += i.stack_pointer_increment() as i64;
            }
            if i.mnemonic() == Mnemonic::Pop {
                self.registers.remove(&destination);
            }
            return;
        }
        for register in self.info.info(i).used_registers() {
            if matches!(
                register.access(),
                OpAccess::Write
                    | OpAccess::CondWrite
                    | OpAccess::ReadWrite
                    | OpAccess::ReadCondWrite
            ) {
                self.registers.remove(&register.register().full_register());
            }
        }
        if let Some(value) = value
            && (i.op0_register().size() == 8 || value == Value::Flags)
        {
            self.registers.insert(destination, value);
        }
        if i.mnemonic() == Mnemonic::Call {
            if let Some(stack) = call_stack {
                self.registers.insert(Register::RSP, stack);
            }
            for register in [
                Register::RAX,
                Register::RCX,
                Register::RDX,
                Register::R8,
                Register::R9,
                Register::R10,
                Register::R11,
            ] {
                self.registers.remove(&register);
            }
        }
    }
    fn outgoing(&self, offset: i64, value: Value, size: usize) -> bool {
        matches!(self.registers.get(&Register::RSP), Some(Value::Stack(sp))
            if self.stack.get(&(sp + offset)) == Some(&(value, size)))
    }
}

fn reg(i: &Instruction, mnemonic: Mnemonic, a: Register, b: Register) -> bool {
    i.mnemonic() == mnemonic
        && i.op0_kind() == OpKind::Register
        && i.op1_kind() == OpKind::Register
        && i.op0_register().full_register() == a
        && i.op1_register().full_register() == b
}
fn memory(
    i: &Instruction,
    to: Register,
    base: Register,
    offset: u64,
    index: Register,
    scale: u32,
) -> bool {
    i.mnemonic() == Mnemonic::Mov
        && i.op0_register() == to
        && i.op1_kind() == OpKind::Memory
        && i.memory_size().size() == 8
        && i.memory_base() == base
        && i.memory_displacement64() == offset
        && i.memory_index() == index
        && (index == Register::None || i.memory_index_scale() == scale)
}
fn immediate(i: &Instruction, mnemonic: Mnemonic, to: Register, value: u8) -> bool {
    i.mnemonic() == mnemonic
        && i.op0_register() == to
        && i.op1_kind() == OpKind::Immediate8
        && i.immediate8() == value
}
fn unary(i: &Instruction, mnemonic: Mnemonic, to: Register) -> bool {
    i.mnemonic() == mnemonic && i.op0_register() == to
}

fn hash_lookup(code: &[Instruction], start: usize, flow: &Flow, recursive: u64) -> bool {
    let Some(i) = code.get(start..start + 26) else {
        return false;
    };
    let set = i[0].memory_base();
    if flow.registers.get(&set) != Some(&Value::Exclusions) {
        return false;
    }
    let (begin, mask, empty, key, hash, step, current) = (
        i[0].op0_register(),
        i[1].op0_register(),
        i[4].op0_register(),
        i[5].op0_register(),
        i[10].op0_register(),
        i[14].op0_register().full_register(),
        i[15].op0_register(),
    );
    let roles = [set, begin, mask, empty, key, hash, step, current];
    if roles.contains(&Register::None)
        || roles.iter().copied().collect::<HashSet<_>>().len() != roles.len()
    {
        return false;
    }
    memory(&i[0], begin, set, 0, Register::None, 1)
        && memory(&i[1], mask, set, 8, Register::None, 1)
        && reg(&i[2], Mnemonic::Cmp, begin, mask)
        && i[3].mnemonic() == Mnemonic::Je
        && memory(&i[4], empty, set, 32, Register::None, 1)
        && reg(&i[5], Mnemonic::Cmp, key, empty)
        && i[6].mnemonic() == Mnemonic::Je
        && reg(&i[7], Mnemonic::Sub, mask, begin)
        && immediate(&i[8], Mnemonic::Sar, mask, 3)
        && unary(&i[9], Mnemonic::Dec, mask)
        && reg(&i[10], Mnemonic::Mov, hash, key)
        && immediate(&i[11], Mnemonic::Shr, hash, 3)
        && reg(&i[12], Mnemonic::Add, hash, key)
        && reg(&i[13], Mnemonic::And, hash, mask)
        && reg(&i[14], Mnemonic::Xor, step, step)
        && memory(&i[15], current, begin, 0, hash, 8)
        && reg(&i[16], Mnemonic::Cmp, current, key)
        && i[17].mnemonic() == Mnemonic::Je
        && i[17].near_branch_target() > recursive
        && reg(&i[18], Mnemonic::Cmp, current, empty)
        && i[19].mnemonic() == Mnemonic::Je
        && unary(&i[20], Mnemonic::Inc, hash)
        && reg(&i[21], Mnemonic::Add, hash, step)
        && reg(&i[22], Mnemonic::And, hash, mask)
        && unary(&i[23], Mnemonic::Inc, step)
        && reg(&i[24], Mnemonic::Cmp, step, mask)
        && i[25].mnemonic() == Mnemonic::Jbe
        && i[25].near_branch_target() == i[15].ip()
}

pub(super) fn discover(image: &PeImage<'_>, bytes: &[u8], serializer: usize) -> Result<usize> {
    let decode = |rva| -> Result<Vec<Instruction>> {
        let at = image.rva_to_offset(rva)?;
        let (begin, end) = image.function_bounds(at)?;
        anyhow::ensure!(
            at == begin && end - begin <= 65536,
            "Serializer collection function bounds changed"
        );
        let code = Decoder::with_ip(64, &bytes[begin..end], rva as u64, DecoderOptions::NONE)
            .into_iter()
            .filter(|i| i.mnemonic() != Mnemonic::Nop)
            .collect::<Vec<_>>();
        anyhow::ensure!(
            code.iter().all(|i| !i.is_invalid()),
            "Invalid serializer collection instructions"
        );
        Ok(code)
    };
    let code = decode(serializer)?;
    let mut flow = Flow::new(Register::R8);
    let mut candidates = Vec::new();
    for i in code.iter().take(256) {
        if i.mnemonic() == Mnemonic::Call
            && i.op0_kind() == OpKind::NearBranch64
            && flow.registers.get(&Register::RCX) == Some(&Value::Roots)
            && flow.outgoing(0x20, Value::Flags, 4)
            && flow.outgoing(0x28, Value::Exclusions, 8)
        {
            candidates.push(i.near_branch_target() as usize);
        }
        if matches!(
            i.flow_control(),
            iced_x86::FlowControl::UnconditionalBranch | iced_x86::FlowControl::Return
        ) {
            break;
        }
        flow.step(i);
    }
    let mut found = HashSet::new();
    for candidate in candidates {
        let code = decode(candidate)?;
        let Some(recursive) = code.iter().find(|i| {
            i.mnemonic() == Mnemonic::Call
                && i.op0_kind() == OpKind::NearBranch64
                && i.near_branch_target() as usize == candidate
        }) else {
            continue;
        };
        let mut flow = Flow::new(Register::RCX);
        for (index, i) in code.iter().enumerate().take(128) {
            if hash_lookup(&code, index, &flow, recursive.ip()) {
                found.insert(candidate);
                break;
            }
            if matches!(
                i.flow_control(),
                iced_x86::FlowControl::UnconditionalBranch | iced_x86::FlowControl::Return
            ) {
                break;
            }
            flow.step(i);
        }
    }
    anyhow::ensure!(
        found.len() == 1,
        "Studio retained-state serializer ABI matched {} collectors; update Renium's finder",
        found.len()
    );
    Ok(*found.iter().next().unwrap())
}
