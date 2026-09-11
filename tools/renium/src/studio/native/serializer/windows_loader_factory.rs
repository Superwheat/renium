use super::*;
use anyhow::ensure;
use iced_x86::{Decoder, DecoderOptions, FlowControl, Instruction, Mnemonic, OpKind, Register};

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, PartialOrd, Ord)]
enum Origin {
    Context,
    Interned(usize),
    Factory(usize),
    Vtable(usize),
    Stack,
    One,
}

fn follow_factory_instruction(
    instruction: &Instruction,
    index: usize,
    lookup_index: usize,
    registers: &mut HashMap<Register, Origin>,
    stack: &mut HashMap<(Register, u64), Origin>,
    found: &mut Option<(usize, usize)>,
) -> Option<()> {
    let register = |r: Register| registers.get(&r.full_register()).copied();
    let dest = instruction.op0_register().full_register();
    let memory = (
        instruction.memory_base().full_register(),
        instruction.memory_displacement64(),
    );
    let stack_memory = instruction.memory_index() == Register::None
        && matches!(memory.0, Register::RBP | Register::RSP);
    match instruction.mnemonic() {
        Mnemonic::Mov | Mnemonic::Movzx if instruction.op0_kind() == OpKind::Register => {
            let value = match instruction.op1_kind() {
                OpKind::Register => register(instruction.op1_register()),
                OpKind::Memory if stack_memory && instruction.memory_size().size() == 8 => {
                    stack.get(&memory).copied()
                }
                OpKind::Memory
                    if instruction.memory_index() == Register::None
                        && instruction.memory_displacement64() == 0 =>
                {
                    match register(instruction.memory_base()) {
                        Some(Origin::Factory(intern)) => Some(Origin::Vtable(intern)),
                        _ => None,
                    }
                }
                OpKind::Immediate32 if instruction.immediate32() == 1 => Some(Origin::One),
                _ => None,
            };
            registers.remove(&dest);
            if let Some(value) = value {
                registers.insert(dest, value);
            }
        }
        Mnemonic::Mov if instruction.op0_kind() == OpKind::Memory && stack_memory => {
            let value = (instruction.op1_kind() == OpKind::Register)
                .then(|| register(instruction.op1_register()))
                .flatten();
            stack.remove(&memory);
            if instruction.memory_size().size() == 8
                && let Some(value) = value
            {
                stack.insert(memory, value);
            }
        }
        Mnemonic::Lea if instruction.op0_kind() == OpKind::Register => {
            registers.remove(&dest);
            if stack_memory {
                registers.insert(dest, Origin::Stack);
            }
        }
        Mnemonic::Call => {
            let mut result = None;
            if index == lookup_index {
                if let Some(Origin::Interned(intern)) = register(Register::RCX) {
                    result = Some(Origin::Factory(intern));
                }
            } else if instruction.op0_kind() == OpKind::NearBranch64
                && register(Register::RCX) == Some(Origin::Stack)
            {
                result = Some(Origin::Interned(instruction.near_branch_target() as usize));
            }
            let own_factory = instruction.op0_kind() == OpKind::Memory
                && instruction.memory_index() == Register::None
                && instruction.memory_displacement64() == 0
                && register(Register::RDX) == Some(Origin::Stack)
                && register(Register::R8) == Some(Origin::Context)
                && register(Register::R9) == Some(Origin::One)
                && matches!(register(Register::RCX), Some(Origin::Factory(intern))
                    if register(instruction.memory_base()) == Some(Origin::Vtable(intern)));
            if own_factory && let Some(Origin::Factory(intern)) = register(Register::RCX) {
                let candidate = (intern, instruction.next_ip() as usize);
                if found.is_some_and(|previous| previous != candidate) {
                    return None;
                }
                *found = Some(candidate);
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
                registers.remove(&r);
            }
            if let Some(value) = result {
                registers.insert(Register::RAX, value);
            }
        }
        _ if instruction.op0_kind() == OpKind::Register
            && !matches!(
                instruction.mnemonic(),
                Mnemonic::Cmp | Mnemonic::Test | Mnemonic::Push
            ) =>
        {
            registers.remove(&dest);
        }
        _ => {}
    }
    Some(())
}

fn factory_call(code: &[Instruction], lookup_index: usize) -> Option<(usize, usize, usize)> {
    let positions = code
        .iter()
        .enumerate()
        .map(|(index, instruction)| (instruction.ip(), index))
        .collect::<HashMap<_, _>>();
    let mut pending = vec![(
        0,
        HashMap::from([(Register::RDX, Origin::Context)]),
        HashMap::new(),
    )];
    let mut visited = HashSet::new();
    let mut found = None;
    let mut context_bytes = 8;
    while let Some((mut index, mut registers, mut stack)) = pending.pop() {
        for _ in 0..2048 {
            let instruction = code.get(index)?;
            // Include provenance in the key, so error/cleanup branches cannot
            // poison the success branch's register or stack state.
            let mut signature = registers
                .iter()
                .map(|(r, v)| (*r as u32, *v))
                .collect::<Vec<_>>();
            signature.sort_unstable();
            let mut slots = stack
                .iter()
                .map(|((r, d), v): (&(Register, u64), &Origin)| (*r as u32, *d, *v))
                .collect::<Vec<_>>();
            slots.sort_unstable();
            if !visited.insert((index, signature, slots)) {
                break;
            }
            if visited.len() > 4096 {
                return None;
            }
            let register = |r: Register| registers.get(&r.full_register()).copied();
            let dest = instruction.op0_register().full_register();
            if instruction.op_kinds().any(|kind| kind == OpKind::Memory)
                && register(instruction.memory_base()) == Some(Origin::Context)
            {
                // The normal loader uses a null context. Our identity context
                // is zero-filled: accept only bounded nullable pointer reads,
                // and follow their null branch. Never infer an opaque ABI from
                // the factory call alone; this reader also reads class counters.
                let check = code.get(index + 1)?;
                let branch = code.get(index + 2)?;
                let offset = instruction.memory_displacement64() as usize;
                if instruction.mnemonic() != Mnemonic::Mov
                    || instruction.op0_kind() != OpKind::Register
                    || instruction.op0_register().size() != 8
                    || instruction.memory_index() != Register::None
                    || instruction.memory_size().size() != 8
                    || offset > 504
                    || !offset.is_multiple_of(8)
                    || check.mnemonic() != Mnemonic::Test
                    || check.op0_register() != instruction.op0_register()
                    || check.op1_register() != instruction.op0_register()
                    || branch.mnemonic() != Mnemonic::Je
                {
                    return None;
                }
                context_bytes = context_bytes.max(offset + 8);
                registers.remove(&dest);
                index = *positions.get(&branch.near_branch_target())?;
                continue;
            }
            follow_factory_instruction(
                instruction,
                index,
                lookup_index,
                &mut registers,
                &mut stack,
                &mut found,
            )?;
            match instruction.flow_control() {
                FlowControl::ConditionalBranch | FlowControl::UnconditionalBranch => {
                    if let Some(&target) = positions.get(&instruction.near_branch_target()) {
                        pending.push((target, registers.clone(), stack.clone()));
                    }
                    if instruction.flow_control() == FlowControl::UnconditionalBranch {
                        break;
                    }
                }
                FlowControl::Return | FlowControl::IndirectBranch | FlowControl::Exception => break,
                _ => {}
            }
            index += 1;
            if index == code.len() {
                break;
            }
        }
    }
    found.map(|(intern, origin)| (intern, origin, context_bytes))
}

pub(super) fn discover(image: &PeImage<'_>, bytes: &[u8], reader: usize) -> Result<FactoryTrace> {
    let start = image.rva_to_offset(reader)?;
    let (_, end) = image.function_bounds(start)?;
    let callees = Decoder::with_ip(64, &bytes[start..end], reader as u64, DecoderOptions::NONE)
        .into_iter()
        .filter(|i| i.mnemonic() == Mnemonic::Call && i.op0_kind() == OpKind::NearBranch64)
        .map(|i| i.near_branch_target() as usize)
        .collect::<HashSet<_>>();
    let mut candidates = HashSet::new();
    for rva in callees {
        let Ok(start) = image.rva_to_offset(rva) else {
            continue;
        };
        let Ok((begin, end)) = image.function_bounds(start) else {
            continue;
        };
        if begin != start || end - begin > 8192 {
            continue;
        }
        let code = Decoder::with_ip(64, &bytes[begin..end], rva as u64, DecoderOptions::NONE)
            .into_iter()
            .collect::<Vec<_>>();
        let anchor = b"Unrecognized class %s\0";
        let is_instance_reader = code.iter().any(|i| {
            i.is_ip_rel_memory_operand()
                && image
                    .rva_to_offset(i.ip_rel_memory_address() as usize)
                    .ok()
                    .and_then(|at| bytes.get(at..at + anchor.len()))
                    == Some(anchor.as_slice())
        });
        if !is_instance_reader {
            continue;
        }
        for (index, call) in code.iter().enumerate() {
            if call.mnemonic() != Mnemonic::Call || call.op0_kind() != OpKind::NearBranch64 {
                continue;
            }
            if let Some((intern, return_rva, context_bytes)) = factory_call(&code, index) {
                let lookup = call.near_branch_target() as usize;
                image.require_executable_rva(lookup)?;
                let raw = image.rva_to_offset(lookup)?;
                ensure!(
                    image.function_bounds(raw)?.0 == raw,
                    "Factory lookup is not a function entry"
                );
                image.require_executable_rva(intern)?;
                let raw = image.rva_to_offset(intern)?;
                ensure!(
                    image.function_bounds(raw)?.0 == raw,
                    "Name interning is not a function entry"
                );
                candidates.insert((lookup, return_rva, intern, rva, context_bytes));
            }
        }
    }
    ensure!(
        candidates.len() == 1,
        "Retained reader factory ABI matched {} candidates",
        candidates.len()
    );
    let (lookup, origin, intern_name, instance_reader, context_bytes) =
        candidates.into_iter().next().unwrap();
    Ok(FactoryTrace {
        lookup,
        origin,
        intern_name,
        instance_reader,
        context_bytes,
    })
}

#[test]
fn creator_call_requires_factory_receiver_output_slot_and_flag() {
    // Result -> R10, load creator vtable, explicit result storage and bool.
    let code = [
        0x4c, 0x8b, 0xfa, 0x48, 0x8d, 0x4c, 0x24, 0x30, 0xe8, 0, 0, 0, 0, 0x48, 0x8b, 0xc8, 0xe8,
        0, 0, 0, 0, 0x4c, 0x8b, 0xd0, 0x49, 0x8b, 0x02, 0x41, 0xb9, 1, 0, 0, 0, 0x4d, 0x8b, 0xc7,
        0x48, 0x8d, 0x54, 0x24, 0x20, 0x49, 0x8b, 0xca, 0xff, 0x10, 0xc3,
    ];
    let decode = |bytes: &[u8]| {
        Decoder::with_ip(64, bytes, 0x1000, DecoderOptions::NONE)
            .into_iter()
            .collect::<Vec<_>>()
    };
    assert_eq!(factory_call(&decode(&code), 4), Some((0x100d, 0x102e, 8)));
    for (index, value) in [(29, 0), (43, 0xc9), (35, 0xc6)] {
        let mut changed = code;
        changed[index] = value;
        assert_eq!(factory_call(&decode(&changed), 4), None);
    }
    // The creation context is read *after* the factory returns. A relocated,
    // null-checked counter slot is supported; an unchecked/reversed load is not.
    let mut counters = code[..code.len() - 1].to_vec();
    counters.extend_from_slice(&[
        0x49, 0x8b, 0x8f, 0x90, 0, 0, 0, 0x48, 0x85, 0xc9, 0x74, 0, 0xc3,
    ]);
    assert_eq!(
        factory_call(&decode(&counters), 4),
        Some((0x100d, 0x102e, 152))
    );
    let at = code.len() - 1;
    counters[at + 3] = 0xa0;
    assert_eq!(
        factory_call(&decode(&counters), 4),
        Some((0x100d, 0x102e, 168))
    );
    for (offset, value) in [(2, 0x87), (3, 0xa1), (4, 0x10), (9, 0xc8), (10, 0x75)] {
        let mut changed = counters.clone();
        changed[at + offset] = value;
        assert_eq!(factory_call(&decode(&changed), 4), None);
    }
}

#[test]
#[ignore = "Read-only native factory finder; requires an explicit Studio executable"]
fn discover_installed_retained_factory() -> Result<()> {
    let path = std::env::var("RENIUM_LOADER_PROBE_EXE")?;
    let bytes = fs::read(path)?;
    let image = PeImage::parse(&bytes)?;
    let reader = super::trace_loader(&bytes)?.reader;
    let started = Instant::now();
    let trace = discover(&image, &bytes, reader)?;
    println!(
        "Retained factory discovery {:.3}ms lookupRaw={:#x} returnRaw={:#x} internRaw={:#x} contextBytes={}",
        started.elapsed().as_secs_f64() * 1000.,
        image.rva_to_offset(trace.lookup)?,
        image.rva_to_offset(trace.origin)?,
        image.rva_to_offset(trace.intern_name)?,
        trace.context_bytes,
    );
    Ok(())
}
