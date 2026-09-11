use super::*;
use anyhow::ensure;
use std::io::Write;

mod native_retained_audit {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/studio/native/serializer/fixtures/native-loader-retained-host.rs"
    ));
}

mod native_history_audit {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/studio/native/serializer/fixtures/native-history-host.rs"
    ));
}

mod native_typed_audit {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/studio/native/serializer/fixtures/native-typed-host.rs"
    ));
}

#[test]
fn snapshot_parameters_use_discovered_layout_and_captured_roots() -> Result<()> {
    let trace = SerializerTrace {
        serializer: 1,
        context_builder: 2,
        context_destroy: 3,
        root_collector: 4,
        deallocator: 5,
    };
    for offset in [0, 0x1c8, 0x1f0, 0x298] {
        let model = ActiveDataModel {
            outer: 0x10000,
            owner: 0x20000,
            roots: vec![SharedEntry {
                instance: 0x30000,
                owner: 0x40000,
            }],
            layout: InstanceLayout {
                data_model_instance: offset,
                self_pointer: 8,
                class_descriptor: 24,
                children: 0x98,
                name: 0x50,
            },
        };
        for place in [false, true] {
            let params =
                build_parameters(0x100000, trace, &model, Path::new("snapshot.rbxl"), place)?;
            assert_eq!(
                read_u32(&params, PARAM_DATA_MODEL_INSTANCE_OFFSET)?,
                offset as u32
            );
            assert_eq!(read_u64(&params, 48)?, 0x10000);
            assert_eq!(read_u64(&params, PARAM_ROOTS)?, 0x30000);
            assert_eq!(read_u64(&params, PARAM_ROOTS + 8)?, 0x40000);
            assert_eq!(read_u32(&params, 64)?, 1);
            assert_eq!(read_u32(&params, PARAM_TIMEOUT)?, 15_000);
            assert_eq!(read_u32(&params, PARAM_CHILDREN_OFFSET)?, 0x98);
            assert_eq!(read_u32(&params, PARAM_SELF_OFFSET)?, 8);
            assert_eq!(params.len(), PARAM_SIZE);
        }
    }
    Ok(())
}

#[test]
#[ignore = "One-build ABI feasibility probe; requires an explicitly owned empty Studio fixture and audit DLL"]
#[expect(
    clippy::cognitive_complexity,
    reason = "Opt-in pinned-ABI diagnostic keeps its independent probe modes and safety checks together"
)]
fn native_loader_disposable_probe() -> Result<()> {
    use sha2::{Digest, Sha256};
    let started = Instant::now();
    let pid: u32 = std::env::var("RENIUM_LOADER_PROBE_PID")?.parse()?;
    let title = std::env::var("RENIUM_LOADER_PROBE_TITLE")?;
    anyhow::ensure!(
        title.starts_with("ReniumNativeLoaderProbe")
            && title.ends_with(".rbxl")
            && !title.contains(['/', '\\']),
        "Not an owned probe title"
    );
    let project = Path::new("../..").canonicalize()?;
    let retained_reader = std::env::var_os("RENIUM_LOADER_RETAINED").is_some();
    let fuller_source =
        std::env::var("RENIUM_LOADER_PROBE_SOURCE").as_deref() == Ok("loaded-source");
    let (prefix, mut expected_hash) = if fuller_source {
        (
            "NativeLoader3",
            "654534e20bef464dcfd41b3deced246dd0bb4736975f38a3658a1d110971f671",
        )
    } else {
        (
            "NativeLoader2",
            "3dc14a89557faf498fc50ecf8730cee92a1f7629b9c6042588550a8d817f972f",
        )
    };
    let package_free = std::env::var_os("RENIUM_LOADER_PROBE_PACKAGE_FREE").is_some();
    anyhow::ensure!(
        !package_free || fuller_source,
        "Package-free probe requires loaded-source baseline"
    );
    let wrapped = std::env::var_os("RENIUM_LOADER_PROBE_PRIVATE_WRAPPED").is_some();
    anyhow::ensure!(
        !wrapped || package_free,
        "Wrapped probe requires package-free input"
    );
    let input_prefix = if wrapped {
        expected_hash = "a699068374b48e5df3825b2f244b37f0183394b4694e040ca0d3a362c9dac534";
        "NativeLoader3PrivateWrapped"
    } else if package_free {
        expected_hash = "74f6b6c2e162e4e5fabee26cb8f7fe95f6d00e6a9813550eb4c29721413237da";
        "NativeLoader3PackageFree"
    } else {
        prefix
    };
    let mut fixture = project.join(format!(
        "audit/testplace-spans-20260908/3/{input_prefix}Input.rbxl"
    ));
    if retained_reader {
        ensure!(
            !package_free && !wrapped,
            "Retained reader uses its exact full input"
        );
        fixture = project.join("audit/direct-readback737/Renium737ReadbackSource.rbxl");
        expected_hash = "e36103db2d5a65015476bb92ca25b1015e65d917cb673291f3b901bbce7abeee";
    }
    let fixture_bytes = fs::read(&fixture)?;
    anyhow::ensure!(
        format!("{:x}", Sha256::digest(&fixture_bytes)) == expected_hash,
        "Probe accepts only the exact captured test fixture"
    );
    let retained_input = if retained_reader {
        let input = native_retained_audit::prepare_input(&project, &title, &fixture_bytes)?;
        fixture = input.path.clone();
        Some(input)
    } else {
        None
    };
    let current_modules = modules(pid)?;
    let studio = current_modules.first().context("Studio module missing")?;
    anyhow::ensure!(
        studio.name.eq_ignore_ascii_case("RobloxStudioBeta.exe"),
        "Not Studio"
    );
    let exe = fs::read(&studio.path)?;
    anyhow::ensure!(
        format!("{:x}", Sha256::digest(&exe))
            == "9004f0bd48a99c09ad1932cd46e21405fa35972bb5ae3a44dcb8db67c03add74",
        "Diagnostic ABI is pinned to the inspected Studio image; no guessed addresses"
    );
    let image = PeImage::parse(&exe)?;
    let discovery = Instant::now();
    let loader_trace = loader::prepare(&studio.path)?;
    let loader = loader_trace.loader;
    println!(
        "Dynamic loader discovery: {:.3}ms",
        discovery.elapsed().as_secs_f64() * 1000.
    );
    anyhow::ensure!(
        image.rva_to_offset(loader)? == 0x32c4ca0,
        "Loader call chain changed"
    );
    let layout = package_layout(&studio.path)?;
    let memory = ProcessMemory::open(pid)?;
    verify_loaded_image(&memory, studio, layout.image_stamp)?;
    let model = active_data_model(pid, &memory, studio, layout.data, &title)?;
    println!(
        "Discovered DataModel instance offset: {:#x}",
        model.layout.data_model_instance
    );
    let instance = model.outer + model.layout.data_model_instance;
    anyhow::ensure!(
        loader_trace.instance_offset == model.layout.data_model_instance,
        "Native loader and live DataModel disagree on the Instance layout"
    );
    anyhow::ensure!(
        read_instance_class(&memory, instance, model.layout).as_deref() == Some("DataModel"),
        "Target is not DataModel"
    );
    if std::env::var_os("RENIUM_NATIVE_CAPTURE_STRESS").is_some() {
        let services = ["ServerStorage".to_owned()];
        let before = capture_live_services(pid, &title, &services, Duration::from_secs(15))?;
        anyhow::ensure!(
            before.identities.len() > 50_000 * CAPTURE_ROW_SIZE,
            "Capture cancellation probe needs the populated owned fixture"
        );
        for cycle in 0..3 {
            let timed = Instant::now();
            let error =
                match capture_live_services(pid, &title, &services, Duration::from_millis(150)) {
                    Ok(_) => bail!(
                        "Short capture unexpectedly completed; cancellation was not exercised"
                    ),
                    Err(error) => format!("{error:#}"),
                };
            println!(
                "Capture cancellation {cycle}: {}ms, {error}",
                timed.elapsed().as_millis()
            );
            anyhow::ensure!(
                error.contains("Studio helper exceeded its")
                    || error.contains("Native capture expired after its helper returned"),
                "Timeout did not exercise an already-dispatched capture task: {error}"
            );
            let after = capture_live_services(pid, &title, &services, Duration::from_secs(15))?;
            assert_eq!(
                before.bytes, after.bytes,
                "Capture changed the serialized state"
            );
            assert_eq!(
                before.identities, after.identities,
                "Capture changed identities or parents"
            );
            let prefix = format!("capture-{pid}-");
            for entry in fs::read_dir(std::env::temp_dir().join("renium-native"))? {
                let entry = entry?;
                anyhow::ensure!(
                    !entry.file_name().to_string_lossy().starts_with(&prefix),
                    "Completed or cancelled capture retained its transport"
                );
            }
        }
        println!(
            "Three dispatched capture cancellations recovered with exact bytes and identities"
        );
        return Ok(());
    }
    if std::env::var_os("RENIUM_LOADER_PROBE_PRODUCTION_EXPORT").is_some() {
        let output = project.join(format!(
            "audit/testplace-spans-20260908/3/NativeProductionQueued-{title}"
        ));
        let snapshot = write_live_place(pid, &title, &output)?;
        println!(
            "{}",
            serde_json::json!({"instances":snapshot.instance_count,"bytes":snapshot.output_size,"totalMs":snapshot.elapsed_ms,"serializeMs":snapshot.serialize_ms,"invokeMs":snapshot.invoke_ms,"traceMs":snapshot.trace_ms,"discoverMs":snapshot.discover_ms,"helperMs":snapshot.helper_ms,"validateMs":snapshot.validate_ms,"writeMs":snapshot.write_ms})
        );
        return Ok(());
    }
    let task_context = data_model_task_context(&memory, studio, &layout, &model)?;
    let export_flags = std::env::var("RENIUM_LOADER_PROBE_EXPORT")
        .ok()
        .map(|v| v.parse::<u32>())
        .transpose()?;
    anyhow::ensure!(
        export_flags.is_none_or(|v| matches!(v, 0 | 64 | 66 | 72)),
        "Untraced serializer mode"
    );
    if std::env::var_os("RENIUM_LOADER_PROBE_INSPECT").is_some() {
        let mut descriptors = Vec::new();
        for (service, property) in [
            ("Lighting", "Brightness"),
            ("Lighting", "Outlines"),
            ("Lighting", "Technology"),
            ("Workspace", "StreamingEnabled"),
            ("Workspace", "SignalBehavior"),
            ("Workspace", "SignalBehavior2"),
            ("Workspace", "SignalBehaviorAlias"),
            ("Workspace", "WorldPivot"),
            ("Workspace", "WorldPivotData"),
        ] {
            let root = model
                .roots
                .iter()
                .find(|r| {
                    read_instance_class(&memory, r.instance, model.layout).as_deref()
                        == Some(service)
                })
                .context("Missing inspected service")?;
            let descriptor = match find_class_member_descriptor(
                &memory,
                root.instance,
                model.layout,
                property,
            ) {
                Ok(value) => value,
                Err(error) => {
                    descriptors.push(serde_json::json!({"service":service,"property":property,"error":error.to_string()}));
                    continue;
                }
            };
            let vtable = memory.read_u64(descriptor)? as usize;
            let value = read_property(
                pid,
                &title,
                &[service.into()],
                &[],
                service,
                property,
                Duration::from_secs(2),
            );
            println!("{service}.{property}: {value:?}");
            let mut functions = Vec::new();
            for slot in 0..36 {
                let address = memory.read_u64(vtable + slot * 8)? as usize;
                if !(studio.base..studio.base + studio.size).contains(&address) {
                    continue;
                }
                let rva = address - studio.base;
                let Ok(offset) = image.rva_to_offset(rva) else {
                    continue;
                };
                functions.push(serde_json::json!({"slot":slot,"fileOffset":format!("{offset:x}"),"code":memory.read_vec(address,48)?.iter().map(|b|format!("{b:02x}")).collect::<String>()}));
            }
            descriptors.push(serde_json::json!({"service":service,"property":property,"kind":read_rtti_type(&memory,descriptor,studio.base,studio.size),"bytes":memory.read_vec(descriptor,0x120)?.iter().map(|b|format!("{b:02x}")).collect::<String>(),"functions":functions}));
        }
        let output = project.join("audit/native-loader-descriptors.json");
        fs::write(&output, serde_json::to_vec_pretty(&descriptors)?)?;
        println!("Read-only descriptor evidence: {}", output.display());
        return Ok(());
    }
    if export_flags.is_none() && !retained_reader {
        for name in [
            "Workspace",
            "ReplicatedStorage",
            "ServerStorage",
            "ServerScriptService",
            "StarterGui",
        ] {
            if let Some(root) = model.roots.iter().find(|root| {
                read_instance_class(&memory, root.instance, model.layout).as_deref() == Some(name)
            }) {
                for child in read_children(&memory, root.instance, model.layout)
                    .context("Cannot inspect blank target")?
                {
                    let class = read_instance_class(&memory, child.instance, model.layout);
                    anyhow::ensure!(
                        name == "Workspace"
                            && matches!(class.as_deref(), Some("Camera" | "Terrain")),
                        "Refusing nonempty target {name}: {class:?}"
                    );
                }
            }
        }
    }
    let loader_offset = image.rva_to_offset(loader)?;
    let loaded_code = memory.read_vec(studio.base + loader, 64)?;
    anyhow::ensure!(
        loaded_code == exe[loader_offset..loader_offset + 64],
        "Loaded loader differs from inspected file"
    );
    let receipts = std::env::var_os("RENIUM_LOADER_PROBE_RECEIPTS").is_some();
    let private = std::env::var_os("RENIUM_LOADER_PROBE_PRIVATE").is_some();
    ensure!(
        !retained_reader || (!receipts && !private && export_flags.is_none()),
        "Retained reader is an isolated full load"
    );
    let handoff = std::env::var_os("RENIUM_LOADER_PROBE_HANDOFF").is_some();
    let handoff_status = std::env::var("RENIUM_LOADER_PROBE_HANDOFF_STATUS")
        .ok()
        .map(|value| value.parse::<u64>())
        .transpose()?;
    anyhow::ensure!(
        handoff_status.is_none_or(|status| handoff && matches!(status, 4 | 6)),
        "Expected completed or expired handoff status"
    );
    let paste_mode = std::env::var("RENIUM_LOADER_PROBE_PASTE")
        .ok()
        .map(|value| value.parse::<u32>())
        .transpose()?;
    anyhow::ensure!(
        paste_mode.is_none_or(|mode| matches!(mode, 1 | 2) && wrapped && private),
        "Paste A/B requires the exact private wrapped fixture"
    );
    anyhow::ensure!(!wrapped || private, "Wrapped probe is private only");
    anyhow::ensure!(
        !handoff || (wrapped && private && paste_mode.is_none()),
        "Callback proof requires only the private wrapped fixture"
    );
    anyhow::ensure!(
        !private || receipts,
        "Private probe requires receipt ownership"
    );
    anyhow::ensure!(
        !receipts || export_flags.is_none(),
        "Receipt probe cannot export"
    );
    let capture_identities = std::env::var_os("RENIUM_SERIALIZER_IDENTITY_PROBE").is_some();
    anyhow::ensure!(
        !capture_identities || matches!(export_flags, Some(64 | 66 | 72)),
        "Identity capture requires place-mode serialization"
    );
    let helper_path = project.join(if retained_reader {
        "audit/native-loader-retained.dll"
    } else if capture_identities {
        "audit/native-serializer-identities.dll"
    } else if handoff {
        "audit/native-loader-probe-handoff.dll"
    } else if paste_mode.is_some() {
        "audit/native-loader-probe-paste.dll"
    } else if private {
        "audit/native-loader-probe-private-capture.dll"
    } else if receipts {
        "audit/native-loader-probe-receipt-events.dll"
    } else {
        "audit/native-loader-probe-filtered.dll"
    });
    let helper_bytes = fs::read(&helper_path)?;
    let helper_image = PeImage::parse(&helper_bytes)?;
    let header = read_u32(&helper_bytes, 0x3c)? as usize + 24;
    let exports = helper_image.rva_to_offset(read_u32(&helper_bytes, header + 112)? as usize)?;
    let names = helper_image.rva_to_offset(read_u32(&helper_bytes, exports + 32)? as usize)?;
    let ordinals = helper_image.rva_to_offset(read_u32(&helper_bytes, exports + 36)? as usize)?;
    let functions = helper_image.rva_to_offset(read_u32(&helper_bytes, exports + 28)? as usize)?;
    let mut entry = None;
    for index in 0..read_u32(&helper_bytes, exports + 24)? as usize {
        let name =
            helper_image.rva_to_offset(read_u32(&helper_bytes, names + index * 4)? as usize)?;
        let symbol: &[u8] = if handoff_status.is_some() {
            b"ReniumLoaderProbeHandoffStatus\0"
        } else if export_flags.is_some() {
            b"ReniumSerializerProbe\0"
        } else {
            b"ReniumLoaderProbeBatch\0"
        };
        if helper_bytes[name..].starts_with(symbol) {
            let ordinal = read_u16(&helper_bytes, ordinals + index * 2)? as usize;
            entry = Some(read_u32(&helper_bytes, functions + ordinal * 4)? as usize);
        }
    }
    let entry = entry.context("Missing diagnostic export")?;
    let kernel = current_modules
        .iter()
        .find(|m| m.name.eq_ignore_ascii_case("kernel32.dll"))
        .context("Missing kernel32")?;
    let local_kernel = unsafe { GetModuleHandleW(wide("kernel32.dll").as_ptr()) };
    anyhow::ensure!(!local_kernel.is_null(), "Missing local kernel32");
    let local_load = unsafe { GetProcAddress(local_kernel, c"LoadLibraryW".as_ptr().cast()) }
        .context("Missing LoadLibraryW")? as usize;
    let load = kernel.base
        + local_load
            .checked_sub(local_kernel as usize)
            .context("Loader outside kernel32")?;
    let path_bytes = wide(helper_path.as_os_str())
        .iter()
        .flat_map(|v| v.to_le_bytes())
        .collect::<Vec<_>>();
    let mut remote_path = memory.allocate(path_bytes.len())?;
    memory.write(remote_path.address, &path_bytes)?;
    remote_path.run(load, 2_000)?;
    let helper = modules(pid)?
        .into_iter()
        .find(|m| module_path_matches(m, &helper_path))
        .context("Diagnostic helper not loaded")?;
    if let Some(expected) = handoff_status {
        let mut remote = memory.allocate(24)?;
        let mut output = [0u8; 24];
        memory.write(remote.address, &output)?;
        anyhow::ensure!(
            remote.run(helper.base + entry, 2_000)? == 0,
            "Handoff status failed"
        );
        memory.read(remote.address, &mut output)?;
        assert_eq!(
            read_u64(&output, 0)?,
            expected,
            "Unexpected handoff final state"
        );
        assert_eq!(
            read_u64(&output, 8)? as usize,
            studio.base + 0x4cc8dd0,
            "Native method was not restored"
        );
        assert_eq!(
            read_u64(&output, 16)?,
            0,
            "Private roots remain held by the hook"
        );
        println!("Handoff state {expected}: exact method restored, no roots held");
        return Ok(());
    }
    if let Some(flags) = export_flags {
        use std::io::Write;
        let scope = std::env::var("RENIUM_LOADER_PROBE_SCOPE").unwrap_or_else(|_| "all".into());
        anyhow::ensure!(
            matches!(scope.as_str(), "all" | "sync"),
            "Unknown probe scope"
        );
        let output = project.join(format!(
            "audit/testplace-spans-20260908/3/NativeSerializerMode{flags}-{scope}-{title}"
        ));
        anyhow::ensure!(!output.exists(), "Probe export already exists");
        let trace = trace_serializer(&studio.path, &exe)?;
        let roots = memory.read_u64(instance + model.layout.children)? as usize;
        let mut params = vec![0; if capture_identities { 4504 } else { 4448 }];
        for (at, value) in [
            (0, instance),
            (8, model.owner),
            (16, roots),
            (24, studio.base + layout.submit_task),
            (32, task_context),
            (40, studio.base + trace.serializer),
        ] {
            put_u64(&mut params, at, value);
        }
        put_u32(&mut params, 48, flags);
        put_u32(&mut params, 52, 10_000);
        if capture_identities {
            let (binding, getter) =
                properties::identity_binding(&memory, studio, &layout, &model, instance)?;
            put_u64(&mut params, 4448, binding);
            put_u64(&mut params, 4456, getter);
            let debug_id =
                properties::debug_id_function(&memory, studio, &layout, &model, instance)?;
            assert_eq!(
                debug_id - studio.base,
                0x1538680,
                "Pinned fixture's reflected GetDebugId entry changed"
            );
            put_u64(&mut params, 4496, debug_id);
            put_u32(&mut params, 4464, model.layout.children as u32);
            put_u32(
                &mut params,
                4468,
                properties::parent_offset(&memory, &model)? as u32,
            );
        }
        if scope == "sync" {
            let selected = model
                .roots
                .iter()
                .filter(|root| {
                    read_instance_class(&memory, root.instance, model.layout).is_some_and(|class| {
                        crate::roblox::services::explorer_service_order(&class).is_some()
                    })
                })
                .collect::<Vec<_>>();
            anyhow::ensure!(
                !selected.is_empty() && selected.len() <= 256,
                "Invalid selected roots"
            );
            put_u32(&mut params, 60, selected.len() as u32);
            for (index, root) in selected.iter().enumerate() {
                put_u64(&mut params, 352 + index * 16, root.instance);
                put_u64(&mut params, 360 + index * 16, root.owner);
            }
        }
        let mut remote = memory.allocate(params.len())?;
        memory.write(remote.address, &params)?;
        let invoke = Instant::now();
        let exit = remote.run(helper.base + entry, 12_000)?;
        memory.read(remote.address, &mut params)?;
        let buffer = RemoteAllocation {
            memory: &memory,
            address: read_u64(&params, 64)? as usize,
        };
        let status = read_u32(&params, 56)?;
        println!(
            "{}",
            serde_json::json!({"flags":flags,"status":status,"exit":exit,"invokeMs":invoke.elapsed().as_secs_f64()*1000.,"serializeMs":read_u64(&params,80)? as f64/1000.,"queueMs":read_u64(&params,88)? as f64/1000.,"error":error_text_at(&params,96)})
        );
        anyhow::ensure!(exit == 0 && status == 4, "Serializer probe failed");
        let len = read_u64(&params, 72)? as usize;
        anyhow::ensure!(
            (32..=64 * 1024 * 1024).contains(&len) && buffer.address != 0,
            "Invalid serializer buffer"
        );
        let bytes = memory.read_vec(buffer.address, len)?;
        rbx_binary::from_reader(bytes.as_slice()).context("Serializer returned invalid RBXL")?;
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&output)?
            .write_all(&bytes)?;
        println!("Serialized {len} bytes: {}", output.display());
        if capture_identities {
            let identities = RemoteAllocation {
                memory: &memory,
                address: read_u64(&params, 4472)? as usize,
            };
            let length = read_u64(&params, 4480)? as usize;
            anyhow::ensure!(
                identities.address != 0
                    && length > 0
                    && length <= 200_000 * 72
                    && length.is_multiple_of(72),
                "Invalid native identity capture"
            );
            let rows = memory.read_vec(identities.address, length)?;
            let path = output.with_extension("identities.bin");
            fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)?
                .write_all(&rows)?;
            println!(
                "Native identity capture: rows={} ms={:.3} output={}",
                length / 72,
                read_u64(&params, 4488)? as f64 / 1000.,
                path.display()
            );
        }
        return Ok(());
    }
    let retained_only = std::env::var_os("RENIUM_LOADER_PROBE_RETAINED_ONLY").is_some();
    anyhow::ensure!(
        !retained_only || receipts,
        "Retained event probe requires receipts"
    );
    let mut calls = if retained_only {
        Vec::new()
    } else {
        vec![(instance, model.owner, fixture)]
    };
    for (service, class) in [
        ("Workspace", "Terrain"),
        ("StarterPlayer", "StarterCharacterScripts"),
    ] {
        if paste_mode.is_some() || handoff || retained_reader {
            break;
        }
        let root = model
            .roots
            .iter()
            .find(|root| {
                read_instance_class(&memory, root.instance, model.layout).as_deref()
                    == Some(service)
            })
            .context("Missing retained service")?;
        let matches = read_children(&memory, root.instance, model.layout)
            .context("Missing retained children")?
            .into_iter()
            .filter(|child| {
                read_instance_class(&memory, child.instance, model.layout).as_deref() == Some(class)
            })
            .collect::<Vec<_>>();
        anyhow::ensure!(matches.len() == 1, "Ambiguous retained {service}.{class}");
        let target = matches[0];
        anyhow::ensure!(
            read_children(&memory, target.instance, model.layout)
                .context("Unreadable retained target")?
                .is_empty(),
            "Retained target is not empty"
        );
        calls.push((
            target.instance,
            target.owner,
            project.join(format!(
                "audit/testplace-spans-20260908/3/{prefix}{class}Children.rbxm"
            )),
        ));
    }
    let loader = if receipts {
        let reader = loader_trace.reader;
        let offset = image.rva_to_offset(reader)?;
        anyhow::ensure!(
            memory.read_vec(studio.base + reader, 64)? == exe[offset..offset + 64],
            "Loaded reader differs from pinned image"
        );
        // PRNT appends one shared pair per serialized root, never per descendant.
        // The fixed audit receipt must not enter the engine's vector-grow path.
        for (_, _, fixture) in &calls {
            let dom = rbx_binary::from_reader(fs::File::open(fixture)?)?;
            anyhow::ensure!(dom.root().children().len() < 256, "Too many receipt roots");
        }
        reader
    } else {
        loader
    };
    let stride = if retained_reader {
        20160
    } else if handoff {
        6568
    } else if paste_mode.is_some() {
        9208
    } else if private {
        6552
    } else if receipts {
        6496
    } else {
        1368
    };
    let parent_offset = if private {
        Some(properties::parent_offset(&memory, &model)?)
    } else {
        None
    };
    let serializer = if private {
        Some(trace_serializer(&studio.path, &exe)?.serializer)
    } else {
        None
    };
    let handoff_binding = if handoff {
        let services = model
            .roots
            .iter()
            .filter(|root| {
                read_instance_class(&memory, root.instance, model.layout).as_deref()
                    == Some("SerializationService")
            })
            .collect::<Vec<_>>();
        anyhow::ensure!(services.len() == 1, "Expected one SerializationService");
        let service = services[0].instance;
        let descriptor = find_class_member_descriptor(
            &memory,
            service,
            model.layout,
            "DeserializeInstancesAsync",
        )?;
        anyhow::ensure!(
            descriptor == studio.base + 0xdea0d30
                && memory.read_u64(descriptor + 0x78)? as usize == studio.base + 0x4cc8dd0
                && memory.read_u32(descriptor + 0x80)? == 0,
            "Private callback binding differs from inspected registration: descriptorRva={:#x}, methodRva={:#x}, adjustment={:#x}",
            descriptor - studio.base,
            memory.read_u64(descriptor + 0x78)? as usize - studio.base,
            memory.read_u32(descriptor + 0x80)?
        );
        Some((descriptor, service))
    } else {
        None
    };
    let mut batch = vec![0; 8 + 3 * stride];
    put_u32(&mut batch, 0, calls.len() as u32);
    let flags = std::env::var("RENIUM_LOADER_PROBE_FLAGS")
        .unwrap_or_else(|_| "4".into())
        .parse()?;
    anyhow::ensure!(
        matches!(flags, 0 | 4),
        "Only traced native caller flags are allowed"
    );
    put_u32(&mut batch, 4, flags);
    for (index, (target, owner, fixture)) in calls.iter().enumerate() {
        let params = &mut batch[8 + index * stride..8 + (index + 1) * stride];
        for (offset, value) in [
            (0, *target),
            (8, *owner),
            (16, task_context),
            (24, studio.base + layout.submit_task),
            (32, studio.base + loader),
        ] {
            put_u64(params, offset, value);
        }
        put_u32(params, 40, 15_000);
        if receipts {
            put_u32(params, 1372, model.layout.children as u32);
        }
        if let Some(offset) = parent_offset {
            put_u32(params, 6496, offset as u32);
        }
        if let Some(serializer) = serializer {
            put_u64(params, 6520, studio.base + serializer);
        }
        if let Some((descriptor, service)) = handoff_binding {
            put_u64(params, 6552, descriptor);
            put_u64(params, 6560, service);
        }
        if let Some(mode) = paste_mode {
            let workspace = model
                .roots
                .iter()
                .find(|root| {
                    read_instance_class(&memory, root.instance, model.layout).as_deref()
                        == Some("Workspace")
                })
                .context("Missing Workspace for native paste")?;
            anyhow::ensure!(
                memory.read_u64(model.outer + 0x348)? as usize == workspace.instance,
                "Pinned paste Workspace getter no longer matches the discovered root"
            );
            put_u64(params, 6552, workspace.instance);
            for (parameter, raw) in [
                (6560, 0x369d1e0),
                (6568, 0x369eb20),
                (6576, 0x369da40),
                (6584, 0x369d300),
                (6592, 0x9f6d80),
                (6600, 0x6d86fc0),
                (6608, 0x732970),
            ] {
                let rva = image.offset_to_rva(raw)?;
                image.require_executable_rva(rva)?;
                anyhow::ensure!(
                    image.function_bounds(raw)?.0 == raw
                        && memory.read_vec(studio.base + rva, 32)? == exe[raw..raw + 32],
                    "Native paste probe function differs from inspected image"
                );
                put_u64(params, parameter, studio.base + rva);
            }
            let dom = rbx_binary::from_reader(fixture_bytes.as_slice())?;
            anyhow::ensure!(
                dom.root().children().len() <= 32,
                "Too many native service wrappers"
            );
            put_u32(params, 6616, dom.root().children().len() as u32);
            put_u32(params, 6620, mode);
            put_u32(params, 6624, model.layout.name as u32);
            let name_ptr = memory.read_u64(workspace.instance + model.layout.name)? as usize;
            let name_inner = [0, 8]
                .into_iter()
                .find(|offset| {
                    read_msvc_string(&memory, name_ptr + offset).as_deref() == Some("Workspace")
                })
                .context("Native instance-name string layout changed")?;
            put_u32(params, 6628, name_inner as u32);
            for (index, root) in dom.root().children().iter().enumerate() {
                let source = dom.get_by_ref(*root).context("Missing source wrapper")?;
                anyhow::ensure!(
                    source.class.as_str() == "Folder" && source.name.len() < 64,
                    "Unexpected native service wrapper"
                );
                let target = model
                    .roots
                    .iter()
                    .find(|candidate| {
                        read_instance_class(&memory, candidate.instance, model.layout).as_deref()
                            == Some(source.name.as_str())
                    })
                    .context("Missing target service for native paste")?;
                put_u64(params, 6632 + index * 16, target.instance);
                put_u64(params, 6640 + index * 16, target.owner);
                params[7144 + index * 64..7144 + index * 64 + source.name.len()]
                    .copy_from_slice(source.name.as_bytes());
            }
        }
        if retained_reader {
            native_retained_audit::configure(
                params,
                &memory,
                studio,
                &model,
                &image,
                &exe,
                retained_input
                    .as_ref()
                    .context("Missing retained input model")?,
            )?;
        }
        let input = wide(fixture.as_os_str())
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect::<Vec<_>>();
        anyhow::ensure!(input.len() <= 1040, "Fixture path too long");
        params[72..72 + input.len()].copy_from_slice(&input);
    }
    let mut remote = memory.allocate(batch.len())?;
    memory.write(remote.address, &batch)?;
    let prepared_ms = started.elapsed().as_secs_f64() * 1000.;
    println!("Probe preparation complete: {prepared_ms:.3}ms; invoking isolated native operation");
    let invoked = Instant::now();
    let exit_code = remote.run(helper.base + entry, 17_000)?;
    memory.read(remote.address, &mut batch)?;
    let invoke_ms = invoked.elapsed().as_secs_f64() * 1000.;
    let outputs = if private {
        (0..calls.len())
            .map(|index| {
                Ok(RemoteAllocation {
                    memory: &memory,
                    address: read_u64(&batch, 8 + index * stride + 6528)? as usize,
                })
            })
            .collect::<Result<Vec<_>>>()?
    } else {
        Vec::new()
    };
    for (index, (_, _, fixture)) in calls.iter().enumerate() {
        let params = &batch[8 + index * stride..8 + (index + 1) * stride];
        let status = read_u32(params, 44)?;
        println!(
            "{}",
            serde_json::json!({"file":fixture.file_name().map(|name| name.to_string_lossy()),"pid":pid,"preparationMs":prepared_ms,"batchInvokeMs":invoke_ms,"readMs":read_u64(params,48)? as f64/1000.,"queueMs":read_u64(params,56)? as f64/1000.,"loadMs":read_u64(params,64)? as f64/1000.,"status":status,"exitCode":exit_code,"error":error_text_at(params,1112)})
        );
        anyhow::ensure!(
            exit_code == 0 && status == 4,
            "Native loader failed; inspect the owned fixture before further mutations"
        );
        if retained_reader {
            let backup = RemoteAllocation {
                memory: &memory,
                address: read_u64(params, 20136)? as usize,
            };
            if backup.address != 0 {
                let length = read_u64(params, 20144)? as usize;
                ensure!(
                    (32..=256 * 1024).contains(&length),
                    "Invalid retained baseline length"
                );
                let bytes = memory.read_vec(backup.address, length)?;
                let dom = rbx_binary::from_reader(bytes.as_slice())?;
                let path = project.join(format!(
                    "audit/native-retained-reader/{title}.baseline.rbxl"
                ));
                fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(path)?
                    .write_all(&bytes)?;
                println!(
                    "Retained baseline: instances={} bytes={} captureMs={:.3} sha256={:x}",
                    dom.descendants().count() - 1,
                    length,
                    read_u64(params, 20152)? as f64 / 1000.,
                    Sha256::digest(&bytes)
                );
            }
            let count = read_u32(params, 1388)?;
            ensure!(
                read_u32(params, 2160)? == count,
                "Retained reader did not bind every target"
            );
            println!(
                "Retained native reader: all {count} existing objects reused, factory slots restored"
            );
        }
        if let Some(mode) = paste_mode {
            println!(
                "Native insertion A/B: mode={mode} attachMs={:.3} roots={} groups={}",
                read_u64(params, 9192)? as f64 / 1000.,
                read_u32(params, 9200)?,
                read_u32(params, 9204)?
            );
        }
        if receipts {
            let count = read_u32(params, 1368)? as usize;
            anyhow::ensure!(count < 256, "Invalid receipt count");
            if handoff {
                let dom = rbx_binary::from_reader(fs::File::open(fixture)?)?;
                assert_eq!(count, dom.root().children().len());
                assert_eq!(
                    read_u32(params, 6500)? as usize,
                    dom.descendants().count() - 1
                );
                println!(
                    "Private handoff armed: roots={count} instances={}",
                    read_u32(params, 6500)?
                );
                continue;
            }
            if private {
                use std::io::Write;
                let length = read_u64(params, 6536)? as usize;
                anyhow::ensure!(
                    outputs[index].address != 0 && (32..=64 * 1024 * 1024).contains(&length),
                    "Invalid private snapshot"
                );
                let bytes = memory.read_vec(outputs[index].address, length)?;
                let captured = rbx_binary::from_reader(bytes.as_slice())?;
                let output_path = fixture.with_file_name(format!("Private-{index}-{title}"));
                fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&output_path)?
                    .write_all(&bytes)?;
                let dom = rbx_binary::from_reader(fs::File::open(fixture)?)?;
                println!(
                    "Private receipt: roots={count} expectedRoots={} instances={} expectedInstances={} inspectMs={:.3} serializeMs={:.3} disposeMs={:.3} classes={:?}",
                    dom.root().children().len(),
                    read_u32(params, 6500)?,
                    dom.descendants().count() - 1,
                    read_u64(params, 6504)? as f64 / 1000.,
                    read_u64(params, 6544)? as f64 / 1000.,
                    read_u64(params, 6512)? as f64 / 1000.,
                    captured
                        .root()
                        .children()
                        .iter()
                        .map(|id| captured.get_by_ref(*id).unwrap().class.as_str())
                        .collect::<Vec<_>>()
                );
                assert_eq!(
                    read_u32(params, 6500)? as usize,
                    captured.descendants().count() - 1
                );
                if wrapped {
                    assert_eq!(count, dom.root().children().len());
                    assert_eq!(captured.descendants().count(), dom.descendants().count());
                }
                continue;
            }
            let mut observed = Vec::with_capacity(count);
            for index in 0..count {
                let instance = read_u64(params, 1376 + index * 16)? as usize;
                let owner = read_u64(params, 1384 + index * 16)? as usize;
                anyhow::ensure!(instance != 0 && owner != 0, "Receipt lost its owner");
                observed.push(
                    read_instance_class(&memory, instance, model.layout)
                        .context("Receipt instance is unreadable")?,
                );
                println!(
                    "Receipt before queue release: class={} children={}",
                    observed.last().unwrap(),
                    read_u32(params, 5472 + index * 4)?
                );
                if observed.last().is_some_and(|class| class == "Clouds")
                    && let Ok(expected) = std::env::var("RENIUM_LOADER_PROBE_CLOUD_CHILDREN")
                {
                    assert_eq!(
                        read_u32(params, 5472 + index * 4)?,
                        expected.parse::<u32>()?,
                        "Foreign callback ordering changed inside the native task"
                    );
                }
            }
            let dom = rbx_binary::from_reader(fs::File::open(fixture)?)?;
            let mut expected = dom
                .root()
                .children()
                .iter()
                .map(|id| dom.get_by_ref(*id).unwrap().class.to_string())
                .collect::<Vec<_>>();
            observed.sort();
            expected.sort();
            assert_eq!(
                observed, expected,
                "Receipt must contain exactly the serialized roots"
            );
            println!("Native root receipt: {count} exact live roots ({observed:?})");
        }
    }
    Ok(())
}

#[test]
#[ignore = "Builds only an owned local package fixture from the offline comparison copy"]
fn build_property_package_fixture() -> Result<()> {
    use rbx_dom_weak::InstanceBuilder;
    let mut source =
        rbx_binary::from_reader(fs::File::open("../../audit/place-comparison/current.rbxl")?)?;
    let mut pending = source.root().children().to_vec();
    let root = loop {
        let id = pending
            .pop()
            .context("No linked package in offline fixture")?;
        let node = source.get_by_ref(id).unwrap();
        if node.class.as_str() == "PackageLink" {
            break node.parent();
        }
        pending.extend_from_slice(node.children());
    };
    let child = source
        .get_by_ref(root)
        .unwrap()
        .children()
        .iter()
        .map(|id| source.get_by_ref(*id).unwrap())
        .find(|node| node.class.as_str() != "PackageLink")
        .context("Package has no editable child")?
        .name
        .clone();
    source.get_by_ref_mut(root).unwrap().name = "ReniumAccessPackage".into();
    let mut target = rbx_binary::from_reader(fs::File::open(
        "../../audit/performance/ReniumPropertyTest.rbxl",
    )?)?;
    let service = target.insert(target.root_ref(), InstanceBuilder::new("ReplicatedStorage"));
    source.transfer(root, &mut target, service);
    rbx_binary::to_writer(
        fs::File::create("../../audit/performance/ReniumPropertyPackageTest.rbxl")?,
        &target,
        target.root().children(),
    )?;
    println!(
        "{}",
        serde_json::json!({"target":["ReplicatedStorage","ReniumAccessPackage",child]})
    );
    Ok(())
}

#[test]
#[ignore = "Builds an owned local geometry fixture from the existing offline comparison copy"]
fn build_property_geometry_fixture() -> Result<()> {
    use rbx_dom_weak::{InstanceBuilder, types::Variant};
    let mut source =
        rbx_binary::from_reader(fs::File::open("../../audit/place-comparison/current.rbxl")?)?;
    let mut pending = source.root().children().to_vec();
    let mesh = loop {
        let id = pending.pop().context("No MeshPart in fixture")?;
        let node = source.get_by_ref(id).unwrap();
        if node.class.as_str() == "MeshPart" {
            break id;
        }
        pending.extend_from_slice(node.children());
    };
    for child in source.get_by_ref(mesh).unwrap().children().to_vec() {
        source.destroy(child);
    }
    let mut target = rbx_binary::from_reader(fs::File::open(
        "../../audit/network-simulation/ReniumNetworkTest.rbxl",
    )?)?;
    let workspace = target
        .root()
        .children()
        .iter()
        .find(|id| target.get_by_ref(**id).unwrap().class.as_str() == "Workspace")
        .copied()
        .unwrap_or_else(|| target.insert(target.root_ref(), InstanceBuilder::new("Workspace")));
    let node = source.get_by_ref_mut(mesh).unwrap();
    node.name = "ReniumAccessFixture".into();
    node.properties
        .insert("Anchored".into(), Variant::Bool(true));
    println!(
        "Geometry fixture: {:?}",
        node.properties.get(&"MeshId".into())
    );
    source.transfer(mesh, &mut target, workspace);
    target.insert(
        workspace,
        InstanceBuilder::new("StringValue")
            .with_name("ReniumAccessText")
            .with_property("Value", "fidelity-".repeat(1024)),
    );
    rbx_binary::to_writer(
        fs::File::create("../../audit/performance/ReniumPropertyTest.rbxl")?,
        &target,
        target.root().children(),
    )?;
    Ok(())
}

#[test]
#[ignore = "Requires RENIUM_INSPECT_FIXTURE_PID pointing at the explicitly prepared disposable place"]
fn protected_property_live_fixture() -> Result<()> {
    let pid = std::env::var("RENIUM_INSPECT_FIXTURE_PID")?.parse()?;
    let title = "ReniumPropertyTest.rbxl";
    let mut samples = Vec::new();
    for index in 0..10 {
        let started = Instant::now();
        let mut text = prepare_property(
            pid,
            title,
            &["Workspace".into(), "ReniumAccessText".into()],
            &[],
            "Value",
            Duration::from_secs(3),
        )?;
        assert_eq!(text.class_name, "StringValue");
        assert_eq!(text.read()?, "fidelity-".repeat(1024));
        samples.push(started.elapsed().as_secs_f64() * 1000.);
        if index == 0 {
            let changed = text.write("native value 🧪");
            let restored = text.write(&"fidelity-".repeat(1024));
            assert_eq!(changed?, "native value 🧪");
            assert_eq!(restored?, "fidelity-".repeat(1024));
        }
    }
    println!("end-to-end native prepare/read ms: {samples:?}");
    let path = ["Workspace".into(), "ReniumAccessFixture".into()];
    let mut property = prepare_property(
        pid,
        title,
        &path,
        &[],
        "CollisionFidelity",
        Duration::from_secs(3),
    )?;
    assert_eq!(property.class_name, "MeshPart");
    let initial = property.read()?;
    println!("geometry initial CollisionFidelity={initial}");
    let started = Instant::now();
    assert_eq!(property.write("Hull")?, "Hull");
    println!(
        "verified CollisionFidelity write ms: {:.3}",
        started.elapsed().as_secs_f64() * 1000.
    );
    assert_eq!(property.write(&initial)?, initial);
    let mut streaming = prepare_property(
        pid,
        title,
        &["Workspace".into()],
        &[],
        "StreamingEnabled",
        Duration::from_secs(3),
    )?;
    assert_eq!(streaming.read()?, "false");
    let changed = streaming.write("true");
    let restored = streaming.write("false");
    assert_eq!(changed?, "true");
    assert_eq!(restored?, "false");
    Ok(())
}

#[test]
#[ignore = "Exercises deletion/replacement only inside the owned ReniumPropertyTest fixture"]
fn protected_property_rejects_replaced_live_targets() -> Result<()> {
    let pid = std::env::var("RENIUM_INSPECT_FIXTURE_PID")?.parse()?;
    let title = "ReniumPropertyTest.rbxl";
    let executable = std::env::var_os("RENIUM_LIVE_TEST_RBX").context(
        "Set RENIUM_LIVE_TEST_RBX to the installed CLI, avoiding old workspace binaries",
    )?;
    let run = |code: &str| -> Result<()> {
        let output = std::process::Command::new(&executable)
            .current_dir("../../audit/network-simulation")
            .args(["--place", title, "l", code, "--timeout", "3"])
            .output()?;
        anyhow::ensure!(
            output.status.success(),
            "Fixture command failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let result: serde_json::Value = serde_json::from_slice(&output.stdout)?;
        anyhow::ensure!(
            output.status.success() && result["ok"] == true,
            "Fixture edit failed: {result}; {}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(())
    };
    let replace = "local old=workspace:FindFirstChild(\"ReniumAccessReplacement\"); if old then old:Destroy() end; local p=Instance.new(\"StringValue\"); p.Name=\"ReniumAccessReplacement\"; p.Value=\"unchanged\"; p.Parent=workspace";
    let path = ["Workspace".into(), "ReniumAccessReplacement".into()];
    for _ in 0..20 {
        run(replace)?;
        let mut before = prepare_property(pid, title, &path, &[], "Value", Duration::from_secs(3))?;
        run(replace)?;
        assert!(
            before.write("wrong-instance").is_err(),
            "A stale target must never be edited"
        );
        let mut after = prepare_property(pid, title, &path, &[], "Value", Duration::from_secs(3))?;
        assert_ne!(before.instance_id, after.instance_id);
        assert_eq!(after.read()?, "unchanged");
    }
    run("workspace.ReniumAccessReplacement:Destroy()")?;
    Ok(())
}
