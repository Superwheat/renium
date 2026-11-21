//! Relate the payload to a fresh byte-string construction, not its appearance.
//! Offsets come from engine cleanup/construction and are compared with the
//! helper's actual std::string objects before any engine call.
use super::*;
const OBJECT: i64 = 4096;
const DATA: Value = Value::Bytes(Storage::Descriptor, 16384, 8);
const LENGTH: Value = Value::Bytes(Storage::Descriptor, 32768, 8);

fn initial() -> State {
    State {
        registers: HashMap::from([
            (Register::RSP, Value::Pointer(Storage::Stack, 0)),
            (Register::RCX, Value::Pointer(Storage::Descriptor, OBJECT)),
            (Register::RDX, DATA),
            (Register::R8, LENGTH),
        ]),
        stack: BTreeMap::new(),
        lua_callback: false,
        zero: None,
        complete_receiver: false,
    }
}
fn next(i: &Instruction) -> Option<Vec<u64>> {
    Some(match i.flow_control() {
        FlowControl::Return | FlowControl::Exception | FlowControl::Interrupt => vec![],
        FlowControl::ConditionalBranch => vec![i.next_ip(), i.near_branch_target()],
        FlowControl::UnconditionalBranch => vec![i.near_branch_target()],
        FlowControl::IndirectBranch => return None,
        _ => vec![i.next_ip()],
    })
}
fn complete(analysis: &Flow) -> bool {
    analysis.states.keys().all(|pc| {
        next(&analysis.instructions[pc]).is_some_and(|edges| {
            edges
                .into_iter()
                .all(|pc| analysis.states.contains_key(&pc))
        })
    })
}
// Every path reaching `end` must cross a qualified producer; reject intervening
// operations selected by the caller. Walking backwards also handles joins.
fn preceded(analysis: &Flow, entry: u64, end: u64, accept: impl Fn(u64) -> Option<bool>) -> bool {
    let mut pending = vec![end];
    let mut seen = HashSet::new();
    while let Some(at) = pending.pop() {
        if !seen.insert(at) {
            continue;
        }
        match accept(at) {
            Some(true) => continue,
            Some(false) => return false,
            None => {}
        }
        if at == entry {
            return false;
        }
        let mut found = false;
        for (pc, i) in &analysis.instructions {
            if analysis.states.contains_key(pc) && next(i).is_some_and(|next| next.contains(&at)) {
                pending.push(*pc);
                found = true;
            }
        }
        if !found {
            return false;
        }
    }
    true
}
fn writes(i: &Instruction, reg: Register) -> bool {
    InstructionInfoFactory::new()
        .info(i)
        .used_registers()
        .iter()
        .any(|r| {
            r.register().full_register() == reg
                && matches!(
                    r.access(),
                    OpAccess::Write
                        | OpAccess::ReadWrite
                        | OpAccess::CondWrite
                        | OpAccess::ReadCondWrite
                )
        })
}
// A call through memory writes only its return address.
fn writes_memory(i: &Instruction) -> bool {
    i.op0_kind() == OpKind::Memory
        && i.flow_control() != FlowControl::IndirectCall
        && InstructionInfoFactory::new()
            .info(i)
            .used_memory()
            .iter()
            .any(|m| {
                matches!(
                    m.access(),
                    OpAccess::Write
                        | OpAccess::ReadWrite
                        | OpAccess::CondWrite
                        | OpAccess::ReadCondWrite
                )
            })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Layout {
    size: i64,
    capacity: i64,
    data: i64,
    small: u64,
}
impl Layout {
    fn encoded(self) -> Option<u32> {
        if !(0..64).contains(&self.size)
            || !(0..64).contains(&self.capacity)
            || !(0..64).contains(&self.data)
            || !(1..64).contains(&self.small)
            || self.size == self.capacity
            || self.data == self.size
            || self.data == self.capacity
        {
            return None;
        }
        Some(
            2 | (self.size as u32) << 2
                | (self.capacity as u32) << 8
                | (self.data as u32) << 14
                | (self.small as u32) << 20,
        )
    }
}
fn cleanup(consumer: u64, code: &mut impl FnMut(u64) -> Option<Vec<u8>>) -> Option<Layout> {
    let bytes = code(consumer)?;
    let calls: Vec<_> = Decoder::with_ip(64, &bytes, consumer, DecoderOptions::NONE)
        .into_iter()
        .filter(|i| i.flow_control() == FlowControl::Call)
        .collect();
    let target = calls.first()?.near_branch_target(); // callable_consumer proved this cleanup's argument.
    let analysis = flow(target, initial(), &mut |at, _| code(at), &mut 32768)?;
    let mut layout = Layout {
        size: -1,
        capacity: -1,
        data: -1,
        small: 0,
    };
    for (pc, state) in &analysis.states {
        let i = &analysis.instructions[pc];
        if i.mnemonic() != Mnemonic::Mov || i.op0_kind() != OpKind::Memory {
            continue;
        }
        let Some((Storage::Descriptor, at)) = state.address(i) else {
            continue;
        };
        match (
            i.memory_size().size(),
            state.read(i, 1, i.memory_size().size()),
        ) {
            (8, Some(Value::Integer(0))) => layout.size = at - OBJECT,
            (8, Some(Value::Integer(small))) if small > 0 => {
                layout.capacity = at - OBJECT;
                layout.small = small;
            }
            (1, Some(Value::Integer(0))) => layout.data = at - OBJECT,
            _ => return None,
        }
    }
    layout.encoded()?;
    Some(layout)
}
fn copy_tail(
    analysis: &Flow,
    start: u64,
    destination: Register,
    layout: Layout,
    inplace: bool,
) -> bool {
    let mut pending = vec![(start, false, inplace)];
    let mut seen = HashSet::new();
    let mut returned = false;
    while let Some((pc, mut nul, mut published)) = pending.pop() {
        if !seen.insert((pc, nul, published)) {
            continue;
        }
        let (Some(i), Some(state)) = (analysis.instructions.get(&pc), analysis.states.get(&pc))
        else {
            return false;
        };
        if i.flow_control() == FlowControl::Return {
            if !nul || !published {
                return false;
            }
            returned = true;
            continue;
        }
        let mut allowed_store = matches!(state.address(i), Some((Storage::Stack, _)));
        if i.mnemonic() == Mnemonic::Mov && i.op0_kind() == OpKind::Memory {
            if i.memory_size().size() == 1
                && i.memory_base().full_register() == destination
                && state.register(i.memory_index()) == Some(LENGTH)
                && i.memory_index_scale() == 1
                && i.memory_displacement64() == 0
                && state.read(i, 1, 1) == Some(Value::Integer(0))
            {
                nul = true;
                allowed_store = true;
            }
            if state.address(i) == Some((Storage::Descriptor, OBJECT + layout.data)) {
                if i.op1_kind() != OpKind::Register
                    || i.op1_register().full_register() != destination
                {
                    return false;
                }
                published = true;
                allowed_store = true;
            }
        }
        if writes_memory(i) && !allowed_store {
            return false;
        }
        if writes(i, destination) && (!nul || !published) {
            return false;
        }
        let Some(next) = next(i) else {
            return false;
        };
        pending.extend(next.into_iter().map(|pc| (pc, nul, published)));
    }
    returned
}
fn builder(
    entry: u64,
    layout: Layout,
    code: &mut impl FnMut(u64) -> Option<Vec<u8>>,
    byte_copy: &mut impl FnMut(u64) -> bool,
) -> bool {
    let Some(analysis) = flow(entry, initial(), &mut |at, _| code(at), &mut 32768) else {
        return false;
    };
    if !complete(&analysis) {
        return false;
    }
    let mut copies = HashSet::new();
    for (pc, state) in &analysis.states {
        let i = &analysis.instructions[pc];
        if i.flow_control() != FlowControl::Call
            || state.register(Register::RDX) != Some(DATA)
            || state.register(Register::R8) != Some(LENGTH)
            || !byte_copy(i.near_branch_target())
        {
            continue;
        }
        let Some(previous) = analysis
            .instructions
            .values()
            .find(|before| before.next_ip() == *pc)
        else {
            continue;
        };
        if previous.mnemonic() != Mnemonic::Mov
            || previous.op0_register() != Register::RCX
            || previous.op1_kind() != OpKind::Register
        {
            continue;
        }
        let destination = previous.op1_register().full_register();
        let inplace = preceded(&analysis, entry, previous.ip(), |at| {
            let i = &analysis.instructions[&at];
            if at == previous.ip() || !writes(i, destination) {
                return None;
            }
            let state = &analysis.states[&at];
            Some(
                i.mnemonic() == Mnemonic::Mov
                    && matches!(state.read(i, 1, 8),
                Some(Value::Pointer(Storage::Descriptor, value)) if value == OBJECT + layout.data)
                    || i.mnemonic() == Mnemonic::Mov
                        && state.read(i, 1, 8)
                            == Some(Value::Bytes(Storage::Descriptor, OBJECT + layout.data, 8)),
            )
        });
        let sized = preceded(&analysis, entry, *pc, |at| {
            let i = &analysis.instructions[&at];
            let state = &analysis.states[&at];
            (i.op0_kind() == OpKind::Memory
                && state.address(i) == Some((Storage::Descriptor, OBJECT + layout.size)))
            .then(|| {
                i.mnemonic() == Mnemonic::Mov
                    && i.memory_size().size() == 8
                    && state.read(i, 1, 8) == Some(LENGTH)
            })
        });
        let tail = copy_tail(&analysis, i.next_ip(), destination, layout, inplace);
        if sized && tail {
            copies.insert(*pc);
        }
    }
    let returns: Vec<_> = analysis
        .states
        .keys()
        .filter(|pc| analysis.instructions[pc].flow_control() == FlowControl::Return)
        .collect();
    !copies.is_empty()
        && !returns.is_empty()
        && returns.into_iter().all(|pc| {
            preceded(&analysis, entry, *pc, |at| {
                copies.contains(&at).then_some(true)
            })
        })
}

pub(super) fn payload(
    analysis: &Flow,
    entry: u64,
    consumer_pc: u64,
    consumer: u64,
    code: &mut impl FnMut(u64) -> Option<Vec<u8>>,
    byte_copy: &mut impl FnMut(u64) -> bool,
) -> Option<u32> {
    let layout = cleanup(consumer, code)?;
    if !complete(analysis) {
        return None;
    }
    let Value::Pointer(Storage::Stack, payload) =
        analysis.states[&consumer_pc].register(Register::RDX)?
    else {
        return None;
    };
    let mut producers = HashSet::new();
    for (pc, state) in &analysis.states {
        let i = &analysis.instructions[pc];
        if i.flow_control() != FlowControl::Call
            || state.register(Register::RCX) != Some(Value::Pointer(Storage::Stack, payload))
        {
            continue;
        }
        let fresh = [
            (layout.data, 0),
            (layout.size, 0),
            (layout.capacity, layout.small),
        ]
        .into_iter()
        .all(|(offset, value)| {
            state.stack.get(&(payload + offset)) == Some(&(8, Value::Integer(value)))
        });
        if fresh && builder(i.near_branch_target(), layout, code, byte_copy) {
            producers.insert(*pc);
        }
    }
    preceded(analysis, entry, consumer_pc, |at| {
        if producers.contains(&at) {
            return Some(true);
        }
        if at == consumer_pc {
            return None;
        }
        let i = &analysis.instructions[&at];
        let state = &analysis.states[&at];
        (matches!(
            i.flow_control(),
            FlowControl::Call | FlowControl::IndirectCall
        ) || writes_memory(i)
            && state.address(i).is_none_or(|(storage, at)| {
                storage != Storage::Stack
                    || at >= payload && at < payload + layout.capacity.max(layout.size) + 8
            }))
        .then_some(false)
    })
    .then(|| layout.encoded())
    .flatten()
}
