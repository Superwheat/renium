//! ARM64 history discovery from reflected types and bounded native call graphs.
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
    Function,
}

fn voxel_playback(code: &[u8], address: u64) -> bool {
    let mut registers = [None; 32];
    registers[0] = Some(Value::Entry);
    registers[1] = Some(Value::Direction);
    let mut grid_call = false;
    let mut voxel_call = false;
    let mut loop_branch = false;
    for (offset, bytes) in code.chunks_exact(4).enumerate() {
        let word = u32::from_le_bytes(bytes.try_into().unwrap());
        let dst = (word & 31) as usize;
        let base = ((word >> 5) & 31) as usize;
        if word & 0xffe0ffe0 == 0xaa0003e0 {
            registers[dst] = registers[((word >> 16) & 31) as usize];
        } else if word & 0xffc00000 == 0xf9400000 {
            let field = ((word >> 10) & 4095) as usize * 8;
            registers[dst] = match registers[base] {
                Some(Value::Entry) if field == 0 => Some(Value::History),
                Some(Value::History) if (0x80..0x800).contains(&field) => Some(Value::Terrain),
                Some(Value::Terrain) if (0x100..0x800).contains(&field) => Some(Value::Grid),
                Some(Value::Grid) if field == 0 => Some(Value::Table),
                Some(Value::Table) if (0x80..0x200).contains(&field) => Some(Value::Function),
                _ => None,
            };
        } else if word & 0xfffffc1f == 0xd63f0000 || word & 0xfc000000 == 0x94000000 {
            if !grid_call {
                if word & 0xfffffc1f != 0xd63f0000
                    || registers[base] != Some(Value::Function)
                    || registers[0] != Some(Value::Grid)
                {
                    return false;
                }
                grid_call = true;
            } else if word & 0xfc000000 == 0x94000000
                && registers[0] == Some(Value::Entry)
                && registers[2] == Some(Value::Direction)
            {
                voxel_call = true;
            }
            registers[..19].fill(None);
        } else if word & 0x7e000000 == 0x34000000 || word & 0xff000010 == 0x54000000 {
            let delta = ((word << 8) as i32 >> 11) as i64;
            loop_branch |= (address + offset as u64 * 4).wrapping_add_signed(delta)
                < address + offset as u64 * 4;
        } else if word & 0x3b000000 != 0x29000000 // pair loads/stores
            && word & 0x3b000000 != 0x39000000 // stores
            && word & 0x7c000000 != 0x14000000 // branches
            && word & 0x7e000000 != 0x36000000 // test-bit branches
            && dst!=31
        {
            registers[dst] = None;
        }
    }
    grid_call && voxel_call && loop_branch
}

fn function<'a>(image: &MachImage<'a>, address: u64) -> Option<&'a [u8]> {
    let index = image.function_starts.binary_search(&address).ok()?;
    let end = *image.function_starts.get(index + 1)?;
    let length = usize::try_from(end - address).ok()?;
    if !(32..=16384).contains(&length) {
        return None;
    }
    let offset = image.text_offset_for_address(address)?;
    image.bytes.get(offset..offset + length)
}

fn discover(image: &MachImage<'_>, finish: u64) -> Result<u64> {
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
        for (offset, bytes) in code.chunks_exact(4).enumerate() {
            let word = u32::from_le_bytes(bytes.try_into().unwrap());
            if word & 0x7c000000 == 0x14000000 {
                let target = (address + offset as u64 * 4)
                    .wrapping_add_signed(((word << 6) as i32 >> 4) as i64);
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
    for bytes in function(image, target).unwrap()[..16].chunks_exact(4) {
        let word = u32::from_le_bytes(bytes.try_into().unwrap());
        anyhow::ensure!(
            word & 0xffc003ff == 0xd10003ff || word & 0xffc003e0 == 0xa90003e0,
            "Voxel playback prologue cannot be relocated"
        );
    }
    Ok(target)
}

pub(super) fn binding(prepared: &NativeProperty) -> Result<Vec<u8>> {
    let instance = read_u64(&prepared.parameters, 8).unwrap();
    let descriptor = prepared.memory.member(
        instance,
        read_u64(&prepared.parameters, 80).unwrap(),
        "FinishRecording",
    )?;
    anyhow::ensure!(
        prepared.memory.rtti(descriptor)?
            == "N3RBX10Reflection13BoundFuncDescINS_20ChangeHistoryServiceEFvNSt3__112basic_stringIcNS3_11char_traitsIcEENS3_9allocatorIcEEEENS_5Enums24FinishRecordingOperationENS3_8optionalINS3_10shared_ptrIKNS0_10ValueTableEEEEEELb0ELi3EEE",
        "Unsupported FinishRecording calling convention"
    );
    let bytes = fs::read(&prepared.memory.executable)?;
    let image = MachImage::parse(&bytes)?;
    anyhow::ensure!(
        image.cpu == CPU_TYPE_ARM64,
        "Terrain history requires the native ARM64 engine ABI"
    );
    let fields = prepared.memory.read(descriptor, 256)?;
    let candidates = (64..256)
        .step_by(8)
        .filter_map(|offset| {
            let target = read_u64(&fields, offset)?;
            let native = target
                .checked_sub(prepared.memory.base)?
                .checked_add(image.image_base)?;
            function(&image, native)?;
            Some((offset, target, native))
        })
        .collect::<Vec<_>>();
    anyhow::ensure!(
        candidates.len() == 1,
        "History member function is missing or ambiguous"
    );
    let (offset, finish, native) = candidates[0];
    anyhow::ensure!(
        read_u64(&fields, offset + 8) == Some(0),
        "Unsupported history member adjustment"
    );
    let voxel = discover(&image, native)?;
    let code = function(&image, voxel).unwrap();
    let target = prepared.memory.base + voxel - image.image_base;
    prepared.memory.code(target, code.len())?;
    prepared
        .memory
        .code(finish, function(&image, native).unwrap().len())?;
    let mut binding = vec![0; 80];
    for (at, value) in [
        (0, descriptor),
        (8, prepared.memory.pointer(descriptor)?),
        (16, offset as u64),
        (24, finish),
        (32, target),
    ] {
        put64(&mut binding, at, value);
    }
    put32(&mut binding, 40, 16);
    binding[48..64].copy_from_slice(&code[..16]);
    Ok(binding)
}
