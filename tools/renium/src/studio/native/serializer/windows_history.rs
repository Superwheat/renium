//! Find voxel playback through the reflected history method and decoded calls.
//! Unknown layouts fail before any code or transaction is changed.
use super::*;
use std::collections::VecDeque;

#[derive(Clone, Copy, PartialEq)]
enum Value {
    Entry,
    Direction,
    History,
    Terrain,
    Grid,
    Table,
}

fn voxel_playback(code: &[u8], address: usize) -> bool {
    let instructions = Decoder::with_ip(64, code, address as u64, DecoderOptions::NONE)
        .into_iter()
        .collect::<Vec<_>>();
    if instructions.iter().any(Instruction::is_invalid) {
        return false;
    }
    let mut registers = HashMap::from([
        (Register::RCX, Value::Entry),
        (Register::RDX, Value::Direction),
    ]);
    let mut grid_call = false;
    let mut voxel_call = false;
    for i in &instructions {
        if matches!(i.mnemonic(), Mnemonic::Mov | Mnemonic::Movzx)
            && i.op0_kind() == OpKind::Register
        {
            let value = if i.op1_kind() == OpKind::Register {
                registers.get(&i.op1_register().full_register()).copied()
            } else if i.op1_kind() == OpKind::Memory
                && i.memory_index() == Register::None
                && i.memory_size().size() == 8
            {
                let offset = i.memory_displacement64() as usize;
                match registers.get(&i.memory_base().full_register()) {
                    Some(Value::Entry) if offset == 0 => Some(Value::History),
                    Some(Value::History)
                        if (0x80..0x800).contains(&offset) && offset.is_multiple_of(8) =>
                    {
                        Some(Value::Terrain)
                    }
                    Some(Value::Terrain)
                        if (0x100..0x800).contains(&offset) && offset.is_multiple_of(8) =>
                    {
                        Some(Value::Grid)
                    }
                    Some(Value::Grid) if offset == 0 => Some(Value::Table),
                    _ => None,
                }
            } else {
                None
            };
            registers.remove(&i.op0_register().full_register());
            if let Some(value) = value {
                registers.insert(i.op0_register().full_register(), value);
            }
        } else if i.mnemonic() == Mnemonic::Call {
            if !grid_call {
                if i.op0_kind() != OpKind::Memory
                    || i.memory_index() != Register::None
                    || registers.get(&i.memory_base().full_register()) != Some(&Value::Table)
                    || registers.get(&Register::RCX) != Some(&Value::Grid)
                    || !(0x80..0x200).contains(&i.memory_displacement64())
                {
                    return false;
                }
                grid_call = true;
            } else if i.op0_kind() == OpKind::NearBranch64
                && registers.get(&Register::RCX) == Some(&Value::Entry)
                && registers.get(&Register::R8) == Some(&Value::Direction)
            {
                voxel_call = true;
            }
            for register in [
                Register::RAX,
                Register::RCX,
                Register::RDX,
                Register::R8,
                Register::R9,
                Register::R10,
                Register::R11,
            ] {
                registers.remove(&register);
            }
        } else {
            let mut info = InstructionInfoFactory::new();
            for used in info.info(i).used_registers() {
                if matches!(
                    used.access(),
                    OpAccess::Write
                        | OpAccess::CondWrite
                        | OpAccess::ReadWrite
                        | OpAccess::ReadCondWrite
                ) {
                    registers.remove(&used.register().full_register());
                }
            }
        }
    }
    grid_call
        && voxel_call
        && instructions.iter().any(|i| {
            i.flow_control() == FlowControl::ConditionalBranch && i.near_branch_target() < i.ip()
        })
}

fn function<'a>(image: &PeImage<'a>, address: usize) -> Option<&'a [u8]> {
    image.require_executable_rva(address).ok()?;
    let offset = image.rva_to_offset(address).ok()?;
    let (start, end) = image.function_bounds(offset).ok()?;
    (start == offset && (32..=16384).contains(&(end - start))).then_some(&image.bytes[start..end])
}

fn discover(image: &PeImage<'_>, finish: usize) -> Result<(usize, usize)> {
    let mut pending = VecDeque::from([(finish, 0)]);
    let mut seen = HashSet::new();
    let mut found = Vec::new();
    while let Some((address, depth)) = pending.pop_front() {
        if !seen.insert(address) {
            continue;
        }
        anyhow::ensure!(
            seen.len() <= 4096,
            "History call graph exceeds discovery limit"
        );
        let Some(code) = function(image, address) else {
            continue;
        };
        if voxel_playback(code, address) {
            found.push(address);
        }
        if depth >= 8 {
            continue;
        }
        for i in Decoder::with_ip(64, code, address as u64, DecoderOptions::NONE) {
            if matches!(i.mnemonic(), Mnemonic::Call | Mnemonic::Jmp)
                && i.op0_kind() == OpKind::NearBranch64
            {
                let target = i.near_branch_target() as usize;
                // Follow the bounded history implementation, not allocator/VM graphs.
                if target.abs_diff(finish) < 2 * 1024 * 1024 {
                    pending.push_back((target, depth + 1));
                }
            }
        }
    }
    anyhow::ensure!(
        found.len() == 1,
        "Studio voxel history playback is missing or ambiguous"
    );
    let target = found[0];
    let code = function(image, target).context("Missing voxel playback code")?;
    let mut length = 0;
    for i in Decoder::with_ip(64, code, target as u64, DecoderOptions::NONE) {
        anyhow::ensure!(
            !i.is_invalid()
                && i.flow_control() == FlowControl::Next
                && !i.is_ip_rel_memory_operand(),
            "Voxel playback prologue cannot be relocated"
        );
        length += i.len();
        if length >= 14 {
            break;
        }
    }
    anyhow::ensure!(
        (14..=32).contains(&length),
        "Invalid voxel playback prologue"
    );
    Ok((target, length))
}

pub(super) fn binding(prepared: &NativeProperty, pid: u32, title: &str) -> Result<Vec<u8>> {
    let current_modules = modules(pid)?;
    let studio = current_modules
        .iter()
        .find(|m| m.name.eq_ignore_ascii_case("RobloxStudioBeta.exe"))
        .context("Missing Studio")?;
    let layout = package_layout(&studio.path)?;
    let model = active_data_model(pid, &prepared.memory, studio, layout.data, title)?;
    let descriptor = find_class_member_descriptor(
        &prepared.memory,
        read_u64(&prepared.parameters, 16)? as usize,
        model.layout,
        "FinishRecording",
    )?;
    let kind = read_rtti_type(&prepared.memory, descriptor, studio.base, studio.size)
        .context("Missing history descriptor type")?;
    anyhow::ensure!(
        kind == ".?AV?$BoundFuncDesc@VChangeHistoryService@RBX@@$$A6AXV?$basic_string@DU?$char_traits@D@std@@V?$allocator@D@2@@std@@W4FinishRecordingOperation@Enums@2@V?$optional@V?$shared_ptr@$$CBVValueTable@Reflection@RBX@@@std@@@4@@Z$0A@$02@Reflection@RBX@@",
        "Unsupported FinishRecording calling convention"
    );
    let bytes = fs::read(&studio.path)?;
    let image = PeImage::parse(&bytes)?;
    let fields = prepared.memory.read_vec(descriptor, 256)?;
    let candidates = (64..256)
        .step_by(8)
        .filter_map(|offset| {
            let target = read_u64(&fields, offset).ok()? as usize;
            let rva = target.checked_sub(studio.base)?;
            function(&image, rva)?;
            Some((offset, target))
        })
        .collect::<Vec<_>>();
    anyhow::ensure!(
        candidates.len() == 1,
        "History member function is missing or ambiguous"
    );
    let (offset, finish) = candidates[0];
    anyhow::ensure!(
        read_i32(&fields, offset + 8)? == 0,
        "Unsupported history member adjustment"
    );
    let (voxel, length) = discover(&image, finish - studio.base)?;
    let code = function(&image, voxel).unwrap();
    verified_code(
        &prepared.memory,
        studio,
        &layout,
        studio.base + voxel,
        code.len(),
    )?;
    verified_code(
        &prepared.memory,
        studio,
        &layout,
        finish,
        function(&image, finish - studio.base).unwrap().len(),
    )?;
    let mut binding = vec![0; 80];
    for (at, value) in [
        (0, descriptor),
        (8, prepared.memory.read_u64(descriptor)? as usize),
        (16, offset),
        (24, finish),
        (32, studio.base + voxel),
    ] {
        put_u64(&mut binding, at, value);
    }
    put_u32(&mut binding, 40, length as u32);
    binding[48..48 + length].copy_from_slice(&code[..length]);
    Ok(binding)
}
