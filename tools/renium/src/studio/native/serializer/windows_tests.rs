use super::*;

#[test]
#[ignore = "requires RENIUM_PROBE_PID; only reads the selected Studio process"]
fn manual_play_teleport_launch_probe() -> Result<()> {
    let pid: u32 = std::env::var("RENIUM_PROBE_PID")?.parse()?;
    let title = crate::studio::input::studio_window_title(pid)?;
    let result = probe_operation(pid, &title, NativeOperation::PlayTeleportLaunch);
    println!(
        "{}",
        serde_json::to_string(&super::super::probes::ProbeVerdict::from_result(
            NativeOperation::PlayTeleportLaunch,
            &result,
        ))?
    );
    result.map(|_| ())
}

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
                | "launchTestModeAction"
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
        task_abi: Cell::new(None),
        play_abi: Cell::new(None),
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
        task_abi: Cell::new(None),
        play_abi: Cell::new(None),
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
        task_abi: Cell::new(None),
        play_abi: Cell::new(None),
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
    for (call_at, lea_at, slot, capture) in [(8, 32, 16, 8), (64, 64, 0, 0), (24, 48, 40, 24)] {
        let mut fixture = SerializerFixture::new(capture)?;
        fixture.relative(0x1000 + call_at, &[0xe8], 0x1100);
        fixture.relative(0x1100 + lea_at, &[0x48, 0x8d, 0x0d], 0x3000);
        for at in (0..=slot + 8).step_by(8) {
            fixture.pointer(0x3000 + at, 0x1500);
        }
        fixture.pointer(0x3000 + slot, 0x1200);
        fixture.pointer(0x3000 + slot + 8, 0x1280);
        let image = fixture.image();
        assert_eq!(serializer_callback_target(&image, 0x1200), Some(0x1300));
        assert_eq!(serializer_callback_target(&image, 0x1280), Some(0x1400));
        assert_eq!(serializer_implementation(&image, 0x1000)?, 0x1300);
    }
    Ok(())
}

#[test]
fn serializer_discovery_refuses_wrong_stream_abi_and_ambiguous_targets() -> Result<()> {
    let mut fixture = SerializerFixture::new(0)?;
    fixture.relative(0x1020, &[0xe8], 0x1100);
    fixture.relative(0x1140, &[0x48, 0x8d, 0x0d], 0x3000);
    fixture.pointer(0x3000, 0x1280); // Binary serializer, but takes a string buffer.
    assert!(serializer_implementation(&fixture.image(), 0x1000).is_err());
    fixture.pointer(0x3008, 0x1200);
    assert_eq!(serializer_implementation(&fixture.image(), 0x1000)?, 0x1300);
    fixture.relative(0x140a, &[0xff, 0x15], 0x3100);
    assert!(serializer_implementation(&fixture.image(), 0x1000).is_err());
    // Duplicate table entries for one implementation are not ambiguity.
    fixture.pointer(0x3000, 0x1200);
    assert_eq!(serializer_implementation(&fixture.image(), 0x1000)?, 0x1300);
    Ok(())
}

#[test]
fn serializer_discovery_validates_all_forwarded_arguments_and_binary_format() -> Result<()> {
    let fixture = SerializerFixture::new(8)?;
    let original = fixture.bytes.clone();
    // Capture loads must retain their relationship, including the stack arguments.
    for needle in [
        "4c8b4920", "4c8b4118", "488b5110", "488b4148", "8b4128", "488b4908",
    ] {
        let mut changed = SerializerFixture {
            bytes: original.clone(),
        };
        let pattern = hex(needle)?;
        let at = memmem::find(&changed.bytes[0x600..0x660], &pattern).unwrap() + 0x600;
        changed.bytes[at + pattern.len() - 1] += 8;
        assert_eq!(serializer_callback_target(&changed.image(), 0x1200), None);
    }
    assert!(serializer_stream_contract(
        &fixture.image(),
        0x1300,
        0x3100
    )?);
    for offset in [0x702, 0x712, 0x1520] {
        // Header, chunk reference, import name.
        let mut changed = SerializerFixture {
            bytes: original.clone(),
        };
        changed.bytes[offset] ^= 1;
        let image = changed.image();
        assert!(
            serializer_stream_write(&image).is_err()
                || !serializer_stream_contract(&image, 0x1300, 0x3100)?
        );
    }
    Ok(())
}

struct SerializerFixture {
    bytes: Vec<u8>,
}

impl SerializerFixture {
    fn new(capture: u8) -> Result<Self> {
        let mut fixture = Self {
            bytes: vec![0; 0x1900],
        };
        fixture.bytes[0x400..0x1400].fill(0x90);
        put_u32(&mut fixture.bytes, 0x3c, 0x80);
        put_u32(&mut fixture.bytes, 0x98 + 108, 16);
        put_u32(&mut fixture.bytes, 0x98 + 112 + 12 * 8, 0x3100);
        put_u32(&mut fixture.bytes, 0x98 + 116 + 12 * 8, 8);
        put_u64(&mut fixture.bytes, 0x1500, 0x311e);
        let name = b"?write@?$basic_ostream@DU?$char_traits@D@std@@@std@@QEAAAEAV12@PEBD_J@Z\0";
        fixture.bytes[0x1520..0x1520 + name.len()].copy_from_slice(name);
        for (n, start) in [0x1000, 0x1100, 0x1200, 0x1280, 0x1300, 0x1400, 0x1500]
            .into_iter()
            .enumerate()
        {
            put_u32(&mut fixture.bytes, 0x1800 + n * 12, start);
            put_u32(&mut fixture.bytes, 0x1804 + n * 12, start + 0x80);
        }
        // Deliberately unrelated prologues: neither is a locator signature.
        let prologue = hex("48895c2410488974241848897c242055")?;
        fixture.bytes[0x500..0x500 + prologue.len()].copy_from_slice(&prologue);
        fixture.callback(0x1200, capture, 0x1300)?;
        fixture.callback(0x1280, capture, 0x1400)?;
        for rva in [0x1300, 0x1400] {
            let offset = fixture.image().rva_to_offset(rva)?;
            fixture.bytes[offset..offset + 2].copy_from_slice(&[0x48, 0xb8]);
            fixture.bytes[offset + 2..offset + 10].copy_from_slice(b"<roblox!");
            for (n, chunk) in [b"INST\0", b"PROP\0", b"PRNT\0", b"END\0\0"]
                .into_iter()
                .enumerate()
            {
                fixture.bytes[0x15c0 + n * 8..0x15c5 + n * 8].copy_from_slice(chunk);
                fixture.relative(rva + 16 + n * 7, &[0x48, 0x8d, 0x15], 0x31c0 + n * 8);
            }
        }
        fixture.relative(0x130a, &[0xff, 0x15], 0x3100);
        Ok(fixture)
    }

    fn callback(&mut self, rva: usize, base: u8, target: usize) -> Result<()> {
        let mut code = hex("4883ec68488b41")?;
        code.push(base + 72);
        for (opcode, field) in [("4c8b49", 24), ("4c8b41", 16), ("488b51", 8)] {
            code.extend(hex(opcode)?);
            code.push(base + field);
        }
        code.extend(hex("48c7442450000000004889442448")?);
        for (field, stack) in [(64, 0x40), (56, 0x38), (48, 0x30), (40, 0x28)] {
            code.extend([
                0x48,
                0x8b,
                0x41,
                base + field,
                0x48,
                0x89,
                0x44,
                0x24,
                stack,
            ]);
        }
        code.extend([0x8b, 0x41, base + 32]);
        if base == 0 {
            code.extend([0x48, 0x8b, 0x09]);
        } else {
            code.extend([0x48, 0x8b, 0x49, base]);
        }
        code.extend([0x89, 0x44, 0x24, 0x20]);
        let offset = self.image().rva_to_offset(rva)?;
        self.bytes[offset..offset + code.len()].copy_from_slice(&code);
        self.relative(rva + code.len(), &[0xe8], target);
        self.bytes[offset + code.len() + 5..offset + code.len() + 10]
            .copy_from_slice(&[0x48, 0x83, 0xc4, 0x68, 0xc3]);
        Ok(())
    }

    fn relative(&mut self, rva: usize, opcode: &[u8], target: usize) {
        let offset = self.image().rva_to_offset(rva).unwrap();
        self.bytes[offset..offset + opcode.len()].copy_from_slice(opcode);
        let displacement = target as i32 - (rva + opcode.len() + 4) as i32;
        self.bytes[offset + opcode.len()..offset + opcode.len() + 4]
            .copy_from_slice(&displacement.to_le_bytes());
    }

    fn pointer(&mut self, rva: usize, target: usize) {
        let offset = self.image().rva_to_offset(rva).unwrap();
        put_u64(&mut self.bytes, offset, 0x100000 + target);
    }

    fn image(&self) -> PeImage<'_> {
        let sections = [
            (b".text\0\0\0", 0x1000, 0x400, 0x1000, 0x2000_0000),
            (b".rdata\0\0", 0x3000, 0x1400, 0x400, 0),
            (b".pdata\0\0", 0x4000, 0x1800, 7 * 12, 0),
        ]
        .into_iter()
        .map(
            |(name, virtual_address, raw_offset, raw_size, characteristics)| PeSection {
                name: *name,
                virtual_address,
                virtual_size: raw_size,
                raw_offset,
                raw_size,
                characteristics,
            },
        )
        .collect();
        PeImage {
            bytes: &self.bytes,
            image_base: 0x100000,
            image_stamp: [1, 2, 3],
            sections,
        }
    }
}
