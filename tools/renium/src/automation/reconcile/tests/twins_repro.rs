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

#[test]
#[ignore = "requires private RENIUM_REPRO_BASE, RENIUM_REPRO_EDITOR and RENIUM_REPRO_STUDIO store paths"]
#[allow(clippy::cognitive_complexity)]
fn repro_merge_from_store_files_lists_pushes() {
    let read = |name: &str| {
        let path =
            std::env::var_os(name).unwrap_or_else(|| panic!("set {name} to a .renium store"));
        std::fs::read(path).unwrap()
    };
    let prefix = std::env::var("RENIUM_REPRO_PREFIX").unwrap_or_default();
    crate::app::context::set_cli_project(
        std::env::var_os("RENIUM_REPRO_PROJECT").map(PathBuf::from),
    );
    let snapshot = |bytes: Vec<u8>| ProjectSnapshot {
        entries: [(
            PathBuf::from("instances/Workspace.renium"),
            SnapshotEntry::File(bytes),
        )]
        .into_iter()
        .collect(),
    };
    let base = snapshot(read("RENIUM_REPRO_BASE"));
    let editor = snapshot(read("RENIUM_REPRO_EDITOR"));
    let studio = snapshot(read("RENIUM_REPRO_STUDIO"));
    let (merged, conflicts, changes) = merge_snapshots_with_changes(
        Some(&base),
        &editor,
        &studio,
        ConflictPreference::None,
        None,
    )
    .unwrap();
    std::eprintln!(
        "conflicts={} editor_changed={:?} studio_changed={:?}",
        conflicts.len(),
        changes.editor,
        changes.studio
    );
    for conflict in conflicts.iter().take(10) {
        std::eprintln!("conflict: {conflict}");
    }
    let merged_doc = settings_document(merged.entries.values().next()).unwrap();
    let studio_doc = settings_document(studio.entries.values().next()).unwrap();
    let path_of = |document: &SettingsBytecode, index: usize| {
        let mut names = Vec::new();
        let mut next = Some(index);
        while let Some(current) = next {
            names.push(document.instances[current].name.clone());
            next = document.instances[current].parent_index;
        }
        names.reverse();
        names.join(".")
    };
    let studio_by_id = studio_doc
        .instances
        .iter()
        .enumerate()
        .map(|(index, instance)| (instance.settings_id.as_str(), index))
        .collect::<HashMap<_, _>>();
    let mut shown = 0;
    for (index, instance) in merged_doc.instances.iter().enumerate() {
        let path = path_of(&merged_doc, index);
        if !path.starts_with(&prefix) {
            continue;
        }
        match studio_by_id.get(instance.settings_id.as_str()) {
            None => {
                std::eprintln!("push ADD {path} ({})", instance.settings_id);
                shown += 1;
            }
            Some(&studio_index) => {
                let other = &studio_doc.instances[studio_index];
                let other_path = path_of(&studio_doc, studio_index);
                let mut changed = instance
                    .properties
                    .iter()
                    .filter(|(name, value)| other.properties.get(*name) != Some(value))
                    .map(|(name, _)| name.clone())
                    .collect::<Vec<_>>();
                if other_path != path {
                    changed.push(format!("PATH {other_path} -> {path}"));
                }
                if !changed.is_empty() {
                    std::eprintln!("push CHANGE {path} ({}): {changed:?}", instance.settings_id);
                    shown += 1;
                }
            }
        }
        if shown >= 40 {
            break;
        }
    }
    std::eprintln!("shown {shown} push entries under {prefix:?}");
    let mut changes = changes;
    let prepared = {
        let mut map = HashMap::new();
        for (path, mut previous) in std::mem::take(&mut changes.aligned_studio) {
            let current = settings_document(merged.entries.get(&path)).unwrap();
            crate::settings::equivalence::align_equivalent_values(&current, &mut previous);
            map.insert(path, PreparedEditorSettingsChange { previous, current });
        }
        map
    };
    std::eprintln!("aligned studio documents: {}", prepared.len());
    if let Ok(probe) = std::env::var("RENIUM_REPRO_PROBE") {
        let ordinals = probe
            .split(',')
            .map(|part| part.trim().parse::<usize>().unwrap())
            .collect::<Vec<_>>();
        let paths = crate::editor::paths::build_editor_instance_paths(&studio_doc, "Workspace");
        for (index, path) in paths.iter().enumerate() {
            let Some(path) = path else { continue };
            if path.path_ordinals.len() >= ordinals.len()
                && path.path_ordinals[..ordinals.len()] == ordinals[..]
            {
                std::eprintln!(
                    "probe {:?} {:?} -> {} {} (store index {index})",
                    path.path_segments.join("."),
                    path.path_ordinals,
                    studio_doc.instances[index].class_name,
                    studio_doc.instances[index].settings_id
                );
            }
        }
    }
    for change in prepared.values() {
        let aligned = &change.previous;
        let mut id_counts: HashMap<&str, usize> = HashMap::new();
        for instance in &aligned.instances {
            *id_counts.entry(instance.settings_id.as_str()).or_default() += 1;
        }
        let duplicate_ids = id_counts.values().filter(|count| **count > 1).count();
        let desired_by_id = change
            .current
            .instances
            .iter()
            .map(|instance| (instance.settings_id.as_str(), instance))
            .collect::<HashMap<_, _>>();
        let mut class_mismatches = 0;
        let mut kept_raw_id = 0;
        let mut order_mismatches = 0;
        let raw_namespaces = studio_doc
            .instances
            .iter()
            .map(|instance| {
                instance
                    .settings_id
                    .split(':')
                    .next()
                    .unwrap_or("")
                    .to_string()
            })
            .fold(HashMap::<String, usize>::new(), |mut counts, namespace| {
                *counts.entry(namespace).or_default() += 1;
                counts
            });
        std::eprintln!("raw studio id namespaces: {raw_namespaces:?}");
        for (index, instance) in aligned.instances.iter().enumerate() {
            if studio_doc.instances[index].class_name != instance.class_name
                || studio_doc.instances[index].name != instance.name
            {
                order_mismatches += 1;
            }
            if let Some(desired) = desired_by_id.get(instance.settings_id.as_str())
                && (desired.class_name != instance.class_name || desired.name != instance.name)
            {
                class_mismatches += 1;
                let raw = &studio_doc.instances[index];
                if raw.settings_id == instance.settings_id {
                    kept_raw_id += 1;
                }
                if class_mismatches <= 5 {
                    std::eprintln!(
                        "aligned {} (raw {}) {} {} at {} paired with desired {} {} at {}",
                        instance.settings_id,
                        raw.settings_id,
                        instance.class_name,
                        instance.name,
                        path_of(&studio_doc, index),
                        desired.class_name,
                        desired.name,
                        path_of(
                            &change.current,
                            change
                                .current
                                .instances
                                .iter()
                                .position(|candidate| candidate.settings_id == instance.settings_id)
                                .unwrap()
                        )
                    );
                }
            }
        }
        std::eprintln!(
            "aligned studio: instances={} duplicate_ids={duplicate_ids} class_mismatches={class_mismatches} kept_raw_id={kept_raw_id} order_mismatches={order_mismatches} raw_instances={}",
            aligned.instances.len(),
            studio_doc.instances.len()
        );
    }
    let plan = reconciliation_push_plan_for_paths_with_prepared_settings(
        &studio,
        &merged,
        &changes.studio,
        &prepared,
        false,
        false,
    )
    .unwrap();
    // Every previous location must name a distinct Studio instance of the
    // desired name and class; Studio resolves pushes by these paths first.
    let studio_paths = crate::editor::paths::build_editor_instance_paths(&studio_doc, "Workspace");
    let studio_by_path = studio_paths
        .iter()
        .enumerate()
        .filter_map(|(index, path)| {
            path.as_ref().map(|path| {
                (
                    (path.path_segments.clone(), path.path_ordinals.clone()),
                    index,
                )
            })
        })
        .collect::<HashMap<_, _>>();
    let merged_by_id = merged_doc
        .instances
        .iter()
        .enumerate()
        .map(|(index, instance)| (instance.settings_id.as_str(), index))
        .collect::<HashMap<_, _>>();
    let mut seen: HashMap<(Vec<String>, Vec<usize>), String> = HashMap::new();
    let (mut duplicates, mut unresolved, mut mismatched, mut total) = (0, 0, 0, 0);
    for ((_, id), previous) in &plan.previous_paths {
        total += 1;
        let key = (
            previous.path_segments.clone(),
            previous.path_ordinals.clone(),
        );
        if let Some(other) = seen.insert(key.clone(), id.clone()) {
            duplicates += 1;
            if duplicates <= 10 {
                std::eprintln!(
                    "duplicate previous path {:?} for {other} and {id}",
                    previous.path_segments.join(".")
                );
            }
        }
        let Some(&studio_index) = studio_by_path.get(&key) else {
            unresolved += 1;
            if unresolved <= 10 {
                std::eprintln!(
                    "unresolved previous path {:?} {:?} for {id}",
                    previous.path_segments.join("."),
                    previous.path_ordinals
                );
            }
            continue;
        };
        let observed = &studio_doc.instances[studio_index];
        let desired = &merged_doc.instances[merged_by_id[id.as_str()]];
        let declared_replacement = plan
            .previous_class_names
            .get(&("Workspace".to_string(), id.clone()))
            .is_some_and(|class_name| *class_name == observed.class_name);
        if observed.class_name != desired.class_name && !declared_replacement {
            mismatched += 1;
            if mismatched <= 10 {
                std::eprintln!(
                    "previous path {} is {} {} but {id} wants {} {}",
                    previous.path_segments.join("."),
                    observed.class_name,
                    observed.name,
                    desired.class_name,
                    desired.name
                );
            }
        }
    }
    if let Ok(ids) = std::env::var("RENIUM_REPRO_IDS") {
        let aligned = &prepared.values().next().unwrap().previous;
        for id in ids.split(',') {
            let key = ("Workspace".to_string(), id.to_string());
            std::eprintln!(
                "id {id}: target={} recreated={} in_place={} previous={:?} aligned_studio={} merged={}",
                plan.target_settings_ids.iter().any(|t| t == id),
                plan.recreated_settings_ids.contains(id),
                plan.in_place_instances.contains(&key),
                plan.previous_paths
                    .get(&key)
                    .map(|p| p.path_ordinals.clone()),
                aligned
                    .instances
                    .iter()
                    .position(|i| i.settings_id == id)
                    .map(|i| path_of(aligned, i))
                    .unwrap_or_default(),
                merged_doc
                    .instances
                    .iter()
                    .position(|i| i.settings_id == id)
                    .map(|i| path_of(&merged_doc, i))
                    .unwrap_or_default()
            );
        }
    }
    std::eprintln!(
        "previous paths: total={total} duplicates={duplicates} unresolved={unresolved} undeclared_class_change={mismatched} replacements={} targets={} recreated={}",
        plan.previous_class_names.len(),
        plan.target_settings_ids.len(),
        plan.recreated_settings_ids.len()
    );
    assert_eq!(duplicates + unresolved + mismatched, 0);
}

#[test]
#[ignore = "requires private RENIUM_REPRO_EDITOR and RENIUM_REPRO_STUDIO store paths"]
fn repro_pairing_is_injective_and_keeps_parents() {
    let read = |name: &str| {
        let path =
            std::env::var_os(name).unwrap_or_else(|| panic!("set {name} to a .renium store"));
        decode_settings_bytecode(&std::fs::read(path).unwrap()).unwrap()
    };
    let editor = read("RENIUM_REPRO_EDITOR");
    let studio = read("RENIUM_REPRO_STUDIO");
    let prefix = std::env::var("RENIUM_REPRO_PREFIX").unwrap_or_default();
    let resolved = pairing::twins::resolve(&editor, &studio);
    let path_of = |document: &SettingsBytecode, index: usize| {
        let mut names = Vec::new();
        let mut next = Some(index);
        while let Some(current) = next {
            names.push(document.instances[current].name.clone());
            next = document.instances[current].parent_index;
        }
        names.reverse();
        names.join(".")
    };
    let mut owners: HashMap<usize, Vec<usize>> = HashMap::new();
    let mut cross_parent = 0;
    for (source, target) in resolved.partners.iter().enumerate() {
        let Some(target) = target else { continue };
        let source_path = path_of(&studio, source);
        if !source_path.starts_with(&prefix) {
            continue;
        }
        owners.entry(*target).or_default().push(source);
        let source_parent = studio.instances[source]
            .parent_index
            .map(|p| path_of(&studio, p));
        let target_parent = editor.instances[*target]
            .parent_index
            .map(|p| path_of(&editor, p));
        if source_parent != target_parent {
            cross_parent += 1;
            if cross_parent <= 5 {
                std::eprintln!(
                    "cross-parent pair: studio {source_path} -> editor {}",
                    path_of(&editor, *target)
                );
            }
        }
    }
    let duplicates = owners
        .iter()
        .filter(|(_, sources)| sources.len() > 1)
        .count();
    for (target, sources) in owners
        .iter()
        .filter(|(_, sources)| sources.len() > 1)
        .take(5)
    {
        std::eprintln!(
            "editor {} claimed by {} studio instances: {:?}",
            path_of(&editor, *target),
            sources.len(),
            sources
                .iter()
                .map(|s| path_of(&studio, *s))
                .collect::<Vec<_>>()
        );
    }
    std::eprintln!("duplicate targets={duplicates} cross_parent_pairs={cross_parent}");
    assert_eq!(duplicates, 0);
    assert_eq!(cross_parent, 0);
}

#[test]
#[ignore = "requires private RENIUM_REPRO_EDITOR and RENIUM_REPRO_STUDIO store paths"]
fn repro_first_pairing_assigns_each_editor_id_once() {
    let read = |name: &str| {
        let path =
            std::env::var_os(name).unwrap_or_else(|| panic!("set {name} to a .renium store"));
        decode_settings_bytecode(&std::fs::read(path).unwrap()).unwrap()
    };
    let mut editor = read("RENIUM_REPRO_EDITOR");
    let mut studio = read("RENIUM_REPRO_STUDIO");
    let prefix = std::env::var("RENIUM_REPRO_PREFIX").unwrap_or_default();
    let mut conflicts = Vec::new();
    align_first_pairing(
        Path::new("instances/Workspace.renium"),
        &mut editor,
        &mut studio,
        ConflictPreference::None,
        &mut conflicts,
    );
    let path_of = |document: &SettingsBytecode, index: usize| {
        let mut names = Vec::new();
        let mut next = Some(index);
        while let Some(current) = next {
            names.push(document.instances[current].name.clone());
            next = document.instances[current].parent_index;
        }
        names.reverse();
        names.join(".")
    };
    let editor_by_id = editor
        .instances
        .iter()
        .enumerate()
        .map(|(index, instance)| (instance.settings_id.clone(), index))
        .collect::<HashMap<_, _>>();
    let mut claims: HashMap<String, Vec<usize>> = HashMap::new();
    let mut cross_parent = 0;
    for (index, instance) in studio.instances.iter().enumerate() {
        let path = path_of(&studio, index);
        if !path.starts_with(&prefix) {
            continue;
        }
        claims
            .entry(instance.settings_id.clone())
            .or_default()
            .push(index);
        if let Some(&target) = editor_by_id.get(&instance.settings_id) {
            let sp = instance.parent_index.map(|p| path_of(&studio, p));
            let tp = editor.instances[target]
                .parent_index
                .map(|p| path_of(&editor, p));
            if sp != tp {
                cross_parent += 1;
                if cross_parent <= 6 {
                    std::eprintln!(
                        "cross-parent id {}: studio {path} -> editor {}",
                        instance.settings_id,
                        path_of(&editor, target)
                    );
                }
            }
        }
    }
    let duplicates = claims
        .iter()
        .filter(|(_, v)| v.len() > 1)
        .collect::<Vec<_>>();
    for (id, sources) in duplicates.iter().take(6) {
        std::eprintln!(
            "id {id} on {} studio instances: {:?}",
            sources.len(),
            sources
                .iter()
                .map(|s| path_of(&studio, *s))
                .collect::<Vec<_>>()
        );
    }
    std::eprintln!(
        "conflicts={} duplicate_ids={} cross_parent={cross_parent}",
        conflicts.len(),
        duplicates.len()
    );
    assert!(duplicates.is_empty() && cross_parent == 0);
}
