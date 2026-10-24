use super::*;

#[test]
#[ignore = "requires an explicitly owned Studio PID; invokes its real Play/Stop control"]
fn manual_play_control_for_teleport_regression() -> Result<()> {
    let pid: u32 = std::env::var("RENIUM_TELEPORT_TEST_PID")?.parse()?;
    let action = std::env::var("RENIUM_TELEPORT_TEST_ACTION")?;
    anyhow::ensure!(
        matches!(
            action.as_str(),
            "simulationPlayAction"
                | "simulationPlayFromCameraAction"
                | "simulationResetAction"
                | "startServerAction"
                | "addClientsAction"
                | "cleanupServersAndPlayersAction"
        ),
        "Not a playtest control"
    );
    let title = crate::studio::input::studio_window_title(pid)?;
    let foreground = unsafe { windows_sys::Win32::UI::WindowsAndMessaging::GetForegroundWindow() };
    let result = platform_trigger_studio_action(pid, &title, &action)?;
    anyhow::ensure!(
        foreground == unsafe { windows_sys::Win32::UI::WindowsAndMessaging::GetForegroundWindow() },
        "Play control changed keyboard focus"
    );
    println!(
        "Studio control {action}: {} matching action(s)",
        result.found
    );
    Ok(())
}

#[test]
fn native_gate_rejects_writes_allocations_and_both_thread_ownership_modes() {
    // Invalid handles/addresses must never reach a Windows mutation API.
    let memory = std::mem::ManuallyDrop::new(ProcessMemory {
        handle: null_mut(),
        pid: 0,
        verified: Cell::new(false),
    });
    assert_eq!(
        memory.write(0, &[1]).unwrap_err().to_string(),
        NATIVE_WRITES_DISABLED
    );
    assert_eq!(
        memory.allocate(1).err().unwrap().to_string(),
        NATIVE_WRITES_DISABLED
    );
    assert_eq!(
        memory.start_thread(0, 0).unwrap_err().to_string(),
        NATIVE_WRITES_DISABLED
    );
    for helper_owned in [false, true] {
        let mut remote = RemoteAllocation {
            memory: &memory,
            address: 0,
        };
        assert_eq!(
            remote
                .run_with_input_ownership(0, 0, helper_owned)
                .unwrap_err()
                .to_string(),
            NATIVE_WRITES_DISABLED
        );
    }
}

#[test]
fn native_gate_rejects_new_and_resident_helpers() {
    let memory = std::mem::ManuallyDrop::new(ProcessMemory {
        handle: null_mut(),
        pid: 0,
        verified: Cell::new(false),
    });
    let path = Path::new("resident-helper.dll");
    let resident = [ModuleEntry {
        base: 1,
        size: 1,
        name: "resident-helper.dll".into(),
        path: path.into(),
    }];
    for modules in [&[][..], &resident[..]] {
        assert_eq!(
            ensure_library_loaded(0, &memory, modules, 0, path)
                .unwrap_err()
                .to_string(),
            NATIVE_WRITES_DISABLED
        );
    }
    assert_eq!(
        ensure_helper_loaded(0, &memory, &[])
            .unwrap_err()
            .to_string(),
        NATIVE_WRITES_DISABLED
    );
    let verified = std::mem::ManuallyDrop::new(ProcessMemory {
        handle: null_mut(),
        pid: 0,
        verified: Cell::new(true),
    });
    assert_eq!(
        ensure_library_loaded(0, &verified, &resident, 0, path).unwrap(),
        1
    );
}

#[test]
fn read_only_probes_refuse_non_studio_processes_without_loading_a_helper() -> Result<()> {
    let pid = std::process::id();
    let before = modules(pid)?.len();
    for operation in NativeOperation::ALL.into_iter().chain([
        NativeOperation::PlayTeleport,
        NativeOperation::PlayTeleportLoad,
        NativeOperation::PlayTeleportDispatch,
        NativeOperation::PlayTeleportLaunch,
        NativeOperation::PlayTeleportJoin,
    ]) {
        let error = probe_operation(pid, "", operation).unwrap_err().to_string();
        assert!(
            error.contains("not Roblox Studio"),
            "{operation:?}: {error}"
        );
    }
    assert_eq!(before, modules(pid)?.len());
    Ok(())
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
            assert_eq!(
                read_u32(&params, PARAM_PROTOCOL_VERSION)?,
                SERIALIZER_VERSION
            );
        }
    }
    Ok(())
}

#[test]
fn embedded_helper_exports_the_matching_serializer_protocol() -> Result<()> {
    let image = PeImage::parse(HELPER_BYTES)?;
    let rva = helper_export_rva("ReniumSerializerAbiVersion")?;
    assert_eq!(
        read_u32(HELPER_BYTES, image.rva_to_offset(rva)?)?,
        SERIALIZER_VERSION
    );
    Ok(())
}

/// `RENIUM_PROBE_PID=PID cargo test probe_reflection_members -- --ignored --nocapture`
/// prints the reflection descriptors of the listed members in a running Studio.
#[test]
#[ignore]
fn probe_reflection_members() -> Result<()> {
    let pid: u32 = std::env::var("RENIUM_PROBE_PID")?.parse()?;
    let title = crate::studio::input::studio_window_title(pid)?;
    let current = modules(pid)?;
    let studio = current
        .iter()
        .find(|module| module.name.eq_ignore_ascii_case("RobloxStudioBeta.exe"))
        .context("Studio module missing")?;
    let layout = package_layout(&studio.path)?;
    let memory = ProcessMemory::open(pid)?;
    let model = active_data_model(pid, &memory, studio, layout.data, &title)?;
    for (class, members) in [
        (
            "StudioPublishService",
            &[
                "PublishAs",
                "ShowSaveOrPublishPlaceToRoblox",
                "SetUploadNames",
                "PublishLocked",
            ][..],
        ),
        (
            "HttpRbxApiService",
            &["GetAsync", "GetDocumentationUrl"][..],
        ),
    ] {
        let Some(service) = model.roots.iter().copied().find(|entry| {
            read_instance_class(&memory, entry.instance, model.layout).as_deref() == Some(class)
        }) else {
            println!("{class}: no service root");
            continue;
        };
        for name in members {
            match find_class_member_descriptor(&memory, service.instance, model.layout, name) {
                Ok(descriptor) => {
                    let signature = read_rtti_type(&memory, descriptor, studio.base, studio.size);
                    let kind = memory
                        .read_u64(descriptor + 0x28)
                        .ok()
                        .and_then(|kind| read_msvc_string(&memory, kind as usize));
                    println!(
                        "{class}.{name}: descriptor={descriptor:#x} kind={kind:?} rtti={signature:?}"
                    );
                }
                Err(error) => println!("{class}.{name}: {error:#}"),
            }
        }
    }
    Ok(())
}

/// Explicitly opt into exercising only a dedicated scratch Studio.
#[test]
#[ignore]
fn exercise_native_scratch_capture() -> Result<()> {
    let pid: u32 = std::env::var("RENIUM_NATIVE_TARGET_PID")?.parse()?;
    let title = crate::studio::input::studio_window_title(pid)?;
    anyhow::ensure!(
        title.contains("astra-native-") || title.contains("renium-native-canary-"),
        "A dedicated scratch place is required"
    );
    let directory = PathBuf::from(std::env::var("RENIUM_NATIVE_ARTIFACTS")?);
    fs::create_dir_all(&directory)?;
    let start = Instant::now();
    for index in 0..20 {
        let capture = capture_live_services(
            pid,
            &title,
            &["Workspace".into(), "ServerScriptService".into()],
            Duration::from_secs(30),
        )?;
        assert!(!capture.identities.is_empty());
        if index == 0 || index == 19 {
            println!(
                "capture {index}: {} rows",
                capture.identities.len() / CAPTURE_ROW_SIZE
            );
        }
    }
    let output = directory.join(format!(
        "native-{pid}-{}.rbxl",
        crate::app::timing::current_millis()
    ));
    let result = write_live_place(pid, &title, &output)?;
    assert!(result.instance_count > 0);
    println!(
        "snapshot: {} instances, {} bytes; 20 captures + snapshot {:.3}s; {}",
        result.instance_count,
        result.output_size,
        start.elapsed().as_secs_f64(),
        output.display()
    );
    Ok(())
}

/// Exercise engine-owned strings and the Terrain dispatcher only in a canary.
#[test]
#[ignore]
fn exercise_native_scratch_properties_and_terrain() -> Result<()> {
    use rbx_dom_weak::types::Variant;
    let pid: u32 = std::env::var("RENIUM_NATIVE_TARGET_PID")?.parse()?;
    let title = crate::studio::input::studio_window_title(pid)?;
    anyhow::ensure!(
        title.contains("astra-native-") || title.contains("renium-native-canary-"),
        "A dedicated scratch place is required"
    );
    let directory = PathBuf::from(std::env::var("RENIUM_NATIVE_ARTIFACTS")?);
    fs::create_dir_all(&directory)?;
    let mut property = prepare_property(
        pid,
        &title,
        &["ServerScriptService".into()],
        &[],
        "Name",
        Duration::from_secs(60),
    )?;
    for _ in 0..100 {
        assert_eq!(property.read()?, "ServerScriptService");
    }
    println!("100 engine-owned heap-string reads passed");

    let snapshot = directory.join(format!(
        "terrain-before-{pid}-{}.rbxl",
        crate::app::timing::current_millis()
    ));
    write_live_place(pid, &title, &snapshot)?;
    let dom = rbx_binary::from_reader(std::io::BufReader::new(fs::File::open(snapshot)?))?;
    let original = dom
        .descendants()
        .find(|node| node.class == "Terrain")
        .context("Scratch snapshot has no Terrain")?;
    let bytes = |name: &str| -> Result<Vec<u8>> {
        match original.properties.get(&name.into()) {
            Some(Variant::BinaryString(value)) => {
                let bytes: &[u8] = value.as_ref();
                Ok(bytes.to_vec())
            }
            None => Ok(Vec::new()),
            _ => bail!("Unexpected Terrain {name} representation"),
        }
    };
    let smooth = bytes("SmoothGrid")?;
    let physics = bytes("PhysicsGrid")?;
    let token = format!(
        "native-canary-terrain-{}",
        crate::app::timing::current_millis()
    );
    register_history(pid, &title, &token)?;
    let mut terrain = prepare_terrain(
        pid,
        &title,
        &["Workspace".into(), "Terrain".into()],
        &[],
        Duration::from_secs(60),
    )?;
    assert!(!terrain.write(&token, None, None)?);
    let fingerprint = terrain.fingerprint();
    // One canonical v1 chunk, 128 runs of 256 solid material-1 cells.
    let mut changed = vec![1, 5];
    changed.extend_from_slice(&[0; 12]);
    for _ in 0..128 {
        changed.extend_from_slice(&[0x81, 255]);
    }
    if smooth == changed {
        changed[13] = 1;
    }
    let result = terrain.write(&token, Some(&changed), None);
    println!(
        "Terrain mutation: {result:?}; saved grids {} / {} bytes",
        smooth.len(),
        physics.len()
    );
    // These are two standalone writes. Let deferred physics work settle, then
    // give restoration its own guard instead of reusing the first write's state.
    std::thread::sleep(Duration::from_millis(100));
    let restore = format!("{token}-restore");
    register_history(pid, &title, &restore)?;
    terrain
        .write(&restore, None, None)
        .context("Read restoration baseline")?;
    terrain
        .write(&restore, Some(&smooth), Some(&physics))
        .context("Restore saved Terrain grids")?;
    assert_eq!(terrain.fingerprint(), fingerprint);
    assert!(result?);
    println!("Terrain Clear/replace/restore passed; original fingerprint {fingerprint}");
    Ok(())
}

#[test]
fn serializer_discovery_follows_relocated_calls_and_callback_slots() -> Result<()> {
    for (call_at, lea_at, slot) in [(8usize, 16usize, 0usize), (64, 48, 32)] {
        let mut bytes = vec![0; 768];
        bytes[..512].fill(0x90);
        let call = |bytes: &mut [u8], offset: usize, target: usize| {
            bytes[offset] = 0xe8;
            bytes[offset + 1..offset + 5]
                .copy_from_slice(&((target as i32) - (0x1000 + offset + 5) as i32).to_le_bytes());
        };
        call(&mut bytes, call_at, 0x1080);
        bytes[128..132].copy_from_slice(&[0x55, 0x48, 0x89, 0xe5]);
        let lea = 128 + lea_at;
        bytes[lea..lea + 3].copy_from_slice(&[0x48, 0x8d, 0x0d]);
        bytes[lea + 3..lea + 7]
            .copy_from_slice(&(0x2000_i32 - (0x1000 + lea + 7) as i32).to_le_bytes());
        bytes[256..260].copy_from_slice(&[0x48, 0x83, 0xec, 0x68]);
        call(&mut bytes, 280, 0x11f0);
        for offset in (0..=slot).step_by(8) {
            put_u64(&mut bytes, 512 + offset, 0x1011e0);
        }
        put_u64(&mut bytes, 512 + slot, 0x101100);
        for (index, (start, end)) in [(0x1000, 0x1050), (0x1080, 0x10d0), (0x1100, 0x1130)]
            .into_iter()
            .enumerate()
        {
            put_u32(&mut bytes, 640 + index * 12, start);
            put_u32(&mut bytes, 644 + index * 12, end);
        }
        let image = PeImage {
            bytes: &bytes,
            image_base: 0x100000,
            image_stamp: [1, 2, 3],
            sections: vec![
                PeSection {
                    name: *b".text\0\0\0",
                    virtual_address: 0x1000,
                    virtual_size: 512,
                    raw_offset: 0,
                    raw_size: 512,
                    characteristics: 0x2000_0000,
                },
                PeSection {
                    name: *b".rdata\0\0",
                    virtual_address: 0x2000,
                    virtual_size: 128,
                    raw_offset: 512,
                    raw_size: 128,
                    characteristics: 0,
                },
                PeSection {
                    name: *b".pdata\0\0",
                    virtual_address: 0x3000,
                    virtual_size: 36,
                    raw_offset: 640,
                    raw_size: 36,
                    characteristics: 0,
                },
            ],
        };
        assert_eq!(
            unique_function_call(&image, 0x1000, &[0x55, 0x48, 0x89, 0xe5])?,
            0x1080
        );
        assert_eq!(
            serializer_callback(&image, 0x1080, &[0x48, 0x83, 0xec, 0x68])?,
            0x1100
        );
        assert_eq!(direct_calls(&image, 0x1100)?, vec![0x11f0]);
        assert!(serializer_callback(&image, 0x1080, &[0x48, 0x83, 0xec, 0x70]).is_err());
    }
    Ok(())
}
