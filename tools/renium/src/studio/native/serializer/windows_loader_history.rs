//! Locate the history insertion callback through its complete-object RTTI,
//! record selection and LuaSourceContainer tail. No build-specific addresses.
use super::*;
use iced_x86::{Decoder, DecoderOptions, Instruction, Mnemonic, OpKind, Register};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct HistoryTrace {
    pub table: usize,
    pub slots: usize,
    pub insertion: usize,
    pub record: usize,
    pub pending: usize,
    pub playback: usize,
}

fn function<'a>(image: &PeImage<'a>, rva: usize) -> Option<&'a [u8]> {
    image.require_executable_rva(rva).ok()?;
    let offset = image.rva_to_offset(rva).ok()?;
    let (start, end) = image.function_bounds(offset).ok()?;
    (start == offset && end - start <= 16_384).then_some(&image.bytes[start..end])
}

fn source_class(image: &PeImage<'_>, rva: usize) -> bool {
    let Some(bytes) = function(image, rva) else {
        return false;
    };
    Decoder::with_ip(64, bytes, rva as u64, DecoderOptions::NONE)
        .into_iter()
        .any(|i| {
            i.mnemonic() == Mnemonic::Lea
                && i.op0_register() == Register::R8
                && i.is_ip_rel_memory_operand()
                && image
                    .rva_to_offset(i.ip_rel_memory_address() as usize)
                    .ok()
                    .is_some_and(|offset| {
                        image.bytes.get(offset..offset + 19) == Some(b"LuaSourceContainer\0")
                    })
        })
}

fn history_field(i: &Instruction, size: usize) -> Option<usize> {
    let offset = i.memory_displacement64() as usize;
    (i.memory_base() == Register::RCX
        && i.memory_index() == Register::None
        && i.memory_size().size() == size
        && (0x80..0x800).contains(&offset)
        && (size == 1 || offset.is_multiple_of(8)))
    .then_some(offset)
}

fn insertion(image: &PeImage<'_>, rva: usize) -> Option<(usize, usize, usize, usize)> {
    let bytes = function(image, rva)?;
    let code = Decoder::with_ip(64, bytes, rva as u64, DecoderOptions::NONE)
        .into_iter()
        .collect::<Vec<_>>();
    if code.iter().any(Instruction::is_invalid) {
        return None;
    }
    // Before the first direct call, the callback selects the current/pending
    // record and rejects playback. These are this-relative fields, not offsets
    // guessed from plausible live memory.
    let first_call = code
        .iter()
        .position(|i| i.mnemonic() == Mnemonic::Call && i.op0_kind() == OpKind::NearBranch64)?;
    let prefix = &code[..first_call];
    let playback_index = prefix
        .iter()
        .position(|i| i.mnemonic() == Mnemonic::Cmp && history_field(i, 1).is_some())?;
    let playback = history_field(&prefix[playback_index], 1)?;
    let zero = prefix[playback_index].op1_register();
    if prefix.get(playback_index + 1)?.mnemonic() != Mnemonic::Je
        || !prefix[..playback_index].iter().any(|i| {
            i.mnemonic() == Mnemonic::Xor
                && i.op0_register().full_register() == zero.full_register()
                && i.op0_register() == i.op1_register()
        })
    {
        return None;
    }
    let selected = prefix
        .iter()
        .enumerate()
        .filter_map(|(index, i)| {
            (i.mnemonic() == Mnemonic::Mov && i.op0_kind() == OpKind::Register)
                .then(|| history_field(i, 8).map(|field| (index, field, i.op0_register())))?
        })
        .collect::<Vec<_>>();
    let [
        (record_at, record, record_register),
        (pending_at, pending, pending_register),
    ] = selected.as_slice()
    else {
        return None;
    };
    if record == pending
        || *record_at <= playback_index
        || *pending_at <= *record_at
        || !prefix[*record_at..*pending_at].iter().any(|i| {
            i.mnemonic() == Mnemonic::Cmp
                && history_field(i, 8) == Some(*pending)
                && i.op1_register() == *record_register
        })
        || !prefix[*pending_at..].iter().any(|i| {
            i.mnemonic() == Mnemonic::Cmovne
                && i.op0_register() == *record_register
                && i.op1_register() == *pending_register
        })
        || prefix.last()?.mnemonic() != Mnemonic::Mov
        || prefix.last()?.op0_register() != Register::RDX
        || prefix.last()?.memory_base() != Register::RDX
        || prefix.last()?.memory_displacement64() != 0
    {
        return None;
    }
    let calls = code
        .iter()
        .enumerate()
        .filter(|(_, i)| i.mnemonic() == Mnemonic::Call && i.op0_kind() == OpKind::NearBranch64)
        .collect::<Vec<_>>();
    let sources = calls
        .iter()
        .enumerate()
        .filter(|(_, (_, i))| source_class(image, i.near_branch_target() as usize))
        .collect::<Vec<_>>();
    let [(source_index, (source_at, source))] = sources.as_slice() else {
        return None;
    };
    // Admission, identity context, entry and capture precede the script-only
    // tail. After it: ScriptGuid setup, identity restore, shared_ptr release.
    if *source_index != 5
        || calls.len() != 9
        || calls[1].1.near_branch_target() != calls[7].1.near_branch_target()
    {
        return None;
    }
    let tail = code.get(source_at + 1..source_at + 6)?;
    if tail[0].mnemonic() != Mnemonic::Movzx
        || tail[1].mnemonic() != Mnemonic::Movzx
        || tail[2].mnemonic() != Mnemonic::Sub
        || tail[3].mnemonic() != Mnemonic::Movzx
        || tail[4].mnemonic() != Mnemonic::Cmp
        || tail[0].memory_size().size() != 2
        || tail[1].memory_base() != Register::RAX
        || tail[0].memory_displacement64() != tail[1].memory_displacement64()
        || tail[3].memory_base() != Register::RAX
        || tail[3].memory_displacement64() != tail[1].memory_displacement64() + 2
        || code.get(source_at + 6)?.mnemonic() != Mnemonic::Ja
    {
        return None;
    }
    Some((
        *record,
        *pending,
        playback,
        source.near_branch_target() as usize,
    ))
}

pub(super) fn discover(image: &PeImage<'_>) -> Result<(HistoryTrace, Vec<usize>)> {
    let rdata = image.section(b".rdata")?;
    let data = &image.bytes[rdata.raw_offset..rdata.raw_offset + rdata.raw_size];
    let mut found = Vec::new();
    for name in memmem::find_iter(image.bytes, b".?AVChangeHistoryService@RBX@@\0") {
        let type_rva =
            image.offset_to_rva(name.checked_sub(16).context("Invalid history RTTI")?)?;
        for reference in memmem::find_iter(data, &(type_rva as u32).to_le_bytes()) {
            let Some(locator) = reference
                .checked_sub(12)
                .map(|offset| rdata.raw_offset + offset)
            else {
                continue;
            };
            let locator_rva = image.offset_to_rva(locator)?;
            if locator % 4 != 0
                || read_u32(image.bytes, locator)? != 1
                || read_u32(image.bytes, locator + 4)? != 0
                || read_u32(image.bytes, locator + 8)? != 0
                || read_u32(image.bytes, locator + 20)? as usize != locator_rva
            {
                continue;
            }
            for pointer in memmem::find_iter(
                data,
                &((image.image_base + locator_rva) as u64).to_le_bytes(),
            ) {
                let table_offset = rdata.raw_offset + pointer + 8;
                if table_offset % 8 != 0 {
                    continue;
                }
                let mut methods = Vec::new();
                for index in 0..512 {
                    let address = read_u64(image.bytes, table_offset + index * 8)? as usize;
                    let Some(rva) = address.checked_sub(image.image_base) else {
                        break;
                    };
                    if image.require_executable_rva(rva).is_err() {
                        break;
                    }
                    methods.push(rva);
                }
                if methods.is_empty() || methods.len() == 512 {
                    continue;
                }
                for (index, &method) in methods.iter().enumerate() {
                    if let Some((record, pending, playback, source)) = insertion(image, method) {
                        found.push((
                            HistoryTrace {
                                table: image.offset_to_rva(table_offset)?,
                                slots: methods.len(),
                                insertion: index,
                                record,
                                pending,
                                playback,
                            },
                            vec![method, source],
                        ));
                    }
                }
            }
        }
    }
    anyhow::ensure!(
        found.len() == 1,
        "Studio history insertion layout is unrecognized; Renium's native reader detector needs updating"
    );
    Ok(found.remove(0))
}
