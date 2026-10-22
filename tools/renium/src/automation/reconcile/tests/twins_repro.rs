use super::*;

fn mirror_twins(studio: bool, shared_uids: bool) -> SettingsBytecode {
    let mut document = twin_document();
    for model_number in if studio { [2, 1] } else { [1, 2] } {
        let model = twin_add(&mut document, 0, "Model", "Model", json!({}));
        document.instances[model]
            .attributes
            .insert("Marker".into(), json!(model_number));
        let mut identified = vec![(model, model_number * 100)];
        for number in 1..=model_number {
            let part = twin_add(
                &mut document,
                model,
                "Part",
                "Part",
                json!({"Transparency":number as f64 / 10.0}),
            );
            identified.push((part, model_number * 100 + number));
        }
        if !studio && model_number == 2 {
            let added = twin_add(&mut document, model, "OnlyInFiles", "Folder", json!({}));
            identified.push((added, 299));
        }
        for (index, number) in identified {
            let instance = &mut document.instances[index];
            instance.settings_id = if studio && index != model {
                format!("editor:{number}")
            } else {
                format!("debug:1_{number}")
            };
            let uid = number + if studio && !shared_uids { 10000 } else { 0 };
            instance.properties.insert(
                "UniqueId".into(),
                json!({"_type":"UniqueId", "value":format!("{uid:032x}")}),
            );
        }
        let reference = twin_ref(&document, model + 1);
        let pointer = twin_add(
            &mut document,
            model,
            "Pointer",
            "ObjectValue",
            json!({"Value":reference}),
        );
        let number = model_number * 100 + 50;
        document.instances[pointer].settings_id = if studio {
            format!("editor:{number}")
        } else {
            format!("debug:1_{number}")
        };
        let uid = number + if studio && !shared_uids { 10000 } else { 0 };
        document.instances[pointer].properties.insert(
            "UniqueId".into(),
            json!({"_type":"UniqueId", "value":format!("{uid:032x}")}),
        );
    }
    document
}

#[test]
fn first_pairing_twins_package_mirror_ids_and_descendants_survive_reordering() {
    // A package mirror may carry the package source's UniqueIds, not the
    // destination's. Cover both that case and shared persistent identities.
    for shared_uids in [true, false] {
        let mut editor = mirror_twins(false, shared_uids);
        let mut studio = mirror_twins(true, shared_uids);
        let expected_ids = studio
            .instances
            .iter()
            .map(|instance| {
                persistent_identity(instance).map_or_else(
                    || "root".to_string(),
                    |id| {
                        let uid = usize::from_str_radix(id, 16).unwrap();
                        let uid = uid - if shared_uids { 0 } else { 10000 };
                        format!("debug:1_{uid}")
                    },
                )
            })
            .collect::<Vec<_>>();
        let conflicts = twin_align(&mut editor, &mut studio);
        assert!(
            conflicts.is_empty(),
            "shared_uids={shared_uids}: {conflicts:?}"
        );
        assert_eq!(editor.instances.len(), studio.instances.len() + 1);
        assert_twin_ids_unique(&studio);
        for (index, instance) in studio.instances.iter().enumerate() {
            assert_eq!(instance.settings_id, expected_ids[index]);
            let target = editor
                .instances
                .iter()
                .position(|i| i.settings_id == instance.settings_id)
                .unwrap();
            assert!(settings_instances_equal(&editor, target, &studio, index));
        }
    }
}

#[test]
fn first_pairing_twins_equivalent_slots_can_replace_provisional_ids() {
    let mut editor = twin_document();
    for number in [1, 2] {
        twin_add(
            &mut editor,
            0,
            "Attachment",
            "Attachment",
            json!({"UniqueId":{"_type":"UniqueId", "value":format!("{number:032x}")}}),
        );
    }
    twin_add(&mut editor, 0, "Marker", "Folder", json!({}));
    let mut studio = editor.clone();
    studio.instances.swap(1, 2);
    studio.instances.swap(2, 3);
    for index in [1, 3] {
        studio.instances[index].properties.insert(
            "UniqueId".into(),
            json!({"_type":"UniqueId", "value":format!("{:032x}", index + 10)}),
        );
    }
    let conflicts = twin_align(&mut editor, &mut studio);
    assert!(conflicts.is_empty(), "{conflicts:?}");
    assert_twin_ids_unique(&studio);
}

#[test]
#[ignore = "requires private RENIUM_REPRO_EDITOR and RENIUM_REPRO_STUDIO store paths"]
fn repro_first_pairing_from_store_files() {
    let read = |name: &str| {
        let path =
            std::env::var_os(name).unwrap_or_else(|| panic!("set {name} to a .renium store"));
        SettingsBytecode::read_file(Path::new(&path)).unwrap()
    };
    let mut editor = read("RENIUM_REPRO_EDITOR");
    let mut studio = read("RENIUM_REPRO_STUDIO");
    let started = std::time::Instant::now();
    let conflicts = twin_align(&mut editor, &mut studio);
    std::eprintln!(
        "editor_instances={} studio_instances={} conflicts={} align_ms={:.3}",
        editor.instances.len(),
        studio.instances.len(),
        conflicts.len(),
        started.elapsed().as_secs_f64() * 1000.0,
    );
    for conflict in &conflicts {
        std::eprintln!("{conflict}");
    }
    assert!(conflicts.is_empty(), "{} conflicts", conflicts.len());
}

fn subset_twins(order: &[usize]) -> SettingsBytecode {
    let mut document = twin_document();
    for &number in order {
        let model = twin_add(&mut document, 0, "Twin", "Model", json!({}));
        let part = twin_add(
            &mut document,
            model,
            "Part",
            "Part",
            json!({"Transparency":number as f64 / 10.0}),
        );
        let reference = twin_ref(&document, part);
        twin_add(
            &mut document,
            model,
            "Pointer",
            "ObjectValue",
            json!({"Value":reference}),
        );
    }
    document
}

#[test]
fn first_pairing_twins_unequal_groups_preserve_additions_and_references() {
    let snapshot = |document: &SettingsBytecode| ProjectSnapshot {
        entries: [(
            PathBuf::from("instances/Workspace.renium"),
            SnapshotEntry::File(encode_settings_bytecode(document).unwrap()),
        )]
        .into_iter()
        .collect(),
    };
    for added_order in [[3, 2, 1], [2, 3, 1], [2, 1, 3]] {
        for added_in_editor in [false, true] {
            let smaller = subset_twins(&[1, 2]);
            let larger = subset_twins(&added_order);
            let (editor, studio) = if added_in_editor {
                (larger, smaller)
            } else {
                (smaller, larger)
            };
            let studio = twin_observation(&studio);
            let (merged, conflicts) = merge_snapshots(
                None,
                &snapshot(&editor),
                &snapshot(&studio),
                ConflictPreference::None,
            )
            .unwrap();
            assert!(
                conflicts.is_empty(),
                "{added_order:?}, added_in_editor={added_in_editor}: {conflicts:?}"
            );
            let document = settings_document(merged.entries.values().next()).unwrap();
            assert_eq!(document.instances.len(), 10);
            assert_twin_ids_unique(&document);
            for pointer in document.instances.iter().filter(|i| i.name == "Pointer") {
                let part = document
                    .instances
                    .iter()
                    .find(|i| i.name == "Part" && i.parent_index == pointer.parent_index)
                    .unwrap();
                assert_eq!(pointer.properties["Value"]["settingsId"], part.settings_id);
            }
        }
    }
}

#[test]
fn first_pairing_twins_unequal_groups_reject_changed_values_and_references() {
    for changed_reference in [false, true] {
        for added_in_editor in [false, true] {
            let smaller = subset_twins(&[1, 2]);
            let mut larger = subset_twins(&[3, 2, 1]);
            if changed_reference {
                // The second model now points outside its own subtree, to
                // the newly added model's distinguishable Part.
                let reference = twin_ref(&larger, 2);
                larger.instances[6]
                    .properties
                    .insert("Value".into(), reference);
            } else {
                larger.instances[5]
                    .properties
                    .insert("Transparency".into(), json!(0.9));
            }
            let (mut editor, studio) = if added_in_editor {
                (larger, smaller)
            } else {
                (smaller, larger)
            };
            let mut studio = twin_observation(&studio);
            let conflicts = twin_align(&mut editor, &mut studio);
            assert!(
                !conflicts.is_empty(),
                "changed_reference={changed_reference}, added_in_editor={added_in_editor}"
            );
            assert_twin_ids_unique(&studio);
            assert_eq!(editor.instances.len() + studio.instances.len(), 17);
        }
    }
}

#[test]
fn first_pairing_twins_pivot_noise_preserves_real_edit_detection() {
    let mut editor = twin_document();
    let model = twin_add(
        &mut editor,
        0,
        "Model",
        "Model",
        json!({"WorldPivot":{"_type":"CFrame", "components":[100.0, 10.0, 500.0, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0]}}),
    );
    for (rotation, conflict) in [(8.940696716308594e-6, false), (1e-5, true)] {
        let mut studio = twin_observation(&editor);
        studio.instances[model].properties["WorldPivot"]["components"][0] = json!(100.000061);
        studio.instances[model].properties["WorldPivot"]["components"][11] = json!(1.0 + rotation);
        let conflicts = twin_align(&mut editor.clone(), &mut studio);
        assert_eq!(!conflicts.is_empty(), conflict, "{conflicts:?}");
    }
}
