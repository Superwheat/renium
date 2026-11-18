//! Locate the history insertion callback through its complete-object RTTI,
//! record selection and LuaSourceContainer tail.
use super::*;
use iced_x86::{Decoder, DecoderOptions, Instruction, Mnemonic, OpKind, Register};

#[path = "windows_class_metadata.rs"]
mod metadata;
pub(super) use metadata::Evidence;

pub(super) struct Discovery {
    pub trace: HistoryTrace,
    pub code: Vec<usize>,
    pub metadata: Evidence,
}

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

fn source_class(image: &PeImage<'_>, rva: usize, class_table: Option<usize>) -> Option<Evidence> {
    let bytes = function(image, rva)?;
    let direct = Decoder::with_ip(64, bytes, rva as u64, DecoderOptions::NONE)
        .into_iter()
        .find(|i| {
            i.mnemonic() == Mnemonic::Lea
                && i.op0_register() == Register::R8
                && i.is_ip_rel_memory_operand()
                && image
                    .rva_to_offset(i.ip_rel_memory_address() as usize)
                    .ok()
                    .is_some_and(|offset| {
                        image.bytes.get(offset..offset + SOURCE_CLASS_NAME.len())
                            == Some(SOURCE_CLASS_NAME)
                    })
        });
    if let Some(name) = direct {
        return Some(Evidence {
            code: vec![rva],
            names: [name.ip_rel_memory_address() as usize].into(),
            ..Evidence::default()
        });
    }
    metadata::source(image, rva, class_table?)
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

fn insertion(
    image: &PeImage<'_>,
    rva: usize,
    class_table: Option<usize>,
) -> Option<(usize, usize, usize, Evidence)> {
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
    if calls.len() != 9 || calls[1].1.near_branch_target() != calls[7].1.near_branch_target() {
        return None;
    }
    let sources = calls
        .iter()
        .enumerate()
        .filter_map(|(index, (at, i))| {
            source_class(image, i.near_branch_target() as usize, class_table)
                .map(|evidence| (index, *at, evidence))
        })
        .collect::<Vec<_>>();
    let [(source_index, source_at, evidence)] = sources.as_slice() else {
        return None;
    };
    // Admission, identity context, entry and capture precede the script-only
    // tail. After it: ScriptGuid setup, identity restore, shared_ptr release.
    if *source_index != 5 {
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
    Some((*record, *pending, playback, evidence.clone()))
}

pub(super) fn discover(image: &PeImage<'_>) -> Result<Discovery> {
    let class_table =
        undo_selection::descriptor_vtable(image, b".?AVClassDescriptor@Reflection@RBX@@\0")
            .context("Cannot validate reflection ClassDescriptor RTTI for the service reader");
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
                    if let Some((record, pending, playback, metadata)) =
                        insertion(image, method, class_table.as_ref().ok().copied())
                    {
                        let mut code = metadata.code.clone();
                        code.push(method);
                        found.push(Discovery {
                            trace: HistoryTrace {
                                table: image.offset_to_rva(table_offset)?,
                                slots: methods.len(),
                                insertion: index,
                                record,
                                pending,
                                playback,
                            },
                            code,
                            metadata,
                        });
                    }
                }
            }
        }
    }
    anyhow::ensure!(
        found.len() == 1,
        "Studio history insertion check found {} validated callbacks; the service reader requires exactly one{}. Update Renium's finder or use --slow-import",
        found.len(),
        class_table
            .err()
            .map(|error| format!("; {error:#}"))
            .unwrap_or_default()
    );
    Ok(found.remove(0))
}

#[cfg(test)]
mod compatibility_tests {
    use super::*;

    // Synthetic image, not bytes copied from an installed Studio. The history
    // complete-object locator and direct source-name reference are independent
    // of whether ClassDescriptor RTTI is present.
    fn fixture() -> Result<Vec<u8>> {
        let mut bytes = vec![0; 0x4000];
        let name = b".?AVChangeHistoryService@RBX@@\0";
        bytes[0x3010..0x3010 + name.len()].copy_from_slice(name);
        bytes[0x3100..0x3100 + SOURCE_CLASS_NAME.len()].copy_from_slice(SOURCE_CLASS_NAME);
        for (at, value) in [
            (0x3200, 1u32),
            (0x320c, 0x3000),
            (0x3214, 0x3200),
            (0x3800, 0x1000),
            (0x3804, 0x1100),
            (0x380c, 0x1200),
            (0x3810, 0x1210),
            (0x3818, 0x1300),
            (0x381c, 0x1301),
        ] {
            bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
        }
        for (at, value) in [(0x3300, 0x3200u64), (0x3308, 0x1000)] {
            bytes[at..at + 8].copy_from_slice(&(0x140000000 + value).to_le_bytes());
        }
        let mut code =
            hex("33C03881800000007400488B998800000048399990000000488BB190000000480F45DE488B12")?;
        for index in 0..9 {
            let target = if index == 5 { 0x1200i32 } else { 0x1300 };
            let next = 0x1000 + code.len() as i32 + 5;
            code.push(0xe8);
            code.extend_from_slice(&(target - next).to_le_bytes());
            if index == 5 {
                code.extend(hex("0FB74B100FB750102BCA0FB740123BC87700")?);
            }
        }
        code.push(0xc3);
        bytes[0x1000..0x1100].fill(0x90);
        bytes[0x1000..0x1000 + code.len()].copy_from_slice(&code);
        bytes[0x1200..0x1203].copy_from_slice(&[0x4c, 0x8d, 0x05]);
        bytes[0x1203..0x1207].copy_from_slice(&(0x3100i32 - 0x1207).to_le_bytes());
        bytes[0x1207..0x1210].fill(0x90);
        bytes[0x1300] = 0xc3;
        Ok(bytes)
    }

    fn image(bytes: &[u8]) -> PeImage<'_> {
        PeImage {
            bytes,
            image_base: 0x140000000,
            image_stamp: [0; 3],
            sections: [
                (*b".text\0\0\0", 0x1000, 0x1000, 0x20000000),
                (*b".rdata\0\0", 0x3000, 0x800, 0),
                (*b".pdata\0\0", 0x3800, 36, 0),
            ]
            .into_iter()
            .map(|(name, start, size, characteristics)| PeSection {
                name,
                virtual_size: size,
                virtual_address: start,
                raw_size: size,
                raw_offset: start,
                characteristics,
            })
            .collect(),
        }
    }

    #[test]
    fn direct_source_history_does_not_require_class_descriptor_rtti() -> Result<()> {
        let bytes = fixture()?;
        let image = image(&bytes);
        assert!(
            undo_selection::descriptor_vtable(&image, b".?AVClassDescriptor@Reflection@RBX@@\0")
                .is_err()
        );
        let found = discover(&image)?;
        assert_eq!(
            (
                found.trace.record,
                found.trace.pending,
                found.trace.playback
            ),
            (0x88, 0x90, 0x80)
        );
        assert_eq!(found.trace.table, 0x3308);
        assert_eq!(
            found.metadata.names.into_iter().collect::<Vec<_>>(),
            [0x3100]
        );
        Ok(())
    }

    #[test]
    fn missing_source_and_ambiguous_history_keep_rtti_diagnostic() -> Result<()> {
        for duplicate in [false, true] {
            let mut bytes = fixture()?;
            if duplicate {
                bytes[0x3310..0x3318].copy_from_slice(&0x140001000u64.to_le_bytes());
            } else {
                bytes[0x3100] = b'X';
            }
            let error = discover(&image(&bytes))
                .err()
                .context("accepted invalid callback")?;
            let message = format!("{error:#}");
            assert!(message.contains("validated callbacks"), "{message}");
            assert!(message.contains("ClassDescriptor RTTI"), "{message}");
            assert!(message.contains("--slow-import"), "{message}");
        }
        Ok(())
    }
}
