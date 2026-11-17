use super::*;
use crate::snapshot::types::NativeSettingsProperty;
use crate::tests::support::{SimRng, run_seeds};

#[path = "captured_limits_sim.rs"]
mod limits;

pub(crate) fn captured_state(document: &SettingsBytecode, native: bool) -> ServiceState {
    let mut children = vec![Vec::new(); document.instances.len()];
    let mut native_rows = Vec::new();
    let instances = document
        .instances
        .iter()
        .enumerate()
        .map(|(index, row)| {
            if let Some(parent) = row.parent_index {
                children[parent].push(index);
            }
            let mut properties = row.properties.clone();
            let native_values = if native {
                properties
                    .iter()
                    .filter_map(|(name, value)| {
                        if matches!(
                            name.as_str(),
                            "Source" | "Name" | "ClassName" | "Parent" | "RobloxLocked"
                        ) {
                            return None;
                        }
                        native_value(value).map(|value| {
                            crate::snapshot::types::NativeSettingsProperty {
                                name: name.clone(),
                                value,
                            }
                        })
                    })
                    .collect::<Vec<_>>()
            } else {
                Vec::new()
            };
            for property in &native_values {
                properties.remove(&property.name);
            }
            native_rows.push(native_values);
            SnapshotInstance {
                name: row.name.clone(),
                class_name: row.class_name.as_str().into(),
                properties,
                attributes: row.attributes.clone(),
                transported_settings_id: Some(row.settings_id.clone()),
                instance_index: Some(index + 1),
                parent_index: row.parent_index.map(|index| index + 1),
                ..Default::default()
            }
        })
        .collect();
    ServiceState {
        instances,
        native_properties_by_instance: native.then_some(native_rows),
        children_by_index: children,
        source_in_subtree: vec![false; document.instances.len()],
        service_root_index: document
            .instances
            .iter()
            .position(|row| row.parent_index.is_none())
            .unwrap_or(0),
        class_defaults_by_class: HashMap::new(),
        properties_default_elided: true,
        dense_index_topology: true,
    }
}

fn native_value(value: &Value) -> Option<NativeSettingsValue> {
    match value {
        Value::Bool(value) => Some(NativeSettingsValue::Bool(*value)),
        Value::String(value) => Some(NativeSettingsValue::String(value.clone())),
        Value::Number(value) => value
            .as_i64()
            .map(NativeSettingsValue::Int)
            .or_else(|| value.as_f64().map(NativeSettingsValue::Float64)),
        Value::Object(fields) => match fields.get("_type")?.as_str()? {
            // A payload the capture holds once for every instance that has it.
            "SharedString" | "BinaryString" => Some(NativeSettingsValue::Shared(
                std::sync::Arc::new(value.clone()),
            )),
            "Ref" => {
                settings_reference_index(fields.get("instanceIndex")?).map(NativeSettingsValue::Ref)
            }
            "EnumItem" => Some(NativeSettingsValue::Enum(
                fields.get("name")?.as_str()?.into(),
            )),
            "Vector2" => Some(NativeSettingsValue::Vector2(native_components(
                fields,
                ["x", "y"],
            )?)),
            "Vector3" => Some(NativeSettingsValue::Vector3(native_components(
                fields,
                ["x", "y", "z"],
            )?)),
            "UDim" => Some(NativeSettingsValue::UDim(native_components(
                fields,
                ["scale", "offset"],
            )?)),
            "UDim2" => Some(NativeSettingsValue::UDim2(native_components(
                fields,
                ["xScale", "xOffset", "yScale", "yOffset"],
            )?)),
            "Color3" => Some(NativeSettingsValue::Color3(native_components(
                fields,
                ["r", "g", "b"],
            )?)),
            "Rect" => Some(NativeSettingsValue::Rect(native_components(
                fields,
                ["minX", "minY", "maxX", "maxY"],
            )?)),
            "CFrame" => Some(NativeSettingsValue::CFrame(
                cframe_component_bits(value).ok()?.map(f32::from_bits),
            )),
            _ => None,
        },
        _ => None,
    }
}

fn native_components<const N: usize>(
    fields: &Map<String, Value>,
    names: [&str; N],
) -> Option<[f32; N]> {
    let mut values = [0_f32; N];
    for (index, name) in names.into_iter().enumerate() {
        values[index] = settings_number(fields.get(name)?)? as f32;
    }
    Some(values)
}

fn values(seed: u64) -> Vec<Value> {
    let mut rng = SimRng(seed);
    let mut values = vec![
        Value::Null,
        json!(false),
        json!(true),
        json!(i64::MIN),
        json!(u64::MAX),
        json!(-0.0),
        json!(1.125),
        json!(0.1),
        json!("Unicode \u{2603}\0"),
        json!([]),
        json!([1, -0.0, 0.125]),
        json!([0.1, 1e39]),
        json!([1, {"t":"numeric", "v":"inf"}, {"_type":"Float", "value":"nan"}]),
        json!([false, null, {"_type":"EnumItem", "name":"A"},
            {"_type":"EnumItem", "enumType":"FutureEnum", "name":"B"}]),
        json!({"_type":"EnumItem", "name":"Plastic", "enumType":"Material", "value":256}),
        json!({"_type":"EnumItem", "name":false}),
        json!({"_type":"BrickColor", "number":21, "discarded":true}),
        json!({"_type":"Font", "family":"asset://font", "weight":"Enum.FontWeight.Bold", "style":"Enum.FontStyle.Italic", "extra":12}),
        json!({"_type":"ColorSequence", "keypoints":[false, {}, {"time":0.5, "color":{"_type":"Color3", "r":1,"g":0.1,"b":0}, "extra":true}]}),
        json!({"_type":"NumberSequence", "keypoints":[false, {}, {"time":0.5, "value":2,"envelope":0.1, "extra":true}]}),
        json!({"_type":"NumberRange", "min":0,"max":5}),
        json!({"_type":"BinaryString", "base64":"AAEC"}),
        json!({"_type":"FutureType", "child":[null, {"_type":"EnumItem", "enumType":"FutureEnum", "name":"B"}]}),
        json!({"_type":"Ref", "instanceIndex":1}),
        json!({"Ref":{"instanceIndex":2}}),
        json!({"_type":"Ref", "settingsId":"unresolved", "pathSegments":["OtherService","A"], "pathOrdinals":[1,2]}),
        json!({"_type":"Ref"}),
        json!({"_type":"Float", "value":"-inf"}),
        json!({"_type":"Float64", "value":0.1}),
        json!({"_type":"Int64", "value":42}),
        json!({"EnumItem":{"enumType":"FutureEnum", "name":"B"}}),
        json!({"_type":"Ref", "debugId":"0_999"}),
        json!({"ColorSequence":{"keypoints":[]}}),
        json!({"NumberSequence":{"keypoints":[]}}),
        json!({"Font":{"family":"asset://font","weight":"Bold","style":"Italic"}}),
    ];
    for (kind, fields) in [
        ("Vector2", &["x", "y"][..]),
        ("Vector3", &["x", "y", "z"]),
        ("UDim", &["scale", "offset"]),
        ("UDim2", &["xScale", "xOffset", "yScale", "yOffset"]),
        ("Color3", &["r", "g", "b"]),
        ("Rect", &["minX", "minY", "maxX", "maxY"]),
    ] {
        let mut fields: Map<String, Value> = fields
            .iter()
            .enumerate()
            .map(|(index, name)| {
                let number = [
                    json!(1e39),
                    json!(-0.0),
                    json!(0.1),
                    json!({"t":"numeric", "v":"nan"}),
                ][(index + seed as usize) % 4]
                    .clone();
                ((*name).into(), number)
            })
            .collect();
        fields.insert("_type".into(), json!(kind));
        fields.insert("extra".into(), json!(rng.next()));
        let value = Value::Object(fields);
        values.push(json!({kind: value.clone()}));
        values.push(value);
    }
    values.push(
        json!({"_type":"CFrame", "components":[0.1,-0.0,1e39,1,0,0,0,1,0,0,0,1], "extra":true}),
    );
    values
}

fn native_values(seed: u64, count: usize) -> Vec<NativeSettingsValue> {
    let mut rng = SimRng(seed);
    let number = f32::from_bits(rng.next() as u32);
    vec![
        NativeSettingsValue::Bool(false),
        NativeSettingsValue::Bool(true),
        NativeSettingsValue::Int(rng.next() as i64),
        NativeSettingsValue::Float32(number),
        NativeSettingsValue::Float32(-0.0),
        NativeSettingsValue::Float32(f32::NAN),
        NativeSettingsValue::Float64(f64::from_bits(rng.next())),
        NativeSettingsValue::Float64(f64::NEG_INFINITY),
        NativeSettingsValue::Float64(0.1),
        NativeSettingsValue::String("text \u{2603}\0".into()),
        NativeSettingsValue::Enum("FutureEnum.Item".into()),
        NativeSettingsValue::Ref(rng.below(count)),
        NativeSettingsValue::Vector2([number, -0.0]),
        NativeSettingsValue::Vector3([0.1, number, f32::INFINITY]),
        NativeSettingsValue::UDim([number, 1.0]),
        NativeSettingsValue::UDim2([0.1, number, 0.0, -0.0]),
        NativeSettingsValue::Color3([0.1, number, f32::NEG_INFINITY]),
        NativeSettingsValue::Rect([0.1, number, 0.0, -0.0]),
        NativeSettingsValue::CFrame([
            number, -0.0, 0.1, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0,
        ]),
    ]
}

fn generated_state(seed: u64, count: usize) -> ServiceState {
    let mut rng = SimRng(seed);
    let mut ids = (0..count)
        .map(|index| format!("debug:0_{}", index + 1))
        .collect::<Vec<_>>();
    rng.shuffle(&mut ids);
    let document = SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances: ids
            .into_iter()
            .enumerate()
            .map(|(index, id)| {
                let parent = (index + 1 < count).then(|| index + 1 + rng.below(count - index - 1));
                let mut row =
                    SettingsBytecodeInstance::new(id, "duplicate".into(), "Folder".into(), parent);
                row.properties.insert("UniqueId".into(), json!("duplicate"));
                row.properties.insert("Scalar".into(), json!(rng.next()));
                row.properties.insert(
                    "Target".into(),
                    json!({"_type":"Ref", "instanceIndex":1 + rng.below(count)}),
                );
                if index == 0 {
                    row.class_name = "Camera".into();
                }
                if index == 1 {
                    row.class_name = if seed.is_multiple_of(2) {
                        "Script"
                    } else {
                        "LocalScript"
                    }
                    .into();
                    row.properties.insert(
                        "RunContext".into(),
                        json!({"_type":"EnumItem", "name":"Server"}),
                    );
                }
                if index + 1 == count {
                    row.name = "Workspace".into();
                    row.class_name = "Workspace".into();
                    row.properties.insert(
                        "CurrentCamera".into(),
                        json!({"_type":"Ref", "instanceIndex":1}),
                    );
                    for (at, value) in values(seed).into_iter().enumerate() {
                        row.properties.insert(format!("Value{at}"), value.clone());
                        let attribute = value.is_boolean()
                            || value.is_number()
                            || value.is_string()
                            || value.as_object().is_some_and(|fields| {
                                !matches!(
                                    fields.get("_type").and_then(Value::as_str),
                                    Some("FutureType" | "Ref")
                                ) && !fields.contains_key("Ref")
                            });
                        if attribute {
                            row.attributes.insert(format!("Attribute{at}"), value);
                        }
                    }
                    for name in [
                        "Source",
                        "Name",
                        "ClassName",
                        "Parent",
                        "RobloxLocked",
                        "RunContext",
                        "name",
                        "parent",
                    ] {
                        row.properties
                            .insert(name.into(), json!("not a record field"));
                    }
                    for (name, value) in [
                        ("Archivable", json!(true)),
                        ("CharacterAutoLoads", json!(true)),
                        ("Sandboxed", json!(false)),
                        ("Default", json!(42)),
                    ] {
                        row.properties.insert(name.into(), value);
                    }
                }
                row
            })
            .collect(),
    };
    let mut state = captured_state(&document, false);
    state.class_defaults_by_class.insert(
        "Workspace".into(),
        Map::from_iter([("Default".into(), json!(42))]),
    );
    // Transported ids, debug ids and the fallback all have distinct precedence.
    for (index, row) in state.instances.iter_mut().enumerate() {
        if seed.is_multiple_of(4) {
            row.transported_settings_id = Some(format!("store:{index}"));
            continue;
        }
        match (index + seed as usize) % 5 {
            0 => row.transported_settings_id = Some(format!("editor:{index}")),
            1 => {
                row.transported_settings_id = None;
                row.debug_id = Some(format!("0_{}", count + index + 1));
            }
            2 => {
                row.transported_settings_id = Some(String::new());
                row.debug_id = Some(format!("0_0{index}"));
            }
            _ => {}
        }
    }
    if let Some(row) = state.instances.first_mut() {
        row.properties
            .insert("Attributes".into(), json!({"Raw": false}));
    }
    if let Some(debug_id) = state.instances.iter().find_map(|row| row.debug_id.clone()) {
        state.instances[state.service_root_index].properties.insert(
            "DebugTarget".into(),
            json!({"_type":"Ref","debugId":debug_id}),
        );
    }
    state
}

fn add_native_properties(state: &mut ServiceState, seed: u64) {
    let count = state.instances.len();
    if count == 0 {
        state.native_properties_by_instance = Some(Vec::new());
        return;
    }
    let mut rows = vec![Vec::new(); count];
    let row = &mut rows[state.service_root_index];
    row.extend(
        native_values(seed, count)
            .into_iter()
            .enumerate()
            .map(|(at, value)| NativeSettingsProperty {
                name: format!("Native{at}"),
                value,
            }),
    );
    for (name, value) in [
        ("Scalar", NativeSettingsValue::String("JSON wins".into())),
        ("Name", NativeSettingsValue::String("record wins".into())),
        ("RunContext", NativeSettingsValue::Int(1)),
        ("Mixed", NativeSettingsValue::Bool(true)),
        (
            "Mixed",
            NativeSettingsValue::String("higher kind wins".into()),
        ),
    ] {
        row.push(NativeSettingsProperty {
            name: name.into(),
            value,
        });
    }
    if count > 1 {
        state.instances[1].properties.remove("RunContext");
        rows[1].push(NativeSettingsProperty {
            name: "RunContext".into(),
            value: NativeSettingsValue::Enum("Server".into()),
        });
    }
    state.native_properties_by_instance = Some(rows);
}

fn oracle(state: &ServiceState, pools: &[rayon::ThreadPool]) -> Result<bool, String> {
    let expected = pools[0].install(|| {
        encode_service_settings_binary(state).and_then(|bytes| decode_settings_bytecode(&bytes))
    });
    for pool in pools {
        let actual = pool.install(|| settings_bytecode_from_service_state(state));
        match (&expected, actual) {
            (Ok(expected), Ok(actual)) => {
                if !crate::settings::codec_sim::same_document(expected, &actual) {
                    let differing = expected
                        .instances
                        .iter()
                        .zip(&actual.instances)
                        .position(|(left, right)| left != right);
                    return Err(format!("captured conversion differs at row {differing:?}"));
                }
                let output = |document| {
                    encode_settings_bytecode(document).map_err(|error| format!("{error:#}"))
                };
                if output(expected) != output(&actual) {
                    return Err("captured conversion changed encoded bytes".into());
                }
                // A digest taken chunk by chunk is the document's own.
                let document =
                    crate::settings::equivalence::pulled_settings_document_digest(&actual);
                if pool
                    .install(|| pulled_digest_from_service_state(state))
                    .is_ok_and(|digest| digest != document)
                {
                    return Err("captured digest differs from its document's".into());
                }
            }
            (Err(expected), Err(actual)) => {
                if format!("{expected:#}") != format!("{actual:#}") {
                    return Err(format!(
                        "captured conversion error differs: {expected:#} vs {actual:#}"
                    ));
                }
            }
            (Ok(_), Err(error)) => {
                return Err(format!(
                    "captured conversion rejected accepted state: {error:#}"
                ));
            }
            (Err(error), Ok(_)) => {
                return Err(format!(
                    "captured conversion accepted rejected state: {error:#}"
                ));
            }
        }
    }
    Ok(expected.is_ok())
}

fn sparse_indices(state: &mut ServiceState) {
    let sparse = |index: usize| index * 7 + 11;
    for row in &mut state.instances {
        row.instance_index = row.instance_index.map(sparse);
        row.parent_index = row.parent_index.map(sparse);
        visit_reference_objects_mut(&mut row.properties, |reference| {
            if let Some(index) = reference.get("instanceIndex").and_then(Value::as_u64) {
                reference.insert("instanceIndex".into(), json!(sparse(index as usize)));
            }
        });
    }
}

/// A value the capture holds once for many instances is written, and
/// converted, as the JSON value it stands for.
#[test]
fn a_shared_value_is_stored_as_the_json_value_it_holds() {
    let payload = json!({"_type": "SharedString", "base64": "AAECAwQFBgcICQ=="});
    let mut document = SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances: vec![SettingsBytecodeInstance::new(
            "root".into(),
            "Workspace".into(),
            "Workspace".into(),
            None,
        )],
    };
    for name in ["A", "B"] {
        let mut part =
            SettingsBytecodeInstance::new(name.into(), name.into(), "MeshPart".into(), Some(0));
        part.properties
            .insert("PhysicalConfigData".into(), payload.clone());
        part.properties.insert("Anchored".into(), json!(true));
        document.instances.push(part);
    }
    let as_json = captured_state(&document, false);
    let as_shared = captured_state(&document, true);
    let shared = as_shared
        .native_properties_by_instance
        .iter()
        .flatten()
        .flatten()
        .filter(|property| matches!(property.value, NativeSettingsValue::Shared(_)))
        .count();
    assert_eq!(shared, 2);
    let bytes = encode_service_settings_binary(&as_shared).unwrap();
    assert!(bytes == encode_service_settings_binary(&as_json).unwrap());
    let converted = settings_bytecode_from_service_state(&as_shared).unwrap();
    assert!(crate::settings::codec_sim::same_document(
        &converted,
        &decode_settings_bytecode(&bytes).unwrap()
    ));
    for part in &converted.instances[1..] {
        assert_eq!(part.properties["PhysicalConfigData"], payload);
    }
}

/// A digest taken chunk by chunk from a captured state is its document's own,
/// and leaves out what the viewport camera holds.
#[test]
fn a_captured_digest_is_its_documents_and_leaves_out_the_viewport_camera() {
    let document = |field_of_view: f64, transparency: f64| {
        let instance = |id: String, class: &str, parent| {
            SettingsBytecodeInstance::new(id.clone(), id, class.into(), parent)
        };
        let mut root = instance("Workspace".into(), "Workspace", None);
        root.properties.insert(
            "CurrentCamera".into(),
            json!({"_type": "Ref", "instanceIndex": 2}),
        );
        let mut camera = instance("Camera".into(), "Camera", Some(0));
        camera
            .properties
            .insert("FieldOfView".into(), json!(field_of_view));
        let mut instances = vec![root, camera];
        for index in 0..9_000 {
            let mut part = instance(format!("Part{index}"), "Part", Some(0));
            let value = if index == 8_765 { transparency } else { 0.5 };
            part.properties.insert("Transparency".into(), json!(value));
            instances.push(part);
        }
        SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances,
        }
    };
    for native in [false, true] {
        let digest = |field_of_view, transparency| {
            let state = captured_state(&document(field_of_view, transparency), native);
            let streamed = pulled_digest_from_service_state(&state).unwrap();
            let converted = settings_bytecode_from_service_state(&state).unwrap();
            assert!(
                streamed
                    == crate::settings::equivalence::pulled_settings_document_digest(&converted)
            );
            streamed
        };
        let unchanged = digest(70.0, 0.5);
        assert!(unchanged == digest(55.0, 0.5), "native={native}");
        assert!(unchanged != digest(70.0, 0.25), "native={native}");
    }
}

#[test]
fn captured_documents_equal_encoding_then_decoding() {
    let pools = [1, 4, 64].map(|workers| {
        rayon::ThreadPoolBuilder::new()
            .num_threads(workers)
            .build()
            .unwrap()
    });
    run_seeds(24, |seed, _| {
        for native in [false, true] {
            for elided in [false, true] {
                for dense in [false, true] {
                    let mut state = generated_state(seed, 8 + seed as usize % 24);
                    if native {
                        add_native_properties(&mut state, seed);
                    }
                    state.properties_default_elided = elided;
                    state.dense_index_topology = dense;
                    if !dense {
                        sparse_indices(&mut state);
                    }
                    if !oracle(&state, &pools)? {
                        let error = encode_service_settings_binary(&state)
                            .and_then(|bytes| decode_settings_bytecode(&bytes))
                            .unwrap_err();
                        return Err(format!(
                            "generated coverage state was rejected (native={native}, elided={elided}, dense={dense}): {error:#}"
                        ));
                    }
                }
            }
        }
        let input = crate::settings::codec_sim::captured_input(seed);
        let mut state = captured_state(&input, seed % 2 == 0);
        state.properties_default_elided = seed % 3 != 0;
        state.dense_index_topology = seed % 3 != 1;
        oracle(&state, &pools)?;
        Ok(())
    });
}

#[test]
fn captured_documents_keep_chunk_boundaries_and_rejections() {
    let pools = [1, 4, 64].map(|workers| {
        rayon::ThreadPoolBuilder::new()
            .num_threads(workers)
            .build()
            .unwrap()
    });
    run_seeds(8, |seed, _| {
        let count = [0, 1, 2047, 2048, 4095, 4096, 4097, 8193][seed as usize % 8];
        let mut state = generated_state(seed, count);
        add_native_properties(&mut state, seed);
        state.properties_default_elided = seed % 3 != 1;
        state.dense_index_topology = seed % 3 != 0;
        oracle(&state, &pools)?;
        if count < 2 {
            return Ok(());
        }
        for value in [
            json!({"_type":"Vector3","x":1}),
            json!({"_type":"CFrame","components":[]}),
            json!({"_type":"Ref","pathSegments":["OtherService"],"pathOrdinals":[false]}),
        ] {
            state.instances[0]
                .properties
                .insert("Invalid".into(), value);
            if oracle(&state, &pools)? {
                return Err("malformed property was accepted".into());
            }
        }
        state.instances[0].properties.remove("Invalid");
        let old_ids = [
            state.instances[0].transported_settings_id.clone(),
            state.instances[1].transported_settings_id.clone(),
        ];
        for row in &mut state.instances[..2] {
            row.transported_settings_id = Some("duplicate".into());
        }
        if oracle(&state, &pools)? {
            return Err("duplicate settings id was accepted".into());
        }
        for (row, id) in state.instances.iter_mut().zip(old_ids) {
            row.transported_settings_id = id;
        }
        let rows = state.native_properties_by_instance.as_mut().unwrap();
        rows[0].extend([false, true].map(|_| NativeSettingsProperty {
            name: "Duplicate".into(),
            value: NativeSettingsValue::Int(1),
        }));
        if oracle(&state, &pools)? {
            return Err("duplicate group row was accepted".into());
        }
        Ok(())
    });
}
