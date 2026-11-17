use super::*;
use crate::tests::support::{SimRng, run_seeds};

pub(crate) fn check_codec_document(bytes: &[u8]) -> Result<()> {
    for mode in 0..4 {
        let mut oracle = decode_settings_bytecode(bytes)?;
        let (mut document, mut numeric) = decode_for_property_edit(bytes)?;
        if mode == 0 {
            assert_eq!(
                numeric.encode(&document)?,
                encode_settings_bytecode(&oracle)?
            );
        }
        for index in 0..oracle.instances.len() {
            assert_eq!(
                crate::settings::equivalence::is_reconciliation_protected_workspace_camera(
                    &document, index
                ),
                crate::settings::equivalence::is_reconciliation_protected_workspace_camera(
                    &oracle, index
                ),
            );
        }
        let indices = (0..oracle.instances.len())
            .filter(|index| mode == 3 || index % 7 == mode)
            .collect::<Vec<_>>();
        numeric.materialize(&mut document, &indices)?;
        for index in indices {
            assert_eq!(document.instances[index], oracle.instances[index]);
            edit(&mut document.instances[index], mode);
            edit(&mut oracle.instances[index], mode);
        }
        assert_eq!(
            numeric.encode(&document)?,
            encode_settings_bytecode(&oracle)?
        );
    }
    Ok(())
}

fn edit(instance: &mut SettingsBytecodeInstance, mode: usize) {
    match mode {
        1 => {
            instance
                .attributes
                .insert("NumericEdit".into(), json!(3.125));
            instance.properties.insert("TextScaled".into(), json!(true));
            instance.properties.insert(
                "Nested".into(),
                json!([{
                    "_type":"EnumItem", "enumType":"Material", "name":"Plastic"
                }]),
            );
        }
        2 => {
            instance.properties.clear();
            instance.attributes.clear();
            instance
                .properties
                .insert("NewValue".into(), json!("new string"));
        }
        3 => {
            for value in instance.properties.values_mut() {
                *value = json!([1, 2, 3]);
            }
        }
        _ => {}
    }
}

#[test]
fn property_edits_preserve_eager_encoder_bytes() {
    run_seeds(32, |seed, _| {
        let mut rng = SimRng(seed);
        let count = if seed % 8 == 0 {
            2051
        } else {
            2 + rng.below(64)
        };
        let mut instances = Vec::new();
        let mut ids = (0..count)
            .map(|index| format!("debug:{index:x}"))
            .collect::<Vec<_>>();
        rng.shuffle(&mut ids);
        for (index, id) in ids.into_iter().enumerate() {
            let mut instance = SettingsBytecodeInstance::new(
                id,
                format!("node{}", rng.below(5)),
                "Part".into(),
                (index + 1 < count).then_some(index + 1),
            );
            // A shallow hierarchy whose parents follow their children.
            if index + 1 < count {
                instance.parent_index = Some(count - 1);
            }
            for kind in [11, 12, 13, 14, 15, 16, 18] {
                let fixed = fixed_numeric_kind_from_tag(kind).unwrap();
                let mut bytes = Vec::new();
                for at in 0..fixed_numeric_len(fixed) {
                    let bits = if index % 17 == 0 {
                        [0, 0x8000_0000, 1, 0x7f80_0000, 0x7fc0_0001, 0xff80_0000][at % 6]
                    } else if kind == 16 && seed % 2 == 0 && at >= 3 {
                        (if at % 4 == 3 { 1_f32 } else { 0_f32 }).to_bits()
                    } else {
                        rng.next() as u32
                    };
                    bytes.extend_from_slice(&bits.to_le_bytes());
                }
                let budget = DecodeBudget::new(usize::MAX);
                let value = decode_raw_value_payload(
                    &mut BytecodeReader::new(&bytes),
                    kind,
                    ValueScope {
                        strings: &[],
                        instance_count: count,
                        budget: &budget,
                    },
                    0,
                )
                .unwrap();
                instance.properties.insert(format!("Field{kind}"), value);
            }
            // Mixed-kind groups use the ordinary overwrite ordering.
            instance.properties.insert(
                "Mixed".into(),
                if index % 2 == 0 {
                    json!(false)
                } else {
                    instance.properties["Field12"].clone()
                },
            );
            instance
                .properties
                .insert("Link".into(), json!({"_type":"Ref", "instanceIndex":count}));
            instance.attributes.insert(
                "Typed".into(),
                json!({"_type":"Vector3", "x":1, "y":2, "z":3}),
            );
            instance
                .properties
                .insert("UniqueId".into(), json!("shared"));
            instance.properties.insert(
                "Nested".into(),
                json!([{"_type":"EnumItem", "name":"Plastic"}]),
            );
            instances.push(instance);
        }
        instances[0].class_name = "Camera".into();
        let camera_frame = instances[0].properties["Field16"].clone();
        instances[0]
            .properties
            .insert("CFrame".into(), camera_frame);
        instances[count - 1].class_name = "Workspace".into();
        instances[count - 1].name = "Workspace".into();
        instances[count - 1].properties.insert(
            "CurrentCamera".into(),
            json!({"_type":"Ref", "instanceIndex":1}),
        );
        let document = SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances,
        };
        let mut bytes = encode_settings_bytecode(&document).map_err(|error| error.to_string())?;
        if seed % 2 == 0 {
            bytes[SETTINGS_BINARY_MAGIC.len()] = SETTINGS_BINARY_MIN_VERSION;
        }
        check_codec_document(&bytes).map_err(|error| error.to_string())
    });
}

/// Payloads of one length that agree where they are sampled are still told
/// apart: each row keeps its own.
#[test]
fn payloads_alike_in_length_and_sample_keep_their_own_rows() {
    let root = SettingsBytecodeInstance::new("root".into(), "Root".into(), "Folder".into(), None);
    let mut instances = vec![root];
    for (index, text) in ["AAAAAAAAAAAAAAAA", "ABAAAAAAAAAAAAAA", "AAAAAAAAAAAAAAAA"]
        .into_iter()
        .enumerate()
    {
        let mut part = SettingsBytecodeInstance::new(
            format!("part{index}"),
            format!("Part{index}"),
            "MeshPart".into(),
            Some(0),
        );
        part.properties.insert(
            "PhysicsData".into(),
            json!({"_type":"BinaryString", "base64": text}),
        );
        instances.push(part);
    }
    let document = SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances,
    };
    check_codec_document(&encode_settings_bytecode(&document).unwrap()).unwrap();
}

#[test]
fn property_edit_decoder_retains_group_overwrite_and_error_rules() {
    let instance =
        SettingsBytecodeInstance::new("root".into(), "Root".into(), "Folder".into(), None);
    let mut document = SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances: vec![instance],
    };
    document.instances[0].properties.insert(
        "Value".into(),
        json!({"_type":"Vector3", "x":1, "y":2, "z":3}),
    );
    let bytes = encode_settings_bytecode(&document).unwrap();
    let (_, payload) = decode_settings_bytecode_container(&bytes).unwrap();
    let mut reader = BytecodeReader::new(&payload);
    let budget = DecodeBudget::for_file(bytes.len());
    let header = decode_settings_payload_header(&mut reader, &budget).unwrap();
    let prefix = &payload[..reader.position()];
    let specs = read_settings_property_groups(&mut reader, &header.properties).unwrap();
    let body = specs[0].3;
    for (count, bodies) in [
        (1, vec![(12, body.to_vec()), (2, vec![0])]),
        (1, vec![(2, vec![0]), (12, body.to_vec())]),
        (1, vec![(12, body.to_vec()), (12, body.to_vec())]),
        (2, vec![(12, [body, body].concat())]),
        (1, vec![(12, vec![2])]),
        (1, vec![(16, vec![2])]),
        (1, vec![(5, [&[0][..], &0.25_f64.to_le_bytes()].concat())]),
        (1, vec![(20, [&[0][..], &f32::NAN.to_le_bytes()].concat())]),
    ] {
        let mut payload = prefix.to_vec();
        write_var_u64(&mut payload, bodies.len() as u64).unwrap();
        for (kind, body) in bodies {
            write_var_u64(&mut payload, 0).unwrap();
            payload.push(kind);
            write_var_u64(&mut payload, count).unwrap();
            write_var_u64(&mut payload, body.len() as u64).unwrap();
            payload.extend(body);
        }
        let bytes = wrap_settings_bytecode_payload(&payload).unwrap();
        match (
            decode_settings_bytecode(&bytes),
            decode_for_property_edit(&bytes),
        ) {
            (Ok(oracle), Ok((mut document, mut numeric))) => {
                assert_eq!(
                    numeric.encode(&document).unwrap(),
                    encode_settings_bytecode(&oracle).unwrap()
                );
                numeric.materialize(&mut document, &[0]).unwrap();
                assert_eq!(document, oracle);
                assert_eq!(
                    numeric.encode(&document).unwrap(),
                    encode_settings_bytecode(&oracle).unwrap()
                );
            }
            (Err(oracle), Err(actual)) => assert_eq!(oracle.to_string(), actual.to_string()),
            _ => panic!("decoders disagree"),
        }
    }
}
