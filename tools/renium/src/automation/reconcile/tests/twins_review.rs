use super::*;

// Regression fixtures from the independent first-pairing review.

fn review_uid(document: &mut SettingsBytecode, index: usize, uid: usize) {
    document.instances[index].properties.insert(
        "UniqueId".into(),
        json!({"_type":"UniqueId", "value":format!("{uid:032x}")}),
    );
}

#[test]
fn review_added_twin_must_not_steal_locked_identity() {
    let mut editor = twin_document();
    for uid in [1, 2] {
        let index = twin_add(&mut editor, 0, "Twin", "Part", json!({}));
        review_uid(&mut editor, index, uid);
    }
    let mut studio = twin_document();
    for uid in [2, 3, 1] {
        let index = twin_add(&mut studio, 0, "Twin", "Part", json!({}));
        review_uid(&mut studio, index, uid);
        studio.instances[index].settings_id = format!("studio-{uid}");
    }
    let conflicts = twin_align(&mut editor, &mut studio);
    std::eprintln!(
        "addition conflicts={conflicts:?}; ids={:?}",
        studio
            .instances
            .iter()
            .map(|i| (&i.settings_id, persistent_identity(i)))
            .collect::<Vec<_>>()
    );
    assert_twin_ids_unique(&studio);
    for index in [1, 3] {
        let expected = editor
            .instances
            .iter()
            .find(|instance| {
                persistent_identity(instance) == persistent_identity(&studio.instances[index])
            })
            .unwrap();
        assert_eq!(studio.instances[index].settings_id, expected.settings_id);
    }
}

#[test]
fn review_reference_to_changed_twin_is_still_a_difference() {
    let mut editor = twin_document();
    let first = twin_add(
        &mut editor,
        0,
        "MotorAttachment",
        "Attachment",
        json!({"Visible":false}),
    );
    let second = twin_add(
        &mut editor,
        0,
        "MotorAttachment",
        "Attachment",
        json!({"Visible":false}),
    );
    review_uid(&mut editor, first, 1);
    review_uid(&mut editor, second, 2);
    let reference = twin_ref(&editor, first);
    let motor = twin_add(
        &mut editor,
        0,
        "Motor",
        "HingeConstraint",
        json!({"Attachment0":reference}),
    );
    let mut studio = editor.clone();
    studio.instances[second]
        .properties
        .insert("Visible".into(), json!(true));
    studio.instances[motor]
        .properties
        .insert("Attachment0".into(), twin_ref(&editor, second));
    let conflicts = twin_align(&mut editor, &mut studio);
    std::eprintln!(
        "changed-target conflicts={conflicts:?}; ref={}",
        studio.instances[motor].properties["Attachment0"]
    );
    assert!(
        conflicts.iter().any(|value| value.contains("Attachment0")),
        "reference edit was hidden: {conflicts:?}"
    );
}

#[test]
fn review_nested_reference_groups_resolve_dependencies() {
    let mut editor = twin_document();
    let left = twin_add(&mut editor, 0, "Left", "Part", json!({}));
    let right = twin_add(&mut editor, 0, "Right", "Part", json!({}));
    for marker in ["A", "B"] {
        let model = twin_add(&mut editor, 0, "Garage", "Model", json!({}));
        twin_add(
            &mut editor,
            model,
            "Marker",
            "StringValue",
            json!({"Value":marker}),
        );
        let mut joints = Vec::new();
        for target in [left, right] {
            let reference = twin_ref(&editor, target);
            let joint = twin_add(
                &mut editor,
                model,
                "Target",
                "ObjectValue",
                json!({"Value":reference}),
            );
            joints.push(joint);
        }
        for joint in joints {
            let reference = twin_ref(&editor, joint);
            twin_add(
                &mut editor,
                model,
                "Dependent",
                "ObjectValue",
                json!({"Value":reference}),
            );
        }
    }
    let mut studio = twin_observation(&editor);
    // Reversed storage encounters Dependent before its reference Target group.
    let resolved = pairing::twins::resolve(&editor, &studio);
    std::eprintln!("nested partners={:?}", resolved.partners);
    let conflicts = twin_align(&mut editor, &mut studio);
    assert!(conflicts.is_empty(), "{conflicts:?}");
}

#[test]
fn review_six_nested_levels_reversed() {
    fn add(document: &mut SettingsBytecode, parent: usize, depth: usize, marker: usize) {
        if depth == 0 {
            twin_add(
                document,
                parent,
                "Leaf",
                "StringValue",
                json!({"Value":marker.to_string()}),
            );
        } else {
            for side in 0..2 {
                let child = twin_add(document, parent, "Model", "Model", json!({}));
                add(document, child, depth - 1, marker * 2 + side);
            }
        }
    }
    let mut editor = twin_document();
    add(&mut editor, 0, 6, 0);
    let mut studio = twin_observation(&editor);
    let conflicts = twin_align(&mut editor, &mut studio);
    assert!(conflicts.is_empty(), "{conflicts:?}");
    assert_twin_ids_unique(&studio);
    assert!(settings_documents_equivalent(&editor, &studio));
}

fn review_snapshot(document: &SettingsBytecode) -> ProjectSnapshot {
    ProjectSnapshot {
        entries: [(
            PathBuf::from("src/Workspace/__roblox_sync_settings.renium"),
            SnapshotEntry::File(encode_settings_bytecode(document).unwrap()),
        )]
        .into_iter()
        .collect(),
    }
}

#[test]
fn review_added_locked_twin_survives_merge() {
    let mut editor = twin_document();
    for uid in [1, 2] {
        let index = twin_add(&mut editor, 0, "Twin", "Part", json!({}));
        review_uid(&mut editor, index, uid);
    }
    let mut studio = twin_document();
    for uid in [2, 3, 1] {
        let index = twin_add(&mut studio, 0, "Twin", "Part", json!({}));
        review_uid(&mut studio, index, uid);
        studio.instances[index].settings_id = format!("studio-{uid}");
    }
    let (merged, conflicts) = merge_snapshots(
        None,
        &review_snapshot(&editor),
        &review_snapshot(&studio),
        ConflictPreference::None,
    )
    .unwrap();
    let document = settings_document(merged.entries.values().next()).unwrap();
    std::eprintln!(
        "merge addition: count={}, conflicts={conflicts:?}",
        document.instances.len()
    );
    assert!(conflicts.is_empty(), "{conflicts:?}");
    assert_eq!(
        document.instances.len(),
        4,
        "a genuinely new instance was lost"
    );
    assert_twin_ids_unique(&document);
}

#[test]
fn review_renamed_unique_id_cannot_be_reassigned_by_content() {
    let mut editor = twin_document();
    let known = twin_add(
        &mut editor,
        0,
        "OldName",
        "StringValue",
        json!({"Value":"B"}),
    );
    review_uid(&mut editor, known, 1);
    twin_add(&mut editor, 0, "Twin", "StringValue", json!({"Value":"A"}));
    twin_add(&mut editor, 0, "Twin", "StringValue", json!({"Value":"B"}));
    let mut studio = twin_document();
    let renamed = twin_add(&mut studio, 0, "Twin", "StringValue", json!({"Value":"B"}));
    review_uid(&mut studio, renamed, 1);
    studio.instances[renamed].settings_id = "studio-known".into();
    let other = twin_add(&mut studio, 0, "Twin", "StringValue", json!({"Value":"A"}));
    studio.instances[other].settings_id = "studio-other".into();
    let conflicts = twin_align(&mut editor, &mut studio);
    std::eprintln!(
        "renamed identity: conflicts={conflicts:?}; expected_id={}, actual_id={}",
        editor.instances[known].settings_id,
        studio.instances[renamed].settings_id
    );
    assert!(
        conflicts
            .iter()
            .any(|value| value.contains("same instance identity at different paths")),
        "known identity was silently reassigned"
    );
    assert_eq!(
        studio.instances[renamed].settings_id,
        editor.instances[known].settings_id
    );
    assert_eq!(
        persistent_identity(&studio.instances[renamed]),
        persistent_identity(&editor.instances[known])
    );
}

#[test]
fn review_tolerant_reference_equality_is_not_transitive() {
    let mut editor = twin_document();
    for x in [0.0, 0.00015] {
        twin_add(
            &mut editor,
            0,
            "MotorAttachment",
            "Attachment",
            json!({"CFrame":{"_type":"CFrame", "components":[x,0,0,1,0,0,0,1,0,0,0,1]}}),
        );
    }
    let reference = twin_ref(&editor, 1);
    let motor = twin_add(
        &mut editor,
        0,
        "Motor",
        "HingeConstraint",
        json!({"Attachment0":reference}),
    );
    let mut studio = editor.clone();
    studio.instances[2].properties["CFrame"]["components"][0] = json!(0.00030);
    studio.instances[motor]
        .properties
        .insert("Attachment0".into(), twin_ref(&editor, 2));
    assert!(!reconciliation_property_values_equal(
        "Attachment",
        "CFrame",
        editor.instances[1].properties.get("CFrame"),
        studio.instances[2].properties.get("CFrame")
    ));
    let conflicts = twin_align(&mut editor, &mut studio);
    std::eprintln!(
        "nontransitive reference: conflicts={conflicts:?}; ref={}",
        studio.instances[motor].properties["Attachment0"]
    );
    assert!(
        conflicts.iter().any(|value| value.contains("Attachment0")),
        "0 ~ 0.00015 ~ 0.00030 must not imply 0 ~ 0.00030"
    );
}

#[test]
fn review_repaired_children_keep_unmatched_parent_assignment() {
    let mut editor = twin_document();
    for marker in ["A", "B"] {
        let parent = twin_add(&mut editor, 0, "Garage", "Model", json!({}));
        editor.instances[parent]
            .attributes
            .insert("Marker".into(), json!(marker));
        for value in ["one", "two"] {
            twin_add(
                &mut editor,
                parent,
                "Twin",
                "StringValue",
                json!({"Value":value}),
            );
        }
    }
    let mut studio = twin_observation(&editor);
    for instance in &mut studio.instances {
        if instance.name == "Garage" {
            instance
                .attributes
                .insert("Marker".into(), json!("changed"));
        }
    }
    let resolved = pairing::twins::resolve(&editor, &studio);
    let targets = resolved.partners.iter().flatten().collect::<HashSet<_>>();
    assert_eq!(targets.len(), studio.instances.len());
    for (source, target) in resolved.partners.iter().enumerate() {
        let target = target.unwrap();
        assert_eq!(
            studio.instances[source]
                .parent_index
                .and_then(|p| resolved.partners[p]),
            editor.instances[target].parent_index
        );
        if studio.instances[source].name == "Twin" {
            assert_eq!(
                studio.instances[source].properties,
                editor.instances[target].properties
            );
        }
    }
    let conflicts = twin_align(&mut editor, &mut studio);
    assert_eq!(conflicts.len(), 2, "{conflicts:?}");
    assert!(
        conflicts
            .iter()
            .all(|value| value.contains("attribute Marker")),
        "{conflicts:?}"
    );
    assert_twin_ids_unique(&studio);
}

#[test]
fn review_external_and_internal_references_follow_swapped_subtrees() {
    let mut editor = twin_document();
    let mut parts = Vec::new();
    for transparency in [0.2, 0.8] {
        let model = twin_add(&mut editor, 0, "Model", "Model", json!({}));
        let part = twin_add(
            &mut editor,
            model,
            "Part",
            "Part",
            json!({"Transparency":transparency}),
        );
        let primary = twin_ref(&editor, part);
        editor.instances[model]
            .properties
            .insert("PrimaryPart".into(), primary);
        let reference = twin_ref(&editor, part);
        twin_add(
            &mut editor,
            model,
            "Pointer",
            "ObjectValue",
            json!({"Value":reference}),
        );
        parts.push(part);
    }
    for (index, part) in parts.into_iter().enumerate() {
        let reference = twin_ref(&editor, part);
        twin_add(
            &mut editor,
            0,
            &format!("Outside{index}"),
            "ObjectValue",
            json!({"Value":reference}),
        );
    }
    let mut studio = twin_observation(&editor);
    let conflicts = twin_align(&mut editor, &mut studio);
    assert!(conflicts.is_empty(), "{conflicts:?}");
    assert_twin_ids_unique(&studio);
    assert!(settings_documents_equivalent(&editor, &studio));
}

#[test]
fn review_cyclic_groups_validate_atomically() {
    let mut editor = twin_document();
    let a = [
        twin_add(&mut editor, 0, "A", "ObjectValue", json!({})),
        twin_add(&mut editor, 0, "A", "ObjectValue", json!({})),
    ];
    let b = [
        twin_add(&mut editor, 0, "B", "ObjectValue", json!({})),
        twin_add(&mut editor, 0, "B", "ObjectValue", json!({})),
    ];
    for (side, (a, b)) in a.into_iter().zip(b).enumerate() {
        editor.instances[a]
            .attributes
            .insert("Marker".into(), json!(side));
        editor.instances[b]
            .attributes
            .insert("Marker".into(), json!(side));
        let to_b = twin_ref(&editor, b);
        let to_a = twin_ref(&editor, a);
        editor.instances[a].properties.insert("Value".into(), to_b);
        editor.instances[b].properties.insert("Value".into(), to_a);
    }
    let mut studio = twin_observation(&editor);
    let conflicts = twin_align(&mut editor, &mut studio);
    assert!(conflicts.is_empty(), "{conflicts:?}");
    assert_twin_ids_unique(&studio);
    assert!(settings_documents_equivalent(&editor, &studio));
}

#[test]
fn review_package_links_and_class_paths_follow_swapped_models() {
    let mut editor = twin_document();
    for (marker, class) in [("A", "Part"), ("B", "Folder")] {
        let parent = twin_add(&mut editor, 0, "Garage", "Model", json!({}));
        twin_add(&mut editor, parent, "Child", class, json!({}));
        twin_add(
            &mut editor,
            parent,
            "Marker",
            "StringValue",
            json!({"Value":marker}),
        );
        twin_add(
            &mut editor,
            parent,
            "PackageLink",
            "PackageLink",
            json!({"PackageContent":"rbxassetid://42"}),
        );
    }
    let studio = twin_observation(&editor);
    let (merged, conflicts) = merge_snapshots(
        None,
        &review_snapshot(&editor),
        &review_snapshot(&studio),
        ConflictPreference::None,
    )
    .unwrap();
    assert!(conflicts.is_empty(), "{conflicts:?}");
    let result = settings_document(merged.entries.values().next()).unwrap();
    assert_eq!(result.instances.len(), editor.instances.len());
    assert_twin_ids_unique(&result);
    assert!(settings_documents_equivalent(&editor, &result));
}

#[test]
fn review_package_updates_do_not_block_repaired_parent_pairing() {
    let mut editor = twin_document();
    for (marker, class) in [("A", "Part"), ("B", "Folder")] {
        let parent = twin_add(&mut editor, 0, "Garage", "Model", json!({}));
        twin_add(&mut editor, parent, "Child", class, json!({}));
        twin_add(
            &mut editor,
            parent,
            "Marker",
            "StringValue",
            json!({"Value":marker}),
        );
        twin_add(
            &mut editor,
            parent,
            "PackageLink",
            "PackageLink",
            json!({"PackageContent":"rbxassetid://42", "VersionNumber":1}),
        );
    }
    let mut studio = twin_observation(&editor);
    for instance in &mut studio.instances {
        if instance.class_name == "PackageLink" {
            instance.properties.insert("VersionNumber".into(), json!(2));
            instance.properties.insert("AutoUpdate".into(), json!(true));
            instance
                .attributes
                .insert("StudioMetadata".into(), json!("updated"));
        }
    }
    let (merged, conflicts) = merge_snapshots(
        None,
        &review_snapshot(&editor),
        &review_snapshot(&studio),
        ConflictPreference::None,
    )
    .unwrap();
    std::eprintln!("package-update conflicts={conflicts:?}");
    assert!(conflicts.is_empty(), "{conflicts:?}");
    let result = settings_document(merged.entries.values().next()).unwrap();
    assert_eq!(result.instances.len(), editor.instances.len());
    assert!(
        result
            .instances
            .iter()
            .filter(|i| i.class_name == "PackageLink")
            .all(|i| i.properties["VersionNumber"] == json!(2)
                && i.properties["AutoUpdate"] == json!(true)
                && i.attributes["StudioMetadata"] == json!("updated"))
    );
    assert_twin_ids_unique(&result);
}

#[test]
fn review_perf_repeated_references_to_large_identical_twins() {
    let _ = rbx_reflection_database::get().unwrap();
    let mut timings = Vec::new();
    for size in [250, 500, 1000, 2000] {
        let mut editor = twin_document();
        let mut targets = Vec::new();
        for _ in 0..2 {
            let model = twin_add(&mut editor, 0, "Model", "Model", json!({}));
            targets.push(model);
            for index in 0..size {
                twin_add(
                    &mut editor,
                    model,
                    &format!("Child{index}"),
                    "Folder",
                    json!({}),
                );
            }
        }
        for index in 0..size {
            let reference = twin_ref(&editor, targets[0]);
            twin_add(
                &mut editor,
                0,
                &format!("Pointer{index}"),
                "ObjectValue",
                json!({"Value":reference}),
            );
        }
        let mut studio = editor.clone();
        for instance in &mut studio.instances {
            if instance.class_name == "ObjectValue" {
                instance
                    .properties
                    .insert("Value".into(), twin_ref(&editor, targets[1]));
            }
        }
        let mut fastest = std::time::Duration::MAX;
        for _ in 0..3 {
            let started = Instant::now();
            let result = pairing::twins::resolve(&editor, &studio);
            fastest = fastest.min(started.elapsed());
            assert!(result.partners.iter().all(Option::is_some));
            // Work, unlike wall time, is deterministic under concurrent tests.
            assert_eq!(
                result.reference_comparisons, 1,
                "the same directed subtree pair must be cached"
            );
        }
        timings.push(fastest);
        std::eprintln!(
            "repeated-reference cost: children={size}, refs={size}, instances={}, elapsed_ms={:.3}",
            editor.instances.len(),
            fastest.as_secs_f64() * 1000.0
        );
    }
    // Eight times the document size allows twice linear growth plus scheduler
    // noise. The former quadratic implementation exceeded 50x on this family.
    assert!(
        timings[3] <= timings[0] * 16 + std::time::Duration::from_millis(25),
        "repeated-reference scaling: {timings:?}"
    );
}

#[test]
fn review_perf_512_late_attribute_mismatch() {
    let _ = rbx_reflection_database::get().unwrap();
    for size in [128, 256, 512] {
        let mut editor = twin_document();
        for _ in 0..size {
            let index = twin_add(&mut editor, 0, "Twin", "Folder", json!({}));
            editor.instances[index]
                .attributes
                .insert("Payload".into(), json!("x".repeat(64 * 1024)));
            editor.instances[index]
                .attributes
                .insert("ZMarker".into(), json!(0));
        }
        let mut studio = editor.clone();
        studio.instances[size]
            .attributes
            .insert("ZMarker".into(), json!(1));
        let started = Instant::now();
        let result = pairing::twins::resolve(&editor, &studio);
        std::eprintln!(
            "large-attribute late mismatch: group={size}, elapsed_ms={:.3}",
            started.elapsed().as_secs_f64() * 1000.0
        );
        assert_eq!(result.partners, (0..=size).map(Some).collect::<Vec<_>>());
    }
}

#[test]
fn review_perf_95k_complete_first_comparison() {
    let _ = rbx_reflection_database::get().unwrap();
    let mut editor = twin_document();
    for group in 0..47_499 {
        for value in ["A", "B"] {
            twin_add(
                &mut editor,
                0,
                &format!("Twin{group}"),
                "StringValue",
                json!({"Value":value}),
            );
        }
    }
    twin_add(&mut editor, 0, "Singleton", "Folder", json!({}));
    assert_eq!(editor.instances.len(), 95_000);
    let mut studio = twin_observation(&editor);
    let started = Instant::now();
    let result = pairing::twins::resolve(&editor, &studio);
    let repair_ms = started.elapsed().as_secs_f64() * 1000.0;
    assert!(result.partners.iter().all(Option::is_some));
    let started = Instant::now();
    let conflicts = twin_align(&mut editor, &mut studio);
    let full_ms = started.elapsed().as_secs_f64() * 1000.0;
    std::eprintln!(
        "95k synthetic: resolve_ms={repair_ms:.3}, align_first_pairing_ms={full_ms:.3}, conflicts={}",
        conflicts.len()
    );
    assert!(conflicts.is_empty(), "{conflicts:?}");
    assert_twin_ids_unique(&studio);
}

#[test]
fn review_cframe_position_and_rotation_overlap_is_pairwise() {
    for (component, step) in [(0, 0.0003), (3, 0.000012)] {
        let mut editor = twin_document();
        for multiple in [0.0, 1.0, 2.0] {
            let mut frame = json!({"_type":"CFrame", "components":[0,0,0,1,0,0,0,1,0,0,0,1]});
            frame["components"][component] = json!(multiple * step);
            twin_add(
                &mut editor,
                0,
                "Twin",
                "Attachment",
                json!({"CFrame":frame}),
            );
        }
        for (values, expected) in [
            ([0.5, 0.0, 2.0], vec![Some(0), Some(2), Some(1), Some(3)]),
            ([0.0, 0.0, 1.0], vec![Some(0), Some(1), Some(2), Some(3)]),
        ] {
            let mut studio = editor.clone();
            for (instance, multiple) in studio.instances[1..].iter_mut().zip(values) {
                instance.properties["CFrame"]["components"][component] = json!(multiple * step);
            }
            let resolved = pairing::twins::resolve(&editor, &studio);
            assert_eq!(resolved.partners, expected);
        }
    }
}

#[test]
fn moved_unique_id_keeps_its_partner_and_reports_the_move() {
    let mut editor = twin_document();
    let left = twin_add(&mut editor, 0, "Left", "Folder", json!({}));
    let right = twin_add(&mut editor, 0, "Right", "Folder", json!({}));
    let known = twin_add(
        &mut editor,
        left,
        "Twin",
        "StringValue",
        json!({"Value":"known"}),
    );
    review_uid(&mut editor, known, 1);
    for value in ["A", "B"] {
        twin_add(
            &mut editor,
            0,
            "Repair",
            "StringValue",
            json!({"Value":value}),
        );
    }
    let mut studio = twin_observation(&editor);
    let moved = studio
        .instances
        .iter()
        .position(|i| persistent_identity(i).is_some())
        .unwrap();
    let destination = studio
        .instances
        .iter()
        .position(|i| i.name == "Right")
        .unwrap();
    studio.instances[moved].parent_index = Some(destination);
    let resolved = pairing::twins::resolve(&editor, &studio);
    assert_eq!(resolved.locked[moved], Some(known));
    assert_eq!(resolved.partners[moved], Some(known));
    assert_eq!(resolved.partners[destination], Some(right));
    let conflicts = twin_align(&mut editor, &mut studio);
    assert!(
        conflicts
            .iter()
            .any(|c| c.contains("same instance identity at different paths")),
        "{conflicts:?}"
    );
    assert_eq!(
        studio.instances[moved].settings_id,
        editor.instances[known].settings_id
    );
    assert_eq!(studio.instances[moved].parent_index, Some(destination));
    assert_twin_ids_unique(&studio);
}

#[test]
fn added_twin_subtrees_and_references_survive_every_insertion_slot() {
    for order in [[3, 2, 1], [2, 3, 1], [2, 1, 3]] {
        let mut editor = twin_document();
        let mut studio = twin_document();
        for (document, ids) in [(&mut editor, &[1, 2][..]), (&mut studio, &order[..])] {
            for &uid in ids {
                let model = twin_add(document, 0, "Twin", "Model", json!({}));
                review_uid(document, model, uid);
                let child = twin_add(
                    document,
                    model,
                    "Child",
                    "StringValue",
                    json!({"Value":uid.to_string()}),
                );
                let reference = twin_ref(document, child);
                twin_add(
                    document,
                    model,
                    "Pointer",
                    "ObjectValue",
                    json!({"Value":reference}),
                );
            }
        }
        let ids = studio
            .instances
            .iter()
            .map(|instance| {
                (
                    instance.settings_id.clone(),
                    format!("studio-{}", instance.settings_id),
                )
            })
            .collect::<HashMap<_, _>>();
        for instance in &mut studio.instances {
            instance.settings_id = ids[&instance.settings_id].clone();
            remap_record_reference_ids(&mut instance.properties, &ids);
        }
        let (merged, conflicts) = merge_snapshots(
            None,
            &review_snapshot(&editor),
            &review_snapshot(&studio),
            ConflictPreference::None,
        )
        .unwrap();
        assert!(conflicts.is_empty(), "order={order:?}: {conflicts:?}");
        let document = settings_document(merged.entries.values().next()).unwrap();
        assert_eq!(document.instances.len(), 10);
        assert_twin_ids_unique(&document);
        for uid in [1, 2, 3] {
            let identity = format!("{uid:032x}");
            let model = document
                .instances
                .iter()
                .position(|i| persistent_identity(i) == Some(identity.as_str()))
                .unwrap();
            let child = document
                .instances
                .iter()
                .find(|i| i.parent_index == Some(model) && i.name == "Child")
                .unwrap();
            let pointer = document
                .instances
                .iter()
                .find(|i| i.parent_index == Some(model) && i.name == "Pointer")
                .unwrap();
            assert_eq!(child.properties["Value"], uid.to_string());
            assert_eq!(pointer.properties["Value"]["settingsId"], child.settings_id);
        }
    }
}

#[test]
fn reference_to_a_twin_with_a_changed_descendant_is_not_interchangeable() {
    let mut editor = twin_document();
    let mut models = Vec::new();
    let mut children = Vec::new();
    for uid in [1, 2] {
        let model = twin_add(&mut editor, 0, "Twin", "Model", json!({}));
        review_uid(&mut editor, model, uid);
        models.push(model);
        children.push(twin_add(
            &mut editor,
            model,
            "Child",
            "StringValue",
            json!({"Value":"same"}),
        ));
    }
    let reference = twin_ref(&editor, models[0]);
    let pointer = twin_add(
        &mut editor,
        0,
        "Outside",
        "ObjectValue",
        json!({"Value":reference}),
    );
    let mut studio = editor.clone();
    studio.instances[children[1]]
        .properties
        .insert("Value".into(), json!("changed"));
    studio.instances[pointer]
        .properties
        .insert("Value".into(), twin_ref(&editor, models[1]));
    let conflicts = twin_align(&mut editor, &mut studio);
    assert!(
        conflicts
            .iter()
            .any(|c| c.contains("Workspace.Outside: property Value")),
        "{conflicts:?}"
    );
    assert_eq!(
        studio.instances[pointer].properties["Value"],
        twin_ref(&editor, models[1])
    );
}

#[test]
fn unpaired_settings_id_collision_cannot_steal_a_locked_identity() {
    let mut editor = twin_document();
    let mut studio = twin_document();
    for (document, ids) in [(&mut editor, &[1, 2][..]), (&mut studio, &[2, 3, 1][..])] {
        for &uid in ids {
            let index = twin_add(document, 0, "Twin", "Part", json!({}));
            review_uid(document, index, uid);
        }
    }
    // Even when raw settings IDs conflict, they must not normalize the added
    // UniqueId to the identity of a different sibling or create duplicate IDs.
    let _ = twin_align(&mut editor, &mut studio);
    assert_twin_ids_unique(&studio);
    assert_eq!(
        persistent_identity(&studio.instances[2]),
        Some("00000000000000000000000000000003")
    );
    assert!(
        editor
            .instances
            .iter()
            .all(|i| i.settings_id != studio.instances[2].settings_id)
    );
    assert_eq!(
        studio.instances[1].settings_id,
        editor.instances[2].settings_id
    );
    assert_eq!(
        studio.instances[3].settings_id,
        editor.instances[1].settings_id
    );
}

fn cyclic_garages() -> SettingsBytecode {
    let mut editor = twin_document();
    for marker in ["left", "right"] {
        let garage = twin_add(&mut editor, 0, "Garage", "Model", json!({}));
        editor.instances[garage]
            .attributes
            .insert("Marker".into(), json!(marker));
        let groups = ["A", "B", "C"].map(|name| {
            [0, 1].map(|side| {
                let index = twin_add(&mut editor, garage, name, "ObjectValue", json!({}));
                if name == "C" {
                    editor.instances[index]
                        .attributes
                        .insert("Marker".into(), json!(side));
                }
                index
            })
        });
        // A -> C -> B -> A. Reversed storage visits C, B, A; repairing A
        // invalidates B's tentative positional choice, requiring a revisit.
        for (group, target) in [(0, 2), (1, 0), (2, 1)] {
            for (source, target) in groups[group].into_iter().zip(groups[target]) {
                let reference = twin_ref(&editor, target);
                editor.instances[source]
                    .properties
                    .insert("Value".into(), reference);
            }
        }
    }
    editor
}

#[test]
fn nested_reference_cycles_revisit_dependents_before_publication() {
    let mut editor = cyclic_garages();
    let mut studio = twin_observation(&editor);
    let conflicts = twin_align(&mut editor, &mut studio);
    assert!(conflicts.is_empty(), "{conflicts:?}");
    assert!(settings_documents_equivalent(&editor, &studio));
    assert_twin_ids_unique(&studio);
}

#[test]
fn invalid_nested_reference_cycle_rolls_back_without_blocking_a_valid_component() {
    let mut editor = cyclic_garages();
    let target = twin_add(&mut editor, 0, "Outside", "Part", json!({}));
    for value in ["one", "two"] {
        twin_add(
            &mut editor,
            0,
            "Independent",
            "StringValue",
            json!({"Value":value}),
        );
    }
    let mut studio = twin_observation(&editor);
    let target_id = format!("studio-{}", editor.instances[target].settings_id);
    for instance in &mut studio.instances {
        if instance.name == "C" {
            instance.properties.insert(
                "Value".into(),
                json!({"_type":"Ref", "settingsId":target_id}),
            );
        }
    }
    let resolved = pairing::twins::resolve(&editor, &studio);
    assert_eq!(
        resolved.partners.iter().flatten().count(),
        resolved
            .partners
            .iter()
            .flatten()
            .collect::<HashSet<_>>()
            .len()
    );
    for (source, instance) in studio
        .instances
        .iter()
        .enumerate()
        .filter(|(_, i)| i.name == "Independent")
    {
        assert_eq!(
            instance.properties,
            editor.instances[resolved.partners[source].unwrap()].properties
        );
    }
    let conflicts = twin_align(&mut editor, &mut studio);
    assert!(!conflicts.is_empty());
    assert_twin_ids_unique(&studio);
    assert!(
        studio
            .instances
            .iter()
            .filter(|i| i.name == "C")
            .all(|i| i.properties["Value"]["settingsId"] == editor.instances[target].settings_id)
    );
}

#[test]
fn protected_package_metadata_does_not_allow_a_package_link_rename() {
    let mut editor = twin_document();
    let package = twin_add(&mut editor, 0, "Package", "Model", json!({}));
    let link = twin_add(
        &mut editor,
        package,
        "PackageLink",
        "PackageLink",
        json!({"VersionNumber":1}),
    );
    review_uid(&mut editor, link, 1);
    let mut studio = editor.clone();
    studio.instances[link].name = "Renamed".into();
    studio.instances[link]
        .properties
        .insert("VersionNumber".into(), json!(2));
    let conflicts = twin_align(&mut editor, &mut studio);
    assert!(
        conflicts
            .iter()
            .any(|c| c.contains("directly renames or reparents PackageLink")),
        "{conflicts:?}"
    );
    assert_eq!(
        studio.instances[link].settings_id,
        editor.instances[link].settings_id
    );
}

#[test]
fn review_packages_with_different_package_ids_are_not_interchangeable() {
    let mut editor = twin_document();
    let mut models = Vec::new();
    for id in [100, 200] {
        let model = twin_add(&mut editor, 0, "Twin", "Model", json!({}));
        models.push(model);
        review_uid(&mut editor, model, id);
        twin_add(
            &mut editor,
            model,
            "PackageLink",
            "PackageLink",
            json!({"PackageId": format!("rbxassetid://{id}"), "VersionNumber": 1}),
        );
    }
    let reference = twin_ref(&editor, models[0]);
    let pointer = twin_add(
        &mut editor,
        0,
        "Pointer",
        "ObjectValue",
        json!({"Value": reference}),
    );
    let mut studio = editor.clone();
    let moved = twin_ref(&studio, models[1]);
    studio.instances[pointer]
        .properties
        .insert("Value".into(), moved);
    let conflicts = twin_align(&mut editor, &mut studio);
    assert!(
        conflicts
            .iter()
            .any(|conflict| conflict.contains("Pointer") && conflict.contains("Value")),
        "a reference moved between different packages must stay a difference: {conflicts:?}"
    );
}

#[test]
fn review_reversed_identical_package_bodies_keep_their_package_identity() {
    let mut editor = twin_document();
    for id in [100, 200] {
        let model = twin_add(&mut editor, 0, "Twin", "Model", json!({}));
        twin_add(
            &mut editor,
            model,
            "PackageLink",
            "PackageLink",
            json!({"PackageId": format!("rbxassetid://{id}"), "VersionNumber": 1}),
        );
    }
    let mut studio = twin_observation(&editor);
    let original = editor.clone();
    let conflicts = twin_align(&mut editor, &mut studio);
    assert!(conflicts.is_empty(), "{conflicts:?}");
    for (before, after) in original.instances.iter().zip(&editor.instances) {
        assert_eq!(
            before.properties.get("PackageId"),
            after.properties.get("PackageId")
        );
    }
}
