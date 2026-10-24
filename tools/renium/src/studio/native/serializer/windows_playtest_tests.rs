use super::*;

fn fixture(shift: usize) -> (Vec<u8>, Vec<PeSection>) {
    let base = 0x140000000usize;
    let mut bytes = vec![0u8; 0x1800];
    let sections = [
        (b".text\0\0\0", 0, 0x400, 0x1000, 0x2000_0000),
        (b".rdata\0\0", 0x400, 0x800, 0x3000, 0),
        (b".data\0\0\0", 0xc00, 0x800, 0x5000, 0),
        (b".pdata\0\0", 0x1400, 0x60, 0x7000, 0),
    ]
    .into_iter()
    .map(|(name, raw, size, rva, flags)| PeSection {
        name: *name,
        raw_offset: raw,
        raw_size: size,
        virtual_size: size,
        virtual_address: rva + shift,
        characteristics: flags,
    })
    .collect();
    bytes[..3].copy_from_slice(&[0x48, 0x8d, 0x0d]);
    bytes[3..7].copy_from_slice(&(0x3000i32 - 0x1007).to_le_bytes());
    bytes[7] = 0xe8;
    bytes[8..12].copy_from_slice(&(0x1100i32 - 0x100c).to_le_bytes());
    bytes[12..15].copy_from_slice(&[0x48, 0x8b, 0x81]);
    put_u32(&mut bytes, 15, 0x358);
    bytes[19] = 0xc3;
    bytes[0x100] = 0xc3;
    let anchor = b"ExecutePlayModeAsync\0";
    bytes[0x400..0x400 + anchor.len()].copy_from_slice(anchor);
    let kind = b".?AVPlayController@RBX@@\0";
    bytes[0x490..0x490 + kind.len()].copy_from_slice(kind);
    put_u32(&mut bytes, 0x500, 1);
    put_u32(&mut bytes, 0x50c, (0x3080 + shift) as u32);
    put_u32(&mut bytes, 0x510, (0x3180 + shift) as u32);
    put_u32(&mut bytes, 0x514, (0x3100 + shift) as u32);
    put_u64(&mut bytes, 0x600, base + 0x3100 + shift);
    put_u64(&mut bytes, 0x608, base + 0x1100 + shift);
    for (index, unit) in "StartServer\0".encode_utf16().enumerate() {
        bytes[0x701 + index * 2..0x703 + index * 2].copy_from_slice(&unit.to_le_bytes());
    }
    put_u64(&mut bytes, 0x780, base + 0x3000 + shift);
    for (index, (begin, end)) in [(0x1000, 0x1014), (0x1100, 0x1101)].into_iter().enumerate() {
        put_u32(&mut bytes, 0x1400 + index * 12, (begin + shift) as u32);
        put_u32(&mut bytes, 0x1404 + index * 12, (end + shift) as u32);
    }
    (bytes, sections)
}

#[test]
fn playtest_discovery_follows_relocated_rtti_strings_calls_and_fields() -> Result<()> {
    for shift in [0, 0x12000, 0x900000] {
        let (bytes, sections) = fixture(shift);
        let pe = PeImage {
            bytes: &bytes,
            image_base: 0x140000000,
            sections,
            image_stamp: [0; 3],
        };
        let anchors = image::anchors(&pe, &[])?;
        let focused = image::anchors(&pe, &["executePLAYmode".into()])?;
        assert_eq!(focused.len(), 1);
        assert_eq!(focused[0].text, "ExecutePlayModeAsync");
        assert!(
            anchors
                .iter()
                .any(|a| a.text == "ExecutePlayModeAsync" && a.rva == 0x3000 + shift)
        );
        assert!(
            anchors
                .iter()
                .any(|a| a.text == "StartServer" && a.rva == 0x3301 + shift)
        );
        let rtti = image::rtti(&pe, &anchors)?;
        assert_eq!(rtti.len(), 1);
        assert_eq!(rtti[0]["vtableRva"], 0x3208 + shift);
        assert_eq!(rtti[0]["slots"][0]["targetRva"], 0x1100 + shift);
        let targets = anchors.iter().map(|a| a.rva).collect();
        let refs = image::pointer_references(&pe, &targets)?;
        assert_eq!(refs[&(0x3000 + shift)], vec![0x3380 + shift]);
        let index = image::index(&pe, &targets)?;
        assert_eq!(
            index.references[&(0x3000 + shift)][0].target,
            0x1000 + shift
        );
        assert_eq!(index.calls[&(0x1000 + shift)][0].target, 0x1100 + shift);
        assert_eq!(index.callers[&(0x1100 + shift)][0].target, 0x1000 + shift);
        assert_eq!(
            image::code_range(&pe, &index, 0x1007 + shift)?,
            (0x1000 + shift, 0x1014 + shift)
        );
        let operands = image::operands(&bytes[..20], 0x1000 + shift);
        assert!(
            operands
                .iter()
                .any(|row| row["displacement"] == 0x358 && row["base"] == "RCX")
        );
    }
    Ok(())
}

#[test]
fn playtest_discovery_rejects_broken_rtti_and_unbounded_functions() -> Result<()> {
    let (mut bytes, sections) = fixture(0);
    put_u32(&mut bytes, 0x514, 0x3104);
    let pe = PeImage {
        bytes: &bytes,
        image_base: 0x140000000,
        sections,
        image_stamp: [0; 3],
    };
    assert!(image::rtti(&pe, &image::anchors(&pe, &[])?)?.is_empty());
    let index = image::index(&pe, &BTreeSet::new())?;
    assert!(image::code_range(&pe, &index, 0x1200).is_err());
    assert!(image::code_range(&pe, &index, 0x3000).is_err());
    assert!(image::anchors(&pe, &[String::new()]).is_err());
    Ok(())
}

#[test]
fn playtest_http_profile_finds_relocated_request_and_response_leads() -> Result<()> {
    for shift in [0, 0x900000] {
        let (mut bytes, sections) = fixture(shift);
        for (offset, text) in [
            (0x800, "Request Context Failure"),
            (0x900, "makePlaceLauncherRequestForTeleport"),
            (0xa00, "GameJoinResponse"),
            (0xb00, "assetdelivery"),
        ] {
            bytes[offset..offset + text.len()].copy_from_slice(text.as_bytes());
        }
        let pe = PeImage {
            bytes: &bytes,
            image_base: 0x140000000,
            sections,
            image_stamp: [0; 3],
        };
        let anchors = image::anchors(
            &pe,
            &image::HTTP_TERMS
                .iter()
                .map(|term| (*term).to_owned())
                .collect::<Vec<_>>(),
        )?;
        assert_eq!(anchors.len(), 4);
        assert!(anchors.iter().all(|a| a.rva >= 0x3400 + shift));
        assert!(anchors.iter().any(|a| a.text == "GameJoinResponse"));
        assert!(anchors.iter().all(|a| a.text != "ExecutePlayModeAsync"));
    }
    assert!(image::http_import(
        &json!({"module":"WINHTTP.dll","symbol":"WinHttpSendRequest"})
    ));
    assert!(image::http_import(
        &json!({"module":"WS2_32.dll","symbol":{"ordinal":19}})
    ));
    assert!(!image::http_import(
        &json!({"module":"Qt5Widgets.dll","symbol":"isSliderDown"})
    ));
    Ok(())
}

#[test]
fn playtest_discovery_follows_unwindless_tail_thunks() -> Result<()> {
    let (mut bytes, sections) = fixture(0);
    bytes[0x200] = 0xe9;
    bytes[0x201..0x205].copy_from_slice(&(0x1100i32 - 0x1205).to_le_bytes());
    let pe = PeImage {
        bytes: &bytes,
        image_base: 0x140000000,
        sections,
        image_stamp: [0; 3],
    };
    let index = image::index(&pe, &BTreeSet::new())?;
    assert_eq!(image::code_range(&pe, &index, 0x1200)?, (0x1200, 0x1205));
    let edges = image::leaf_calls(&bytes[0x200..0x205], 0x1200, 0x1205);
    assert_eq!(edges.len(), 1);
    assert_eq!(edges[0].target, 0x1100);
    Ok(())
}

#[test]
fn playtest_discovery_links_code_globals_to_live_objects_without_invocation() -> Result<()> {
    let (bytes, sections) = fixture(0);
    let pe = PeImage {
        bytes: &bytes,
        image_base: 0x140000000,
        sections,
        image_stamp: [0; 3],
    };
    let mut loaded = vec![0u8; 0x9000];
    let base = loaded.as_ptr() as usize;
    for section in &pe.sections {
        loaded[section.virtual_address..section.virtual_address + section.raw_size]
            .copy_from_slice(&bytes[section.raw_offset..section.raw_offset + section.raw_size]);
    }
    put_u64(&mut loaded, 0x3200, base + 0x3100);
    put_u64(&mut loaded, 0x3208, base + 0x1100);
    put_u64(&mut loaded, 0x5000, base + 0x5600);
    put_u64(&mut loaded, 0x5600, base + 0x3208);
    let studio = ModuleEntry {
        base,
        size: loaded.len(),
        name: "fixture.exe".into(),
        path: PathBuf::new(),
    };
    let memory = ProcessMemory::open_read_only(std::process::id())?;
    let rows = live::referenced_objects(
        &memory,
        &studio,
        &pe,
        &[json!({"ripTarget":0x5000}), json!({"ripTarget":0x1000})],
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["object"], base + 0x5600);
    assert_eq!(rows[0]["type"], ".?AVPlayController@RBX@@");
    assert_eq!(rows[0]["storage"], "pointer");
    assert!(!memory.verified.get());
    Ok(())
}

#[test]
fn playtest_command_line_exposes_only_allowlisted_nonsecret_values() {
    let report = live::command_line(
        r#""C:\Studio Folder\RobloxStudioBeta.exe" -task StartServer -placeId 12 -universeId 7 -localPlaceFile "E:\private location\source.rbxl" -authenticationTicket secret -jobId private-job -studiotestservicemode -unknown private-data"#,
    );
    let text = report.to_string();
    for forbidden in [
        "secret",
        "private",
        "Studio Folder",
        "source.rbxl",
        "unknown\"",
    ] {
        assert!(!text.contains(forbidden), "{forbidden}: {text}");
    }
    assert!(text.contains("StartServer") && text.contains("12") && text.contains("7"));
    assert_eq!(report["arguments"].as_array().unwrap().len(), 7);
}

#[test]
fn playtest_cli_requires_pid_and_forbids_launch_or_unbounded_expansion() {
    let parse = |args: Vec<&str>| crate::cli::command().try_get_matches_from(args);
    assert!(
        parse(vec![
            "rbx",
            "native-probe",
            "--pid",
            "123",
            "--playtest-discovery",
            "--playtest-http",
            "--playtest-rva",
            "0x1000",
            "--playtest-depth",
            "3"
        ])
        .is_ok()
    );
    for args in [
        vec!["rbx", "native-probe", "--pid", "123", "--playtest-http"],
        vec![
            "rbx",
            "native-probe",
            "--launch",
            "studio.exe",
            "--file",
            "fixture.rbxl",
            "--playtest-discovery",
        ],
        vec![
            "rbx",
            "native-probe",
            "--pid",
            "123",
            "--playtest-rva",
            "0x1000",
        ],
        vec![
            "rbx",
            "native-probe",
            "--pid",
            "123",
            "--playtest-discovery",
            "--playtest-depth",
            "4",
        ],
        vec![
            "rbx",
            "native-probe",
            "--pid",
            "123",
            "--playtest-discovery",
            "--playtest-rva",
            "nope",
        ],
    ] {
        assert!(parse(args).is_err());
    }
}
