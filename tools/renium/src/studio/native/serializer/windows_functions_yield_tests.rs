use super::*;

fn checked(bytes: &[u8], entry: u64, argument: usize, table: u64) -> Option<usize> {
    dispatch_field_checked(
        entry,
        &mut |address| (address == entry).then(|| bytes.to_vec()),
        false,
        &mut |_, state| callback_table(state, argument) == Some(table),
    )
}

fn dispatcher(field: i32, buffer: u8, argument: usize) -> Vec<u8> {
    let mut code = vec![
        0x48,
        0x89,
        0xcb, // mov rbx,rcx (descriptor)
        0x48,
        0x89,
        0xd6, // mov rsi,rdx (instance)
        0x48,
        0x81,
        0xec,
        0x00,
        0x02,
        0x00,
        0x00, // sub rsp,512
        0x48,
        0x8d,
        0x05,
        0xec,
        0x0f,
        0x00,
        0x00, // lea rax,[rip] => 0x1000
        0x48,
        0x89,
        0x44,
        0x24,
        buffer, // mov [rsp+buffer],rax (vtable)
        0x4c,
        0x8d,
        0x4c,
        0x24,
        buffer, // lea r9,[rsp+buffer]
        0x4c,
        0x89,
        0x4c,
        0x24,
        buffer + 56, // mov [rsp+buffer+56],r9
    ];
    if argument == 5 {
        code.extend([0x4c, 0x89, 0x4c, 0x24, 0x28]); // fifth stack slot
    }
    code.extend([0x48, 0x63, 0x8b]); // movsxd rcx,[rbx+adjustment]
    code.extend((field + 8).to_le_bytes());
    code.extend([0x48, 0x01, 0xf1, 0x48, 0x8b, 0x83]); // add rcx,rsi; mov rax,[rbx+field]
    code.extend(field.to_le_bytes());
    code.extend([0xff, 0xd0, 0xc3]); // call rax; ret
    code
}

#[test]
fn receiver_binding_does_not_certify_yield_callbacks() {
    for field in [0x88, 0x300, 0x2000] {
        let mut bytes = dispatcher(field, 0x40, 5);
        bytes[34] = 0x40 + 24; // A different callable layout, same receiver.
        assert_eq!(
            dispatch_field(0, |at| (at == 0).then(|| bytes.clone())),
            Some(field as usize)
        );
        assert_eq!(checked(&bytes, 0, 5, 0x1000), None);
    }
}

#[test]
fn yield_callback_tracks_stack_arguments_and_relocated_fields() {
    for field in [0x88, 0xa8, 0x300, 0x2000] {
        for buffer in [0x38, 0x40] {
            for argument in [3, 5] {
                let bytes = dispatcher(field, buffer, argument);
                assert_eq!(checked(&bytes, 0, argument, 0x1000), Some(field as usize));
                assert_eq!(checked(&bytes, 0, 4, 0x1000), None);
                let mut wrong_offset = bytes.clone();
                wrong_offset[34] = buffer + 24;
                assert_eq!(checked(&wrong_offset, 0, argument, 0x1000), None);
                let mut overwritten = bytes;
                overwritten.splice(35..35, [0x4d, 0x31, 0xc9]); // xor r9,r9
                assert_eq!(checked(&overwritten, 0, argument, 0x1000), None);
            }
        }
    }
}

#[test]
fn yield_callback_rejects_unproven_paths_and_unmodelled_mutation() {
    let code = dispatcher(0xa8, 0x40, 3);
    for at in [13usize, 20, 25, 30] {
        let mut branched = code.clone();
        // A conditional path skips the next constructor operation.
        branched.splice(at..at, [0x74, if at == 13 { 7 } else { 5 }]);
        assert_eq!(checked(&branched, 0, 3, 0x1000), None);
    }
    let mut escaped = code.clone();
    escaped.splice(35..35, [0x4c, 0x89, 0xc9, 0xe8, 0, 0, 0, 0]);
    assert_eq!(checked(&escaped, 0, 3, 0x1000), None);
    for end in [13, 20, 35, code.len() - 1] {
        assert_eq!(checked(&code[..end], 0, 3, 0x1000), None);
    }
}

#[test]
fn optional_constructor_fetch_does_not_poison_a_reflection_binding() {
    let mut code = dispatcher(0xa8, 0x40, 3);
    // An unrelated call with two stack addresses is only a constructor
    // candidate. Its absent/unverifiable code is not a binding dependency.
    code.splice(
        13..13,
        [
            0x48, 0x8d, 0x4c, 0x24, 0x20, 0x48, 0x8d, 0x54, 0x24, 0x28, 0xe8, 0, 0x10, 0, 0,
        ],
    );
    let mut required_error = false;
    let mut optional_fetches = 0;
    let field = dispatch_field_fetch(
        0,
        &mut |at, required| {
            if at == 0 {
                return Some(code.clone());
            }
            required_error |= required;
            optional_fetches += usize::from(!required);
            None
        },
        false,
        0,
        &mut |_, _| true,
    );
    assert_eq!(field, Some(0xa8));
    assert!(!required_error);
    assert!(optional_fetches > 0);
}

#[test]
fn continuation_analysis_distinguishes_incomplete_methods_and_interior_aliases() {
    let analyze = |code: Vec<u8>| {
        dispatch_analysis(
            0,
            &mut |at, _| (at == 0).then(|| code.clone()),
            false,
            0,
            &mut |_, _| true,
        )
    };
    assert_eq!(analyze(vec![0xc3]), Some(HashSet::new()));
    assert_eq!(analyze(vec![0xeb, 0xff]), None);
    for offset in [0, 8, 24, 56] {
        let mut code = dispatcher(0xa8, 0x40, 3);
        code.splice(
            35..35,
            [
                0x49, 0x8d, 0x49, offset, // lea rcx,[r9+offset]
                0xe8, 0, 0x10, 0, 0, // unknown call
                0x4c, 0x8d, 0x4c, 0x24, 0x40,
            ],
        ); // restore callback argument
        assert_eq!(checked(&code, 0, 3, 0x1000), None);
    }
}

#[test]
fn continuation_analysis_follows_verified_unwind_fragments_and_refuses_missing_ones() {
    let rows: Vec<serde_json::Value> = serde_json::from_str(include_str!(
        "../../../../native/tests/windows-yield-fragments.json"
    ))
    .unwrap();
    for slide in [0, 0x40000000] {
        let entry = 0xfcc640 + slide;
        let analyze = |missing: bool| {
            dispatch_analysis(
                entry,
                &mut |at, _| {
                    rows.iter().find_map(|row| {
                        let rva = row["entry"].as_u64().unwrap();
                        (at == rva + slide && !(missing && rva == 0x7a45df))
                            .then(|| hex(row["hex"].as_str().unwrap()).unwrap())
                    })
                },
                false,
                0,
                &mut |_, _| true,
            )
        };
        assert_eq!(analyze(false), Some(HashSet::new()));
        assert_eq!(analyze(true), None);
    }
}

#[test]
fn tagged_consumers_validate_invocation_ownership_and_changed_layouts() {
    for (role, entry, encoded) in [
        (
            0,
            0x775f30,
            "4889542410534883ec50488b0537e0400c4833c44889442440488bda4c8bc14889542438488b491848f7c1f8ffffff74340fb6c1d0f8a80175034d8b004883e1f8488b01498bc8ffd090488bcbe86ed0feff488b4c24404833cce8e1c604074883c4505bc3488d4c2420e83129ffff488d1552cf2a0c488d4c2420e830e00407cc",
        ),
        (
            1,
            0x775fc0,
            "48895c2408488974242057415641574883ec20488bda488bf9488b4a18488bc14883e0fc4c8bc04983e0f87420d0f9f6c101743ec0f802f6d0a801742c0f10020f1107f20f104a10f20f114f10488b431848894718488b5c2440488b7424584883c420415f415e5fc3488bcf41ff5008ebdb4c8b72084c8b7a104c897c2448498bd7498bcee89697f302488bf04889442450488b43184883e0fc0fb6c8c0f902f6d1f6c101741b4d8bc6488b13488bcee87ddf04074889374c8977084c897f10eb8b4883e0f8488b4008488b13488bceffd0904889374c8977084c897f10e96affffff",
        ),
        (
            3,
            0x770bf0,
            "48895c2408574883ec204c8b4118498bc04883e0fc48a9f8ffffff745e0fb6d0c0fa02f6d24533c9f6c2017405418bc1eb084883e0f8488b401841d0f841f6c00174174c8949184885c0742fffd090488b5c24304883c4205fc3488b19488b79104c8949184885c07405488bcbffd0488bd7488bcbe8b6ebf30290488b5c24304883c4205fc3",
        ),
    ] {
        let bytes = hex(encoded).unwrap();
        assert!(callable_consumer(&bytes, entry, role), "role {role}");
        for wrong_role in [0, 1, 3] {
            if role != wrong_role {
                assert!(!callable_consumer(&bytes, entry, wrong_role));
            }
        }
        // The first load of the callable's tag must be from the helper ABI field.
        let mut wrong = bytes.clone();
        let tag = wrong
            .windows(4)
            .position(|w| {
                matches!(
                    w,
                    [0x48, 0x8b, 0x49, 24] | [0x48, 0x8b, 0x4a, 24] | [0x4c, 0x8b, 0x41, 24]
                )
            })
            .unwrap();
        wrong[tag + 3] = 16;
        assert!(!callable_consumer(&wrong, entry, role));
        assert!(!callable_consumer(&bytes[..tag], entry, role));
    }
}

fn extra_parameter(
    entry: u64,
    count: i64,
    code: &mut impl FnMut(u64) -> Option<Vec<u8>>,
) -> Vec<u8> {
    let bytes = code(entry).unwrap();
    let initial = State {
        registers: HashMap::from([(Register::RSP, Value::Pointer(Storage::Stack, 0))]),
        stack: BTreeMap::new(),
        lua_callback: false,
        zero: None,
        complete_receiver: false,
    };
    let analysis = flow(entry, initial, &mut |at, _| code(at), &mut 32768).unwrap();
    let mut changed = bytes.clone();
    let mut decoder = Decoder::with_ip(64, &bytes, entry, DecoderOptions::NONE);
    while decoder.can_decode() {
        let i = decoder.decode();
        if i.mnemonic() != Mnemonic::Mov || i.op1_kind() != OpKind::Memory {
            continue;
        }
        let Some(state) = analysis.states.get(&i.ip()) else {
            continue;
        };
        if state.address(&i) != Some((Storage::Stack, count * 8)) {
            continue;
        }
        // Same instructions/branches, but now the body consumes an appended
        // native parameter. Existing callback positions at dispatch still pass.
        let offsets = decoder.get_constant_offsets(&i);
        assert_eq!(offsets.displacement_size(), 4);
        let offset = (i.ip() - entry) as usize + offsets.displacement_offset();
        changed[offset..offset + 4]
            .copy_from_slice(&((i.memory_displacement64() as i32 + 8).to_le_bytes()));
        break;
    }
    assert_ne!(changed, bytes);
    changed
}

// Leave the valid consumer in the graph, but stop passing our callback to it.
fn disconnect_invocation(bytes: &mut [u8], entry: u64, consumer: u64) {
    let mut previous = None;
    let instructions: Vec<_> = Decoder::with_ip(64, bytes, entry, DecoderOptions::NONE)
        .into_iter()
        .collect();
    for i in instructions {
        if i.flow_control() == FlowControl::Call && i.near_branch_target() == consumer {
            let before: Instruction = previous.unwrap();
            let start = (before.ip() - entry) as usize;
            bytes[start..(before.next_ip() - entry) as usize].fill(0x90);
            bytes[start..start + 3].copy_from_slice(&[0x48, 0x31, 0xc9]); // xor rcx,rcx
        }
        previous = Some(i);
    }
}

fn captured_fixture() -> serde_json::Result<serde_json::Value> {
    serde_json::from_str(include_str!(
        "../../../../native/tests/windows-http-yield.json"
    ))
}

#[test]
fn captured_http_yield_contract() -> Result<()> {
    // Captured code is only fixture data; production discovers every address.
    let fixture = captured_fixture()?;
    let mut code = |at: u64| hex(fixture["code"][at.to_string()].as_str()?).ok();
    let mover = 0x8164b0;
    let mut bytes = code(mover).unwrap();
    assert!(callable_move(&bytes, mover, &mut code));
    // A changed move slot must refuse even when copy/invoke/destroy still match.
    for at in 0..bytes.len().saturating_sub(3) {
        if bytes[at..at + 4] == [0x4c, 0x8b, 0x40, 16] {
            bytes[at + 3] = 8;
        }
    }
    assert!(!callable_move(&bytes, mover, &mut code));
    let methods = fixture["methods"].as_array().unwrap();
    for (index, entry) in fixture["members"].as_array().unwrap().iter().enumerate() {
        let entry = entry.as_u64().unwrap();
        assert!(incoming_ownership(entry, if index < 2 { 5 } else { 7 }, &mut code).is_err());
        for slide in [0, 0x140000000] {
            let mut operation = |at: u64| {
                fixture["operations"][at.checked_sub(slide)?.to_string()]
                    .as_u64()
                    .map(|rva| rva + slide)
            };
            qualify(
                Member::of(if index < 2 { 1 } else { 2 }, None),
                0xa8,
                entry + slide,
                &[methods[usize::from(index >= 2)].as_u64().unwrap() + slide],
                &mut |at| {
                    let at = at.checked_sub(slide)?;
                    hex(fixture["code"][at.to_string()].as_str()?).ok()
                },
                &mut operation,
                &mut |at| {
                    fixture["byteCopies"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|v| v.as_u64() == at.checked_sub(slide))
                },
            )
            .with_context(|| format!("captured member=0x{entry:x} slide=0x{slide:x}"))?;
            let changed = extra_parameter(entry, if index < 2 { 6 } else { 8 }, &mut code);
            let error = qualify(
                Member::of(if index < 2 { 1 } else { 2 }, None),
                0xa8,
                entry + slide,
                &[methods[usize::from(index >= 2)].as_u64().unwrap() + slide],
                &mut |at| {
                    let at = at.checked_sub(slide)?;
                    if at == entry {
                        Some(changed.clone())
                    } else {
                        code(at)
                    }
                },
                &mut operation,
                &mut |at| {
                    fixture["byteCopies"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|v| v.as_u64() == at.checked_sub(slide))
                },
            )
            .unwrap_err();
            assert!(
                format!("{error:#}").contains("yield-native-arity: access beyond"),
                "{error:#}"
            );
            let error = qualify(
                Member::of(if index < 2 { 1 } else { 2 }, None),
                0xa8,
                entry + slide,
                &[methods[usize::from(index >= 2)].as_u64().unwrap() + slide],
                &mut |at| {
                    let original = at.checked_sub(slide)?;
                    let mut bytes = code(original)?;
                    disconnect_invocation(&mut bytes, at, 7823152 + slide);
                    Some(bytes)
                },
                &mut operation,
                &mut |at| {
                    fixture["byteCopies"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|v| v.as_u64() == at.checked_sub(slide))
                },
            )
            .unwrap_err();
            assert!(
                format!("{error:#}").contains("yield-callback-consumers"),
                "{error:#}"
            );
            // Changed invocation: cleanup receives another register instead of
            // the original payload. A valid callable table must not hide this.
            let error = qualify(
                Member::of(if index < 2 { 1 } else { 2 }, None),
                0xa8,
                entry + slide,
                &[methods[usize::from(index >= 2)].as_u64().unwrap() + slide],
                &mut |at| {
                    let original = at.checked_sub(slide)?;
                    let mut bytes = code(original)?;
                    if original == 7823152 {
                        let offset = bytes
                            .windows(5)
                            .position(|w| w == [0x90, 0x48, 0x8b, 0xcb, 0xe8])?;
                        bytes[offset + 3] = 0xcf;
                    }
                    Some(bytes)
                },
                &mut operation,
                &mut |at| {
                    fixture["byteCopies"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|v| v.as_u64() == at.checked_sub(slide))
                },
            )
            .unwrap_err()
            .to_string();
            assert!(error.contains("yield-callback-consumers"), "{error}");
            // Keep both callbacks and their consumer intact, but stop storing
            // the copied byte count in the constructed payload's length field.
            let error = qualify(
                Member::of(if index < 2 { 1 } else { 2 }, None),
                0xa8,
                entry + slide,
                &[methods[usize::from(index >= 2)].as_u64().unwrap() + slide],
                &mut |at| {
                    let original = at.checked_sub(slide)?;
                    let mut bytes = code(original)?;
                    if original == 0x76f550 {
                        let offset = bytes
                            .windows(4)
                            .position(|w| w == [0x48, 0x89, 0x71, 0x10])?;
                        bytes[offset + 3] = 8;
                    }
                    Some(bytes)
                },
                &mut operation,
                &mut |at| {
                    fixture["byteCopies"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|v| v.as_u64() == at.checked_sub(slide))
                },
            )
            .unwrap_err();
            assert!(
                format!("{error:#}").contains("yield-callback-consumers"),
                "{error:#}"
            );
        }
    }
    Ok(())
}

#[test]
fn actual_callback_operation_rejects_additional_inputs() {
    let operation = |bytes: &[u8]| {
        bound_invocations(
            HashSet::from([0x1000]),
            &mut |at| (at == 0x2000).then(|| bytes.to_vec()),
            &mut |at| (at == 0x1000).then_some(0x2000),
        )
    };
    operation(&hex("4889d0c3").unwrap()).unwrap(); // mov rax,rdx; ret
    // Extra integer/stack arguments and floating-point input banks all refuse.
    for bytes in [
        "4c89c0c3",
        "488b442428c3",
        "0f28c2c3",
        "f20f2cc1c3",
        "85c974034531c04c89c0c3", // R8 overwritten on only one branch.
        "4889e04801c8488b00c3",   // Stack alias transformed by an unknown input.
        "488b00c3",               // Memory through an unprovided/lost register.
    ] {
        let error = operation(&hex(bytes).unwrap()).unwrap_err();
        assert!(format!("{error:#}").contains("yield-callback-bound-invoke"));
        assert!(format!("{error:#}").contains("yield-native-arity"));
    }
}

#[test]
fn changed_callback_payloads_refuse_and_failed_qualification_clears_reserved() {
    // These consumers forward a pointer just like the old accepted invocation,
    // including a consumer with an extra integer argument. Neither proves a
    // single by-value string, and the table carries no success/error type.
    for encoded in [
        "488b41184883e0f8ff10c3",
        "41b807000000488b41184883e0f8ff10c3",
    ] {
        let bytes = hex(encoded).unwrap();
        assert!(!callable_consumer(&bytes, 0, 0));
    }
    let mut input = crate::studio::native::serializer::functions::input(
        "HttpRbxApiService",
        "GetAsync",
        &[serde_json::json!("/x")],
    )
    .unwrap();
    input.bytes[52..56].copy_from_slice(&2u32.to_le_bytes());
    let error = input
        .qualify_yield(|| {
            qualify(
                Member::of(1, None),
                8,
                0,
                &[0],
                &mut |_| Some(vec![0xc3]),
                &mut |_| None,
                &mut |_| false,
            )
        })
        .unwrap_err()
        .to_string();
    assert!(error.contains("yield-callback-construction"));
    assert_eq!(&input.bytes[52..56], &[0; 4]);
}

#[test]
fn captured_string_construction_covers_both_response_paths() -> Result<()> {
    let fixture = captured_fixture()?;
    let mut code = |at: u64| hex(fixture["code"][at.to_string()].as_str()?).ok();
    let entry = 0x4ca6540;
    let initial = State {
        registers: HashMap::from([(Register::RSP, Value::Pointer(Storage::Stack, 0))]),
        stack: BTreeMap::from([
            (40, (8, Value::Continuation(0))),
            (48, (8, Value::Continuation(1))),
        ]),
        lua_callback: false,
        zero: None,
        complete_receiver: false,
    };
    let analysis =
        flow(entry, initial, &mut |at, _| code(at), &mut 32768).context("handler flow")?;
    let (pc, _) = analysis
        .instructions
        .iter()
        .find(|(_, i)| i.flow_control() == FlowControl::Call && i.near_branch_target() == 0x775f30)
        .unwrap();
    assert_eq!(
        analysis.states[pc].register(Register::RCX),
        Some(Value::Continuation(4))
    );
    let contract = strings::payload(&analysis, entry, *pc, 0x775f30, &mut code, &mut |at| {
        fixture["byteCopies"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v.as_u64() == Some(at))
    });
    assert!(
        contract.is_some(),
        "fresh response/error string construction was not proved"
    );
    // The layout proved on Studio's code is the one the helper's strings have.
    let helper = crate::system::tools::hidden_command(concat!(
        env!("OUT_DIR"),
        "/renium-functions-test.exe"
    ))
    .arg("--contract")
    .output()?;
    assert_eq!(
        contract.map(|layout| layout.to_string()).as_deref(),
        Some(String::from_utf8(helper.stdout)?.trim())
    );
    // The builder's allocating path ends its copy with a byte that is not the
    // terminator: that path no longer builds a string.
    let (fragment, terminator) = (0x76f5ab, 0x76f644 - 0x76f5ab);
    let mut changed = code(fragment).unwrap();
    assert_eq!(
        changed[terminator..terminator + 5],
        [0x41, 0xc6, 0x04, 0x36, 0x00]
    );
    changed[terminator + 4] = 1;
    let mut code = |at: u64| {
        if at == fragment {
            Some(changed.clone())
        } else {
            code(at)
        }
    };
    let analysis = flow(
        entry,
        analysis.states[&entry].clone(),
        &mut |at, _| code(at),
        &mut 32768,
    )
    .context("handler flow")?;
    assert!(
        strings::payload(&analysis, entry, *pc, 0x775f30, &mut code, &mut |at| {
            fixture["byteCopies"]
                .as_array()
                .unwrap()
                .iter()
                .any(|v| v.as_u64() == Some(at))
        })
        .is_none()
    );
    Ok(())
}

#[test]
fn captured_success_continuation_cannot_be_substituted() -> Result<()> {
    let fixture = captured_fixture()?;
    let error = qualify(
        Member::of(1, None),
        0xa8,
        fixture["members"][1].as_u64().unwrap(),
        &[fixture["methods"][0].as_u64().unwrap()],
        &mut |at| {
            let mut bytes = hex(fixture["code"][at.to_string()].as_str()?).ok()?;
            if at == 0x4ca6540 {
                let offset = bytes
                    .windows(4)
                    .position(|w| w == [0x48, 0x8b, 0xcf, 0xe9])?;
                bytes[offset..offset + 3].copy_from_slice(&[0x48, 0x31, 0xc9]);
            }
            Some(bytes)
        },
        &mut |at| fixture["operations"][at.to_string()].as_u64(),
        &mut |at| {
            fixture["byteCopies"]
                .as_array()
                .unwrap()
                .iter()
                .any(|v| v.as_u64() == Some(at))
        },
    )
    .unwrap_err();
    assert!(
        format!("{error:#}").contains("yield-callback-consumers"),
        "{error:#}"
    );
    Ok(())
}

#[test]
fn captured_members_native_arity() -> Result<()> {
    let fixture = captured_fixture()?;
    for (index, argument) in [(0, 4), (1, 4), (2, 6), (3, 6)] {
        incoming_ownership(
            fixture["members"][index].as_u64().unwrap(),
            argument,
            &mut |at| hex(fixture["code"][at.to_string()].as_str()?).ok(),
        )?;
    }
    Ok(())
}

#[test]
fn native_arity_keeps_local_addresses_apart_from_parameters() {
    let arity = |bytes: &str| {
        let bytes = hex(bytes).unwrap();
        let initial = State {
            registers: HashMap::from([(Register::RSP, Value::Pointer(Storage::Stack, 0))]),
            stack: BTreeMap::new(),
            lua_callback: false,
            zero: None,
            complete_receiver: false,
        };
        let analysis = flow(
            0x2000,
            initial,
            &mut |at, _| (at == 0x2000).then(|| bytes.clone()),
            &mut 32768,
        )
        .unwrap();
        verify_arity(0x2000, &analysis, 4)
    };
    for bytes in [
        "4883ec28c6440c08004883c428c3", // sub rsp,28h; mov byte [rsp+rcx+8],0: an indexed local.
        "4883ec28488d4c2420e800000000488b40084883c428c3", // What a callee returns of a local it was handed.
        "488d5424f0488b0411c3", // lea rdx,[rsp-10h]; mov rax,[rcx+rdx]: a local as the index.
        "488d542420c3",         // lea rdx,[rsp+20h]: the address of the fourth parameter's home.
        "4883ec28488d7c242031c0b908000000f3aac607004883c428c3", // A local buffer filled by rep stosb, then written past its end.
    ] {
        arity(bytes).unwrap();
    }
    for (bytes, refusal) in [
        ("488b442430c3", "access beyond"), // mov rax,[rsp+30h]: a fifth parameter.
        ("488d542430c3", "address beyond"), // lea rdx,[rsp+30h]: its address, for a callee.
        ("488d7c242048ab488b07c3", "address beyond"), // lea rdi,[rsp+20h]; stosq; mov rax,[rdi]: stepped past the last one.
        ("488d4424204889442408488344240810c3", "address beyond"), // The same address made inside a stack slot.
        (
            "488d442420488d7c2408ab48c1e820ab488b4c2408488b4140c3",
            "lost memory origin",
        ), // An address stored in two halves by stosd.
        (
            "488d442420488d7c24084885c97405488d7c241048ab488b4c2410488b4140c3",
            "unproved memory provenance",
        ), // stosq to one of two slots.
        (
            "488d442420488d7c2410fd48ab48abfc488b4c2408488b4140c3",
            "unproved memory provenance",
        ), // std: stores run downward.
        (
            "4883ec10488d4424304889442404595a480facd120488b4140c3",
            "lost memory origin",
        ), // pop of half a stored address.
        ("488d7c24e0488d742408f3a4c3", "access beyond"), // lea rdi,[rsp-20h]; lea rsi,[rsp+8]; rep movsb: a copy out of the parameters.
        ("4885c97406488d0424eb029090488b00c3", "lost memory origin"), // rax set on one path only, then read through.
        ("4889e04883c030c3", "address beyond"), // mov rax,rsp; add rax,30h: the same, computed.
        ("488b44cc28c3", "lost memory origin"), // mov rax,[rsp+rcx*8+28h]: indexed from the parameters.
        (
            "4883ec28488d4c2430e800000000488b004883c428c3",
            "lost memory origin",
        ), // A callee handed a home slot.
        ("488d5424f0488b04d1c3", "lost memory origin"), // mov rax,[rcx+rdx*8]: a scaled local.
    ] {
        let error = format!("{:#}", arity(bytes).unwrap_err());
        assert!(
            error.contains("yield-native-arity") && error.contains(refusal),
            "{bytes}: {error}"
        );
    }
}

#[test]
fn a_copy_export_may_differ_from_its_file_only_by_the_jump_the_runtime_picked() {
    let file = hex("e91bf0ffffcccccccccccccc").unwrap();
    let live = hex("e9fbd9ffffcccccccccccccc").unwrap();
    // A jump leads on whether or not the runtime moved it; code stands.
    assert_eq!(copy_entry(&file, &file, 0x13010), Some(0x12030));
    assert_eq!(copy_entry(&live, &file, 0x13010), Some(0x10a10));
    let code = hex("488bc14c8d15c6dffeff4983").unwrap();
    assert_eq!(copy_entry(&code, &code, 0x10a10), Some(0x10a10));
    // What follows a jump never runs: a neighbor moved as well changes nothing.
    let mut beside = live.clone();
    beside[7] = 0x90;
    assert_eq!(copy_entry(&beside, &file, 0x13010), Some(0x10a10));
    // A jump where the file has code, changed code, a jump to itself, a jump
    // to before the image.
    assert_eq!(copy_entry(&live, &code, 0x13010), None);
    assert_eq!(copy_entry(&code[1..], &code[..11], 0x10a10), None);
    let own = hex("e9fbffffffcccccccccccccc").unwrap();
    assert_eq!(copy_entry(&own, &file, 0x13010), None);
    let far = hex("e900000080cccccccccccccc").unwrap();
    assert_eq!(copy_entry(&far, &file, 0x13010), None);
}

// The captured fixture holds only the functions one search visited. This runs
// the same members against every function the executable holds, as a live
// Studio serves them: a search that passes on the fixture can still run out of
// its limit there.
#[test]
#[ignore = "reads RENIUM_STUDIO_IMAGE, the Studio executable the fixture was captured from"]
fn installed_image_http_yield_contract() -> Result<()> {
    let path = std::env::var("RENIUM_STUDIO_IMAGE").context("RENIUM_STUDIO_IMAGE")?;
    let bytes = std::fs::read(path)?;
    let image = PeImage::parse(&bytes)?;
    let (text, table) = (image.section(b".text")?, image.section(b".pdata")?);
    let ranges = (table.raw_offset..table.raw_offset + table.raw_size)
        .step_by(12)
        .map(|at| {
            Ok((
                read_u32(&bytes, at)? as usize,
                read_u32(&bytes, at + 4)? as usize,
            ))
        })
        .collect::<Result<Vec<_>>>()?;
    let read = |rva: usize, len: usize| {
        let offset = rva.checked_sub(text.virtual_address)?;
        (offset + len <= text.raw_size).then(|| bytes[text.raw_offset + offset..][..len].to_vec())
    };
    let mut code = |at: u64| {
        let rva = at as usize;
        let index = ranges.partition_point(|(start, _)| *start <= rva);
        match index.checked_sub(1).map(|index| ranges[index]) {
            Some((start, end)) if start == rva => {
                (end - start <= 65536).then(|| read(rva, end - start))?
            }
            _ => {
                let bytes = read(rva, 256)?;
                leaf_dispatch_length(&bytes).map(|length| bytes[..length].to_vec())
            }
        }
    };
    let mut operation = |at: u64| {
        let offset = image.rva_to_offset(at as usize).ok()?;
        let value = u64::from_le_bytes(bytes.get(offset..offset + 8)?.try_into().ok()?);
        value.checked_sub(image.image_base as u64)
    };
    let fixture = captured_fixture()?;
    let methods = fixture["methods"].as_array().unwrap();
    let mut failed = Vec::new();
    for (index, entry) in fixture["members"].as_array().unwrap().iter().enumerate() {
        let entry = entry.as_u64().unwrap();
        let result = qualify(
            Member::of(if index < 2 { 1 } else { 2 }, None),
            0xa8,
            entry,
            &[methods[usize::from(index >= 2)].as_u64().unwrap()],
            &mut code,
            &mut operation,
            &mut |at| {
                (fixture["byteCopies"].as_array().unwrap().iter()).any(|v| v.as_u64() == Some(at))
            },
        );
        if let Err(error) = result {
            failed.push(format!("0x{entry:x}: {error:#}"));
        }
    }
    anyhow::ensure!(failed.is_empty(), "{failed:#?}");
    Ok(())
}

fn post_fixture() -> serde_json::Result<serde_json::Value> {
    serde_json::from_str(include_str!(
        "../../../../native/tests/windows-post-yield.json"
    ))
}

// Qualifies the captured post member with `change` applied to its code.
fn post_contract(
    fixture: &serde_json::Value,
    member: Member,
    slide: u64,
    change: &dyn Fn(u64, &mut Vec<u8>),
) -> Result<YieldContract> {
    qualify(
        member,
        0xa8,
        fixture["entry"].as_u64().unwrap() + slide,
        &[fixture["method"].as_u64().unwrap() + slide],
        &mut |at| {
            let at = at.checked_sub(slide)?;
            let mut bytes = hex(fixture["code"][at.to_string()].as_str()?).ok()?;
            change(at, &mut bytes);
            Some(bytes)
        },
        &mut |at| {
            fixture["operations"][at.checked_sub(slide)?.to_string()]
                .as_u64()
                .map(|rva| rva + slide)
        },
        &mut |at| {
            fixture["byteCopies"]
                .as_array()
                .unwrap()
                .iter()
                .any(|v| v.as_u64() == at.checked_sub(slide))
        },
    )
}

fn replaced(bytes: &mut [u8], from: &[u8], to: &[u8]) {
    let at = bytes
        .windows(from.len())
        .position(|window| window == from)
        .expect("the captured instruction");
    bytes[at..at + to.len()].copy_from_slice(to);
}

#[test]
fn captured_post_yield_contract() -> Result<()> {
    // The member takes the converted DataModel as it is, moves both
    // continuations into a bound closure, and answers an empty body with a
    // string it initialized itself. Captured code is only fixture data.
    let fixture = post_fixture()?;
    let member = || Member::of(5, Some(0x1f0));
    let unchanged = |_: u64, _: &mut Vec<u8>| {};
    let contract = post_contract(&fixture, member(), 0, &unchanged)?;
    let moved = post_contract(&fixture, member(), 0x140000000, &unchanged)?;
    assert_eq!(contract.layout, moved.layout);
    assert_eq!(contract.sites.len(), moved.sites.len());
    assert!(!contract.sites.is_empty());

    let refused = |member: Member, change: &dyn Fn(u64, &mut Vec<u8>)| {
        post_contract(&fixture, member, 0, change)
            .err()
            .map(|error| format!("{error:#}"))
    };
    assert!(refused(Member::of(5, Some(0x1f8)), &unchanged).is_some());
    assert!(refused(Member::of(5, None), &unchanged).is_some());
    assert!(refused(Member::of(1, None), &unchanged).is_some());
    let adjusted = Member {
        unadjusted: false,
        ..member()
    };
    assert!(
        refused(adjusted, &unchanged).is_some(),
        "a function the dispatch calls another way than the helper will"
    );
    // The answer handler: an empty string with another capacity, and one
    // whose size was never stored, are not strings the helper may read.
    let handler = 0x776330;
    let changes: [(&[u8], &[u8]); 3] = [
        (
            &[0x48, 0xc7, 0x45, 0xd0, 0x0f, 0, 0, 0],
            &[0x48, 0xc7, 0x45, 0xd0, 0x1f, 0, 0, 0],
        ),
        (&[0x48, 0x89, 0x5d, 0xc8], &[0x90; 4]),
        (&[0x48, 0x89, 0x5d, 0xb8], &[0x90; 4]),
    ];
    for (from, to) in changes {
        let change = |at: u64, bytes: &mut Vec<u8>| {
            if at == handler {
                replaced(bytes, from, to);
            }
        };
        let error = refused(member(), &change).expect("a changed handler");
        assert!(error.contains("yield-callback-consumers"), "{error}");
    }
    Ok(())
}
