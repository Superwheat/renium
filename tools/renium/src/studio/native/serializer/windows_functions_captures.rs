//! Follow the actual HTTP continuations through engine aggregates and bound
//! async closures. All field offsets, sizes, targets and tables come from code.
use super::*;
const CAPTURE: i64 = 1 << 40; // Symbolic address space, never an engine address.

fn pointer(value: Value) -> Option<(Storage, i64)> {
    match value {
        Value::Pointer(storage @ (Storage::Stack | Storage::Capture), at) => Some((storage, at)),
        _ => None,
    }
}
fn contains(state: &State, at: i64) -> bool {
    state
        .stack
        .range(at..at.saturating_add(4096))
        .any(|(_, (_, v))| matches!(v, Value::Continuation(0 | 1 | 4)))
}
fn empty() -> State {
    State {
        registers: HashMap::from([(Register::RSP, Value::Pointer(Storage::Stack, 0))]),
        stack: BTreeMap::new(),
        lua_callback: false,
        zero: None,
        complete_receiver: false,
    }
}
fn dominates_returns(analysis: &Flow, entry: u64, required: u64) -> bool {
    let mut queue = vec![entry];
    let mut seen = HashSet::new();
    while let Some(pc) = queue.pop() {
        if pc == required || !seen.insert(pc) {
            continue;
        }
        let Some(i) = analysis.instructions.get(&pc) else {
            return false;
        };
        match i.flow_control() {
            FlowControl::Return => return false,
            FlowControl::Exception | FlowControl::Interrupt => {}
            FlowControl::ConditionalBranch => {
                queue.push(i.next_ip());
                queue.push(i.near_branch_target());
            }
            FlowControl::UnconditionalBranch => queue.push(i.near_branch_target()),
            FlowControl::IndirectBranch => return false,
            _ => queue.push(i.next_ip()),
        }
    }
    true
}
pub(in super::super) struct Updates {
    pub fields: Vec<(i64, usize, Value)>,
    pub consumed: Vec<i64>,
    pub returned: Option<Value>,
}
pub(in super::super) fn copied_fields(
    parent: &State,
    entry: u64,
    code: &mut impl FnMut(u64) -> Option<Vec<u8>>,
    depth: usize,
) -> Option<Updates> {
    let source = pointer(parent.register(Register::RDX)?)?;
    let destination = pointer(parent.register(Register::RCX)?)?;
    if depth > 3 || !contains(parent, source.1) {
        return None;
    }
    let mut initial = empty();
    initial.registers.extend([
        (Register::RCX, Value::Pointer(Storage::Descriptor, 32768)),
        (Register::RDX, Value::Pointer(Storage::Descriptor, 4096)),
    ]);
    let analysis = flow(entry, initial, &mut |at, _| code(at), &mut 32768)?;
    let mut updates = Updates {
        fields: Vec::new(),
        consumed: Vec::new(),
        returned: None,
    };
    for (pc, state) in &analysis.states {
        let i = &analysis.instructions[pc];
        if !dominates_returns(&analysis, entry, *pc) {
            continue;
        }
        if i.mnemonic() == Mnemonic::Mov && i.op0_kind() == OpKind::Memory {
            if let Some((Storage::Descriptor, to)) = state.address(i)
                && let Some(Value::Bytes(Storage::Descriptor, from, width)) =
                    state.read(i, 1, i.memory_size().size())
                && (32768..36864).contains(&to)
                && (4096..8192).contains(&from)
                && let Some((size, value)) = parent.stack.get(&(source.1 + from - 4096))
                && *size == width
            {
                updates
                    .fields
                    .push((destination.1 + to - 32768, width, *value));
            }
        } else if i.flow_control() == FlowControl::Call {
            copy_field_call(
                parent,
                state,
                i.near_branch_target(),
                (source, destination),
                code,
                depth,
                &mut updates,
            );
        }
    }
    if updates.fields.is_empty() {
        return None;
    }
    let returned = analysis
        .states
        .iter()
        .filter(|(pc, _)| analysis.instructions[pc].flow_control() == FlowControl::Return)
        .all(|(_, state)| {
            state.register(Register::RAX) == Some(Value::Pointer(Storage::Descriptor, 32768))
        })
        .then_some(Value::Pointer(destination.0, destination.1));
    updates.returned = returned;
    Some(updates)
}
fn copy_field_call(
    parent: &State,
    state: &State,
    target: u64,
    bindings: ((Storage, i64), (Storage, i64)),
    code: &mut impl FnMut(u64) -> Option<Vec<u8>>,
    depth: usize,
    updates: &mut Updates,
) {
    let (source, destination) = bindings;
    let (
        Some(Value::Pointer(Storage::Descriptor, from)),
        Some(Value::Pointer(Storage::Descriptor, to)),
    ) = (state.register(Register::RDX), state.register(Register::RCX))
    else {
        return;
    };
    if !(4096..8192).contains(&from) || !(32768..36864).contains(&to) {
        return;
    }
    let from = Value::Pointer(source.0, source.1 + from - 4096);
    let to = Value::Pointer(destination.0, destination.1 + to - 32768);
    let mut call = parent.clone();
    call.registers
        .extend([(Register::RDX, from), (Register::RCX, to)]);
    if let Some((at, origin, preserves)) = copied_origin(&call, target, code) {
        updates.fields.push((at, 32, Value::Continuation(origin)));
        if !preserves {
            updates.consumed.push(pointer(from).unwrap().1);
        }
    } else if let Some(nested) = copied_fields(&call, target, code, depth + 1) {
        updates.fields.extend(nested.fields);
        updates.consumed.extend(nested.consumed);
    }
}

pub(super) fn forwarded(state: &State) -> Option<State> {
    let Value::Pointer(Storage::Stack, rsp) = state.register(Register::RSP)? else {
        return None;
    };
    let shift = state
        .stack
        .keys()
        .next_back()
        .copied()
        .unwrap_or(0)
        .max(CAPTURE)
        + 65536;
    let convert = |v| match v {
        Value::Pointer(Storage::Stack, at) => Value::Pointer(Storage::Capture, at + shift),
        other => other,
    };
    let mut child = empty();
    let mut regions = Vec::new();
    let mut passed = false;
    for (index, reg) in [Register::RCX, Register::RDX, Register::R8, Register::R9]
        .into_iter()
        .enumerate()
        .map(|(i, r)| (i, Some(r)))
        .chain((4..8).map(|i| (i, None)))
    {
        let value = if let Some(reg) = reg {
            state.register(reg)
        } else {
            state.stack.get(&(rsp + index as i64 * 8)).map(|(_, v)| *v)
        };
        let Some(value) = value else {
            continue;
        };
        let value = if let Some(origin) = callback_origin(state, value) {
            passed = true;
            Value::Continuation(origin)
        } else if let Some((_, at)) = pointer(value) {
            if !contains(state, at) {
                continue;
            }
            regions.push(at);
            passed = true;
            convert(value)
        } else {
            continue;
        };
        if let Some(reg) = reg {
            child.registers.insert(reg, value);
        } else {
            child.stack.insert((index as i64 + 1) * 8, (8, value));
        }
    }
    for (at, (size, value)) in &state.stack {
        if regions
            .iter()
            .any(|base| *at >= *base && *at < base.saturating_add(4096))
        {
            child.stack.insert(
                if *at < CAPTURE { at + shift } else { *at },
                (*size, convert(*value)),
            );
        }
    }
    passed.then_some(child)
}

// Verify the heap-callable constructor against symbolic helper ABI values:
// allocation, the actual supplied mover, and publication with that exact table.
fn heap_constructor(bytes: &[u8], entry: u64) -> bool {
    let mut state = CallableState::default();
    state.regs.extend([
        (Register::RCX, 0x10000),
        (Register::RDX, 0x1000),
        (Register::R8, 8),
        (Register::R9, 0x20000),
        (Register::RSP, 0x100000),
    ]);
    state
        .memory
        .extend([(0x100028, 0x80000), (0x100030, 0x90000)]);
    let mut allocated = false;
    let mut moved = false;
    for i in Decoder::with_ip(64, bytes, entry, DecoderOptions::NONE) {
        match i.flow_control() {
            FlowControl::Call
                if !allocated
                    && state.reg(Register::RCX) == Some(0x1000)
                    && state.reg(Register::RDX) == Some(8) =>
            {
                state.called();
                state.regs.insert(Register::RAX, 0x30000);
                allocated = true;
            }
            FlowControl::IndirectCall
                if allocated
                    && !moved
                    && state.value(&i, 0) == Some(0x90000)
                    && state.reg(Register::RCX) == Some(0x30000)
                    && state.reg(Register::RDX) == Some(0x20000) =>
            {
                state.called();
                moved = true;
            }
            FlowControl::Return => {
                return moved
                    && state.memory.get(&0x10000) == Some(&0x30000)
                    && state.memory.get(&0x10018) == Some(&0x80004);
            }
            FlowControl::Next if state.step(&i).is_some() => {}
            _ => return false,
        }
    }
    false
}
pub(in super::super) fn heap_fields(
    state: &State,
    target: u64,
    code: &mut impl FnMut(u64) -> Option<Vec<u8>>,
) -> Option<(u64, i64, Updates)> {
    let source = state.register(Register::R9)?;
    let (_, at) = pointer(source)?;
    if !contains(state, at) {
        return None;
    }
    let Value::Pointer(Storage::Stack, rsp) = state.register(Register::RSP)? else {
        return None;
    };
    let (8, Value::ImagePointer(table)) = *state.stack.get(&(rsp + 32))? else {
        return None;
    };
    let (8, Value::ImagePointer(mover)) = *state.stack.get(&(rsp + 40))? else {
        return None;
    };
    let Value::Integer(size) = state.register(Register::RDX)? else {
        return None;
    };
    if size > 4096 || !heap_constructor(&code(target)?, target) {
        return None;
    }
    let heap = state
        .stack
        .keys()
        .next_back()
        .copied()
        .unwrap_or(0)
        .max(CAPTURE)
        + 65536;
    let mut copying = state.clone();
    copying.registers.extend([
        (Register::RCX, Value::Pointer(Storage::Capture, heap)),
        (Register::RDX, source),
    ]);
    let updates = copied_fields(&copying, mover, code, 0)?;
    if updates
        .fields
        .iter()
        .any(|(at, width, _)| *at < heap || (*at - heap) as usize + width > size as usize)
    {
        return None;
    }
    Some((table, heap, updates))
}
pub(super) fn heap_invocation(
    state: &State,
    target: u64,
    code: &mut impl FnMut(u64) -> Option<Vec<u8>>,
    operation: &mut impl FnMut(u64) -> Option<u64>,
) -> Option<(u64, State)> {
    let (table, heap, updates) = heap_fields(state, target, code)?;
    let mut child = empty();
    for (at, width, value) in updates.fields {
        child.store(at, width, Some(value));
    }
    child
        .registers
        .insert(Register::RCX, Value::Pointer(Storage::Capture, heap));
    Some((operation(table)?, child))
}

pub(super) fn signature(state: &State) -> Vec<(u32, i64, u64)> {
    let mut fields = Vec::new();
    let mut add = |key: u32, value: Value| {
        if let Some(origin) = callback_origin(state, value) {
            fields.push((key, -1, origin as u64));
        } else if let Some((_, base)) = pointer(value) {
            for (at, (_, value)) in state.stack.range(base..base.saturating_add(4096)) {
                match value {
                    Value::Continuation(origin) => fields.push((key, at - base, *origin as u64)),
                    Value::ImagePointer(target) => fields.push((key, at - base, *target)),
                    _ => {}
                }
            }
        }
    };
    for (reg, value) in &state.registers {
        if *reg != Register::RSP {
            add(*reg as u32, *value);
        }
    }
    for (at, (_, value)) in state.stack.range(0..72) {
        add(1024 + *at as u32, *value);
    }
    fields.sort_unstable();
    fields
}
