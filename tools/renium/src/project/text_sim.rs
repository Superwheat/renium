use super::*;

pub(super) fn settings_doc_to_text_slow(document: &SettingsBytecode) -> String {
    let mut children: HashMap<Option<usize>, Vec<usize>> = HashMap::new();
    for (index, instance) in document.instances.iter().enumerate() {
        children
            .entry(instance.parent_index)
            .or_default()
            .push(index);
    }
    let mut out = format!(
        "# renium store v{}: {} instances\n",
        document.version,
        document.instances.len()
    );
    let service = document
        .instances
        .iter()
        .find(|instance| instance.parent_index.is_none())
        .map(|instance| instance.name.as_str())
        .unwrap_or_default();
    let (path_segments_by_index, path_ordinals_by_index) =
        build_editor_instance_path_parts(document, service);
    let canonical_settings_ids_by_index = document
        .instances
        .iter()
        .map(|instance| Some(instance.settings_id.clone()))
        .collect::<Vec<_>>();
    let mut stack: Vec<(usize, String)> = Vec::new();
    for root in children
        .get(&None)
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .rev()
    {
        stack.push((root, String::new()));
    }
    while let Some((index, parent_path)) = stack.pop() {
        let instance = &document.instances[index];
        let path = if parent_path.is_empty() {
            visible(&instance.name).into_owned()
        } else {
            format!("{parent_path}/{}", visible(&instance.name))
        };
        let _ = writeln!(
            out,
            "= {path} [{}] id={}",
            visible(&instance.class_name),
            visible(&instance.settings_id)
        );
        let mut property_record = instance.properties.clone();
        stabilize_reference_output(
            document,
            &path_segments_by_index,
            &path_ordinals_by_index,
            &canonical_settings_ids_by_index,
            &mut property_record,
        );
        let mut properties: Vec<(&String, &Value)> = property_record.iter().collect();
        properties.sort_by(|a, b| a.0.cmp(b.0));
        for (key, value) in properties {
            if key == "Source"
                && let Some(text) = value.as_str()
            {
                let _ = writeln!(
                    out,
                    "  Source = <{} lines, {} bytes, fnv1a={}>",
                    text.lines().count(),
                    text.len(),
                    fnv1a_hex(text.as_bytes())
                );
                continue;
            }
            let _ = writeln!(out, "  {} = {}", visible(key), visible(&value.to_string()));
        }
        let mut attribute_record = instance.attributes.clone();
        stabilize_reference_output(
            document,
            &path_segments_by_index,
            &path_ordinals_by_index,
            &canonical_settings_ids_by_index,
            &mut attribute_record,
        );
        let mut attributes: Vec<(&String, &Value)> = attribute_record.iter().collect();
        attributes.sort_by(|a, b| a.0.cmp(b.0));
        for (key, value) in attributes {
            let _ = writeln!(out, "  @{} = {}", visible(key), visible(&value.to_string()));
        }
        if let Some(kids) = children.get(&Some(index)) {
            for kid in kids.iter().rev() {
                stack.push((*kid, path.clone()));
            }
        }
    }
    out
}

#[test]
fn store_text_matches_serial_oracle_for_seeded_trees() {
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(4)
        .build()
        .unwrap();
    let serial = rayon::ThreadPoolBuilder::new()
        .num_threads(1)
        .build()
        .unwrap();
    crate::tests::support::run_seeds(24, |seed, _| {
        let mut rng = crate::tests::support::SimRng(seed);
        let count = [0, 1, 2_047, 2_048, 2_051, 37][seed as usize % 6];
        let mut document = SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: Vec::new(),
        };
        for index in 0..count {
            let parent = (index > 0).then(|| rng.below(index));
            let name = if index == 0 {
                "Workspace"
            } else {
                ["", "same", "same", "a/b", "a.b", "??", "a\nb", "a\u{202e}b"][rng.below(8)]
            };
            let mut node = SettingsBytecodeInstance::new(
                format!("id:{index}"),
                name.into(),
                "Folder".into(),
                parent,
            );
            node.properties.insert(
                "Ordinary".into(),
                json!({"nested": [rng.below(20), "text", {"key": true}]}),
            );
            node.attributes.insert("Value".into(), json!(rng.below(20)));
            node.properties.insert("UniqueId".into(), json!("shared"));
            node.properties.insert(
                "Enum".into(),
                json!({"nested":[{"_type":"EnumItem", "name":"Plastic"}]}),
            );
            node.properties
                .insert("ReferenceForms".into(), reference_forms(&mut rng, count));
            if rng.below(4) == 0 {
                node.properties.insert(
                    "Target".into(),
                    json!({"nested": [{"_type":"Ref", "instanceIndex": rng.below(count) + 1}]}),
                );
            }
            if rng.below(5) == 0 {
                node.attributes.insert("Target".into(), json!({"_type":"Ref", "settingsId":"external", "instanceIndex":rng.below(count) + 1}));
            }
            if rng.below(7) == 0 {
                node.properties
                    .insert("Source".into(), json!("local x = 1\r\nreturn x\n"));
            }
            document.instances.push(node);
        }
        let mut order = (0..count).collect::<Vec<_>>();
        rng.shuffle(&mut order);
        let mut new_indices = vec![0; count];
        for (new, &old) in order.iter().enumerate() {
            new_indices[old] = new;
        }
        let before = document.instances;
        document.instances = order
            .iter()
            .map(|&old| {
                let mut node = before[old].clone();
                node.parent_index = node.parent_index.map(|parent| new_indices[parent]);
                node
            })
            .collect();
        for typed in [false, true] {
            if typed {
                for node in &mut document.instances {
                    node.properties["Enum"]["nested"][0]["enumType"] = json!("Material");
                }
            }
            let expected = settings_doc_to_text_slow(&document);
            for pool in [&serial, &pool] {
                let actual = pool.install(|| settings_doc_to_text(&document));
                if actual.as_bytes() != expected.as_bytes() {
                    return Err("store text bytes differ".into());
                }
            }
        }
        Ok(())
    });
}

fn reference_forms(rng: &mut crate::tests::support::SimRng, count: usize) -> Value {
    let index = rng.below(count) + 1;
    json!([
        {"_type":"Ref", "instanceIndex":index},
        {"instanceIndex":index},
        {"Ref":{"instanceIndex":index}},
        {"_type":"Unknown", "settingsId":"", "instanceIndex":index},
        {"instanceId":"foreign", "instanceIndex":index},
        {"inside":[{"_type":"Ref", "instanceIndex":0}, {"instanceIndex":count + 10}]},
        {"_type":"Ref", "settingsId":null, "instanceIndex":index},
        {"ordinary":{"number":index}},
    ])
}
