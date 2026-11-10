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
    pub append_abi: AppendAbi,
    pub disconnect: usize,
    pub assign: usize,
    pub functions: Vec<usize>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u32)]
pub(crate) enum AppendAbi {
    Node = 0,
    Connection = 1,
}

fn append_abi(
    prefix: &[Instruction],
    model: Register,
    field: usize,
    node: Register,
) -> Result<AppendAbi> {
    let args = prefix
        .get(prefix.len().saturating_sub(2)..)
        .filter(|w| w.len() == 2)
        .context("Signal append has no arguments")?;
    anyhow::ensure!(
        args[1].mnemonic() == Mnemonic::Mov
            && args[1].op0_register() == Register::RCX
            && args[1].op1_kind() == OpKind::Memory
            && mem(&args[1], model, field),
        "Signal append receiver changed"
    );
    if args[0].mnemonic() == Mnemonic::Mov
        && args[0].op0_register() == Register::RDX
        && args[0].op1_register() == node
    {
        return Ok(AppendAbi::Node);
    }
    anyhow::ensure!(
        node == Register::R8
            && lea(&args[0], Register::RDX)
            && matches!(args[0].memory_base(), Register::RSP | Register::RBP)
            && args[0].memory_index() == Register::None,
        "Signal append arguments changed"
    );
    Ok(AppendAbi::Connection)
}

// The connection-returning overload owns the extra weak reference which the
// two-argument append leaves to its caller. Prove both the output and ownership
// before selecting the helper's call signature; never infer it from a version.
fn connection_append_contract(instructions: &[Instruction]) -> bool {
    let saved = |source| {
        instructions
            .iter()
            .take(12)
            .find(|i| {
                i.mnemonic() == Mnemonic::Mov
                    && i.op1_register() == source
                    && matches!(
                        i.op0_register(),
                        Register::RBX
                            | Register::RBP
                            | Register::RSI
                            | Register::RDI
                            | Register::R12
                            | Register::R13
                            | Register::R14
                            | Register::R15
                    )
            })
            .map(|i| i.op0_register())
    };
    let (Some(signal), Some(output), Some(node)) = (
        saved(Register::RCX),
        saved(Register::RDX),
        saved(Register::R8),
    ) else {
        return false;
    };
    if signal == output || signal == node || output == node {
        return false;
    }
    let writes = instructions
        .iter()
        .enumerate()
        .filter(|(_, i)| {
            i.mnemonic() == Mnemonic::Mov && i.op0_kind() == OpKind::Memory && mem(i, output, 0)
        })
        .collect::<Vec<_>>();
    !writes.is_empty()
        && writes.iter().all(|(at, i)| {
            i.memory_size().size() == 8
                && i.op1_register() == node
                && instructions
                    .iter()
                    .skip(at + 1)
                    .take(4)
                    .filter(|i| {
                        i.mnemonic() == Mnemonic::Inc
                            && i.has_lock_prefix()
                            && i.memory_size().size() == 4
                            && mem(i, node, 4)
                    })
                    .count()
                    == 1
        })
        && instructions.iter().any(|i| {
            i.mnemonic() == Mnemonic::Mov
                && i.op0_kind() == OpKind::Memory
                && mem(i, signal, 8)
                && i.op1_register() == node
        })
        && instructions.iter().any(|i| {
            i.mnemonic() == Mnemonic::Inc
                && i.has_lock_prefix()
                && i.memory_size().size() == 4
                && mem(i, node, 0)
        })
        && instructions.iter().any(|i| {
            i.mnemonic() == Mnemonic::Mov
                && i.op0_register() == Register::RAX
                && i.op1_register() == output
        })
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

pub(super) const RELAY_DESCRIPTOR: &str = ".?AV?$EventDesc@VObjectValue@RBX@@$$A6AXV?$shared_ptr@VInstance@RBX@@@std@@@ZV?$signal@$$A6AXV?$shared_ptr@VInstance@RBX@@@std@@@Z@rbx@@PEQ12@V56@@Reflection@RBX@@";

#[derive(Clone, Copy, Debug)]
pub(super) struct RelayTrace {
    pub vtable: usize,
    pub slot: usize,
    pub invoker: usize,
    pub member_field: usize,
    pub fire: usize,
}

// Find the typed ObjectValue signal call, including the shared Instance pair
// passed by address. Both the vtable slot and descriptor member field move.
fn relay_call(instructions: &[Instruction]) -> Result<(usize, usize)> {
    let calls = instructions
        .windows(4)
        .filter(|w| {
            w[0].mnemonic() == Mnemonic::Movsxd
                && w[0].op0_register() == Register::RCX
                && w[0].op1_kind() == OpKind::Memory
                && w[0].memory_base() == Register::RBP
                && w[0].memory_index() == Register::None
                && (0x20..0x400).contains(&w[0].memory_displacement64())
                && w[0].memory_displacement64().is_multiple_of(4)
                && w[1].mnemonic() == Mnemonic::Add
                && w[1].op0_register() == Register::RCX
                && w[1].op1_register() == Register::RSI
                && lea(&w[2], Register::RDX)
                && w[2].memory_base() == Register::RSP
                && w[2].memory_index() == Register::None
                && (0x20..0x400).contains(&w[2].memory_displacement64())
                && w[2].memory_displacement64().is_multiple_of(8)
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
    let dispatch = calls[0];
    let stack = dispatch[2].memory_displacement64() as usize;
    anyhow::ensure!(
        instructions.windows(4).any(|w| {
            w[3].ip() < dispatch[0].ip()
                && w[0].mnemonic() == Mnemonic::Mov
                && w[0].op0_register() == Register::RAX
                && w[0].op1_kind() == OpKind::Memory
                && mem(&w[0], Register::RCX, 0)
                && w[1].mnemonic() == Mnemonic::Mov
                && w[1].op0_kind() == OpKind::Memory
                && mem(&w[1], Register::RSP, stack)
                && w[1].op1_register() == Register::RAX
                && w[2].mnemonic() == Mnemonic::Mov
                && w[2].op0_register() == Register::RAX
                && w[2].op1_kind() == OpKind::Memory
                && mem(&w[2], Register::RCX, 8)
                && w[3].mnemonic() == Mnemonic::Mov
                && w[3].op0_kind() == OpKind::Memory
                && mem(&w[3], Register::RSP, stack + 8)
                && w[3].op1_register() == Register::RAX
        }),
        "Native attribute relay shared Instance argument changed"
    );
    Ok((
        dispatch[0].memory_displacement64() as usize,
        call(&dispatch[3]).unwrap(),
    ))
}

pub(super) fn relay_binding(image: &PeImage<'_>) -> Result<RelayTrace> {
    let vtable = undo_selection::descriptor_vtable(image, RELAY_DESCRIPTOR.as_bytes())?;
    let table = image.rva_to_offset(vtable)?;
    let mut candidates = Vec::new();
    for slot in 0..256 {
        let Some(invoker) =
            (read_u64(image.bytes, table + slot * 8)? as usize).checked_sub(image.image_base)
        else {
            break;
        };
        if image.require_executable_rva(invoker).is_err() {
            break;
        }
        let Ok(instructions) = code(image, invoker) else {
            continue;
        };
        let Ok((member_field, fire)) = relay_call(&instructions) else {
            continue;
        };
        code(image, fire)?;
        candidates.push(RelayTrace {
            vtable,
            slot,
            invoker,
            member_field,
            fire,
        });
    }
    anyhow::ensure!(
        candidates.len() == 1,
        "Native attribute relay invoker is missing or ambiguous"
    );
    Ok(candidates[0])
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
    let append_abi = append_abi(prefix, model, signal_offset, node)?;
    let append = call(&registration[append_at]).unwrap();
    if append_abi == AppendAbi::Connection {
        anyhow::ensure!(
            connection_append_contract(&code(image, append)?),
            "Signal append connection ownership changed"
        );
    }
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
    if append_abi == AppendAbi::Connection {
        let output = &prefix[prefix.len() - 2];
        anyhow::ensure!(
            mem(
                &cleanup[0][2],
                output.memory_base(),
                output.memory_displacement64() as usize
            ),
            "Signal append result is not the assigned connection"
        );
    }
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
        append_abi,
        disconnect,
        assign,
        functions,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn instructions(hexadecimal: &str) -> Vec<Instruction> {
        Decoder::with_ip(64, &hex(hexadecimal).unwrap(), 0x1000, DecoderOptions::NONE)
            .into_iter()
            .collect()
    }

    #[test]
    fn signal_append_distinguishes_node_and_connection_arguments() -> Result<()> {
        assert_eq!(
            append_abi(
                &instructions("488BD7498B8E00070000"),
                Register::R14,
                0x700,
                Register::RDI
            )?,
            AppendAbi::Node
        );
        for (code, field) in [
            ("488D542440498B8E30070000", 0x730),
            ("488D55D0498B8E98090000", 0x998),
        ] {
            assert_eq!(
                append_abi(&instructions(code), Register::R14, field, Register::R8)?,
                AppendAbi::Connection
            );
            assert!(append_abi(&instructions(code), Register::R15, field, Register::R8).is_err());
            assert!(
                append_abi(&instructions(code), Register::R14, field + 8, Register::R8).is_err()
            );
            assert!(append_abi(&instructions(code), Register::R14, field, Register::R9).is_err());
        }
        for code in [
            "",
            "488B542440498B8E30070000",
            "488D542440498B9630070000",
            "488D543840498B8E30070000",
        ] {
            assert!(append_abi(&instructions(code), Register::R14, 0x730, Register::R8).is_err());
        }
        Ok(())
    }

    #[test]
    fn relay_discovers_member_field_and_shared_instance_argument() -> Result<()> {
        let original = "488BF2488BE9488B014889442420488B4108488944242848638D900000004803CE488D542420E800010000";
        let (field, target) = relay_call(&instructions(original))?;
        assert_eq!(field, 0x90);
        assert_eq!(target, 0x112b);
        let moved = original
            .replace("8D90000000", "8DB0000000")
            .replace("442420", "442440")
            .replace("442428", "442448")
            .replace("542420", "542440");
        assert_eq!(relay_call(&instructions(&moved))?, (0xb0, target));
        for (from, to) in [
            ("488BF2", "488BF1"),         // instance no longer comes from RDX
            ("488BE9", "488BEA"),         // descriptor no longer comes from RCX
            ("8D90000000", "8D91000000"), // unaligned descriptor member
            ("488B4108", "488B4110"),     // not the shared owner's second word
            ("4889442428", "4889442430"), // incomplete shared pair
            ("488D542420", "488D542428"), // dispatch points at the owner
            ("4803CE", "4803CF"),         // wrong signal receiver
        ] {
            assert!(
                relay_call(&instructions(&original.replace(from, to))).is_err(),
                "accepted {from}"
            );
        }
        assert!(relay_call(&instructions(&format!("{original}{original}"))).is_err());
        Ok(())
    }

    #[test]
    fn connection_append_requires_output_and_both_owner_references() {
        let original = "498BF84C8BF2488BF1F0FF0748897E0849893EF0FF4704498BC6C3";
        assert!(connection_append_contract(&instructions(original)));
        // Relocate the three nonvolatile captures together with every use.
        assert!(connection_append_contract(&instructions(
            "498BD84C8BE2488BE9F0FF0348895D0849891C24F0FF4304498BC4C3"
        )));
        // Both branch outputs must retain exactly once, including the path
        // that delegates linking to the two-argument overload.
        let two_paths = original.replace(
            "498BC6",
            "EB11488BD7E80000000049893E4885FF7404F0FF4704498BC6",
        );
        assert!(connection_append_contract(&instructions(&two_paths)));
        assert!(!connection_append_contract(&instructions(
            &two_paths.replacen("4885FF7404F0FF4704", "4885FF740490909090", 1)
        )));
        for (from, to) in [
            ("4C8BF2", "4C8BF1"),             // output no longer comes from RDX
            ("498BF8", "498BF9"),             // node no longer comes from R8
            ("49893E", "498936"),             // returns the signal instead of the node
            ("F0FF4704", "F0FF4708"),         // wrong weak owner field
            ("F0FF4704", "90FF4704"),         // non-atomic retain
            ("F0FF07", "F0FF4704"),           // no signal-owned strong reference
            ("48897E08", "48897E10"),         // not the signal's node list
            ("498BC6", "488BC7"),             // wrong result pointer
            ("F0FF4704", "F0FF4704F0FF4704"), // double-retained connection
        ] {
            assert!(
                !connection_append_contract(&instructions(&original.replace(from, to))),
                "accepted {from}"
            );
        }
    }

    /// Use a reflected SetEnabled RVA with an executable; this never opens Studio.
    #[test]
    #[ignore]
    fn installed_studio_exposes_the_attribute_signal_contract() -> Result<()> {
        let path = std::env::var("RENIUM_STUDIO_EXE")?;
        let rva = usize::from_str_radix(
            std::env::var("RENIUM_SET_ENABLED_RVA")?.trim_start_matches("0x"),
            16,
        )?;
        let bytes = fs::read(&path)?;
        let image = PeImage::parse(&bytes)?;
        let trace = discover(&image, rva)?;
        let relay = relay_binding(&image)?;
        println!("{path}: {trace:?}; relay {relay:?}");
        Ok(())
    }

    #[test]
    fn native_signal_append_preserves_connection_ownership() -> Result<()> {
        let status = std::process::Command::new(
            Path::new(env!("OUT_DIR")).join("renium-signal-append-test.exe"),
        )
        .status()?;
        assert!(
            status.success(),
            "signal append ownership test failed: {status}"
        );
        Ok(())
    }
}
