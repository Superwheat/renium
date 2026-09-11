// Read-only ABI discovery for the independent DataModel attribute guard.
// Addresses and the DataModel field are derived from Studio's SetEnabled path.
use super::*;
use iced_x86::{Decoder, DecoderOptions, Instruction, Mnemonic, OpKind, Register};

#[derive(Clone, Debug)]
pub(crate) struct ObservationTrace {
    pub signal_offset: usize,
    pub ensure: usize,
    pub allocate: usize,
    pub append: usize,
    pub disconnect: usize,
    pub assign: usize,
    pub functions: Vec<usize>,
}

fn code(image: &PeImage<'_>, rva: usize) -> Result<Vec<Instruction>> {
    image.require_executable_rva(rva)?;
    let offset = image.rva_to_offset(rva)?;
    let (begin, end) = image.function_bounds(offset)?;
    anyhow::ensure!(
        begin == offset && end - begin <= 8192,
        "Unbounded signal function"
    );
    let result = Decoder::with_ip(
        64,
        &image.bytes[begin..end],
        rva as u64,
        DecoderOptions::NONE,
    )
    .into_iter()
    .collect::<Vec<_>>();
    anyhow::ensure!(
        !result.iter().any(Instruction::is_invalid),
        "Invalid signal code"
    );
    Ok(result)
}
fn call(i: &Instruction) -> Option<usize> {
    (i.mnemonic() == Mnemonic::Call && i.op0_kind() == OpKind::NearBranch64)
        .then(|| i.near_branch_target() as usize)
}
fn mem(i: &Instruction, base: Register, offset: usize) -> bool {
    i.memory_base() == base
        && i.memory_index() == Register::None
        && i.memory_displacement64() == offset as u64
}
fn lea(i: &Instruction, to: Register) -> bool {
    i.mnemonic() == Mnemonic::Lea && i.op0_register() == to && i.op1_kind() == OpKind::Memory
}

/// Find the typed ObjectValue signal call in its verified reflection invoker.
/// The descriptor supplies a member offset, and the invoker passes a two-word
/// shared Instance by address. No Studio-version-specific address is used.
pub(super) fn relay_dispatch(image: &PeImage<'_>, invoker: usize) -> Result<usize> {
    let instructions = code(image, invoker)?;
    let calls = instructions
        .windows(4)
        .filter(|w| {
            w[0].mnemonic() == Mnemonic::Movsxd
                && w[0].op0_register() == Register::RCX
                && mem(&w[0], Register::RBP, 0x78)
                && w[1].mnemonic() == Mnemonic::Add
                && w[1].op0_register() == Register::RCX
                && w[1].op1_register() == Register::RSI
                && lea(&w[2], Register::RDX)
                && mem(&w[2], Register::RSP, 0x20)
                && call(&w[3]).is_some()
        })
        .collect::<Vec<_>>();
    anyhow::ensure!(
        calls.len() == 1,
        "Native attribute relay dispatch is unrecognized"
    );
    for (target, source) in [
        (Register::RSI, Register::RDX),
        (Register::RBP, Register::RCX),
    ] {
        anyhow::ensure!(
            instructions
                .iter()
                .take(12)
                .any(|i| i.mnemonic() == Mnemonic::Mov
                    && i.op0_register() == target
                    && i.op1_register() == source),
            "Native attribute relay argument mapping changed"
        );
    }
    let target = call(&calls[0][3]).unwrap();
    code(image, target)?;
    Ok(target)
}

pub(crate) fn discover(image: &PeImage<'_>, set_enabled: usize) -> Result<ObservationTrace> {
    let enabled = code(image, set_enabled)?;
    let branch = enabled
        .windows(5)
        .filter(|w| {
            w[0].mnemonic() == Mnemonic::Test
                && w[0].op0_register() == Register::DL
                && w[0].op1_register() == Register::DL
                && w[1].mnemonic() == Mnemonic::Je
                && call(&w[2]).is_some()
                && w[3].mnemonic() == Mnemonic::Jmp
                && call(&w[4]).is_some()
                && w[1].near_branch_target() == w[4].ip()
                && w[3].near_branch_target() == w[4].next_ip()
        })
        .collect::<Vec<_>>();
    anyhow::ensure!(
        branch.len() == 1,
        "SetEnabled signal registration branch changed"
    );
    let attach = call(&branch[0][2]).unwrap();
    let detach = call(&branch[0][4]).unwrap();
    let instructions = code(image, attach)?;
    let start = instructions
        .windows(4)
        .enumerate()
        .filter(|(_, w)| {
            lea(&w[0], Register::RCX)
                && w[0].memory_index() == Register::None
                && (0x100..0x2000).contains(&(w[0].memory_displacement64() as usize))
                && call(&w[1]).is_some()
                && w[2].mnemonic() == Mnemonic::Mov
                && w[2].op0_register() == Register::ECX
                && w[2].op1_kind() == OpKind::Immediate32
                && w[2].immediate32() == 0x48
                && call(&w[3]).is_some()
        })
        .map(|(i, _)| i)
        .collect::<Vec<_>>();
    anyhow::ensure!(
        start.len() == 1,
        "DataModel signal registration is missing or ambiguous"
    );
    let start = start[0];
    let model = instructions[start].memory_base();
    let signal_offset = instructions[start].memory_displacement64() as usize;
    let ensure = call(&instructions[start + 1]).unwrap();
    let allocate = call(&instructions[start + 3]).unwrap();
    let registration = &instructions[start + 4..];
    let append_at = registration
        .iter()
        .position(|i| call(i).is_some())
        .context("No signal append")?;
    let prefix = &registration[..append_at];
    let node = prefix
        .first()
        .filter(|i| i.mnemonic() == Mnemonic::Mov && i.op1_register() == Register::RAX)
        .context("Signal allocator return changed")?
        .op0_register();
    let invoker = prefix
        .iter()
        .find(|i| lea(i, Register::RAX) && i.is_ip_rel_memory_operand())
        .context("No native signal invoker")?
        .ip_rel_memory_address() as usize;
    // The helper's slot is an engine-owned shared/weak node with a plain callback
    // and no captured C++ object. Reject layouts that would require its destructor.
    for (offset, source) in [
        (8, Register::RAX),
        (0x10, Register::R15),
        (0x18, Register::R15),
        (0x20, Register::RAX),
    ] {
        anyhow::ensure!(
            prefix.iter().any(|i| i.mnemonic() == Mnemonic::Mov
                && i.op0_kind() == OpKind::Memory
                && mem(i, node, offset)
                && i.op1_register() == source),
            "Signal node field changed at {offset:x}"
        );
    }
    anyhow::ensure!(
        prefix.iter().any(|i| i.mnemonic() == Mnemonic::Mov
            && mem(i, Register::RAX, 4)
            && i.op1_kind() == OpKind::Immediate32
            && i.immediate32() == 1),
        "Signal node weak owner changed"
    );
    anyhow::ensure!(
        prefix.iter().any(|i| i.mnemonic() == Mnemonic::And
            && mem(i, node, 0x18)
            && i.immediate(1) == 0xffff_ffff_ffff_fffb),
        "Signal payload destructor contract changed"
    );
    anyhow::ensure!(
        prefix.last().is_some_and(|i| i.mnemonic() == Mnemonic::Mov
            && i.op0_register() == Register::RCX
            && mem(i, model, signal_offset))
            && prefix
                .get(prefix.len() - 2)
                .is_some_and(|i| i.mnemonic() == Mnemonic::Mov
                    && i.op0_register() == Register::RDX
                    && i.op1_register() == node),
        "Signal append arguments changed"
    );
    let append = call(&registration[append_at]).unwrap();
    let tail = &registration[append_at + 1..];
    let cleanup = tail
        .windows(7)
        .filter(|w| {
            lea(&w[0], Register::RCX)
                && call(&w[1]).is_some()
                && lea(&w[2], Register::RDX)
                && lea(&w[3], Register::RCX)
                && call(&w[4]).is_some()
                && w[0].memory_base() == w[3].memory_base()
                && w[0].memory_displacement64() == w[3].memory_displacement64()
        })
        .collect::<Vec<_>>();
    anyhow::ensure!(cleanup.len() == 1, "Signal connection cleanup changed");
    let disconnect = call(&cleanup[0][1]).unwrap();
    let assign = call(&cleanup[0][4]).unwrap();
    let connection_offset = cleanup[0][0].memory_displacement64();
    let detached = code(image, detach)?;
    anyhow::ensure!(
        detached.windows(2).any(|w| lea(&w[0], Register::RCX)
            && w[0].memory_displacement64() == connection_offset
            && call(&w[1]) == Some(disconnect)),
        "History disable does not disconnect this signal"
    );
    // Validate the raw slot-call ABI: shared Instance in RDX, descriptor in R8,
    // one member dispatch plus the shared owner's release path.
    let invoke = code(image, invoker)?;
    anyhow::ensure!(
        invoke.iter().any(|i| i.mnemonic() == Mnemonic::Mov
            && i.op0_register() == Register::R10
            && mem(i, Register::RCX, 0x28))
            && invoke
                .iter()
                .any(|i| i.mnemonic() == Mnemonic::Call && i.op0_register() == Register::R10)
            && invoke
                .iter()
                .any(|i| i.mnemonic() == Mnemonic::Xadd && i.memory_displacement64() == 8),
        "Signal callback ownership ABI changed"
    );
    let functions = vec![
        set_enabled,
        attach,
        detach,
        ensure,
        allocate,
        append,
        disconnect,
        assign,
        invoker,
    ];
    for &rva in &functions {
        code(image, rva)?;
    }
    Ok(ObservationTrace {
        signal_offset,
        ensure,
        allocate,
        append,
        disconnect,
        assign,
        functions,
    })
}
