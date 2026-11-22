use super::*;
use crate::settings::bytecode::blocks::sim::{FORMATS, encoded_as};
use crate::tests::support::{SimRng, run_seeds};

/// The property edit and row chunk laws for a store file of either version:
/// an edit of selected rows writes the store the encoder writes for the same
/// edit of the whole document, a v11 file converted to v12 by that write.
pub(crate) fn check_codec_document(bytes: &[u8]) -> Result<()> {
    check_row_chunks(bytes)?;
    for mode in 0..4 {
        let mut oracle = decode_settings_bytecode(bytes)?;
        let (mut document, mut numeric) = decode_for_property_edit(bytes)?;
        if mode == 0 {
            // A v11 store is converted by its first write.
            let written = numeric.encode(&document)?;
            assert!(blocks::is_block_store(&written));
            assert!(written == encoded_as(bytes, &oracle)?);
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
        assert!(
            numeric.encode(&document)? == encoded_as(bytes, &oracle)?,
            "mode {mode}: the edit of selected rows writes another store"
        );
    }
    Ok(())
}

fn check_row_chunks(bytes: &[u8]) -> Result<()> {
    let oracle = decode_settings_bytecode_holding(bytes)?;
    // What a reader projects before any row is read: each named property,
    // as the projection maps it.
    let named = |name: &str| name.len().is_multiple_of(2);
    let (mut projected, columns) = decode_for_row_chunks(bytes)?;
    columns.project_properties(&mut projected, named, |instance, name, value| {
        (instance.parent_index.is_some() || name.len().is_multiple_of(4)).then_some(value)
    })?;
    for (row, instance) in projected.instances.iter().enumerate() {
        let mut expected = oracle.instances[row].bare();
        for (name, value) in &oracle.instances[row].properties {
            if named(name) && (instance.parent_index.is_some() || name.len().is_multiple_of(4)) {
                expected.properties.insert(name.clone(), value.clone());
            }
        }
        assert_eq!(*instance, expected, "row {row} projected");
    }
    // The rows a reader selects, read in one pass.
    let (mut selected, columns) = decode_for_row_chunks(bytes)?;
    let wanted = (0..oracle.instances.len())
        .map(|row| row % 3 == 1)
        .collect::<Vec<_>>();
    columns.materialize_selected(&mut selected, &wanted)?;
    for (row, instance) in selected.instances.iter().enumerate() {
        let expected = &oracle.instances[row];
        if wanted[row] {
            assert_eq!(instance, expected, "row {row} selected");
        } else {
            assert_eq!(*instance, expected.bare(), "row {row} not selected");
        }
    }
    // The same rows read after their reference columns: those columns as
    // the v11 columns of the same document give them, then the rest.
    let v11 = encode_settings_bytecode_v11(&decode_settings_bytecode(bytes)?)?;
    let referenced = |bytes: &[u8]| -> Result<_> {
        let (mut document, mut columns) = decode_for_row_chunks(bytes)?;
        columns.materialize_references(&mut document, &wanted)?;
        Ok((document, columns))
    };
    let ((mut read, columns), (expected, _)) = (referenced(bytes)?, referenced(&v11)?);
    assert_eq!(read.instances, expected.instances, "reference columns");
    columns.materialize_selected(&mut read, &wanted)?;
    assert_eq!(
        read.instances, selected.instances,
        "rows after their references"
    );
    for size in [1, 7, 4096] {
        let (mut document, mut columns) = decode_for_row_chunks(bytes)?;
        for start in (0..document.instances.len()).step_by(size) {
            let end = (start + size).min(document.instances.len());
            // Every other range comes as values apart from the structure.
            if start / size % 2 == 1 {
                let values = columns.materialize_values(start..end)?;
                for (index, values) in (start..end).zip(values) {
                    let instance = &oracle.instances[index];
                    assert!(
                        crate::settings::held::same_properties(
                            &values.properties,
                            &instance.properties
                        ) && values.attributes == instance.attributes,
                        "row {index} as values"
                    );
                }
                continue;
            }
            columns.materialize(&mut document, start..end)?;
            for index in start..end {
                assert_eq!(document.instances[index], oracle.instances[index]);
                document.instances[index].properties.clear();
                document.instances[index].attributes.clear();
            }
            assert!(
                document
                    .instances
                    .iter()
                    .all(|row| row.properties.is_empty() && row.attributes.is_empty())
            );
        }
        columns.finish()?;
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
            instance.settings_id = format!("renamed {}", instance.settings_id);
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
                    ValueScope::from_owned(&[], count, &budget),
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
        for format in &FORMATS {
            let mut bytes = (format.encode)(&document).map_err(|error| error.to_string())?;
            if seed % 2 == 0 && format.version == FORMATS[0].version {
                bytes[SETTINGS_BINARY_MAGIC.len()] = SETTINGS_BINARY_MIN_VERSION;
            }
            check_codec_document(&bytes).map_err(|error| error.to_string())?;
        }
        Ok(())
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
    for format in &FORMATS {
        check_codec_document(&(format.encode)(&document).unwrap()).unwrap();
    }
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
    let bytes = encode_settings_bytecode_v11(&document).unwrap();
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
            decode_settings_bytecode_holding(&bytes),
            check_row_chunks(&bytes),
        ) {
            (Ok(_), Ok(())) | (Err(_), Err(_)) => {}
            _ => panic!("chunk decoder validation differs from ordinary decode"),
        }
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

/// A column can hold values the row decoder keeps compact beside values it
/// decodes as the ordinary decoder does (a payload held once beside a short
/// one, a number beside one that is not finite). In a store large enough to
/// be applied in parallel, each value still goes to its own row.
#[test]
fn rows_of_a_large_store_hold_the_values_of_a_column_decoded_two_ways() {
    let mut instances = vec![SettingsBytecodeInstance::new(
        "root".into(),
        "Workspace".into(),
        "Workspace".into(),
        None,
    )];
    for index in 0..3 * SETTINGS_BINARY_PARALLEL_CHUNK_SIZE {
        let mut instance = SettingsBytecodeInstance::new(
            format!("id-{index}"),
            format!("Part{index}"),
            "Part".into(),
            Some(0),
        );
        if index % 5 != 0 {
            let payload = if index % 3 == 0 {
                String::new()
            } else {
                format!("payload-{}", index % 11)
            };
            instance.properties.insert(
                "PhysicalConfigData".into(),
                json!({"_type": "SharedString", "base64": base64::encode(payload)}),
            );
        }
        instance
            .properties
            .insert("Transparency".into(), json!(index as f64 / 8.0));
        instances.push(instance);
    }
    let whole = SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances,
    };
    for format in &FORMATS {
        let bytes = (format.encode)(&whole).unwrap();
        let whole = decode_settings_bytecode(&bytes).unwrap();
        for step in [1, 2, 7, 1000] {
            let rows = (1..whole.instances.len()).step_by(step).collect::<Vec<_>>();
            let (read, _) = decode_rows(&bytes, &rows).unwrap();
            for (row, read) in rows.iter().zip(&read) {
                assert_eq!(*read, whole.instances[*row], "row {row} of every {step}");
            }
        }
    }
}
