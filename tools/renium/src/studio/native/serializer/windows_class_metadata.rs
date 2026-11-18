//! Follow the constructor's loads of reflected class metadata.
use super::*;
use iced_x86::{InstructionInfoFactory, OpAccess};
use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Value {
    Receiver,
    Address(usize),
}

#[derive(Clone, Default, Eq, PartialEq)]
struct State {
    registers: HashMap<Register, Value>,
    named: bool,
}

impl State {
    fn get(&self, register: Register) -> Option<Value> {
        self.registers.get(&register.full_register()).copied()
    }

    fn merge(&mut self, other: &Self) -> bool {
        let before = self.clone();
        self.named &= other.named;
        self.registers
            .retain(|reg, value| other.get(*reg) == Some(*value));
        *self != before
    }
}

trait Image {
    fn code(&mut self, address: usize) -> Option<Vec<u8>>;
    fn pointer(&mut self, address: usize) -> Option<usize>;
    fn is_name(&mut self, address: usize) -> bool;
    fn class_table(&self) -> usize;
}

fn moved(image: &mut impl Image, state: &State, i: &Instruction) -> Option<Value> {
    if i.op0_register().size() != 8 {
        return None;
    }
    if i.mnemonic() == Mnemonic::Mov && i.op1_kind() == OpKind::Register {
        return state.get(i.op1_register());
    }
    if i.op1_kind() != OpKind::Memory || i.memory_index() != Register::None {
        return None;
    }
    let address = if i.is_ip_rel_memory_operand() {
        i.ip_rel_memory_address() as usize
    } else {
        let Value::Address(base) = state.get(i.memory_base())? else {
            return None;
        };
        base.checked_add_signed(i.memory_displacement64() as i64 as isize)?
    };
    if i.mnemonic() == Mnemonic::Lea {
        Some(Value::Address(address))
    } else {
        image.pointer(address).map(Value::Address)
    }
}

fn invalidate(state: &mut State, i: &Instruction, info: &mut InstructionInfoFactory) {
    if i.mnemonic() == Mnemonic::Call {
        clear_volatile(&mut state.registers);
    } else {
        for reg in info.info(i).used_registers() {
            if matches!(
                reg.access(),
                OpAccess::Write
                    | OpAccess::CondWrite
                    | OpAccess::ReadWrite
                    | OpAccess::ReadCondWrite
            ) {
                state.registers.remove(&reg.register().full_register());
            }
        }
    }
}

fn named_call(image: &mut impl Image, state: &State, i: &Instruction, depth: usize) -> bool {
    if i.mnemonic() != Mnemonic::Call || i.op0_kind() != OpKind::NearBranch64 {
        return false;
    }
    let Some(Value::Address(metadata)) = state.get(Register::RDX) else {
        return false;
    };
    if state.get(Register::RCX).is_none() || image.is_name(metadata) {
        return false;
    }
    let initial = State {
        registers: HashMap::from([
            (Register::RCX, Value::Receiver),
            (Register::RDX, Value::Address(metadata)),
        ]),
        named: false,
    };
    follows_name(image, i.near_branch_target() as usize, initial, depth + 1)
}

fn follows_name(image: &mut impl Image, entry: usize, initial: State, depth: usize) -> bool {
    if depth > 3 {
        return false;
    }
    let Some(bytes) = image.code(entry) else {
        return false;
    };
    let code = Decoder::with_ip(64, &bytes, entry as u64, DecoderOptions::NONE)
        .into_iter()
        .map(|i| (i.ip() as usize, i))
        .collect::<BTreeMap<_, _>>();
    if code.values().any(Instruction::is_invalid) {
        return false;
    }
    // Recursive checks share the image budget. Inspect the converged states in
    // address order so the same executable always yields the same evidence.
    let mut states = BTreeMap::from([(entry, initial)]);
    let mut pending = VecDeque::from([entry]);
    let mut remaining = 8192usize;
    let mut info = InstructionInfoFactory::new();
    while let Some(pc) = pending.pop_front() {
        let Some(left) = remaining.checked_sub(1) else {
            return false;
        };
        remaining = left;
        let Some(i) = code.get(&pc) else {
            continue;
        };
        let mut state = states[&pc].clone();
        let named = i.mnemonic() == Mnemonic::Call
            && i.op0_kind() == OpKind::NearBranch64
            && state.get(Register::RCX) == Some(Value::Receiver)
            && matches!(state.get(Register::RDX), Some(Value::Address(at)) if image.is_name(at));
        let value = matches!(i.mnemonic(), Mnemonic::Mov | Mnemonic::Lea)
            .then(|| moved(image, &state, i))
            .flatten();
        invalidate(&mut state, i, &mut info);
        state.named |= named;
        if let Some(value) = value {
            state
                .registers
                .insert(i.op0_register().full_register(), value);
        }
        let next = match i.flow_control() {
            FlowControl::Next | FlowControl::Call | FlowControl::IndirectCall => {
                vec![i.next_ip() as usize]
            }
            FlowControl::ConditionalBranch => {
                vec![i.next_ip() as usize, i.near_branch_target() as usize]
            }
            FlowControl::UnconditionalBranch => vec![i.near_branch_target() as usize],
            _ => vec![],
        };
        for at in next {
            let changed = if let Some(existing) = states.get_mut(&at) {
                existing.merge(&state)
            } else {
                states.insert(at, state.clone());
                true
            };
            if changed {
                pending.push_back(at);
            }
        }
    }
    // Inspect only the fixed-point states. A first visit along one branch is
    // insufficient: a later predecessor can invalidate its name/receiver proof.
    states.into_iter().any(|(pc, state)| {
        let Some(i) = code.get(&pc) else {
            return false;
        };
        class_stamp(&state, i, image.class_table()) || named_call(image, &state, i, depth)
    })
}

fn class_stamp(state: &State, i: &Instruction, table: usize) -> bool {
    state.named
        && i.mnemonic() == Mnemonic::Mov
        && i.op0_kind() == OpKind::Memory
        && i.memory_index() == Register::None
        && i.memory_displacement64() == 0
        && i.memory_size().size() == 8
        && state.get(i.memory_base()) == Some(Value::Receiver)
        && state.get(i.op1_register()) == Some(Value::Address(table))
}

#[derive(Clone, Default)]
pub(in super::super) struct Evidence {
    pub code: Vec<usize>,
    pub pointers: BTreeSet<(usize, usize)>,
    pub names: BTreeSet<usize>,
}

struct ClassImage<'a, 'b> {
    image: &'a PeImage<'b>,
    class_table: usize,
    evidence: Evidence,
    remaining_repeats: usize,
}

impl Image for ClassImage<'_, '_> {
    fn code(&mut self, address: usize) -> Option<Vec<u8>> {
        let bytes = function(self.image, address)?;
        if self.evidence.code.contains(&address) {
            self.remaining_repeats = self.remaining_repeats.checked_sub(1)?;
        } else {
            if self.evidence.code.len() >= 32 {
                return None;
            }
            self.evidence.code.push(address);
        }
        Some(bytes.to_vec())
    }
    fn pointer(&mut self, address: usize) -> Option<usize> {
        let section = self.image.section(b".rdata").ok()?;
        if address
            .checked_sub(section.virtual_address)?
            .checked_add(8)?
            > section.raw_size
        {
            return None;
        }
        let offset = self.image.rva_to_offset(address).ok()?;
        let target = (read_u64(self.image.bytes, offset).ok()? as usize)
            .checked_sub(self.image.image_base)?;
        self.image.rva_to_offset(target).ok()?;
        self.evidence.pointers.insert((address, target));
        Some(target)
    }
    fn is_name(&mut self, address: usize) -> bool {
        let found = self
            .image
            .rva_to_offset(address)
            .ok()
            .and_then(|at| self.image.bytes.get(at..at + SOURCE_CLASS_NAME.len()))
            == Some(SOURCE_CLASS_NAME);
        if found {
            self.evidence.names.insert(address);
        }
        found
    }
    fn class_table(&self) -> usize {
        self.class_table
    }
}

pub(super) fn source(image: &PeImage<'_>, entry: usize, class_table: usize) -> Option<Evidence> {
    let mut probe = ClassImage {
        image,
        class_table,
        evidence: Evidence::default(),
        remaining_repeats: 256,
    };
    follows_name(&mut probe, entry, State::default(), 0).then_some(probe.evidence)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeated_function_visits_have_a_separate_budget() {
        let mut bytes = vec![0xc3; 0x1000];
        for index in 0..33u32 {
            let at = 0x800 + index as usize * 12;
            bytes[at..at + 4].copy_from_slice(&(index + 0x100).to_le_bytes());
            bytes[at + 4..at + 8].copy_from_slice(&(index + 0x101).to_le_bytes());
        }
        let image = PeImage {
            bytes: &bytes,
            image_base: 0,
            image_stamp: [0; 3],
            sections: [
                (*b".text\0\0\0", 0x100, 33, 0x20000000),
                (*b".pdata\0\0", 0x800, 33 * 12, 0),
            ]
            .into_iter()
            .map(|(name, start, size, characteristics)| PeSection {
                name,
                virtual_size: size,
                virtual_address: start,
                raw_size: size,
                raw_offset: start,
                characteristics,
            })
            .collect(),
        };
        let mut probe = ClassImage {
            image: &image,
            class_table: 0,
            evidence: Evidence::default(),
            remaining_repeats: 256,
        };
        assert!(probe.code(0x100).is_some());
        for _ in 0..256 {
            assert!(probe.code(0x100).is_some());
        }
        assert!(probe.code(0x100).is_none());
        for at in 0x101..0x120 {
            assert!(probe.code(at).is_some());
        }
        assert!(probe.code(0x120).is_none());
        assert_eq!(probe.evidence.code.len(), 32);
    }
    struct Fixture {
        code: Vec<u8>,
        field: usize,
        name: usize,
        table: usize,
    }
    impl Image for Fixture {
        fn code(&mut self, _: usize) -> Option<Vec<u8>> {
            Some(self.code.clone())
        }
        fn pointer(&mut self, at: usize) -> Option<usize> {
            (at == self.field).then_some(self.name)
        }
        fn is_name(&mut self, at: usize) -> bool {
            at == self.name
        }
        fn class_table(&self) -> usize {
            self.table
        }
    }
    #[test]
    fn metadata_name_follows_constructor_loads_and_rtti() -> Result<()> {
        for field in [0x18u8, 0x38, 0x68] {
            // Save this and metadata, load the name into RDX, call the base
            // descriptor constructor, and stamp the complete ClassDescriptor.
            let mut code = hex("488BD9488BFA488B5718E800000000488D054A000000488903C3")?;
            code[9] = field;
            let mut image = Fixture {
                code,
                field: 0x2000 + field as usize,
                name: 0x3000,
                table: 0x1060,
            };
            let initial = State {
                registers: HashMap::from([
                    (Register::RCX, Value::Receiver),
                    (Register::RDX, Value::Address(0x2000)),
                ]),
                named: false,
            };
            assert!(follows_name(&mut image, 0x1000, initial.clone(), 0));
            image.table += 8;
            assert!(!follows_name(&mut image, 0x1000, initial.clone(), 0));
            image.table -= 8;
            let original = image.code.clone();
            image.code.splice(10..10, [0x74, 0x05]); // one path skips passing the name
            image.table += 2;
            assert!(!follows_name(&mut image, 0x1000, initial.clone(), 0));
            image.table -= 2;
            image.code = original;
            image.code[8] = 0x4f; // name loaded into RCX instead of RDX
            assert!(!follows_name(&mut image, 0x1000, initial, 0));
        }
        Ok(())
    }
}
