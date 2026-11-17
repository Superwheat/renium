pub(crate) use super::captured_sim::captured_state;
use super::*;
use crate::tests::support::{SimRng, run_seeds};

#[test]
fn parallel_group_ranges_keep_overwrites_and_rejections() {
    let pools = [1, 4, 64].map(|workers| {
        rayon::ThreadPoolBuilder::new()
            .num_threads(workers)
            .build()
            .unwrap()
    });
    run_seeds(8, |seed, _| {
        let mut rng = SimRng(seed);
        let count = [2047, 2048, 4097, 8193][seed as usize % 4];
        let document = SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: (0..count)
                .map(|index| {
                    let mut row = SettingsBytecodeInstance::new(
                        format!("id:{index}"),
                        "duplicate".into(),
                        "Folder".into(),
                        (index + 1 < count).then_some(count - 1),
                    );
                    row.properties.insert("Value".into(), json!(index));
                    row.attributes.insert("Index".into(), json!(index));
                    row
                })
                .collect(),
        };
        let payload = encode_settings_bytecode_payload(&document).unwrap();
        let budget = DecodeBudget::new(usize::MAX);
        let mut reader = BytecodeReader::new(&payload);
        let header = decode_settings_payload_header(&mut reader, &budget).unwrap();
        let prefix = &payload[..reader.position()];
        let specs = read_settings_property_groups(&mut reader, &header.properties).unwrap();
        let mut overwrite = Vec::new();
        let indices = [0, rng.below(count - 2) + 1, count - 1];
        let mut previous = 0;
        for index in indices {
            write_property_group_instance_index(&mut overwrite, &mut previous, index).unwrap();
        }
        let mut bad_index = Vec::new();
        write_var_u64(&mut bad_index, count as u64).unwrap();
        for (kind, value_count, body) in [
            (2, 3, overwrite),
            (2, 2, vec![0, 0]),
            (2, 1, bad_index),
            (2, 2, vec![0]),
            (255, 1, vec![0]),
            (2, 1, vec![0, 0]),
        ] {
            let mut groups = specs
                .iter()
                .map(|&(name, kind, count, body)| (name.as_str(), kind, count, body))
                .collect::<Vec<_>>();
            groups.push(("Value", kind, value_count, &body));
            // Attribute groups replace the complete map, including with empty.
            let attributes = [0, 0];
            groups.push(("Attributes", 8, 1, &attributes));
            rng.shuffle(&mut groups);
            let mut changed = prefix.to_vec();
            write_var_u64(&mut changed, groups.len() as u64).unwrap();
            for (name, kind, count, body) in groups {
                let property_id = header
                    .properties
                    .iter()
                    .position(|key| key == name)
                    .unwrap();
                write_var_u64(&mut changed, property_id as u64).unwrap();
                changed.push(kind);
                write_var_u64(&mut changed, count as u64).unwrap();
                write_var_u64(&mut changed, body.len() as u64).unwrap();
                changed.extend_from_slice(body);
            }
            let bytes = wrap_settings_bytecode_payload(&changed).unwrap();
            let expected = pools[0].install(|| decode_settings_bytecode(&bytes));
            for pool in &pools[1..] {
                let actual = pool.install(|| decode_settings_bytecode(&bytes));
                match (&expected, actual) {
                    (Ok(expected), Ok(actual)) => assert_eq!(expected, &actual),
                    (Err(expected), Err(actual)) => {
                        assert_eq!(format!("{expected:#}"), format!("{actual:#}"))
                    }
                    _ => return Err("parallel decoder changed group acceptance".into()),
                }
            }
        }
        Ok(())
    });
}
