//! BoundYieldFuncDesc's RTTI describes the public arguments, not its hidden
//! continuations. Qualify BOTH actual callbacks at the member dispatch.
//! The native adapter supports MSVC std::function and the inline tagged
//! callable; descriptor RTTI alone cannot distinguish these incompatible ABIs.
use super::*;

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize)]
#[repr(u32)]
pub(in super::super::super) enum ContinuationAbi {
    Msvc = 1,
    Tagged = 2,
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
