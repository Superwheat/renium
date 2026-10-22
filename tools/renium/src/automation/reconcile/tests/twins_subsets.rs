use super::*;

fn namespace_studio_ids(document: &mut SettingsBytecode) {
    let ids = document
        .instances
        .iter()
        .enumerate()
        .map(|(index, instance)| (instance.settings_id.clone(), format!("studio-{index}")))
        .collect::<HashMap<_, _>>();
    for instance in &mut document.instances {
        instance.settings_id = ids[&instance.settings_id].clone();
        remap_record_reference_ids(&mut instance.properties, &ids);
        remap_record_reference_ids(&mut instance.attributes, &ids);
    }
}

fn ref_subset(targets: &[usize], studio: bool) -> SettingsBytecode {
    let mut document = twin_document();
    for name in ["A", "B", "C"] {
        twin_add(&mut document, 0, name, "Part", json!({}));
    }
    for target in targets {
        let reference = twin_ref(&document, *target);
        twin_add(
            &mut document,
            0,
            "Twin",
            "ObjectValue",
            json!({"Value":reference}),
        );
    }
    if studio {
        let ids = document
            .instances
            .iter()
            .map(|i| (i.settings_id.clone(), format!("studio-{}", i.settings_id)))
            .collect::<HashMap<_, _>>();
        for instance in &mut document.instances {
            instance.settings_id = ids[&instance.settings_id].clone();
            remap_record_reference_ids(&mut instance.properties, &ids);
        }
    }
    document
}

#[test]
fn larger_studio_reference_group_can_choose_a_different_source_subset() {
    let mut editor = ref_subset(&[1, 2], false);
    let mut studio = ref_subset(&[3, 1, 2], true);
    let conflicts = twin_align(&mut editor, &mut studio);
    std::eprintln!("larger Studio external-reference subset: {conflicts:?}");
    assert!(
        conflicts.is_empty(),
        "all editor twins have complete reference-valid Studio counterparts: {conflicts:?}"
    );
    assert_eq!(
        studio.instances[5].settings_id,
        editor.instances[4].settings_id
    );
    assert_eq!(
        studio.instances[6].settings_id,
        editor.instances[5].settings_id
    );
    assert!(
        editor
            .instances
            .iter()
            .all(|i| i.settings_id != studio.instances[4].settings_id)
    );
}

#[test]
fn larger_editor_reference_group_can_choose_a_different_target_subset() {
    let mut editor = ref_subset(&[3, 1, 2], false);
    let mut studio = ref_subset(&[1, 2], true);
    let conflicts = twin_align(&mut editor, &mut studio);
    assert!(conflicts.is_empty(), "{conflicts:?}");
    assert_eq!(
        studio.instances[4].settings_id,
        editor.instances[5].settings_id
    );
    assert_eq!(
        studio.instances[5].settings_id,
        editor.instances[6].settings_id
    );
}

#[test]
fn larger_studio_reference_group_keeps_publication_injective() {
    let mut editor = ref_subset(&[1, 2], false);
    let mut studio = ref_subset(&[2, 2, 1], true);
    let conflicts = twin_align(&mut editor, &mut studio);
    assert!(conflicts.is_empty(), "{conflicts:?}");
    assert_twin_ids_unique(&studio);
    let matched = studio
        .instances
        .iter()
        .filter(|s| {
            editor
                .instances
                .iter()
                .any(|e| e.settings_id == s.settings_id)
        })
        .count();
    assert_eq!(matched, editor.instances.len());
}

fn subset_snapshot(document: &SettingsBytecode) -> ProjectSnapshot {
    ProjectSnapshot {
        entries: [(
            PathBuf::from("instances/Workspace.renium"),
            SnapshotEntry::File(encode_settings_bytecode(document).unwrap()),
        )]
        .into_iter()
        .collect(),
    }
}

#[test]
fn studio_deletion_of_a_reference_twin_survives_baseline_merge() {
    let editor = ref_subset(&[3, 1, 2], false);
    let baseline = subset_snapshot(&editor);
    let mut studio = editor.clone();
    let deleted = studio.instances.remove(4).settings_id;
    let (merged, conflicts, changes) = merge::merge_snapshots_with_changes(
        Some(&baseline),
        &subset_snapshot(&editor),
        &subset_snapshot(&studio),
        ConflictPreference::None,
        None,
    )
    .unwrap();
    assert!(conflicts.is_empty(), "{conflicts:?}");
    let result = settings_document(merged.entries.values().next()).unwrap();
    assert_eq!(result.instances.len(), editor.instances.len() - 1);
    assert!(result.instances.iter().all(|i| i.settings_id != deleted));
    assert!(settings_documents_equivalent(&result, &studio));
    assert!(!changes.editor.is_empty());
    assert!(changes.studio.is_empty());
}

fn dependent_subset(studio: bool) -> SettingsBytecode {
    let mut document = ref_subset(if studio { &[2, 1] } else { &[1, 2] }, studio);
    for instance in document.instances.iter_mut().skip(4) {
        instance.name = "Target".into();
    }
    for target in if studio { &[3, 4, 5][..] } else { &[4][..] } {
        let reference = twin_ref(&document, *target);
        twin_add(
            &mut document,
            0,
            "Dependent",
            "ObjectValue",
            json!({"Value":reference}),
        );
    }
    document
}

#[test]
fn newly_selected_sources_wait_for_their_reference_dependencies() {
    let mut editor = dependent_subset(false);
    let mut studio = dependent_subset(true);
    namespace_studio_ids(&mut studio);
    let resolved = pairing::twins::resolve(&editor, &studio);
    assert_eq!(&resolved.partners[6..], &[None, None, Some(6)]);
    assert_eq!(&resolved.proven[6..], &[false, false, true]);
    let conflicts = twin_align(&mut editor, &mut studio);
    assert!(conflicts.is_empty(), "{conflicts:?}");
    assert_twin_ids_unique(&studio);
    assert_eq!(
        studio.instances[8].settings_id,
        editor.instances[6].settings_id
    );
    assert_eq!(
        studio.instances[8].properties["Value"],
        editor.instances[6].properties["Value"]
    );
}

#[test]
fn references_to_displaced_sources_cannot_use_their_tentative_partners() {
    let mut editor = ref_subset(&[1, 2], false);
    let mut studio = ref_subset(&[3, 1, 2], true);
    for (document, target) in [(&mut editor, 4), (&mut studio, 4)] {
        for _ in 0..2 {
            let reference = twin_ref(document, target);
            twin_add(
                document,
                0,
                "Dependent",
                "ObjectValue",
                json!({"Value":reference}),
            );
        }
    }
    namespace_studio_ids(&mut studio);
    let resolved = pairing::twins::resolve(&editor, &studio);
    assert_eq!(resolved.partners[4], None);
    assert!(resolved.proven[5] && resolved.proven[6]);
    assert!(!resolved.proven[7] && !resolved.proven[8]);
    let conflicts = twin_align(&mut editor, &mut studio);
    assert!(
        conflicts.iter().any(|c| c.contains("Dependent")),
        "{conflicts:?}"
    );
    assert_twin_ids_unique(&studio);
    assert_eq!(
        studio.instances[7].properties["Value"],
        twin_ref(&studio, 4)
    );
}

#[test]
fn subset_replacement_preserves_locks_inside_abandoned_subtrees() {
    let mut editor = ref_subset(&[1, 2], false);
    let mut studio = ref_subset(&[3, 1, 2], true);
    for document in [&mut editor, &mut studio] {
        for parent in 4..document.instances.len() {
            twin_add(document, parent, "Child", "Folder", json!({}));
        }
    }
    // The first Studio twin cannot match either editor root, but its child has
    // a persistent identity. Abandoning that subtree must retain the lock.
    let moved = twin_add(&mut editor, 0, "Moved", "Folder", json!({}));
    for (document, index) in [(&mut editor, moved), (&mut studio, 7)] {
        document.instances[index].properties.insert(
            "UniqueId".into(),
            json!({"_type":"UniqueId", "value":"00000000000000000000000000000001"}),
        );
    }
    let resolved = pairing::twins::resolve(&editor, &studio);
    assert_eq!(resolved.partners[4], None);
    assert_eq!(resolved.partners[7], Some(moved));
    assert_eq!(resolved.locked[7], Some(moved));
    assert!(!resolved.proven[7]);
    assert!(resolved.proven[5] && resolved.proven[6]);
    let conflicts = twin_align(&mut editor, &mut studio);
    assert!(
        !conflicts.is_empty(),
        "the locked child's move remains a conflict"
    );
    assert_twin_ids_unique(&studio);
}

#[test]
fn reference_subset_permutations_keep_the_final_assignment_injective_and_proven() {
    for mut combination in 0..243 {
        let [a, b, c, d, e] = std::array::from_fn(|_| {
            let value = combination % 3 + 1;
            combination /= 3;
            value
        });
        for swapped in [false, true] {
            let small = [a, b];
            let large = [c, d, e];
            let (left, right) = if swapped {
                (&large[..], &small[..])
            } else {
                (&small[..], &large[..])
            };
            let editor = ref_subset(left, false);
            let studio = ref_subset(right, true);
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
            let can_pair = (1..=3).all(|target| {
                small.iter().filter(|&&v| v == target).count()
                    <= large.iter().filter(|&&v| v == target).count()
            });
            assert_eq!(
                resolved.proven[4..].iter().filter(|&&p| p).count(),
                if can_pair { 2 } else { 0 },
                "{left:?} vs {right:?}"
            );
            for (source, &target) in resolved.partners.iter().enumerate().skip(4) {
                if resolved.proven[source] {
                    assert_eq!(right[source - 4], left[target.unwrap() - 4]);
                }
            }
        }
    }
}
