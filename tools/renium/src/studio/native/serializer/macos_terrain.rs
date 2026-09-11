//! BinaryString getter/setter ABI proved by Studio's reflected copy method.
use super::*;

fn copy_calls(code: &[u8], fields: &[usize]) -> Option<(usize, usize, usize)> {
    use Argument::*;
    let mut r = [None; 32];
    r[0] = Some(Descriptor);
    r[1] = Some(Instance);
    r[2] = Some(Instance);
    r[31] = Some(Scratch(0));
    let mut vectors = [None; 32];
    let mut copied = [None; 24];
    let mut getter = None;
    let mut setter = None;
    let mut temporary = None;
    for bytes in code.chunks_exact(4) {
        let word = u32::from_le_bytes(bytes.try_into().ok()?);
        let base = ((word >> 5) & 31) as usize;
        if word == 0xd65f03c0 {
            return getter.zip(setter).and_then(|((field, get), (other, set))| {
                (field == other).then_some((field, get, set))
            });
        }
        if data_instruction(word, fields, &mut r, &mut vectors, &mut copied, None).is_some() {
            continue;
        }
        if word & 0xfffffc1f == 0xd63f0000 {
            if getter.is_none() {
                getter = indirect_binding(&r, base, CallKind::Text);
                temporary = match r[8] {
                    Some(Scratch(at)) => Some(at),
                    _ => return None,
                };
                getter?;
            } else if setter.is_none() && r[2] == temporary.map(Scratch) {
                setter = indirect_binding(&r, base, CallKind::Setter);
                setter?;
            } else {
                return None;
            }
            r[..19].fill(None);
        } else if word & 0xfc000000 == 0x94000000 {
            setter?;
            r[..19].fill(None);
        } else if word & 0x3b000000 != 0x29000000
            && word & 0x3b000000 != 0x39000000
            && word & 0x7c000000 != 0x14000000
            && word & 0x7e000000 != 0x36000000
        {
            r[(word & 31) as usize] = None;
        }
    }
    None
}

pub(crate) struct NativeTerrain {
    fingerprint: [u8; 64],
    property: NativeProperty,
    bindings: Vec<u8>,
}

pub(crate) fn prepare_terrain(
    pid: u32,
    title: &str,
    path: &[String],
    ordinals: &[usize],
    timeout: Duration,
) -> Result<NativeTerrain> {
    let property = prepare_property(pid, title, path, ordinals, "Name", timeout)?;
    anyhow::ensure!(
        property.class_name == "Terrain",
        "Native voxel target is not Terrain"
    );
    let memory = &property.memory;
    let mut output = Vec::with_capacity(96);
    for name in ["SmoothGrid", "PhysicsGrid"] {
        let descriptor = memory.member(
            read_u64(&property.parameters, 8).unwrap(),
            read_u64(&property.parameters, 80).unwrap(),
            name,
        )?;
        anyhow::ensure!(
            memory.rtti(descriptor)?
                == "N3RBX10Reflection14PropDescriptorINS_19MegaClusterInstanceENS_12BinaryStringEEE",
            "Unsupported Terrain property type"
        );
        let mut fields = Vec::new();
        for field in memory.bindings(descriptor)? {
            if memory.rtti(memory.pointer(descriptor + field as u64)?)?
                == "N3RBX10Reflection14PropDescriptorINS_19MegaClusterInstanceENS_12BinaryStringEE10GetSetImplIMNS_11TerrainPropEKFS3_vEMS6_FvS3_EEE"
            {
                fields.push(field);
            }
        }
        let table = memory.pointer(descriptor)?;
        let mut found = HashSet::new();
        for slot in (16..256).step_by(8) {
            let target = memory.pointer(table + slot)?;
            if let Ok(code) = memory.code(target, 512)
                && let Some(result) = copy_calls(&code, &fields)
            {
                found.insert(result);
            }
        }
        anyhow::ensure!(
            found.len() == 1,
            "Terrain native copy ABI is missing or ambiguous"
        );
        let (field, get_slot, set_slot) = *found.iter().next().unwrap();
        let binding = memory.pointer(descriptor + field as u64)?;
        let table = memory.pointer(binding)?;
        let get = memory.pointer(table + get_slot as u64)?;
        let set = memory.pointer(table + set_slot as u64)?;
        memory.code(get, 128)?;
        memory.code(set, 256)?;
        for value in [binding, table, get, set, get_slot as u64, set_slot as u64] {
            output.extend_from_slice(&value.to_le_bytes());
        }
    }
    let clear = memory.member(
        read_u64(&property.parameters, 8).unwrap(),
        read_u64(&property.parameters, 80).unwrap(),
        "Clear",
    )?;
    anyhow::ensure!(
        memory.rtti(clear)?
            == "N3RBX10Reflection13BoundFuncDescINS_19MegaClusterInstanceEFvvELb0ELi0EEE",
        "Unsupported Terrain Clear ABI"
    );
    let fields = memory.read(clear, 264)?;
    let mut methods = Vec::new();
    for offset in (64..256).step_by(8) {
        let target = read_u64(&fields, offset).unwrap();
        if read_u64(&fields, offset + 8) == Some(0) && memory.code(target, 64).is_ok() {
            methods.push((offset, target));
        }
    }
    anyhow::ensure!(
        methods.len() == 1,
        "Terrain Clear member is missing or ambiguous"
    );
    let (offset, target) = methods[0];
    for value in [clear, memory.pointer(clear)?, offset as u64, target] {
        output.extend_from_slice(&value.to_le_bytes());
    }
    let acquisition = prepare_property(pid, title, path, ordinals, "AcquisitionMethod", timeout)?;
    acquisition.ensure_writable()?;
    for offset in [24, 32, 48, 56, 104, 112] {
        output.extend_from_slice(&acquisition.parameters[offset..offset + 8]);
    }
    Ok(NativeTerrain {
        fingerprint: [0; 64],
        property,
        bindings: output,
    })
}

impl NativeTerrain {
    pub(crate) fn fingerprint(&self) -> String {
        base64::encode(self.fingerprint)
    }
    pub(crate) fn expect(&mut self, encoded: &str) -> Result<()> {
        self.fingerprint = base64::decode(encoded)?
            .try_into()
            .map_err(|_| anyhow::anyhow!("Invalid Terrain baseline"))?;
        Ok(())
    }
    pub(crate) fn write(
        &mut self,
        token: &str,
        smooth: Option<&[u8]>,
        physics: Option<&[u8]>,
    ) -> Result<bool> {
        let payload = crate::studio::native::serializer::terrain_payload(
            &self.bindings,
            token,
            &self.fingerprint,
            smooth,
            physics,
        )?;
        self.property.parameters.truncate(66216);
        self.property.parameters.extend_from_slice(&payload);
        let output = self.property.invoke(7)?;
        anyhow::ensure!(output.len() == 81, "Incomplete Terrain write response");
        self.fingerprint.copy_from_slice(&output[17..81]);
        Ok(output[16] != 0)
    }
}

#[test]
#[ignore = "Opt-in native Terrain transfer in an explicitly owned fixture"]
fn terrain_native_write_live_fixture() -> Result<()> {
    anyhow::ensure!(
        std::env::var("RENIUM_TERRAIN_WRITE_PROBE").as_deref() == Ok("1"),
        "Explicit write opt-in required"
    );
    let pid = std::env::var("RENIUM_INSPECT_FIXTURE_PID")?.parse()?;
    let token = std::env::var("RENIUM_HISTORY_PROBE_TOKEN")?;
    register_history(pid, "ReniumPropertyPackageTest.rbxl", &token)?;
    let mut terrain = prepare_terrain(
        pid,
        "ReniumPropertyPackageTest.rbxl",
        &["Workspace".into(), "Terrain".into()],
        &[],
        Duration::from_secs(10),
    )?;
    let source: serde_json::Value = serde_json::from_slice(&fs::read(
        std::env::var_os("RENIUM_TERRAIN_PROBE_SOURCE")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../../audit/release-readiness/terrain-transfer-before.json")
            }),
    )?)?;
    let fields = &source["roots"][0]["properties"];
    let smooth = base64::decode(
        fields["SmoothGrid"]["base64"]
            .as_str()
            .context("Missing smooth grid")?,
    )?;
    let physics = base64::decode(
        fields["PhysicsGrid"]["base64"]
            .as_str()
            .context("Missing physics grid")?,
    )?;
    terrain.write(&token, None, None)?;
    terrain.write(&token, Some(&smooth), Some(&physics))?;
    Ok(())
}
