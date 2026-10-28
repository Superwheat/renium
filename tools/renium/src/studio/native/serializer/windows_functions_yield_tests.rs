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
                &mut |_, _| true,
            )
        };
        assert_eq!(analyze(false), Some(HashSet::new()));
        assert_eq!(analyze(true), None);
    }
}
