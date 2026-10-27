use super::*;

fn checked(bytes: &[u8], entry: u64, argument: usize, table: u64) -> Option<usize> {
    dispatch_field_checked(
        entry,
        &mut |address| (address == entry).then(|| bytes.to_vec()),
        false,
        &mut |_, state| callback_table(state, argument) == Some(table),
    )
}

#[test]
fn studio_yield_dispatch_distinguishes_both_hidden_continuation_abis() {
    let fixtures: serde_json::Value = serde_json::from_str(include_str!(
        "../../../../native/tests/windows-yield-dispatch.json"
    ))
    .unwrap();
    for fixture in fixtures.as_array().unwrap() {
        let bytes = hex(fixture["hex"].as_str().unwrap()).unwrap();
        let entry = fixture["entry"].as_u64().unwrap();
        let field = fixture["field"].as_u64().unwrap() as usize;
        let table = fixture["errorCallbackTable"].as_u64().unwrap();
        // The old probe found the correct member on BOTH builds. Its RTTI
        // does not encode the hidden continuations and cannot certify them.
        assert_eq!(
            dispatch_field(entry, |at| (at == entry).then(|| bytes.clone())),
            Some(field)
        );
        let expected = fixture["compatible"].as_bool().unwrap().then_some(field);
        assert_eq!(
            checked(&bytes, entry, 3, table),
            expected,
            "{}",
            fixture["build"]
        );
        // Relocate the entire image: no recorded address is an allowlist.
        assert_eq!(
            checked(&bytes, entry + 0x40000000, 3, table + 0x40000000),
            expected
        );
        assert_eq!(checked(&bytes, entry, 3, table + 8), None);
        let abi = if expected.is_some() {
            ContinuationAbi::Msvc
        } else {
            ContinuationAbi::Tagged
        };
        for slide in [0, 0x40000000] {
            assert_eq!(
                dispatch_field_checked(
                    entry + slide,
                    &mut |at| {
                        if at == entry + slide {
                            return Some(bytes.clone());
                        }
                        fixture["constructors"].as_array()?.iter().find_map(|row| {
                            (row["entry"].as_u64()? + slide == at)
                                .then(|| hex(row["hex"].as_str().unwrap()).unwrap())
                        })
                    },
                    false,
                    &mut |_, state| {
                        callback_layout(state, 2)
                            .zip(callback_layout(state, 3))
                            .is_some_and(|(success, error)| success.0 == abi && error.0 == abi)
                    }
                ),
                Some(field)
            );
        }
        if abi == ContinuationAbi::Tagged {
            for at in bytes
                .windows(4)
                .enumerate()
                .filter_map(|(at, w)| (w == [0x48, 0x83, 0xc8, 6]).then_some(at))
            {
                let mut wrong = bytes.clone();
                wrong[at + 3] = 2;
                assert_eq!(
                    dispatch_field_checked(
                        entry,
                        &mut |pos| (pos == entry).then(|| wrong.clone()),
                        false,
                        &mut |_, state| callback_layout(state, 2)
                            .zip(callback_layout(state, 3))
                            .is_some()
                    ),
                    None
                );
            }
        }
    }
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
fn yield_error_callback_requires_the_exact_msvc_signature() {
    let kind = format!(".?AV?$_Func_impl_no_alloc@V<relocated_lambda>{ERROR_CALLBACK_SUFFIX}");
    assert!(error_callback_type(&kind));
    assert!(!error_callback_type(
        &kind.replace("basic_string", "vector")
    ));
    assert!(!error_callback_type(
        &kind.replace("_Func_impl_no_alloc", "OtherCallable")
    ));
}

#[test]
fn loader_callback_uses_the_constructed_table_and_actual_argument() {
    let row: serde_json::Value = serde_json::from_str(include_str!(
        "../../../../native/tests/windows-loader-dispatch.json"
    ))
    .unwrap();
    let bytes = hex(row["hex"].as_str().unwrap()).unwrap();
    let entry = row["entry"].as_u64().unwrap();
    let table = row["table"].as_u64().unwrap();
    for slide in [0, 0x40000000] {
        assert_eq!(
            inline_callback_tables(
                entry + slide,
                |at| (at == entry + slide).then(|| bytes.clone()),
                2
            ),
            Some(vec![(table + slide) as usize])
        );
        assert_eq!(
            inline_callback_tables(
                entry + slide,
                |at| (at == entry + slide).then(|| bytes.clone()),
                1
            ),
            Some(vec![])
        );
    }
    let at = bytes
        .windows(4)
        .position(|w| w == [0x48, 0x83, 0xc8, 2])
        .unwrap();
    for tag in [0, 1, 3, 4, 5, 6, 7] {
        let mut changed = bytes.clone();
        changed[at + 3] = tag;
        assert_eq!(
            inline_callback_tables(entry, |pos| (pos == entry).then(|| changed.clone()), 2),
            Some(vec![])
        );
    }
}
