//! Execute only the certified inline, nontrivial path of a callable consumer.
//! Synthetic addresses distinguish the source, destination and operations;
//! they are never Studio addresses and no engine code is executed.
use super::*;

const SOURCE: i64 = 0x100000;
const DESTINATION: i64 = 0x200000;
const TABLE: u64 = 0x300000;
const MOVED: u8 = 1;
const COPIED: u8 = 2;
const DESTROYED: u8 = 4;

fn initial() -> State {
    State {
        registers: HashMap::from([
            (Register::RCX, Value::Pointer(Storage::Stack, DESTINATION)),
            (Register::RDX, Value::Pointer(Storage::Stack, SOURCE)),
            (Register::RSP, Value::Pointer(Storage::Stack, 0)),
        ]),
        stack: BTreeMap::new(),
        lua_callback: false,
    }
}

fn number(value: Value) -> Option<u64> {
    match value {
        Value::ImagePointer(value) => Some(value),
        Value::Pointer(Storage::Stack, value) => Some(value as u64),
        _ => None,
    }
}

fn read(state: &State, i: &Instruction, operand: u32) -> Option<Value> {
    match i.op_kind(operand) {
        OpKind::Immediate8
        | OpKind::Immediate8to64
        | OpKind::Immediate8to32
        | OpKind::Immediate32
        | OpKind::Immediate32to64 => Some(Value::ImagePointer(i.immediate(operand))),
        OpKind::Register => {
            let reg = i.op_register(operand);
            let value = state.register(reg)?;
            if matches!(reg.size(), 1 | 2 | 4) {
                if let Value::ImagePointer(value) = value {
                    Some(Value::ImagePointer(value & mask(reg.size())))
                } else {
                    None
                }
            } else {
                Some(value)
            }
        }
        OpKind::Memory => {
            let width = i.memory_size().size();
            state.read(i, operand, width).or_else(|| {
                let at = number(state.register(i.memory_base())?)? as i64
                    + i.memory_displacement64() as i64;
                (i.memory_index() == Register::None)
                    .then(|| state.stack.get(&at).map(|(_, value)| *value))?
            })
        }
        _ => None,
    }
}

fn mask(width: usize) -> u64 {
    if width >= 8 {
        u64::MAX
    } else {
        (1u64 << (width * 8)) - 1
    }
}

fn scalar(state: &State, i: &Instruction) -> Option<u64> {
    let left = read(state, i, 0).and_then(number);
    let right = read(state, i, 1).and_then(number);
    let bits = mask(i.op0_register().size());
    match i.mnemonic() {
        Mnemonic::Xor if i.op0_register() == i.op1_register() => Some(0),
        Mnemonic::And => Some(left? & right?),
        Mnemonic::Or => Some(left? | right?),
        Mnemonic::Not => Some(!left? & bits),
        Mnemonic::Sar | Mnemonic::Shr => Some((left? & bits) >> (right? & 63)),
        Mnemonic::Movzx => Some(
            right?
                & mask(if i.op1_kind() == OpKind::Memory {
                    i.memory_size().size()
                } else {
                    i.op1_register().size()
                }),
        ),
        _ => None,
    }
}

fn step(state: &mut State, i: &Instruction) {
    if let Some(value) = scalar(state, i) {
        state
            .registers
            .insert(i.op0_register().full_register(), Value::ImagePointer(value));
    } else if matches!(
        i.mnemonic(),
        Mnemonic::Mov
            | Mnemonic::Movups
            | Mnemonic::Movaps
            | Mnemonic::Movdqu
            | Mnemonic::Movdqa
            | Mnemonic::Movsd
    ) {
        let value = read(state, i, 1);
        if i.op0_kind() == OpKind::Register {
            state.registers.remove(&i.op0_register().full_register());
            if let Some(value) = value {
                state
                    .registers
                    .insert(i.op0_register().full_register(), value);
            }
        } else if let Some((Storage::Stack, at)) = state.address(i) {
            state.store(at, i.memory_size().size(), value);
        }
    } else {
        state.step(i);
    }
}

fn condition(state: &State, i: &Instruction) -> Option<bool> {
    let left = number(read(state, i, 0)?)?;
    let right = number(read(state, i, 1)?)?;
    match i.mnemonic() {
        Mnemonic::Cmp => Some(left == right),
        Mnemonic::Test => Some(left & right == 0),
        _ => None,
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Transfer {
    Move,
    Copy,
    Destroy,
}

fn transfer(code: &[u8], kind: Transfer) -> bool {
    let instructions = Decoder::new(64, code, DecoderOptions::NONE)
        .into_iter()
        .map(|i| (i.ip(), i))
        .collect::<HashMap<_, _>>();
    let mut state = initial();
    if kind == Transfer::Destroy {
        state
            .registers
            .insert(Register::RCX, Value::Pointer(Storage::Stack, SOURCE));
    }
    state.store(SOURCE + 24, 8, Some(Value::ImagePointer(TABLE | 6)));
    state.store(DESTINATION + 24, 8, Some(Value::ImagePointer(0)));
    for slot in 0..4 {
        state.store(
            TABLE as i64 + slot * 8,
            8,
            Some(Value::ImagePointer(TABLE + slot as u64 * 8)),
        );
    }
    let mut pc = 0;
    let mut equal = None;
    let mut moved = false;
    let mut destroyed = false;
    for _ in 0..512 {
        let Some(i) = instructions.get(&pc) else {
            return false;
        };
        if i.is_invalid() {
            return false;
        }
        if matches!(i.mnemonic(), Mnemonic::Cmp | Mnemonic::Test) {
            equal = condition(&state, i);
        } else if matches!(i.mnemonic(), Mnemonic::And | Mnemonic::Or | Mnemonic::Xor) {
            equal = scalar(&state, i).map(|value| value == 0);
        } else if i.rflags_modified() != 0 || i.flow_control() == FlowControl::IndirectCall {
            equal = None;
        }
        match i.flow_control() {
            FlowControl::ConditionalBranch => {
                let take = match (i.mnemonic(), equal) {
                    (Mnemonic::Je, Some(value)) => value,
                    (Mnemonic::Jne, Some(value)) => !value,
                    _ => return false,
                };
                pc = if take {
                    i.near_branch_target()
                } else {
                    i.next_ip()
                };
                continue;
            }
            FlowControl::UnconditionalBranch => {
                pc = i.near_branch_target();
                continue;
            }
            FlowControl::IndirectCall => {
                let target = read(&state, i, 0).and_then(number);
                let source = Some(Value::Pointer(Storage::Stack, SOURCE));
                match target {
                    Some(value)
                        if value == TABLE + if kind == Transfer::Copy { 8 } else { 16 }
                            && kind != Transfer::Destroy
                            && !moved
                            && state.register(Register::RDX) == source
                            && matches!(state.register(Register::RCX), Some(Value::Pointer(Storage::Stack, at)) if at != SOURCE) =>
                    {
                        moved = true
                    }
                    Some(value)
                        if value == TABLE + 24
                            && kind != Transfer::Copy
                            && (moved || kind == Transfer::Destroy)
                            && !destroyed
                            && state.register(Register::RCX) == source =>
                    {
                        destroyed = true
                    }
                    _ => return false,
                }
            }
            FlowControl::Return => {
                return match kind {
                    Transfer::Copy => {
                        moved
                            && !destroyed
                            && state.stack.get(&(SOURCE + 24))
                                == Some(&(8, Value::ImagePointer(TABLE | 6)))
                            && state.stack.get(&(DESTINATION + 24))
                                == Some(&(8, Value::ImagePointer(TABLE | 6)))
                    }
                    Transfer::Destroy => {
                        destroyed
                            && state.stack.get(&(SOURCE + 24)) == Some(&(8, Value::ImagePointer(0)))
                    }
                    Transfer::Move => false,
                };
            }
            FlowControl::Next => (),
            _ => return false,
        }
        step(&mut state, i);
        if kind != Transfer::Destroy
            && state.stack.get(&(SOURCE + 24)) == Some(&(8, Value::ImagePointer(0)))
        {
            return kind == Transfer::Move && moved && destroyed;
        }
        pc = i.next_ip();
    }
    false
}

pub(super) fn tagged_mover(code: &[u8]) -> bool {
    transfer(code, Transfer::Move)
}

pub(super) fn capture_move(code: &[u8]) -> bool {
    let mut state = initial();
    for offset in [0, 8, 16] {
        state.store(
            SOURCE + offset,
            8,
            Some(Value::Bytes(Storage::Descriptor, offset, 8)),
        );
    }
    for i in Decoder::new(64, code, DecoderOptions::NONE) {
        if i.flow_control() == FlowControl::Return {
            return [0, 8, 16].iter().all(|offset| {
                state.stack.get(&(DESTINATION + offset))
                    == Some(&(8, Value::Bytes(Storage::Descriptor, *offset, 8)))
            }) && [0, 8].iter().all(|offset| {
                state.stack.get(&(SOURCE + offset)) == Some(&(8, Value::ImagePointer(0)))
            });
        }
        if i.is_invalid() || i.flow_control() != FlowControl::Next {
            return false;
        }
        step(&mut state, &i);
    }
    false
}

pub(super) fn error_invocation(code: &[u8]) -> bool {
    let mut state = initial();
    state.store(SOURCE, 32, Some(Value::Bytes(Storage::Descriptor, 0, 32)));
    state.store(DESTINATION + 16, 8, Some(Value::ImagePointer(TABLE)));
    for i in Decoder::new(64, code, DecoderOptions::NONE) {
        if i.flow_control() == FlowControl::IndirectCall {
            let Some(Value::Pointer(Storage::Stack, output)) = state.register(Register::RDX) else {
                return false;
            };
            // MSVC string move: transfer all 32 bytes, then restore an empty
            // short string (size 0, capacity 15, leading NUL) in the source.
            return read(&state, &i, 0) == Some(Value::ImagePointer(TABLE))
                && state.stack.get(&output)
                    == Some(&(16, Value::Bytes(Storage::Descriptor, 0, 16)))
                && state.stack.get(&(output + 16))
                    == Some(&(16, Value::Bytes(Storage::Descriptor, 16, 16)))
                && state.stack.get(&(SOURCE + 16)) == Some(&(8, Value::ImagePointer(0)))
                && state.stack.get(&(SOURCE + 24)) == Some(&(8, Value::ImagePointer(15)))
                && state.stack.get(&SOURCE) == Some(&(1, Value::ImagePointer(0)));
        }
        if i.is_invalid() || i.flow_control() != FlowControl::Next {
            return false;
        }
        step(&mut state, &i);
    }
    false
}

pub(super) fn member_moves(
    entry: u64,
    argument: usize,
    code: &mut impl FnMut(u64) -> Option<Vec<u8>>,
) -> Option<()> {
    let mut initial = initial();
    initial.registers.retain(|reg, _| *reg == Register::RSP);
    for arg in [argument - 1, argument] {
        let value = Value::Pointer(Storage::Stack, SOURCE + arg as i64 * 4096);
        if arg < 4 {
            initial.registers.insert(
                [Register::RCX, Register::RDX, Register::R8, Register::R9][arg],
                value,
            );
        } else {
            initial.store(8 + arg as i64 * 8, 8, Some(value));
        }
    }
    let sources = [argument - 1, argument]
        .map(|arg| Value::Pointer(Storage::Stack, SOURCE + arg as i64 * 4096));
    let mut budget = 32768;
    let uses = member_transfers(entry, initial, sources, code, &mut budget, 0)?;
    (budget != 0
        && uses.iter().all(|flags| {
            flags & MOVED != 0 || flags & (COPIED | DESTROYED) == (COPIED | DESTROYED)
        }))
    .then_some(())
}

fn member_transfers(
    entry: u64,
    initial: State,
    sources: [Value; 2],
    code: &mut impl FnMut(u64) -> Option<Vec<u8>>,
    budget: &mut usize,
    depth: usize,
) -> Option<[u8; 2]> {
    if depth > 8 {
        return None;
    }
    let Flow {
        instructions,
        states,
    } = flow(entry, initial, &mut |at, _| code(at), budget)?;
    let mut uses = [0; 2];
    for (pc, mut state) in states {
        let i = &instructions[&pc];
        let target = match i.flow_control() {
            FlowControl::Call => i.near_branch_target(),
            FlowControl::UnconditionalBranch
                if !instructions.contains_key(&i.near_branch_target()) =>
            {
                i.near_branch_target()
            }
            FlowControl::IndirectCall => match read(&state, i, 0) {
                Some(Value::ImagePointer(target)) => target,
                _ => continue,
            },
            _ => continue,
        };
        let passed = [Register::RCX, Register::RDX, Register::R8, Register::R9]
            .iter()
            .any(|reg| state.register(*reg).is_some_and(|v| sources.contains(&v)));
        let stack_passed = state.register(Register::RSP).is_some_and(|rsp| {
            let Value::Pointer(Storage::Stack, at) = rsp else {
                return false;
            };
            (4..8).any(|slot| {
                state
                    .stack
                    .get(&(at + slot * 8))
                    .is_some_and(|(_, v)| sources.contains(v))
            })
        });
        if !passed && !stack_passed {
            continue;
        }
        let Some(bytes) = code(target) else { continue };
        let mut consumed = false;
        for (arg, source) in sources.iter().enumerate() {
            if state.register(Register::RDX) == Some(*source) {
                if tagged_mover(&bytes) {
                    uses[arg] |= MOVED;
                    consumed = true;
                } else if transfer(&bytes, Transfer::Copy) {
                    uses[arg] |= COPIED;
                    consumed = true;
                }
            } else if state.register(Register::RCX) == Some(*source)
                && transfer(&bytes, Transfer::Destroy)
            {
                uses[arg] |= DESTROYED;
                consumed = true;
            }
        }
        if consumed {
            continue;
        }
        // Forwarded continuations can be stack arguments or the capture of a
        // wrapper. Follow their addresses into the callee, including a known
        // capture constructor passed as a function pointer. A candidate that
        // cannot be analyzed contributes no ABI evidence.
        state.registers.retain(|reg, _| {
            matches!(
                reg,
                Register::RCX | Register::RDX | Register::R8 | Register::R9 | Register::RSP
            )
        });
        if i.flow_control() != FlowControl::UnconditionalBranch
            && let Some(Value::Pointer(Storage::Stack, at)) = state.register(Register::RSP)
        {
            state
                .registers
                .insert(Register::RSP, Value::Pointer(Storage::Stack, at - 8));
            state.store(at - 8, 8, None);
        }
        if let Some(found) = member_transfers(target, state, sources, code, budget, depth + 1) {
            for (used, found) in uses.iter_mut().zip(found) {
                *used |= found;
            }
        }
    }
    Some(uses)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixtures() -> Vec<serde_json::Value> {
        serde_json::from_str(include_str!(
            "../../../../native/tests/windows-continuation-consumers.json"
        ))
        .unwrap()
    }
    #[test]
    fn continuation_consumers_reject_move_only_tables_and_non_string_errors() {
        for row in fixtures() {
            let name = row["name"].as_str().unwrap();
            let code = hex(row["hex"].as_str().unwrap()).unwrap();
            match name {
                "move" | "taskMove" => assert!(tagged_mover(&code), "{name}"),
                "moveOnly" => assert!(!tagged_mover(&code)),
                "captureMove" => {
                    assert!(capture_move(&code));
                    let mut wrong = code.clone();
                    let at = wrong
                        .windows(4)
                        .position(|v| v == [0x4c, 0x89, 0x42, 8])
                        .unwrap();
                    wrong[at + 3] = 16;
                    assert!(!capture_move(&wrong));
                }
                "error" => {
                    assert!(error_invocation(&code));
                    let mut changed = code.clone();
                    let at = changed
                        .windows(8)
                        .position(|v| v == [0x48, 0xc7, 0x42, 0x18, 15, 0, 0, 0])
                        .unwrap();
                    changed[at + 4] = 7;
                    assert!(!error_invocation(&changed));
                }
                _ => (),
            }
        }
    }
    #[test]
    fn continuation_members_must_use_the_verified_consumer_for_both_arguments() {
        let rows = fixtures();
        for row in rows.iter().filter(|row| {
            matches!(
                row["name"].as_str(),
                Some("execute" | "invoke" | "teleport")
            )
        }) {
            let entry = row["entry"].as_u64().unwrap();
            let argument = if row["name"] == "teleport" { 5 } else { 3 };
            for slide in [0, 0x40000000] {
                for invalid in [false, true] {
                    let result = member_moves(entry + slide, argument, &mut |at| {
                        let row = rows
                            .iter()
                            .find(|row| row["entry"].as_u64() == at.checked_sub(slide))?;
                        let row = if invalid
                            && matches!(
                                row["name"].as_str(),
                                Some("move" | "taskMove" | "invokeMove")
                            ) {
                            rows.iter().find(|row| row["name"] == "moveOnly").unwrap()
                        } else {
                            row
                        };
                        Some(hex(row["hex"].as_str().unwrap()).unwrap())
                    });
                    assert_eq!(
                        result.is_some(),
                        !invalid,
                        "{} slide={slide:x} invalid={invalid}",
                        row["name"]
                    );
                }
            }
            if row["name"] == "teleport" {
                for missing in ["teleportForward", "wrappedCopy"] {
                    assert!(
                        member_moves(entry, argument, &mut |at| {
                            rows.iter().find_map(|row| {
                                (row["entry"].as_u64() == Some(at) && row["name"] != missing)
                                    .then(|| hex(row["hex"].as_str().unwrap()).unwrap())
                            })
                        })
                        .is_none(),
                        "missing {missing}"
                    );
                }
            }
        }
    }
}
