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
    property.memory.authorize(NativeOperation::Terrain, title)?;
    let instance = read_u64(&property.parameters, 16)? as usize;
    let output = terrain_bindings(&property.memory, studio, &layout, &model, instance)?;
    Ok(NativeTerrain {
        fingerprint: [0; 64],
        property,
        bindings: output,
    })
}

fn terrain_bindings(
    memory: &ProcessMemory,
    studio: &ModuleEntry,
    layout: &PackageLayout,
    model: &ActiveDataModel,
    instance: usize,
) -> Result<Vec<u8>> {
    let mut output = Vec::with_capacity(96);
    for name in ["SmoothGrid", "PhysicsGrid"] {
        let descriptor = find_class_member_descriptor(memory, instance, model.layout, name)?;
        anyhow::ensure!(
            read_rtti_type(memory, descriptor, studio.base, studio.size).as_deref()
                == Some(
                    ".?AV?$PropDescriptor@VMegaClusterInstance@RBX@@VBinaryString@2@@Reflection@RBX@@"
                ),
            "Unsupported Terrain property type"
        );
        let candidates=bindings(memory,studio,descriptor)?.into_iter().filter(|(_,binding)| read_rtti_type(memory,*binding,studio.base,studio.size).as_deref()==Some(".?AV?$GetSetImpl@P8TerrainProp@RBX@@EBA?AVBinaryString@2@XZP812@EAAXV32@@Z@?$PropDescriptor@VMegaClusterInstance@RBX@@VBinaryString@2@@Reflection@RBX@@")).collect::<Vec<_>>();
        let fields = candidates
            .iter()
            .map(|(field, _)| *field)
            .collect::<Vec<_>>();
        let table = memory.read_u64(descriptor)? as usize;
        let mut found = HashSet::new();
        for target in vtable_methods(memory, studio, layout, table)? {
            if let Ok(code) = verified_code(memory, studio, layout, target, 512)
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
        let table = memory.read_u64(binding)? as usize;
        let get = memory.read_u64(table + get_slot)? as usize;
        let set = memory.read_u64(table + set_slot)? as usize;
        verified_code(memory, studio, layout, get, 128)?;
        verified_code(memory, studio, layout, set, 256)?;
        for value in [binding, table, get, set, get_slot, set_slot] {
            output.extend_from_slice(&(value as u64).to_le_bytes());
        }
    }
    let clear = find_class_member_descriptor(memory, instance, model.layout, "Clear")?;
    anyhow::ensure!(
        read_rtti_type(memory, clear, studio.base, studio.size).as_deref()
            == Some(
                ".?AV?$BoundFuncDesc@VMegaClusterInstance@RBX@@$$A6AXXZ$0A@$0A@@Reflection@RBX@@"
            ),
        "Unsupported Terrain Clear ABI"
    );
    let clear_table = memory.read_u64(clear)? as usize;
    let mut dispatchers = Vec::new();
    for (slot, method) in vtable_methods(memory, studio, layout, clear_table)?
        .into_iter()
        .enumerate()
    {
        if let Ok(code) = functions::dispatch_code(memory, studio, layout, method)
            && functions::void_dispatch_field(&code).is_some()
        {
            dispatchers.push((slot * 8, method));
        }
    }
    anyhow::ensure!(
        dispatchers.len() == 1,
        "Terrain Clear has no unique verified void dispatcher"
    );
    let (slot, target) = dispatchers[0];
    // Low bit selects the engine dispatcher ABI; aligned legacy member offsets
    // remain available on macOS, which uses a different member-pointer ABI.
    let offset = slot | 1;
    for value in [clear, memory.read_u64(clear)? as usize, offset, target] {
        output.extend_from_slice(&(value as u64).to_le_bytes());
    }
    let descriptor =
        find_class_member_descriptor(memory, instance, model.layout, "AcquisitionMethod")?;
    let (get, set) = reflection_functions(memory, studio, layout, descriptor)?;
    anyhow::ensure!(
        set != 0,
        "Terrain acquisition metadata setter is unavailable"
    );
    let table = memory.read_u64(descriptor)? as usize;
    let metadata_methods = vtable_methods(memory, studio, layout, table)?;
    let slot = |function| -> Result<usize> {
        let mut matches = Vec::new();
        for (index, method) in metadata_methods.iter().enumerate() {
            if *method == function {
                matches.push(index * 8);
            }
        }
        anyhow::ensure!(matches.len() == 1, "Ambiguous Terrain metadata binding");
        Ok(matches[0])
    };
    for value in [descriptor, table, get, set, slot(get)?, slot(set)?] {
        output.extend_from_slice(&(value as u64).to_le_bytes());
    }
    Ok(output)
}

pub(in super::super) fn probe(
    memory: &ProcessMemory,
    studio: &ModuleEntry,
    layout: &PackageLayout,
    model: &ActiveDataModel,
) -> Result<serde_json::Value> {
    let path = resolve_path(memory, model, &["Workspace".into(), "Terrain".into()], &[])?;
    let instance = path.last().context("Terrain is missing")?.instance;
    let bindings = terrain_bindings(memory, studio, layout, model, instance)?;
    Ok(
        serde_json::json!({"instance": instance, "bindings": bindings.chunks_exact(8)
        .map(|word| u64::from_le_bytes(word.try_into().unwrap())).collect::<Vec<_>>()}),
    )
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
