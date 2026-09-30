use super::*;

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
