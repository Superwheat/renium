// Included only by windows_tests.rs. This does not install or call a production API.
use super::*;
use properties::typed_audit::{FIELD_NAMES, Kind, SPEC_SIZE, discover};
use sha2::{Digest, Sha256};
use std::io::Write;

const PINNED_IMAGE: &str = "9004f0bd48a99c09ad1932cd46e21405fa35972bb5ae3a44dcb8db67c03add74";
const TYPED_SIZE: usize = 7928;

fn validate_transport(bytes: &[u8]) -> Result<(usize, usize)> {
    anyhow::ensure!(bytes.len() >= 320, "Truncated typed response");
    anyhow::ensure!(
        read_u32(bytes, 0)? == 0x50414352 && read_u32(bytes, 4)? == 1,
        "Wrong typed response format"
    );
    let error = String::from_utf8_lossy(&bytes[64..320]);
    anyhow::ensure!(
        read_u32(bytes, 8)? == 4 && read_u32(bytes, 12)? == 0,
        "Typed capture failed: {}",
        error.trim_end_matches('\0')
    );
    let values = usize::try_from(read_u64(bytes, 16)?)?;
    let identities = usize::try_from(read_u64(bytes, 24)?)?;
    anyhow::ensure!(
        values > 0
            && values % 44 == 0
            && identities > 0
            && identities % 72 == 0
            && values / 44 <= identities / 72
            && identities / 72 <= 2_000_000
            && values
                .checked_add(identities)
                .and_then(|n| n.checked_add(320))
                == Some(bytes.len()),
        "Invalid typed response lengths"
    );
    let mut ids = HashSet::new();
    let mut debug_ids = HashSet::new();
    for (index, row) in bytes[320 + values..].chunks_exact(72).enumerate() {
        let id: [u8; 16] = row[..16].try_into()?;
        let text = &row[16..64];
        let end = text
            .iter()
            .position(|byte| *byte == 0)
            .context("Unterminated DebugId")?;
        anyhow::ensure!(
            id != [0; 16]
                && ids.insert(id)
                && end > 0
                && text[end..].iter().all(|byte| *byte == 0)
                && std::str::from_utf8(&text[..end]).is_ok()
                && debug_ids.insert(text[..end].to_vec()),
            "Invalid/duplicate native identity"
        );
        let parent = read_u32(row, 64)?;
        anyhow::ensure!(
            read_u32(row, 68)? == 0
                && if index == 0 {
                    parent == u32::MAX
                } else {
                    (parent as usize) < index
                },
            "Invalid native parent row"
        );
    }
    let mut previous = None;
    for row in bytes[320..320 + values].chunks_exact(44) {
        let index = read_u32(row, 0)? as usize;
        anyhow::ensure!(
            index < identities / 72 && previous.is_none_or(|p| p < index),
            "Invalid/duplicate typed identity index"
        );
        previous = Some(index);
        for field in 0..8 {
            anyhow::ensure!(read_u32(row, 4 + field * 4)? <= 1, "Noncanonical Boolean");
        }
    }
    Ok((values / 44, identities / 72))
}

fn audit_export(bytes: &[u8], symbol: &[u8]) -> Result<usize> {
    let image = PeImage::parse(bytes)?;
    let header = read_u32(bytes, 0x3c)? as usize + 24;
    let exports = image.rva_to_offset(read_u32(bytes, header + 112)? as usize)?;
    let names = image.rva_to_offset(read_u32(bytes, exports + 32)? as usize)?;
    let ordinals = image.rva_to_offset(read_u32(bytes, exports + 36)? as usize)?;
    let functions = image.rva_to_offset(read_u32(bytes, exports + 28)? as usize)?;
    let mut found = Vec::new();
    for index in 0..read_u32(bytes, exports + 24)? as usize {
        let name = image.rva_to_offset(read_u32(bytes, names + index * 4)? as usize)?;
        if bytes
            .get(name..)
            .is_some_and(|name| name.starts_with(symbol))
        {
            let ordinal = read_u16(bytes, ordinals + index * 2)? as usize;
            found.push(read_u32(bytes, functions + ordinal * 4)? as usize);
        }
    }
    anyhow::ensure!(found.len() == 1, "Missing/ambiguous typed audit export");
    Ok(found[0])
}

#[test]
#[ignore = "Explicitly coordinated owned Edit fixture only; loads audit DLL and reads engine getters"]
fn native_typed_live_capture() -> Result<()> {
    let started = Instant::now();
    let timeout = Duration::from_secs(18);
    let pid: u32 = std::env::var("RENIUM_TYPED_PID")?.parse()?;
    let title = std::env::var("RENIUM_TYPED_TITLE")?;
    let private = std::env::var("RENIUM_TYPED_PRIVATE").ok();
    anyhow::ensure!(private.as_deref().is_none_or(|mode| matches!(mode, "proof" | "timeout")), "Invalid private audit mode");
    anyhow::ensure!(
        title.starts_with("ReniumNativeTyped")
            && title.ends_with(".rbxl")
            && !title.contains(['/', '\\']),
        "Not an owned typed audit title"
    );
    let project = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()?;
    let output = project.join(format!(
        "audit/native-typed-{}{}.{}.bin",
        if private.is_some() { "private-" } else { "" },
        title.trim_end_matches(".rbxl"),
        pid
    ));
    anyhow::ensure!(
        !output.exists(),
        "Do not overwrite a previous typed capture"
    );
    let memory = ProcessMemory::open(pid)?;
    let current_modules = modules(pid)?;
    let studio = current_modules.first().context("Studio module missing")?;
    anyhow::ensure!(
        studio.name.eq_ignore_ascii_case("RobloxStudioBeta.exe")
            && format!("{:x}", Sha256::digest(fs::read(&studio.path)?)) == PINNED_IMAGE,
        "Typed audit accepts only the inspected 0.737.0.7371584 image"
    );
    let window = capture_window(pid, &title)?;
    let (data, trace, stamp) = studio_layout(&studio.path)?;
    verify_loaded_image(&memory, studio, stamp)?;
    let mut model = active_data_model(pid, &memory, studio, data, &title)?;
    let layout = package_layout(&studio.path)?;
    let task_context = data_model_task_context(&memory, studio, &layout, &model)?;
    for rva in [trace.deallocator, layout.submit_task] {
        properties::verified_code(&memory, studio, &layout, studio.base + rva, 64)?;
    }
    let model_instance = model.outer + model.layout.data_model_instance;
    let (identity_binding, identity_getter) =
        properties::identity_binding(&memory, studio, &layout, &model, model_instance)?;
    let debug_id = properties::debug_id_function(&memory, studio, &layout, &model, model_instance)?;
    let parent_offset = properties::parent_offset(&memory, &model)?;
    let roots = model
        .roots
        .iter()
        .map(|entry| {
            Ok((
                *entry,
                read_instance_class(&memory, entry.instance, model.layout)
                    .context("Missing service class")?,
            ))
        })
        .collect::<Result<Vec<_>>>()?;
    let serialization_service = if private.is_some() {
        Some(roots.iter().find(|(_, name)| name == "SerializationService")
            .context("Create SerializationService through the owned fixture harness before arming")?.0.instance)
    } else { None };
    model.roots = select_capture_roots(&roots, &["ServerStorage".into()])?;
    let mut samples = [None, None];
    let mut pending = model.roots.clone();
    let mut visited = HashSet::new();
    while let Some(entry) = pending.pop() {
        capture_remaining_ms(started, timeout)?;
        anyhow::ensure!(
            visited.len() < 100_000 && visited.insert(entry.instance),
            "Invalid/oversized sample search"
        );
        match read_instance_class(&memory, entry.instance, model.layout).as_deref() {
            Some("Part") => {
                samples[0].get_or_insert(entry.instance);
            }
            Some("MeshPart") => {
                samples[1].get_or_insert(entry.instance);
            }
            _ => {}
        }
        if samples.iter().all(Option::is_some) {
            break;
        }
        let children = read_children(&memory, entry.instance, model.layout)
            .context("Invalid sample children")?;
        pending.extend(children.into_iter().rev());
    }
    let [part, mesh] = samples.map(|sample| sample.context("Need both Part and MeshPart samples"));
    let (part, mesh) = (part?, mesh?);
    let mut specs = Vec::new();
    for (index, name) in FIELD_NAMES.iter().enumerate() {
        let kind = if index < 8 {
            Kind::Boolean
        } else {
            Kind::Float32
        };
        let spec = discover(&memory, studio, &layout, &model, part, name, kind)?;
        anyhow::ensure!(
            spec == discover(&memory, studio, &layout, &model, mesh, name, kind)?,
            "Part/MeshPart do not share the audited {name} binding"
        );
        specs.extend(spec);
    }
    let mut nonce = [0; 16];
    getrandom::fill(&mut nonce).map_err(|e| anyhow::anyhow!("Nonce: {e}"))?;
    let path = project.join(format!(
        "audit/native-typed-transport-{:032x}.tmp",
        u128::from_le_bytes(nonce)
    ));
    let mut transport = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .share_mode(7)
        .custom_flags(0x04000000)
        .open(&path)?;
    let mut params = build_parameters(studio.base, trace, &model, &path, true)?;
    params.resize(TYPED_SIZE, 0);
    put_u32(&mut params, PARAM_REQUESTED_MXCSR, 0);
    for (offset, value) in [
        (PARAM_TASK_CONTEXT, task_context),
        (PARAM_SUBMIT_TASK, studio.base + layout.submit_task),
        (PARAM_IDENTITY_BINDING, identity_binding),
        (PARAM_IDENTITY_GETTER, identity_getter),
        (PARAM_DEBUG_ID_GETTER, debug_id),
        (PARAM_WINDOW, window.0),
        (
            5912,
            memory.read_u64(part + model.layout.class_descriptor)? as usize,
        ),
        (
            5920,
            memory.read_u64(mesh + model.layout.class_descriptor)? as usize,
        ),
    ] {
        put_u64(&mut params, offset, value);
    }
    put_u32(&mut params, PARAM_CAPTURE_MODE, 1);
    put_u32(
        &mut params,
        PARAM_PARENT_OFFSET,
        u32::try_from(parent_offset)?,
    );
    put_u32(&mut params, PARAM_PROCESS_ID, pid);
    put_u32(
        &mut params,
        5904,
        u32::try_from(model.layout.class_descriptor)?,
    );
    put_u32(&mut params, 5908, FIELD_NAMES.len() as u32);
    assert_eq!(specs.len(), FIELD_NAMES.len() * SPEC_SIZE);
    params[5928..].copy_from_slice(&specs);
    let mut private_nonce = String::new();
    let mut private_method = None;
    if let Some(service) = serialization_service {
        let descriptor = find_class_member_descriptor(&memory, service, model.layout, "SerializeInstancesAsync")?;
        let contract: serde_json::Value = serde_json::from_slice(&fs::read(project.join("audit/native-typed-private-contract.json"))?)?;
        anyhow::ensure!(descriptor == studio.base + contract["descriptor"].as_u64().context("Missing descriptor contract")? as usize,
            "Private method descriptor differs from current inspected registration");
        for check in contract["checks"].as_array().context("Missing private code checks")? {
            properties::verified_code(&memory, studio, &layout,
                studio.base + check["rva"].as_u64().context("Missing code RVA")? as usize,
                check["size"].as_u64().context("Missing code size")? as usize)?;
        }
        let method = studio.base + contract["checks"][0]["rva"].as_u64().context("Missing method contract")? as usize;
        anyhow::ensure!(memory.read_u64(descriptor + 0x78)? as usize == method
            && read_u32(&memory.read_vec(descriptor + 0x80, 4)?, 0)? == 0,
            "Private method/padding-safe adjustment mismatch");
        private_method = Some((descriptor, method));
        let name = memory.read_u64(service + model.layout.name)? as usize;
        let text_offset = [0, 8].into_iter().find(|offset| read_msvc_string(&memory, name + offset).as_deref() == Some("SerializationService"))
            .context("Unsupported Instance name layout")?;
        let mut secret = [0u8; 32];
        getrandom::fill(&mut secret).map_err(|e| anyhow::anyhow!("Private nonce: {e}"))?;
        private_nonce = format!("renium-private-typed:{}", secret.iter().map(|byte| format!("{byte:02x}")).collect::<String>());
        params.resize(8048, 0);
        put_u64(&mut params, 7928, descriptor);
        put_u64(&mut params, 7936, service);
        put_u32(&mut params, 7944, u32::try_from(model.layout.name)?);
        put_u32(&mut params, 7948, text_offset as u32);
        params[7952..7952 + private_nonce.len()].copy_from_slice(private_nonce.as_bytes());
    }
    // Only now load the audit DLL: all typed ABIs have already been validated.
    let helper_path = project.join("audit/native-typed-probe.dll");
    let entry_rva = audit_export(&fs::read(&helper_path)?, if private.is_some() { b"ReniumTypedPrivate\0" } else { b"ReniumTypedCapture\0" })?;
    let kernel = current_modules
        .iter()
        .find(|m| m.name.eq_ignore_ascii_case("kernel32.dll"))
        .context("Missing kernel32")?;
    let local = unsafe { GetModuleHandleW(wide("kernel32.dll").as_ptr()) };
    anyhow::ensure!(!local.is_null(), "Missing local kernel32");
    let load = unsafe { GetProcAddress(local, c"LoadLibraryW".as_ptr().cast()) }
        .context("Missing LoadLibraryW")?;
    let load = kernel.base
        + (load as usize)
            .checked_sub(local as usize)
            .context("Loader outside kernel32")?;
    let path_bytes = wide(helper_path.as_os_str())
        .iter()
        .flat_map(|v| v.to_le_bytes())
        .collect::<Vec<_>>();
    let mut remote_path = memory.allocate(path_bytes.len())?;
    memory.write(remote_path.address, &path_bytes)?;
    remote_path.run(load, capture_remaining_ms(started, timeout)?.min(2000))?;
    let helper = modules(pid)?
        .into_iter()
        .find(|m| module_path_matches(m, &helper_path))
        .context("Typed audit DLL was not loaded")?;
    anyhow::ensure!(
        capture_window(pid, &title)? == window,
        "Studio window changed during discovery"
    );
    let discovery_ms = started.elapsed().as_secs_f64() * 1000.0;
    put_u32(
        &mut params,
        PARAM_TIMEOUT,
        capture_remaining_ms(started, timeout)?.min(15000),
    );
    let remote = memory.allocate(params.len())?;
    memory.write(remote.address, &params)?;
    if private.is_some() {
        let ready = output.with_extension("ready.json");
        fs::OpenOptions::new().write(true).create_new(true).open(ready)?.write_all(
            &serde_json::to_vec(&serde_json::json!({"pid":pid,"title":title,"nonce":private_nonce,"transport":path,"output":output,"mode":private}))?)?;
    }
    let native_started = Instant::now();
    let exit = remote.run_owned(
        helper.base + entry_rva,
        capture_remaining_ms(started, timeout)?,
    )?;
    let native_ms = native_started.elapsed().as_secs_f64() * 1000.0;
    let read_started = Instant::now();
    anyhow::ensure!(
        transport.metadata()?.len() <= 232_000_320,
        "Oversized typed transport"
    );
    let mut bytes = Vec::new();
    transport.read_to_end(&mut bytes)?;
    if let Some((descriptor, method)) = private_method {
        anyhow::ensure!(bytes.len() == 320 && read_u32(&bytes, 0)? == 0x50414352,
            "Invalid private audit completion header");
        let expected = if private.as_deref() == Some("timeout") { 6 } else { 4 };
        anyhow::ensure!(read_u32(&bytes, 8)? == expected && exit == if expected == 4 { 0 } else { 6 },
            "Private native completion failed: {}", String::from_utf8_lossy(&bytes));
        anyhow::ensure!(memory.read_u64(descriptor + 0x78)? as usize == method && capture_window(pid, &title)? == window,
            "Private method/window not restored");
        fs::OpenOptions::new().write(true).create_new(true).open(&output)?.write_all(&bytes)?;
        fs::write(output.with_extension("json"), serde_json::to_vec_pretty(&serde_json::json!({
            "status":expected,"methodRestored":true,"discoveryMs":discovery_ms,"waitMs":native_ms,"image":PINNED_IMAGE}))?)?;
        return Ok(());
    }
    let (rows, identities) = validate_transport(&bytes)?;
    anyhow::ensure!(
        exit == 0 && capture_window(pid, &title)? == window,
        "Typed exit/window mismatch"
    );
    capture_remaining_ms(started, timeout)?;
    let total_ms = started.elapsed().as_secs_f64() * 1000.0;
    let metadata = serde_json::json!({"image":PINNED_IMAGE, "fields":FIELD_NAMES,
        "rows":rows,"identities":identities,"discoveryMs":discovery_ms,"nativeMs":native_ms,
        "readValidateMs":read_started.elapsed().as_secs_f64()*1000.0,"totalMs":total_ms,
        "queueUs":read_u64(&bytes,32)?,"gettersUs":read_u64(&bytes,40)?,
        "identitiesUs":read_u64(&bytes,48)?,"writeUs":read_u64(&bytes,56)?,
        "canonicalEquivalence":"NOT YET CHECKED", "output":output});
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&output)?
        .write_all(&bytes)?;
    fs::write(
        output.with_extension("json"),
        serde_json::to_vec_pretty(&metadata)?,
    )?;
    eprintln!("{metadata}");
    Ok(())
}

#[test]
fn native_typed_transport_rejects_unproven_outputs() {
    assert!(validate_transport(&[]).is_err());
    let mut bytes = vec![0; 320 + 44 + 72];
    for (offset, value) in [(0, 0x50414352), (4, 1), (8, 4)] {
        put_u32(&mut bytes, offset, value);
    }
    put_u64(&mut bytes, 16, 44);
    put_u64(&mut bytes, 24, 72);
    bytes[364] = 1;
    bytes[380] = b'1';
    put_u32(&mut bytes, 428, u32::MAX);
    assert_eq!(validate_transport(&bytes).unwrap(), (1, 1));
    for (offset, value) in [(8, 6), (12, 1), (320, 1), (324, 2), (428, 0), (432, 1)] {
        let mut bad = bytes.clone();
        put_u32(&mut bad, offset, value);
        assert!(validate_transport(&bad).is_err(), "offset {offset}");
    }
    bytes.push(0);
    assert!(validate_transport(&bytes).is_err());
}
