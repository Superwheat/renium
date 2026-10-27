//! BoundYieldFuncDesc's RTTI describes the public arguments, not its hidden
//! continuations. Qualify BOTH actual callbacks at the member dispatch.
//! The native adapter supports MSVC std::function and the inline tagged
//! callable; descriptor RTTI alone cannot distinguish these incompatible ABIs.
use super::*;
#[path = "windows_functions_continuation.rs"]
mod consumer;

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize)]
#[repr(u32)]
pub(in super::super::super) enum ContinuationAbi {
    Msvc = 1,
    Tagged = 2,
}

const ERROR_CALLBACK_SUFFIX: &str =
    "@@XV?$basic_string@DU?$char_traits@D@std@@V?$allocator@D@2@@std@@@std@@";

fn error_callback_type(kind: &str) -> bool {
    kind.starts_with(".?AV?$_Func_impl_no_alloc@") && kind.ends_with(ERROR_CALLBACK_SUFFIX)
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

fn callback_layout(state: &State, argument: usize) -> Option<(ContinuationAbi, u64)> {
    if let Some(table) = callback_table(state, argument) {
        return Some((ContinuationAbi::Msvc, table));
    }
    let object = callback_object(state, argument)?;
    match state.stack.get(&(object + 24)) {
        Some((8, Value::TaggedPointer(table, 6))) if table % 8 == 0 => {
            Some((ContinuationAbi::Tagged, *table))
        }
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

fn verify_callback(
    memory: &ProcessMemory,
    studio: &ModuleEntry,
    layout: &PackageLayout,
    callback: (ContinuationAbi, u64),
    error: bool,
) -> Result<()> {
    let (abi, table) = callback;
    if abi == ContinuationAbi::Msvc {
        let kind = rtti_type_for_vtable(memory, studio, table as usize)
            .context("Native continuation has no complete callable identity")?;
        anyhow::ensure!(
            if error {
                error_callback_type(&kind)
            } else {
                kind.starts_with(".?AV?$_Func_impl_no_alloc@") && kind.ends_with("@std@@")
            },
            "Native continuation signature changed"
        );
        vtable_methods(memory, studio, layout, table as usize)?;
    } else {
        // Inline nontrivial callable: invocation, copy, move, destruction.
        // Follow the actual constructor's table, not an adjacent RTTI match.
        let mapped = observation::ImageView::open(&studio.path)?;
        let pe = PeImage::parse(mapped.bytes())?;
        for slot in 0..4 {
            let address = table as usize + slot * 8;
            let rva = address
                .checked_sub(studio.base)
                .context("Callable table is outside Studio")?;
            let expected = read_u64(pe.bytes, pe.rva_to_offset(rva)?)? as usize;
            let function = memory.read_u64(address)? as usize;
            anyhow::ensure!(
                function.checked_sub(studio.base) == expected.checked_sub(pe.image_base),
                "Native continuation operations changed in memory"
            );
            let code = dispatch_code(memory, studio, layout, function)?;
            if slot == 2 {
                anyhow::ensure!(
                    consumer::capture_move(&code),
                    "Native continuation move entry does not transfer its capture ownership"
                );
            }
            if error && slot == 0 {
                anyhow::ensure!(
                    consumer::error_invocation(&code),
                    "Native error continuation does not move a verified string argument"
                );
            }
        }
    }
    Ok(())
}

pub(in super::super::super) fn continuation_abi(
    memory: &ProcessMemory,
    studio: &ModuleEntry,
    layout: &PackageLayout,
    descriptor: usize,
    field: usize,
    argument: usize,
) -> Result<ContinuationAbi> {
    let table = memory.read_u64(descriptor)? as usize;
    let mut found = false;
    let mut rejected = false;
    let mut resolved = None;
    let mut failure = None;
    for method in vtable_methods(memory, studio, layout, table)? {
        let checked = dispatch_analysis(
            method as u64,
            &mut |address, _| dispatch_code(memory, studio, layout, address as usize).ok(),
            false,
            &mut |called, state| {
                let callbacks =
                    callback_layout(state, argument - 1).zip(callback_layout(state, argument));
                let compatible = called == field
                    && callbacks.is_some_and(|(success, error)| {
                        if success.0 != error.0 || resolved.is_some_and(|abi| abi != success.0) {
                            return false;
                        }
                        if let Err(error) = verify_callback(memory, studio, layout, success, false)
                            .and_then(|()| verify_callback(memory, studio, layout, error, true))
                        {
                            failure = Some(error);
                            return false;
                        }
                        resolved = Some(success.0);
                        true
                    });
                rejected |= !compatible;
                compatible
            },
        );
        if let Some(error) = failure.take() {
            return Err(error);
        }
        let fields = checked.context("Native continuation dispatcher analysis is incomplete")?;
        anyhow::ensure!(
            fields.is_empty() || fields == HashSet::from([field]),
            "Native continuation dispatcher is ambiguous"
        );
        found |= fields.contains(&field);
    }
    anyhow::ensure!(
        found && !rejected,
        "Studio's native continuation layout could not be verified for Play"
    );
    let abi = resolved.context("Studio's native continuation ABI is ambiguous")?;
    if abi == ContinuationAbi::Tagged {
        let member = memory.read_u64(descriptor + field)?;
        consumer::member_moves(member, argument, &mut |address| {
            dispatch_code(memory, studio, layout, address as usize).ok()
        })
        .context("Native continuation move/destroy contract could not be verified")?;
    }
    Ok(abi)
}

fn rtti_type_for_vtable(
    memory: &ProcessMemory,
    studio: &ModuleEntry,
    table: usize,
) -> Option<String> {
    let locator = memory.read_u64(table.checked_sub(8)?).ok()? as usize;
    if memory.read_u32(locator).ok()? != 1
        || memory.read_u32(locator + 4).ok()? != 0
        || studio
            .base
            .checked_add(memory.read_u32(locator + 20).ok()? as usize)?
            != locator
    {
        return None;
    }
    let descriptor = studio
        .base
        .checked_add(memory.read_u32(locator + 12).ok()? as usize)?;
    read_c_string(memory, descriptor + 16, 2048)
}

#[cfg(test)]
#[path = "windows_functions_yield_tests.rs"]
mod tests;
