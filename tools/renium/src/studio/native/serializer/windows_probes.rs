//! External-process probes: no remote allocation, writes, helper loads or calls.
use super::*;
use serde_json::{Value, json};

pub(in super::super) fn probe_operation(
    pid: u32,
    title: &str,
    operation: NativeOperation,
) -> Result<Value> {
    check(&ProcessMemory::open_read_only(pid)?, pid, title, operation)
}

pub(super) fn check(
    memory: &ProcessMemory,
    pid: u32,
    title: &str,
    operation: NativeOperation,
) -> Result<Value> {
    let loaded = modules(pid)?;
    let studio = loaded.first().context("Studio has no loaded image")?;
    anyhow::ensure!(
        studio.name.eq_ignore_ascii_case("RobloxStudioBeta.exe"),
        "Probe target is not Roblox Studio"
    );
    match operation {
        NativeOperation::UndoSelection => return undo_selection::probe(memory, studio),
        NativeOperation::PackageNotices => return package_notice::probe(memory, studio),
        NativeOperation::AudioGate => return audio_probe(memory, &loaded),
        NativeOperation::StudioActions => return action_probe(memory, pid, title, &loaded),
        NativeOperation::Input | NativeOperation::ViewportCapture => {
            let (window, _) = capture_window(pid, title)?;
            return Ok(
                json!({"window": window, "contract": "PID-scoped Windows window APIs; no engine offsets"}),
            );
        }
        _ => {}
    }
    let layout = package_layout(&studio.path)?;
    verify_loaded_image(memory, studio, layout.image_stamp)?;
    let model = active_data_model(pid, memory, studio, layout.data, title)?;
    let capture_binding = (operation == NativeOperation::Capture)
        .then(|| data_model_task_binding(memory, studio, &layout, &model))
        .transpose()?;
    let context = match capture_binding {
        Some(binding) => binding.field.pointer,
        None => data_model_task_context(memory, studio, &layout, &model)?,
    };
    properties::verified_code(
        memory,
        studio,
        &layout,
        studio.base + layout.submit_task,
        64,
    )?;
    let task_abi = task_abi::resolve(memory, studio, &layout, context)?;
    let allocator = allocator_probe(memory, studio, &loaded)?;
    let details = engine_probe(memory, studio, &layout, &model, operation)?;
    Ok(json!({
        "imageStamp": layout.image_stamp, "imageBase": studio.base,
        "dataModel": model.outer, "owner": model.owner,
        "instanceOffset": model.layout.data_model_instance,
        "childrenOffset": model.layout.children, "selfOffset": model.layout.self_pointer,
        "taskContext": context, "submitTaskRva": layout.submit_task,
        "taskContinuationAbi": task_abi as u32,
        "captureTaskContext": capture_binding.map(TaskContextBinding::readings),
        "binding": details, "allocator": allocator,
    }))
}

fn engine_probe(
    memory: &ProcessMemory,
    studio: &ModuleEntry,
    layout: &PackageLayout,
    model: &ActiveDataModel,
    operation: NativeOperation,
) -> Result<Value> {
    match operation {
        NativeOperation::PlayTeleport => Ok(json!({
            "launch": engine_probe(memory, studio, layout, model, NativeOperation::PlayTeleportLaunch)?,
            "load": engine_probe(memory, studio, layout, model, NativeOperation::PlayTeleportLoad)?,
            "arrival": playtest::join::probe(memory, studio)?,
        })),
        NativeOperation::PlayTeleportDispatch => playtest::dispatch::probe(memory, studio, model),
        NativeOperation::PlayTeleportJoin => playtest::join::probe(memory, studio),
        NativeOperation::PlayTeleportGui => playtest::gui::probe(memory, studio),
        NativeOperation::PlayTeleportLaunch => {
            let dispatch = playtest::dispatch::probe(memory, studio, model)?;
            let owner = playtest::owner::resolve(memory, studio, model)?;
            let loader = playtest::loader::resolve(memory, studio, model)?;
            helper_export_rva("ReniumPlayTeleport")?;
            helper_export_rva("ReniumPlayContinuations")?;
            Ok(
                json!({"dispatch":dispatch,"launch":owner.fields,"execute":owner.execute,
                "loaderSlot":loader.slot,"loaderFunction":loader.function,
                "loaderDirectModel":loader.direct_model}),
            )
        }
        NativeOperation::PlayTeleportLoad => {
            let binding = playtest::loader::resolve(memory, studio, model)?;
            let nonce = playtest::loader::nonce_bindings(memory, studio, model)?;
            Ok(
                json!({"callbackSlotRva":binding.slot,"callbackRva":binding.function,
                "captureField":binding.fields.capture,"stringField":binding.fields.string,
                "placeField":binding.fields.place,"universeField":binding.fields.universe,
                "launchCheck":nonce}),
            )
        }
        NativeOperation::Snapshot => snapshot_probe(memory, studio, layout),
        NativeOperation::Capture => {
            let serializer = snapshot_probe(memory, studio, layout)?;
            let instance = model.outer + model.layout.data_model_instance;
            let (binding, getter) =
                properties::identity_binding(memory, studio, layout, model, instance)?;
            let debug_id = properties::debug_id_function(memory, studio, layout, model, instance)?;
            let parent = properties::parent_offset(memory, model)?;
            Ok(json!({"serializer": serializer, "identityBinding": binding,
                "identityGetter": getter, "debugIdGetter": debug_id, "parentOffset": parent}))
        }
        NativeOperation::ServiceReader => {
            let (reader, _) = loader::verify_loaded(memory, studio)?;
            anyhow::ensure!(
                reader.instance_offset == model.layout.data_model_instance,
                "Native reader and live DataModel disagree on instance ownership"
            );
            let serializer = snapshot_probe(memory, studio, layout)?;
            Ok(
                json!({"loaderRva": reader.loader, "readerRva": reader.reader,
                "factoryRva": reader.factory.lookup, "serializer": serializer}),
            )
        }
        NativeOperation::DescriptorBinding => {
            properties::probe_binding(memory, studio, layout, model)
        }
        NativeOperation::History | NativeOperation::VoxelHistory => {
            let binding = properties::probe_history(memory, studio, layout, model)?;
            let voxel = read_u64(&binding, 32)?;
            anyhow::ensure!(
                operation != NativeOperation::VoxelHistory || voxel != 0,
                "Voxel playback call graph has no unique validated hook; explicit Terrain rollback is used"
            );
            Ok(
                json!({"descriptor": read_u64(&binding, 0)?, "memberSlot": read_u64(&binding, 16)?,
                "finishRecording": read_u64(&binding, 24)?, "voxelPlayback": voxel}),
            )
        }
        NativeOperation::TerrainObservation => {
            let binding = properties::probe_terrain_observation(memory, studio, layout, model)?;
            Ok(
                json!({"engineVtable": read_u64(&binding, 0)?, "listenerOffset": read_u64(&binding, 8)?,
                "vtableEntries": read_u64(&binding, 16)?, "callbackSlot": read_u64(&binding, 24)?}),
            )
        }
        NativeOperation::AttributeObservation => observation::probe(memory, studio, model),
        NativeOperation::PackageActions => {
            let bindings = ["PublishPackage", "SetPackageVersion"].into_iter().map(|name| {
                let binding = package_ui_binding(memory, studio, model, name)?;
                properties::verified_code(memory, studio, layout, studio.base + binding.operation_rva, 64)?;
                Ok(json!({"name": name, "service": binding.service, "operationRva": binding.operation_rva}))
            }).collect::<Result<Vec<_>>>()?;
            Ok(json!({"actions": bindings}))
        }
        NativeOperation::Terrain => properties::probe_terrain(memory, studio, layout, model),
        _ => anyhow::bail!("Operation does not use an engine binding"),
    }
}

fn snapshot_probe(
    memory: &ProcessMemory,
    studio: &ModuleEntry,
    layout: &PackageLayout,
) -> Result<Value> {
    let (_, trace, stamp) = studio_layout(&studio.path)?;
    verify_loaded_image(memory, studio, stamp)?;
    for rva in [
        trace.serializer,
        trace.context_builder,
        trace.context_destroy,
        trace.root_collector,
        trace.deallocator,
    ] {
        properties::verified_code(memory, studio, layout, studio.base + rva, 64)?;
    }
    Ok(
        json!({"serializerRva": trace.serializer, "contextBuilderRva": trace.context_builder,
        "contextDestroyRva": trace.context_destroy, "rootCollectorRva": trace.root_collector,
        "deallocatorRva": trace.deallocator}),
    )
}

fn audio_probe(memory: &ProcessMemory, loaded: &[ModuleEntry]) -> Result<Value> {
    let audio = loaded.iter().find(|module| module.name.eq_ignore_ascii_case("audioses.dll"))
        .context("Windows audio client is not initialized; the render gate will probe when a stream exists")?;
    let mapped = observation::ImageView::open(&audio.path)?;
    let image = PeImage::parse(mapped.bytes())?;
    verify_loaded_image(memory, audio, image.image_stamp)?;
    let factory = exported_rva(&image, "DllGetClassObject")?;
    let offset = image.rva_to_offset(factory)?;
    anyhow::ensure!(
        memory.read_vec(audio.base + factory, 32)? == slice(image.bytes, offset, 32)?,
        "Windows audio COM factory was changed"
    );
    Ok(
        json!({"module": audio.name, "base": audio.base, "factoryRva": factory,
        "contract": "IAudioRenderClient OS ABI; every actual vtable entry is probed before attachment"}),
    )
}

fn action_probe(
    memory: &ProcessMemory,
    pid: u32,
    title: &str,
    loaded: &[ModuleEntry],
) -> Result<Value> {
    let (window, _) = capture_window(pid, title)?;
    let mut exports = Vec::new();
    for (module, names) in [
        (
            "Qt5Core.dll",
            &[
                "?children@QObject@@QEBAAEBV?$QList@PEAVQObject@@@@XZ",
                "?objectName@QObject@@QEBA?AVQString@@XZ",
                "??1QString@@QEAA@XZ",
                "?inherits@QObject@@QEBA_NPEBD@Z",
            ][..],
        ),
        (
            "Qt5Widgets.dll",
            &[
                "?find@QWidget@@SAPEAV1@_K@Z",
                "?activate@QAction@@QEAAXW4ActionEvent@1@@Z",
                "?isEnabled@QAction@@QEBA_NXZ",
            ][..],
        ),
    ] {
        let module = loaded
            .iter()
            .find(|m| m.name.eq_ignore_ascii_case(module))
            .with_context(|| format!("Studio action dependency {module} is not loaded"))?;
        let mapped = observation::ImageView::open(&module.path)?;
        let image = PeImage::parse(mapped.bytes())?;
        verify_loaded_image(memory, module, image.image_stamp)?;
        for name in names {
            let rva = exported_rva(&image, name)?;
            let offset = image.rva_to_offset(rva)?;
            anyhow::ensure!(
                memory.read_vec(module.base + rva, 32)? == slice(image.bytes, offset, 32)?,
                "Qt action export {name} was changed"
            );
            exports.push(json!({"name":name,"module":module.name,"rva":rva}));
        }
    }
    Ok(
        json!({"window": window, "exports": exports, "contract": "Qt named exports and QObject/QAction runtime types"}),
    )
}

pub(super) fn image_string<'a>(image: &PeImage<'a>, rva: usize) -> Result<&'a str> {
    let start = image.rva_to_offset(rva)?;
    let bytes = &image.bytes[start..];
    let len = bytes
        .iter()
        .take(4096)
        .position(|b| *b == 0)
        .context("Unterminated PE name")?;
    std::str::from_utf8(&bytes[..len]).context("PE name is not UTF-8")
}

pub(super) fn directory(image: &PeImage<'_>, index: usize) -> Result<(usize, usize)> {
    let optional = read_u32(image.bytes, 0x3c)? as usize + 24;
    anyhow::ensure!(
        index < read_u32(image.bytes, optional + 108)? as usize,
        "PE directory is absent"
    );
    Ok((
        read_u32(image.bytes, optional + 112 + index * 8)? as usize,
        read_u32(image.bytes, optional + 116 + index * 8)? as usize,
    ))
}

fn exported_rva(image: &PeImage<'_>, name: &str) -> Result<usize> {
    let (rva, size) = directory(image, 0)?;
    let start = image.rva_to_offset(rva)?;
    let count = read_u32(image.bytes, start + 24)? as usize;
    let functions = image.rva_to_offset(read_u32(image.bytes, start + 28)? as usize)?;
    let names = image.rva_to_offset(read_u32(image.bytes, start + 32)? as usize)?;
    let ordinals = image.rva_to_offset(read_u32(image.bytes, start + 36)? as usize)?;
    for index in 0..count {
        if image_string(image, read_u32(image.bytes, names + index * 4)? as usize)? != name {
            continue;
        }
        let ordinal = read_u16(image.bytes, ordinals + index * 2)? as usize;
        anyhow::ensure!(
            ordinal < read_u32(image.bytes, start + 20)? as usize,
            "Invalid PE export ordinal"
        );
        let target = read_u32(image.bytes, functions + ordinal * 4)? as usize;
        anyhow::ensure!(
            !(rva..rva + size).contains(&target),
            "Forwarded export {name} needs a locator update"
        );
        image.require_executable_rva(target)?;
        return Ok(target);
    }
    bail!("Required export {name} is absent")
}

fn allocator_imports(image: &PeImage<'_>) -> Result<Vec<(String, usize)>> {
    let (rva, size) = directory(image, 1)?;
    let table = image.rva_to_offset(rva)?;
    let mut crt = Vec::new();
    for offset in (0..size.saturating_sub(19)).step_by(20) {
        let entry = table + offset;
        let name = read_u32(image.bytes, entry + 12)? as usize;
        if name == 0 {
            break;
        }
        let dll = image_string(image, name)?;
        let mimalloc = dll.eq_ignore_ascii_case("mimalloc.dll");
        let legacy = dll.eq_ignore_ascii_case("api-ms-win-crt-heap-l1-1-0.dll")
            || dll.eq_ignore_ascii_case("ucrtbase.dll");
        if !mimalloc && !legacy {
            continue;
        }
        let names = read_u32(image.bytes, entry)? as usize;
        let iat = read_u32(image.bytes, entry + 16)? as usize;
        anyhow::ensure!(
            names != 0 && iat != 0,
            "Engine allocator import table is incomplete"
        );
        let names = image.rva_to_offset(names)?;
        let mut imports = Vec::new();
        for slot in 0..4096 {
            let entry = read_u64(image.bytes, names + slot * 8)?;
            if entry == 0 {
                break;
            }
            if entry >> 63 != 0 {
                continue;
            }
            let name = image_string(image, entry as usize + 2)?;
            let required = if mimalloc {
                &["mi_malloc", "mi_free", "mi_malloc_aligned"][..]
            } else {
                &["malloc", "free", "_aligned_malloc", "_aligned_free"][..]
            };
            if required.contains(&name) {
                imports.push((name.to_owned(), iat + slot * 8));
            }
        }
        if mimalloc {
            anyhow::ensure!(
                imports.len() == 3,
                "Engine allocator requires mi_malloc, mi_free and mi_malloc_aligned"
            );
            return Ok(imports);
        }
        crt.extend(imports);
    }
    crt.sort_unstable();
    crt.dedup();
    anyhow::ensure!(
        crt.len() == 4,
        "Engine has no complete recognized allocator contract"
    );
    Ok(crt)
}

fn verify_deallocator(
    memory: &ProcessMemory,
    studio: &ModuleEntry,
    image: &PeImage<'_>,
    free_slot: usize,
) -> Result<Vec<usize>> {
    let (_, trace, _) = studio_layout(&studio.path)?;
    let mut queue = std::collections::VecDeque::from([(trace.deallocator, 0)]);
    let mut seen = HashSet::new();
    while let Some((rva, depth)) = queue.pop_front() {
        if !seen.insert(rva) {
            continue;
        }
        anyhow::ensure!(
            seen.len() <= 128,
            "Engine deallocator call graph exceeds the probe budget"
        );
        let offset = image.rva_to_offset(rva)?;
        let boundary = image.function_end(offset);
        let bounded = boundary.is_ok();
        let end = boundary.unwrap_or((offset + 32).min(image.bytes.len()));
        let bytes = &image.bytes[offset..end];
        anyhow::ensure!(
            memory.read_vec(studio.base + rva, bytes.len())? == bytes,
            "Engine deallocator code changed"
        );
        for i in iced_x86::Decoder::with_ip(64, bytes, rva as u64, iced_x86::DecoderOptions::NONE) {
            anyhow::ensure!(
                !i.is_invalid(),
                "Engine deallocator contains an invalid instruction"
            );
            if !bounded && i.mnemonic() == iced_x86::Mnemonic::Ret {
                break;
            }
            if !matches!(
                i.mnemonic(),
                iced_x86::Mnemonic::Call | iced_x86::Mnemonic::Jmp
            ) {
                continue;
            }
            if i.is_ip_rel_memory_operand() && i.ip_rel_memory_address() as usize == free_slot {
                let mut evidence = seen.into_iter().collect::<Vec<_>>();
                evidence.sort_unstable();
                return Ok(evidence);
            }
            if depth < 3 && i.op0_kind() == iced_x86::OpKind::NearBranch64 {
                let target = i.near_branch_target() as usize;
                if image.require_executable_rva(target).is_ok() {
                    queue.push_back((target, depth + 1));
                }
            }
            if !bounded && i.mnemonic() == iced_x86::Mnemonic::Jmp {
                break;
            }
        }
    }
    bail!("Engine deallocator does not reach the selected allocator's imported free")
}

fn allocator_probe(
    memory: &ProcessMemory,
    studio: &ModuleEntry,
    loaded: &[ModuleEntry],
) -> Result<Value> {
    let mapped = observation::ImageView::open(&studio.path)?;
    let image = PeImage::parse(mapped.bytes())?;
    let imports = allocator_imports(&image)?;
    let mimalloc = imports.iter().any(|(name, _)| name == "mi_free");
    let module_name = if mimalloc {
        "mimalloc.dll"
    } else {
        "ucrtbase.dll"
    };
    let free_name = if mimalloc { "mi_free" } else { "free" };
    let free_slot = imports
        .iter()
        .find(|(name, _)| name == free_name)
        .context("Engine free import missing")?
        .1;
    let deallocator = verify_deallocator(memory, studio, &image, free_slot)?;
    let module = loaded
        .iter()
        .find(|m| m.name.eq_ignore_ascii_case(module_name))
        .context("Engine allocator module is not loaded")?;
    let allocator = observation::ImageView::open(&module.path)?;
    let allocator = PeImage::parse(allocator.bytes())?;
    verify_loaded_image(memory, module, allocator.image_stamp)?;
    let mut bindings = Vec::new();
    for (name, slot) in imports {
        let rva = exported_rva(&allocator, &name)?;
        let address = memory.read_u64(studio.base + slot)? as usize;
        anyhow::ensure!(
            address == module.base + rva,
            "Engine allocator import {name} differs from its export"
        );
        let offset = allocator.rva_to_offset(rva)?;
        anyhow::ensure!(
            memory.read_vec(address, 32)? == slice(allocator.bytes, offset, 32)?,
            "Engine allocator {name} code was changed"
        );
        bindings.push(json!({"name":name,"iatRva":slot,"address":address}));
    }
    Ok(
        json!({"kind":if mimalloc {"mimalloc"} else {"process CRT"},"module":module.name,"base":module.base,"bindings":bindings,"deallocatorCallGraphRvas":deallocator}),
    )
}
