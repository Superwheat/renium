use super::*;

#[test]
fn native_build_gate_rejects_newer_studio_builds() {
    for build in [740, 741] {
        assert!(native_build_verified(build), "0.{build}");
    }
    for build in [742, 743, u32::MAX] {
        assert!(!native_build_verified(build), "0.{build}");
    }
}

#[test]
fn native_gate_rejects_writes_allocations_and_both_thread_ownership_modes() {
    // Invalid handles/addresses must never reach a Windows mutation API.
    let memory = std::mem::ManuallyDrop::new(ProcessMemory {
        handle: null_mut(),
        verified: false,
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
        verified: false,
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
        verified: true,
    });
    assert_eq!(
        ensure_library_loaded(0, &verified, &resident, 0, path).unwrap(),
        1
    );
}

#[test]
fn native_snapshots_and_capture_reject_unknown_builds_before_discovery() {
    // The test process has no Studio version. Exercise both public snapshot
    // paths without opening or modifying any running Studio.
    let pid = std::process::id();
    let output = std::env::temp_dir().join(format!("renium-unverified-snapshot-{pid}.rbxl"));
    assert!(!output.exists());
    for result in [
        write_live_place(pid, "", &output),
        write_live_service(pid, "", "Workspace", &output),
    ] {
        assert_eq!(result.err().unwrap().to_string(), NATIVE_WRITES_DISABLED);
    }
    assert_eq!(
        capture_live_services(pid, "", &["Workspace".into()], Duration::from_secs(1))
            .err()
            .unwrap()
            .to_string(),
        NATIVE_WRITES_DISABLED
    );
    assert!(!output.exists());
}

#[test]
fn native_gate_preserves_the_windows_audio_exemption() -> Result<()> {
    let pid = std::process::id();
    assert!(!ProcessMemory::open(pid)?.verified);
    let audio = ProcessMemory::open_for_windows_audio(pid)?;
    audio.ensure_writes_verified()?;
    // Keep the exempt handle alive while reopening for an engine operation.
    // Neither the audio permission nor the once-per-PID warning grants access.
    for _ in 0..2 {
        assert!(!ProcessMemory::open(pid)?.verified);
    }
    Ok(())
}

/// Run only against an owned scratch Studio, never a working place:
/// `RENIUM_PROBE_PID=PID RENIUM_PROBE_PLACE=PATH cargo test
/// live_unverified_studio_snapshot_gate -- --ignored --nocapture`.
#[test]
#[ignore = "requires an explicitly selected scratch Studio on an unverified build"]
fn live_unverified_studio_snapshot_gate() -> Result<()> {
    let pid: u32 = std::env::var("RENIUM_PROBE_PID")?.parse()?;
    let place = fs::canonicalize(std::env::var("RENIUM_PROBE_PLACE")?)?;
    let title = crate::studio::input::studio_window_title(pid)?;
    let title_place = title
        .strip_suffix(" - Roblox Studio")
        .context("Expected a local scratch place window")?;
    assert_eq!(fs::canonicalize(title_place)?, place);
    let build = studio_build_number(pid)?;
    assert!(
        !native_build_verified(build),
        "Studio 0.{build} is verified"
    );
    let output = place.with_file_name("snapshot-gate-refused.rbxl");
    assert!(!output.exists());
    let helper_modules = || -> Result<Vec<PathBuf>> {
        Ok(modules(pid)?
            .into_iter()
            .filter(|module| module.name.starts_with("renium-studio-helper-"))
            .map(|module| module.path)
            .collect())
    };
    assert!(helper_modules()?.is_empty(), "Use a fresh scratch Studio");
    let before = fs::read(&place)?;
    // Opening the audio-exempt handle does not invoke or install an audio hook.
    let audio = ProcessMemory::open_for_windows_audio(pid)?;
    audio.ensure_writes_verified()?;
    for attempt in 1..=2 {
        for result in [
            write_live_place(pid, &title, &output),
            write_live_service(pid, &title, "Workspace", &output),
        ] {
            assert_eq!(result.err().unwrap().to_string(), NATIVE_WRITES_DISABLED);
        }
        assert!(!output.exists());
        assert!(helper_modules()?.is_empty());
        println!(
            "pid={pid} build=0.{build} attempt={attempt}: place/service snapshots refused, no helper loaded"
        );
    }
    assert_eq!(fs::read(&place)?, before);
    assert_eq!(crate::studio::input::studio_window_title(pid)?, title);
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
        }
    }
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
