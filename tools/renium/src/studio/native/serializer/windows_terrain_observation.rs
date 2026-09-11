//! Resolve the Terrain listener interface and Instance property notification ABI.
use super::*;

pub(crate) fn observe_terrain(pid: u32, title: &str, relay_path: &[String]) -> Result<()> {
    anyhow::ensure!(
        relay_path.len() == 2
            && relay_path[0] == "CoreGui"
            && relay_path[1].starts_with("ReniumTerrainChanges_"),
        "Invalid Terrain relay path"
    );
    let relay = prepare_property(
        pid,
        title,
        relay_path,
        &[],
        "Value",
        Duration::from_secs(10),
    )?;
    anyhow::ensure!(
        relay.class_name == "BoolValue",
        "Terrain relay changed class"
    );
    relay.ensure_writable()?;
    let mut property = prepare_property(
        pid,
        title,
        &["Workspace".into(), "Terrain".into()],
        &[],
        "Name",
        Duration::from_secs(10),
    )?;
    anyhow::ensure!(
        property.class_name == "Terrain",
        "Voxel observer target is not Terrain"
    );
    property.invoke(8)?;
    let mut bytes = property.parameters[96..128].to_vec();
    if bytes.iter().all(|v| *v == 0) {
        bytes = binding(&property, pid)?;
    }
    let parent = relay.memory.read_u64(
        read_u64(&relay.parameters, 16)? as usize + read_u64(&relay.parameters, 131488)? as usize,
    )?;
    for value in [
        read_u64(&relay.parameters, 16)?,
        read_u64(&relay.parameters, 24)?,
        read_u64(&relay.parameters, 56)?,
        parent,
        read_u64(&relay.parameters, 65888)?,
        read_u64(&relay.parameters, 65896)?,
    ] {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    bytes.extend_from_slice(&relay.parameters[EXPECTED_IDENTITY..EXPECTED_IDENTITY + 16]);
    for field in [40, 64, 65904] {
        bytes.extend_from_slice(&read_u64(&relay.parameters, field)?.to_le_bytes());
    }
    property.parameters[INPUT..INPUT + bytes.len()].copy_from_slice(&bytes);
    put_u32(&mut property.parameters, 65916, bytes.len() as u32);
    property.invoke(8)
}

fn binding(property: &NativeProperty, pid: u32) -> Result<Vec<u8>> {
    let modules = modules(pid)?;
    let studio = modules
        .iter()
        .find(|m| m.name.eq_ignore_ascii_case("RobloxStudioBeta.exe"))
        .context("Missing Studio")?;
    let layout = package_layout(&studio.path)?;
    let memory = &property.memory;
    let instance = read_u64(&property.parameters, 16)? as usize;
    let primary = memory.read_u64(instance)? as usize;
    let locator = memory.read_u64(primary - 8)? as usize;
    anyhow::ensure!(memory.read_u32(locator)? == 1, "Unsupported Terrain RTTI");
    let hierarchy = studio.base + memory.read_u32(locator + 16)? as usize;
    let count = memory.read_u32(hierarchy + 8)? as usize;
    anyhow::ensure!(count < 100, "Unexpected Terrain inheritance count");
    let array = studio.base + memory.read_u32(hierarchy + 12)? as usize;
    let mut offsets = Vec::new();
    for i in 0..count {
        let descriptor = studio.base + memory.read_u32(array + i * 4)? as usize;
        let name = read_c_string(
            memory,
            studio.base + memory.read_u32(descriptor)? as usize + 16,
            4096,
        );
        if name.as_deref() == Some(".?AVGridListener@Voxel2@RBX@@")
            && memory.read_u32(descriptor + 12)? == u32::MAX
        {
            offsets.push(memory.read_u32(descriptor + 8)? as usize);
        }
    }
    anyhow::ensure!(
        offsets.len() == 1,
        "Terrain listener base is missing or ambiguous"
    );
    let offset = offsets[0];
    let table = memory.read_u64(instance + offset)? as usize;
    let secondary = memory.read_u64(table - 8)? as usize;
    anyhow::ensure!(
        memory.read_u32(secondary + 4)? as usize == offset
            && memory.read_u32(secondary + 12)? == memory.read_u32(locator + 12)?,
        "Terrain listener complete-object identity changed"
    );
    let mut count = 0;
    let mut slots = Vec::new();
    for slot in (0..128).step_by(8) {
        let target = memory.read_u64(table + slot)? as usize;
        let Ok(code) = verified_code(memory, studio, &layout, target, 256) else {
            break;
        };
        count += 1;
        let i = Decoder::with_ip(64, &code, target as u64, DecoderOptions::NONE)
            .into_iter()
            .take(2)
            .collect::<Vec<_>>();
        if i.len() == 2
            && i[0].mnemonic() == Mnemonic::Test
            && i[0].op0_register() == Register::R8D
            && i[0].op1_register() == Register::R8D
            && i[1].mnemonic() == Mnemonic::Jne
        {
            slots.push(slot);
        }
    }
    anyhow::ensure!(
        count < 16 && slots.len() == 1,
        "Terrain change callback is missing or ambiguous"
    );
    Ok([table, offset, count, slots[0]]
        .into_iter()
        .flat_map(|v| (v as u64).to_le_bytes())
        .collect())
}
