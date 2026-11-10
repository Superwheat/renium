use super::super::arm64_serializer::{self, Code};
use super::*;

const CONTROL_TYPE: &[u8] =
    b"NSt3__120__shared_ptr_emplaceI21StudioLocalSaveHelperNS_9allocatorIS1_EEEE\0";

fn data_references(image: &MachImage<'_>, value: u64) -> Vec<u64> {
    memchr::memmem::find_iter(image.bytes, &value.to_le_bytes())
        .filter(|offset| offset % 8 == 0)
        .filter_map(|offset| image.address_for_offset(offset))
        .filter(|address| image.text_offset_for_address(*address).is_none())
        .collect()
}

fn value_at(image: &MachImage<'_>, address: u64) -> Option<u64> {
    image.sections.iter().find_map(|section| {
        let relative = address.checked_sub(section.address)?;
        if relative.checked_add(8)? > section.size {
            return None;
        }
        read_u64(image.bytes, section.offset.checked_add(relative as usize)?)
    })
}

fn control_table(image: &MachImage<'_>) -> Result<u64> {
    let name = find_unique_bytes(image.bytes, CONTROL_TYPE, "local-save control type")?;
    let name = image
        .address_for_offset(name)
        .context("Local-save RTTI is outside the image")?;
    let mut tables = Vec::new();
    for field in data_references(image, name) {
        let Some(info) = field.checked_sub(8) else {
            continue;
        };
        for reference in data_references(image, info) {
            let table = reference + 8;
            if reference.checked_sub(8).and_then(|at| value_at(image, at)) == Some(0)
                && [0, 8, 16, 32].into_iter().all(|slot| {
                    value_at(image, table + slot).is_some_and(|function| {
                        image.function_starts.binary_search(&function).is_ok()
                    })
                })
            {
                tables.push(table);
            }
        }
    }
    anyhow::ensure!(
        tables.len() == 1,
        "Local-save control vtable resolved {} candidates",
        tables.len()
    );
    Ok(tables[0])
}

fn allocation_function(image: &MachImage<'_>) -> Result<u64> {
    let mut cursor = MACH_HEADER_64_SIZE;
    for _ in 0..read_u32(image.bytes, 16).context("Missing Mach-O commands")? {
        let command = read_u32(image.bytes, cursor).context("Truncated Mach-O command")?;
        let size =
            read_u32(image.bytes, cursor + 4).context("Truncated Mach-O command size")? as usize;
        if command == 2 {
            // LC_SYMTAB; the allocator is an exported C++ ABI symbol.
            let symbols = read_u32(image.bytes, cursor + 8).context("Missing symbols")? as usize;
            let count =
                read_u32(image.bytes, cursor + 12).context("Missing symbol count")? as usize;
            let strings =
                read_u32(image.bytes, cursor + 16).context("Missing string table")? as usize;
            for index in 0..count {
                let entry = symbols + index * 16;
                let name = read_u32(image.bytes, entry).context("Truncated symbol")? as usize;
                if image.bytes.get(strings + name..strings + name + 7) == Some(b"__Znwm\0") {
                    let address =
                        read_u64(image.bytes, entry + 8).context("Missing allocator address")?;
                    anyhow::ensure!(
                        image.function_starts.binary_search(&address).is_ok(),
                        "Allocator has no function boundary"
                    );
                    return Ok(address);
                }
            }
        }
        cursor += size;
    }
    bail!("Studio C++ allocator symbol is unavailable")
}

pub(super) fn trace(image: &MachImage<'_>, log_xref: u64) -> Result<SerializerTrace> {
    let code = Code {
        bytes: image.text_bytes()?,
        address: image.text.address,
        starts: &image.function_starts,
    };
    let serializer = code
        .containing(log_xref)
        .context("Serializer log has no function boundary")?;
    let execute = code
        .execute(serializer)
        .context("Studio local-save execution graph is missing or ambiguous")?;
    let table = control_table(image)?;
    let mut constructors = arm64_address_xrefs(image, table)?
        .into_iter()
        .filter_map(|site| code.containing(site))
        .filter(|&start| {
            code.body(start)
                .is_some_and(|body| arm64_serializer::constructor(body, start, table))
        })
        .collect::<Vec<_>>();
    constructors.sort_unstable();
    constructors.dedup();
    anyhow::ensure!(
        constructors.len() == 1,
        "Local-save control constructor resolved {} candidates",
        constructors.len()
    );
    let constructor = constructors[0];
    let allocator = allocation_function(image)?;
    let factories = code
        .callers(&[constructor])
        .into_keys()
        .filter(|&start| {
            code.body(start)
                .is_some_and(|body| arm64_serializer::factory(body, start, constructor, allocator))
        })
        .collect::<Vec<_>>();
    anyhow::ensure!(
        factories.len() == 1,
        "Local-save shared_ptr factory resolved {} candidates",
        factories.len()
    );
    Ok(SerializerTrace {
        factory_rva: factories[0]
            .checked_sub(image.image_base)
            .context("Factory precedes image")?,
        execute_rva: execute
            .checked_sub(image.image_base)
            .context("Execute precedes image")?,
        image_uuid: image.image_uuid,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[ignore = "requires an explicitly selected installed Studio engine"]
    fn installed_mac_serializer_structure() -> Result<()> {
        let path = std::env::var("RENIUM_SERIALIZER_TEST_IMAGE")?;
        let trace = trace_studio(Path::new(&path))?;
        println!(
            "factory={:#x} execute={:#x}",
            trace.factory_rva, trace.execute_rva
        );
        Ok(())
    }

    #[test]
    #[ignore = "requires an explicitly selected scratch Studio and output directory"]
    fn exercise_native_scratch_snapshot() -> Result<()> {
        let pid = std::env::var("RENIUM_NATIVE_TARGET_PID")?.parse()?;
        let place = PathBuf::from(std::env::var("RENIUM_NATIVE_PLACE")?);
        anyhow::ensure!(
            crate::studio::input::studio_document_path(pid)?.canonicalize()?
                == place.canonicalize()?,
            "The selected process must own the specified scratch place"
        );
        let title = crate::studio::input::studio_window_title(pid)?;
        let directory = PathBuf::from(std::env::var("RENIUM_NATIVE_ARTIFACTS")?);
        fs::create_dir_all(&directory)?;
        for index in 0..3 {
            let output = directory.join(format!(
                "native-snapshot-{pid}-{}-{index}.rbxl",
                current_millis()
            ));
            let snapshot = write_live_place(pid, &title, &output)?;
            anyhow::ensure!(snapshot.instance_count > 0, "The native snapshot is empty");
            println!(
                "snapshot: {} instances, {} bytes, {:.1}ms; {}",
                snapshot.instance_count,
                snapshot.output_size,
                snapshot.elapsed_ms,
                output.display()
            );
        }
        Ok(())
    }
}
