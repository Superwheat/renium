use super::*;

#[test]
#[ignore = "Read-only frame counter probe in the explicitly owned pacing fixture"]
fn native_edit_frame_progress() -> Result<()> {
    let pid = std::env::var("RENIUM_FRAME_PROBE_PID")?.parse()?;
    let title = "ReniumFramePacingProbe.rbxl";
    let path = ["Run Service".to_string()];
    let mut samples = Vec::new();
    for _ in 0..4 {
        samples.push(properties::read_property(
            pid,
            title,
            &path,
            &[],
            "RunService",
            "FrameNumber",
            Duration::from_secs(5),
        )?);
        std::thread::sleep(Duration::from_millis(40));
    }
    println!("FrameNumber samples: {samples:?}");
    Ok(())
}

#[test]
#[ignore = "Owned fixture only: descriptor inspection, with an explicit opt-in native relay probe"]
#[expect(
    clippy::cognitive_complexity,
    reason = "Opt-in pinned-ABI diagnostic keeps its independent probe modes and safety checks together"
)]
fn native_attribute_relay_layout() -> Result<()> {
    use serde_json::json;
    use sha2::{Digest, Sha256};
    let pid: u32 = std::env::var("RENIUM_ATTRIBUTE_RELAY_PID")?.parse()?;
    let subtrees = std::env::var_os("RENIUM_SUBTREE_RELAY").is_some();
    let benchmark = std::env::var("RENIUM_ATTRIBUTE_RELAY_BENCH").ok();
    let title = benchmark
        .as_deref()
        .unwrap_or("ReniumNativeAttributeRelay1.rbxl");
    let project = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()?;
    let directory = project.join("audit/full-push-optimization/attribute-relay");
    let fixture = if let Some(name) = title
        .strip_prefix("ReniumPushBench-")
        .and_then(|name| name.strip_suffix(".rbxl"))
    {
        anyhow::ensure!(
            name.starts_with("encoding-lz4-")
                && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'),
            "Invalid owned benchmark name"
        );
        project
            .join("audit/full-push-optimization")
            .join(name)
            .join(title)
    } else {
        anyhow::ensure!(
            title == "ReniumNativeAttributeRelay1.rbxl",
            "Unexpected relay fixture"
        );
        directory.join(title)
    };
    anyhow::ensure!(fixture.is_file(), "Missing owned relay fixture");
    capture_window(pid, title)?;
    let current_modules = modules(pid)?;
    let studio = current_modules
        .iter()
        .find(|m| m.name.eq_ignore_ascii_case("RobloxStudioBeta.exe"))
        .context("Not Studio")?;
    anyhow::ensure!(
        format!("{:x}", Sha256::digest(fs::read(&studio.path)?))
            == "baba316f8be298c82cd6160e3eaf2a9cd7de3d9bad2e484dc811b745ade68238",
        "Audit image changed"
    );
    let memory = ProcessMemory::open(pid)?;
    let layout = package_layout(&studio.path)?;
    verify_loaded_image(&memory, studio, layout.image_stamp)?;
    let mut model = active_data_model(pid, &memory, studio, layout.data, title)?;
    let path = [
        if benchmark.is_some() {
            "CoreGui"
        } else {
            "ServerStorage"
        },
        "ReniumAttributeRelay",
        "Notify",
    ]
    .map(str::to_string);
    let ancestors = properties::resolve_path(&memory, &model, &path, &[])?;
    let target = ancestors.last().context("Missing relay")?;
    anyhow::ensure!(
        read_instance_class(&memory, target.instance, model.layout).as_deref()
            == Some("ObjectValue"),
        "Relay class changed"
    );
    let mut descriptions = Vec::new();
    for name in [
        "Value",
        "Changed",
        "Attributes",
        "AttributeChanged",
        "DescendantAdded",
    ] {
        let descriptor =
            find_class_member_descriptor(&memory, target.instance, model.layout, name)?;
        let vtable = memory.read_u64(descriptor)? as usize;
        let slots = (0..32)
            .map(|slot| {
                let address = memory.read_u64(vtable + slot * 8)? as usize;
                Ok(json!({"slot":slot * 8, "rva":address.checked_sub(studio.base)}))
            })
            .collect::<Result<Vec<_>>>()?;
        let mut fields = Vec::new();
        for offset in (0x40..0x180).step_by(8) {
            let pointer = memory.read_u64(descriptor + offset)? as usize;
            if let Some(kind) = read_rtti_type(&memory, pointer, studio.base, studio.size) {
                let vtable = memory.read_u64(pointer)? as usize;
                let slots = (0..8)
                    .map(|slot| {
                        let address = memory.read_u64(vtable + slot * 8)? as usize;
                        Ok(json!({"slot":slot * 8, "rva":address.checked_sub(studio.base)}))
                    })
                    .collect::<Result<Vec<_>>>()?;
                fields.push(json!({"offset":offset, "kind":kind,"slots":slots}));
            }
        }
        descriptions.push(json!({"name":name,"kind":read_rtti_type(&memory,descriptor,studio.base,studio.size), "slots":slots,"fields":fields}));
    }
    fs::write(
        directory.join("descriptors.json"),
        serde_json::to_vec_pretty(&descriptions)?,
    )?;
    if std::env::var_os("RENIUM_ATTRIBUTE_RELAY_RUN").is_some() {
        let target = *target;
        let history = model
            .roots
            .iter()
            .find(|root| {
                read_instance_class(&memory, root.instance, model.layout).as_deref()
                    == Some("ChangeHistoryService")
            })
            .context("No history service")?;
        let descriptor =
            find_class_member_descriptor(&memory, history.instance, model.layout, "SetEnabled")?;
        let members = memory.read_vec(descriptor + 0x50, 0x58)?;
        let bytes = fs::read(&studio.path)?;
        let image = PeImage::parse(&bytes)?;
        let mut candidates = Vec::new();
        for offset in (0..0x50).step_by(8) {
            let Some(rva) = (read_u64(&members, offset)? as usize).checked_sub(studio.base) else {
                continue;
            };
            if let Ok(trace) = observation::signal::discover(&image, rva) {
                anyhow::ensure!(
                    read_u32(&members, offset + 8)? == 0,
                    "Changed this adjustment"
                );
                candidates.push(trace);
            }
        }
        anyhow::ensure!(candidates.len() == 1, "Ambiguous native signal layout");
        let trace = candidates.remove(0);
        for &rva in trace
            .functions
            .iter()
            .chain([0xce6320usize, 0x17fff60].iter())
        {
            let offset = image.rva_to_offset(rva)?;
            let (start, end) = image.function_bounds(offset)?;
            anyhow::ensure!(
                start == offset
                    && memory.read_vec(studio.base + rva, end - start)? == bytes[start..end],
                "Loaded signal code differs"
            );
        }
        let changed =
            find_class_member_descriptor(&memory, target.instance, model.layout, "Changed")?;
        anyhow::ensure!(
            read_rtti_type(&memory, changed, studio.base, studio.size).as_deref()
                == Some(
                    ".?AV?$EventDesc@VObjectValue@RBX@@$$A6AXV?$shared_ptr@VInstance@RBX@@@std@@@ZV?$signal@$$A6AXV?$shared_ptr@VInstance@RBX@@@std@@@Z@rbx@@PEQ12@V56@@Reflection@RBX@@"
                ),
            "Typed relay signature changed"
        );
        let changed_vtable = memory.read_u64(changed)? as usize;
        anyhow::ensure!(
            memory.read_u64(changed_vtable + 0x30)? as usize == studio.base + 0x17fff60,
            "Typed relay dispatcher changed"
        );
        let relay_offset = memory.read_u32(changed + 0x78)? as usize;
        anyhow::ensure!(
            (8..0x1000).contains(&relay_offset) && relay_offset.is_multiple_of(8),
            "Invalid signal member"
        );
        let attributes =
            find_class_member_descriptor(&memory, target.instance, model.layout, "Attributes")?;
        let subtree_signal = if subtrees {
            let descriptor = find_class_member_descriptor(
                &memory,
                target.instance,
                model.layout,
                "DescendantAdded",
            )?;
            let kind = read_rtti_type(&memory, descriptor, studio.base, studio.size)
                .context("No descendant signal RTTI")?;
            anyhow::ensure!(kind.starts_with(".?AV?$EventDesc@VInstance@RBX@@$$A6AXV?$shared_ptr@VInstance@RBX@@@std@@@ZV?$signal@"), "Unexpected descendant signal: {kind}");
            anyhow::ensure!(
                kind.ends_with("P812@EAAPEAV56@_N@Z@Reflection@RBX@@"),
                "Descendant signal accessor ABI changed"
            );
            let accessor = memory.read_u64(descriptor + 0x78)? as usize;
            anyhow::ensure!(
                memory.read_u32(descriptor + 0x80)? == 0,
                "Descendant accessor this adjustment changed"
            );
            let accessor_offset = image.rva_to_offset(
                accessor
                    .checked_sub(studio.base)
                    .context("Accessor outside module")?,
            )?;
            let (accessor_start, accessor_end) = image.function_bounds(accessor_offset)?;
            anyhow::ensure!(
                accessor_start == accessor_offset
                    && memory.read_vec(accessor, accessor_end - accessor_start)?
                        == bytes[accessor_start..accessor_end],
                "Loaded descendant accessor differs"
            );
            let vtable = memory.read_u64(descriptor)? as usize;
            let dispatcher = memory.read_u64(vtable + 0x30)? as usize - studio.base;
            let start = image.rva_to_offset(dispatcher)?;
            let (_, end) = image.function_bounds(start)?;
            anyhow::ensure!(
                memory.read_vec(studio.base + dispatcher, end - start)? == bytes[start..end],
                "Descendant dispatcher differs"
            );
            let calls = iced_x86::Decoder::with_ip(
                64,
                &bytes[start..end],
                dispatcher as u64,
                iced_x86::DecoderOptions::NONE,
            )
            .into_iter()
            .filter(|i| {
                i.mnemonic() == iced_x86::Mnemonic::Call
                    && i.op0_kind() == iced_x86::OpKind::NearBranch64
                    && i.near_branch_target() == 0xce6320
            })
            .count();
            anyhow::ensure!(
                calls == 1,
                "Descendant dispatcher does not use the proven typed signal fire"
            );
            Some(accessor)
        } else {
            None
        };
        let context = data_model_task_context(&memory, studio, &layout, &model)?;
        properties::verified_code(
            &memory,
            studio,
            &layout,
            studio.base + layout.submit_task,
            64,
        )?;
        let parent = properties::parent_offset(&memory, &model)?;
        if benchmark.is_none() {
            model.roots.retain(|root| {
                read_instance_class(&memory, root.instance, model.layout).as_deref()
                    == Some("ServerStorage")
            });
        } else if subtrees {
            model.roots.retain(|root| {
                matches!(
                    read_instance_class(&memory, root.instance, model.layout).as_deref(),
                    Some(
                        "Workspace"
                            | "Players"
                            | "Lighting"
                            | "MaterialService"
                            | "ReplicatedFirst"
                            | "ReplicatedStorage"
                            | "ServerScriptService"
                            | "ServerStorage"
                            | "StarterGui"
                            | "StarterPack"
                            | "StarterPlayer"
                            | "Teams"
                            | "SoundService"
                            | "VoiceChatService"
                            | "TextChatService"
                            | "TestService"
                            | "LocalizationService"
                            | "VRService"
                    )
                )
            });
        }
        let (_, serializer, _) = studio_layout(&studio.path)?;
        let output = directory.join(if benchmark.is_some() {
            "response-bench.bin"
        } else {
            "response.bin"
        });
        fs::write(&output, [])?;
        let mut params = build_parameters(studio.base, serializer, &model, &output, true)?;
        params.resize(if subtrees { 6024 } else { 6008 }, 0);
        for (offset, value) in [
            (PARAM_TASK_CONTEXT, context),
            (PARAM_SUBMIT_TASK, studio.base + layout.submit_task),
            (PARAM_WINDOW, capture_window(pid, title)?.0),
            (5904, subtree_signal.unwrap_or(trace.signal_offset)),
            (5912, studio.base + trace.ensure),
            (5920, studio.base + trace.allocate),
            (5928, studio.base + trace.append),
            (5936, studio.base + trace.disconnect),
            (5944, studio.base + trace.assign),
            (5952, attributes),
            (5976, target.instance),
            (5984, target.owner),
            (5992, relay_offset),
            (6000, studio.base + 0xce6320),
        ] {
            put_u64(&mut params, offset, value);
        }
        if subtrees {
            let mut single_path = path.clone();
            single_path[2] = "Single".to_string();
            // CoreGui is intentionally outside the observed export roots.
            let complete_model = active_data_model(pid, &memory, studio, layout.data, title)?;
            let ancestors = properties::resolve_path(&memory, &complete_model, &single_path, &[])?;
            let single = ancestors.last().context("No single-event relay")?;
            anyhow::ensure!(
                read_instance_class(&memory, single.instance, model.layout).as_deref()
                    == Some("ObjectValue"),
                "Single relay changed class"
            );
            put_u64(&mut params, 6008, single.instance);
            put_u64(&mut params, 6016, single.owner);
        }
        put_u32(&mut params, PARAM_PROCESS_ID, pid);
        put_u32(&mut params, PARAM_PARENT_OFFSET, u32::try_from(parent)?);
        put_u32(
            &mut params,
            PARAM_TIMEOUT,
            if benchmark.is_some() { 10_000 } else { 1000 },
        );
        let helper_path = directory.join(if subtrees {
            "subtrees.dll"
        } else {
            "relay.dll"
        });
        let helper_bytes = fs::read(&helper_path)?;
        let helper_image = PeImage::parse(&helper_bytes)?;
        let optional = read_u32(&helper_bytes, 60)? as usize + 24;
        let exports =
            helper_image.rva_to_offset(read_u32(&helper_bytes, optional + 112)? as usize)?;
        let names = helper_image.rva_to_offset(read_u32(&helper_bytes, exports + 32)? as usize)?;
        let ordinals =
            helper_image.rva_to_offset(read_u32(&helper_bytes, exports + 36)? as usize)?;
        let functions =
            helper_image.rva_to_offset(read_u32(&helper_bytes, exports + 28)? as usize)?;
        let mut entry = None;
        for i in 0..read_u32(&helper_bytes, exports + 24)? as usize {
            let name =
                helper_image.rva_to_offset(read_u32(&helper_bytes, names + i * 4)? as usize)?;
            if helper_bytes[name..].starts_with(if subtrees {
                b"ReniumAuditSubtreeRelay\0"
            } else {
                b"ReniumAuditAttributeRelay\0"
            }) {
                entry = Some(read_u32(
                    &helper_bytes,
                    functions + read_u16(&helper_bytes, ordinals + i * 2)? as usize * 4,
                )? as usize);
            }
        }
        let entry = entry.context("Missing relay export")?;
        let kernel = current_modules
            .iter()
            .find(|m| m.name.eq_ignore_ascii_case("kernel32.dll"))
            .context("No kernel")?;
        let local_kernel = unsafe { GetModuleHandleW(wide("kernel32.dll").as_ptr()) };
        anyhow::ensure!(!local_kernel.is_null(), "No local kernel");
        let load = unsafe { GetProcAddress(local_kernel, c"LoadLibraryW".as_ptr().cast()) }
            .context("No loader")? as usize;
        let load_rva = load
            .checked_sub(local_kernel as usize)
            .context("Forwarded loader")?;
        let path_bytes = wide(helper_path.as_os_str())
            .into_iter()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>();
        let mut remote_path = memory.allocate(path_bytes.len())?;
        memory.write(remote_path.address, &path_bytes)?;
        remote_path.run(kernel.base + load_rva, 2000)?;
        let helper = modules(pid)?
            .into_iter()
            .find(|m| module_path_matches(m, &helper_path))
            .context("Relay was not loaded")?;
        let mut remote = memory.allocate(params.len())?;
        memory.write(remote.address, &params)?;
        let result = remote.run(helper.base + entry, 15_000)?;
        let response = fs::read(&output)?;
        anyhow::ensure!(
            result == 0 && read_u32(&response, 8)? == 4,
            "Relay failed {result}: {}",
            String::from_utf8_lossy(response.get(64..).unwrap_or_default())
        );
        println!(
            "Native relay completed and disconnected; events={}, batched={}",
            read_u64(&response, 16)?,
            read_u64(&response, 24)?
        );
    }
    println!("Relay descriptor inspection saved");
    Ok(())
}

#[test]
#[ignore = "Owned blank Edit fixture and explicit audit sampler DLL only; never part of normal checks"]
#[expect(
    clippy::cognitive_complexity,
    reason = "Opt-in pinned-ABI diagnostic keeps its independent probe modes and safety checks together"
)]
fn native_push_worker_samples() -> Result<()> {
    use std::os::windows::process::CommandExt;
    let profile = std::env::var_os("RENIUM_PUSH_PROFILE").is_some();
    let pid: u32 = std::env::var("RENIUM_PUSH_SAMPLE_PID")?.parse()?;
    let title = std::env::var("RENIUM_PUSH_SAMPLE_TITLE")?;
    let number = title
        .strip_prefix("ReniumNativePushSample")
        .and_then(|s| s.strip_suffix(".rbxl"))
        .context("Not an owned sampler fixture")?;
    anyhow::ensure!(
        !number.is_empty() && number.bytes().all(|c| c.is_ascii_digit()),
        "Invalid sampler fixture number"
    );
    let project = Path::new("../..").canonicalize()?;
    let fixture = project.join("audit/import-emission1").join(&title);
    anyhow::ensure!(fixture.is_file(), "Owned sampler fixture is absent");
    let window = capture_window(pid, &title)?;
    let current_modules = modules(pid)?;
    anyhow::ensure!(
        current_modules
            .iter()
            .any(|m| m.name.eq_ignore_ascii_case("RobloxStudioBeta.exe")),
        "Not a Studio process"
    );
    let memory = ProcessMemory::open(pid)?;
    let profile_model = if profile {
        eprintln!("Profiler: validating Studio image");
        let studio = current_modules
            .iter()
            .find(|m| m.name.eq_ignore_ascii_case("RobloxStudioBeta.exe"))
            .context("Not Studio")?;
        // Use the OS implementation: this host is deliberately a debug test
        // binary, whose software SHA loop can dominate fixture startup.
        let hash = std::process::Command::new("certutil.exe")
            .args(["-hashfile"])
            .arg(&studio.path)
            .arg("SHA256")
            .creation_flags(0x0800_0000)
            .output()?;
        anyhow::ensure!(
            hash.status.success()
                && String::from_utf8_lossy(&hash.stdout)
                    .lines()
                    .any(|line| line.trim()
                        == "baba316f8be298c82cd6160e3eaf2a9cd7de3d9bad2e484dc811b745ade68238"),
            "Native call profiler image changed"
        );
        eprintln!("Profiler: resolving image layout");
        let layout = package_layout(&studio.path)?;
        verify_loaded_image(&memory, studio, layout.image_stamp)?;
        eprintln!("Profiler: resolving owned DataModel");
        let model = active_data_model(pid, &memory, studio, layout.data, &title)?;
        eprintln!("Profiler: owned DataModel resolved");
        Some(model.outer + model.layout.data_model_instance)
    } else {
        None
    };
    let helper_path = project.join(if profile {
        "audit/native-push-profiler.dll"
    } else {
        "audit/native-push-sampler.dll"
    });
    let bytes = fs::read(&helper_path)?;
    let pe = PeImage::parse(&bytes)?;
    let optional = read_u32(&bytes, 60)? as usize + 24;
    let exports = pe.rva_to_offset(read_u32(&bytes, optional + 112)? as usize)?;
    let names = pe.rva_to_offset(read_u32(&bytes, exports + 32)? as usize)?;
    let ordinals = pe.rva_to_offset(read_u32(&bytes, exports + 36)? as usize)?;
    let functions = pe.rva_to_offset(read_u32(&bytes, exports + 28)? as usize)?;
    let mut entry = None;
    for i in 0..read_u32(&bytes, exports + 24)? as usize {
        let name = pe.rva_to_offset(read_u32(&bytes, names + i * 4)? as usize)?;
        if bytes[name..].starts_with(b"ReniumPushSampler\0") {
            let ordinal = read_u16(&bytes, ordinals + i * 2)? as usize;
            entry = Some(read_u32(&bytes, functions + ordinal * 4)? as usize);
        }
    }
    let entry = entry.context("Sampler export is absent")?;
    let kernel = current_modules
        .iter()
        .find(|m| m.name.eq_ignore_ascii_case("kernel32.dll"))
        .context("Missing target kernel32")?;
    let local_kernel = unsafe { GetModuleHandleW(wide("kernel32.dll").as_ptr()) };
    anyhow::ensure!(!local_kernel.is_null(), "Missing local kernel32");
    let load = unsafe { GetProcAddress(local_kernel, c"LoadLibraryW".as_ptr().cast()) }
        .context("Missing LoadLibraryW")? as usize;
    let rva = load
        .checked_sub(local_kernel as usize)
        .context("Forwarded loader")?;
    let path_bytes = wide(helper_path.as_os_str())
        .into_iter()
        .flat_map(u16::to_le_bytes)
        .collect::<Vec<_>>();
    let mut remote_path = memory.allocate(path_bytes.len())?;
    memory.write(remote_path.address, &path_bytes)?;
    remote_path.run(kernel.base + rva, 2_000)?;
    let helper = modules(pid)?
        .into_iter()
        .find(|m| module_path_matches(m, &helper_path))
        .context("Sampler helper was not loaded")?;
    let output = project
        .join("audit")
        .join(format!("native-push-sample{number}"));
    anyhow::ensure!(
        !output.with_extension("ready.json").exists(),
        "Sample output exists"
    );
    let encoded = wide(output.as_os_str());
    anyhow::ensure!(encoded.len() <= 520, "Sampler output path is too long");
    let mut params = vec![0u8; if profile { 1312 } else { 1304 }];
    if let Some(model) = profile_model {
        put_u64(&mut params, 1304, model);
    }
    for (i, unit) in encoded.into_iter().enumerate() {
        params[i * 2..i * 2 + 2].copy_from_slice(&unit.to_le_bytes());
    }
    put_u32(&mut params, 1040, 10_000);
    anyhow::ensure!(
        capture_window(pid, &title)? == window,
        "Sampler target changed"
    );
    let mut remote = memory.allocate(params.len())?;
    memory.write(remote.address, &params)?;
    eprintln!("Sampler: invoking helper");
    let code = remote.run(helper.base + entry, 15_000)?;
    memory.read(remote.address, &mut params)?;
    anyhow::ensure!(
        code == 0 && read_u32(&params, 1044)? == 4,
        "Sampler failed: code {code:#x}, status {}, {}",
        read_u32(&params, 1044)?,
        String::from_utf8_lossy(&params[1048..1304])
    );
    println!("Samples: {}.{pid}.stacks.tsv", output.display());
    Ok(())
}
