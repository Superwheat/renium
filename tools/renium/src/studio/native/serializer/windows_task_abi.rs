//! Qualify the actual task consumer, not just the submitter's entry signature.
use super::*;
use iced_x86::{Decoder, DecoderOptions, FlowControl, Mnemonic, OpKind, Register};
use properties::ContinuationAbi;

pub(super) fn from_report(
    operation: NativeOperation,
    report: &serde_json::Value,
) -> Result<Option<u32>> {
    if matches!(
        operation,
        NativeOperation::UndoSelection
            | NativeOperation::PackageNotices
            | NativeOperation::AudioGate
            | NativeOperation::StudioActions
            | NativeOperation::Input
            | NativeOperation::ViewportCapture
    ) {
        return Ok(None);
    }
    match report["taskContinuationAbi"].as_u64() {
        Some(value @ (1 | 2)) => Ok(Some(value as u32)),
        _ => bail!("Native task continuation ABI is missing or unsupported"),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Origin {
    Context,
    Work,
    Receiver(usize),
    Table(usize),
}

fn submit_fields(code: &[u8], at: u64) -> Option<(usize, usize)> {
    let mut regs = HashMap::from([
        (Register::RCX, Origin::Context),
        (Register::RDX, Origin::Work),
    ]);
    let mut found = Vec::new();
    for i in Decoder::with_ip(64, code, at, DecoderOptions::NONE) {
        if i.is_invalid() {
            return None;
        }
        if i.flow_control() == FlowControl::IndirectCall
            && i.op0_kind() == OpKind::Memory
            && i.memory_index() == Register::None
            && let Some(Origin::Table(field)) = regs.get(&i.memory_base()).copied()
            && regs.get(&Register::RCX) == Some(&Origin::Receiver(field))
            && regs.get(&Register::RDX) == Some(&Origin::Work)
        {
            found.push((field, i.memory_displacement64() as usize));
        }
        if matches!(
            i.flow_control(),
            FlowControl::Call | FlowControl::IndirectCall
        ) {
            for r in [
                Register::RAX,
                Register::RCX,
                Register::RDX,
                Register::R8,
                Register::R9,
                Register::R10,
                Register::R11,
            ] {
                regs.remove(&r);
            }
        } else if i.op0_kind() == OpKind::Register
            && !matches!(
                i.mnemonic(),
                Mnemonic::Test | Mnemonic::Cmp | Mnemonic::Push
            )
        {
            let value = submit_value(&i, &regs);
            let dst = i.op0_register().full_register();
            regs.remove(&dst);
            if let Some(value) = value {
                regs.insert(dst, value);
            }
        }
    }
    (found.len() == 1).then(|| found[0])
}

fn submit_value(i: &iced_x86::Instruction, regs: &HashMap<Register, Origin>) -> Option<Origin> {
    if i.mnemonic() != Mnemonic::Mov {
        return None;
    }
    if i.op1_kind() == OpKind::Register {
        return regs.get(&i.op1_register().full_register()).copied();
    }
    if i.op1_kind() != OpKind::Memory
        || i.memory_index() != Register::None
        || i.memory_size().size() != 8
    {
        return None;
    }
    match regs.get(&i.memory_base())? {
        Origin::Context => Some(Origin::Receiver(i.memory_displacement64() as usize)),
        Origin::Receiver(field) if i.memory_displacement64() == 0 => Some(Origin::Table(*field)),
        _ => None,
    }
}

fn mover(code: &[u8], at: u64) -> Option<usize> {
    let mut output = false;
    for i in Decoder::with_ip(64, code, at, DecoderOptions::NONE) {
        if i.is_invalid() {
            return None;
        }
        if i.flow_control() == FlowControl::Call {
            return output.then_some(i.near_branch_target() as usize);
        }
        if i.flow_control() != FlowControl::Next {
            return None;
        }
        if i.op0_kind() == OpKind::Register {
            if i.op0_register().full_register() == Register::RDX {
                return None;
            }
            if i.op0_register().full_register() == Register::RCX {
                output = i.mnemonic() == Mnemonic::Lea && i.memory_base() == Register::RSP;
            }
        }
    }
    None
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Value {
    Input(u8),
    Field(u8, usize),
    Table(ContinuationAbi),
    Result,
}

// Follow the move and destroy calls and the source ownership reset. These
// offsets are the supported callable contracts, never Studio object offsets.
fn mover_abi(code: &[u8]) -> Option<ContinuationAbi> {
    let mut regs = HashMap::from([
        (Register::RCX, Value::Input(0)),
        (Register::RDX, Value::Input(1)),
    ]);
    let mut moved = None;
    let mut destroyed = None;
    let mut reset = None;
    for i in Decoder::new(64, code, DecoderOptions::NONE) {
        if i.is_invalid() {
            return None;
        }
        if i.flow_control() == FlowControl::Return {
            break;
        }
        if i.flow_control() == FlowControl::IndirectCall {
            let Value::Table(abi) = *regs.get(&i.memory_base())? else {
                return None;
            };
            let args = (regs.get(&Register::RCX), regs.get(&Register::RDX));
            let slot = i.memory_displacement64();
            match (abi, slot, args) {
                (ContinuationAbi::Msvc, 8, (Some(Value::Field(1, 56)), Some(Value::Input(0))))
                | (ContinuationAbi::Tagged, 16, (Some(Value::Input(0)), Some(Value::Input(1)))) => {
                    moved = Some(abi)
                }
                (ContinuationAbi::Msvc, 32, (Some(Value::Field(1, 56)), _))
                | (ContinuationAbi::Tagged, 24, (Some(Value::Input(1)), _)) => {
                    destroyed = Some(abi)
                }
                _ => return None,
            }
            for r in [
                Register::RAX,
                Register::RCX,
                Register::RDX,
                Register::R8,
                Register::R9,
                Register::R10,
                Register::R11,
            ] {
                regs.remove(&r);
            }
            regs.insert(Register::RAX, Value::Result);
        } else {
            if i.mnemonic() == Mnemonic::Mov
                && i.op0_kind() == OpKind::Memory
                && regs.get(&i.memory_base()) == Some(&Value::Input(1))
                && i.op1_kind() == OpKind::Immediate32to64
                && i.immediate32to64() == 0
            {
                reset = match i.memory_displacement64() {
                    24 => Some(ContinuationAbi::Tagged),
                    56 => Some(ContinuationAbi::Msvc),
                    _ => None,
                };
            }
            move_step(&i, &mut regs);
        }
    }
    moved.filter(|abi| destroyed == Some(*abi) && reset == Some(*abi))
}

fn move_step(i: &iced_x86::Instruction, regs: &mut HashMap<Register, Value>) {
    if i.op0_kind() != OpKind::Register
        || matches!(
            i.mnemonic(),
            Mnemonic::Cmp | Mnemonic::Test | Mnemonic::Push
        )
    {
        return;
    }
    let dst = i.op0_register().full_register();
    let value = match i.mnemonic() {
        Mnemonic::Mov if i.op1_kind() == OpKind::Register => {
            regs.get(&i.op1_register().full_register()).copied()
        }
        Mnemonic::Mov if i.op1_kind() == OpKind::Memory && i.memory_index() == Register::None => {
            match regs.get(&i.memory_base()) {
                Some(Value::Input(arg)) => {
                    Some(Value::Field(*arg, i.memory_displacement64() as usize))
                }
                Some(Value::Field(1, 56)) if i.memory_displacement64() == 0 => {
                    Some(Value::Table(ContinuationAbi::Msvc))
                }
                _ => None,
            }
        }
        Mnemonic::And
            if matches!(
                i.op1_kind(),
                OpKind::Immediate8to64 | OpKind::Immediate32to64
            ) =>
        {
            match (regs.get(&dst), i.immediate(1) as i64) {
                (Some(value @ Value::Field(_, 24)), -4) => Some(*value),
                (Some(Value::Field(_, 24)), -8) => Some(Value::Table(ContinuationAbi::Tagged)),
                _ => None,
            }
        }
        _ => None,
    };
    regs.remove(&dst);
    if let Some(value) = value {
        regs.insert(dst, value);
    }
}

pub(super) fn resolve(
    memory: &ProcessMemory,
    studio: &ModuleEntry,
    layout: &PackageLayout,
    context: usize,
) -> Result<ContinuationAbi> {
    let submit = studio.base + layout.submit_task;
    let code = properties::dispatch_code(memory, studio, layout, submit)?;
    let (field, slot) = submit_fields(&code, submit as u64)
        .context("Native task submitter has no verified callback consumer")?;
    anyhow::ensure!(
        field <= 512 && slot < 512 && field % 8 == 0 && slot % 8 == 0,
        "Native task consumer binding is invalid"
    );
    let receiver = memory.read_u64(context + field)? as usize;
    anyhow::ensure!(
        properties::complete_reflection_type(memory, studio, receiver).as_deref()
            == Some(".?AVDataModelManager@RBX@@"),
        "Native task consumer identity changed"
    );
    let table = memory.read_u64(receiver)? as usize;
    let entry = memory.read_u64(table + slot)? as usize;
    let code = properties::dispatch_code(memory, studio, layout, entry)?;
    let transfer =
        mover(&code, entry as u64).context("Native task callback transfer is unverified")?;
    let code = properties::dispatch_code(memory, studio, layout, transfer)?;
    mover_abi(&code).context("Native task continuation layout could not be verified")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn task_authorization_requires_explicit_supported_abi_evidence() {
        for value in [
            serde_json::Value::Null,
            serde_json::json!(0),
            serde_json::json!(3),
            serde_json::json!(4294967297u64),
        ] {
            assert!(
                from_report(
                    NativeOperation::PlayTeleportJoin,
                    &serde_json::json!({"taskContinuationAbi": value})
                )
                .is_err()
            );
        }
        for value in [1, 2] {
            assert_eq!(
                from_report(
                    NativeOperation::PackageActions,
                    &serde_json::json!({"taskContinuationAbi": value})
                )
                .unwrap(),
                Some(value)
            );
        }
        assert_eq!(
            from_report(NativeOperation::Input, &serde_json::Value::Null).unwrap(),
            None
        );
    }
    #[test]
    fn task_consumer_distinguishes_callback_ownership_and_rejects_wrong_fields() {
        let fixtures: serde_json::Value = serde_json::from_str(include_str!(
            "../../../../native/tests/windows-task-dispatch.json"
        ))
        .unwrap();
        for row in fixtures.as_array().unwrap() {
            let code = hex(row["hex"].as_str().unwrap()).unwrap();
            let at = row["entry"].as_u64().unwrap();
            assert_eq!(
                mover(&code, at),
                Some(row["move"].as_u64().unwrap() as usize)
            );
            let code = hex(row["moveHex"].as_str().unwrap()).unwrap();
            let abi = if row["abi"] == 1 {
                ContinuationAbi::Msvc
            } else {
                ContinuationAbi::Tagged
            };
            assert_eq!(mover_abi(&code), Some(abi));
            let mut wrong = code.clone();
            // Changing the source field invalidates ownership provenance.
            let from = if abi == ContinuationAbi::Msvc { 56 } else { 24 };
            let at = wrong
                .windows(4)
                .position(|w| w == [0x48, 0x8b, 0x4a, from] || w == [0x4c, 0x8b, 0x42, from])
                .unwrap();
            wrong[at + 3] += 8;
            assert_eq!(mover_abi(&wrong), None);
        }
    }
}
