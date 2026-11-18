use super::*;

fn plain_state(count: usize) -> ServiceState {
    ServiceState {
        instances: (0..count)
            .map(|index| SnapshotInstance {
                name: "node".into(),
                class_name: "Folder".into(),
                instance_index: Some(index + 1),
                parent_index: (index != 0).then_some(1),
                debug_id: Some(format!("0_{}", index + 1)),
                ..Default::default()
            })
            .collect(),
        native_properties_by_instance: None,
        children_by_index: vec![Vec::new(); count],
        source_in_subtree: vec![false; count],
        service_root_index: 0,
        class_defaults_by_class: HashMap::new(),
        properties_default_elided: true,
        dense_index_topology: true,
    }
}

fn pools() -> [rayon::ThreadPool; 3] {
    [1, 4, 64].map(|workers| {
        rayon::ThreadPoolBuilder::new()
            .num_threads(workers)
            .build()
            .unwrap()
    })
}

fn rejected(state: &ServiceState, message: &str, pools: &[rayon::ThreadPool]) {
    let error = pools[0].install(|| {
        encode_service_settings_binary(state)
            .and_then(|bytes| decode_settings_bytecode(&bytes))
            .unwrap_err()
    });
    assert_eq!(format!("{error:#}"), message);
    assert!(!oracle(state, pools).unwrap());
}

#[test]
fn captured_conversion_preserves_decoder_collection_and_string_limits() {
    let pools = pools();
    let mut state = plain_state(1);
    for kind in [9, 21] {
        let value = if kind == 9 { json!(0.1) } else { json!(0.125) };
        state.instances[0].properties.insert(
            "Array".into(),
            Value::Array(vec![value; MAX_SETTINGS_COLLECTION_ITEMS + 1]),
        );
        let label = if kind == 21 {
            "f32 numeric array length"
        } else {
            "numeric array length"
        };
        rejected(
            &state,
            &format!("{label} exceeds safe collection limit of 500000 items"),
            &pools,
        );
    }
    state.instances[0].properties.clear();
    for part in 0..2 {
        state.instances[0].properties.insert(
            format!("Array{part}"),
            Value::Array(
                (0..260_000)
                    .map(|index| json!(format!("entry-{}", part * 260_000 + index)))
                    .collect(),
            ),
        );
    }
    rejected(
        &state,
        "string count exceeds safe collection limit of 500000 items",
        &pools,
    );
    state.instances[0].properties.clear();
    state.instances[0].properties.insert(
        "String".into(),
        json!("x".repeat(MAX_SETTINGS_STRING_BYTES + 1)),
    );
    rejected(
        &state,
        "Settings string exceeds safe size limit of 33554432 bytes",
        &pools,
    );
    state.instances[0].properties.clear();
    drop(state);
    rejected(
        &plain_state(MAX_SETTINGS_COLLECTION_ITEMS + 1),
        "instance count exceeds safe collection limit of 500000 items",
        &pools,
    );
}

#[test]
fn captured_conversion_preserves_hierarchy_and_value_errors() {
    let pools = pools();
    let mut state = plain_state(2);
    state.instances[1].parent_index = None;
    rejected(
        &state,
        "Settings bytecode must contain exactly one root, found 2",
        &pools,
    );
    let mut state = plain_state(MAX_SETTINGS_HIERARCHY_DEPTH + 1);
    for (index, row) in state.instances.iter_mut().enumerate().skip(1) {
        row.parent_index = Some(index);
    }
    rejected(
        &state,
        "Settings bytecode hierarchy exceeds safe depth of 512",
        &pools,
    );
    let mut state = plain_state(1);
    state.instances[0]
        .properties
        .insert("Vector".into(), json!({"_type":"Vector3", "x":1}));
    rejected(&state, "Expected numeric field y", &pools);
    state.instances[0].properties.clear();
    let mut value = json!(true);
    for _ in 0..MAX_SETTINGS_VALUE_DEPTH + 2 {
        value = json!([value]);
    }
    state.instances[0].properties.insert("Deep".into(), value);
    rejected(
        &state,
        "Settings bytecode value nesting exceeds safe depth of 128",
        &pools,
    );
}

#[test]
#[ignore = "The full-size shared-string reproduction needs over two GiB of RAM"]
fn captured_conversion_preserves_size_dependent_memory_budget() {
    let pools = pools();
    let mut state = plain_state(1);
    state.instances[0].properties.insert(
        "Repeated".into(),
        Value::Array((0..1_100).map(|_| json!("x".repeat(1024 * 1024))).collect()),
    );
    rejected(
        &state,
        "Settings bytecode decodes to more memory than a file of its size may take",
        &pools,
    );
}

/// One of Studio's ids that came along as text, and a typed float that is no
/// number, are written from a captured state as from its document.
#[test]
fn a_captured_state_is_written_as_its_document_is() {
    let pools = pools();
    for numeric_id in [false, true] {
        let mut state = plain_state(2);
        state.instances[1].transported_settings_id =
            Some(if numeric_id { "debug:0_2" } else { "store:row" }.into());
        if !numeric_id {
            state.native_properties_by_instance = Some(vec![
                Vec::new(),
                vec![NativeSettingsProperty {
                    name: "Number".into(),
                    value: NativeSettingsValue::Float32(f32::NAN),
                }],
            ]);
        }
        let captured = encode_service_settings_binary(&state).unwrap();
        let document = settings_bytecode_from_service_state(&state).unwrap();
        let stored = encode_settings_bytecode(&document).unwrap();
        assert!(captured == stored);
        assert!(crate::settings::codec_sim::same_document(
            &decode_settings_bytecode(&captured).unwrap(),
            &decode_settings_bytecode(&stored).unwrap(),
        ));
        assert!(oracle(&state, &pools).unwrap());
    }
}

#[test]
fn captured_conversion_keeps_raw_attributes_and_column_precedence() {
    let pools = pools();
    run_seeds(12, |seed, _| {
        let mut state = generated_state(seed, 4 + seed as usize % 8);
        for row in &mut state.instances {
            row.properties.remove("Attributes");
            row.attributes.clear();
        }
        let raw_values = [
            json!({"ColorSequence":{"keypoints":[]}}),
            json!({"ColorSequence":{"keypoints":[{"time":0.5,"color":[0.1,0.2,0.3]}]}}),
            json!({"NumberSequence":{"keypoints":[{"time":0.5,"value":1,"envelope":0.2}]}}),
            json!({"BrickColor":42}),
            json!({"Font":{"family":"asset://font","weight":"Bold","style":"Italic"}}),
            json!({"EnumItem":{"enumType":"FutureEnum","name":"Item"}}),
        ];
        for (index, value) in raw_values.into_iter().enumerate() {
            state.instances[0]
                .attributes
                .insert(format!("Raw{index}"), value);
        }
        add_native_properties(&mut state, seed);
        assert!(oracle(&state, &pools)?);
        // An accepted raw capture must also reach the direct normalization;
        // an encode/decode fallback must not conceal a second attribute model.
        captured::convert_with_budget(&state, &DecodeBudget::for_file(0))
            .map_err(|error| format!("{error:#}"))?;
        state.instances[0].attributes.clear();
        state.instances[0]
            .properties
            .insert("Attributes".into(), json!({"Held": {"Bool":true}}));
        assert!(oracle(&state, &pools)?);
        let document = settings_bytecode_from_service_state(&state).unwrap();
        assert_eq!(
            document.instances[0].attributes.get("Held"),
            Some(&json!(true))
        );
        state.instances[0]
            .attributes
            .insert("Held".into(), json!(false));
        assert!(!oracle(&state, &pools)?);
        Ok(())
    });
}

#[test]
fn generated_captures_record_service_and_document_wire_differences() {
    let pools = pools();
    let simple = plain_state(2);
    assert_eq!(
        encode_service_settings_binary(&simple).unwrap(),
        encode_settings_bytecode(&settings_bytecode_from_service_state(&simple).unwrap()).unwrap()
    );
    run_seeds(24, |seed, _| {
        for native in [false, true] {
            let mut state = generated_state(seed, 8 + seed as usize % 24);
            for row in &mut state.instances {
                row.attributes.clear();
                row.properties.remove("Attributes");
            }
            if native {
                add_native_properties(&mut state, seed);
            }
            let wire = encode_service_settings_binary(&state).unwrap();
            let expected = decode_settings_bytecode(&wire).unwrap();
            let document_wire = encode_settings_bytecode(&expected).unwrap();
            if native {
                assert_ne!(
                    wire, document_wire,
                    "a property a typed row holds twice is written twice"
                );
                assert!(
                    !pulled_digest_from_service_state(&state)
                        .unwrap()
                        .rows
                        .encodes_as_its_document()
                );
            }
            for pool in &pools {
                pool.install(|| {
                    assert_eq!(wire, encode_service_settings_binary(&state).unwrap());
                    let document = settings_bytecode_from_service_state(&state).unwrap();
                    assert_eq!(document_wire, encode_settings_bytecode(&document).unwrap());
                    assert!(crate::settings::codec_sim::same_document(
                        &document,
                        &decode_settings_bytecode(&document_wire).unwrap(),
                    ));
                });
            }
        }
        Ok(())
    });
}

#[test]
fn captured_conversion_shares_decoder_budget_charges() {
    run_seeds(24, |seed, _| {
        let mut state = generated_state(seed, 8 + seed as usize % 24);
        if seed.is_multiple_of(2) {
            add_native_properties(&mut state, seed);
        }
        let bytes = encode_service_settings_binary(&state).map_err(|error| format!("{error:#}"))?;
        let (version, payload) = decode_settings_bytecode_container(&bytes).unwrap();
        let budget = DecodeBudget::new(usize::MAX);
        let expected = decode_settings_bytecode_document(version, &payload, &budget).unwrap();
        let charge = usize::MAX - budget.remaining.load(Ordering::Relaxed);
        for limit in [charge - 1, charge, charge + 1] {
            let expected =
                decode_settings_bytecode_document(version, &payload, &DecodeBudget::new(limit));
            let actual = captured::convert_with_budget(&state, &DecodeBudget::new(limit));
            match (expected, actual) {
                (Ok(expected), Ok(actual)) => {
                    if !crate::settings::codec_sim::same_document(&expected, &actual) {
                        return Err("budget-bound conversion changed document".into());
                    }
                }
                (Err(expected), Err(actual)) => {
                    assert_eq!(format!("{expected:#}"), format!("{actual:#}"))
                }
                _ => {
                    return Err(format!(
                        "direct and wire budget acceptance differs at {limit} bytes"
                    ));
                }
            }
        }
        assert!(crate::settings::codec_sim::same_document(
            &expected,
            &settings_bytecode_from_service_state(&state).unwrap()
        ));
        Ok(())
    });
}

#[test]
#[ignore = "The near-container-limit reproduction allocates several hundred MiB"]
fn captured_conversion_uses_wire_fallback_when_the_size_bound_is_inconclusive() {
    let count = 400_000;
    let width = MAX_SETTINGS_BYTECODE_BYTES / count - 11;
    let mut state = plain_state(1);
    state.instances[0].properties.insert(
        "Wide".into(),
        Value::Array(
            (0..count)
                .map(|index| json!(format!("{index:0width$}")))
                .collect(),
        ),
    );
    let error = captured::convert_with_budget(&state, &DecodeBudget::for_file(0)).unwrap_err();
    assert_eq!(
        error.to_string(),
        "Decoded settings bytecode payload exceeds safe size limit of 134217728 bytes"
    );
    assert!(oracle(&state, &pools()).unwrap());
}
