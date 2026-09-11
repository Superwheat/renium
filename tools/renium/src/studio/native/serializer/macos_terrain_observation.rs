//! Itanium RTTI and ARM64 argument flow locate Terrain's voxel notification.
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
    let mut bytes = property.invoke(8)?[16..].to_vec();
    if bytes.iter().all(|v| *v == 0) {
        bytes = binding(&property)?;
    }
    let parent = relay.memory.pointer(
        read_u64(&relay.parameters, 8).unwrap() + read_u64(&relay.parameters, 96).unwrap(),
    )?;
    for value in [
        read_u64(&relay.parameters, 8).unwrap(),
        read_u64(&relay.parameters, 16).unwrap(),
        read_u64(&relay.parameters, 40).unwrap(),
        parent,
        read_u64(&relay.parameters, 64).unwrap(),
        read_u64(&relay.parameters, 72).unwrap(),
    ] {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    bytes.extend_from_slice(&relay.parameters[664..680]);
    for field in [24, 32, 56] {
        bytes.extend_from_slice(&read_u64(&relay.parameters, field).unwrap().to_le_bytes());
    }
    property.parameters[680..680 + bytes.len()].copy_from_slice(&bytes);
    put32(&mut property.parameters, 136, bytes.len() as u32);
    property.invoke(8).map(|_| ())
}

fn binding(property: &NativeProperty) -> Result<Vec<u8>> {
    let memory = &property.memory;
    let instance = read_u64(&property.parameters, 8).unwrap();
    let primary = memory.pointer(instance)?;
    let info = memory.pointer(primary - 8)?;
    anyhow::ensure!(
        memory.rtti(info)?.contains("__vmi_class_type_info"),
        "Unsupported Terrain inheritance"
    );
    let header = memory.read(info + 16, 8)?;
    let count = read_u32(&header, 4).unwrap();
    anyhow::ensure!(count <= 32, "Unexpected Terrain base count");
    let mut offsets = Vec::new();
    for i in 0..count {
        let entry = info + 24 + u64::from(i) * 16;
        let base = memory.pointer(entry)?;
        if memory.cstring(memory.pointer(base + 8)?)? == "N3RBX6Voxel212GridListenerE" {
            let flags = memory.pointer(entry + 8)?;
            anyhow::ensure!(
                flags & 1 == 0,
                "Virtual Terrain listener base is unsupported"
            );
            offsets.push(flags >> 8);
        }
    }
    anyhow::ensure!(
        offsets.len() == 1,
        "Terrain listener base is missing or ambiguous"
    );
    let offset = offsets[0];
    let table = memory.pointer(instance + offset)?;
    anyhow::ensure!(
        memory.pointer(table - 16)? as i64 == -(offset as i64)
            && memory.pointer(table - 8)? == info,
        "Terrain listener complete-object identity changed"
    );
    let mut count = 0;
    let mut slots = Vec::new();
    for slot in (0..128).step_by(8) {
        let method = memory.pointer(table + slot)?;
        let Ok(code) = memory.code(method, 16) else {
            break;
        };
        count += 1;
        let sub = read_u32(&code, 0).unwrap();
        let zero = read_u32(&code, 4).unwrap();
        let branch = read_u32(&code, 8).unwrap();
        if sub & 0xffc003ff == 0xd1000000
            && u64::from((sub >> 10) & 4095) == offset
            && zero == 0x52800003
            && branch & 0xfc000000 == 0x14000000
        {
            let target = (method as i64 + 8 + ((branch << 6) as i32 >> 4) as i64) as u64;
            let body = memory.code(target, 8)?;
            if read_u32(&body, 0).unwrap() & 0xff00001f == 0x34000002
                && read_u32(&body, 4).unwrap() == 0xd65f03c0
            {
                slots.push(slot);
            }
        }
    }
    anyhow::ensure!(
        count < 16 && slots.len() == 1,
        "Terrain change callback is missing or ambiguous"
    );
    Ok([table, offset, count, slots[0]]
        .into_iter()
        .flat_map(u64::to_le_bytes)
        .collect())
}
