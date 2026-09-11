//! Discover BinaryString access through the descriptor's native copy method.
use super::*;

fn copy_calls(code: &[u8], fields: &[usize]) -> Option<(usize, usize, usize)> {
    use Operand::*;
    let mut r = HashMap::from([
        (Register::RCX, Descriptor),
        (Register::RDX, Instance),
        (Register::R8, Instance),
    ]);
    let mut getter = None;
    let mut setter = None;
    for i in Decoder::new(64, code, DecoderOptions::NONE) {
        if i.is_invalid() {
            return None;
        }
        match i.mnemonic() {
            Mnemonic::Ret => {
                return getter.zip(setter).and_then(|((field, get), (other, set))| {
                    (field == other).then_some((field, get, set))
                });
            }
            Mnemonic::Mov | Mnemonic::Lea if i.op0_kind() == OpKind::Register => {
                let value = setter_operand(&i, &r, fields);
                r.remove(&i.op0_register().full_register());
                if let Some(value) = value {
                    r.insert(i.op0_register().full_register(), value);
                }
            }
            Mnemonic::Call => {
                if i.op0_kind() == OpKind::Memory
                    && i.memory_index() == Register::None
                    && !i.is_ip_rel_memory_operand()
                {
                    let Some(Vtable(field)) = r.get(&i.memory_base().full_register()).copied()
                    else {
                        return None;
                    };
                    if r.get(&Register::RCX) != Some(&Binding(field)) {
                        return None;
                    }
                    let slot = i.memory_displacement64() as usize;
                    if getter.is_none()
                        && r.get(&Register::RDX) == Some(&Scratch)
                        && r.get(&Register::R8) == Some(&Instance)
                    {
                        getter = Some((field, slot));
                    } else if getter.is_some()
                        && setter.is_none()
                        && r.get(&Register::RDX) == Some(&Instance)
                        && r.get(&Register::R8) == Some(&Scratch)
                    {
                        setter = Some((field, slot));
                    } else {
                        return None;
                    }
                } else if setter.is_none() {
                    return None;
                }
                for reg in [
                    Register::RAX,
                    Register::RCX,
                    Register::RDX,
                    Register::R8,
                    Register::R9,
                    Register::R10,
                    Register::R11,
                ] {
                    r.remove(&reg);
                }
            }
            Mnemonic::Push | Mnemonic::Pop | Mnemonic::Cmp | Mnemonic::Test | Mnemonic::Nop => {}
            _ => {
                let mut factory = InstructionInfoFactory::new();
                for used in factory.info(&i).used_registers() {
                    if matches!(
                        used.access(),
                        OpAccess::Write
                            | OpAccess::CondWrite
                            | OpAccess::ReadWrite
                            | OpAccess::ReadCondWrite
                    ) {
                        r.remove(&used.register().full_register());
                    }
                }
            }
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
    let current_modules = modules(pid)?;
    let studio = current_modules
        .iter()
        .find(|m| m.name.eq_ignore_ascii_case("RobloxStudioBeta.exe"))
        .context("Missing Studio")?;
    let layout = package_layout(&studio.path)?;
    let model = active_data_model(pid, &property.memory, studio, layout.data, title)?;
    let mut output = Vec::with_capacity(96);
    for name in ["SmoothGrid", "PhysicsGrid"] {
        let descriptor = find_class_member_descriptor(
            &property.memory,
            read_u64(&property.parameters, 16)? as usize,
            model.layout,
            name,
        )?;
        anyhow::ensure!(
            read_rtti_type(&property.memory, descriptor, studio.base, studio.size).as_deref()
                == Some(
                    ".?AV?$PropDescriptor@VMegaClusterInstance@RBX@@VBinaryString@2@@Reflection@RBX@@"
                ),
            "Unsupported Terrain property type"
        );
        let candidates=bindings(&property.memory,studio,descriptor)?.into_iter().filter(|(_,binding)| read_rtti_type(&property.memory,*binding,studio.base,studio.size).as_deref()==Some(".?AV?$GetSetImpl@P8TerrainProp@RBX@@EBA?AVBinaryString@2@XZP812@EAAXV32@@Z@?$PropDescriptor@VMegaClusterInstance@RBX@@VBinaryString@2@@Reflection@RBX@@")).collect::<Vec<_>>();
        let fields = candidates
            .iter()
            .map(|(field, _)| *field)
            .collect::<Vec<_>>();
        let table = property.memory.read_u64(descriptor)? as usize;
        let mut found = HashSet::new();
        for slot in (16..256).step_by(8) {
            let target = property.memory.read_u64(table + slot)? as usize;
            if let Ok(code) = verified_code(&property.memory, studio, &layout, target, 512)
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
        let binding = candidates
            .iter()
            .find(|(offset, _)| *offset == field)
            .unwrap()
            .1;
        let table = property.memory.read_u64(binding)? as usize;
        let get = property.memory.read_u64(table + get_slot)? as usize;
        let set = property.memory.read_u64(table + set_slot)? as usize;
        verified_code(&property.memory, studio, &layout, get, 128)?;
        verified_code(&property.memory, studio, &layout, set, 256)?;
        for value in [binding, table, get, set, get_slot, set_slot] {
            output.extend_from_slice(&(value as u64).to_le_bytes());
        }
    }
    let clear = find_class_member_descriptor(
        &property.memory,
        read_u64(&property.parameters, 16)? as usize,
        model.layout,
        "Clear",
    )?;
    anyhow::ensure!(
        read_rtti_type(&property.memory, clear, studio.base, studio.size).as_deref()
            == Some(
                ".?AV?$BoundFuncDesc@VMegaClusterInstance@RBX@@$$A6AXXZ$0A@$0A@@Reflection@RBX@@"
            ),
        "Unsupported Terrain Clear ABI"
    );
    let fields = property.memory.read_vec(clear, 264)?;
    let mut methods = Vec::new();
    for offset in (64..256).step_by(8) {
        let target = read_u64(&fields, offset)? as usize;
        if read_i32(&fields, offset + 8)? == 0
            && verified_code(&property.memory, studio, &layout, target, 64).is_ok()
        {
            methods.push((offset, target));
        }
    }
    anyhow::ensure!(
        methods.len() == 1,
        "Terrain Clear member is missing or ambiguous"
    );
    let (offset, target) = methods[0];
    for value in [
        clear,
        property.memory.read_u64(clear)? as usize,
        offset,
        target,
    ] {
        output.extend_from_slice(&(value as u64).to_le_bytes());
    }
    let acquisition = prepare_property(pid, title, path, ordinals, "AcquisitionMethod", timeout)?;
    acquisition.ensure_writable()?;
    let data = &acquisition.parameters;
    let table = read_u64(data, 64)? as usize;
    let get = read_u64(data, 48)? as usize;
    let set = read_u64(data, 65904)? as usize;
    let slot = |function| -> Result<usize> {
        let mut matches = Vec::new();
        for offset in (16..256).step_by(8) {
            if acquisition.memory.read_u64(table + offset)? as usize == function {
                matches.push(offset);
            }
        }
        anyhow::ensure!(matches.len() == 1, "Ambiguous Terrain metadata binding");
        Ok(matches[0])
    };
    for value in [
        read_u64(data, 40)? as usize,
        table,
        get,
        set,
        slot(get)?,
        slot(set)?,
    ] {
        output.extend_from_slice(&(value as u64).to_le_bytes());
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
        self.property.parameters.truncate(SIZE);
        self.property.parameters.extend_from_slice(&payload);
        self.property.invoke(7)?;
        self.fingerprint
            .copy_from_slice(&self.property.parameters[97..161]);
        Ok(self.property.parameters[96] != 0)
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
    register_history(pid, "ReniumTerrainTest.rbxl", &token)?;
    let mut terrain = prepare_terrain(
        pid,
        "ReniumTerrainTest.rbxl",
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
