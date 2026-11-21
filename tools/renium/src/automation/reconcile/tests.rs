use super::*;

fn twin_document() -> SettingsBytecode {
    SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances: vec![SettingsBytecodeInstance::new(
            "root".into(),
            "Workspace".into(),
            "Workspace".into(),
            None,
        )],
    }
}

fn twin_add(
    document: &mut SettingsBytecode,
    parent: usize,
    name: &str,
    class: &str,
    properties: Value,
) -> usize {
    let index = document.instances.len();
    let mut instance = SettingsBytecodeInstance::new(
        format!("editor-{index}"),
        name.into(),
        class.into(),
        Some(parent),
    );
    instance.properties = properties.as_object().unwrap().clone();
    document.instances.push(instance);
    index
}

fn twin_ref(document: &SettingsBytecode, index: usize) -> Value {
    json!({"_type":"Ref", "settingsId":document.instances[index].settings_id})
}

fn twin_observation(editor: &SettingsBytecode) -> SettingsBytecode {
    let children = crate::settings::tree::settings_children_by_parent(editor);
    let mut pending = vec![0];
    let mut order = Vec::new();
    while let Some(index) = pending.pop() {
        order.push(index);
        pending.extend(&children[index]); // LIFO reverses every sibling list.
    }
    let mut positions = vec![0; order.len()];
    for (new, &old) in order.iter().enumerate() {
        positions[old] = new;
    }
    let ids = editor
        .instances
        .iter()
        .map(|instance| {
            (
                instance.settings_id.clone(),
                format!("studio-{}", instance.settings_id),
            )
        })
        .collect::<HashMap<_, _>>();
    let instances = order
        .iter()
        .map(|&index| {
            let mut instance = editor.instances[index].clone();
            instance.parent_index = instance.parent_index.map(|parent| positions[parent]);
            instance.settings_id = ids[&instance.settings_id].clone();
            remap_record_reference_ids(&mut instance.properties, &ids, None);
            remap_record_reference_ids(&mut instance.attributes, &ids, None);
            instance
        })
        .collect();
    SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances,
    }
}

fn twin_align(editor: &mut SettingsBytecode, studio: &mut SettingsBytecode) -> Vec<String> {
    let mut conflicts = ConflictLog::new(ConflictPreference::None);
    let _ = align_first_pairing(
        Path::new("instances/Workspace.renium"),
        editor,
        studio,
        &mut conflicts,
    );
    conflicts
        .open()
        .iter()
        .map(|conflict| conflict.detail.clone())
        .collect()
}

fn assert_twin_ids_unique(document: &SettingsBytecode) {
    assert_eq!(
        document.instances.len(),
        document
            .instances
            .iter()
            .map(|instance| &instance.settings_id)
            .collect::<HashSet<_>>()
            .len()
    );
}

#[test]
fn first_pairing_twins_changed_group_keeps_every_positional_slot() {
    let mut editor = twin_document();
    for value in ["A", "B", "C"] {
        twin_add(
            &mut editor,
            0,
            "NotGlass",
            "StringValue",
            json!({"Value":value}),
        );
    }
    let mut studio = twin_observation(&editor);
    studio.instances[1]
        .properties
        .insert("Value".into(), json!("edited"));
    let resolved = pairing::twins::resolve(&editor, &studio);
    assert_eq!(resolved.partners, vec![Some(0), Some(1), Some(2), Some(3)]);
    let conflicts = twin_align(&mut editor, &mut studio);
    assert!(
        conflicts
            .iter()
            .any(|conflict| conflict.contains("ambiguous duplicate")),
        "{conflicts:?}"
    );
    assert_twin_ids_unique(&studio);
}

#[test]
fn first_pairing_twins_reference_to_an_indistinguishable_attachment_is_equal() {
    for identities in [false, true] {
        let mut editor = twin_document();
        let mount = twin_add(&mut editor, 0, "Mount", "Part", json!({}));
        let first = twin_add(
            &mut editor,
            mount,
            "MotorAttachment",
            "Attachment",
            json!({"CFrame":{"_type":"CFrame","components":[0,0,0,1,0,0,0,1,0,0,0,1]}}),
        );
        let second = twin_add(
            &mut editor,
            mount,
            "MotorAttachment",
            "Attachment",
            json!({"CFrame":{"_type":"CFrame","components":[0,0,0,1,0,0,0,1,0,0,0,1]}}),
        );
        if identities {
            for index in [first, second] {
                editor.instances[index].properties.insert(
                    "UniqueId".into(),
                    json!({"_type":"UniqueId", "value":format!("{index:032x}")}),
                );
            }
        }
        let reference = twin_ref(&editor, second);
        let motor = twin_add(
            &mut editor,
            0,
            "WheelRLMotor",
            "HingeConstraint",
            json!({"Attachment0":reference}),
        );
        let mut studio = editor.clone();
        studio.instances[motor]
            .properties
            .insert("Attachment0".into(), twin_ref(&editor, first));
        let conflicts = twin_align(&mut editor, &mut studio);
        assert!(conflicts.is_empty(), "{conflicts:?}");
        assert_eq!(studio.instances.len(), 5);
        assert_twin_ids_unique(&studio);
        assert!(settings_documents_equivalent(&editor, &studio));
    }
}

#[test]
fn first_pairing_twins_tolerant_overlap_requires_a_complete_bijection() {
    for values in [[3e-7, 0.0, 1.2e-6], [0.0, 0.0, 6e-7]] {
        let mut editor = twin_document();
        for value in [0.0, 6e-7, 1.2e-6] {
            twin_add(
                &mut editor,
                0,
                "Part",
                "Part",
                json!({"Transparency":value}),
            );
        }
        let mut studio = editor.clone();
        for (instance, value) in studio.instances[1..].iter_mut().zip(values) {
            instance.settings_id = format!("studio-{}", instance.settings_id);
            instance
                .properties
                .insert("Transparency".into(), json!(value));
        }
        let resolved = pairing::twins::resolve(&editor, &studio);
        if values[0] == 0.0 {
            assert_eq!(resolved.partners, vec![Some(0), Some(1), Some(2), Some(3)]);
        } else {
            assert_eq!(resolved.partners, vec![Some(0), Some(2), Some(1), Some(3)]);
            let conflicts = twin_align(&mut editor, &mut studio);
            assert!(conflicts.is_empty(), "{conflicts:?}");
        }
        assert_eq!(resolved.partners.iter().collect::<HashSet<_>>().len(), 4);
    }
}

#[test]
fn first_pairing_twins_reference_failure_rolls_back_the_entire_subtree() {
    let mut editor = twin_document();
    let left = twin_add(&mut editor, 0, "Left", "Part", json!({}));
    let right = twin_add(&mut editor, 0, "Right", "Part", json!({}));
    for class in ["Part", "Folder"] {
        let reference = twin_ref(&editor, left);
        let model = twin_add(
            &mut editor,
            0,
            "Garage",
            "Model",
            json!({"PrimaryPart":reference}),
        );
        twin_add(&mut editor, model, "Child", class, json!({}));
    }
    let mut studio = twin_observation(&editor);
    let target_id = format!("studio-{}", editor.instances[right].settings_id);
    for instance in studio
        .instances
        .iter_mut()
        .filter(|instance| instance.name == "Garage")
    {
        instance.properties.insert(
            "PrimaryPart".into(),
            json!({"_type":"Ref","settingsId":target_id}),
        );
    }
    let resolved = pairing::twins::resolve(&editor, &studio);
    let matched = resolved.partners.iter().flatten().collect::<Vec<_>>();
    assert_eq!(matched.len(), matched.iter().collect::<HashSet<_>>().len());
    assert!(
        resolved.partners.iter().any(Option::is_none),
        "different-class positional children stay unmatched"
    );
    let conflicts = twin_align(&mut editor, &mut studio);
    assert!(
        conflicts
            .iter()
            .any(|conflict| conflict.contains("different classes")),
        "{conflicts:?}"
    );
    assert_twin_ids_unique(&studio);
}

#[test]
fn first_pairing_twins_at_512_borrow_large_property_maps() {
    let mut editor = twin_document();
    for index in 0..512 {
        let twin = twin_add(
            &mut editor,
            0,
            "Twin",
            "StringValue",
            json!({"Value":index.to_string()}),
        );
        editor.instances[twin]
            .attributes
            .insert("Payload".into(), json!("x".repeat(64 * 1024)));
    }
    let studio = twin_observation(&editor);
    let started = Instant::now();
    let resolved = pairing::twins::resolve(&editor, &studio);
    eprintln!(
        "512-member group with 64 KiB attributes: {:.3} ms",
        started.elapsed().as_secs_f64() * 1000.0
    );
    assert_eq!(resolved.partners[0], Some(0));
    assert_eq!(
        resolved.partners[1..],
        (1..=512).rev().map(Some).collect::<Vec<_>>()
    );
}

#[test]
fn first_pairing_twins_use_first_comparison_noise_rules() {
    let mut editor = twin_document();
    for x in [1.0, 3.0] {
        twin_add(
            &mut editor,
            0,
            "NotGlass",
            "MeshPart",
            json!({
                "CFrame":{"_type":"CFrame", "components":[x,0,0,1,0,0,0,1,0,0,0,1]},
                "Tags":["one", "two"], "MeshId":"rbxassetid://42",
                "PhysicalConfigData":{"_type":"SharedString", "base64":base64::encode(b"CSGPHS\x08\0\0\0\x01\0first cook")}
            }),
        );
    }
    let mut studio = twin_observation(&editor);
    for instance in &mut studio.instances[1..] {
        let x = instance.properties["CFrame"]["components"][0]
            .as_f64()
            .unwrap();
        instance.properties["CFrame"]["components"][0] = json!(x + 0.0001);
        instance
            .properties
            .insert("Tags".into(), json!(["two", "one"]));
        instance.properties.insert(
            "MeshId".into(),
            json!("https://www.roblox.com/asset/?id=42"),
        );
        instance.properties.insert("PhysicalConfigData".into(), json!({"_type":"SharedString", "base64":base64::encode(b"CSGPHS\x08\0\0\0\x01\0second cook")}));
    }
    let conflicts = twin_align(&mut editor, &mut studio);
    assert!(conflicts.is_empty(), "{conflicts:?}");
    assert_twin_ids_unique(&studio);
    for source in 1..studio.instances.len() {
        let target = editor
            .instances
            .iter()
            .position(|instance| instance.settings_id == studio.instances[source].settings_id)
            .unwrap();
        assert!(settings_instances_equal(&editor, target, &studio, source));
    }
}

fn lighting_settings_entry(clock_time: f64, visible: bool) -> SnapshotEntry {
    let mut document = SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances: vec![SettingsBytecodeInstance::new(
            "lighting".into(),
            "Lighting".into(),
            "Lighting".into(),
            None,
        )],
    };
    document.instances[0].properties = json!({ "ClockTime": clock_time })
        .as_object()
        .unwrap()
        .clone();
    let mut sky = SettingsBytecodeInstance::new("sky".into(), "Sky".into(), "Sky".into(), Some(0));
    sky.properties = json!({ "CelestialBodiesShown": visible })
        .as_object()
        .unwrap()
        .clone();
    document.instances.push(sky);
    SnapshotEntry::File(encode_settings_bytecode(&document).unwrap())
}

#[test]
fn first_pairing_prefer_studio_keeps_studio_settings_values() {
    let path = PathBuf::from("instances/Lighting.renium");
    let editor = ProjectSnapshot {
        entries: [(path.clone(), lighting_settings_entry(17.7, false))]
            .into_iter()
            .collect(),
    };
    let studio = ProjectSnapshot {
        entries: [(path.clone(), lighting_settings_entry(10.3, true))]
            .into_iter()
            .collect(),
    };
    let (_, conflicts, _) =
        merge_snapshots_with_changes(None, &editor, &studio, ConflictPreference::None, None)
            .unwrap();
    assert_eq!(conflicts.len(), 2, "{conflicts:?}");
    for preference in [ConflictPreference::Studio, ConflictPreference::Editor] {
        let (merged, conflicts, changes) =
            merge_snapshots_with_changes(None, &editor, &studio, preference, None).unwrap();
        assert!(conflicts.is_empty(), "{preference:?}: {conflicts:?}");
        let winner = if preference == ConflictPreference::Studio {
            &studio
        } else {
            &editor
        };
        let merged_doc = settings_document(merged.entries.get(&path)).unwrap();
        let winner_doc = settings_document(winner.entries.get(&path)).unwrap();
        assert!(
            settings_documents_equivalent(&merged_doc, &winner_doc),
            "{preference:?} merged {merged_doc:?}"
        );
        if preference == ConflictPreference::Studio {
            assert!(
                !changes.studio.contains(&path),
                "{preference:?} pushes into Studio"
            );
            assert!(
                changes.editor.contains(&path),
                "{preference:?} leaves the file stale"
            );
            assert!(!changes.aligned_studio.contains_key(&path));
        } else {
            assert!(
                changes.studio.contains(&path),
                "{preference:?} leaves Studio stale"
            );
            assert!(!changes.editor.contains(&path));
            let previous = &changes.aligned_studio[&path];
            let studio_doc = settings_document(studio.entries.get(&path)).unwrap();
            assert!(
                settings_documents_equivalent(previous, &studio_doc),
                "the push must start from what Studio holds"
            );
        }
    }
}

#[test]
fn load_record_drops_a_baseline_whose_packs_are_missing() -> Result<()> {
    let root = crate::tests::support::temp_dir("missing-packs");
    fs::create_dir_all(root.join("src"))?;
    fs::write(root.join("src/A.luau"), b"return 1")?;
    let context = BoundContext {
        id: 1,
        initialized: true,
        project: root
            .join("renium.project.jsonc")
            .to_string_lossy()
            .into_owned(),
        root: root.to_string_lossy().into_owned(),
        experience: String::new(),
        source: root.join("src").to_string_lossy().into_owned(),
        resource_lease: None,
        place_id: None,
        game_id: None,
        selector: String::new(),
        runtime_id: None,
        plugin_build: None,
        fingerprint: String::new(),
    };
    let snapshot = capture_snapshot(&root, &[PathBuf::from("src")])?;
    let record = PairRecord {
        version: RECORD_VERSION,
        identity: PairIdentity {
            experience: root.to_string_lossy().into_owned(),
            project: root.to_string_lossy().into_owned(),
            fingerprint: String::new(),
            game_id: None,
            place_id: None,
            local_file: None,
        },
        mode: PairMode::Reconcile,
        conflict_preference: ConflictPreference::None,
        runtime_settings: Map::new(),
        baseline: Some(store::StoredSnapshot::write(&root, "pair", &snapshot)?),
        head: None,
        conflicts: Vec::new(),
        resolution_required: false,
        last_runtime_id: None,
        local_file_stamp: None,
        local_file_digest: None,
        studio_checkpoint: Some(StudioCheckpoint {
            runtime_id: "runtime".into(),
            change_tracker_version: 4,
            seq: 7,
            service_generations: BTreeMap::new(),
        }),
        differences: Vec::new(),
    };
    write_record(&context, "pair", &record)?;
    let intact = load_record(&context, "pair")?.unwrap();
    assert!(intact.baseline.is_some());
    assert!(intact.studio_checkpoint.is_some());
    let packs = root.join(".renium/reconcile/pair/packs");
    for entry in fs::read_dir(&packs)? {
        fs::remove_file(entry?.path())?;
    }
    let healed = load_record(&context, "pair")?.unwrap();
    assert!(healed.baseline.is_none());
    assert!(healed.studio_checkpoint.is_none());
    assert!(!root.join(".renium/reconcile/pair").exists());
    let bytes = fs::read(record_path(&context, "pair"))?;
    let persisted: PairRecord = rmp_serde::from_slice(&bytes)?;
    assert!(persisted.baseline.is_none());
    let _ = fs::remove_dir_all(root);
    Ok(())
}

#[test]
fn pulled_baseline_records_studio_bytes_even_when_restoration_publishes_no_file_change()
-> Result<()> {
    let root = crate::tests::support::temp_dir("restored-pull-baseline");
    let project = root.join("renium.project.jsonc");
    fs::write(&project, br#"{"schemaVersion":1,"sourceRoot":"src"}"#)?;
    fs::create_dir_all(root.join("src/ReplicatedStorage/Pkg"))?;
    let source = PathBuf::from("src/ReplicatedStorage/Pkg/Config.luau");
    let added = PathBuf::from("src/ReplicatedStorage/Pkg/Added.luau");
    fs::write(root.join(&source), b"return 'package'")?;
    fs::write(root.join(&added), b"return 'added by restore'")?;
    let context = BoundContext {
        id: 1,
        initialized: true,
        project: project.to_string_lossy().into_owned(),
        root: root.to_string_lossy().into_owned(),
        experience: String::new(),
        source: root.join("src").to_string_lossy().into_owned(),
        resource_lease: None,
        place_id: None,
        game_id: None,
        selector: String::new(),
        runtime_id: None,
        plugin_build: None,
        fingerprint: String::new(),
    };
    let initial = capture_snapshot(&root, &[PathBuf::from("src")])?;
    let record = PairRecord {
        version: RECORD_VERSION,
        identity: PairIdentity {
            experience: root.to_string_lossy().into_owned(),
            project: project.to_string_lossy().into_owned(),
            fingerprint: String::new(),
            game_id: None,
            place_id: None,
            local_file: None,
        },
        mode: PairMode::Reconcile,
        conflict_preference: ConflictPreference::None,
        runtime_settings: Map::new(),
        baseline: Some(store::StoredSnapshot::write(&root, "pull-test", &initial)?),
        head: None,
        conflicts: Vec::new(),
        resolution_required: false,
        last_runtime_id: None,
        local_file_stamp: None,
        local_file_digest: None,
        studio_checkpoint: None,
        differences: Vec::new(),
    };
    write_record(&context, "pull-test", &record)?;
    let studio = SnapshotEntry::File(b"return 'studio'".to_vec());
    let overrides = BTreeMap::from([
        (source.clone(), Some(studio.clone())),
        (added.clone(), None),
    ]);
    // A no-op publication, or a user save before resume, does not make Studio
    // have the restored package's bytes.
    fs::write(root.join(&source), b"return 'later user edit'")?;
    Coordinator::default().advance_pulled_baseline(&context, "pull-test", &[], &overrides)?;
    let recorded = load_record(&context, "pull-test")?
        .unwrap()
        .baseline
        .unwrap()
        .load(&root, "pull-test")?;
    assert!(recorded.entries[&source] == studio);
    assert!(!recorded.entries.contains_key(&added));
    assert_eq!(fs::read(root.join(source))?, b"return 'later user edit'");
    let _ = fs::remove_dir_all(root);
    Ok(())
}

#[test]
fn relocated_supporting_stores_use_relative_snapshot_keys() -> Result<()> {
    let root = crate::tests::support::temp_dir("supporting-store-scope");
    let _cleanup = crate::system::files::OnDrop::new(|| {
        crate::project::storage::forget(&root);
        let _ = fs::remove_dir_all(&root);
    });
    let project = root.join("renium.project.jsonc");
    fs::write(
        &project,
        br#"{"schemaVersion":1,"sourceRoot":"code/scripts"}"#,
    )?;
    config::load_project(Some(&project), None)?;
    let context = BoundContext {
        id: 1,
        initialized: true,
        project: project.to_string_lossy().into_owned(),
        root: root.to_string_lossy().into_owned(),
        experience: String::new(),
        source: root.join("code/scripts").to_string_lossy().into_owned(),
        resource_lease: None,
        place_id: None,
        game_id: None,
        selector: String::new(),
        runtime_id: None,
        plugin_build: None,
        fingerprint: String::new(),
    };
    let store = PathBuf::from("instances/ServerStorage.renium");
    fs::create_dir_all(root.join("instances"))?;
    fs::write(root.join(&store), b"unchanged store")?;
    for path in [
        PathBuf::from("code/scripts/ServerStorage/Module.luau"),
        store.clone(),
    ] {
        let changed = HashSet::from([path]);
        assert_eq!(
            services_for_snapshot_paths(&context, &changed),
            ["ServerStorage"]
        );
        let scopes = supporting_settings_scopes(&context, &changed)?;
        assert_eq!(scopes, std::slice::from_ref(&store));
        let expected = capture_snapshot(&root, std::slice::from_ref(&store))?;
        let current = capture_snapshot(&root, &scopes)?;
        assert_eq!(
            expected.entries.keys().collect::<Vec<_>>(),
            current.entries.keys().collect::<Vec<_>>()
        );
        assert!(
            snapshot_path_differences(&expected, &current, &scopes.into_iter().collect())?
                .is_empty()
        );
    }
    fs::write(
        &project,
        br#"{"schemaVersion":1,"sourceRoot":"code/scripts","tree":{"StarterPlayer":{"StarterPlayerScripts":{"$className":"StarterPlayerScripts","$path":"code/scripts/client"}}}}"#,
    )?;
    fs::create_dir_all(root.join("code/scripts/client"))?;
    fs::write(
        root.join("code/scripts/client/Main.client.luau"),
        b"-- client\n",
    )?;
    let changed = HashSet::from([PathBuf::from("code/scripts/client/Main.client.luau")]);
    let services = services_for_snapshot_paths(&context, &changed);
    assert_eq!(services, ["StarterPlayer"]);
    let stage = ExportProjectStage::create(&root, Path::new("code/scripts"), &services)?;
    assert!(
        stage
            .loaded
            .as_ref()
            .unwrap()
            .project
            .tree
            .contains_key("StarterPlayer")
    );
    assert!(
        stage
            .import_project_root
            .join("StarterPlayer/StarterPlayerScripts/Main.client.luau")
            .is_file()
    );
    Ok(())
}

#[test]
fn native_bootstrap_does_not_replace_nonempty_service_deltas() {
    let document = Arc::new(SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances: vec![
            SettingsBytecodeInstance::new(
                "root".into(),
                "Workspace".into(),
                "Workspace".into(),
                None,
            ),
            SettingsBytecodeInstance::new("mesh".into(), "Mesh".into(), "MeshPart".into(), Some(0)),
        ],
    });
    let mut changes = EditorChangeSet::default();
    for (service, mode) in [
        ("TestService", "deleteInstances"),
        ("Workspace", "upsertInstances"),
        ("TestService", "upsertInstances"),
        ("Workspace", "deleteInstances"),
    ] {
        changes.instance_changes.push(EditorInstanceChange {
            service: service.into(),
            mode: mode.into(),
            allow_deletes: false,
            instances: Vec::new(),
            preserve_instances: Vec::new(),
        });
    }
    let empty = HashMap::new();
    let before = serde_json::to_value(&changes.instance_changes).unwrap();
    stage_native_bootstrap_services(&mut changes, &empty).unwrap();
    assert_eq!(
        serde_json::to_value(&changes.instance_changes).unwrap(),
        before
    );
    stage_native_bootstrap_services(
        &mut changes,
        &HashMap::from([("Workspace".into(), Arc::clone(&document))]),
    )
    .unwrap();
    assert_eq!(changes.instance_changes.len(), 3);
    assert_eq!(changes.instance_changes[0].service, "TestService");
    assert_eq!(changes.instance_changes[0].mode, "deleteInstances");
    assert_eq!(changes.instance_changes[1].service, "TestService");
    assert_eq!(changes.instance_changes[1].mode, "upsertInstances");
    let native = &changes.instance_changes[2];
    assert_eq!(native.service, "Workspace");
    assert_eq!(native.mode, "reconcileService");
    assert!(native.allow_deletes);
    assert_eq!(native.instances.len(), 1);
    assert_eq!(native.instances[0].settings_id, "mesh");
}

#[test]
fn native_bootstrap_preserves_retained_root_edits_and_source_paths() {
    let document = Arc::new(SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances: vec![
            SettingsBytecodeInstance::new(
                "root".into(),
                "Workspace".into(),
                "Workspace".into(),
                None,
            ),
            SettingsBytecodeInstance::new("mesh".into(), "Mesh".into(), "MeshPart".into(), Some(0)),
        ],
    });
    let mut desired = document.as_ref().clone();
    desired.instances[0]
        .properties
        .insert("Gravity".into(), json!(150));
    for index in 0..4096 {
        desired.instances.push(SettingsBytecodeInstance::new(
            format!("new-{index}"),
            format!("Part{index}"),
            "Part".into(),
            Some(0),
        ));
    }
    let mut previous = SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances: vec![document.instances[0].clone()],
    };
    previous.instances[0]
        .attributes
        .insert("Removed".into(), json!(true));
    let path = PathBuf::from("src/Workspace/__roblox_sync_settings.renium");
    let prepared = HashMap::from([(
        path.clone(),
        PreparedEditorSettingsChange {
            previous,
            current: desired,
        },
    )]);
    let paths = HashSet::from([path.clone()]);
    let snapshot = ProjectSnapshot::default();
    let full = reconciliation_push_plan_for_paths_with_prepared_settings(
        &snapshot, &snapshot, &paths, &prepared, true, false,
    )
    .unwrap();
    assert_eq!(full.changed_paths, vec![path.clone()]);
    assert_eq!(full.target_settings_ids, vec!["root"]);
    assert!(full.recreated_settings_ids.is_empty());
    assert_eq!(
        full.property_removals[0].deleted_attributes,
        vec!["Removed"]
    );
    assert!(full.initial_native_services.contains("Workspace"));
    let incremental = reconciliation_push_plan_for_paths_with_prepared_settings(
        &snapshot, &snapshot, &paths, &prepared, false, false,
    )
    .unwrap();
    assert_eq!(incremental.recreated_settings_ids.len(), 4097);
    assert_eq!(incremental.target_settings_ids.len(), 4098);
    assert_eq!(
        incremental.property_removals[0].deleted_attributes,
        vec!["Removed"]
    );

    let mut outgoing = prepared[&path].previous.clone();
    outgoing.instances.push(SettingsBytecodeInstance::new(
        "outgoing".into(),
        "OnlyInStudio".into(),
        "Folder".into(),
        Some(0),
    ));
    let mut before = ProjectSnapshot {
        entries: BTreeMap::from([(
            path.clone(),
            SnapshotEntry::File(encode_settings_bytecode(&outgoing).unwrap()),
        )]),
    };
    let mut requested = ProjectSnapshot {
        entries: BTreeMap::from([(
            path.clone(),
            SnapshotEntry::File(encode_settings_bytecode(&prepared[&path].current).unwrap()),
        )]),
    };
    let source_path = PathBuf::from("src/Workspace/Unchanged.server.luau");
    let source = SnapshotEntry::File(b"return 1".to_vec());
    before.entries.insert(source_path.clone(), source.clone());
    requested.entries.insert(source_path.clone(), source);
    let mut replacement = HashMap::new();
    let paths = project_replacement_paths(&requested, &before);
    prepare_project_replacement(&requested, &before, &paths, &mut replacement).unwrap();
    assert_eq!(replacement[&path].previous.instances.len(), 1);
    assert_eq!(replacement[&path].current.instances.len(), 4098);
    let replaced = reconciliation_push_plan_for_paths_with_prepared_settings(
        &before,
        &requested,
        &paths,
        &replacement,
        true,
        true,
    )
    .unwrap();
    assert_eq!(replaced.target_settings_ids, vec!["root"]);
    assert!(replaced.initial_native_services.contains("Workspace"));
    assert!(replaced.recreated_settings_ids.is_empty());
    assert!(replaced.instance_deletes.is_empty());
    assert!(replaced.changed_paths.contains(&source_path));
    assert_eq!(
        replaced.property_removals[0].deleted_attributes,
        vec!["Removed"]
    );
}

#[test]
fn full_push_clears_authored_contents_when_only_engine_containers_remain() {
    for (service, container) in [
        ("ReplicatedStorage", None),
        ("StarterPlayer", Some("StarterPlayerScripts")),
    ] {
        let mut current = SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: vec![SettingsBytecodeInstance::new(
                "root".into(),
                service.into(),
                service.into(),
                None,
            )],
        };
        if let Some(container) = container {
            current.instances.push(SettingsBytecodeInstance::new(
                "container".into(),
                container.into(),
                container.into(),
                Some(0),
            ));
        }
        let mut previous = current.clone();
        previous.instances.push(SettingsBytecodeInstance::new(
            "removed".into(),
            "OnlyInStudio".into(),
            "ModuleScript".into(),
            Some(current.instances.len() - 1),
        ));
        let path = PathBuf::from(format!("instances/{service}.renium"));
        let snapshot = |document: &SettingsBytecode| ProjectSnapshot {
            entries: BTreeMap::from([(
                path.clone(),
                SnapshotEntry::File(encode_settings_bytecode(document).unwrap()),
            )]),
        };
        let desired = snapshot(&current);
        let observed = snapshot(&previous);
        let paths = project_replacement_paths(&desired, &observed);
        let mut prepared = HashMap::new();
        let unchanged =
            prepare_project_replacement(&desired, &observed, &paths, &mut prepared).unwrap();
        assert!(!unchanged.contains(&path));
        assert!(!settings_documents_equivalent(
            &prepared[&path].current,
            &prepared[&path].previous,
        ));
        let plan = reconciliation_push_plan_for_paths_with_prepared_settings(
            &observed, &desired, &paths, &prepared, true, false,
        )
        .unwrap();
        assert!(plan.initial_native_services.is_empty());
        assert_eq!(plan.instance_deletes.len(), 1);
        assert_eq!(plan.instance_deletes[0].instances.len(), 1);
        assert_eq!(plan.instance_deletes[0].instances[0].settings_id, "removed");
    }
}

#[test]
fn default_collision_fidelity_and_unchanged_native_values_are_not_pushed() {
    let root = SettingsBytecodeInstance::new(
        "root".into(),
        "ServerStorage".into(),
        "ServerStorage".into(),
        None,
    );
    let mut current = SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances: vec![root],
    };
    for index in 0..3 {
        let mut part = SettingsBytecodeInstance::new(
            format!("mesh-{index}"),
            format!("Mesh{index}"),
            "MeshPart".into(),
            Some(0),
        );
        part.properties
            .insert("MeshContent".into(), json!("rbxassetid://123"));
        part.properties.insert("SourceAssetId".into(), json!(456));
        part.properties.insert("Transparency".into(), json!(0.25));
        current.instances.push(part);
    }
    let mut previous = current.clone();
    for (index, instance) in previous.instances.iter_mut().enumerate().skip(1) {
        let fidelity = if index == 1 { "Hull" } else { "Default" };
        instance.properties.insert(
            "CollisionFidelity".into(),
            json!({"_type": "EnumItem", "enumType": "CollisionFidelity", "name": fidelity}),
        );
    }
    current.instances[2]
        .properties
        .insert("Transparency".into(), json!(0.5));
    let path = PathBuf::from("src/ServerStorage/__roblox_sync_settings.renium");
    let snapshot = |document: &SettingsBytecode| ProjectSnapshot {
        entries: BTreeMap::from([(
            path.clone(),
            SnapshotEntry::File(encode_settings_bytecode(document).unwrap()),
        )]),
    };
    let desired = snapshot(&current);
    let observed = snapshot(&previous);
    let paths = project_replacement_paths(&desired, &observed);
    let mut prepared = HashMap::new();
    prepare_project_replacement(&desired, &observed, &paths, &mut prepared).unwrap();
    let plan = reconciliation_push_plan_for_paths_with_prepared_settings(
        &observed, &desired, &paths, &prepared, true, false,
    )
    .unwrap();
    assert_eq!(plan.target_settings_ids, vec!["mesh-1"]);
    let mut unchanged = plan
        .unchanged_native_root_properties
        .get(&("ServerStorage".into(), "mesh-1".into()))
        .cloned()
        .unwrap();
    unchanged.sort();
    assert_eq!(unchanged, vec!["MeshContent", "SourceAssetId"]);
}

#[test]
fn a_mesh_part_pushed_in_place_sends_no_engine_written_value_studio_holds() {
    // Hulls of version 8 cooked as Default and as Hull.
    let (as_default, as_hull) = ("Q1NHUEhTCAAAAAIAaHVsbA==", "Q1NHUEhTCAAAAAEAaHVsbA==");
    let hull = |cooked: &str| json!({"_type": "SharedString", "base64": cooked});
    let fidelity = |name: &str| json!({"_type": "EnumItem", "name": name});
    let mut files = SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances: vec![SettingsBytecodeInstance::new(
            "root".into(),
            "Workspace".into(),
            "Workspace".into(),
            None,
        )],
    };
    for name in ["plain", "spelled", "cooked"] {
        let mut part =
            SettingsBytecodeInstance::new(name.into(), name.into(), "MeshPart".into(), Some(0));
        for (property, value) in [
            ("MeshId", json!("rbxassetid://123")),
            ("PhysicalConfigData", hull(as_default)),
            ("UnscaledVolume", json!(7.0)),
            ("Transparency", json!(0.0)),
        ] {
            part.properties.insert(property.into(), value);
        }
        files.instances.push(part);
    }
    // A push that leaves an instance as it is edits the others in place.
    files.instances.push(SettingsBytecodeInstance::new(
        "kept".into(),
        "kept".into(),
        "Folder".into(),
        Some(0),
    ));
    let mut studio = files.clone();
    // Studio holds the last part cooked as a hull.
    for (property, value) in [
        ("CollisionFidelity", fidelity("Hull")),
        ("PhysicalConfigData", hull(as_hull)),
    ] {
        studio.instances[3]
            .properties
            .insert(property.into(), value);
    }
    // An older store spells out the fidelity a capture leaves out today.
    files.instances[2]
        .properties
        .insert("CollisionFidelity".into(), fidelity("Default"));
    for part in &mut files.instances[1..4] {
        part.properties.insert("Transparency".into(), json!(0.5));
    }
    let path = PathBuf::from("src/Workspace/__roblox_sync_settings.renium");
    let snapshot = |document: &SettingsBytecode| ProjectSnapshot {
        entries: BTreeMap::from([(
            path.clone(),
            SnapshotEntry::File(encode_settings_bytecode(document).unwrap()),
        )]),
    };
    let (desired, observed) = (snapshot(&files), snapshot(&studio));
    let project_root = crate::tests::support::temp_dir("mesh-part-in-place");
    std::fs::create_dir_all(project_root.join(&path).parent().unwrap()).unwrap();
    if let Some(SnapshotEntry::File(bytes)) = desired.entries.get(&path) {
        std::fs::write(project_root.join(&path), bytes).unwrap();
    }
    let paths = project_replacement_paths(&desired, &observed);
    let mut prepared = HashMap::new();
    prepare_project_replacement(&desired, &observed, &paths, &mut prepared).unwrap();
    let plan = reconciliation_push_plan_for_paths_with_prepared_settings(
        &observed, &desired, &paths, &prepared, false, false,
    )
    .unwrap();
    let mut args = crate::cli::PushEditorChangesArgs::new(
        crate::cli::ProjectSourceArgs {
            project_root,
            src_root: PathBuf::from("src"),
        },
        crate::cli::BridgeConnectionArgs::local(0.1),
    );
    args.changed_paths.clone_from(&plan.changed_paths);
    args.target_settings_ids
        .clone_from(&plan.target_settings_ids);
    args.exact_target_ids = true;
    args.targets_extend_paths = true;
    let documents = prepared
        .remove(&path)
        .map(|change| ("Workspace".to_string(), Arc::new(change.current)))
        .into_iter()
        .collect();
    let mut changes =
        crate::editor::sync::collect_prepared_editor_changes(&args, documents, HashSet::new())
            .unwrap();
    amend_reconciled_changes(&mut changes, plan).unwrap();
    let sent = |id: &str| {
        let change = changes
            .property_changes
            .iter()
            .find(|change| change.settings_id.as_deref() == Some(id))
            .unwrap();
        let mut names = change.properties.keys().cloned().collect::<Vec<_>>();
        names.sort();
        (names, change.properties.get("CollisionFidelity").cloned())
    };
    assert_eq!(sent("plain"), (vec!["Transparency".to_string()], None));
    assert_eq!(sent("spelled"), (vec!["Transparency".to_string()], None));
    // A fidelity that differs cooks the part, and the saved mass follows it.
    let (names, written) = sent("cooked");
    assert_eq!(
        names,
        ["CollisionFidelity", "Transparency", "UnscaledVolume"]
    );
    assert_eq!(written.unwrap()["name"], "Default");
}

#[test]
fn full_push_reuses_populated_instances_and_plans_only_the_delta() {
    let root = SettingsBytecodeInstance::new(
        "root".into(),
        "ServerStorage".into(),
        "ServerStorage".into(),
        None,
    );
    let mut current = SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances: vec![root],
    };
    for index in 0..4096 {
        let mut part = SettingsBytecodeInstance::new(
            format!("part-{index}"),
            format!("Part{index}"),
            "Part".into(),
            Some(0),
        );
        part.attributes.insert("Keep".into(), json!(true));
        current.instances.push(part);
    }
    let mut previous = current.clone();
    current.instances[43]
        .attributes
        .insert("Keep".into(), json!(false));
    previous.instances.push(SettingsBytecodeInstance::new(
        "outgoing".into(),
        "OnlyInStudio".into(),
        "Folder".into(),
        Some(0),
    ));
    let path = PathBuf::from("src/ServerStorage/__roblox_sync_settings.renium");
    let snapshot = |document: &SettingsBytecode| ProjectSnapshot {
        entries: BTreeMap::from([(
            path.clone(),
            SnapshotEntry::File(encode_settings_bytecode(document).unwrap()),
        )]),
    };
    let desired = snapshot(&current);
    let observed = snapshot(&previous);
    let paths = project_replacement_paths(&desired, &observed);
    let mut prepared = HashMap::new();
    prepare_project_replacement(&desired, &observed, &paths, &mut prepared).unwrap();
    assert_eq!(prepared[&path].previous.instances.len(), 4098);
    let plan = reconciliation_push_plan_for_paths_with_prepared_settings(
        &observed, &desired, &paths, &prepared, true, false,
    )
    .unwrap();
    assert!(plan.initial_native_services.is_empty());
    assert!(plan.recreated_settings_ids.is_empty());
    assert_eq!(plan.target_settings_ids, vec!["part-42"]);
    assert!(
        plan.attribute_only_instances
            .contains(&("ServerStorage".into(), "part-42".into()))
    );
    assert_eq!(plan.instance_deletes.len(), 1);
    assert_eq!(plan.instance_deletes[0].instances.len(), 1);
    assert_eq!(
        plan.instance_deletes[0].instances[0]
            .path_segments
            .last()
            .unwrap(),
        "OnlyInStudio"
    );
    let mut changes = EditorChangeSet::default();
    changes.instance_changes.push(EditorInstanceChange {
        mode: "upsertInstances".into(),
        service: "ServerStorage".into(),
        allow_deletes: false,
        instances: vec![crate::editor::types::EditorInstanceDescriptor {
            settings_id: "part-42".into(),
            class_name: "Part".into(),
            ..Default::default()
        }],
        preserve_instances: Vec::new(),
    });
    changes.property_changes.push(EditorPropertyChange {
        service: "ServerStorage".into(),
        settings_id: Some("part-42".into()),
        class_name: "Part".into(),
        path_segments: Vec::new(),
        path_ordinals: Vec::new(),
        properties: Map::from_iter([("Anchored".into(), json!(true))]),
        reset_properties: Vec::new(),
        attributes: Map::from_iter([("Keep".into(), json!(false))]),
        deleted_attributes: vec!["Removed".into()],
        attributes_complete: false,
    });
    amend_reconciled_changes(&mut changes, plan).unwrap();
    assert_eq!(changes.instance_changes[0].mode, "deleteInstances");
    assert!(!changes.instance_changes[0].instances[0].anchor_only);
    assert!(changes.instance_changes[1].instances[0].anchor_only);
    assert!(changes.property_changes[0].properties.is_empty());
    assert_eq!(changes.property_changes[0].attributes["Keep"], false);
    assert_eq!(changes.property_changes[0].deleted_attributes, ["Removed"]);
}

#[test]
fn in_place_edits_preserve_observed_paths_through_reordered_duplicate_ancestors() {
    let node = |id: &str, name: &str, class: &str, parent| {
        SettingsBytecodeInstance::new(id.into(), name.into(), class.into(), parent)
    };
    let mut desired = SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances: vec![
            node("root", "Workspace", "Workspace", None),
            node("a", "Lampost", "Model", Some(0)),
            node("b", "Lampost", "Model", Some(0)),
            node("light-a", "Light", "Model", Some(1)),
            node("light-b", "Light", "Model", Some(2)),
            node("we", "we", "Part", Some(3)),
        ],
    };
    let mut observed = desired.clone();
    observed.instances.swap(1, 2);
    observed.instances[3].parent_index = Some(2);
    observed.instances[4].parent_index = Some(1);
    desired.instances[5]
        .attributes
        .insert("Changed".into(), json!(true));
    let mut plan = ReconcilePushPlan::default();
    append_aligned_settings_push_plan(
        Path::new("instances/Workspace.renium"),
        &desired,
        &observed,
        &mut plan,
    )
    .unwrap();
    assert_eq!(plan.target_settings_ids, ["we"]);
    let key = |id: &str| ("Workspace".to_string(), id.to_string());
    assert!(plan.in_place_instances.contains(&key("we")));
    assert_eq!(plan.previous_paths[&key("we")].path_ordinals, [1, 2, 1, 1]);
    assert_eq!(plan.previous_paths[&key("a")].path_ordinals, [1, 2]);
    assert_eq!(
        plan.previous_paths[&key("light-a")].path_ordinals,
        [1, 2, 1]
    );
    assert!(
        !plan.previous_paths.contains_key(&key("light-b")),
        "Do not traverse unrelated subtrees for a small edit"
    );
}

#[test]
fn part_colors_that_share_a_byte_are_equal() {
    let files = json!({"_type": "Color3", "r": 0.2, "g": 1.0, "b": 0.3});
    let studio = json!({"_type": "Color3", "r": 0.2, "g": 1.0, "b": 0.30196078431372547});
    let other = json!({"_type": "Color3", "r": 0.2, "g": 1.0, "b": 0.31});
    assert!(reconciliation_property_values_equal(
        "Part",
        "Color",
        Some(&files),
        Some(&studio)
    ));
    assert!(!reconciliation_property_values_equal(
        "Part",
        "Color",
        Some(&files),
        Some(&other)
    ));
    assert!(
        !reconciliation_property_values_equal("Folder", "IconTint", Some(&files), Some(&studio)),
        "a float color keeps its exact value"
    );
}

#[test]
fn initial_native_import_requires_no_ordinary_instances() {
    let mut root =
        SettingsBytecodeInstance::new("root".into(), "Workspace".into(), "Workspace".into(), None);
    root.properties.insert(
        "CurrentCamera".into(),
        json!({"_type":"Ref", "settingsId":"viewport"}),
    );
    let mut document = SettingsBytecode {
        version: crate::settings::bytecode::SETTINGS_BINARY_VERSION,
        instances: vec![
            root,
            SettingsBytecodeInstance::new(
                "viewport".into(),
                "Renamed".into(),
                "Camera".into(),
                Some(0),
            ),
            SettingsBytecodeInstance::new(
                "terrain".into(),
                "Terrain".into(),
                "Terrain".into(),
                Some(0),
            ),
        ],
    };
    assert!(service_has_only_native_containers(&document));
    for (class, parent) in [
        ("Camera", 0),
        ("Part", 0),
        ("Folder", 2),
        ("StringValue", 1),
    ] {
        document.instances.push(SettingsBytecodeInstance::new(
            "ordinary".into(),
            "Camera".into(),
            class.into(),
            Some(parent),
        ));
        assert!(!service_has_only_native_containers(&document));
        document.instances.pop();
    }
    document.instances[0].properties.clear();
    assert!(!service_has_only_native_containers(&document));

    let mut chat = SettingsBytecode {
        version: document.version,
        instances: vec![SettingsBytecodeInstance::new(
            "chat".into(),
            "TextChatService".into(),
            "TextChatService".into(),
            None,
        )],
    };
    for class in [
        "ChatWindowConfiguration",
        "ChatInputBarConfiguration",
        "BubbleChatConfiguration",
        "ChannelTabsConfiguration",
    ] {
        chat.instances.push(SettingsBytecodeInstance::new(
            class.into(),
            class.into(),
            class.into(),
            Some(0),
        ));
    }
    assert!(service_has_only_native_containers(&chat));
    chat.instances.push(SettingsBytecodeInstance::new(
        "content".into(),
        "Content".into(),
        "Folder".into(),
        Some(3),
    ));
    assert!(
        !service_has_only_native_containers(&chat),
        "User contents must not qualify as an empty destination"
    );
}
use crate::editor::types::EditorSettingsWrite;
use crate::settings::bytecode::SettingsBytecodeInstance;

#[test]
fn staged_settings_redirect_uses_original_project_not_merged_stage() {
    let root = crate::tests::support::temp_dir("staged-settings-hash");
    let stage = root.join("stage");
    let relative = PathBuf::from("src/ServerScriptService/__roblox_sync_settings.renium");
    let destination = root.join(&relative);
    let staged_path = stage.join(&relative);
    fs::create_dir_all(destination.parent().unwrap()).unwrap();
    fs::create_dir_all(staged_path.parent().unwrap()).unwrap();
    let document = |id: &str| SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances: vec![SettingsBytecodeInstance {
            settings_id: id.into(),
            name: "ServerScriptService".into(),
            class_name: "ServerScriptService".into(),
            parent_index: None,
            properties: Map::new(),
            attributes: Map::new(),
        }],
    };
    let original = encode_settings_bytecode(&document("editor-id")).unwrap();
    let merged = encode_settings_bytecode(&document("aligned-id")).unwrap();
    fs::write(&destination, &original).unwrap();
    fs::write(&staged_path, &merged).unwrap();
    let mut changes = EditorChangeSet {
        settings_writes: vec![EditorSettingsWrite {
            path: staged_path.clone(),
            expected_hash: settings_file_hash(&staged_path).unwrap(),
            document: document("aligned-id").into(),
            encoded: None,
        }],
        ..Default::default()
    };
    let project = file_snapshot(&[(relative.to_str().unwrap(), &original)]);
    let generated =
        redirect_staged_settings_writes(&mut changes, &stage, &root, Some(&project)).unwrap();
    let write = &changes.settings_writes[0];
    assert_eq!(write.path, destination);
    assert_eq!(
        write.expected_hash,
        settings_file_hash(&destination).unwrap()
    );
    assert!(generated.entries.get(&relative) == Some(&SnapshotEntry::File(merged)));
    assert_eq!(fs::read(&destination).unwrap(), original);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn staged_settings_redirect_keeps_concurrent_write_protection() {
    let root = crate::tests::support::temp_dir("staged-settings-concurrency");
    let stage = root.join("stage");
    let relative = Path::new("settings.renium");
    let destination = root.join(relative);
    let document = SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances: Vec::new(),
    };
    let bytes = encode_settings_bytecode(&document).unwrap();
    for existed in [false, true] {
        for with_snapshot in [false, true] {
            let project = if existed {
                file_snapshot(&[("settings.renium", &bytes)])
            } else {
                ProjectSnapshot::default()
            };
            let expected = with_snapshot.then_some(&project);
            for concurrent_edit in [false, true] {
                if existed || concurrent_edit {
                    fs::write(
                        &destination,
                        if concurrent_edit {
                            b"newer editor data".as_slice()
                        } else {
                            bytes.as_slice()
                        },
                    )
                    .unwrap();
                } else if destination.exists() {
                    fs::remove_file(&destination).unwrap();
                }
                let hash = existed.then(|| Sha256::digest(&bytes).into());
                let mut changes = EditorChangeSet {
                    settings_writes: vec![EditorSettingsWrite {
                        path: stage.join(relative),
                        expected_hash: hash,
                        document: document.clone().into(),
                        encoded: None,
                    }],
                    ..Default::default()
                };
                let result = redirect_staged_settings_writes(&mut changes, &stage, &root, expected);
                if concurrent_edit {
                    let message = result.err().unwrap().to_string();
                    assert!(message.contains("changed while its Studio update was being prepared"));
                    assert_eq!(fs::read(&destination).unwrap(), b"newer editor data");
                } else {
                    assert!(result.is_ok());
                    assert_eq!(changes.settings_writes[0].expected_hash, hash);
                    assert_eq!(changes.settings_writes[0].path, destination);
                }
            }
        }
    }
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn project_binding_uses_saved_pairing_without_enabling_live_sync() {
    let root = crate::tests::support::temp_dir("saved-studio-pairing");
    let records = root.join(".renium").join(RECORD_DIR);
    fs::create_dir_all(&records).unwrap();
    let mut record = PairRecord {
        version: RECORD_VERSION,
        identity: PairIdentity {
            experience: canonical_string(&root).unwrap(),
            project: canonical_string(&root).unwrap(),
            fingerprint: "prior-configuration".into(),
            game_id: Some(10),
            place_id: Some(20),
            local_file: None,
        },
        mode: PairMode::Reconcile,
        conflict_preference: ConflictPreference::None,
        runtime_settings: Map::new(),
        baseline: None,
        head: None,
        conflicts: Vec::new(),
        resolution_required: false,
        last_runtime_id: None,
        local_file_stamp: None,
        local_file_digest: None,
        studio_checkpoint: None,
        differences: Vec::new(),
    };
    let write = |name: &str, record: &PairRecord| {
        fs::write(records.join(name), rmp_serde::to_vec(record).unwrap()).unwrap();
    };
    assert_eq!(saved_studio_target_for_root(&root, &root).unwrap(), None);
    write("first.rmp", &record);
    write("duplicate.rmp", &record);
    assert_eq!(
        saved_studio_target_for_root(&root, &root).unwrap(),
        Some(crate::automation::StudioReopenTarget {
            file: None,
            game_id: Some(10),
            place_id: Some(20),
        })
    );
    assert!(!root.join(".renium/live-watch-state.enabled").exists());
    record.identity.project = "a different project".into();
    write("foreign.rmp", &record);
    assert!(
        saved_studio_target_for_root(&root, &root)
            .unwrap()
            .is_some()
    );
    record.identity.project = canonical_string(&root).unwrap();
    record.identity.place_id = Some(30);
    write("different-place.rmp", &record);
    assert_eq!(saved_studio_target_for_root(&root, &root).unwrap(), None);
    fs::remove_dir_all(root).unwrap();
}

fn file_snapshot(entries: &[(&str, &[u8])]) -> ProjectSnapshot {
    ProjectSnapshot {
        entries: entries
            .iter()
            .map(|(path, bytes)| (PathBuf::from(path), SnapshotEntry::File(bytes.to_vec())))
            .collect(),
    }
}

#[test]
fn comparison_capture_retries_one_transient_studio_change() {
    let mut attempts = 0;
    let value = retry_transient_studio_capture(|| {
        attempts += 1;
        if attempts == 1 {
            anyhow::bail!(
                "Studio changed Workspace while native import was staged; retry the sync"
            );
        }
        Ok(42)
    })
    .unwrap();
    assert_eq!(value, 42);
    assert_eq!(attempts, 2);

    let mut persistent_attempts = 0;
    let error = retry_transient_studio_capture::<()>(|| {
        persistent_attempts += 1;
        anyhow::bail!("Studio changed Workspace while native import was staged; retry the sync")
    })
    .unwrap_err();
    assert!(error.to_string().contains("retry the sync"));
    assert_eq!(persistent_attempts, 2);

    let mut ordinary_attempts = 0;
    let error = retry_transient_studio_capture::<()>(|| {
        ordinary_attempts += 1;
        anyhow::bail!("invalid snapshot")
    })
    .unwrap_err();
    assert_eq!(error.to_string(), "invalid snapshot");
    assert_eq!(ordinary_attempts, 1);
}

#[test]
fn protected_snapshot_handoff_replaces_only_its_originating_runtime() {
    let context = BoundContext {
        id: 1,
        initialized: true,
        project: "project".into(),
        root: "root".into(),
        experience: String::new(),
        source: "src".into(),
        resource_lease: None,
        place_id: Some(0),
        game_id: Some(0),
        selector: "Fixture".into(),
        runtime_id: Some("old".into()),
        plugin_build: None,
        fingerprint: "same".into(),
    };
    assert!(
        reopened_push_context(&context, &Map::new())
            .unwrap()
            .is_none()
    );
    let mut summary = Map::from_iter([(
        "protectedOfflineApply".into(),
        json!({
            "ok": true, "previousRuntimeId": "old", "reopenedRuntimeId": "new"
        }),
    )]);
    let replacement = reopened_push_context(&context, &summary).unwrap().unwrap();
    assert_eq!(replacement.runtime_id.as_deref(), Some("new"));
    assert_eq!(replacement.project, context.project);
    assert_eq!(replacement.fingerprint, context.fingerprint);
    for invalid in [
        json!({"ok":true,"previousRuntimeId":"foreign","reopenedRuntimeId":"new"}),
        json!({"ok":true,"previousRuntimeId":"old","reopenedRuntimeId":"old"}),
        json!({"ok":true,"previousRuntimeId":"old","reopenedRuntimeId":""}),
        json!({"ok":false,"previousRuntimeId":"old","reopenedRuntimeId":"new"}),
    ] {
        summary.insert("protectedOfflineApply".into(), invalid);
        assert!(reopened_push_context(&context, &summary).is_err());
    }
}

#[test]
fn studio_checkpoint_requires_uninterrupted_clean_tracking() {
    let context = BoundContext {
        id: 1,
        initialized: true,
        project: String::new(),
        root: String::new(),
        experience: String::new(),
        source: String::new(),
        resource_lease: None,
        place_id: None,
        game_id: None,
        selector: String::new(),
        runtime_id: Some("runtime".to_string()),
        plugin_build: None,
        fingerprint: String::new(),
    };
    let services = sync_services();
    let generations = services
        .iter()
        .map(|service| (service.clone(), Value::from(7)))
        .collect::<Map<_, _>>();
    let state = json!({
        "tracking": true,
        "trackedServices": services.len(),
        "dirtyServices": [],
        "fullSyncServices": [],
        "runtimeId": "runtime",
        "changeTrackerVersion": 4,
        "seq": 9,
        "serviceGenerations": generations.clone(),
        "checkpointGenerations": generations,
    });
    let checkpoint = StudioCheckpoint::from_state(&context, &state).unwrap();
    assert!(checkpoint.matches_state(&context, &state));
    assert_eq!(
        checkpoint.changed_services(&context, &state),
        Some(Vec::new())
    );

    let mut transaction_generation = state.clone();
    transaction_generation["serviceGenerations"][&services[0]] = Value::from(8);
    assert!(checkpoint.matches_state(&context, &transaction_generation));

    let mut interrupted = state.clone();
    interrupted["checkpointGenerations"][&services[0]] = Value::from(8);
    assert!(!checkpoint.matches_state(&context, &interrupted));
    assert_eq!(
        checkpoint.changed_services(&context, &interrupted),
        Some(vec![services[0].clone()])
    );

    let mut pending = state.clone();
    pending["dirtyServices"] = json!([&services[0]]);
    assert!(!checkpoint.matches_state(&context, &pending));
    assert_eq!(
        checkpoint.changed_services(&context, &pending),
        Some(vec![services[0].clone()])
    );

    let mut moved_references = state;
    moved_references["referencePathsMayChange"] = Value::Bool(true);
    assert_eq!(
        checkpoint.changed_services(&context, &moved_references),
        None
    );

    let mut unexpected_service = moved_references;
    unexpected_service["referencePathsMayChange"] = Value::Bool(false);
    unexpected_service["checkpointGenerations"]["UnexpectedService"] = Value::from(7);
    assert!(StudioCheckpoint::from_state(&context, &unexpected_service).is_none());
    assert_eq!(
        checkpoint.changed_services(&context, &unexpected_service),
        None
    );
}

#[test]
fn source_only_push_skips_service_readback_only_after_exact_verification() {
    let paths = HashSet::from([PathBuf::from(
        "src/ServerScriptService/Verified.server.luau",
    )]);
    let verified = Map::from_iter([
        ("sourceVerified".to_string(), Value::from(1)),
        ("sourceVerifyFailed".to_string(), Value::from(0)),
    ]);
    assert!(exact_source_push_verified(&verified, &paths));

    let unverified = Map::from_iter([
        ("sourceVerified".to_string(), Value::from(0)),
        ("sourceVerifyFailed".to_string(), Value::from(0)),
    ]);
    assert!(!exact_source_push_verified(&unverified, &paths));
}

#[test]
fn a_weld_studio_replaced_with_a_rigid_constraint_is_not_put_back() {
    use super::pairing::drop_joints_studio_migrated;
    let document = |joint: Option<(&str, &str)>| {
        let mut instances = vec![
            SettingsBytecodeInstance::new(
                "ws".into(),
                "Workspace".into(),
                "Workspace".into(),
                None,
            ),
            SettingsBytecodeInstance::new("hat".into(), "Hat".into(), "Accessory".into(), Some(0)),
            SettingsBytecodeInstance::new("handle".into(), "Handle".into(), "Part".into(), Some(1)),
            SettingsBytecodeInstance::new(
                "mesh".into(),
                "Mesh".into(),
                "SpecialMesh".into(),
                Some(2),
            ),
        ];
        if let Some((class, name)) = joint {
            instances.push(SettingsBytecodeInstance::new(
                name.to_lowercase(),
                name.into(),
                class.into(),
                Some(2),
            ));
        }
        SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances,
        }
    };
    let mut editor = document(Some(("Weld", "AccessoryWeld")));
    drop_joints_studio_migrated(
        &mut editor,
        &document(Some(("RigidConstraint", "AccessoryRigidConstraint"))),
    );
    assert_eq!(
        editor,
        document(None),
        "Studio replaced the weld while loading"
    );

    let mut editor = document(Some(("Weld", "AccessoryWeld")));
    drop_joints_studio_migrated(&mut editor, &document(None));
    assert_eq!(
        editor,
        document(Some(("Weld", "AccessoryWeld"))),
        "a deleted weld is a Studio edit"
    );

    let mut editor = document(Some(("Weld", "AccessoryWeld")));
    let mut both = document(Some(("RigidConstraint", "AccessoryRigidConstraint")));
    both.instances.push(SettingsBytecodeInstance::new(
        "weld".into(),
        "AccessoryWeld".into(),
        "Weld".into(),
        Some(2),
    ));
    drop_joints_studio_migrated(&mut editor, &both);
    assert_eq!(
        editor,
        document(Some(("Weld", "AccessoryWeld"))),
        "Studio still holds the weld"
    );

    let mut editor = document(Some(("Weld", "Joint")));
    drop_joints_studio_migrated(
        &mut editor,
        &document(Some(("RigidConstraint", "AccessoryRigidConstraint"))),
    );
    assert_eq!(
        editor,
        document(Some(("Weld", "Joint"))),
        "only the accessory weld is migrated"
    );

    for (duplicate_editor, duplicate_studio) in [(true, false), (false, true), (true, true)] {
        let mut editor = document(Some(("Weld", "AccessoryWeld")));
        let mut studio = document(Some(("RigidConstraint", "AccessoryRigidConstraint")));
        for (document, duplicate) in [
            (&mut editor, duplicate_editor),
            (&mut studio, duplicate_studio),
        ] {
            if duplicate {
                let parent = document.instances.len();
                document.instances.push(SettingsBytecodeInstance::new(
                    "second-hat".into(),
                    "Hat".into(),
                    "Accessory".into(),
                    Some(0),
                ));
                document.instances.push(SettingsBytecodeInstance::new(
                    "second-handle".into(),
                    "Handle".into(),
                    "Part".into(),
                    Some(parent),
                ));
            }
        }
        let before = editor.clone();
        drop_joints_studio_migrated(&mut editor, &studio);
        assert_eq!(
            editor, before,
            "a same-named accessory is not proof of migration"
        );
    }
}

#[test]
fn captured_optional_properties_still_verify_explicit_writes() {
    for (class, name, previous, desired) in [
        (
            "Lighting",
            "PrioritizeLightingQuality",
            json!(false),
            json!(true),
        ),
        (
            "Lighting",
            "LightingStyle",
            json!({"_type":"EnumItem", "name":"Soft"}),
            json!({"_type":"EnumItem", "name":"Realistic"}),
        ),
        ("Frame", "InputSinkEnabled", json!(false), json!(true)),
    ] {
        let instance =
            SettingsBytecodeInstance::new("target".into(), class.into(), class.into(), None);
        let before = Map::from_iter([(name.to_string(), previous)]);
        let desired = Map::from_iter([(name.to_string(), desired)]);
        assert!(
            expected_map_mismatch(&desired, &before, true, &instance).is_some(),
            "{class}.{name}"
        );
        assert!(
            changed_map_mismatch(&before, &desired, &before, true, &instance).is_some(),
            "{class}.{name}"
        );
        assert_eq!(
            expected_map_mismatch(&desired, &desired, true, &instance),
            None
        );
        assert_eq!(
            changed_map_mismatch(&before, &desired, &desired, true, &instance),
            None
        );
        assert_eq!(
            expected_map_mismatch(&desired, &Map::new(), true, &instance),
            None
        );
        assert_eq!(
            changed_map_mismatch(&before, &desired, &Map::new(), true, &instance),
            None
        );
    }
}

#[test]
fn a_field_the_files_never_carry_is_not_checked_for_retention() {
    let description = SettingsBytecodeInstance::new(
        "hd".into(),
        "HumanoidDescription".into(),
        "HumanoidDescription".into(),
        Some(0),
    );
    let gray = Map::from_iter([(
        "HeadColor".to_string(),
        json!({"_type": "Color3", "r": 0.5, "g": 0.5, "b": 0.5}),
    )]);
    assert_eq!(
        changed_map_mismatch(&gray, &Map::new(), &gray, true, &description),
        None,
        "HeadColor does not serialize, so the files never state it"
    );
    let tire =
        SettingsBytecodeInstance::new("tire".into(), "Tire".into(), "MeshPart".into(), Some(0));
    let fidelity = Map::from_iter([(
        "CollisionFidelity".to_string(),
        json!({"_type": "EnumItem", "name": "Default"}),
    )]);
    assert_eq!(
        changed_map_mismatch(&fidelity, &Map::new(), &fidelity, true, &tire),
        None,
        "a capture may carry the fidelity the files leave out"
    );
    let sound = SettingsBytecodeInstance::new("sq".into(), "SQ".into(), "Sound".into(), Some(0));
    let muted = Map::from_iter([("IsMutedForCapture".to_string(), json!(false))]);
    assert_eq!(
        changed_map_mismatch(&Map::new(), &muted, &Map::new(), true, &sound),
        None,
        "a capture may leave out a field the reflection database does not know"
    );
    let reflectance = Map::from_iter([("Reflectance".to_string(), json!(0.7))]);
    assert!(
        changed_map_mismatch(&Map::new(), &reflectance, &Map::new(), true, &tire).is_some(),
        "a known saved field the capture lost is still a retention failure"
    );
    let unmuted = Map::from_iter([("IsMutedForCapture".to_string(), json!(true))]);
    assert!(
        changed_map_mismatch(&Map::new(), &unmuted, &Map::new(), true, &sound).is_some(),
        "a set value of an unknown field the capture lost is still a retention failure"
    );
}

#[test]
fn a_service_field_the_push_never_sends_is_not_demanded_from_studio() {
    for (service, name, files) in [
        ("Lighting", "Outlines", json!(false)),
        ("StarterGui", "ResetPlayerGuiOnSpawn", json!(false)),
        ("Workspace", "Wind", json!(3.0)),
    ] {
        let root =
            SettingsBytecodeInstance::new("root".into(), service.into(), service.into(), None);
        let desired = Map::from_iter([(name.to_string(), files)]);
        assert_eq!(
            expected_map_mismatch(&desired, &Map::new(), true, &root),
            None,
            "{service}.{name}"
        );
        assert_eq!(
            changed_map_mismatch(&Map::new(), &desired, &Map::new(), true, &root),
            None,
            "{service}.{name}"
        );
    }
    let lighting =
        SettingsBytecodeInstance::new("root".into(), "Lighting".into(), "Lighting".into(), None);
    let outlines = |value: bool| Map::from_iter([("Outlines".to_string(), json!(value))]);
    assert!(
        changed_map_mismatch(
            &Map::new(),
            &outlines(false),
            &outlines(true),
            true,
            &lighting
        )
        .is_some(),
        "a value Studio reports still has to match"
    );
    let brightness = Map::from_iter([("Brightness".to_string(), json!(3.0))]);
    assert!(
        changed_map_mismatch(&Map::new(), &brightness, &Map::new(), true, &lighting).is_some(),
        "a root field the push writes is still demanded"
    );
    let frame =
        SettingsBytecodeInstance::new("frame".into(), "Frame".into(), "Frame".into(), Some(0));
    let draggable = Map::from_iter([("Draggable".to_string(), json!(true))]);
    assert!(
        expected_map_mismatch(&draggable, &Map::new(), true, &frame).is_some(),
        "an import writes a descendant's deprecated field and the capture reports it"
    );

    let path = PathBuf::from("instances/Lighting.renium");
    let entry = |properties: Value| {
        let mut root = lighting.clone();
        root.properties = properties.as_object().unwrap().clone();
        SnapshotEntry::File(
            encode_settings_bytecode(&SettingsBytecode {
                version: SETTINGS_BINARY_VERSION,
                instances: vec![root],
            })
            .unwrap(),
        )
    };
    let before = entry(json!({"Brightness": 2.0}));
    let desired = entry(json!({"Brightness": 3.0, "Outlines": false}));
    let pushed = entry(json!({"Brightness": 3.0}));
    assert_eq!(
        settings_delta_mismatch(&path, Some(&before), Some(&desired), Some(&pushed), None).unwrap(),
        None
    );
    assert!(
        settings_delta_mismatch(&path, Some(&before), Some(&desired), Some(&before), None)
            .unwrap()
            .is_some_and(|detail| detail.contains("Brightness"))
    );
}

#[test]
fn retention_checks_distinguish_editor_state_and_authored_attributes() {
    for (class, name) in [
        ("Workspace", "InsertPoint"),
        ("WeldConstraint", "State"),
        ("GuiObject", "AbsoluteSize"),
        ("Sound", "TimeLength"),
        ("ScrollingFrame", "CanvasPosition"),
    ] {
        let instance = SettingsBytecodeInstance::new("id".into(), class.into(), class.into(), None);
        let values = |value| Map::from_iter([(name.to_string(), json!(value))]);
        assert_eq!(
            expected_map_mismatch(&values(1), &values(2), true, &instance),
            None,
            "{class}.{name}"
        );
        assert_eq!(
            changed_map_mismatch(&values(0), &values(1), &values(2), true, &instance),
            None,
            "{class}.{name}"
        );
        assert!(expected_map_mismatch(&values(1), &values(2), false, &instance).is_some());
        assert!(
            changed_map_mismatch(&values(1), &Map::new(), &values(0), false, &instance).is_some()
        );
    }
    let part = SettingsBytecodeInstance::new("id".into(), "Part".into(), "Part".into(), Some(0));
    let reflectance = Map::from_iter([("Reflectance".into(), json!(0))]);
    assert!(changed_map_mismatch(&reflectance, &Map::new(), &reflectance, false, &part).is_some());
    for value in [json!(false), json!(0), json!("")] {
        let attribute = Map::from_iter([("UserValue".into(), value)]);
        assert!(expected_map_mismatch(&attribute, &Map::new(), false, &part).is_some());
        assert!(changed_map_mismatch(&attribute, &Map::new(), &attribute, false, &part).is_some());
        assert_eq!(
            changed_map_mismatch(&attribute, &Map::new(), &Map::new(), false, &part),
            None
        );
    }
    let marker = Map::from_iter([("RBX_LightingTechnologyUnifiedMigration".into(), json!(true))]);
    assert_eq!(
        expected_map_mismatch(&marker, &Map::new(), false, &part),
        None
    );
    assert_eq!(
        changed_map_mismatch(&marker, &Map::new(), &marker, false, &part),
        None
    );
}

#[test]
fn push_verification_distinguishes_engine_identity_from_authored_data() {
    let mut instance =
        SettingsBytecodeInstance::new("part".into(), "Part".into(), "Part".into(), None);
    for name in ["UniqueId", "HistoryId"] {
        let identity = |value| {
            Map::from_iter([(
                name.to_string(),
                json!({"_type": "UniqueId", "value": value}),
            )])
        };
        let before = identity("00000001000000000000000000000001");
        let desired = identity("00000002000000000000000000000002");
        let observed = identity("00000003000000000000000000000003");
        assert_eq!(
            expected_map_mismatch(&desired, &observed, true, &instance),
            None
        );
        assert_eq!(
            changed_map_mismatch(&before, &desired, &observed, true, &instance),
            None
        );
        assert!(expected_map_mismatch(&desired, &observed, false, &instance).is_some());
        assert!(changed_map_mismatch(&before, &desired, &observed, false, &instance).is_some());
        instance.properties = desired.clone();
        let document = SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: vec![instance.clone()],
        };
        let decoded =
            decode_settings_bytecode(&encode_settings_bytecode(&document).unwrap()).unwrap();
        assert_eq!(decoded.instances[0].properties[name], desired[name]);
    }
    for name in ["MeshSize", "CollisionFidelity", "TexturePack", "Part0"] {
        let before = Map::from_iter([(name.to_string(), json!("before"))]);
        let desired = Map::from_iter([(name.to_string(), json!("source"))]);
        let observed = Map::from_iter([(name.to_string(), json!("destination"))]);
        assert!(expected_map_mismatch(&desired, &observed, true, &instance).is_some());
        assert!(changed_map_mismatch(&before, &desired, &observed, true, &instance).is_some());
    }
}

#[test]
fn push_verification_accepts_the_texture_pack_studio_issues_for_an_appearance() {
    let appearance = SettingsBytecodeInstance::new(
        "appearance".into(),
        "SurfaceAppearance".into(),
        "SurfaceAppearance".into(),
        None,
    );
    let pack = |value: &str| Map::from_iter([("TexturePack".to_string(), json!(value))]);
    let before = pack("rbxassetid://1");
    let desired = pack("rbxassetid://102986607617177");
    let observed = pack("rbxassetid://99673417571091");
    assert_eq!(
        expected_map_mismatch(&desired, &observed, true, &appearance),
        None
    );
    assert_eq!(
        changed_map_mismatch(&before, &desired, &observed, true, &appearance),
        None
    );
    assert!(expected_map_mismatch(&desired, &pack(""), true, &appearance).is_some());
    assert!(expected_map_mismatch(&desired, &Map::new(), true, &appearance).is_some());
    let part = SettingsBytecodeInstance::new("part".into(), "Part".into(), "Part".into(), None);
    assert!(expected_map_mismatch(&desired, &observed, true, &part).is_some());
}

#[test]
fn terrain_readback_accepts_rebuilt_cache_only_after_verified_voxel_replacement() {
    let instance = SettingsBytecodeInstance::new(
        "terrain".into(),
        "Terrain".into(),
        "Terrain".into(),
        Some(0),
    );
    let grid = |bytes: &str| json!({"_type": "BinaryString", "base64": base64::encode(bytes)});
    let properties = |voxels: &str, cache: &str| {
        Map::from_iter([
            ("SmoothGrid".into(), grid(voxels)),
            ("PhysicsGrid".into(), grid(cache)),
        ])
    };
    let before = properties("empty", "empty cache");
    let desired = properties("voxels", "saved cache");
    let observed = properties("voxels", "rebuilt cache");
    assert!(expected_map_mismatch(&desired, &observed, true, &instance).is_none());
    assert!(changed_map_mismatch(&before, &desired, &observed, true, &instance).is_none());
    // Without a voxel replacement, a physics-only write still verifies exactly.
    let physics_only = properties("voxels", "previous cache");
    assert!(changed_map_mismatch(&physics_only, &desired, &observed, true, &instance).is_some());
    for actual in [properties("outside painting", "rebuilt cache"), Map::new()] {
        assert!(expected_map_mismatch(&desired, &actual, true, &instance).is_some());
        assert!(changed_map_mismatch(&before, &desired, &actual, true, &instance).is_some());
    }
    let mut missing_cache = observed.clone();
    missing_cache.remove("PhysicsGrid");
    assert!(expected_map_mismatch(&desired, &missing_cache, true, &instance).is_some());
    // Same-named attributes remain authored values.
    assert!(expected_map_mismatch(&desired, &observed, false, &instance).is_some());
    assert!(changed_map_mismatch(&before, &desired, &observed, false, &instance).is_some());
}

#[test]
fn push_verification_follows_game_settings_policy_without_hiding_saved_values() {
    let path = PathBuf::from("src/Players/__roblox_sync_settings.renium");
    let document = |name: &str, capacity: Option<i64>, auto_loads: bool| SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances: vec![SettingsBytecodeInstance {
            settings_id: "players".to_string(),
            name: "Players".to_string(),
            class_name: "Players".to_string(),
            parent_index: None,
            properties: std::iter::once(("CharacterAutoLoads".to_string(), json!(auto_loads)))
                .chain(capacity.map(|value| (name.to_string(), json!(value))))
                .collect(),
            attributes: Map::new(),
        }],
    };
    let entry = |document: &SettingsBytecode| {
        SnapshotEntry::File(encode_settings_bytecode(document).unwrap())
    };
    for name in [
        "MaxPlayers",
        "PreferredPlayers",
        "MaxPlayersInternal",
        "PreferredPlayersInternal",
    ] {
        let before = entry(&document(name, Some(12), true));
        let desired_document = document(name, Some(60), false);
        let desired = entry(&desired_document);
        let observed = entry(&document(name, None, false));
        let failed_ordinary_write = entry(&document(name, None, true));
        for previous in [None, Some(&before)] {
            assert_eq!(
                settings_delta_mismatch(&path, previous, Some(&desired), Some(&observed), None)
                    .unwrap(),
                None,
                "{name} is not a push target"
            );
            assert!(
                settings_delta_mismatch(
                    &path,
                    previous,
                    Some(&desired),
                    Some(&failed_ordinary_write),
                    None
                )
                .unwrap()
                .is_some_and(|detail| detail.contains("CharacterAutoLoads"))
            );
        }
        let decoded = settings_document(Some(&desired)).unwrap();
        let logical_name = name.strip_suffix("Internal").unwrap_or(name);
        assert_eq!(decoded.instances[0].properties[logical_name], json!(60));
        assert!(!settings_documents_equivalent(
            &decoded,
            &settings_document(Some(&observed)).unwrap()
        ));

        // A store that differs from Studio in the capacity alone gives a
        // push nothing to write; with another field it is a difference.
        let observed_capacity = document(name, Some(12), false);
        assert!(super::verify::differs_only_in_externally_managed_fields(
            &desired_document,
            &observed_capacity
        ));
        assert!(super::verify::differs_only_in_externally_managed_fields(
            &desired_document,
            &document(name, None, false)
        ));
        assert!(!super::verify::differs_only_in_externally_managed_fields(
            &desired_document,
            &document(name, Some(12), true)
        ));

        // This policy applies to properties of the actual service root,
        // not attributes, a same-named Folder, or a nested instance.
        let missing = Map::new();
        let mut instance = desired_document.instances[0].clone();
        let requested = Map::from_iter([(name.to_string(), json!(60))]);
        for (class, parent, properties) in [
            ("Players", None, false),
            ("Folder", None, true),
            ("Players", Some(0), true),
        ] {
            instance.class_name = class.to_string();
            instance.parent_index = parent;
            assert!(expected_map_mismatch(&requested, &missing, properties, &instance).is_some());
            assert!(
                changed_map_mismatch(&missing, &requested, &missing, properties, &instance)
                    .is_some()
            );
        }
    }
}

#[test]
fn push_verification_batch_preserves_all_failures_and_decode_errors() {
    let before = ProjectSnapshot::default();
    let mut desired = ProjectSnapshot::default();
    let mut observed = ProjectSnapshot::default();
    for service in ["ReplicatedStorage", "ServerStorage", "Workspace"] {
        let path = PathBuf::from(format!("src/{service}/__roblox_sync_settings.renium"));
        let mut document = SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: vec![SettingsBytecodeInstance::new(
                "root".to_string(),
                service.to_string(),
                service.to_string(),
                None,
            )],
        };
        for index in 1..=2_048 {
            let mut part = SettingsBytecodeInstance::new(
                format!("part{index}"),
                format!("Part{index:04}"),
                "Part".to_string(),
                Some(0),
            );
            part.properties.insert("Anchored".to_string(), json!(true));
            document.instances.push(part);
        }
        desired.entries.insert(
            path.clone(),
            SnapshotEntry::File(encode_settings_bytecode(&document).unwrap()),
        );
        if service != "ServerStorage" {
            for index in [17, 1_537] {
                document.instances[index]
                    .properties
                    .insert("Anchored".to_string(), json!(false));
            }
        }
        observed.entries.insert(
            path,
            SnapshotEntry::File(encode_settings_bytecode(&document).unwrap()),
        );
    }
    let script = PathBuf::from("src/ServerScriptService/Script.server.luau");
    desired
        .entries
        .insert(script.clone(), SnapshotEntry::File(b"print(1)\n".to_vec()));
    observed
        .entries
        .insert(script, SnapshotEntry::File(b"print(1)\r\n".to_vec()));
    let paths = desired.entries.keys().cloned().collect();
    let expected_paths = vec![
        PathBuf::from("src/ReplicatedStorage/__roblox_sync_settings.renium"),
        PathBuf::from("src/Workspace/__roblox_sync_settings.renium"),
    ];
    for workers in [1, 4] {
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(workers)
            .build()
            .unwrap();
        pool.install(|| {
            let (mismatches, detail) = snapshot_intended_delta_mismatches(
                &before,
                &desired,
                &observed,
                &paths,
                &mut HashMap::new(),
            )
            .unwrap();
            assert_eq!(mismatches, expected_paths);
            assert!(
                detail.as_deref().is_some_and(
                    |detail| detail.starts_with("Part0017.Anchored was not retained (")
                ),
                "{detail:?}"
            );
            let mut malformed = observed.clone();
            malformed
                .entries
                .insert(expected_paths[0].clone(), SnapshotEntry::File(vec![0]));
            assert!(
                snapshot_intended_delta_mismatches(
                    &before,
                    &desired,
                    &malformed,
                    &paths,
                    &mut HashMap::new()
                )
                .is_err()
            );
        });
    }
}

#[test]
fn targeted_push_verification_ignores_package_modified_state() {
    let path = PathBuf::from("src/ReplicatedStorage/__roblox_sync_settings.renium");
    let instance = |id: &str, name: &str, class_name: &str, parent_index: Option<usize>| {
        SettingsBytecodeInstance {
            settings_id: id.to_string(),
            name: name.to_string(),
            class_name: class_name.to_string(),
            parent_index,
            properties: Map::new(),
            attributes: Map::new(),
        }
    };
    let snapshot = |modified_state: i64, archivable: Option<bool>| {
        let event_properties = archivable
            .map(|archivable| Map::from_iter([("Archivable".to_string(), Value::Bool(archivable))]))
            .unwrap_or_default();
        let document = SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: vec![
                instance("root", "ReplicatedStorage", "ReplicatedStorage", None),
                instance("package", "testPackage", "Folder", Some(0)),
                SettingsBytecodeInstance {
                    settings_id: "link".to_string(),
                    name: "PackageLink".to_string(),
                    class_name: "PackageLink".to_string(),
                    parent_index: Some(1),
                    properties: Map::from_iter([(
                        "ModifiedState".to_string(),
                        Value::from(modified_state),
                    )]),
                    attributes: Map::new(),
                },
                SettingsBytecodeInstance {
                    settings_id: "event".to_string(),
                    name: "RemoteEvent".to_string(),
                    class_name: "RemoteEvent".to_string(),
                    parent_index: Some(1),
                    properties: event_properties,
                    attributes: Map::new(),
                },
            ],
        };
        ProjectSnapshot {
            entries: [(
                path.clone(),
                SnapshotEntry::File(encode_settings_bytecode(&document).unwrap()),
            )]
            .into_iter()
            .collect(),
        }
    };
    let before = snapshot(1, Some(false));
    let desired = snapshot(-1, Some(true));
    let observed = snapshot(1, None);
    let paths = HashSet::from([path.clone()]);
    let empty = ProjectSnapshot {
        entries: BTreeMap::new(),
    };
    let (added_mismatches, added_detail) = snapshot_intended_delta_mismatches(
        &empty,
        &desired,
        &observed,
        &paths,
        &mut HashMap::new(),
    )
    .unwrap();
    assert!(added_mismatches.is_empty(), "{added_detail:?}");
    let (mismatches, _) = snapshot_intended_delta_mismatches(
        &before,
        &desired,
        &observed,
        &paths,
        &mut HashMap::new(),
    )
    .unwrap();
    assert!(mismatches.is_empty());

    let observed = snapshot(1, Some(false));
    let (mismatches, detail) = snapshot_intended_delta_mismatches(
        &before,
        &desired,
        &observed,
        &paths,
        &mut HashMap::new(),
    )
    .unwrap();
    assert_eq!(mismatches, vec![path]);
    assert!(detail.is_some_and(|detail| detail.contains("Archivable")));
}

#[test]
fn a_verified_pair_keeps_reconcile_on_a_verify_rerun() {
    use super::coordinator::continued_pair_mode;
    assert_eq!(
        continued_pair_mode(PairMode::Verify, Some(PairMode::Reconcile), false, false),
        PairMode::Reconcile
    );
    assert_eq!(
        continued_pair_mode(PairMode::Verify, Some(PairMode::Reconcile), true, false),
        PairMode::Verify
    );
    assert_eq!(
        continued_pair_mode(PairMode::Verify, Some(PairMode::Reconcile), false, true),
        PairMode::Verify
    );
    assert_eq!(
        continued_pair_mode(PairMode::Verify, Some(PairMode::Verify), false, false),
        PairMode::Verify
    );
    assert_eq!(
        continued_pair_mode(PairMode::Verify, None, false, false),
        PairMode::Verify
    );
    assert_eq!(
        continued_pair_mode(PairMode::Reconcile, Some(PairMode::Verify), false, false),
        PairMode::Reconcile
    );
}

#[test]
fn unchanged_local_file_bootstraps_a_replacement_runtime() {
    let digest = "same-digest";

    assert!(should_bootstrap_studio_from_editor(
        PairMode::Reconcile,
        Some("old-runtime"),
        Some("new-runtime"),
        Some(digest),
        Some(digest),
    ));
    assert!(!should_bootstrap_studio_from_editor(
        PairMode::Verify,
        Some("old-runtime"),
        Some("new-runtime"),
        Some(digest),
        Some(digest),
    ));
}

#[test]
fn changed_or_unknown_local_file_uses_normal_reconciliation() {
    assert!(!should_bootstrap_studio_from_editor(
        PairMode::Reconcile,
        Some("old-runtime"),
        Some("new-runtime"),
        Some("previous-digest"),
        Some("changed-digest"),
    ));
    assert!(!should_bootstrap_studio_from_editor(
        PairMode::Reconcile,
        None,
        Some("new-runtime"),
        Some("digest"),
        Some("digest"),
    ));
    assert!(!should_bootstrap_studio_from_editor(
        PairMode::Reconcile,
        Some("old-runtime"),
        Some("new-runtime"),
        None,
        Some("digest"),
    ));
}

#[test]
fn runtime_bootstrap_requires_tracking_without_fresh_edits() {
    assert!(studio_runtime_bootstrap_safe(&json!({
        "tracking": true,
        "dirtyServices": ["ReplicatedStorage"],
        "restoredPendingServices": ["ReplicatedStorage"],
    })));
    assert!(!studio_runtime_bootstrap_safe(&json!({
        "tracking": false,
        "dirtyServices": [],
        "restoredPendingServices": [],
    })));
    assert!(!studio_runtime_bootstrap_safe(&json!({
        "tracking": true,
        "dirtyServices": ["ReplicatedStorage", "StarterGui"],
        "restoredPendingServices": ["ReplicatedStorage"],
    })));
}

#[test]
fn staged_source_root_accepts_cli_relative_and_bound_absolute_paths() {
    let root = if cfg!(windows) {
        Path::new("C:/project")
    } else {
        Path::new("/project")
    };

    assert_eq!(
        project_relative_source_root(root, Path::new("src")).unwrap(),
        Path::new("src")
    );
    assert_eq!(
        project_relative_source_root(root, &root.join("src")).unwrap(),
        Path::new("src")
    );
}

#[test]
fn a_configuration_change_keeps_the_baseline_and_forces_a_full_capture() {
    let root = crate::tests::support::temp_dir("configuration-change-baseline");
    let snapshot = file_snapshot(&[("src/A.luau", b"return 1")]);
    let mut record = PairRecord {
        version: RECORD_VERSION,
        identity: PairIdentity {
            experience: canonical_string(&root).unwrap(),
            project: canonical_string(&root).unwrap(),
            fingerprint: "before-the-link".into(),
            game_id: Some(10),
            place_id: Some(20),
            local_file: None,
        },
        mode: PairMode::Reconcile,
        conflict_preference: ConflictPreference::None,
        runtime_settings: Map::new(),
        baseline: Some(StoredSnapshot::write(&root, "pair", &snapshot).unwrap()),
        head: None,
        conflicts: vec![Conflict::legacy("stale".into())],
        resolution_required: true,
        last_runtime_id: None,
        local_file_stamp: None,
        local_file_digest: None,
        studio_checkpoint: Some(StudioCheckpoint {
            runtime_id: "runtime".into(),
            change_tracker_version: 4,
            seq: 7,
            service_generations: BTreeMap::new(),
        }),
        differences: Vec::new(),
    };
    note_configuration_change(&mut record);
    assert!(record.baseline.as_ref().unwrap().matches(&snapshot));
    assert!(record.studio_checkpoint.is_none());
    assert!(record.conflicts.is_empty());
    assert!(!record.resolution_required);
    fs::remove_dir_all(root).unwrap();
}

fn workspace_store_snapshot(document: SettingsBytecode) -> ProjectSnapshot {
    file_snapshot(&[(
        "instances/Workspace.renium",
        &encode_settings_bytecode(&document).unwrap(),
    )])
}

#[test]
fn a_store_new_to_the_baseline_still_gets_first_pairing_review() {
    let baseline = file_snapshot(&[("src/Other/Existing.luau", b"return 1")]);
    let mut editor_doc = twin_document();
    twin_add(&mut editor_doc, 0, "Thing", "Folder", json!({}));
    editor_doc.instances[1]
        .attributes
        .insert("Author".into(), json!("files"));
    let mut studio_doc = editor_doc.clone();
    studio_doc.instances[1].attributes.clear();
    let editor = workspace_store_snapshot(editor_doc);
    let studio = workspace_store_snapshot(studio_doc);
    let (_, fresh_conflicts) =
        merge_snapshots(None, &editor, &studio, ConflictPreference::None).unwrap();
    assert!(
        !fresh_conflicts.is_empty(),
        "control must require first-pairing review"
    );
    let (merged, retained_conflicts) =
        merge_snapshots(Some(&baseline), &editor, &studio, ConflictPreference::None).unwrap();
    let merged_doc = settings_document(merged.entries.values().next()).unwrap();
    assert!(
        !retained_conflicts.is_empty(),
        "retained baseline accepted conflicting new scope without review: fresh={fresh_conflicts:?}, merged attributes={:?}",
        merged_doc.instances[1].attributes
    );
}

#[test]
fn a_store_new_to_the_baseline_reports_class_mismatches() {
    let baseline = file_snapshot(&[("src/Other/Existing.luau", b"return 1")]);
    let mut editor_doc = twin_document();
    twin_add(&mut editor_doc, 0, "Thing", "Folder", json!({}));
    let mut studio_doc = twin_document();
    twin_add(&mut studio_doc, 0, "Thing", "Part", json!({}));
    studio_doc.instances[1].settings_id = "observed-thing".into();
    let editor = workspace_store_snapshot(editor_doc);
    let studio = workspace_store_snapshot(studio_doc);
    let (_, fresh_conflicts) =
        merge_snapshots(None, &editor, &studio, ConflictPreference::None).unwrap();
    assert!(
        !fresh_conflicts.is_empty(),
        "control must require first-pairing review"
    );
    let (merged, retained_conflicts) =
        merge_snapshots(Some(&baseline), &editor, &studio, ConflictPreference::None).unwrap();
    let merged_doc = settings_document(merged.entries.values().next()).unwrap();
    assert!(
        !retained_conflicts.is_empty(),
        "retained baseline silently merged class mismatch: fresh={fresh_conflicts:?}, classes={:?}",
        merged_doc
            .instances
            .iter()
            .map(|i| &i.class_name)
            .collect::<Vec<_>>()
    );
}

#[test]
fn a_merge_settles_a_reference_to_an_instance_the_other_side_removed_from_another_store() {
    let instance = |id: &str, name: &str, class_name: &str, parent| {
        SettingsBytecodeInstance::new(
            id.to_string(),
            name.to_string(),
            class_name.to_string(),
            parent,
        )
    };
    let reference = json!({
        "_type": "Ref",
        "settingsId": "target",
        "pathSegments": ["Workspace", "Kit", "Target"],
        "pathOrdinals": [1, 1, 1],
    });
    let workspace_path = PathBuf::from("instances/Workspace.renium");
    let replicated_path = PathBuf::from("instances/ReplicatedStorage.renium");
    let snapshot = |with_target: bool, value: Option<&Value>| {
        let mut workspace = vec![instance("ws", "Workspace", "Workspace", None)];
        if with_target {
            workspace.push(instance("kit", "Kit", "Folder", Some(0)));
            workspace.push(instance("target", "Target", "Folder", Some(1)));
        }
        let mut pointer = instance("pointer", "Pointer", "ObjectValue", Some(0));
        if let Some(value) = value {
            pointer
                .properties
                .insert("Value".to_string(), value.clone());
        }
        let encode = |instances| {
            encode_settings_bytecode(&SettingsBytecode {
                version: SETTINGS_BINARY_VERSION,
                instances,
            })
            .unwrap()
        };
        file_snapshot(&[
            ("instances/Workspace.renium", &encode(workspace)),
            (
                "instances/ReplicatedStorage.renium",
                &encode(vec![
                    instance("rs", "ReplicatedStorage", "ReplicatedStorage", None),
                    pointer,
                ]),
            ),
        ])
    };
    // The files point ReplicatedStorage.Pointer at Workspace.Kit.Target
    // while Studio removes Kit: each store changed on one side only.
    let baseline = snapshot(true, None);
    let editor = snapshot(true, Some(&reference));
    let studio = snapshot(false, None);
    let merge = |preference| {
        let (merged, conflicts, changes) =
            merge_snapshots_with_changes(Some(&baseline), &editor, &studio, preference, None)
                .unwrap();
        let names = settings_document(merged.entries.get(&workspace_path))
            .unwrap()
            .instances
            .iter()
            .map(|instance| instance.name.clone())
            .collect::<Vec<_>>();
        let value = settings_document(merged.entries.get(&replicated_path))
            .unwrap()
            .instances[1]
            .properties
            .get("Value")
            .cloned();
        (names, value, conflicts, changes)
    };
    let (_, _, conflicts, _) = merge(ConflictPreference::None);
    assert_eq!(
        conflicts
            .iter()
            .map(|conflict| conflict.detail.as_str())
            .collect::<Vec<_>>(),
        [
            "instances/ReplicatedStorage.renium: Pointer: Value refers to Workspace.Kit.Target (id=target), which Studio removed; deciding studio clears Value if Workspace.Kit.Target stays removed; deciding editor keeps Value, bringing Workspace.Kit.Target back if removed"
        ]
    );
    let (names, value, conflicts, changes) = merge(ConflictPreference::Editor);
    assert!(conflicts.is_empty(), "{conflicts:?}");
    assert_eq!(names, ["Workspace", "Kit", "Target"]);
    assert_eq!(value, Some(reference.clone()));
    assert!(changes.studio.contains(&workspace_path));
    assert!(!changes.editor.contains(&workspace_path));
    let (names, value, conflicts, changes) = merge(ConflictPreference::Studio);
    assert!(conflicts.is_empty(), "{conflicts:?}");
    assert_eq!(names, ["Workspace"]);
    assert_eq!(value, Some(json!({"_type": "Ref"})));
    assert!(changes.editor.contains(&replicated_path));
}

#[test]
fn a_decision_another_decision_made_moot_is_reported_with_that_decision() {
    let reference = json!({
        "_type": "Ref",
        "settingsId": "target",
        "pathSegments": ["Workspace", "Kit", "Target"],
        "pathOrdinals": [1, 1, 1],
    });
    let workspace = |target: Option<Value>| {
        let mut instances = vec![reference_merge_instance(
            "ws",
            "Workspace",
            "Workspace",
            None,
            None,
        )];
        if let Some(edited) = target {
            instances.push(reference_merge_instance(
                "kit",
                "Kit",
                "Folder",
                Some(0),
                None,
            ));
            let mut target = reference_merge_instance("target", "Target", "Folder", Some(1), None);
            target.attributes.insert("Edited".into(), edited);
            instances.push(target);
        }
        reference_merge_store(instances)
    };
    let replicated = |value: Option<&Value>| {
        reference_merge_store(vec![
            reference_merge_instance("rs", "ReplicatedStorage", "ReplicatedStorage", None, None),
            reference_merge_instance("pointer", "Pointer", "ObjectValue", Some(0), value),
        ])
    };
    let snapshot = |workspace, replicated| ProjectSnapshot {
        entries: BTreeMap::from([
            (PathBuf::from("instances/Workspace.renium"), workspace),
            (
                PathBuf::from("instances/ReplicatedStorage.renium"),
                replicated,
            ),
        ]),
    };
    // The files edit Target and point Pointer at it; Studio removes Kit.
    let baseline = snapshot(workspace(Some(json!(false))), replicated(None));
    let editor = snapshot(workspace(Some(json!(true))), replicated(Some(&reference)));
    let studio = snapshot(workspace(None), replicated(None));
    let merge = |decisions: ConflictDecisions| {
        merge_snapshots_with_changes(Some(&baseline), &editor, &studio, decisions, None).unwrap()
    };
    let (_, open, _) = merge(ConflictPreference::None.into());
    let id_of = |instance: &str| {
        open.iter()
            .find(|conflict| conflict.instance.as_ref().unwrap().id == instance)
            .unwrap()
            .id
            .clone()
    };
    let (removal, pointer) = (id_of("target"), id_of("pointer"));
    let (merged, open, changes) = merge(ConflictDecisions {
        preference: ConflictPreference::None,
        by_id: BTreeMap::from([
            (removal.clone(), ConflictSide::Editor),
            (pointer.clone(), ConflictSide::Studio),
        ]),
    });
    assert!(open.is_empty(), "{open:#?}");
    assert_eq!(
        changes.moot_decisions,
        [MootDecision {
            id: pointer,
            decision: ConflictSide::Studio,
            by: vec![removal],
        }]
    );
    let value = settings_document(
        merged
            .entries
            .get(Path::new("instances/ReplicatedStorage.renium")),
    )
    .unwrap()
    .instances[1]
        .properties
        .get("Value")
        .cloned();
    assert_eq!(value, Some(reference), "Target stays, so Value is kept");
}

fn reference_merge_instance(
    id: &str,
    name: &str,
    class_name: &str,
    parent: Option<usize>,
    value: Option<&Value>,
) -> SettingsBytecodeInstance {
    let mut instance = SettingsBytecodeInstance::new(
        id.to_string(),
        name.to_string(),
        class_name.to_string(),
        parent,
    );
    if let Some(value) = value {
        instance
            .properties
            .insert("Value".to_string(), value.clone());
    }
    instance
}

fn reference_merge_store(instances: Vec<SettingsBytecodeInstance>) -> SnapshotEntry {
    SnapshotEntry::File(
        encode_settings_bytecode(&SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances,
        })
        .unwrap(),
    )
}

/// A reference to ReplicatedStorage.Target, which is `2` in its store, as
/// some instance of Workspace is in the tests below: stores number their
/// instances alike.
fn outside_reference_to_id_2() -> Value {
    json!({
        "_type": "Ref",
        "settingsId": "2",
        "pathSegments": ["ReplicatedStorage", "Target"],
        "pathOrdinals": [1, 1],
    })
}

/// A Workspace store of `children`, by (id, name, class), with a Holder
/// that refers to ReplicatedStorage.Target when `holder` gives its id.
fn workspace_with_outside_holder(
    children: &[(&str, &str, &str)],
    holder: Option<&str>,
) -> SettingsBytecode {
    let instance = reference_merge_instance;
    let outside = outside_reference_to_id_2();
    let mut instances = vec![instance("ws", "Workspace", "Workspace", None, None)];
    instances.extend(
        children
            .iter()
            .map(|(id, name, class_name)| instance(id, name, class_name, Some(0), None)),
    );
    instances
        .extend(holder.map(|id| instance(id, "Holder", "ObjectValue", Some(0), Some(&outside))));
    SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances,
    }
}

fn held_reference(document: &SettingsBytecode) -> &Value {
    &document
        .instances
        .iter()
        .find(|instance| instance.name == "Holder")
        .unwrap()
        .properties["Value"]
}

#[test]
fn releasing_a_reused_id_keeps_the_ids_of_references_into_other_services() {
    // The files reuse the id of a removed folder for a part, which gets a
    // fresh one.
    let baseline = workspace_with_outside_holder(&[("2", "Old", "Folder")], None);
    let mut editor = workspace_with_outside_holder(&[("2", "New", "Part")], Some("holder"));
    release_reassigned_editor_ids(&baseline, &mut editor, &baseline, &HashSet::new());
    assert_ne!(editor.instances[1].settings_id, "2");
    assert_eq!(*held_reference(&editor), outside_reference_to_id_2());
}

#[test]
fn aligning_new_instances_keeps_the_ids_of_references_into_other_services() {
    // Studio numbers a folder `2` that the files added as `fresh`.
    let baseline = workspace_with_outside_holder(&[], None);
    let editor = workspace_with_outside_holder(&[("fresh", "A", "Folder")], None);
    let mut studio = workspace_with_outside_holder(&[("2", "A", "Folder")], Some("holder"));
    align_new_instance_ids(&baseline, &editor, &mut studio).unwrap();
    assert_eq!(studio.instances[1].settings_id, "fresh");
    assert_eq!(*held_reference(&studio), outside_reference_to_id_2());
}

#[test]
fn a_first_pairing_keeps_the_ids_of_references_into_other_services() {
    let mut editor = workspace_with_outside_holder(&[("fresh", "A", "Folder")], Some("holder"));
    let mut studio = workspace_with_outside_holder(&[("2", "A", "Folder")], Some("studio-holder"));
    let mut conflicts = ConflictLog::new(ConflictPreference::None);
    align_first_pairing(
        Path::new("instances/Workspace.renium"),
        &mut editor,
        &mut studio,
        &mut conflicts,
    );
    assert!(conflicts.open().is_empty(), "{:?}", conflicts.open());
    assert_eq!(studio.instances[1].settings_id, "fresh");
    assert_eq!(*held_reference(&studio), outside_reference_to_id_2());
}

#[test]
fn a_merge_settles_a_reference_whose_target_the_other_side_replaced_at_its_path() {
    let instance = reference_merge_instance;
    let reference = json!({
        "_type": "Ref",
        "settingsId": "target",
        "pathSegments": ["Workspace", "Kit", "Target"],
        "pathOrdinals": [1, 1, 1],
    });
    let workspace_path = PathBuf::from("instances/Workspace.renium");
    let replicated_path = PathBuf::from("instances/ReplicatedStorage.renium");
    let snapshot = |replaced: bool, value: Option<&Value>| {
        let target = if replaced {
            instance("replacement", "Target", "Part", Some(1), None)
        } else {
            instance("target", "Target", "Folder", Some(1), None)
        };
        ProjectSnapshot {
            entries: BTreeMap::from([
                (
                    workspace_path.clone(),
                    reference_merge_store(vec![
                        instance("ws", "Workspace", "Workspace", None, None),
                        instance("kit", "Kit", "Folder", Some(0), None),
                        target,
                    ]),
                ),
                (
                    replicated_path.clone(),
                    reference_merge_store(vec![
                        instance("rs", "ReplicatedStorage", "ReplicatedStorage", None, None),
                        instance("pointer", "Pointer", "ObjectValue", Some(0), value),
                    ]),
                ),
            ]),
        }
    };
    // The files point ReplicatedStorage.Pointer at Workspace.Kit.Target
    // while Studio removes that folder and puts a part at its path.
    let baseline = snapshot(false, None);
    let editor = snapshot(false, Some(&reference));
    let studio = snapshot(true, None);
    let merge = |preference| {
        let (merged, conflicts, _) =
            merge_snapshots_with_changes(Some(&baseline), &editor, &studio, preference, None)
                .unwrap();
        let targets = settings_document(merged.entries.get(&workspace_path))
            .unwrap()
            .instances
            .iter()
            .filter(|instance| instance.name == "Target")
            .map(|instance| instance.settings_id.clone())
            .collect::<BTreeSet<_>>();
        let value = settings_document(merged.entries.get(&replicated_path))
            .unwrap()
            .instances[1]
            .properties
            .get("Value")
            .cloned();
        (targets, value, conflicts)
    };
    let ids = |ids: &[&str]| ids.iter().map(|id| id.to_string()).collect::<BTreeSet<_>>();
    let (_, _, conflicts) = merge(ConflictPreference::None);
    assert_eq!(
        conflicts
            .iter()
            .map(|conflict| conflict.detail.as_str())
            .collect::<Vec<_>>(),
        [
            "instances/ReplicatedStorage.renium: Pointer: Value refers to Workspace.Kit.Target (id=target), which Studio removed; deciding studio clears Value if Workspace.Kit.Target stays removed; deciding editor keeps Value, bringing Workspace.Kit.Target back if removed"
        ]
    );
    let (targets, value, conflicts) = merge(ConflictPreference::Editor);
    assert!(conflicts.is_empty(), "{conflicts:?}");
    assert_eq!(targets, ids(&["replacement", "target"]));
    assert_eq!(value.unwrap()["settingsId"], "target");
    let (targets, value, conflicts) = merge(ConflictPreference::Studio);
    assert!(conflicts.is_empty(), "{conflicts:?}");
    assert_eq!(targets, ids(&["replacement"]));
    assert_eq!(value, Some(json!({"_type": "Ref"})));
}

#[test]
fn a_restored_reference_target_brings_back_what_it_refers_to() {
    let instance = reference_merge_instance;
    let reference = |service: &str, name: &str, id: &str| {
        json!({
            "_type": "Ref",
            "settingsId": id,
            "pathSegments": [service, name],
            "pathOrdinals": [1, 1],
        })
    };
    let workspace_path = PathBuf::from("src/Workspace/__roblox_sync_settings.renium");
    let replicated_path = PathBuf::from("src/ReplicatedStorage/__roblox_sync_settings.renium");
    let server_path = PathBuf::from("src/ServerStorage/__roblox_sync_settings.renium");
    let source_path = PathBuf::from("src/ServerStorage/C.luau");
    let source = b"return 'c'".as_slice();
    // Workspace.A refers to Workspace.B, B to the script ServerStorage.C.
    let snapshot = |with_targets: bool, value: Option<&Value>| {
        let mut workspace = vec![instance("ws", "Workspace", "Workspace", None, None)];
        let mut server = vec![instance("ss", "ServerStorage", "ServerStorage", None, None)];
        let mut entries = BTreeMap::new();
        if with_targets {
            workspace.push(instance(
                "a",
                "A",
                "ObjectValue",
                Some(0),
                Some(&reference("Workspace", "B", "b")),
            ));
            workspace.push(instance(
                "b",
                "B",
                "ObjectValue",
                Some(0),
                Some(&reference("ServerStorage", "C", "c")),
            ));
            server.push(instance("c", "C", "ModuleScript", Some(0), None));
            entries.insert(source_path.clone(), SnapshotEntry::File(source.to_vec()));
        }
        entries.insert(workspace_path.clone(), reference_merge_store(workspace));
        entries.insert(server_path.clone(), reference_merge_store(server));
        entries.insert(
            replicated_path.clone(),
            reference_merge_store(vec![
                instance("rs", "ReplicatedStorage", "ReplicatedStorage", None, None),
                instance("pointer", "Pointer", "ObjectValue", Some(0), value),
            ]),
        );
        ProjectSnapshot { entries }
    };
    // The files point ReplicatedStorage.Pointer at A while Studio removes
    // A, B and C.
    let baseline = snapshot(true, None);
    let editor = snapshot(true, Some(&reference("Workspace", "A", "a")));
    let studio = snapshot(false, None);
    let (merged, conflicts, changes) = merge_snapshots_with_changes(
        Some(&baseline),
        &editor,
        &studio,
        ConflictPreference::Editor,
        None,
    )
    .unwrap();
    assert!(conflicts.is_empty(), "{conflicts:?}");
    let names = |path: &PathBuf| {
        settings_document(merged.entries.get(path))
            .unwrap()
            .instances
            .into_iter()
            .map(|instance| instance.name)
            .collect::<Vec<_>>()
    };
    assert_eq!(names(&workspace_path), ["Workspace", "A", "B"]);
    assert_eq!(names(&server_path), ["ServerStorage", "C"]);
    assert!(merged.entries.get(&source_path) == editor.entries.get(&source_path));
    for path in [&workspace_path, &server_path, &source_path] {
        assert!(changes.studio.contains(path), "{}", path.display());
        assert!(!changes.editor.contains(path), "{}", path.display());
    }
}

#[test]
fn a_scope_removed_from_publication_is_not_republished() {
    let baseline = file_snapshot(&[
        ("old/Workspace/Removed.luau", b"return 1"),
        ("new/Workspace/Kept.luau", b"return 2"),
    ]);
    let editor = file_snapshot(&[("new/Workspace/Kept.luau", b"return 3")]);
    let studio = file_snapshot(&[("new/Workspace/Kept.luau", b"return 2")]);
    let differences = snapshot_differences(&editor, &studio).unwrap();
    let (merged, conflicts, changes) = merge_snapshots_with_changes(
        Some(&baseline),
        &editor,
        &studio,
        ConflictPreference::None,
        Some(&differences),
    )
    .unwrap();
    assert!(conflicts.is_empty());
    let old = PathBuf::from("old/Workspace/Removed.luau");
    assert!(!merged.entries.contains_key(&old));
    assert!(!changes.editor.contains(&old));
    assert!(!changes.studio.contains(&old));
    let root = crate::tests::support::temp_dir("removed-scopes");
    let stored = StoredSnapshot::write(&root, "pair", &baseline).unwrap();
    let selected = stored
        .load_scopes(&root, "pair", &[PathBuf::from("new")])
        .unwrap();
    assert!(!selected.entries.contains_key(&old));
    assert_eq!(selected.entries.len(), 1);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn a_package_link_both_sides_already_lack_is_not_a_removal_conflict() {
    let link = |id: &str| SettingsBytecodeInstance {
        settings_id: id.to_string(),
        name: "PackageLink".to_string(),
        class_name: "PackageLink".to_string(),
        parent_index: Some(1),
        properties: Map::new(),
        attributes: Map::new(),
    };
    let root = SettingsBytecodeInstance {
        settings_id: "ws".to_string(),
        name: "Workspace".to_string(),
        class_name: "Workspace".to_string(),
        parent_index: None,
        properties: Map::new(),
        attributes: Map::new(),
    };
    let car = SettingsBytecodeInstance {
        settings_id: "car".to_string(),
        name: "Bavria".to_string(),
        class_name: "Model".to_string(),
        parent_index: Some(0),
        properties: Map::new(),
        attributes: Map::new(),
    };
    let base = SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances: vec![root.clone(), car.clone(), link("pl")],
    };
    let unlinked = SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances: vec![root.clone(), car.clone()],
    };
    let path = Path::new("instances/Workspace.renium");
    let mut conflicts = ConflictLog::new(ConflictPreference::None);
    pairing::protect_package_links(
        path,
        Some(&base),
        &mut unlinked.clone(),
        &mut unlinked.clone(),
        &mut conflicts,
    );
    assert!(conflicts.open().is_empty(), "{:?}", conflicts.open());
    let mut conflicts = ConflictLog::new(ConflictPreference::Studio);
    pairing::protect_package_links(
        path,
        Some(&base),
        &mut base.clone(),
        &mut unlinked.clone(),
        &mut conflicts,
    );
    let open = conflicts.open();
    assert_eq!(open.len(), 1, "{open:?}");
    assert!(open[0].detail.contains("removes PackageLink"));
    assert_eq!(open[0].kind, ConflictKind::Package);
    assert!(
        !open[0].decidable,
        "no preference settles a PackageLink edit"
    );
}

#[test]
fn target_ownership_ends_with_the_live_pair() {
    let coordinator = Coordinator::default();
    let first = PairIdentity {
        experience: "experience".to_string(),
        project: "first".to_string(),
        fingerprint: "first-fingerprint".to_string(),
        game_id: Some(1),
        place_id: Some(2),
        local_file: None,
    };
    let second = PairIdentity {
        project: "second".to_string(),
        fingerprint: "second-fingerprint".to_string(),
        ..first.clone()
    };

    assert_eq!(coordinator.claim_target(&first), None);
    assert_eq!(coordinator.claim_target(&second), Some("first".to_string()));

    coordinator.release_target(&first.pair_key());

    assert_eq!(coordinator.claim_target(&second), None);
}

#[test]
fn first_pairing_unions_separate_files_and_blocks_same_file_conflicts() {
    let editor = file_snapshot(&[("src/A.luau", b"return 'editor'")]);
    let studio = file_snapshot(&[("src/B.luau", b"return 'studio'")]);
    let (merged, conflicts) =
        merge_snapshots(None, &editor, &studio, ConflictPreference::None).unwrap();
    assert!(conflicts.is_empty());
    assert_eq!(merged.entries.len(), 2);

    let studio = file_snapshot(&[("src/A.luau", b"return 'studio'")]);
    let (_, conflicts) = merge_snapshots(None, &editor, &studio, ConflictPreference::None).unwrap();
    assert_eq!(conflicts.len(), 1);

    let editor = file_snapshot(&[("src/A.luau", b"local a = 1\r\nreturn a\r\n")]);
    let studio = file_snapshot(&[("src/A.luau", b"local a = 1\nreturn a\n")]);
    let (merged, conflicts) =
        merge_snapshots(None, &editor, &studio, ConflictPreference::None).unwrap();
    assert!(conflicts.is_empty());
    assert!(merged == editor);
    assert!(snapshots_equivalent(&editor, &studio).unwrap());

    let service_root = |id: &str| SettingsBytecodeInstance {
        settings_id: id.to_string(),
        name: "ReplicatedStorage".to_string(),
        class_name: "ReplicatedStorage".to_string(),
        parent_index: None,
        properties: Map::new(),
        attributes: Map::new(),
    };
    let child = |id: &str, name: &str| SettingsBytecodeInstance {
        settings_id: id.to_string(),
        name: name.to_string(),
        class_name: "Folder".to_string(),
        parent_index: Some(0),
        properties: Map::new(),
        attributes: Map::new(),
    };
    let settings_snapshot = |document: SettingsBytecode| ProjectSnapshot {
        entries: [(
            PathBuf::from("src/ReplicatedStorage/__roblox_sync_settings.renium"),
            SnapshotEntry::File(encode_settings_bytecode(&document).unwrap()),
        )]
        .into_iter()
        .collect(),
    };
    let editor = settings_snapshot(SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances: vec![
            service_root("editor-root"),
            child("editor-child", "FromEditor"),
        ],
    });
    let studio = settings_snapshot(SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances: vec![
            service_root("studio-root"),
            child("studio-child", "FromStudio"),
        ],
    });
    let (merged, conflicts) =
        merge_snapshots(None, &editor, &studio, ConflictPreference::None).unwrap();
    assert!(conflicts.is_empty());
    let merged = settings_document(merged.entries.get(Path::new(
        "src/ReplicatedStorage/__roblox_sync_settings.renium",
    )))
    .unwrap();
    assert_eq!(merged.instances.len(), 3);

    let duplicate = |id: &str, value: &str| SettingsBytecodeInstance {
        settings_id: id.to_string(),
        name: "Duplicate".to_string(),
        class_name: "StringValue".to_string(),
        parent_index: Some(0),
        properties: Map::from_iter([("Value".to_string(), Value::String(value.to_string()))]),
        attributes: Map::new(),
    };
    let editor = settings_snapshot(SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances: vec![
            service_root("editor-root"),
            duplicate("editor-a", "A"),
            duplicate("editor-b", "B"),
        ],
    });
    let studio = settings_snapshot(SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances: vec![
            service_root("studio-root"),
            duplicate("studio-a", "B"),
            duplicate("studio-b", "A"),
        ],
    });
    let (merged, conflicts) =
        merge_snapshots(None, &editor, &studio, ConflictPreference::Editor).unwrap();
    assert!(conflicts.is_empty(), "{conflicts:?}");
    assert_eq!(
        settings_document(merged.entries.values().next())
            .unwrap()
            .instances
            .len(),
        3
    );

    // A hull compares by the fidelity it encodes in the first comparison;
    // the merge itself keeps whichever bytes Studio has.
    let hull = |bytes: &[u8]| json!({"_type": "SharedString", "base64": base64::encode(bytes)});
    let meshed = |id: &str, hull_value: Value| SettingsBytecodeInstance {
        settings_id: id.to_string(),
        name: "Mesh".to_string(),
        class_name: "MeshPart".to_string(),
        parent_index: Some(0),
        properties: Map::from_iter([("PhysicalConfigData".to_string(), hull_value)]),
        attributes: Map::new(),
    };
    let same_fidelity = SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances: vec![
            service_root("root"),
            meshed("files", hull(b"CSGPHS\x08\0\0\0\x01\0first cook")),
            meshed("files", hull(b"CSGPHS\x08\0\0\0\x01\0second cook")),
            meshed("files", hull(b"CSGPHS\x08\0\0\0\x02\0box")),
            meshed("files", hull(b"CSGPHS\0\0\0\0BLOCK")),
            meshed("files", hull(b"opaque legacy hull a")),
            meshed("files", hull(b"opaque legacy hull b")),
        ],
    };
    assert!(settings_instances_equal(
        &same_fidelity,
        1,
        &same_fidelity,
        2
    ));
    assert!(!settings_instances_equal(
        &same_fidelity,
        1,
        &same_fidelity,
        3
    ));
    assert!(settings_instances_equal(
        &same_fidelity,
        3,
        &same_fidelity,
        4
    ));
    assert!(!settings_instances_equal(
        &same_fidelity,
        5,
        &same_fidelity,
        6
    ));
    assert!(settings_instances_equal(
        &same_fidelity,
        5,
        &same_fidelity,
        5
    ));

    let editor = settings_snapshot(SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances: vec![service_root("editor-root"), child("editor-child", "Same")],
    });
    let mut studio_child = child("studio-child", "Same");
    studio_child.class_name = "Model".to_string();
    let studio = settings_snapshot(SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances: vec![service_root("studio-root"), studio_child],
    });
    let (_, conflicts) =
        merge_snapshots(None, &editor, &studio, ConflictPreference::Editor).unwrap();
    assert!(
        conflicts
            .iter()
            .any(|conflict| conflict.detail.contains("different classes"))
    );
}

#[test]
fn live_sync_readback_retains_nonfinite_cframes_and_describes_real_differences() {
    let path = PathBuf::from("instances/ReplicatedStorage.renium");
    let mut root = SettingsBytecodeInstance::new(
        "root".into(),
        "ReplicatedStorage".into(),
        "ReplicatedStorage".into(),
        None,
    );
    root.attributes.insert("Revision".into(), json!(1));
    let mut model = SettingsBytecodeInstance::new(
        "car".into(),
        "Cat Mobile 5000".into(),
        "Part".into(),
        Some(0),
    );
    model.properties.insert(
        "CFrame".into(),
        json!({"_type":"CFrame",
        "components": vec![json!({"_type":"Float","value":"nan"});12]}),
    );
    let document = SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances: vec![root, model],
    };
    let snapshot = |document: &SettingsBytecode| ProjectSnapshot {
        entries: [(
            path.clone(),
            SnapshotEntry::File(encode_settings_bytecode(document).unwrap()),
        )]
        .into_iter()
        .collect(),
    };
    let expected = snapshot(&document);
    let mut reordered = document.clone();
    reordered.instances[1].settings_id = "fresh-runtime".into();
    let observed = snapshot(&reordered);
    assert!(
        snapshot_differences(&observed, &expected)
            .unwrap()
            .is_empty()
    );
    assert!(
        snapshot_mismatch_details(&observed, &expected, std::slice::from_ref(&path))
            .unwrap()
            .is_none()
    );

    reordered.instances[1].properties["CFrame"]["components"][0] = json!(42);
    let observed = snapshot(&reordered);
    assert_eq!(
        snapshot_differences(&observed, &expected).unwrap(),
        HashSet::from([path.clone()])
    );
    let detail = snapshot_mismatch_details(&observed, &expected, &[path])
        .unwrap()
        .unwrap();
    assert!(
        detail.contains("Cat Mobile 5000.CFrame property"),
        "{detail}"
    );
    assert!(detail.contains("42") && detail.contains("nan"), "{detail}");
    assert!(detail.contains("CFrame(42.0, nan"), "{detail}");
    assert!(!detail.contains("is CFrame; expected CFrame"), "{detail}");
}

#[test]
fn three_way_merge_keeps_independent_changes() {
    let baseline = file_snapshot(&[
        ("src/A.luau", b"return 'base-a'"),
        ("src/B.luau", b"return 'base-b'"),
    ]);
    let editor = file_snapshot(&[
        ("src/A.luau", b"return 'editor'"),
        ("src/B.luau", b"return 'base-b'"),
    ]);
    let studio = file_snapshot(&[
        ("src/A.luau", b"return 'base-a'"),
        ("src/B.luau", b"return 'studio'"),
    ]);
    let (merged, conflicts) =
        merge_snapshots(Some(&baseline), &editor, &studio, ConflictPreference::None).unwrap();
    assert!(conflicts.is_empty());
    assert!(
        merged.entries.get(Path::new("src/A.luau")) == editor.entries.get(Path::new("src/A.luau"))
    );
    assert!(
        merged.entries.get(Path::new("src/B.luau")) == studio.entries.get(Path::new("src/B.luau"))
    );

    let shared = file_snapshot(&[("src/A.luau", b"return 'shared'")]);
    let reverted = file_snapshot(&[("src/A.luau", b"return 'base-a'")]);
    let changed = file_snapshot(&[("src/A.luau", b"return 'studio-again'")]);
    let (_, stale_conflicts) = merge_snapshots(
        Some(&baseline),
        &reverted,
        &changed,
        ConflictPreference::None,
    )
    .unwrap();
    assert!(stale_conflicts.is_empty());
    let (_, current_conflicts) =
        merge_snapshots(Some(&shared), &reverted, &changed, ConflictPreference::None).unwrap();
    assert_eq!(current_conflicts.len(), 1);

    let deleted = ProjectSnapshot::default();
    let (merged, structural_conflicts) = merge_snapshots(
        Some(&shared),
        &deleted,
        &changed,
        ConflictPreference::Editor,
    )
    .unwrap();
    assert!(structural_conflicts.is_empty());
    assert!(!merged.entries.contains_key(Path::new("src/A.luau")));
    let (merged, structural_conflicts) = merge_snapshots(
        Some(&shared),
        &deleted,
        &changed,
        ConflictPreference::Studio,
    )
    .unwrap();
    assert!(structural_conflicts.is_empty());
    assert!(
        merged.entries.get(Path::new("src/A.luau")) == changed.entries.get(Path::new("src/A.luau"))
    );

    let parent = PathBuf::from("src/Folder");
    let baseline = ProjectSnapshot {
        entries: [(parent.clone(), SnapshotEntry::Directory)]
            .into_iter()
            .collect(),
    };
    let studio = ProjectSnapshot {
        entries: [
            (parent, SnapshotEntry::Directory),
            (
                PathBuf::from("src/Folder/New.luau"),
                SnapshotEntry::File(b"return true".to_vec()),
            ),
        ]
        .into_iter()
        .collect(),
    };
    let (merged, structural_conflicts) = merge_snapshots(
        Some(&baseline),
        &ProjectSnapshot::default(),
        &studio,
        ConflictPreference::Editor,
    )
    .unwrap();
    assert!(structural_conflicts.is_empty());
    assert!(
        merged.entries.get(Path::new("src/Folder/New.luau"))
            == studio.entries.get(Path::new("src/Folder/New.luau"))
    );

    let instance = |id: &str, name: &str, parent_index: Option<usize>| SettingsBytecodeInstance {
        settings_id: id.to_string(),
        name: name.to_string(),
        class_name: "Folder".to_string(),
        parent_index,
        properties: Map::new(),
        attributes: Map::new(),
    };
    let settings_snapshot = |instances| ProjectSnapshot {
        entries: [(
            PathBuf::from("src/ReplicatedStorage/__roblox_sync_settings.renium"),
            SnapshotEntry::File(
                encode_settings_bytecode(&SettingsBytecode {
                    version: SETTINGS_BINARY_VERSION,
                    instances,
                })
                .unwrap(),
            ),
        )]
        .into_iter()
        .collect(),
    };
    let baseline = settings_snapshot(vec![
        instance("root", "ReplicatedStorage", None),
        instance("a", "A", Some(0)),
        instance("b", "B", Some(0)),
    ]);
    let editor = settings_snapshot(vec![
        instance("root", "ReplicatedStorage", None),
        instance("a", "A", Some(0)),
        instance("b", "B", Some(1)),
    ]);
    let studio = settings_snapshot(vec![
        instance("root", "ReplicatedStorage", None),
        instance("b", "B", Some(0)),
        instance("a", "A", Some(1)),
    ]);
    let (merged, cycle_conflicts) =
        merge_snapshots(Some(&baseline), &editor, &studio, ConflictPreference::None).unwrap();
    assert!(!cycle_conflicts.is_empty());
    assert_eq!(
        settings_document(merged.entries.values().next())
            .unwrap()
            .instances
            .len(),
        3
    );
    for (preference, expected_a, expected_b) in [
        (ConflictPreference::Studio, Some("b"), Some("root")),
        (ConflictPreference::Editor, Some("root"), Some("a")),
    ] {
        let (merged, conflicts) =
            merge_snapshots(Some(&baseline), &editor, &studio, preference).unwrap();
        assert!(conflicts.is_empty());
        let document = settings_document(merged.entries.values().next()).unwrap();
        let parents = document
            .instances
            .iter()
            .map(|instance| {
                (
                    instance.settings_id.as_str(),
                    instance
                        .parent_index
                        .map(|parent| document.instances[parent].settings_id.as_str()),
                )
            })
            .collect::<HashMap<_, _>>();
        assert_eq!(parents["a"], expected_a);
        assert_eq!(parents["b"], expected_b);
    }

    let script_snapshot = |name: &str, source: &[u8]| ProjectSnapshot {
        entries: [
            (
                PathBuf::from("src/ReplicatedStorage/__roblox_sync_settings.renium"),
                SnapshotEntry::File(
                    encode_settings_bytecode(&SettingsBytecode {
                        version: SETTINGS_BINARY_VERSION,
                        instances: vec![
                            instance("root", "ReplicatedStorage", None),
                            SettingsBytecodeInstance {
                                settings_id: "script".to_string(),
                                name: name.to_string(),
                                class_name: "ModuleScript".to_string(),
                                parent_index: Some(0),
                                properties: Map::new(),
                                attributes: Map::new(),
                            },
                        ],
                    })
                    .unwrap(),
                ),
            ),
            (
                PathBuf::from(format!("src/ReplicatedStorage/{name}.luau")),
                SnapshotEntry::File(source.to_vec()),
            ),
        ]
        .into_iter()
        .collect(),
    };
    let baseline = script_snapshot("Original", b"return 'base'");
    let editor = script_snapshot("EditorName", b"return 'editor'");
    let studio = script_snapshot("StudioName", b"return 'studio'");
    let (merged, conflicts) = merge_snapshots(
        Some(&baseline),
        &editor,
        &studio,
        ConflictPreference::Editor,
    )
    .unwrap();
    assert!(conflicts.is_empty());
    assert!(
        merged
            .entries
            .contains_key(Path::new("src/ReplicatedStorage/EditorName.luau"))
    );
    assert!(
        !merged
            .entries
            .contains_key(Path::new("src/ReplicatedStorage/StudioName.luau"))
    );
    let (merged, conflicts) = merge_snapshots(
        Some(&baseline),
        &editor,
        &studio,
        ConflictPreference::Studio,
    )
    .unwrap();
    assert!(conflicts.is_empty());
    assert!(
        merged
            .entries
            .contains_key(Path::new("src/ReplicatedStorage/StudioName.luau"))
    );
    assert!(
        !merged
            .entries
            .contains_key(Path::new("src/ReplicatedStorage/EditorName.luau"))
    );
}

#[test]
fn source_edit_conflicts_with_script_deletion_before_source_enters_baseline() {
    let settings_path = PathBuf::from("src/ReplicatedStorage/__roblox_sync_settings.renium");
    let source_path = PathBuf::from("src/ReplicatedStorage/Logic.luau");
    let settings = |include_script| {
        let mut instances = vec![SettingsBytecodeInstance {
            settings_id: "root".to_string(),
            name: "ReplicatedStorage".to_string(),
            class_name: "ReplicatedStorage".to_string(),
            parent_index: None,
            properties: Map::new(),
            attributes: Map::new(),
        }];
        if include_script {
            instances.push(SettingsBytecodeInstance {
                settings_id: "script".to_string(),
                name: "Logic".to_string(),
                class_name: "ModuleScript".to_string(),
                parent_index: Some(0),
                properties: Map::new(),
                attributes: Map::new(),
            });
        }
        SnapshotEntry::File(
            encode_settings_bytecode(&SettingsBytecode {
                version: SETTINGS_BINARY_VERSION,
                instances,
            })
            .unwrap(),
        )
    };
    let baseline = ProjectSnapshot {
        entries: [(settings_path.clone(), settings(true))]
            .into_iter()
            .collect(),
    };
    let editor = ProjectSnapshot {
        entries: [
            (settings_path.clone(), settings(true)),
            (
                source_path.clone(),
                SnapshotEntry::File(b"return 'edited before baseline'".to_vec()),
            ),
        ]
        .into_iter()
        .collect(),
    };
    let studio = ProjectSnapshot {
        entries: [(settings_path.clone(), settings(false))]
            .into_iter()
            .collect(),
    };

    let (_, conflicts) =
        merge_snapshots(Some(&baseline), &editor, &studio, ConflictPreference::None).unwrap();
    assert_eq!(conflicts.len(), 1);
    assert!(
        conflicts[0]
            .detail
            .contains("the files changed it and Studio removed it"),
        "{conflicts:?}"
    );

    let (merged, conflicts) = merge_snapshots(
        Some(&baseline),
        &editor,
        &studio,
        ConflictPreference::Editor,
    )
    .unwrap();
    assert!(conflicts.is_empty());
    assert!(merged.entries.contains_key(&source_path));
    assert_eq!(
        settings_document(merged.entries.get(&settings_path))
            .unwrap()
            .instances
            .len(),
        2
    );

    let (merged, conflicts) = merge_snapshots(
        Some(&baseline),
        &editor,
        &studio,
        ConflictPreference::Studio,
    )
    .unwrap();
    assert!(conflicts.is_empty());
    assert!(!merged.entries.contains_key(&source_path));
    assert_eq!(
        settings_document(merged.entries.get(&settings_path))
            .unwrap()
            .instances
            .len(),
        1
    );

    let (_, conflicts) =
        merge_snapshots(Some(&baseline), &studio, &editor, ConflictPreference::None).unwrap();
    assert_eq!(conflicts.len(), 1);
    assert!(
        conflicts[0]
            .detail
            .contains("Studio changed it and the files removed it"),
        "{conflicts:?}"
    );

    let (merged, conflicts) = merge_snapshots(
        Some(&baseline),
        &studio,
        &editor,
        ConflictPreference::Studio,
    )
    .unwrap();
    assert!(conflicts.is_empty());
    assert!(merged.entries.contains_key(&source_path));

    let (merged, conflicts) = merge_snapshots(
        Some(&baseline),
        &studio,
        &editor,
        ConflictPreference::Editor,
    )
    .unwrap();
    assert!(conflicts.is_empty());
    assert!(!merged.entries.contains_key(&source_path));
}

#[test]
fn a_script_the_merge_brings_back_keeps_its_source_file() {
    let settings_path = PathBuf::from("src/ReplicatedStorage/__roblox_sync_settings.renium");
    let source_path = PathBuf::from("src/ReplicatedStorage/Logic.luau");
    let instance = |id: &str, name: &str, class_name: &str, parent_index: Option<usize>| {
        SettingsBytecodeInstance {
            settings_id: id.to_string(),
            name: name.to_string(),
            class_name: class_name.to_string(),
            parent_index,
            properties: Map::new(),
            attributes: Map::new(),
        }
    };
    let root = || instance("root", "ReplicatedStorage", "ReplicatedStorage", None);
    let logic = || instance("script", "Logic", "ModuleScript", Some(0));
    let snapshot = |instances: Vec<SettingsBytecodeInstance>, source: Option<&[u8]>| {
        let mut snapshot = ProjectSnapshot::default();
        snapshot.entries.insert(
            settings_path.clone(),
            SnapshotEntry::File(
                encode_settings_bytecode(&SettingsBytecode {
                    version: SETTINGS_BINARY_VERSION,
                    instances,
                })
                .unwrap(),
            ),
        );
        if let Some(source) = source {
            snapshot
                .entries
                .insert(source_path.clone(), SnapshotEntry::File(source.to_vec()));
        }
        snapshot
    };
    let names = |merged: &ProjectSnapshot| {
        settings_document(merged.entries.get(&settings_path))
            .unwrap()
            .instances
            .into_iter()
            .map(|instance| instance.name)
            .collect::<Vec<_>>()
    };
    let source = b"return 'base'".as_slice();
    let baseline = snapshot(
        vec![
            root(),
            logic(),
            instance("ptr", "Pointer", "ObjectValue", Some(0)),
        ],
        Some(source),
    );
    let removed = snapshot(
        vec![root(), instance("ptr", "Pointer", "ObjectValue", Some(0))],
        None,
    );
    let mut pointer = instance("ptr", "Pointer", "ObjectValue", Some(0));
    pointer.properties.insert(
        "Value".to_string(),
        json!({"_type": "Ref", "settingsId": "script"}),
    );
    let referred = snapshot(vec![root(), logic(), pointer], Some(source));

    // The files removed the script, Studio points at it and wins: the script
    // comes back with the source Studio has, which the files now lack.
    let (merged, conflicts, changes) = merge_snapshots_with_changes(
        Some(&baseline),
        &removed,
        &referred,
        ConflictPreference::Studio,
        None,
    )
    .unwrap();
    assert!(conflicts.is_empty(), "{conflicts:?}");
    assert_eq!(names(&merged), ["ReplicatedStorage", "Pointer", "Logic"]);
    assert!(merged.entries.get(&source_path) == referred.entries.get(&source_path));
    assert!(changes.editor.contains(&source_path));
    assert!(!changes.studio.contains(&source_path));

    // The same with the sides swapped.
    let (merged, conflicts, changes) = merge_snapshots_with_changes(
        Some(&baseline),
        &referred,
        &removed,
        ConflictPreference::Editor,
        None,
    )
    .unwrap();
    assert!(conflicts.is_empty(), "{conflicts:?}");
    assert_eq!(names(&merged), ["ReplicatedStorage", "Pointer", "Logic"]);
    assert!(merged.entries.get(&source_path) == referred.entries.get(&source_path));
    assert!(changes.studio.contains(&source_path));
    assert!(!changes.editor.contains(&source_path));

    // The removal wins: the script and its source stay removed.
    let (merged, conflicts) = merge_snapshots(
        Some(&baseline),
        &removed,
        &referred,
        ConflictPreference::Editor,
    )
    .unwrap();
    assert!(conflicts.is_empty(), "{conflicts:?}");
    assert_eq!(names(&merged), ["ReplicatedStorage", "Pointer"]);
    assert!(!merged.entries.contains_key(&source_path));

    // A script kept for an attribute Studio gave it keeps its source too.
    let mut marked = logic();
    marked.attributes.insert("Mark".to_string(), json!(1));
    let changed = snapshot(
        vec![
            root(),
            marked,
            instance("ptr", "Pointer", "ObjectValue", Some(0)),
        ],
        Some(source),
    );
    let (merged, conflicts, changes) = merge_snapshots_with_changes(
        Some(&baseline),
        &removed,
        &changed,
        ConflictPreference::Studio,
        None,
    )
    .unwrap();
    assert!(conflicts.is_empty(), "{conflicts:?}");
    assert_eq!(names(&merged), ["ReplicatedStorage", "Logic", "Pointer"]);
    assert!(merged.entries.get(&source_path) == changed.entries.get(&source_path));
    assert!(changes.editor.contains(&source_path));

    // A script that comes back as the parent of an added instance keeps its
    // source as well.
    let folder_path = PathBuf::from("src/ReplicatedStorage/Logic");
    let init_path = folder_path.join("init.luau");
    let nested = |mut snapshot: ProjectSnapshot| {
        if let Some(source) = snapshot.entries.remove(&source_path) {
            snapshot
                .entries
                .insert(folder_path.clone(), SnapshotEntry::Directory);
            snapshot.entries.insert(init_path.clone(), source);
        }
        snapshot
    };
    let old = || instance("old", "Old", "Folder", Some(1));
    let baseline = nested(snapshot(vec![root(), logic(), old()], Some(source)));
    let removed = snapshot(vec![root()], None);
    let added = nested(snapshot(
        vec![
            root(),
            logic(),
            old(),
            instance("new", "New", "Folder", Some(1)),
        ],
        Some(source),
    ));
    let (merged, conflicts, changes) = merge_snapshots_with_changes(
        Some(&baseline),
        &removed,
        &added,
        ConflictPreference::Studio,
        None,
    )
    .unwrap();
    assert!(conflicts.is_empty(), "{conflicts:?}");
    assert_eq!(names(&merged), ["ReplicatedStorage", "Logic", "New"]);
    assert!(merged.entries.get(&init_path) == added.entries.get(&init_path));
    assert!(merged.entries.get(&folder_path) == Some(&SnapshotEntry::Directory));
    assert!(changes.editor.contains(&init_path));
    assert!(changes.editor.contains(&folder_path));
    assert!(!changes.studio.contains(&init_path));
}

fn new_branch_fixture() -> (SettingsBytecode, SettingsBytecode, SettingsBytecode) {
    let instance = |id: &str, name: &str, class: &str, parent| {
        SettingsBytecodeInstance::new(id.to_string(), name.to_string(), class.to_string(), parent)
    };
    let base = SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances: vec![instance(
            "root",
            "ReplicatedStorage",
            "ReplicatedStorage",
            None,
        )],
    };
    let mut editor = base.clone();
    editor.instances.extend([
        instance("button", "ShiftLockButton", "TextButton", Some(0)),
        instance("corner", "UICorner", "UICorner", Some(1)),
        instance("ref", "Reference", "ObjectValue", Some(1)),
    ]);
    for name in [
        "TopLeftRadius",
        "TopRightRadius",
        "BottomLeftRadius",
        "BottomRightRadius",
    ] {
        editor.instances[2].properties.insert(
            name.to_string(),
            json!({"_type":"UDim","scale":1.0,"offset":0.0}),
        );
    }
    editor.instances[3].properties.insert(
        "Value".to_string(),
        json!({"_type":"Ref","settingsId":"corner"}),
    );
    let mut studio = editor.clone();
    // New instance IDs can collide across observations, including cycles and
    // an unrelated addition occupying the ID wanted by a matching node.
    studio.instances[1].settings_id = "corner".to_string();
    studio.instances[2].settings_id = "button".to_string();
    studio.instances[3].settings_id = "studio-ref".to_string();
    studio.instances[3].properties.insert(
        "Value".to_string(),
        json!({"_type":"Ref","settingsId":"button"}),
    );
    studio.instances[1]
        .properties
        .insert("Sink".to_string(), json!({"_type":"EnumItem","name":"1"}));
    studio
        .instances
        .push(instance("ref", "StudioOnly", "Folder", Some(0)));
    (base, editor, studio)
}

#[test]
fn reconciliation_matches_new_branches_without_duplicating_property_differences() {
    let path = Path::new("src/ReplicatedStorage/__roblox_sync_settings.renium");
    for iteration in 0..32 {
        let (base, mut editor, mut studio) = new_branch_fixture();
        // Vary traversal indices independently of structural path IDs.
        for i in 0..iteration {
            studio.instances.push(SettingsBytecodeInstance::new(
                format!("unrelated-{i}"),
                format!("Unrelated{i}"),
                "Folder".to_string(),
                Some(0),
            ));
        }
        if iteration % 2 == 0 {
            std::mem::swap(&mut editor, &mut studio);
        }
        let snapshot = |doc: &SettingsBytecode| {
            file_snapshot(&[(
                path.to_str().unwrap(),
                &encode_settings_bytecode(doc).unwrap(),
            )])
        };
        let (merged, conflicts) = merge_snapshots(
            Some(&snapshot(&base)),
            &snapshot(&editor),
            &snapshot(&studio),
            ConflictPreference::None,
        )
        .unwrap();
        assert!(conflicts.is_empty(), "{conflicts:?}");
        let merged = settings_document(merged.entries.get(path)).unwrap();
        for name in ["ShiftLockButton", "UICorner", "Reference", "StudioOnly"] {
            assert_eq!(
                merged.instances.iter().filter(|i| i.name == name).count(),
                1,
                "{name}, iteration {iteration}"
            );
        }
        let corner = merged
            .instances
            .iter()
            .find(|i| i.name == "UICorner")
            .unwrap();
        assert_eq!(
            corner.properties,
            new_branch_fixture().1.instances[2].properties
        );
        let holder = merged
            .instances
            .iter()
            .find(|i| i.name == "Reference")
            .unwrap();
        assert_eq!(holder.properties["Value"]["settingsId"], corner.settings_id);
        assert_eq!(
            merged
                .instances
                .iter()
                .map(|i| &i.settings_id)
                .collect::<HashSet<_>>()
                .len(),
            merged.instances.len()
        );
        let mut plan = ReconcilePushPlan::default();
        append_settings_push_plan(path, &merged, &studio, &mut plan).unwrap();
        assert!(
            !plan.recreated_settings_ids.contains(&corner.settings_id),
            "untouched corner scheduled for recreation"
        );
        assert!(
            plan.property_removals
                .iter()
                .all(|change| change.settings_id.as_deref() != Some(&corner.settings_id)),
            "unchanged radii scheduled for reset"
        );
    }
}

#[test]
fn matched_new_property_conflicts_do_not_become_copies() {
    let (base, mut editor, mut studio) = new_branch_fixture();
    editor.instances[1]
        .properties
        .insert("Text".to_string(), json!("editor"));
    studio.instances[1]
        .properties
        .insert("Text".to_string(), json!("studio"));
    align_new_instance_ids(&base, &editor, &mut studio).unwrap();
    let mut conflicts = ConflictLog::new(ConflictPreference::None);
    let merged = merge_reconciliation_settings_documents(
        Path::new("instances/StarterGui.renium"),
        [&base, &editor, &studio],
        &mut conflicts,
        &HashSet::new(),
        &HashSet::new(),
    );
    assert_eq!(
        merged
            .instances
            .iter()
            .filter(|i| i.name == "ShiftLockButton")
            .count(),
        1
    );
    assert!(
        conflicts
            .open()
            .iter()
            .any(|c| c.field.as_deref() == Some("Text"))
    );
}

#[test]
fn new_duplicate_names_with_different_values_are_not_guessed() {
    let (base, editor, mut studio) = new_branch_fixture();
    studio.instances[1]
        .properties
        .insert("Text".to_string(), json!("studio"));
    let mut duplicate = studio.instances[1].clone();
    duplicate.settings_id = "duplicate".to_string();
    studio.instances.push(duplicate);
    let before = encode_settings_bytecode(&studio).unwrap();
    let error = align_new_instance_ids(&base, &editor, &mut studio).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("Ambiguous new duplicate instances")
    );
    assert_eq!(encode_settings_bytecode(&studio).unwrap(), before);
}

#[test]
fn new_duplicates_in_a_different_sibling_order_pair_by_their_data() {
    let (base, editor, mut studio) = new_branch_fixture();
    let mut editor = editor;
    let border = |id: &str, x: f64| {
        let mut part = SettingsBytecodeInstance::new(
            id.to_string(),
            "Border".to_string(),
            "Part".to_string(),
            Some(0),
        );
        part.properties.insert(
            "Size".to_string(),
            json!({"_type":"Vector3","x":x,"y":1.0,"z":1.0}),
        );
        part
    };
    editor.instances.push(border("left", 1.0));
    editor.instances.push(border("middle", 2.0));
    editor.instances.push(border("right", 3.0));
    studio.instances.push(border("s3", 3.0));
    studio.instances.push(border("s1", 1.0));
    studio.instances.push(border("s2", 2.0));
    align_new_instance_ids(&base, &editor, &mut studio).unwrap();
    let id_of = |x: f64| {
        studio
            .instances
            .iter()
            .find(|i| i.name == "Border" && i.properties["Size"]["x"] == json!(x))
            .unwrap()
            .settings_id
            .clone()
    };
    assert_eq!(id_of(1.0), "left");
    assert_eq!(id_of(2.0), "middle");
    assert_eq!(id_of(3.0), "right");
}

#[test]
fn extra_new_duplicates_on_one_side_are_additions_once_the_rest_pair_by_data() {
    let border = |id: &str, x: f64| {
        let mut part = SettingsBytecodeInstance::new(
            id.to_string(),
            "Border".to_string(),
            "Part".to_string(),
            Some(0),
        );
        part.properties.insert(
            "Size".to_string(),
            json!({"_type":"Vector3","x":x,"y":1.0,"z":1.0}),
        );
        part
    };
    let (base, mut editor, mut studio) = new_branch_fixture();
    editor.instances.push(border("left", 1.0));
    editor.instances.push(border("right", 2.0));
    studio.instances.push(border("extra", 9.0));
    studio.instances.push(border("s2", 2.0));
    studio.instances.push(border("s1", 1.0));
    align_new_instance_ids(&base, &editor, &mut studio).unwrap();
    let id_of = |x: f64| {
        studio
            .instances
            .iter()
            .find(|i| i.name == "Border" && i.properties["Size"]["x"] == json!(x))
            .unwrap()
            .settings_id
            .clone()
    };
    assert_eq!(id_of(1.0), "left");
    assert_eq!(id_of(2.0), "right");
    assert_eq!(id_of(9.0), "extra");

    let (base, mut editor, mut studio) = new_branch_fixture();
    editor.instances.push(border("left", 1.0));
    editor.instances.push(border("changed", 2.0));
    studio.instances.push(border("s1", 1.0));
    studio.instances.push(border("s2", 3.0));
    let error = align_new_instance_ids(&base, &editor, &mut studio).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("Ambiguous new duplicate instances")
    );
}

#[test]
fn children_follow_a_new_parent_paired_with_a_reordered_sibling() {
    let (base, mut editor, mut studio) = new_branch_fixture();
    let part = |id: &str, x: f64| {
        let mut part = SettingsBytecodeInstance::new(
            id.to_string(),
            "Part".to_string(),
            "Part".to_string(),
            Some(0),
        );
        part.properties.insert(
            "Size".to_string(),
            json!({"_type":"Vector3","x":x,"y":1.0,"z":1.0}),
        );
        part
    };
    let texture = |id: &str, parent: usize, r: f64| {
        let mut texture = SettingsBytecodeInstance::new(
            id.to_string(),
            "Texture".to_string(),
            "Texture".to_string(),
            Some(parent),
        );
        texture.properties.insert(
            "Color3".to_string(),
            json!({"_type":"Color3","r":r,"g":1.0,"b":1.0}),
        );
        texture
    };
    let editor_first = editor.instances.len();
    editor.instances.push(part("first", 1.0));
    editor.instances.push(part("second", 2.0));
    editor
        .instances
        .push(texture("first-texture", editor_first, 1.0));
    editor
        .instances
        .push(texture("second-texture", editor_first + 1, 0.5));
    let studio_first = studio.instances.len();
    studio.instances.push(part("s2", 2.0));
    studio.instances.push(part("s1", 1.0));
    studio
        .instances
        .push(texture("s2-texture", studio_first, 0.5));
    studio
        .instances
        .push(texture("s1-texture", studio_first + 1, 1.0));
    align_new_instance_ids(&base, &editor, &mut studio).unwrap();
    let id_of = |name: &str, r: f64| {
        studio
            .instances
            .iter()
            .find(|i| i.name == name && i.properties["Color3"]["r"] == json!(r))
            .unwrap()
            .settings_id
            .clone()
    };
    assert_eq!(id_of("Texture", 1.0), "first-texture");
    assert_eq!(id_of("Texture", 0.5), "second-texture");
}

#[test]
fn new_duplicates_keep_persistent_identity_after_edits_and_reordering() {
    let root =
        SettingsBytecodeInstance::new("root".into(), "Workspace".into(), "Workspace".into(), None);
    let base = SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances: vec![root],
    };
    let mut editor = base.clone();
    for i in 1..=2 {
        let mut part = SettingsBytecodeInstance::new(
            format!("debug:old-{i}"),
            "Part".into(),
            "Part".into(),
            Some(0),
        );
        part.properties.insert(
            "UniqueId".into(),
            json!({"_type":"UniqueId","value":format!("{i:032x}")}),
        );
        part.properties
            .insert("Transparency".into(), json!(i as f64 / 10.0));
        editor.instances.push(part);
    }
    let mut studio = editor.clone();
    studio.instances[1].settings_id = "debug:new-1".into();
    studio.instances[2].settings_id = "debug:new-2".into();
    studio.instances[1]
        .properties
        .insert("Transparency".into(), json!(0.7));
    studio.instances[2]
        .properties
        .insert("Transparency".into(), json!(0.8));
    studio.instances[0].properties.insert(
        "PrimaryPart".into(),
        json!({"_type":"Ref","settingsId":"debug:new-1"}),
    );
    studio.instances.swap(1, 2);
    let mut first_pair = studio.clone();
    let mut first_pair_conflicts = ConflictLog::new(ConflictPreference::None);
    align_first_pairing(
        Path::new("instances/Workspace.renium"),
        &mut editor.clone(),
        &mut first_pair,
        &mut first_pair_conflicts,
    );
    let first_pair_conflicts = first_pair_conflicts.open();
    assert!(!first_pair_conflicts.is_empty());
    assert!(
        first_pair_conflicts
            .iter()
            .all(|conflict| conflict.kind != ConflictKind::Pairing),
        "{first_pair_conflicts:?}"
    );
    align_new_instance_ids(&base, &editor, &mut studio).unwrap();
    assert_eq!(studio.instances[1].settings_id, "debug:old-2");
    assert_eq!(studio.instances[2].settings_id, "debug:old-1");
    assert_eq!(
        studio.instances[0].properties["PrimaryPart"]["settingsId"],
        "debug:old-1"
    );
    assert_eq!(studio.instances[2].properties["Transparency"], 0.7);
    let mut conflicts = ConflictLog::new(ConflictPreference::None);
    let merged = merge_reconciliation_settings_documents(
        Path::new("instances/Workspace.renium"),
        [&base, &editor, &studio],
        &mut conflicts,
        &HashSet::new(),
        &HashSet::new(),
    );
    assert_eq!(
        merged.instances.len(),
        3,
        "matched parts must not be duplicated"
    );
    assert!(
        !conflicts.open().is_empty(),
        "identity matching must retain the real value conflict"
    );
}

#[test]
fn first_pairing_ignores_only_the_identified_viewport_camera() {
    let mut root =
        SettingsBytecodeInstance::new("root".into(), "Workspace".into(), "Workspace".into(), None);
    root.properties.insert(
        "CurrentCamera".into(),
        json!({"_type":"Ref","settingsId":"viewport"}),
    );
    let mut camera =
        SettingsBytecodeInstance::new("viewport".into(), "View".into(), "Camera".into(), Some(0));
    camera.properties.insert("FieldOfView".into(), json!(70));
    let mut authored = camera.clone();
    authored.settings_id = "authored".into();
    authored.name = "AuthoredCamera".into();
    let editor = SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances: vec![root, camera, authored],
    };
    let mut studio = editor.clone();
    studio.instances[1]
        .properties
        .insert("FieldOfView".into(), json!(90));
    studio.instances[2]
        .properties
        .insert("FieldOfView".into(), json!(100));
    let mut conflicts = ConflictLog::new(ConflictPreference::None);
    align_first_pairing(
        Path::new("instances/Workspace.renium"),
        &mut editor.clone(),
        &mut studio,
        &mut conflicts,
    );
    let conflicts = conflicts.open();
    assert_eq!(conflicts.len(), 1, "{conflicts:?}");
    assert!(conflicts[0].detail.contains("AuthoredCamera"));
    assert_eq!(studio.instances[1].properties["FieldOfView"], 70);
}

#[test]
fn prepared_full_push_reuses_the_same_duplicate_and_reference_mapping() {
    let root =
        SettingsBytecodeInstance::new("root".into(), "Workspace".into(), "Workspace".into(), None);
    let mut first = SettingsBytecodeInstance::new(
        "first".into(),
        "Duplicate".into(),
        "StringValue".into(),
        Some(0),
    );
    first.properties.insert("Value".into(), json!("first"));
    let mut second = first.clone();
    second.settings_id = "second".into();
    second.properties.insert("Value".into(), json!("second"));
    let mut pointer = SettingsBytecodeInstance::new(
        "pointer".into(),
        "Pointer".into(),
        "ObjectValue".into(),
        Some(0),
    );
    pointer
        .properties
        .insert("Value".into(), json!({"_type":"Ref","settingsId":"first"}));
    let mut desired = SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances: vec![root, first, second, pointer],
    };
    let mut observed = desired.clone();
    observed.instances.swap(1, 2);
    observed.instances[1].settings_id = "debug:second".into();
    observed.instances[2].settings_id = "debug:first".into();
    observed.instances[3].properties.insert(
        "Value".into(),
        json!({"_type":"Ref","settingsId":"debug:first"}),
    );
    desired.instances[0]
        .attributes
        .insert("Revision".into(), json!(1));
    let path = PathBuf::from("src/Workspace/__roblox_sync_settings.renium");
    let snapshot = |doc: &SettingsBytecode| ProjectSnapshot {
        entries: BTreeMap::from([(
            path.clone(),
            SnapshotEntry::File(encode_settings_bytecode(doc).unwrap()),
        )]),
    };
    let studio = snapshot(&observed);
    let project = snapshot(&desired);
    let mut prepared = HashMap::new();
    let paths =
        snapshot_differences_prepared(&project, &studio, Some(&mut prepared), false).unwrap();
    assert_eq!(paths, snapshot_differences(&project, &studio).unwrap());
    assert_eq!(prepared.len(), 1);
    let change = &prepared[&path];
    assert_eq!(change.previous.instances[1].settings_id, "second");
    assert_eq!(change.previous.instances[2].settings_id, "first");
    assert_eq!(
        change.previous.instances[3].properties["Value"]["settingsId"],
        "first"
    );
    let expected = reconciliation_push_plan_for_paths_with_prepared_settings(
        &studio,
        &project,
        &paths,
        &HashMap::new(),
        false,
        false,
    )
    .unwrap();
    let actual = reconciliation_push_plan_for_paths_with_prepared_settings(
        &studio, &project, &paths, &prepared, false, false,
    )
    .unwrap();
    assert_eq!(actual.changed_paths, expected.changed_paths);
    assert_eq!(actual.target_settings_ids, expected.target_settings_ids);
    assert_eq!(
        actual.recreated_settings_ids,
        expected.recreated_settings_ids
    );
    assert_eq!(
        serde_json::to_value(&actual.instance_deletes).unwrap(),
        serde_json::to_value(&expected.instance_deletes).unwrap()
    );
    assert_eq!(
        serde_json::to_value(&actual.property_removals).unwrap(),
        serde_json::to_value(&expected.property_removals).unwrap()
    );
}

#[test]
fn native_import_settlement_keeps_checked_values_and_rejects_failed_captures() -> Result<()> {
    let path = PathBuf::from("instances/Workspace.renium");
    let mut root =
        SettingsBytecodeInstance::new("root".into(), "Workspace".into(), "Workspace".into(), None);
    root.properties.insert(
        "CurrentCamera".into(),
        json!({"_type":"Ref", "settingsId":"viewport"}),
    );
    let mut camera =
        SettingsBytecodeInstance::new("viewport".into(), "Camera".into(), "Camera".into(), Some(0));
    camera.properties.insert("FieldOfView".into(), json!(70));
    let mut before = SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances: vec![root, camera],
    };
    let mut desired = before.clone();
    let mut part =
        SettingsBytecodeInstance::new("part".into(), "Part".into(), "Part".into(), Some(0));
    part.properties.insert("Transparency".into(), json!(0.25));
    part.properties.insert(
        "UniqueId".into(),
        json!({"_type":"UniqueId", "value":format!("{:032x}", 1)}),
    );
    desired.instances.push(part);
    let mut observed = desired.clone();
    observed.instances[1]
        .properties
        .insert("FieldOfView".into(), json!(90));
    observed.instances[2].properties.insert(
        "UniqueId".into(),
        json!({"_type":"UniqueId", "value":format!("{:032x}", 2)}),
    );
    let snapshot = |document: &SettingsBytecode| -> Result<ProjectSnapshot> {
        Ok(ProjectSnapshot {
            entries: BTreeMap::from([(
                path.clone(),
                SnapshotEntry::File(encode_settings_bytecode(document)?),
            )]),
        })
    };
    let desired_snapshot = snapshot(&desired)?;
    let before_snapshot = snapshot(&before)?;
    let paths = HashSet::from([path.clone()]);
    let mut readback = snapshot(&observed)?;
    let mut prepared = HashMap::from([(
        path.clone(),
        PreparedPushVerification {
            previous: std::mem::replace(
                &mut before,
                SettingsBytecode {
                    version: SETTINGS_BINARY_VERSION,
                    instances: Vec::new(),
                },
            ),
            desired: Some(Arc::new(desired.clone())),
        },
    )]);
    let (failures, _) = verify_and_settle_native_import(
        &before_snapshot,
        &desired_snapshot,
        &mut readback,
        &paths,
        &mut prepared,
        None,
    )?;
    assert!(failures.is_empty());
    let settled = settings_document(readback.entries.get(&path))?;
    assert_eq!(settled.instances[1].properties["FieldOfView"], json!(70));
    assert_eq!(
        settled.instances[2].properties["UniqueId"],
        observed.instances[2].properties["UniqueId"]
    );
    assert!(matches!(
        crate::settings::equivalence::align_pulled_settings_bytes_to_reference(
            match &readback.entries[&path] {
                SnapshotEntry::File(bytes) => bytes,
                _ => unreachable!(),
            },
            &encode_settings_bytecode(&observed)?,
        )?,
        crate::settings::equivalence::SettingsAlignment::Equivalent
    ));

    observed.instances[2]
        .properties
        .insert("Transparency".into(), json!(0.5));
    let mut rejected = snapshot(&observed)?;
    let original = rejected.clone();
    let (failures, _) = verify_and_settle_native_import(
        &before_snapshot,
        &desired_snapshot,
        &mut rejected,
        &paths,
        &mut HashMap::new(),
        None,
    )?;
    assert_eq!(failures, vec![path]);
    assert!(
        rejected == original,
        "A failed verification must not produce settled bytes"
    );
    Ok(())
}

#[test]
fn viewport_retention_does_not_skip_other_camera_mutations_or_verification() {
    let path = Path::new("src/Workspace/__roblox_sync_settings.renium");
    let mut root =
        SettingsBytecodeInstance::new("root".into(), "Workspace".into(), "Workspace".into(), None);
    root.properties.insert(
        "CurrentCamera".into(),
        json!({"_type": "Ref", "settingsId": "viewport"}),
    );
    let mut camera =
        SettingsBytecodeInstance::new("viewport".into(), "Camera".into(), "Camera".into(), Some(0));
    camera.properties.insert("FieldOfView".into(), json!(70));
    let mut other = camera.clone();
    other.settings_id = "other".into();
    let before = SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances: vec![root, camera, other],
    };
    let entry = |document: &SettingsBytecode| {
        SnapshotEntry::File(encode_settings_bytecode(document).unwrap())
    };
    let before_entry = entry(&before);
    let mut desired = before.clone();
    for instance in &mut desired.instances[1..] {
        instance.properties.insert("FieldOfView".into(), json!(85));
    }
    let mut plan = ReconcilePushPlan::default();
    append_aligned_settings_push_plan(path, &desired, &before, &mut plan).unwrap();
    assert_eq!(plan.target_settings_ids, ["other"]);
    let desired_entry = entry(&desired);
    assert!(
        settings_delta_mismatch(
            path,
            Some(&before_entry),
            Some(&desired_entry),
            Some(&before_entry),
            None
        )
        .unwrap()
        .is_some()
    );
    let mut observed = before.clone();
    observed.instances[2]
        .properties
        .insert("FieldOfView".into(), json!(85));
    assert!(
        settings_delta_mismatch(
            path,
            Some(&before_entry),
            Some(&desired_entry),
            Some(&entry(&observed)),
            None
        )
        .unwrap()
        .is_none()
    );

    desired.instances.truncate(1);
    desired.instances[0].properties.remove("CurrentCamera");
    let mut plan = ReconcilePushPlan::default();
    append_aligned_settings_push_plan(path, &desired, &before, &mut plan).unwrap();
    assert_eq!(plan.instance_deletes.len(), 1);
    assert_eq!(plan.instance_deletes[0].instances.len(), 1);
    let desired_entry = entry(&desired);
    assert!(
        settings_delta_mismatch(
            path,
            Some(&before_entry),
            Some(&desired_entry),
            Some(&before_entry),
            None
        )
        .unwrap()
        .is_some()
    );
    observed.instances.truncate(2);
    assert!(
        settings_delta_mismatch(
            path,
            Some(&before_entry),
            Some(&desired_entry),
            Some(&entry(&observed)),
            None
        )
        .unwrap()
        .is_none()
    );
}

#[test]
fn omitted_engine_container_keeps_anchor_but_removes_ordinary_contents() {
    for (service, class_name) in [
        ("Workspace", "Terrain"),
        ("StarterPlayer", "StarterPlayerScripts"),
        ("StarterPlayer", "StarterCharacterScripts"),
        ("TextChatService", "ChatWindowConfiguration"),
        ("TextChatService", "ChatInputBarConfiguration"),
        ("TextChatService", "BubbleChatConfiguration"),
        ("TextChatService", "ChannelTabsConfiguration"),
    ] {
        let instance = |id: &str, name: &str, class: &str, parent_index| SettingsBytecodeInstance {
            settings_id: id.into(),
            name: name.into(),
            class_name: class.into(),
            parent_index,
            properties: Map::new(),
            attributes: Map::new(),
        };
        let root = instance("root", service, service, None);
        let container = instance("anchor", class_name, class_name, Some(0));
        let child = instance("child", "RemovedContent", "Folder", Some(1));
        let namesake = instance("namesake", class_name, "Folder", Some(0));
        let document = |instances| SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances,
        };
        let before = document(vec![root.clone(), container.clone(), child, namesake]);
        let desired = document(vec![root.clone()]);
        let actual = document(vec![root, container]);
        let path = PathBuf::from(format!("src/{service}/__roblox_sync_settings.renium"));
        let mut plan = ReconcilePushPlan::default();
        append_aligned_settings_push_plan(&path, &desired, &before, &mut plan).unwrap();
        let deleted: HashSet<_> = plan
            .instance_deletes
            .iter()
            .flat_map(|change| {
                change
                    .instances
                    .iter()
                    .map(|entry| entry.settings_id.as_str())
            })
            .collect();
        assert_eq!(
            deleted,
            HashSet::from(["child", "namesake"]),
            "{class_name}"
        );
        let entry =
            |doc: &SettingsBytecode| SnapshotEntry::File(encode_settings_bytecode(doc).unwrap());
        for desired in [Some(entry(&desired)), None] {
            assert_eq!(
                settings_delta_mismatch(
                    &path,
                    Some(&entry(&before)),
                    desired.as_ref(),
                    Some(&entry(&actual)),
                    None
                )
                .unwrap(),
                None,
                "{class_name}"
            );
            assert!(
                settings_delta_mismatch(
                    &path,
                    Some(&entry(&before)),
                    desired.as_ref(),
                    Some(&entry(&before)),
                    None
                )
                .unwrap()
                .is_some(),
                "Contents must be removed"
            );
        }
        let mut authored = actual.clone();
        authored.instances[1]
            .attributes
            .insert("Authored".into(), json!(true));
        assert!(
            settings_delta_mismatch(
                &path,
                Some(&entry(&actual)),
                Some(&entry(&authored)),
                Some(&entry(&actual)),
                None
            )
            .unwrap()
            .is_some(),
            "Authored anchor edits must verify"
        );
    }
}

#[test]
fn ordinary_reconciliation_never_deletes_a_package_link() {
    let root = SettingsBytecodeInstance {
        settings_id: "root".to_string(),
        name: "ReplicatedStorage".to_string(),
        class_name: "ReplicatedStorage".to_string(),
        parent_index: None,
        properties: Map::new(),
        attributes: Map::new(),
    };
    let package_link = SettingsBytecodeInstance {
        settings_id: "link".to_string(),
        name: "PackageLink".to_string(),
        class_name: "PackageLink".to_string(),
        parent_index: Some(0),
        properties: Map::new(),
        attributes: Map::new(),
    };
    let baseline_doc = SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances: vec![root.clone(), package_link],
    };
    let editor_doc = SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances: vec![root],
    };
    let settings_path = PathBuf::from("src/ReplicatedStorage/__roblox_sync_settings.renium");
    let snapshot = |document: &SettingsBytecode| ProjectSnapshot {
        entries: [(
            settings_path.clone(),
            SnapshotEntry::File(encode_settings_bytecode(document).unwrap()),
        )]
        .into_iter()
        .collect(),
    };
    let baseline = snapshot(&baseline_doc);
    let editor = snapshot(&editor_doc);
    let studio = snapshot(&baseline_doc);
    let (_, conflicts) = merge_snapshots(
        Some(&baseline),
        &editor,
        &studio,
        ConflictPreference::Editor,
    )
    .unwrap();
    assert!(
        conflicts
            .iter()
            .any(|conflict| conflict.detail.contains("PackageLink"))
    );
    assert!(
        validate_editor_package_links(&baseline, &editor, std::slice::from_ref(&settings_path))
            .is_err()
    );
}

#[test]
fn reconciliation_allows_replacing_an_entire_package_root() {
    let instance =
        |id: &str, name: &str, class_name: &str, parent_index| SettingsBytecodeInstance {
            settings_id: id.to_string(),
            name: name.to_string(),
            class_name: class_name.to_string(),
            parent_index,
            properties: Map::new(),
            attributes: Map::new(),
        };
    let root = instance("root", "Workspace", "Workspace", None);
    let old_package = instance("old-package", "Old car", "Model", Some(0));
    let old_link = instance("old-link", "PackageLink", "PackageLink", Some(1));
    let new_package = instance("new-package", "New car", "Model", Some(0));
    let new_link = instance("new-link", "PackageLink", "PackageLink", Some(1));
    let document = |instances| SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances,
    };
    let settings_path = PathBuf::from("src/Workspace/__roblox_sync_settings.renium");
    let snapshot = |document: SettingsBytecode| ProjectSnapshot {
        entries: [(
            settings_path.clone(),
            SnapshotEntry::File(encode_settings_bytecode(&document).unwrap()),
        )]
        .into_iter()
        .collect(),
    };
    let baseline = snapshot(document(vec![
        root.clone(),
        old_package.clone(),
        old_link.clone(),
    ]));
    let editor = baseline.clone();
    let studio = snapshot(document(vec![root.clone(), new_package, new_link]));

    let (merged, conflicts) =
        merge_snapshots(Some(&baseline), &editor, &studio, ConflictPreference::None).unwrap();
    assert!(conflicts.is_empty());
    let merged = settings_document(merged.entries.get(&settings_path)).unwrap();
    assert!(
        merged
            .instances
            .iter()
            .any(|instance| instance.name == "New car")
    );
    assert!(
        !merged
            .instances
            .iter()
            .any(|instance| instance.name == "Old car")
    );

    let removed_package = snapshot(document(vec![root]));
    assert!(
        validate_editor_package_links(
            &baseline,
            &removed_package,
            std::slice::from_ref(&settings_path),
        )
        .is_ok()
    );
    let direct_link_removal = snapshot(document(vec![
        instance("root", "Workspace", "Workspace", None),
        old_package,
    ]));
    assert!(
        validate_editor_package_links(
            &baseline,
            &direct_link_removal,
            std::slice::from_ref(&settings_path),
        )
        .is_err()
    );
}

#[test]
fn package_link_guard_uses_project_identity_not_export_ids() {
    let document = |ids: [&str; 4], moved: bool, package_content: &str| SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances: vec![
            SettingsBytecodeInstance {
                settings_id: ids[0].to_string(),
                name: "ReplicatedStorage".to_string(),
                class_name: "ReplicatedStorage".to_string(),
                parent_index: None,
                properties: Map::new(),
                attributes: Map::new(),
            },
            SettingsBytecodeInstance {
                settings_id: ids[1].to_string(),
                name: "Container".to_string(),
                class_name: "Folder".to_string(),
                parent_index: Some(0),
                properties: Map::new(),
                attributes: Map::new(),
            },
            SettingsBytecodeInstance {
                settings_id: ids[2].to_string(),
                name: "Package".to_string(),
                class_name: "Folder".to_string(),
                parent_index: Some(usize::from(moved)),
                properties: Map::new(),
                attributes: Map::new(),
            },
            SettingsBytecodeInstance {
                settings_id: ids[3].to_string(),
                name: "PackageLink".to_string(),
                class_name: "PackageLink".to_string(),
                parent_index: Some(2),
                properties: Map::from_iter([(
                    "PackageContent".to_string(),
                    Value::String(package_content.to_string()),
                )]),
                attributes: Map::new(),
            },
        ],
    };
    let path = PathBuf::from("src/ReplicatedStorage/__roblox_sync_settings.renium");
    let snapshot = |document: &SettingsBytecode| ProjectSnapshot {
        entries: [(
            path.clone(),
            SnapshotEntry::File(encode_settings_bytecode(document).unwrap()),
        )]
        .into_iter()
        .collect(),
    };
    let baseline = snapshot(&document(
        ["root", "container", "package", "link"],
        false,
        "rbxassetid://1",
    ));
    let exported = snapshot(&document(
        ["debug:1", "debug:2", "debug:3", "debug:4"],
        false,
        "rbxassetid://1",
    ));
    assert!(
        validate_editor_package_links(&baseline, &exported, std::slice::from_ref(&path)).is_ok()
    );
    let mut transport_omission = document(
        ["debug:1", "debug:2", "debug:3", "debug:4"],
        false,
        "rbxassetid://1",
    );
    transport_omission.instances[3]
        .properties
        .remove("PackageContent");
    transport_omission.instances[3]
        .properties
        .insert("Archivable".to_string(), Value::Bool(true));
    assert!(
        validate_editor_package_links(
            &baseline,
            &snapshot(&transport_omission),
            std::slice::from_ref(&path),
        )
        .is_ok()
    );
    let mut runtime_state = document(
        ["debug:1", "debug:2", "debug:3", "debug:4"],
        false,
        "rbxassetid://1",
    );
    runtime_state.instances[3]
        .properties
        .insert("ModifiedState".to_string(), Value::Number(1.into()));
    assert!(
        validate_editor_package_links(
            &baseline,
            &snapshot(&runtime_state),
            std::slice::from_ref(&path),
        )
        .is_ok()
    );
    let aligned = align_snapshot_ids(&baseline, &exported).unwrap();
    let aligned = settings_document(aligned.entries.get(&path)).unwrap();
    assert_eq!(aligned.instances[3].settings_id, "link");
    assert_eq!(
        aligned.instances[3].properties.get("PackageContent"),
        Some(&Value::String("rbxassetid://1".to_string()))
    );

    let moved = snapshot(&document(
        ["root", "container", "package", "link"],
        true,
        "rbxassetid://1",
    ));
    assert!(validate_editor_package_links(&baseline, &moved, std::slice::from_ref(&path)).is_ok());

    let edited = snapshot(&document(
        ["debug:1", "debug:2", "debug:3", "debug:4"],
        false,
        "rbxassetid://2",
    ));
    assert!(
        validate_editor_package_links(&baseline, &edited, std::slice::from_ref(&path)).is_err()
    );
}

#[test]
fn reconciliation_pushes_only_semantically_changed_instances() {
    let settings_snapshot = |service: &str, document: SettingsBytecode| {
        (
            PathBuf::from(format!("src/{service}/__roblox_sync_settings.renium")),
            SnapshotEntry::File(encode_settings_bytecode(&document).unwrap()),
        )
    };
    let root = |service: &str| SettingsBytecodeInstance {
        settings_id: format!("{service}-root"),
        name: service.to_string(),
        class_name: service.to_string(),
        parent_index: None,
        properties: Map::new(),
        attributes: Map::new(),
    };
    let replicated = |value: &str| SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances: vec![
            root("ReplicatedStorage"),
            SettingsBytecodeInstance {
                settings_id: "changed-folder".to_string(),
                name: "Changed".to_string(),
                class_name: "Folder".to_string(),
                parent_index: Some(0),
                properties: Map::from_iter([(
                    "Archivable".to_string(),
                    Value::String(value.to_string()),
                )]),
                attributes: Map::new(),
            },
        ],
    };
    let server = |asset_id: &str, replacement_class: &str| SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances: vec![
            root("ServerStorage"),
            SettingsBytecodeInstance {
                settings_id: "replacement".to_string(),
                name: "Replacement".to_string(),
                class_name: replacement_class.to_string(),
                parent_index: Some(0),
                properties: Map::new(),
                attributes: Map::new(),
            },
            SettingsBytecodeInstance {
                settings_id: "public".to_string(),
                name: "Public".to_string(),
                class_name: "Folder".to_string(),
                parent_index: Some(0),
                properties: Map::new(),
                attributes: Map::new(),
            },
            SettingsBytecodeInstance {
                settings_id: "package-link".to_string(),
                name: "PackageLink".to_string(),
                class_name: "PackageLink".to_string(),
                parent_index: Some(2),
                properties: Map::from_iter([(
                    "PackageId".to_string(),
                    Value::String(asset_id.to_string()),
                )]),
                attributes: Map::new(),
            },
        ],
    };
    let studio = ProjectSnapshot {
        entries: [
            settings_snapshot("ReplicatedStorage", replicated("before")),
            settings_snapshot("ServerStorage", server("old", "Folder")),
        ]
        .into_iter()
        .collect(),
    };
    let merged = ProjectSnapshot {
        entries: [
            settings_snapshot("ReplicatedStorage", replicated("after")),
            settings_snapshot("ServerStorage", server("new", "Model")),
        ]
        .into_iter()
        .collect(),
    };

    let plan = reconciliation_push_plan(&studio, &merged).unwrap();
    assert_eq!(
        plan.changed_paths,
        vec![
            PathBuf::from("src/ReplicatedStorage/__roblox_sync_settings.renium"),
            PathBuf::from("src/ServerStorage/__roblox_sync_settings.renium"),
        ]
    );
    assert_eq!(
        plan.target_settings_ids,
        vec!["changed-folder", "replacement"]
    );
    assert_eq!(
        plan.previous_class_names
            .get(&("ServerStorage".to_string(), "replacement".to_string()))
            .map(String::as_str),
        Some("Folder")
    );
    assert!(
        !plan
            .target_settings_ids
            .iter()
            .any(|id| id == "package-link")
    );
    assert!(plan.instance_deletes.is_empty());

    let ordered = |ids: [&str; 3]| SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances: std::iter::once(root("ReplicatedStorage"))
            .chain(ids.into_iter().map(|id| SettingsBytecodeInstance {
                settings_id: id.to_string(),
                name: id.to_uppercase(),
                class_name: "Folder".to_string(),
                parent_index: Some(0),
                properties: Map::new(),
                attributes: Map::new(),
            }))
            .collect(),
    };
    let mut order_plan = ReconcilePushPlan::default();
    append_settings_push_plan(
        Path::new("src/ReplicatedStorage/__roblox_sync_settings.renium"),
        &ordered(["a", "c", "b"]),
        &ordered(["a", "b", "c"]),
        &mut order_plan,
    )
    .unwrap();
    assert!(order_plan.target_settings_ids.is_empty());
}

#[test]
fn incremental_editor_plan_uses_persisted_ids_for_duplicate_siblings() {
    let path = PathBuf::from("src/Workspace/__roblox_sync_settings.renium");
    let root = SettingsBytecodeInstance {
        settings_id: "1".to_string(),
        name: "Workspace".to_string(),
        class_name: "Workspace".to_string(),
        parent_index: None,
        properties: Map::new(),
        attributes: Map::new(),
    };
    let duplicate = |index: usize| SettingsBytecodeInstance {
        settings_id: format!("debug:{index}"),
        name: "Duplicate".to_string(),
        class_name: "StringValue".to_string(),
        parent_index: Some(0),
        properties: Map::from_iter([("Value".to_string(), json!(index.to_string()))]),
        attributes: Map::new(),
    };
    let package_link = SettingsBytecodeInstance {
        settings_id: "debug:package-link".to_string(),
        name: "PackageLink".to_string(),
        class_name: "PackageLink".to_string(),
        parent_index: Some(0),
        properties: Map::from_iter([("PackageContent".to_string(), json!("rbxassetid://1"))]),
        attributes: Map::new(),
    };
    let mut previous_instances = Vec::with_capacity(4_098);
    previous_instances.push(root.clone());
    previous_instances.extend((0..4_096).map(duplicate));
    previous_instances.push(package_link.clone());
    let previous_document = SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances: previous_instances,
    };
    let mut current_document = previous_document.clone();
    current_document.instances[1_001]
        .properties
        .insert("Value".to_string(), json!("changed"));
    current_document.instances.remove(2_001);
    current_document.instances.push(SettingsBytecodeInstance {
        settings_id: "debug:new".to_string(),
        ..duplicate(4_096)
    });

    let snapshot = |document: &SettingsBytecode| ProjectSnapshot {
        entries: [(
            path.clone(),
            SnapshotEntry::File(encode_settings_bytecode(document).unwrap()),
        )]
        .into_iter()
        .collect(),
    };
    let previous = snapshot(&previous_document);
    let current = snapshot(&current_document);
    let prepared =
        prepare_editor_settings_changes(&previous, &current, std::slice::from_ref(&path)).unwrap();
    let plan = reconciliation_push_plan_for_paths_with_prepared_settings(
        &previous,
        &current,
        &HashSet::from_iter([path.clone()]),
        &prepared,
        false,
        false,
    )
    .unwrap();

    assert_eq!(plan.changed_paths, vec![path]);
    assert_eq!(
        plan.target_settings_ids,
        vec!["debug:1000".to_string(), "debug:new".to_string()]
    );
    assert_eq!(plan.instance_deletes.len(), 1);
    assert_eq!(plan.instance_deletes[0].instances.len(), 1);
    assert_eq!(
        plan.instance_deletes[0].instances[0].settings_id,
        "debug:2000"
    );
}

#[test]
fn incremental_cross_service_recreation_reapplies_target_service_referrers() {
    let root = |service: &str| SettingsBytecodeInstance {
        settings_id: format!("{service}-root"),
        name: service.to_string(),
        class_name: service.to_string(),
        parent_index: None,
        properties: Map::new(),
        attributes: Map::new(),
    };
    let target = SettingsBytecodeInstance {
        settings_id: "target".to_string(),
        name: "Target".to_string(),
        class_name: "Folder".to_string(),
        parent_index: Some(0),
        properties: Map::new(),
        attributes: Map::new(),
    };
    let holder = |service: &str| SettingsBytecodeInstance {
        settings_id: "holder".to_string(),
        name: "Holder".to_string(),
        class_name: "ObjectValue".to_string(),
        parent_index: Some(0),
        properties: Map::from_iter([(
            "Value".to_string(),
            json!({
                "_type": "Ref",
                "settingsId": "target",
                "pathSegments": [service, "Target"],
                "pathOrdinals": [1, 1]
            }),
        )]),
        attributes: Map::new(),
    };
    let replicated_path = PathBuf::from("src/ReplicatedStorage/__roblox_sync_settings.renium");
    let storage_path = PathBuf::from("src/ServerStorage/__roblox_sync_settings.renium");
    let previous_replicated = SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances: vec![root("ReplicatedStorage"), target.clone()],
    };
    let previous_storage = SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances: vec![root("ServerStorage"), holder("ReplicatedStorage")],
    };
    let current_replicated = SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances: vec![root("ReplicatedStorage")],
    };
    let current_storage = SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances: vec![root("ServerStorage"), holder("ServerStorage"), target],
    };
    let previous = ProjectSnapshot {
        entries: [
            (
                replicated_path.clone(),
                SnapshotEntry::File(encode_settings_bytecode(&previous_replicated).unwrap()),
            ),
            (
                storage_path.clone(),
                SnapshotEntry::File(encode_settings_bytecode(&previous_storage).unwrap()),
            ),
        ]
        .into_iter()
        .collect(),
    };
    let current = ProjectSnapshot {
        entries: [
            (
                replicated_path.clone(),
                SnapshotEntry::File(encode_settings_bytecode(&current_replicated).unwrap()),
            ),
            (
                storage_path.clone(),
                SnapshotEntry::File(encode_settings_bytecode(&current_storage).unwrap()),
            ),
        ]
        .into_iter()
        .collect(),
    };
    let scopes = vec![replicated_path.clone(), storage_path.clone()];
    let prepared = prepare_editor_settings_changes(&previous, &current, &scopes).unwrap();
    let plan = reconciliation_push_plan_for_paths_with_prepared_settings(
        &previous,
        &current,
        &scopes.into_iter().collect(),
        &prepared,
        false,
        false,
    )
    .unwrap();

    assert!(plan.target_settings_ids.iter().any(|id| id == "target"));
    assert!(plan.target_settings_ids.iter().any(|id| id == "holder"));
}

/// A store that differs in nothing is read for what refers to a new instance
/// only where it holds that instance's id, and its referrer is sent then.
#[test]
fn an_unchanged_store_that_names_a_new_instance_is_pushed_with_it() {
    let root = |service: &str| {
        SettingsBytecodeInstance::new(
            format!("{service}-root"),
            service.into(),
            service.into(),
            None,
        )
    };
    let instance = |id: &str, class: &str| {
        SettingsBytecodeInstance::new(id.into(), id.into(), class.into(), Some(0))
    };
    let mut holder = instance("holder", "ObjectValue");
    holder.properties.insert(
        "Value".into(),
        json!({
            "_type": "Ref",
            "settingsId": "target",
            "pathSegments": ["ReplicatedStorage", "target"],
            "pathOrdinals": [1, 1]
        }),
    );
    let store = |service: &str, instances: Vec<SettingsBytecodeInstance>| {
        let mut all = vec![root(service)];
        all.extend(instances);
        (
            PathBuf::from(format!("src/{service}/__roblox_sync_settings.renium")),
            SnapshotEntry::File(
                encode_settings_bytecode(&SettingsBytecode {
                    version: SETTINGS_BINARY_VERSION,
                    instances: all,
                })
                .unwrap(),
            ),
        )
    };
    let snapshot = |target: bool| ProjectSnapshot {
        entries: BTreeMap::from([
            store(
                "ReplicatedStorage",
                if target {
                    vec![instance("target", "Folder")]
                } else {
                    Vec::new()
                },
            ),
            store("ServerStorage", vec![holder.clone()]),
            store("Lighting", vec![instance("other", "Folder")]),
        ]),
    };
    let (previous, current) = (snapshot(false), snapshot(true));
    let changed = PathBuf::from("src/ReplicatedStorage/__roblox_sync_settings.renium");
    let scopes = vec![changed.clone()];
    let prepared = prepare_editor_settings_changes(&previous, &current, &scopes).unwrap();
    let plan = reconciliation_push_plan_for_paths_with_prepared_settings(
        &previous,
        &current,
        &HashSet::from([changed]),
        &prepared,
        false,
        false,
    )
    .unwrap();
    assert_eq!(plan.target_settings_ids, ["holder", "target"]);
    assert!(
        !plan
            .changed_paths
            .iter()
            .any(|path| path.starts_with("src/Lighting"))
    );
}

#[test]
fn incremental_added_target_reapplies_existing_same_service_referrer() {
    let root = SettingsBytecodeInstance {
        settings_id: "storage-root".to_string(),
        name: "ServerStorage".to_string(),
        class_name: "ServerStorage".to_string(),
        parent_index: None,
        properties: Map::new(),
        attributes: Map::new(),
    };
    let holder = |service: &str| SettingsBytecodeInstance {
        settings_id: "holder".to_string(),
        name: "Holder".to_string(),
        class_name: "ObjectValue".to_string(),
        parent_index: Some(0),
        properties: Map::from_iter([(
            "Value".to_string(),
            json!({
                "_type": "Ref",
                "settingsId": "target",
                "pathSegments": [service, "Target"],
                "pathOrdinals": [1, 1]
            }),
        )]),
        attributes: Map::new(),
    };
    let target = SettingsBytecodeInstance {
        settings_id: "target".to_string(),
        name: "Target".to_string(),
        class_name: "Folder".to_string(),
        parent_index: Some(0),
        properties: Map::new(),
        attributes: Map::new(),
    };
    let path = PathBuf::from("src/ServerStorage/__roblox_sync_settings.renium");
    let previous_document = SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances: vec![root.clone(), holder("ReplicatedStorage")],
    };
    let current_document = SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances: vec![root, holder("ServerStorage"), target],
    };
    let snapshot = |document: &SettingsBytecode| ProjectSnapshot {
        entries: [(
            path.clone(),
            SnapshotEntry::File(encode_settings_bytecode(document).unwrap()),
        )]
        .into_iter()
        .collect(),
    };
    let previous = snapshot(&previous_document);
    let current = snapshot(&current_document);
    let scopes = vec![path.clone()];
    let prepared = prepare_editor_settings_changes(&previous, &current, &scopes).unwrap();
    let plan = reconciliation_push_plan_for_paths_with_prepared_settings(
        &previous,
        &current,
        &HashSet::from([path]),
        &prepared,
        false,
        false,
    )
    .unwrap();

    assert!(plan.target_settings_ids.iter().any(|id| id == "target"));
    assert!(plan.target_settings_ids.iter().any(|id| id == "holder"));
}

#[test]
fn reconciliation_uses_targeted_deletes_and_blocks_direct_package_link_deletes() {
    let root = SettingsBytecodeInstance {
        settings_id: "root".to_string(),
        name: "ServerStorage".to_string(),
        class_name: "ServerStorage".to_string(),
        parent_index: None,
        properties: Map::new(),
        attributes: Map::new(),
    };
    let folder = SettingsBytecodeInstance {
        settings_id: "folder".to_string(),
        name: "Removed".to_string(),
        class_name: "Folder".to_string(),
        parent_index: Some(0),
        properties: Map::new(),
        attributes: Map::new(),
    };
    let snapshot = |instances| ProjectSnapshot {
        entries: [(
            PathBuf::from("src/ServerStorage/__roblox_sync_settings.renium"),
            SnapshotEntry::File(
                encode_settings_bytecode(&SettingsBytecode {
                    version: SETTINGS_BINARY_VERSION,
                    instances,
                })
                .unwrap(),
            ),
        )]
        .into_iter()
        .collect(),
    };
    let desired = snapshot(vec![root.clone()]);
    let studio = snapshot(vec![root.clone(), folder.clone()]);
    let plan = reconciliation_push_plan(&studio, &desired).unwrap();
    assert!(plan.changed_paths.is_empty());
    assert!(plan.target_settings_ids.is_empty());
    assert_eq!(plan.instance_deletes.len(), 1);
    assert_eq!(plan.instance_deletes[0].instances.len(), 1);

    let plan = reconciliation_push_plan(&studio, &ProjectSnapshot::default()).unwrap();
    assert!(plan.changed_paths.is_empty());
    assert!(plan.target_settings_ids.is_empty());
    assert_eq!(plan.instance_deletes.len(), 1);
    assert_eq!(plan.instance_deletes[0].instances.len(), 1);
    assert_eq!(plan.instance_deletes[0].instances[0].settings_id, "folder");

    let mut changes = EditorChangeSet::default();
    changes.instance_changes.push(EditorInstanceChange {
        mode: "upsertInstances".to_string(),
        service: "ReplicatedStorage".to_string(),
        allow_deletes: false,
        instances: Vec::new(),
        preserve_instances: Vec::new(),
    });
    amend_reconciled_changes(&mut changes, plan).unwrap();
    assert_eq!(changes.instance_changes[0].mode, "deleteInstances");
    assert_eq!(changes.instance_changes[1].mode, "upsertInstances");

    let package_link = SettingsBytecodeInstance {
        settings_id: "link".to_string(),
        name: "PackageLink".to_string(),
        class_name: "PackageLink".to_string(),
        parent_index: Some(1),
        properties: Map::new(),
        attributes: Map::new(),
    };
    let studio = snapshot(vec![root.clone(), folder.clone(), package_link]);
    let plan = reconciliation_push_plan(&studio, &desired).unwrap();
    assert_eq!(plan.instance_deletes.len(), 1);
    assert_eq!(plan.instance_deletes[0].instances[0].settings_id, "folder");

    let package_link = SettingsBytecodeInstance {
        settings_id: "link".to_string(),
        name: "PackageLink".to_string(),
        class_name: "PackageLink".to_string(),
        parent_index: Some(1),
        properties: Map::new(),
        attributes: Map::new(),
    };
    let studio = snapshot(vec![root.clone(), folder.clone(), package_link]);
    let desired = snapshot(vec![root, folder]);
    assert!(reconciliation_push_plan(&studio, &desired).is_err());
}

#[test]
fn reconciliation_ignores_transient_instance_ids_and_script_guids() {
    let document =
        |root_id: &str, script_id: &str, script_guid: &str, value: &str| SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: vec![
                SettingsBytecodeInstance {
                    settings_id: root_id.to_string(),
                    name: "ReplicatedStorage".to_string(),
                    class_name: "ReplicatedStorage".to_string(),
                    parent_index: None,
                    properties: Map::new(),
                    attributes: Map::new(),
                },
                SettingsBytecodeInstance {
                    settings_id: script_id.to_string(),
                    name: "Module".to_string(),
                    class_name: "ModuleScript".to_string(),
                    parent_index: Some(0),
                    properties: Map::from_iter([
                        (
                            "ScriptGuid".to_string(),
                            Value::String(script_guid.to_string()),
                        ),
                        ("Value".to_string(), Value::String(value.to_string())),
                    ]),
                    attributes: Map::new(),
                },
            ],
        };
    let snapshot = |document: SettingsBytecode| ProjectSnapshot {
        entries: [(
            PathBuf::from("src/ReplicatedStorage/__roblox_sync_settings.renium"),
            SnapshotEntry::File(encode_settings_bytecode(&document).unwrap()),
        )]
        .into_iter()
        .collect(),
    };
    assert!(
        snapshots_equivalent(
            &snapshot(document("root-a", "script-a", "guid-a", "same")),
            &snapshot(document("root-b", "script-b", "guid-b", "same")),
        )
        .unwrap()
    );

    let baseline = snapshot(document("root-a", "script-a", "guid-a", "base"));
    let editor = snapshot(document("root-a", "script-a", "guid-a", "editor"));
    let studio = snapshot(document("root-b", "script-b", "guid-b", "base"));
    let (merged, conflicts) =
        merge_snapshots(Some(&baseline), &editor, &studio, ConflictPreference::None).unwrap();
    assert!(conflicts.is_empty());
    let settings = settings_document(merged.entries.values().next()).unwrap();
    assert_eq!(settings.instances.len(), 2);
    assert_eq!(
        settings.instances[1].properties.get("Value"),
        Some(&Value::String("editor".to_string()))
    );

    let mut editor = document("root-a", "script-a", "guid-a", "same");
    let mut studio = document("root-b", "script-b", "guid-b", "same");
    editor.instances[1].properties.extend([
        (
            "Position".to_string(),
            json!({"_type":"Vector3","x":0.10000000149011612,"y":2.0,"z":3.0}),
        ),
        (
            "WorldPosition".to_string(),
            json!({"_type":"Vector3","x":1.0,"y":2.0,"z":3.0}),
        ),
    ]);
    studio.instances[1].properties.extend([
        (
            "Position".to_string(),
            json!({"_type":"Vector3","x":0.10000000149011613,"y":2.0,"z":3.0}),
        ),
        (
            "WorldPosition".to_string(),
            json!({"_type":"Vector3","x":999.0,"y":2.0,"z":3.0}),
        ),
    ]);
    align_observation_ids_to_baseline(&editor, &mut studio);
    assert!(settings_documents_equivalent(&editor, &studio));

    let reference_document =
        |target_id: &str, holder_id: &str, reference_id: &str| SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: vec![
                SettingsBytecodeInstance {
                    settings_id: "root".to_string(),
                    name: "ReplicatedStorage".to_string(),
                    class_name: "ReplicatedStorage".to_string(),
                    parent_index: None,
                    properties: Map::new(),
                    attributes: Map::new(),
                },
                SettingsBytecodeInstance {
                    settings_id: target_id.to_string(),
                    name: "Target".to_string(),
                    class_name: "Attachment".to_string(),
                    parent_index: Some(0),
                    properties: Map::new(),
                    attributes: Map::new(),
                },
                SettingsBytecodeInstance {
                    settings_id: holder_id.to_string(),
                    name: "Holder".to_string(),
                    class_name: "WeldConstraint".to_string(),
                    parent_index: Some(0),
                    properties: Map::from_iter([(
                        "Attachment0".to_string(),
                        json!({"_type":"Ref","settingsId":reference_id}),
                    )]),
                    attributes: Map::new(),
                },
            ],
        };
    let baseline = reference_document("target", "holder", "target");
    let mut observed = reference_document("debug:target", "debug:holder", "debug:target");
    align_observation_ids_to_baseline(&baseline, &mut observed);
    assert_eq!(observed.instances[1].settings_id, "target");
    assert_eq!(observed.instances[2].settings_id, "holder");
    assert_eq!(
        observed.instances[2]
            .properties
            .get("Attachment0")
            .and_then(|value| value.get("settingsId")),
        Some(&Value::String("target".to_string()))
    );

    let duplicate = |id: &str, marker: &str| SettingsBytecodeInstance {
        settings_id: id.to_string(),
        name: "Duplicate".to_string(),
        class_name: "Folder".to_string(),
        parent_index: Some(0),
        properties: Map::new(),
        attributes: Map::from_iter([("Marker".to_string(), json!(marker))]),
    };
    let baseline = SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances: vec![
            SettingsBytecodeInstance {
                settings_id: "root".to_string(),
                name: "ReplicatedStorage".to_string(),
                class_name: "ReplicatedStorage".to_string(),
                parent_index: None,
                properties: Map::new(),
                attributes: Map::new(),
            },
            duplicate("a", "A"),
            duplicate("b", "B"),
        ],
    };
    let mut observed = SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances: vec![
            SettingsBytecodeInstance {
                settings_id: "observed-root".to_string(),
                name: "ReplicatedStorage".to_string(),
                class_name: "ReplicatedStorage".to_string(),
                parent_index: None,
                properties: Map::new(),
                attributes: Map::new(),
            },
            duplicate("observed-b", "B"),
        ],
    };
    align_observation_ids_to_baseline(&baseline, &mut observed);
    assert_eq!(observed.instances[1].settings_id, "b");

    let baseline = SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances: vec![
            SettingsBytecodeInstance {
                settings_id: "root".to_string(),
                name: "ReplicatedStorage".to_string(),
                class_name: "ReplicatedStorage".to_string(),
                parent_index: None,
                properties: Map::new(),
                attributes: Map::new(),
            },
            SettingsBytecodeInstance {
                settings_id: "debug:0_reserved".to_string(),
                name: "Existing".to_string(),
                class_name: "Folder".to_string(),
                parent_index: Some(0),
                properties: Map::new(),
                attributes: Map::new(),
            },
        ],
    };
    let mut observed = SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances: vec![
            SettingsBytecodeInstance {
                settings_id: "observed-root".to_string(),
                name: "ReplicatedStorage".to_string(),
                class_name: "ReplicatedStorage".to_string(),
                parent_index: None,
                properties: Map::new(),
                attributes: Map::new(),
            },
            SettingsBytecodeInstance {
                settings_id: "debug:0_reserved".to_string(),
                name: "New".to_string(),
                class_name: "Folder".to_string(),
                parent_index: Some(0),
                properties: Map::new(),
                attributes: Map::new(),
            },
            SettingsBytecodeInstance {
                settings_id: "observed-existing".to_string(),
                name: "Existing".to_string(),
                class_name: "Folder".to_string(),
                parent_index: Some(0),
                properties: Map::new(),
                attributes: Map::new(),
            },
        ],
    };
    align_observation_ids_to_baseline(&baseline, &mut observed);
    assert_eq!(observed.instances[2].settings_id, "debug:0_reserved");
    assert_ne!(observed.instances[1].settings_id, "debug:0_reserved");
    encode_settings_bytecode(&observed).unwrap();
}

#[test]
fn verify_mode_lists_the_paths_reconcile_would_move() {
    let push = HashSet::from([PathBuf::from("src/A.luau"), PathBuf::from("src/B.luau")]);
    let pull = HashSet::from([PathBuf::from("instances/Workspace.renium")]);
    let lines = super::coordinator::verify_difference_lines(&push, &pull);
    assert_eq!(lines[0], "Studio and project files differ");
    assert_eq!(
        &lines[1..],
        [
            "pull: instances/Workspace.renium",
            "push: src/A.luau",
            "push: src/B.luau"
        ]
    );
    let many = (0..70)
        .map(|index| PathBuf::from(format!("src/{index:03}.luau")))
        .collect::<HashSet<_>>();
    let lines = super::coordinator::verify_difference_lines(&many, &HashSet::new());
    assert_eq!(lines.len(), 62);
    assert_eq!(lines[61], "10 more paths");
}

mod twins_review;

mod twins_repro;

mod twins_subsets;

mod merge_rules;

mod conflict_records;

mod live_store_edits;

#[test]
fn a_file_identity_reused_for_another_class_is_a_new_instance_not_a_replacement() {
    let instance =
        |id: &str, name: &str, class_name: &str, parent: Option<usize>| SettingsBytecodeInstance {
            settings_id: id.to_string(),
            name: name.to_string(),
            class_name: class_name.to_string(),
            parent_index: parent,
            properties: Map::new(),
            attributes: Map::new(),
        };
    let settings_path = PathBuf::from("src/Workspace/__roblox_sync_settings.renium");
    let snapshot = |document: &SettingsBytecode| ProjectSnapshot {
        entries: [(
            settings_path.clone(),
            SnapshotEntry::File(encode_settings_bytecode(document).unwrap()),
        )]
        .into_iter()
        .collect(),
    };
    // The baseline and Studio agree: a Model holding a Part and a light.
    let baseline = SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances: vec![
            instance("ws", "Workspace", "Workspace", None),
            instance("garage", "Garage", "Folder", Some(0)),
            instance("a", "Model", "Model", Some(1)),
            instance("b", "Part", "Part", Some(2)),
            instance("c", "Light", "PointLight", Some(3)),
        ],
    };
    // The files replaced the subtree with a revised copy; its new instances
    // were numbered from the store's size and so reuse the old identities for
    // other classes.
    let revised = |id: &str, name: &str, class_name: &str, parent: Option<usize>| {
        let mut revised = instance(id, name, class_name, parent);
        revised.attributes = Map::from_iter([("Revision".to_string(), json!(2))]);
        revised
    };
    let editor = SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances: vec![
            instance("ws", "Workspace", "Workspace", None),
            instance("garage", "Garage", "Folder", Some(0)),
            revised("c", "Model", "Model", Some(1)),
            revised("a", "Part", "Part", Some(2)),
            revised("b", "Light", "PointLight", Some(3)),
        ],
    };
    let mut released = editor.clone();
    release_reassigned_editor_ids(&baseline, &mut released, &baseline, &HashSet::new());
    let released_ids = released
        .instances
        .iter()
        .map(|instance| instance.settings_id.as_str())
        .collect::<Vec<_>>();
    assert_eq!(&released_ids[..2], &["ws", "garage"]);
    assert!(
        released_ids[2..]
            .iter()
            .all(|id| !["a", "b", "c"].contains(id)),
        "{released_ids:?}"
    );
    assert_eq!(released_ids.iter().collect::<HashSet<_>>().len(), 5);

    let base = snapshot(&baseline);
    let (merged, conflicts, changes) = merge_snapshots_with_changes(
        Some(&base),
        &snapshot(&editor),
        &base,
        ConflictPreference::None,
        None,
    )
    .unwrap();
    assert!(conflicts.is_empty(), "{conflicts:?}");
    assert_eq!(changes.studio, HashSet::from([settings_path.clone()]));
    let merged_doc = settings_document(merged.entries.get(&settings_path)).unwrap();
    let classes = merged_doc
        .instances
        .iter()
        .map(|instance| (instance.name.as_str(), instance.class_name.as_str()))
        .collect::<Vec<_>>();
    assert_eq!(
        classes,
        vec![
            ("Workspace", "Workspace"),
            ("Garage", "Folder"),
            ("Model", "Model"),
            ("Part", "Part"),
            ("Light", "PointLight"),
        ]
    );
    // Studio keeps every instance in place; nothing is replaced by a stranger.
    let prepared = HashMap::from([(
        settings_path.clone(),
        PreparedEditorSettingsChange {
            previous: changes.aligned_studio[&settings_path].clone(),
            current: merged_doc,
        },
    )]);
    let plan = reconciliation_push_plan_for_paths_with_prepared_settings(
        &base,
        &merged,
        &changes.studio,
        &prepared,
        false,
        false,
    )
    .unwrap();
    assert!(
        plan.previous_class_names.is_empty(),
        "{:?}",
        plan.previous_class_names
    );
    assert!(
        plan.recreated_settings_ids.is_empty(),
        "{:?}",
        plan.recreated_settings_ids
    );
    assert_eq!(plan.target_settings_ids.len(), 3);
    assert_eq!(plan.in_place_instances.len(), 3);
}

#[test]
fn previous_paths_and_removals_carry_the_identity_studio_captured() {
    let instance =
        |id: &str, name: &str, class_name: &str, parent: Option<usize>| SettingsBytecodeInstance {
            settings_id: id.to_string(),
            name: name.to_string(),
            class_name: class_name.to_string(),
            parent_index: parent,
            properties: Map::new(),
            attributes: Map::new(),
        };
    let settings_path = PathBuf::from("src/Workspace/__roblox_sync_settings.renium");
    // Studio captured three same-named Parts it never identified, plus one it
    // had matched; the merge paired them to the files' identities and the
    // files drop the last one.
    let capture = SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances: vec![
            instance("ws", "Workspace", "Workspace", None),
            instance("debug:1_11", "Part", "Part", Some(0)),
            instance("debug:1_12", "Part", "Part", Some(0)),
            instance("part-c", "Part", "Part", Some(0)),
            instance("debug:1_14", "Part", "Part", Some(0)),
        ],
    };
    let aligned = SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances: vec![
            instance("ws", "Workspace", "Workspace", None),
            instance("part-b", "Part", "Part", Some(0)),
            instance("part-a", "Part", "Part", Some(0)),
            instance("part-c", "Part", "Part", Some(0)),
            instance("part-d", "Part", "Part", Some(0)),
        ],
    };
    let mut desired = aligned.clone();
    desired.instances.truncate(4);
    for part in &mut desired.instances[1..] {
        part.attributes = Map::from_iter([("Moved".to_string(), json!(true))]);
    }
    let studio = ProjectSnapshot {
        entries: [(
            settings_path.clone(),
            SnapshotEntry::File(encode_settings_bytecode(&capture).unwrap()),
        )]
        .into_iter()
        .collect(),
    };
    let merged = ProjectSnapshot {
        entries: [(
            settings_path.clone(),
            SnapshotEntry::File(encode_settings_bytecode(&desired).unwrap()),
        )]
        .into_iter()
        .collect(),
    };
    let prepared = HashMap::from([(
        settings_path.clone(),
        PreparedEditorSettingsChange {
            previous: aligned,
            current: desired,
        },
    )]);
    let mut plan = reconciliation_push_plan_for_paths_with_prepared_settings(
        &studio,
        &merged,
        &HashSet::from([settings_path.clone()]),
        &prepared,
        false,
        false,
    )
    .unwrap();
    attach_capture_identities(&mut plan, &studio, &prepared).unwrap();
    let identity = |id: &str| {
        plan.previous_identities
            .get(&("Workspace".to_string(), id.to_string()))
            .map(String::as_str)
    };
    assert_eq!(identity("part-a"), Some("debug:1_12"));
    assert_eq!(identity("part-b"), Some("debug:1_11"));
    assert_eq!(identity("part-c"), Some("part-c"));
    let removed = plan
        .instance_deletes
        .iter()
        .flat_map(|change| &change.instances)
        .find(|descriptor| descriptor.settings_id == "part-d")
        .expect("the dropped Part is removed");
    assert_eq!(removed.previous_settings_id.as_deref(), Some("debug:1_14"));

    let mut changes = EditorChangeSet::default();
    crate::editor::diff::push_editor_instance_change(
        &mut changes,
        "upsertInstances",
        "Workspace",
        false,
        vec![
            crate::editor::diff::editor_instance_descriptor_for_known_path(
                &prepared[&settings_path].current,
                2,
                vec!["Workspace".to_string(), "Part".to_string()],
                vec![1, 2],
            )
            .unwrap(),
        ],
    );
    amend_reconciled_changes(&mut changes, plan).unwrap();
    let upsert = changes
        .instance_changes
        .iter()
        .find(|change| change.mode == "upsertInstances")
        .unwrap();
    assert_eq!(
        upsert.instances[0].previous_settings_id.as_deref(),
        Some("debug:1_12")
    );
}

#[test]
fn a_union_whose_geometry_changed_is_removed_and_imported_anew() {
    let union = |id: &str, mesh: &str| SettingsBytecodeInstance {
        settings_id: id.to_string(),
        name: "Union".to_string(),
        class_name: "UnionOperation".to_string(),
        parent_index: Some(1),
        properties: Map::from_iter([
            (
                "MeshData2".to_string(),
                json!({"_type":"SharedString","base64":mesh}),
            ),
            ("Anchored".to_string(), json!(true)),
        ]),
        attributes: Map::new(),
    };
    let root = SettingsBytecodeInstance {
        settings_id: "ws".to_string(),
        name: "Workspace".to_string(),
        class_name: "Workspace".to_string(),
        parent_index: None,
        properties: Map::new(),
        attributes: Map::new(),
    };
    let settings_path = PathBuf::from("src/Workspace/__roblox_sync_settings.renium");
    let snapshot = |document: &SettingsBytecode| ProjectSnapshot {
        entries: [(
            settings_path.clone(),
            SnapshotEntry::File(encode_settings_bytecode(document).unwrap()),
        )]
        .into_iter()
        .collect(),
    };
    let model = SettingsBytecodeInstance {
        settings_id: "model".to_string(),
        name: "Model".to_string(),
        class_name: "Model".to_string(),
        parent_index: Some(0),
        properties: Map::new(),
        attributes: Map::new(),
    };
    let observed = SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances: vec![root.clone(), model.clone(), union("u", "b2xk")],
    };
    let desired = SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances: vec![root, model, union("u", "bmV3")],
    };
    let prepared = HashMap::from([(
        settings_path.clone(),
        PreparedEditorSettingsChange {
            previous: observed.clone(),
            current: desired.clone(),
        },
    )]);
    let plan = reconciliation_push_plan_for_paths_with_prepared_settings(
        &snapshot(&observed),
        &snapshot(&desired),
        &HashSet::from([settings_path.clone()]),
        &prepared,
        false,
        false,
    )
    .unwrap();
    assert!(plan.recreated_settings_ids.contains("u"));
    assert!(
        !plan
            .in_place_instances
            .contains(&("Workspace".to_string(), "u".to_string()))
    );
    assert!(
        !plan
            .previous_paths
            .contains_key(&("Workspace".to_string(), "u".to_string()))
    );
    let removed = plan
        .instance_deletes
        .iter()
        .flat_map(|change| &change.instances)
        .map(|descriptor| descriptor.settings_id.as_str())
        .collect::<Vec<_>>();
    assert_eq!(removed, vec!["u"]);

    // The same union with the same geometry but another anchoring is edited
    // in place.
    let mut moved = desired.clone();
    moved.instances[2].properties = observed.instances[2].properties.clone();
    moved.instances[2]
        .properties
        .insert("Anchored".to_string(), json!(false));
    let prepared = HashMap::from([(
        settings_path.clone(),
        PreparedEditorSettingsChange {
            previous: observed.clone(),
            current: moved.clone(),
        },
    )]);
    let plan = reconciliation_push_plan_for_paths_with_prepared_settings(
        &snapshot(&observed),
        &snapshot(&moved),
        &HashSet::from([settings_path.clone()]),
        &prepared,
        false,
        false,
    )
    .unwrap();
    assert!(plan.recreated_settings_ids.is_empty());
    assert!(plan.instance_deletes.is_empty());
    assert!(
        plan.in_place_instances
            .contains(&("Workspace".to_string(), "u".to_string()))
    );
}

#[test]
fn a_store_missing_from_the_files_is_a_conflict_not_a_deletion() {
    let store = PathBuf::from("instances/Workspace.renium");
    let mut document = twin_document();
    twin_add(&mut document, 0, "Thing", "Folder", json!({}));
    let studio = workspace_store_snapshot(document);
    let baseline = studio.clone();
    let editor = ProjectSnapshot::default();
    for preference in [ConflictPreference::None, ConflictPreference::Editor] {
        let (merged, conflicts) =
            merge_snapshots(Some(&baseline), &editor, &studio, preference).unwrap();
        assert_eq!(conflicts.len(), 1, "{conflicts:?}");
        assert!(
            conflicts[0]
                .detail
                .contains("instances/Workspace.renium is missing")
                && conflicts[0].detail.contains("`rbx pl`")
                && conflicts[0]
                    .detail
                    .contains("`rbx ps instances/Workspace.renium -u`"),
            "{}",
            conflicts[0]
        );
        assert!(merged.entries.get(&store) == studio.entries.get(&store));
    }
    let (merged, conflicts) = merge_snapshots(
        Some(&baseline),
        &editor,
        &studio,
        ConflictPreference::Studio,
    )
    .unwrap();
    assert!(conflicts.is_empty(), "{conflicts:?}");
    assert!(merged.entries.get(&store) == studio.entries.get(&store));
    let (merged, conflicts) =
        merge_snapshots(None, &editor, &studio, ConflictPreference::None).unwrap();
    assert!(
        conflicts.is_empty(),
        "a first pairing writes the store: {conflicts:?}"
    );
    assert!(merged.entries.get(&store) == studio.entries.get(&store));
}

#[test]
fn the_worker_refuses_a_vanished_store_and_names_an_undecodable_one() {
    let store = PathBuf::from("instances/Workspace.renium");
    let mut document = twin_document();
    twin_add(&mut document, 0, "Thing", "Folder", json!({}));
    let previous = workspace_store_snapshot(document);
    let scopes = [store.clone()];
    let message =
        match prepare_editor_settings_changes(&previous, &ProjectSnapshot::default(), &scopes) {
            Ok(_) => panic!("a vanished store was accepted"),
            Err(error) => format!("{error:#}"),
        };
    assert!(
        message.contains("instances/Workspace.renium is missing") && message.contains("`rbx pl`"),
        "{message}"
    );
    let garbage = file_snapshot(&[("instances/Workspace.renium", b"not a store")]);
    let message = match prepare_editor_settings_changes(&previous, &garbage, &scopes) {
        Ok(_) => panic!("an undecodable store was accepted"),
        Err(error) => format!("{error:#}"),
    };
    assert!(
        message.contains("Could not read the store instances/Workspace.renium")
            && message.contains("Invalid settings bytecode magic"),
        "{message}"
    );
}

/// The automation selection is process-wide; tests that select a project
/// take this lock so a concurrent one does not restore a deleted selection.
static PROJECT_SELECTION: Mutex<()> = Mutex::new(());

/// The comparison's packing as one call: the enforcement built for the
/// operation, then the fold.
fn pack_read_only_link_scripts(
    root: &Path,
    src_root: &Path,
    editor: &ProjectSnapshot,
    studio: &mut ProjectSnapshot,
) -> Result<()> {
    match packing_link_enforcement(root, src_root)? {
        Some(enforcement) => pack_read_only_link_scripts_with(&enforcement, root, editor, studio),
        None => Ok(()),
    }
}

#[test]
fn a_packed_link_script_compares_by_its_inline_text_against_the_capture() {
    let _serial = PROJECT_SELECTION.lock_recover();
    let root = crate::tests::support::temp_dir("packed-link-comparison");
    let _cleanup = OnDrop::new(|| {
        crate::project::storage::forget(&root);
        let _ = fs::remove_dir_all(&root);
    });
    crate::tests::support::write_packed_link_project(&root);
    config::load_project(Some(&root.join("renium.project.jsonc")), None).unwrap();
    let _selection = crate::app::context::select_automation_for_thread(
        None,
        root.join("renium.project.jsonc"),
        None,
    );
    let scopes = [PathBuf::from("instances"), PathBuf::from("src")];
    let editor = capture_snapshot(&root, &scopes).unwrap();
    let store = PathBuf::from("instances/ReplicatedStorage.renium");
    let script = PathBuf::from("src/ReplicatedStorage/Shared/Util.luau");
    let mut captured = settings_document(editor.entries.get(&store)).unwrap();
    captured.instances[2].settings_id = "studio:util".into();
    captured.instances[2].properties.insert(
        "Source".into(),
        json!(crate::settings::EXTERNAL_SOURCE_MARKER),
    );
    let mut studio = ProjectSnapshot::default();
    studio.entries.insert(
        store.clone(),
        SnapshotEntry::File(encode_settings_bytecode(&captured).unwrap()),
    );
    studio.entries.insert(
        PathBuf::from("src/ReplicatedStorage/Shared"),
        SnapshotEntry::Directory,
    );
    studio.entries.insert(
        script.clone(),
        SnapshotEntry::File(b"return { version = 1 }".to_vec()),
    );
    let (_, conflicts) = merge_snapshots(None, &editor, &studio, ConflictPreference::None).unwrap();
    assert!(
        conflicts
            .iter()
            .any(|conflict| conflict.detail.contains("ReplicatedStorage/Shared/Util")),
        "control: the raw capture conflicts on the packed script: {conflicts:?}"
    );

    pack_read_only_link_scripts(&root, &root.join("src"), &editor, &mut studio).unwrap();
    assert!(!studio.entries.contains_key(&script));
    let packed = settings_document(studio.entries.get(&store)).unwrap();
    assert_eq!(
        packed.instances[2].properties.get("Source"),
        Some(&json!("return { version = 1 }"))
    );
    let (merged, conflicts) =
        merge_snapshots(None, &editor, &studio, ConflictPreference::None).unwrap();
    assert!(conflicts.is_empty(), "{conflicts:?}");
    assert!(!merged.entries.contains_key(&script));

    let mut drifted = studio.clone();
    let mut drifted_doc = settings_document(drifted.entries.get(&store)).unwrap();
    drifted_doc.instances[2]
        .properties
        .insert("Source".into(), json!("return { version = 2 }"));
    drifted.entries.insert(
        store.clone(),
        SnapshotEntry::File(encode_settings_bytecode(&drifted_doc).unwrap()),
    );
    let (_, conflicts) =
        merge_snapshots(None, &editor, &drifted, ConflictPreference::None).unwrap();
    assert!(
        conflicts
            .iter()
            .any(|conflict| conflict.detail.contains("ReplicatedStorage/Shared/Util")),
        "a Studio edit of the packed text is still a difference: {conflicts:?}"
    );
}

#[test]
fn a_stripped_package_link_stays_in_studio_on_the_first_comparison() {
    let _serial = PROJECT_SELECTION.lock_recover();
    let root = crate::tests::support::temp_dir("stripped-link-comparison");
    let _cleanup = OnDrop::new(|| {
        crate::project::storage::forget(&root);
        let _ = fs::remove_dir_all(&root);
    });
    crate::tests::support::write_packed_link_project(&root);
    config::load_project(Some(&root.join("renium.project.jsonc")), None).unwrap();
    let _selection = crate::app::context::select_automation_for_thread(
        None,
        root.join("renium.project.jsonc"),
        None,
    );
    let scopes = [PathBuf::from("instances"), PathBuf::from("src")];
    let editor = capture_snapshot(&root, &scopes).unwrap();
    let store = PathBuf::from("instances/ReplicatedStorage.renium");
    let mut captured = settings_document(editor.entries.get(&store)).unwrap();
    captured
        .instances
        .push(crate::tests::support::settings_instance(
            "studio:link",
            "PackageLink",
            "PackageLink",
            Some(1),
        ));
    let mut studio = ProjectSnapshot::default();
    studio.entries.insert(
        store.clone(),
        SnapshotEntry::File(encode_settings_bytecode(&captured).unwrap()),
    );
    let links_in = |snapshot: &ProjectSnapshot| {
        settings_document(snapshot.entries.get(&store))
            .unwrap()
            .instances
            .iter()
            .filter(|instance| instance.class_name == "PackageLink")
            .count()
    };
    let (merged, _) = merge_snapshots(None, &editor, &studio, ConflictPreference::None).unwrap();
    assert_eq!(
        links_in(&merged),
        1,
        "control: the raw capture pulls the link into the store"
    );

    pack_read_only_link_scripts(&root, &root.join("src"), &editor, &mut studio).unwrap();
    assert_eq!(links_in(&studio), 0);
    let (merged, conflicts) =
        merge_snapshots(None, &editor, &studio, ConflictPreference::None).unwrap();
    assert!(conflicts.is_empty(), "{conflicts:?}");
    assert_eq!(links_in(&merged), 0);

    let package = root.join("links").join("shared.renium");
    let mut linked = SettingsBytecode::read_file(&package).unwrap();
    linked
        .instances
        .push(crate::tests::support::settings_instance(
            "editor:link",
            "PackageLink",
            "PackageLink",
            Some(0),
        ));
    linked.write_file(&package).unwrap();
    let mut kept = ProjectSnapshot::default();
    kept.entries.insert(
        store.clone(),
        SnapshotEntry::File(encode_settings_bytecode(&captured).unwrap()),
    );
    pack_read_only_link_scripts(&root, &root.join("src"), &editor, &mut kept).unwrap();
    assert_eq!(
        links_in(&kept),
        1,
        "a package that carries its own link keeps Studio's in the capture"
    );
}

#[test]
fn a_comparison_reports_a_link_package_it_cannot_enforce_instead_of_packing_nothing() {
    let _serial = PROJECT_SELECTION.lock_recover();
    let root = crate::tests::support::temp_dir("packed-link-package-error");
    let _cleanup = OnDrop::new(|| {
        crate::project::storage::forget(&root);
        let _ = fs::remove_dir_all(&root);
    });
    crate::tests::support::write_packed_link_project(&root);
    config::load_project(Some(&root.join("renium.project.jsonc")), None).unwrap();
    let _selection = crate::app::context::select_automation_for_thread(
        None,
        root.join("renium.project.jsonc"),
        None,
    );
    let scopes = [PathBuf::from("instances"), PathBuf::from("src")];
    let editor = capture_snapshot(&root, &scopes).unwrap();
    let package = root.join("links").join("shared.renium");
    let intact = fs::read(&package).unwrap();
    let pack = |studio: &mut ProjectSnapshot| {
        pack_read_only_link_scripts(&root, &root.join("src"), &editor, studio)
    };

    fs::write(&package, b"not a settings store").unwrap();
    let mut studio = editor.clone();
    let error = format!("{:#}", pack(&mut studio).unwrap_err());
    assert!(error.contains("shared.renium"), "{error}");
    assert!(
        studio.entries == editor.entries,
        "a failed enforcement leaves the capture alone"
    );

    fs::remove_file(&package).unwrap();
    let error = format!("{:#}", pack(&mut editor.clone()).unwrap_err());
    assert!(error.contains("shared"), "{error}");

    fs::write(&package, &intact).unwrap();
    fs::write(
        root.join("renium-link.json"),
        br#"{"version":1,"links":[{"id":"shared","readOnly":true,"source":{"type":"local","path":"links/shared.renium"},"targets":[{"service":"ReplicatedStorage","path":["ReplicatedStorage","Shared"]}]},{"id":"other","readOnly":true,"source":{"type":"local","path":"links/missing.renium"},"targets":[{"service":"ReplicatedStorage","path":["ReplicatedStorage","Other"]}]}],"broken":[]}"#,
    )
    .unwrap();
    let error = format!("{:#}", pack(&mut editor.clone()).unwrap_err());
    assert!(error.contains("missing"), "{error}");

    fs::remove_file(root.join("renium-link.json")).unwrap();
    let mut studio = editor.clone();
    pack(&mut studio).unwrap();
    assert!(
        studio.entries == editor.entries,
        "no manifest is no packed script"
    );
}

/// One service with these `(identity, name, class)` instances under its
/// root, and these script files.
fn scripts_snapshot(instances: &[(&str, &str, &str)], files: &[(&str, &str)]) -> ProjectSnapshot {
    let mut document = SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances: vec![SettingsBytecodeInstance::new(
            "root".into(),
            "ReplicatedStorage".into(),
            "ReplicatedStorage".into(),
            None,
        )],
    };
    for (id, name, class) in instances {
        document.instances.push(SettingsBytecodeInstance::new(
            (*id).into(),
            (*name).into(),
            (*class).into(),
            Some(0),
        ));
    }
    let mut snapshot = ProjectSnapshot::default();
    snapshot.entries.insert(
        PathBuf::from("src/ReplicatedStorage/__roblox_sync_settings.renium"),
        SnapshotEntry::File(encode_settings_bytecode(&document).unwrap()),
    );
    for (name, source) in files {
        snapshot.entries.insert(
            PathBuf::from("src/ReplicatedStorage").join(name),
            SnapshotEntry::File(source.as_bytes().to_vec()),
        );
    }
    snapshot
}

fn script_text(snapshot: &ProjectSnapshot, name: &str) -> Option<String> {
    match snapshot
        .entries
        .get(&PathBuf::from("src/ReplicatedStorage").join(name))
    {
        Some(SnapshotEntry::File(bytes)) => Some(String::from_utf8_lossy(bytes).into_owned()),
        _ => None,
    }
}

#[test]
fn an_edited_script_follows_the_rename_and_a_new_script_keeps_its_old_name() {
    let baseline = scripts_snapshot(
        &[("a", "A", "ModuleScript")],
        &[("A.luau", "return 'base'")],
    );
    let editor = scripts_snapshot(
        &[("a", "A", "ModuleScript")],
        &[("A.luau", "return 'edit'")],
    );
    let studio = scripts_snapshot(
        &[("a", "B", "ModuleScript"), ("n", "A", "ModuleScript")],
        &[("B.luau", "return 'base'"), ("A.luau", "return 'new'")],
    );
    for preference in [
        ConflictPreference::None,
        ConflictPreference::Editor,
        ConflictPreference::Studio,
    ] {
        let (merged, conflicts) =
            merge_snapshots(Some(&baseline), &editor, &studio, preference).unwrap();
        assert!(conflicts.is_empty(), "{conflicts:?}");
        assert_eq!(
            script_text(&merged, "B.luau").as_deref(),
            Some("return 'edit'")
        );
        assert_eq!(
            script_text(&merged, "A.luau").as_deref(),
            Some("return 'new'")
        );
    }
}

#[test]
fn both_sides_editing_a_script_one_side_renamed_is_a_conflict_at_the_files_path() {
    let baseline = scripts_snapshot(
        &[("a", "A", "ModuleScript")],
        &[("A.luau", "return 'base'")],
    );
    let editor = scripts_snapshot(
        &[("a", "A", "ModuleScript")],
        &[("A.luau", "return 'edit'")],
    );
    let studio = scripts_snapshot(
        &[("a", "B", "ModuleScript")],
        &[("B.luau", "return 'studio'")],
    );
    let (_, conflicts) =
        merge_snapshots(Some(&baseline), &editor, &studio, ConflictPreference::None).unwrap();
    assert_eq!(
        conflicts
            .iter()
            .map(|conflict| conflict.detail.as_str())
            .collect::<Vec<_>>(),
        ["src/ReplicatedStorage/A.luau changed on both sides"]
    );
    for (preference, text) in [
        (ConflictPreference::Editor, "return 'edit'"),
        (ConflictPreference::Studio, "return 'studio'"),
    ] {
        let (merged, conflicts) =
            merge_snapshots(Some(&baseline), &editor, &studio, preference).unwrap();
        assert!(conflicts.is_empty(), "{conflicts:?}");
        assert_eq!(script_text(&merged, "B.luau").as_deref(), Some(text));
        assert_eq!(script_text(&merged, "A.luau"), None);
    }
}

#[test]
fn a_script_kept_against_its_removal_does_not_take_the_text_of_the_one_that_reused_its_identity() {
    let baseline = scripts_snapshot(
        &[("editor:9", "Old", "ModuleScript")],
        &[("Old.luau", "return 'old'")],
    );
    let editor = scripts_snapshot(
        &[("editor:9", "New", "Script")],
        &[("New.server.luau", "print('new')")],
    );
    let mut studio = scripts_snapshot(
        &[("editor:9", "Old", "ModuleScript")],
        &[("Old.luau", "return 'old'")],
    );
    let store = PathBuf::from("src/ReplicatedStorage/__roblox_sync_settings.renium");
    let mut document = settings_document(studio.entries.get(&store)).unwrap();
    document.instances[1]
        .attributes
        .insert("Mark".into(), json!(1));
    studio.entries.insert(
        store,
        SnapshotEntry::File(encode_settings_bytecode(&document).unwrap()),
    );
    let (merged, conflicts) = merge_snapshots(
        Some(&baseline),
        &editor,
        &studio,
        ConflictPreference::Studio,
    )
    .unwrap();
    assert!(conflicts.is_empty(), "{conflicts:?}");
    assert_eq!(
        script_text(&merged, "Old.luau").as_deref(),
        Some("return 'old'")
    );
    assert_eq!(
        script_text(&merged, "New.server.luau").as_deref(),
        Some("print('new')")
    );
}

/// A pair that is remembered is answered from its bytes alone.
#[test]
fn a_remembered_pair_of_store_files_is_not_decoded_again() {
    let path = PathBuf::from("src/Workspace/__roblox_sync_settings.renium");
    let (left, right) = (
        b"not a store: left".to_vec(),
        b"not a store: right".to_vec(),
    );
    let digest = |bytes: &[u8]| <[u8; 32]>::from(Sha256::digest(bytes));
    super::verify::equivalent_stores()
        .lock_recover()
        .insert((digest(&left), digest(&right)));
    assert!(
        snapshot_entry_equivalent(
            &path,
            Some(&SnapshotEntry::File(left)),
            Some(&SnapshotEntry::File(right)),
            None
        )
        .unwrap()
    );
}

/// Two store files found equivalent are remembered by their bytes, and two
/// that differ are compared again every time.
#[test]
fn equivalent_store_files_are_remembered_by_their_bytes() {
    let path = PathBuf::from("src/Workspace/__roblox_sync_settings.renium");
    let store = |id: &str, transparency: f64| {
        let mut part = SettingsBytecodeInstance::new(
            id.to_string(),
            "Part".to_string(),
            "Part".to_string(),
            Some(0),
        );
        part.properties
            .insert("Transparency".to_string(), json!(transparency));
        let document = SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: vec![
                SettingsBytecodeInstance::new(
                    "root".to_string(),
                    "Workspace".to_string(),
                    "Workspace".to_string(),
                    None,
                ),
                part,
            ],
        };
        encode_settings_bytecode(&document).unwrap()
    };
    let digest = |bytes: &[u8]| <[u8; 32]>::from(Sha256::digest(bytes));
    let (files, studio, changed) = (store("a", 0.5), store("b", 0.5), store("b", 0.25));
    assert!(files != studio);
    let equivalent = |left: &[u8], right: &[u8]| {
        snapshot_entry_equivalent(
            &path,
            Some(&SnapshotEntry::File(left.to_vec())),
            Some(&SnapshotEntry::File(right.to_vec())),
            None,
        )
        .unwrap()
    };
    let remembered = |left: &[u8], right: &[u8]| {
        super::verify::equivalent_stores()
            .lock_recover()
            .contains(&(digest(left), digest(right)))
    };
    assert!(equivalent(&files, &studio));
    assert!(remembered(&files, &studio));
    assert!(equivalent(&files, &studio));
    assert!(!equivalent(&files, &changed));
    assert!(!remembered(&files, &changed));
}

/// A store for `a_store_prepared_from_its_changed_instances_is_pushed_as_the_whole_documents_push_it`:
/// a Workspace with a camera and parts of a few classes, some of them
/// namesakes, most with a UniqueId, some holding a shared payload, a
/// reference or an attribute.
fn changed_instances_store(rng: &mut crate::tests::support::SimRng) -> SettingsBytecode {
    let mut root =
        SettingsBytecodeInstance::new("1".into(), "Workspace".into(), "Workspace".into(), None);
    root.properties.insert("Gravity".into(), json!(196.2));
    let with_camera = rng.below(4) != 0;
    let mut instances = vec![root];
    if with_camera {
        instances[0].properties.insert(
            "CurrentCamera".into(),
            json!({"_type": "Ref", "instanceIndex": 2}),
        );
        let mut camera = SettingsBytecodeInstance::new(
            "debug:0_9".into(),
            "Camera".into(),
            "Camera".into(),
            Some(0),
        );
        camera.properties.insert("FieldOfView".into(), json!(70.0));
        instances.push(camera);
    }
    let count = 4 + rng.below(20);
    for index in 0..count {
        let position = instances.len();
        let class = ["Part", "MeshPart", "Folder", "Model", "UnionOperation"][rng.below(5)];
        let name = if rng.below(4) == 0 {
            "Twin".to_string()
        } else {
            format!("Item{index}")
        };
        // An instance without a UniqueId mostly has an id the store gave it,
        // which tells it as well.
        let unique = rng.below(8) != 0;
        let id = if rng.below(3) == 0 || !unique && rng.below(12) != 0 {
            format!("item-{index}")
        } else {
            format!("debug:0_{}", 100 + index)
        };
        let parent = if position > 2 && rng.below(3) == 0 {
            1 + usize::from(with_camera) + rng.below(position - 1 - usize::from(with_camera))
        } else {
            0
        };
        let mut instance = SettingsBytecodeInstance::new(id, name, class.into(), Some(parent));
        if unique {
            instance.properties.insert(
                "UniqueId".into(),
                json!({"_type": "UniqueId", "value": format!("{:024x}{:08x}", 7, index + 1)}),
            );
        }
        if class != "Folder" && class != "Model" {
            instance
                .properties
                .insert("Transparency".into(), json!(rng.below(4) as f64 / 4.0));
            instance
                .properties
                .insert("Anchored".into(), json!(rng.below(2) == 0));
            instance.properties.insert(
                "Size".into(),
                json!({"_type": "Vector3", "x": 1.0 + rng.below(3) as f64, "y": 1.0, "z": 2.0}),
            );
            instance.properties.insert(
                "Material".into(),
                json!({"_type": "EnumItem", "name": (["Plastic", "Wood", "Neon"][rng.below(3)])}),
            );
        }
        if class == "UnionOperation" {
            for name in ["MeshData2", "ChildData2"] {
                instance.properties.insert(
                    name.into(),
                    json!({"_type": "SharedString", "base64": base64::encode(format!("{name}-{}", rng.below(3)))}),
                );
            }
        }
        if class == "MeshPart" || class == "UnionOperation" {
            instance.properties.insert(
                "PhysicalConfigData".into(),
                json!({"_type": "SharedString", "base64": base64::encode(format!("CSGPHS-hull-{}", rng.below(3)))}),
            );
            instance.properties.insert(
                "MeshId".into(),
                json!(format!("rbxassetid://{}", 100 + rng.below(3))),
            );
        }
        if position > 1 && rng.below(5) == 0 {
            instance.properties.insert(
                "Target".into(),
                json!({"_type": "Ref", "instanceIndex": 1 + rng.below(position)}),
            );
        }
        // A reference by an id and a path, as a capture gives one it could
        // not place.
        if position > 2 && rng.below(8) == 0 {
            let target = &instances[1 + rng.below(position - 1)];
            instance.properties.insert(
                "Pointer".into(),
                json!({
                    "_type": "Ref",
                    "settingsId": target.settings_id,
                    "pathSegments": ["Workspace", target.name],
                    "pathOrdinals": [1, 1],
                }),
            );
        }
        if rng.below(4) == 0 {
            instance
                .attributes
                .insert("Level".into(), json!(rng.below(5)));
        }
        if rng.below(6) == 0 {
            instance.properties.insert(
                "HistoryId".into(),
                json!(format!("history-{}", rng.below(9))),
            );
        }
        instances.push(instance);
    }
    SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances,
    }
}

fn plan_text(plan: &ReconcilePushPlan) -> String {
    fn sorted(items: impl Iterator<Item = String>) -> String {
        let mut items = items.collect::<Vec<_>>();
        items.sort();
        items.join(" | ")
    }
    let pair = |(service, id): &(String, String)| format!("{service}/{id}");
    [
        sorted(plan.initial_native_services.iter().cloned()),
        sorted(
            plan.changed_paths
                .iter()
                .map(|path| path.display().to_string()),
        ),
        plan.target_settings_ids.join(" | "),
        sorted(plan.recreated_settings_ids.iter().cloned()),
        sorted(plan.recreated_instances.iter().map(pair)),
        sorted(
            plan.previous_class_names
                .iter()
                .map(|(key, class)| format!("{}={class}", pair(key))),
        ),
        sorted(plan.previous_paths.iter().map(|(key, path)| {
            format!(
                "{}={:?}{:?}",
                pair(key),
                path.path_segments,
                path.path_ordinals
            )
        })),
        sorted(
            plan.previous_identities
                .iter()
                .map(|(key, id)| format!("{}={id}", pair(key))),
        ),
        // The planner lists the instances of one removal in no fixed order.
        sorted(plan.instance_deletes.iter().map(|change| {
            let mut change = serde_json::to_value(change).unwrap();
            if let Some(instances) = change["instances"].as_array_mut() {
                instances.sort_by_key(Value::to_string);
            }
            change.to_string()
        })),
        sorted(
            plan.property_removals
                .iter()
                .map(|change| serde_json::to_string(change).unwrap()),
        ),
        sorted(
            plan.geometry_properties
                .iter()
                .map(|(key, names)| format!("{}={names:?}", pair(key))),
        ),
        sorted(plan.attribute_only_instances.iter().map(pair)),
        sorted(plan.in_place_instances.iter().map(pair)),
        sorted(
            plan.unchanged_native_root_properties
                .iter()
                .map(|(key, names)| {
                    let mut names = names.clone();
                    names.sort();
                    format!("{}={names:?}", pair(key))
                }),
        ),
    ]
    .join("\n")
}

/// What the write step sends for a change set, as text.
fn request_text(changes: &crate::editor::types::EditorChangeSet) -> Result<String, String> {
    let mut lists = [
        serde_json::to_value(&changes.source_changes),
        serde_json::to_value(&changes.instance_changes),
        serde_json::to_value(&changes.property_changes),
    ]
    .map(|list| list.map_err(|error| error.to_string()));
    let mut out = Vec::new();
    for list in &mut lists {
        let list = list.as_mut().map_err(|error| error.clone())?;
        if let Some(items) = list.as_array_mut() {
            // The planner lists the instances of one removal in no fixed
            // order.
            for item in items.iter_mut() {
                if let Some(instances) = item["instances"].as_array_mut() {
                    instances.sort_by_key(Value::to_string);
                }
            }
            items.sort_by_key(Value::to_string);
        }
        // A request states payloads, never a handle or an instance that
        // stands for another.
        out.push(
            serde_json::to_string(&crate::settings::held::AsPayloads(list))
                .map_err(|error| format!("request: {error}"))?,
        );
    }
    // A store the push writes is written with every instance.
    let written = changes
        .settings_writes
        .iter()
        .map(|write| {
            (
                write.document.instances.len(),
                crate::settings::equivalence::holds_unread_instances(&write.document),
            )
        })
        .collect::<Vec<_>>();
    Ok(format!(
        "{}\nwhole instances {:?}\nwritten {written:?}",
        out.join("\n"),
        changes
            .whole_instances
            .iter()
            .map(|(service, path)| format!(
                "{service}{:?}{:?}",
                path.path_segments, path.path_ordinals
            ))
            .collect::<Vec<_>>()
    ))
}

/// One change of a store, most of them of a value of one instance.
fn change_an_instance(rng: &mut crate::tests::support::SimRng, document: &mut SettingsBytecode) {
    let count = document.instances.len();
    let index = rng.below(count);
    // Two instances trade their UniqueIds: each is now told for the other.
    if rng.below(12) == 0 {
        let other = rng.below(count);
        let take = |document: &mut SettingsBytecode, at: usize| {
            document.instances[at].properties.remove("UniqueId")
        };
        let (first, second) = (take(document, index), take(document, other));
        for (at, unique_id) in [(index, second), (other, first)] {
            if let Some(unique_id) = unique_id {
                document.instances[at]
                    .properties
                    .insert("UniqueId".into(), unique_id);
            }
        }
        return;
    }
    let instance = &mut document.instances[index];
    match rng.below(15) {
        // The mesh of a union is cut anew.
        14 => {
            if instance.class_name == "UnionOperation" {
                instance.properties.insert(
                    "MeshData2".into(),
                    json!({"_type": "SharedString", "base64": base64::encode(format!("recut-{}", rng.below(3)))}),
                );
            }
        }
        0 | 1 => {
            instance
                .properties
                .insert("Transparency".into(), json!(rng.below(8) as f64 / 8.0));
        }
        2 => {
            instance.properties.insert(
                "Size".into(),
                json!({"_type": "Vector3", "x": 4.0 + rng.below(3) as f64, "y": 1.0, "z": 2.0}),
            );
        }
        3 => {
            instance.properties.insert(
                "PhysicalConfigData".into(),
                json!({"_type": "SharedString", "base64": base64::encode(format!("CSGPHS-other-{}", rng.below(3)))}),
            );
        }
        4 => {
            instance.properties.remove("Anchored");
        }
        5 => {
            instance
                .attributes
                .insert("Level".into(), json!(rng.below(9)));
        }
        6 => {
            instance.attributes.remove("Level");
        }
        7 => {
            instance.properties.insert(
                "Target".into(),
                json!({"_type": "Ref", "instanceIndex": 1 + rng.below(count)}),
            );
        }
        8 => {
            instance.properties.insert(
                "UniqueId".into(),
                json!({"_type": "UniqueId", "value": format!("{:024x}{:08x}", 9, rng.below(40) + 1)}),
            );
        }
        // What the engine alone sets.
        9 => {
            instance.properties.insert(
                "HistoryId".into(),
                json!(format!("history-{}", rng.below(9))),
            );
        }
        10 => {
            instance
                .properties
                .insert("Gravity".into(), json!(50.0 + rng.below(3) as f64));
        }
        11 => {
            instance.properties.insert(
                "Material".into(),
                json!({"_type": "EnumItem", "name": (["Plastic", "Wood", "Neon"][rng.below(3)])}),
            );
        }
        // Another instance, or one less: the two copies no longer stand
        // instance for instance.
        12 => {
            if index > 0 {
                instance.name = format!("Renamed{}", rng.below(3));
            }
        }
        _ => {
            if rng.below(3) == 0 {
                let parent = rng.below(count);
                document.instances.push(SettingsBytecodeInstance::new(
                    format!("added-{count}"),
                    "Added".into(),
                    "Folder".into(),
                    Some(parent),
                ));
            }
        }
    }
}

/// Every parent and every reference by position of a store at the place
/// `placed` gives its instance.
fn place_instances(document: &mut SettingsBytecode, placed: &dyn Fn(usize) -> Option<usize>) {
    fn place(value: &mut Value, placed: &dyn Fn(usize) -> Option<usize>) {
        match value {
            Value::Object(map) => {
                if let Some(index) = map.get("instanceIndex").and_then(Value::as_u64) {
                    let index = (index as usize).checked_sub(1).and_then(placed);
                    map.insert(
                        "instanceIndex".into(),
                        json!(index.map_or(0, |index| index + 1)),
                    );
                }
                map.values_mut().for_each(|value| place(value, placed));
            }
            Value::Array(values) => values.iter_mut().for_each(|value| place(value, placed)),
            _ => {}
        }
    }
    for instance in &mut document.instances {
        instance.parent_index = instance.parent_index.and_then(placed);
        for value in instance
            .properties
            .values_mut()
            .chain(instance.attributes.values_mut())
        {
            place(value, placed);
        }
    }
}

/// A leaf goes to the end of its store, where a store edit writes an
/// instance it adds: Studio holds that one among the others, as the last
/// child of its parent.
fn move_a_leaf_to_the_end(
    rng: &mut crate::tests::support::SimRng,
    document: &mut SettingsBytecode,
) {
    let count = document.instances.len();
    let leaves = (1..count)
        .filter(|index| {
            !document
                .instances
                .iter()
                .any(|instance| instance.parent_index == Some(*index))
        })
        .collect::<Vec<_>>();
    if leaves.is_empty() {
        return;
    }
    let moved = leaves[rng.below(leaves.len())];
    let instance = document.instances.remove(moved);
    document.instances.push(instance);
    place_instances(document, &|index| {
        Some(match index.cmp(&moved) {
            std::cmp::Ordering::Less => index,
            std::cmp::Ordering::Equal => count - 1,
            std::cmp::Ordering::Greater => index - 1,
        })
    });
}

/// One run of instances more in a store, or one less, as a store edit
/// leaves it: an instance somewhere among the others, at times with a child
/// and at times the namesake of another, or a leaf gone. Every reference by
/// position follows the instances.
fn add_or_remove_a_run(rng: &mut crate::tests::support::SimRng, document: &mut SettingsBytecode) {
    let place_all = place_instances;
    let count = document.instances.len();
    if rng.below(2) == 0 {
        let leaves = (1..count)
            .filter(|index| {
                !document
                    .instances
                    .iter()
                    .any(|instance| instance.parent_index == Some(*index))
            })
            .collect::<Vec<_>>();
        if leaves.is_empty() {
            return;
        }
        // Half the time the first of two a place alone cannot tell apart.
        let alike = |first: usize, second: usize| {
            let (first, second) = (&document.instances[first], &document.instances[second]);
            first.name == second.name
                && first.class_name == second.class_name
                && first.parent_index == second.parent_index
        };
        let firsts = leaves
            .iter()
            .copied()
            .filter(|leaf| (leaf + 1..count).any(|other| alike(*leaf, other)))
            .collect::<Vec<_>>();
        let gone = if !firsts.is_empty() && rng.below(2) == 0 {
            firsts[rng.below(firsts.len())]
        } else {
            leaves[rng.below(leaves.len())]
        };
        document.instances.remove(gone);
        place_all(document, &|index| match index.cmp(&gone) {
            std::cmp::Ordering::Less => Some(index),
            std::cmp::Ordering::Equal => None,
            std::cmp::Ordering::Greater => Some(index - 1),
        });
        return;
    }
    // Half the time the namesake of another instance under the same parent
    // and of its class: right ahead of it, or anywhere under that parent.
    let like_at = rng.below(count);
    let like = &document.instances[like_at];
    let twin = rng.below(2) == 0;
    let (name, class, parent, at) = match like.parent_index.filter(|_| twin) {
        Some(parent) => (
            like.name.clone(),
            like.class_name.to_string(),
            parent,
            if rng.below(2) == 0 {
                like_at
            } else {
                parent + 1 + rng.below(count - parent)
            },
        ),
        None => {
            let at = 1 + rng.below(count);
            ("Added".to_string(), "Part".to_string(), rng.below(at), at)
        }
    };
    let with_child = rng.below(3) == 0;
    let by = 1 + usize::from(with_child);
    place_all(document, &|index| {
        Some(if index >= at { index + by } else { index })
    });
    let mut added = SettingsBytecodeInstance::new(
        format!("editor:{count}"),
        name,
        class.as_str().into(),
        Some(parent),
    );
    if class != "Folder" && class != "Model" {
        added
            .properties
            .insert("Transparency".into(), json!(rng.below(8) as f64 / 8.0));
    }
    document.instances.insert(at, added);
    if with_child {
        document.instances.insert(
            at + 1,
            SettingsBytecodeInstance::new(
                format!("editor:{}", count + 1),
                "Under".into(),
                "Folder".into(),
                Some(at),
            ),
        );
    }
}

/// Script files: of each its name, the files' text and Studio's.
type ScriptFiles = Vec<(String, Option<String>, Option<String>)>;

/// The files' texts of `scripts`, written beside `store`.
fn write_script_files(store: &Path, scripts: &ScriptFiles) -> Result<(), String> {
    for (name, text, _) in scripts {
        if let Some(text) = text {
            std::fs::write(store.with_file_name(name), text).map_err(|error| error.to_string())?;
        }
    }
    Ok(())
}

/// Scripts under the roots of two copies of a store, their sources in files:
/// each with the text Studio holds or another, held by the files alone, with
/// its file gone, a file the store knows nothing of, or a file renamed to
/// run where another kind of script does. Some run on the client or as a
/// plugin, which their file names say. Gives of each file its name, the
/// files' text and Studio's, and whether the files alone hold one of the
/// instances.
fn scripts_beside(
    seed: u64,
    files: &mut SettingsBytecode,
    studio: &mut SettingsBytecode,
) -> (ScriptFiles, bool) {
    let mut rng = crate::tests::support::SimRng(seed.wrapping_mul(0x2545_f491_4f6c_dd1d) | 1);
    let (mut scripts, mut uneven) = (Vec::new(), false);
    let count = if seed.is_multiple_of(2) {
        1 + rng.below(4)
    } else {
        0
    };
    for index in 0..count {
        let (class, context, suffix) = [
            ("Script", None, ".server.luau"),
            ("ModuleScript", None, ".luau"),
            ("LocalScript", None, ".client.luau"),
            ("Script", Some("Client"), ".client.luau"),
            ("Script", Some("Plugin"), ".plugin.luau"),
        ][rng.below(5)];
        let name = format!("Code{index}");
        let script = || {
            let mut script = SettingsBytecodeInstance::new(
                format!("code-{index}"),
                name.clone(),
                class.into(),
                Some(0),
            );
            if let Some(context) = context {
                script.properties.insert(
                    "RunContext".into(),
                    crate::editor::paths::editor_run_context_value(context),
                );
            }
            script
        };
        let kind = rng.below(7);
        // The file of a script that runs elsewhere, named as a server
        // script's: Studio holds it under its own name.
        if kind == 6 && context.is_some() {
            files.instances.push(script());
            studio.instances.push(script());
            scripts.push((
                format!("{name}.server.luau"),
                Some("return 1\n".to_string()),
                None,
            ));
            scripts.push((
                format!("{name}{suffix}"),
                None,
                Some("return 1\n".to_string()),
            ));
            continue;
        }
        let (in_files, in_studio) = (kind != 4, kind != 2 && kind != 4);
        uneven |= kind == 2;
        if in_files {
            files.instances.push(script());
        }
        if in_studio {
            studio.instances.push(script());
        }
        scripts.push((
            format!("{name}{suffix}"),
            (kind != 3).then(|| format!("return {}\n", 1 + usize::from(kind == 1))),
            in_studio.then(|| "return 1\n".to_string()),
        ));
    }
    (scripts, uneven)
}

/// A store whose two copies differ in a few instances is prepared from
/// those instances alone, without the whole document of the copy Studio
/// holds. What a push makes of it is what it makes of the two whole
/// documents: the same stores unchanged, the same plan, the same saved-field
/// differences and the same verdict on what Studio holds afterwards.
/// Gives the namesakes of a class under one parent what the first of them
/// holds, but for its UniqueId.
fn twins_alike(document: &mut SettingsBytecode) {
    let mut first = HashMap::new();
    for index in 0..document.instances.len() {
        let instance = &document.instances[index];
        if instance.name != "Twin" {
            continue;
        }
        let key = (instance.parent_index, instance.class_name.clone());
        let Some(&model) = first.get(&key) else {
            first.insert(key, index);
            continue;
        };
        let (mut properties, attributes) = {
            let model: &SettingsBytecodeInstance = &document.instances[model];
            (model.properties.clone(), model.attributes.clone())
        };
        let instance = &mut document.instances[index];
        match instance.properties.remove("UniqueId") {
            Some(unique) => properties.insert("UniqueId".into(), unique),
            None => properties.remove("UniqueId"),
        };
        instance.properties = properties;
        instance.attributes = attributes;
    }
}

/// Namesakes under the root whose own values are alike, told apart by their
/// children: Studio gave the child of its one the files' other value, and
/// the files added a folder and a namesake that holds the child Studio's
/// does, a run beside the namesake Studio holds unchanged.
fn namesakes_told_by_their_children(studio: &mut SettingsBytecode, files: &mut SettingsBytecode) {
    let part = |id: &str, name: &str, parent: usize, material: &str| {
        let mut part =
            SettingsBytecodeInstance::new(id.into(), name.into(), "Part".into(), Some(parent));
        part.properties.insert(
            "Material".into(),
            json!({"_type": "EnumItem", "name": material}),
        );
        part
    };
    let folder = |id: &str, name: &str| {
        SettingsBytecodeInstance::new(id.into(), name.into(), "Folder".into(), Some(0))
    };
    let at = studio.instances.len();
    studio.instances.extend([
        part("debug:0_801", "Namesake", 0, "Wood"),
        part("debug:0_802", "Under", at, "Neon"),
        folder("debug:0_803", "Beside"),
    ]);
    let at = files.instances.len();
    files.instances.extend([
        part("debug:0_801", "Namesake", 0, "Wood"),
        part("debug:0_802", "Under", at, "Wood"),
        folder("debug:0_803", "Beside"),
        folder("editor:801", "Added"),
        part("editor:802", "Namesake", 0, "Wood"),
        part("editor:803", "Under", at + 4, "Neon"),
    ]);
}

/// What came between two copies of a store since the files were written:
/// Studio's ids, the files' edits. Answers whether Studio opened the place
/// again, and whether a push imported the service.
fn copies_apart(
    seed: u64,
    rng: &mut crate::tests::support::SimRng,
    studio: &mut SettingsBytecode,
    files: &mut SettingsBytecode,
) -> (bool, bool) {
    // Studio opened the place again since the files were written: it
    // gave every instance another id of its own and another UniqueId.
    let reopened = rng.below(3) == 0;
    if reopened {
        for (index, instance) in studio.instances.iter_mut().enumerate() {
            // Studio numbers anew from where it did before, so an id of
            // its own can be the one the files hold for another instance.
            if instance.settings_id.starts_with("debug:") {
                instance.settings_id = if seed.is_multiple_of(2) {
                    format!("debug:0_{}", 500 + index)
                } else {
                    format!("debug:0_{}", 100 + index)
                };
            }
            if instance.properties.contains_key("UniqueId") {
                instance.properties.insert(
                    "UniqueId".into(),
                    json!({"_type": "UniqueId", "value": format!("{:024x}{:08x}", 5, index + 1)}),
                );
            }
        }
    }
    // A push imported the service since: every instance of Studio's is
    // another object, under an id of Studio's own and another UniqueId.
    let imported = seed % 7 == 3;
    if imported {
        for (index, instance) in studio.instances.iter_mut().enumerate().skip(1) {
            instance.settings_id = format!("debug:1_{}", 900 + index);
            if instance.properties.contains_key("UniqueId") {
                instance.properties.insert(
                    "UniqueId".into(),
                    json!({"_type": "UniqueId", "value": format!("{:024x}{:08x}", 9, index + 1)}),
                );
            }
        }
    }
    let changes = if imported {
        rng.below(2)
    } else {
        rng.below(5) + usize::from(!seed.is_multiple_of(16))
    };
    for _ in 0..changes {
        change_an_instance(rng, files);
    }
    // A store edit added an instance to the files, or took one away.
    if seed.is_multiple_of(3) {
        add_or_remove_a_run(rng, files);
    }
    // The files hold an instance at the end that Studio holds among
    // the others.
    if (seed % 5 == 1 || imported) && !seed.is_multiple_of(3) {
        move_a_leaf_to_the_end(rng, files);
    }
    (reopened, imported)
}

#[test]
fn a_store_prepared_from_its_changed_instances_is_pushed_as_the_whole_documents_push_it() {
    use crate::tests::support::{SimRng, run_seeds};
    use std::sync::atomic::{AtomicUsize, Ordering};

    let (from_changed, whole, planned, unchanged_stores) = (
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
    );
    let under_other_ids = AtomicUsize::new(0);
    let from_both = AtomicUsize::new(0);
    let sent_from_both = AtomicUsize::new(0);
    let from_blocks_of_both = AtomicUsize::new(0);
    let runs = AtomicUsize::new(0);
    let runs_sent_from_both = AtomicUsize::new(0);
    let compared_by_changed = AtomicUsize::new(0);
    let (with_scripts, sources_sent) = (AtomicUsize::new(0), AtomicUsize::new(0));
    let told_alone_before = super::replacement::TOLD_ALONE.load(Ordering::Relaxed);
    // The copies of each store this test prepares (`copies_key`).
    let copies = std::sync::Mutex::new(HashSet::new());
    let paired_first_before = super::replacement::PAIRED_FIRST.load(Ordering::Relaxed);
    let by_order_before = super::replacement::BY_ORDER
        .each_ref()
        .map(|count| count.load(Ordering::Relaxed));
    run_seeds(300, |seed, _| {
        use crate::settings::bytecode::blocks::sim::{blocks_of_both, in_blocks};
        let mut rng = SimRng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
        // Stores of several blocks: a block both copies hold byte for byte
        // is not read row by row.
        let block_rows = [1, 3, 8, 4096][(seed >> 2) as usize % 4];
        let mut studio = changed_instances_store(&mut rng);
        // Namesakes that hold the same, as copies of one model do.
        if seed % 14 == 3 {
            twins_alike(&mut studio);
        }
        let mut files = studio.clone();
        let (reopened, imported) = copies_apart(seed, &mut rng, &mut studio, &mut files);
        if seed % 13 == 6 && files.instances.len() == studio.instances.len() {
            namesakes_told_by_their_children(&mut studio, &mut files);
        }
        let run = files.instances.len() != studio.instances.len();
        // Scripts beside them, their sources in files: with the text Studio
        // holds or another, held by the files alone, with its file gone, or
        // a file the store knows nothing of. Each is its file name, the
        // files' text and Studio's.
        let (scripts, uneven) = scripts_beside(seed, &mut files, &mut studio);
        let scripts_differ = scripts.iter().any(|(_, files, studio)| files != studio);
        // One run of instances apart, with no script the files alone hold
        // beside it.
        let run = run && !uneven;
        // After an import a run is told only where no namesake stands by it.
        if run && !imported {
            runs.fetch_add(1, Ordering::Relaxed);
        }
        let path = PathBuf::from("src/Workspace/__roblox_sync_settings.renium");
        let snapshot = |document: &SettingsBytecode, of_files: bool| {
            let mut entries = BTreeMap::from([(
                path.clone(),
                SnapshotEntry::File(
                    in_blocks(document, block_rows).map_err(|error| format!("{error:#}"))?,
                ),
            )]);
            for (name, files, studio) in &scripts {
                if let Some(text) = if of_files { files } else { studio } {
                    entries.insert(
                        PathBuf::from("src/Workspace").join(name),
                        SnapshotEntry::File(text.clone().into_bytes()),
                    );
                }
            }
            Ok::<_, String>(ProjectSnapshot { entries })
        };
        let (Ok(desired), Ok(observed)) = (snapshot(&files, true), snapshot(&studio, false)) else {
            return Ok(());
        };
        let held_by_both = match (desired.entries.get(&path), observed.entries.get(&path)) {
            (Some(SnapshotEntry::File(files)), Some(SnapshotEntry::File(studio))) => {
                let key = super::replacement::copies_key(files, studio);
                copies.lock().unwrap().insert(key);
                blocks_of_both(files, studio)
            }
            _ => 0,
        };
        let paths = project_replacement_paths(&desired, &observed);
        // What the write step sends to Studio for a plan, made from the
        // documents the push prepared.
        let project_root = crate::tests::support::temp_dir(&format!("changed-instances-{seed}"));
        let store = project_root.join(&path);
        std::fs::create_dir_all(store.parent().unwrap()).map_err(|error| error.to_string())?;
        if let Some(SnapshotEntry::File(bytes)) = desired.entries.get(&path) {
            std::fs::write(&store, bytes).map_err(|error| error.to_string())?;
        }
        write_script_files(&store, &scripts)?;
        let sent = |prepared: &mut HashMap<PathBuf, PreparedEditorSettingsChange>,
                    plan: ReconcilePushPlan|
         -> Result<String, String> {
            let mut args = crate::cli::PushEditorChangesArgs::new(
                crate::cli::ProjectSourceArgs {
                    project_root: project_root.clone(),
                    src_root: PathBuf::from("src"),
                },
                crate::cli::BridgeConnectionArgs::local(0.1),
            );
            args.changed_paths.clone_from(&plan.changed_paths);
            args.target_settings_ids
                .clone_from(&plan.target_settings_ids);
            args.exact_target_ids = true;
            args.targets_extend_paths = true;
            args.override_packages = true;
            let documents = prepared
                .remove(&path)
                .map(|change| ("Workspace".to_string(), Arc::new(change.current)))
                .into_iter()
                .collect();
            let mut changes = crate::editor::sync::collect_prepared_editor_changes(
                &args,
                documents,
                HashSet::new(),
            )
            .map_err(|error| format!("collect: {error:#}"))?;
            super::push_plan::amend_reconciled_changes(&mut changes, plan)
                .map_err(|error| format!("amend: {error:#}"))?;
            request_text(&changes)
        };
        let pushed = |read: StoresRead| -> Result<(Vec<String>, bool, bool), String> {
            let prepare = || {
                let mut prepared = HashMap::new();
                let unchanged = prepare_project_replacement_from(
                    &desired,
                    &observed,
                    &paths,
                    &mut prepared,
                    read,
                )
                .map_err(|error| format!("prepare: {error:#}"))?;
                Ok::<_, String>((prepared, unchanged))
            };
            let (prepared, unchanged) = prepare()?;
            let stands =
                prepared.get(&path).is_some_and(|change| {
                    change.previous.instances.iter().any(|instance| {
                        crate::settings::held::stands_unchanged(&instance.properties)
                    })
                });
            let mut said = vec![format!("unchanged {}", unchanged.contains(&path))];
            let plan = reconciliation_push_plan_for_paths_with_prepared_settings(
                &observed, &desired, &paths, &prepared, false, false,
            );
            let alone = plan.as_ref().is_ok_and(plan_reads_changed_alone);
            let planned = plan.as_ref().is_ok_and(|plan| {
                !plan.target_settings_ids.is_empty() || !plan.changed_paths.is_empty()
            });
            said.push(match plan {
                Ok(plan) => plan_text(&plan),
                Err(error) => format!("no plan: {error:#}"),
            });
            // The write step, where the plan gives it something to send.
            if planned {
                let (mut prepared, _) = prepare()?;
                let plan = reconciliation_push_plan_for_paths_with_prepared_settings(
                    &observed, &desired, &paths, &prepared, false, false,
                )
                .map_err(|error| format!("{error:#}"))?;
                // A plan that reads more than the changed instances is
                // carried out from the files' whole copy.
                if read != StoresRead::ChangedOfBoth || plan_reads_changed_alone(&plan) {
                    said.push(match sent(&mut prepared, plan) {
                        Ok(sent) => sent,
                        Err(error) => format!("not sent: {error}"),
                    });
                }
            }
            if let Some(change) = prepared.get(&path) {
                said.push(format!(
                    "equivalent {} managed {} saved {:?}",
                    settings_documents_equivalent(&change.current, &change.previous),
                    super::verify::differs_only_in_externally_managed_fields(
                        &change.current,
                        &change.previous
                    ),
                    crate::settings::equivalence::saved_field_differences(
                        &change.current,
                        &change.previous
                    )
                    .len(),
                ));
            }
            // Studio afterwards: what the files hold, what it held before,
            // and the files with one value of its own.
            let mut partly = files.clone();
            if let Some(instance) = partly.instances.last_mut() {
                instance
                    .properties
                    .insert("Transparency".into(), json!(0.875));
            }
            for after in [
                Some(&desired),
                Some(&observed),
                snapshot(&partly, true).ok().as_ref(),
            ] {
                let Some(after) = after else {
                    continue;
                };
                let (mut prepared, _) = prepare()?;
                let verification = prepared
                    .remove(&path)
                    .map(|change| PreparedPushVerification {
                        previous: change.previous,
                        desired: Some(Arc::new(change.current)),
                    });
                said.push(format!(
                    "verified {:?}",
                    super::verify::settings_delta_mismatch(
                        &path,
                        observed.entries.get(&path),
                        desired.entries.get(&path),
                        after.entries.get(&path),
                        verification,
                    )
                    .map_err(|error| format!("{error:#}"))
                ));
            }
            Ok((said, stands, alone))
        };
        let (expected, _, expected_alone) = pushed(StoresRead::Whole)?;
        let (said, stands, _) = pushed(StoresRead::Changed)?;
        // Of both copies the changed instances alone: the plan made from
        // them says whether it may be carried out from them, as the plan
        // made from the whole copies says, and where it may, the push is the
        // same push.
        let (of_both, both_stand, alone) = pushed(StoresRead::ChangedOfBoth)?;
        if alone != expected_alone {
            return Err(format!(
                "read by the changed instances of both copies the plan reads them alone: {alone}, read whole: {expected_alone}"
            ));
        }
        // A push that replaces nothing compares the two copies the same
        // way: whether they differ, and the push its plan makes, which a
        // plan the instances that differ do not carry out makes from the
        // whole copies.
        let compared = |by_changed: bool| -> Result<(Vec<String>, bool), String> {
            let mut prepared = HashMap::new();
            let differences =
                snapshot_differences_prepared(&desired, &observed, Some(&mut prepared), by_changed)
                    .map_err(|error| format!("compare: {error:#}"))?;
            let plan = reconciliation_push_plan_for_paths_with_prepared_settings(
                &observed,
                &desired,
                &differences,
                &prepared,
                true,
                false,
            );
            let stands = plan.as_ref().is_ok_and(plan_reads_changed_alone)
                && prepared.get(&path).is_some_and(|change| {
                    crate::settings::equivalence::holds_unread_instances(&change.current)
                });
            let plan = match plan {
                Ok(plan) if by_changed => super::push_flow::plan_from_whole_stores(
                    (StoresRead::ChangedOfBoth, false, plan),
                    (&desired, &observed),
                    &differences,
                    &mut prepared,
                ),
                plan => plan,
            };
            let mut said = vec![format!("differs {}", differences.contains(&path))];
            match plan {
                Ok(plan) => {
                    said.push(plan_text(&plan));
                    if !plan.target_settings_ids.is_empty() || !plan.changed_paths.is_empty() {
                        said.push(match sent(&mut prepared, plan) {
                            Ok(sent) => sent,
                            Err(error) => format!("not sent: {error}"),
                        });
                    }
                }
                Err(error) => said.push(format!("no plan: {error:#}")),
            }
            Ok((said, stands))
        };
        let (whole_compared, _) = compared(false)?;
        let (by_changed, compared_stand) = compared(true)?;
        if by_changed != whole_compared {
            return Err(format!(
                "compared by the changed instances:\n{}\ncompared whole:\n{}",
                by_changed.join("\n"),
                whole_compared.join("\n")
            ));
        }
        if compared_stand {
            compared_by_changed.fetch_add(1, Ordering::Relaxed);
        }
        let _ = std::fs::remove_dir_all(&project_root);
        if alone && of_both != expected {
            return Err(format!(
                "from the changed instances of both copies ({both_stand}):
{}
from the whole documents:
{}",
                of_both.join(
                    "
"
                ),
                expected.join(
                    "
"
                )
            ));
        }
        if alone && both_stand && scripts_differ {
            with_scripts.fetch_add(1, Ordering::Relaxed);
            if of_both.iter().any(|said| said.contains("\"source\":\"")) {
                sources_sent.fetch_add(1, Ordering::Relaxed);
            }
        }
        if alone && both_stand {
            from_both.fetch_add(1, Ordering::Relaxed);
            if held_by_both > 0 {
                from_blocks_of_both.fetch_add(1, Ordering::Relaxed);
            }
            // The write step made a request from them, and it named values.
            if of_both
                .iter()
                .any(|said| said.contains("whole instances") && said.contains("\"properties\":{\""))
            {
                sent_from_both.fetch_add(1, Ordering::Relaxed);
            }
            // A plan that creates or removes the instances of a run is
            // carried out from them as well.
            if run && of_both.iter().any(|said| said.contains("whole instances")) {
                runs_sent_from_both.fetch_add(1, Ordering::Relaxed);
            }
        }
        if said != expected {
            return Err(format!(
                "from the changed instances ({stands}):\n{}\nfrom the whole documents:\n{}",
                said.join("\n"),
                expected.join("\n")
            ));
        }
        if stands {
            from_changed.fetch_add(1, Ordering::Relaxed);
            if reopened {
                under_other_ids.fetch_add(1, Ordering::Relaxed);
            }
        } else {
            whole.fetch_add(1, Ordering::Relaxed);
        }
        if expected[0] == "unchanged true" {
            unchanged_stores.fetch_add(1, Ordering::Relaxed);
        } else if !expected[1].lines().nth(2).unwrap_or_default().is_empty() {
            planned.fetch_add(1, Ordering::Relaxed);
        }
        Ok(())
    });
    if std::env::var_os("RENIUM_SIM_SEED").is_none() {
        let counts = [
            &from_changed,
            &whole,
            &planned,
            &unchanged_stores,
            &under_other_ids,
            &from_both,
            &sent_from_both,
            &from_blocks_of_both,
            &runs,
            &runs_sent_from_both,
        ]
        .map(|count| count.load(Ordering::Relaxed));
        // The stores whose instances stand at other places in the two
        // copies and were prepared from the ones that differ all the same.
        let told_alone = super::replacement::TOLD_ALONE.load(Ordering::Relaxed) - told_alone_before;
        assert!(told_alone >= 100, "paired each by itself: {told_alone}");
        // The preparations of this test aligned beside a run of instances
        // one copy alone holds: about four for each store with a run over
        // 3,000 seeds, five over the 300 run by default.
        let copies = copies.into_inner().unwrap();
        // Counted and let go: an assert that failed holding it would fail
        // every later preparation of this binary too.
        let aligned = (super::replacement::ALIGNED_BESIDE_RUN
            .lock()
            .unwrap()
            .iter())
        .filter(|key| copies.contains(*key))
        .count();
        assert!(
            aligned >= 2 * counts[8],
            "aligned beside a run: {aligned}, of {} stores with a run",
            counts[8]
        );
        // The ones paired so without a comparison by place before: the store
        // stood apart the last time it was compared.
        let paired_first =
            super::replacement::PAIRED_FIRST.load(Ordering::Relaxed) - paired_first_before;
        assert!(paired_first >= 5, "paired first: {paired_first}");
        // The ones whose namesakes were paired by their order: refused for
        // what such a pair holds, and taken.
        let by_order = [0, 1].map(|taken| {
            super::replacement::BY_ORDER[taken].load(Ordering::Relaxed) - by_order_before[taken]
        });
        assert!(
            by_order[0] >= 5 && by_order[1] >= 8,
            "namesakes paired by their order, refused and taken: {by_order:?}"
        );
        let compared = compared_by_changed.load(Ordering::Relaxed);
        assert!(
            compared >= 100,
            "compared by the changed instances: {compared}"
        );
        let (with_scripts, sources_sent) = (
            with_scripts.load(Ordering::Relaxed),
            sources_sent.load(Ordering::Relaxed),
        );
        assert!(
            with_scripts >= 40 && sources_sent >= 35,
            "beside scripts that differ: {with_scripts}, {sources_sent} of them with a source sent"
        );
        assert!(
            counts[0] >= 30
                && counts[1] >= 30
                && counts[2] >= 50
                && counts[3] >= 10
                && counts[4] >= 5
                && counts[5] >= 12
                && counts[6] >= 8
                && counts[7] >= 8
                && counts[9] >= 20,
            "prepared from the changed instances {} ({} of them under other ids, {} with both copies read so, {} of those with values sent, {} of them with blocks both hold, {} of them sent from both with a run of instances one copy alone holds), from whole documents {}, with a plan {}, unchanged {}",
            counts[0],
            counts[4],
            counts[5],
            counts[6],
            counts[7],
            counts[9],
            counts[1],
            counts[2],
            counts[3]
        );
    }
}

/// A union cut anew is removed from Studio and imported again, and the
/// request that removes it tells it from its namesakes by what each of them
/// holds: a field only its twin states goes into the request with its
/// default. The twin differs in nothing, so the copy Studio holds is read
/// for it all the same, and the request is the one the whole documents give.
#[test]
fn the_namesake_of_a_union_cut_anew_is_read_from_the_store_studio_holds() {
    let root =
        SettingsBytecodeInstance::new("1".into(), "Workspace".into(), "Workspace".into(), None);
    let walls =
        SettingsBytecodeInstance::new("walls".into(), "Walls".into(), "Folder".into(), Some(0));
    let wall = |id: &str, asset: u64| {
        let mut wall = SettingsBytecodeInstance::new(
            id.into(),
            "Wall".into(),
            "UnionOperation".into(),
            Some(1),
        );
        wall.properties.insert("AssetId".into(), json!(asset));
        wall.properties.insert("Anchored".into(), json!(true));
        wall
    };
    let mut twin = wall("wall-b", 7);
    twin.properties.insert("CastShadow".into(), json!(false));
    let mut other =
        SettingsBytecodeInstance::new("floor".into(), "Floor".into(), "Part".into(), Some(0));
    other.properties.insert("Anchored".into(), json!(true));
    let studio = SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances: vec![root, walls, wall("wall-a", 7), twin, other],
    };
    let mut files = studio.clone();
    files.instances[2]
        .properties
        .insert("AssetId".into(), json!(8));
    let path = PathBuf::from("src/Workspace/__roblox_sync_settings.renium");
    let snapshot = |document: &SettingsBytecode| ProjectSnapshot {
        entries: BTreeMap::from([(
            path.clone(),
            SnapshotEntry::File(encode_settings_bytecode(document).unwrap()),
        )]),
    };
    let (desired, observed) = (snapshot(&files), snapshot(&studio));
    let paths = project_replacement_paths(&desired, &observed);
    let removals = |whole_documents: bool| {
        let read = if whole_documents {
            StoresRead::Whole
        } else {
            StoresRead::Changed
        };
        let mut prepared = HashMap::new();
        prepare_project_replacement_from(&desired, &observed, &paths, &mut prepared, read).unwrap();
        let previous = &prepared[&path].previous;
        // The floor stands unchanged; neither wall does.
        assert_eq!(
            previous
                .instances
                .iter()
                .map(|instance| crate::settings::held::stands_unchanged(&instance.properties))
                .collect::<Vec<_>>(),
            [false, !whole_documents, false, false, !whole_documents]
        );
        let plan = reconciliation_push_plan_for_paths_with_prepared_settings(
            &observed, &desired, &paths, &prepared, false, false,
        )
        .unwrap();
        // What stands under the old union goes to the new one, so the
        // instances that differ do not carry this plan out.
        assert!(!plan_reads_changed_alone(&plan));
        serde_json::to_string(&plan.instance_deletes).unwrap()
    };
    let request = removals(false);
    assert!(request.contains("\"wall-a\"") && request.contains("CastShadow"));
    assert_eq!(request, removals(true));
}

/// A store can hold a parent after its child. A request names an instance
/// by its path and tells each instance on that path from its namesakes, so
/// the path of an instance that differs is read to its root wherever its
/// ancestors stand, and their namesakes with them.
#[test]
fn the_ancestors_of_a_changed_instance_are_read_wherever_they_stand() {
    let instance = |id: &str, name: &str, class: &str, parent: Option<usize>| {
        SettingsBytecodeInstance::new(id.into(), name.into(), class.into(), parent)
    };
    let mut part = instance("p", "P", "Part", Some(2));
    part.properties.insert("Transparency".into(), json!(0.25));
    let mut other = instance("o", "Other", "Part", Some(0));
    other.properties.insert("Transparency".into(), json!(0.5));
    let studio = SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances: vec![
            instance("1", "Workspace", "Workspace", None),
            part,
            instance("car", "Car", "Model", Some(0)),
            instance("car-2", "Car", "Folder", Some(0)),
            other,
        ],
    };
    let mut files = studio.clone();
    files.instances[1]
        .properties
        .insert("Transparency".into(), json!(0.75));
    let path = PathBuf::from("src/Workspace/__roblox_sync_settings.renium");
    let snapshot = |document: &SettingsBytecode| ProjectSnapshot {
        entries: BTreeMap::from([(
            path.clone(),
            SnapshotEntry::File(encode_settings_bytecode(document).unwrap()),
        )]),
    };
    let (desired, observed) = (snapshot(&files), snapshot(&studio));
    let paths = project_replacement_paths(&desired, &observed);
    let mut prepared = HashMap::new();
    prepare_project_replacement_from(
        &desired,
        &observed,
        &paths,
        &mut prepared,
        StoresRead::ChangedOfBoth,
    )
    .unwrap();
    let change = &prepared[&path];
    for document in [&change.current, &change.previous] {
        assert_eq!(
            document
                .instances
                .iter()
                .map(|instance| crate::settings::held::stands_unchanged(&instance.properties))
                .collect::<Vec<_>>(),
            [false, false, false, false, true]
        );
    }
}

/// A store whose copies differ in more instances than a push sends one by
/// one is imported anew: it is prepared and planned as a store of which
/// Studio holds nothing but what stays in a service, where the whole
/// documents would send its instances one by one. Rows written another way
/// that hold the same count for nothing.
#[test]
fn a_store_most_of_whose_instances_differ_is_imported_anew() {
    type Stated = Option<(&'static str, Value)>;
    let store = |class: &str, stated: &dyn Fn(usize) -> Vec<Stated>| {
        let instance = |id: String, name: String, class: &str, parent| {
            SettingsBytecodeInstance::new(id, name, class.into(), parent)
        };
        let mut root = instance("1".into(), "Workspace".into(), "Workspace", None);
        root.properties.insert(
            "CurrentCamera".into(),
            json!({"_type": "Ref", "instanceIndex": 2}),
        );
        let mut camera = instance("debug:0_2".into(), "Camera".into(), "Camera", Some(0));
        camera.properties.insert("FieldOfView".into(), json!(70.0));
        let mut terrain = instance("debug:0_3".into(), "Terrain".into(), "Terrain", Some(0));
        terrain
            .properties
            .insert("WaterTransparency".into(), json!(0.25));
        let mut instances = vec![root, camera, terrain];
        for index in 0..17_300 {
            let id = format!("debug:0_{}", 10 + index);
            let mut part = instance(id, format!("Item{index}"), class, Some(0));
            part.properties.insert("Anchored".into(), json!(true));
            for (name, value) in stated(index).into_iter().flatten() {
                part.properties.insert(name.into(), value);
            }
            instances.push(part);
        }
        SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances,
        }
    };
    let path = PathBuf::from("src/Workspace/__roblox_sync_settings.renium");
    let snapshot = |document: &SettingsBytecode| ProjectSnapshot {
        entries: BTreeMap::from([(
            path.clone(),
            SnapshotEntry::File(encode_settings_bytecode(document).unwrap()),
        )]),
    };
    // Whether the store is imported anew, the plan, and what the push takes
    // Studio to hold.
    let pushed = |files: &SettingsBytecode, studio: &SettingsBytecode, read: StoresRead| {
        let (desired, observed) = (snapshot(files), snapshot(studio));
        let paths = project_replacement_paths(&desired, &observed);
        let (mut prepared, imported) = (HashMap::new(), ImportedStores::default());
        prepare_project_replacement_importing(
            &desired,
            &observed,
            &paths,
            &mut prepared,
            read,
            (read != StoresRead::Whole).then_some(&imported),
        )
        .unwrap();
        let plan = reconciliation_push_plan_for_paths_with_prepared_settings(
            &observed, &desired, &paths, &prepared, true, false,
        )
        .unwrap();
        // The plan is carried out from the documents it was made from.
        let plan = super::push_flow::plan_from_whole_stores(
            (read, true, plan),
            (&desired, &observed),
            &paths,
            &mut prepared,
        )
        .unwrap();
        let previous = prepared.remove(&path).map(|change| {
            let said = |instance: &SettingsBytecodeInstance| {
                format!("{} {:?}", instance.settings_id, instance.properties)
            };
            change.previous.instances.iter().map(said).collect()
        });
        (
            imported.into_inner().unwrap().contains(&path),
            plan_text(&plan),
            previous.unwrap_or_else(Vec::new),
        )
    };
    let transparent = || Some(("Transparency", json!(0.5)));

    // All but 300 of the parts differ.
    let studio = store("Part", &|_| vec![]);
    let files = store("Part", &|index| {
        vec![(index < 17_000).then(transparent).flatten()]
    });
    let mut bare = studio.clone();
    bare.instances.truncate(3);
    let (imported, expected, kept) = pushed(&files, &bare, StoresRead::ChangedOfBoth);
    assert!(
        !imported && expected.starts_with("Workspace\n"),
        "{expected}"
    );
    assert_eq!(kept.len(), 3);
    for read in [StoresRead::ChangedOfBoth, StoresRead::Changed] {
        let (imported, plan, previous) = pushed(&files, &studio, read);
        assert!(imported);
        assert_eq!((plan, previous), (expected.clone(), kept.clone()));
    }
    let (_, whole, held) = pushed(&files, &studio, StoresRead::Whole);
    assert!(!whole.starts_with("Workspace\n") && held.len() > 17_000);

    // The files hold a model Studio lacks. One of a quarter of the service's
    // instances is imported with the store; a smaller one is sent as the
    // whole documents send it, instance by instance.
    let with_model = |parts: usize| {
        let mut files = studio.clone();
        let model = (0..=parts).map(|index| {
            let (name, class, parent) = match index {
                0 => ("Model".to_string(), "Folder", 0),
                _ => (format!("Added{index}"), "Part", 3),
            };
            SettingsBytecodeInstance::new(
                format!("editor:{index}"),
                name,
                class.into(),
                Some(parent),
            )
        });
        files.instances.splice(3..3, model);
        files
    };
    let files = with_model(8_000);
    let (_, expected, kept) = pushed(&files, &bare, StoresRead::ChangedOfBoth);
    for read in [StoresRead::ChangedOfBoth, StoresRead::Changed] {
        let (imported, plan, previous) = pushed(&files, &studio, read);
        assert!(imported);
        assert_eq!((plan, previous), (expected.clone(), kept.clone()));
    }
    for parts in [2_500, 40] {
        let files = with_model(parts);
        let (imported, plan, _) = pushed(&files, &studio, StoresRead::ChangedOfBoth);
        assert!(
            !imported && plan.contains(&format!("editor:{parts}")),
            "{plan}"
        );
        assert_eq!(plan, pushed(&files, &studio, StoresRead::Whole).1);
    }

    // Every row differs, and half of them in what they hold: fewer than a
    // push sends one by one.
    let spelled = || {
        Some((
            "CollisionFidelity",
            json!({"_type": "EnumItem", "name": "Default"}),
        ))
    };
    let studio = store("MeshPart", &|_| vec![]);
    let half = store("MeshPart", &|index| {
        vec![spelled(), (index % 2 == 0).then(transparent).flatten()]
    });
    let (imported, plan, _) = pushed(&half, &studio, StoresRead::ChangedOfBoth);
    assert!(!imported);
    assert_eq!(plan, pushed(&half, &studio, StoresRead::Whole).1);

    // Every row differs, and all of them in what they hold.
    let all = store("MeshPart", &|_| vec![spelled(), transparent()]);
    assert!(pushed(&all, &studio, StoresRead::ChangedOfBoth).0);

    // Every row differs, and five of them in what they hold: those five
    // alone are read of Studio's copy.
    let few = store("MeshPart", &|index| {
        vec![spelled(), (index % 4_000 == 7).then(transparent).flatten()]
    });
    let (imported, plan, previous) = pushed(&few, &studio, StoresRead::ChangedOfBoth);
    assert!(!imported && plan.contains("debug:0_4017"), "{plan}");
    assert_eq!(plan, pushed(&few, &studio, StoresRead::Whole).1);
    let read = previous.iter().filter(|row| row.contains("Anchored"));
    assert_eq!(read.count(), 5);

    // Studio holds a folder the files lack, and one part names by its place
    // another instance in each copy: the same number, said of two instances.
    let names = || json!({"_type": "Ref", "instanceIndex": 8});
    let mut beside = studio.clone();
    let gone =
        SettingsBytecodeInstance::new("debug:gone".into(), "Gone".into(), "Folder".into(), Some(0));
    beside.instances.insert(3, gone);
    beside.instances[4_000]
        .properties
        .insert("Target".into(), names());
    let mut naming = few.clone();
    naming.instances[3_999]
        .properties
        .insert("Target".into(), names());
    let (imported, plan, _) = pushed(&naming, &beside, StoresRead::ChangedOfBoth);
    assert!(!imported && plan.contains("debug:0_4006"), "{plan}");
    assert_eq!(plan, pushed(&naming, &beside, StoresRead::Whole).1);

    // Every row differs, and none in what it holds.
    let same = store("MeshPart", &|_| vec![spelled()]);
    let (imported, plan, previous) = pushed(&same, &studio, StoresRead::ChangedOfBoth);
    assert!(!imported && previous.is_empty());
    assert_eq!(plan, pushed(&same, &studio, StoresRead::Whole).1);
}

/// Beside a run of instances the files alone hold, in a store too many of
/// whose rows Studio wrote another way to read them all, twins Studio holds
/// crossed in an engine's attribute, which the comparison of such rows
/// leaves out and the matching weighs, are read and left to the matching.
/// Twins that hold the same stand unread for their counterparts. Either way
/// the push is the one the whole documents make.
#[test]
fn twins_crossed_among_rows_written_another_way_are_read_beside_a_run() {
    let store = |studio: bool, crossed: bool| {
        let instance = |id: String, name: &str, class: &str, parent| {
            SettingsBytecodeInstance::new(id, name.into(), class.into(), parent)
        };
        let session = usize::from(studio);
        let mut root = instance("1".into(), "Workspace", "Workspace", None);
        root.properties.insert(
            "CurrentCamera".into(),
            json!({"_type": "Ref", "instanceIndex": 2}),
        );
        let camera = format!("debug:{session}_2");
        let mut instances = vec![root, instance(camera, "Camera", "Camera", Some(0))];
        if !studio {
            instances.push(instance("editor:added".into(), "Added", "Folder", Some(0)));
        }
        for seen in [1, 2] {
            let id = format!("debug:{session}_{}", 2 + seen);
            let mut twin = instance(id, "A", "Folder", Some(0));
            let seen = if crossed { 3 - seen } else { seen };
            twin.attributes.insert("RBX_Seen".into(), json!(seen));
            instances.push(twin);
        }
        for index in 0..16_400 {
            let id = format!("debug:{session}_{}", 10 + index);
            let mut part = instance(id, &format!("Item{index}"), "MeshPart", Some(0));
            part.properties.insert("Anchored".into(), json!(true));
            if studio {
                part.properties.insert(
                    "CollisionFidelity".into(),
                    json!({"_type": "EnumItem", "name": "Default"}),
                );
            }
            instances.push(part);
        }
        SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances,
        }
    };
    let path = PathBuf::from("src/Workspace/__roblox_sync_settings.renium");
    let snapshot = |document: &SettingsBytecode| ProjectSnapshot {
        entries: BTreeMap::from([(
            path.clone(),
            SnapshotEntry::File(encode_settings_bytecode(document).unwrap()),
        )]),
    };
    // The plan, and whether the store Studio holds stands for the most of
    // its instances unread.
    let pushed = |studio: &SettingsBytecode, read: StoresRead| {
        let (desired, observed) = (snapshot(&store(false, false)), snapshot(studio));
        let paths = project_replacement_paths(&desired, &observed);
        let mut prepared = HashMap::new();
        prepare_project_replacement_from(&desired, &observed, &paths, &mut prepared, read).unwrap();
        let stands = prepared.get(&path).is_some_and(|change| {
            (change.previous.instances.iter())
                .any(|instance| crate::settings::held::stands_unchanged(&instance.properties))
        });
        let plan = reconciliation_push_plan_for_paths_with_prepared_settings(
            &observed, &desired, &paths, &prepared, true, false,
        )
        .unwrap();
        let plan = super::push_flow::plan_from_whole_stores(
            (read, true, plan),
            (&desired, &observed),
            &paths,
            &mut prepared,
        )
        .unwrap();
        (plan_text(&plan), stands)
    };
    for crossed in [true, false] {
        let studio = store(true, crossed);
        let (expected, _) = pushed(&studio, StoresRead::Whole);
        for read in [StoresRead::Changed, StoresRead::ChangedOfBoth] {
            assert_eq!(pushed(&studio, read), (expected.clone(), !crossed));
        }
    }
}

/// An instance the files hold in another store than Studio does is removed
/// from one and created in the other. Prepared from the instances that
/// differ, with what refers to the moved one, the push says and sends what
/// the push prepared from the whole documents says and sends.
#[test]
fn an_instance_moved_to_another_store_is_pushed_from_the_changed_instances_as_the_whole_documents_push_it()
 {
    use crate::settings::bytecode::blocks::sim::in_blocks;
    use crate::tests::support::{SimRng, run_seeds};
    use std::sync::atomic::{AtomicUsize, Ordering};

    let (moved_alone, with_referrers) = (AtomicUsize::new(0), AtomicUsize::new(0));
    let referred_unchanged = AtomicUsize::new(0);
    run_seeds(200, |seed, _| {
        let mut rng = SimRng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
        let block_rows = [1, 3, 8, 4096][(seed >> 2) as usize % 4];
        let mut studio_a = changed_instances_store(&mut rng);
        let refers = |instance: &SettingsBytecodeInstance| {
            let values = format!("{:?}{:?}", instance.properties, instance.attributes);
            values.contains("instanceIndex") || values.contains("settingsId")
        };
        let leaves = (1..studio_a.instances.len())
            .filter(|index| {
                let instance = &studio_a.instances[*index];
                instance.class_name != "Camera"
                    && !refers(instance)
                    && !studio_a
                        .instances
                        .iter()
                        .any(|other| other.parent_index == Some(*index))
            })
            .collect::<Vec<_>>();
        if leaves.is_empty() {
            return Ok(());
        }
        let at = leaves[rng.below(leaves.len())];
        let id = studio_a.instances[at].settings_id.clone();
        let mut studio_b = SettingsBytecode {
            version: crate::settings::bytecode::SETTINGS_BINARY_VERSION,
            instances: vec![SettingsBytecodeInstance::new(
                "rs".into(),
                "ReplicatedStorage".into(),
                "ReplicatedStorage".into(),
                None,
            )],
        };
        for index in 0..1 + rng.below(6) {
            let mut instance = SettingsBytecodeInstance::new(
                format!("b{index}"),
                ["Shared", "Assets", "Shared", "Kit"][rng.below(4)].into(),
                ["Folder", "Part"][rng.below(2)].into(),
                Some(rng.below(index + 1)),
            );
            instance
                .properties
                .insert("Transparency".into(), json!(rng.below(4) as f64 / 4.0));
            studio_b.instances.push(instance);
        }
        // What refers to the moved instance names it by its place in the
        // store that holds it and by its id from another store.
        let by_id = json!({"_type": "Ref", "settingsId": id});
        let pointer = |store: &str, index: usize, parent: usize, value: Value| {
            let mut instance = SettingsBytecodeInstance::new(
                format!("{store}-pointer{index}"),
                "Pointer".into(),
                "ObjectValue".into(),
                Some(parent),
            );
            instance.properties.insert("Value".into(), value);
            instance
        };
        let (in_a, in_b) = (rng.below(3), rng.below(3));
        let mut files_a = studio_a.clone();
        for index in 0..in_a {
            let here = json!({"_type": "Ref", "instanceIndex": at + 1});
            studio_a.instances.push(pointer("a", index, 0, here));
            files_a
                .instances
                .push(pointer("a", index, 0, by_id.clone()));
        }
        let moved = files_a.instances.remove(at);
        place_instances(&mut files_a, &|index| match index.cmp(&at) {
            std::cmp::Ordering::Less => Some(index),
            std::cmp::Ordering::Equal => None,
            std::cmp::Ordering::Greater => Some(index - 1),
        });
        let mut files_b = studio_b.clone();
        let parent = rng.below(files_b.instances.len());
        let there = files_b.instances.len() + in_b;
        for index in 0..in_b {
            let parent = rng.below(studio_b.instances.len());
            studio_b
                .instances
                .push(pointer("b", index, parent, by_id.clone()));
            let here = json!({"_type": "Ref", "instanceIndex": there + 1});
            files_b.instances.push(pointer("b", index, parent, here));
        }
        files_b.instances.push(SettingsBytecodeInstance {
            parent_index: Some(parent),
            ..moved
        });
        // A third store refers to it by its id in both copies: an instance
        // that differs in nothing, beside one that does.
        let mut studio_c = SettingsBytecode {
            version: crate::settings::bytecode::SETTINGS_BINARY_VERSION,
            instances: vec![SettingsBytecodeInstance::new(
                "ss".into(),
                "ServerStorage".into(),
                "ServerStorage".into(),
                None,
            )],
        };
        let in_c = rng.below(3);
        for index in 0..in_c {
            studio_c
                .instances
                .push(pointer("c", index, 0, by_id.clone()));
        }
        for index in 0..2 + rng.below(4) {
            let mut instance = SettingsBytecodeInstance::new(
                format!("c{index}"),
                "Crate".into(),
                "Part".into(),
                Some(0),
            );
            instance
                .properties
                .insert("Transparency".into(), json!(0.25));
            studio_c.instances.push(instance);
        }
        let mut files_c = studio_c.clone();
        if let Some(instance) = files_c.instances.last_mut() {
            instance
                .properties
                .insert("Transparency".into(), json!(0.5));
        }
        let stores = ["Workspace", "ReplicatedStorage", "ServerStorage"]
            .map(|service| PathBuf::from(format!("src/{service}/__roblox_sync_settings.renium")));
        let snapshot = |documents: [&SettingsBytecode; 3]| {
            let mut entries = BTreeMap::new();
            for (path, document) in stores.iter().zip(documents) {
                let bytes =
                    in_blocks(document, block_rows).map_err(|error| format!("{error:#}"))?;
                entries.insert(path.clone(), SnapshotEntry::File(bytes));
            }
            Ok::<_, String>(ProjectSnapshot { entries })
        };
        let (Ok(desired), Ok(observed)) = (
            snapshot([&files_a, &files_b, &files_c]),
            snapshot([&studio_a, &studio_b, &studio_c]),
        ) else {
            return Ok(());
        };
        let paths = project_replacement_paths(&desired, &observed);
        let project_root = crate::tests::support::temp_dir(&format!("moved-instance-{seed}"));
        for (path, entry) in &desired.entries {
            let store = project_root.join(path);
            std::fs::create_dir_all(store.parent().unwrap()).map_err(|error| error.to_string())?;
            if let SnapshotEntry::File(bytes) = entry {
                std::fs::write(&store, bytes).map_err(|error| error.to_string())?;
            }
        }
        let pushed = |read: StoresRead| -> Result<(Vec<String>, bool, usize), String> {
            let mut prepared = HashMap::new();
            prepare_project_replacement_from(&desired, &observed, &paths, &mut prepared, read)
                .map_err(|error| format!("prepare: {error:#}"))?;
            let plan = reconciliation_push_plan_for_paths_with_prepared_settings(
                &observed, &desired, &paths, &prepared, true, false,
            )
            .map_err(|error| format!("plan: {error:#}"))?;
            // The plan as the push carries it out.
            let plan = super::push_flow::plan_from_whole_stores(
                (read, true, plan),
                (&desired, &observed),
                &paths,
                &mut prepared,
            )
            .map_err(|error| format!("plan from whole stores: {error:#}"))?;
            // Whether a store is still read for what differs alone.
            let alone = prepared.values().any(|change| {
                crate::settings::equivalence::holds_unread_instances(&change.current)
            });
            let targets = plan.target_settings_ids.len();
            let mut said = vec![plan_text(&plan)];
            let mut args = crate::cli::PushEditorChangesArgs::new(
                crate::cli::ProjectSourceArgs {
                    project_root: project_root.clone(),
                    src_root: PathBuf::from("src"),
                },
                crate::cli::BridgeConnectionArgs::local(0.1),
            );
            args.changed_paths.clone_from(&plan.changed_paths);
            args.target_settings_ids
                .clone_from(&plan.target_settings_ids);
            args.exact_target_ids = true;
            args.targets_extend_paths = true;
            args.override_packages = true;
            let documents = prepared
                .into_iter()
                .map(|(path, change)| {
                    let service = ["Workspace", "ReplicatedStorage", "ServerStorage"]
                        [stores.iter().position(|store| *store == path).unwrap_or(0)];
                    (service.to_string(), Arc::new(change.current))
                })
                .collect();
            let sent = crate::editor::sync::collect_prepared_editor_changes(
                &args,
                documents,
                HashSet::new(),
            )
            .map_err(|error| format!("collect: {error:#}"))
            .and_then(|mut changes| {
                super::push_plan::amend_reconciled_changes(&mut changes, plan)
                    .map_err(|error| format!("amend: {error:#}"))?;
                request_text(&changes)
            });
            said.push(sent.unwrap_or_else(|error| format!("not sent: {error}")));
            Ok((said, alone, targets))
        };
        let whole = pushed(StoresRead::Whole);
        let of_both = pushed(StoresRead::ChangedOfBoth);
        let _ = std::fs::remove_dir_all(&project_root);
        let (whole, _, _) = whole?;
        let (of_both, alone, targets) = of_both?;
        if of_both != whole {
            return Err(format!(
                "from the changed instances of both copies:\n{}\nfrom the whole documents:\n{}",
                of_both.join("\n"),
                whole.join("\n")
            ));
        }
        if alone {
            moved_alone.fetch_add(1, Ordering::Relaxed);
        }
        // The moved instance, and each instance that refers to it.
        if in_a + in_b + in_c > 0 && targets > in_a + in_b + in_c {
            with_referrers.fetch_add(1, Ordering::Relaxed);
        }
        if in_c > 0 {
            referred_unchanged.fetch_add(1, Ordering::Relaxed);
        }
        Ok(())
    });
    if std::env::var_os("RENIUM_SIM_SEED").is_none() {
        let (moved, referred) = (
            moved_alone.load(Ordering::Relaxed),
            with_referrers.load(Ordering::Relaxed),
        );
        let unchanged = referred_unchanged.load(Ordering::Relaxed);
        assert!(
            moved >= 100 && referred >= 80 && unchanged >= 60,
            "{moved} moves pushed from what differs, {referred} of them with what refers to the moved one, {unchanged} from an instance that differs in nothing"
        );
    }
}

/// A store of which Studio holds nothing but what stays in a service is
/// pushed whole: the import carries its instances from the store file, and
/// the push reads only the ones it handles beside the import. Prepared so,
/// the push says and sends what the push prepared from the whole documents
/// says and sends, and the instances it left in the file, read afterwards,
/// give the whole documents' copy.
#[test]
fn a_bare_store_prepared_from_the_instances_its_push_reads_is_pushed_as_the_whole_documents_push_it()
 {
    use crate::tests::support::{SimRng, run_seeds};
    use std::sync::atomic::{AtomicUsize, Ordering};

    let (in_part, whole, with_containers, emptied) = (
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
    );
    let with_one_kept = AtomicUsize::new(0);
    run_seeds(300, |seed, _| {
        let mut rng = SimRng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
        let mut files = changed_instances_store(&mut rng);
        let camera = files
            .instances
            .iter()
            .position(|instance| instance.class_name == "Camera");
        // What a place holds beside its parts: the terrain, scripts with
        // their text in the store or beside it, a second camera, and
        // instances named or classed as the ones Studio keeps.
        let count = files.instances.len();
        if rng.below(2) == 0 {
            let mut terrain = SettingsBytecodeInstance::new(
                if rng.below(2) == 0 {
                    "terrain".into()
                } else {
                    "debug:0_3".into()
                },
                "Terrain".into(),
                "Terrain".into(),
                Some(0),
            );
            terrain
                .properties
                .insert("WaterTransparency".into(), json!(rng.below(4) as f64 / 4.0));
            files.instances.push(terrain);
        }
        for index in 0..rng.below(4) {
            let parent = rng.below(count);
            let class = ["Script", "ModuleScript", "Camera", "Terrain", "Folder"][rng.below(5)];
            let name = ["Camera", "Terrain", "Main", "Twin", "Workspace"][rng.below(5)];
            let mut instance = SettingsBytecodeInstance::new(
                format!("extra-{index}"),
                name.into(),
                class.into(),
                Some(parent),
            );
            if class.ends_with("Script") {
                instance.properties.insert(
                    "Source".into(),
                    if rng.below(2) == 0 {
                        json!(format!("return {index}"))
                    } else {
                        json!(crate::settings::EXTERNAL_SOURCE_MARKER)
                    },
                );
            }
            if class == "Camera" {
                instance
                    .properties
                    .insert("FieldOfView".into(), json!(40.0));
            }
            if rng.below(2) == 0 {
                instance.properties.insert(
                    "UniqueId".into(),
                    json!({"_type": "UniqueId", "value": format!("{:024x}{:08x}", 9, index + 1)}),
                );
            }
            files.instances.push(instance);
        }
        // A store written before a root named its camera.
        if rng.below(4) == 0 {
            files.instances[0].properties.remove("CurrentCamera");
        }
        // Studio: an empty place of its own, or no store at all.
        let empty = rng.below(12) == 0;
        let mut studio = SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: Vec::new(),
        };
        if !empty {
            let mut root = SettingsBytecodeInstance::new(
                if rng.below(2) == 0 {
                    "1".into()
                } else {
                    "debug:0_1".into()
                },
                "Workspace".into(),
                "Workspace".into(),
                None,
            );
            root.properties
                .insert("Gravity".into(), json!([196.2, 50.0][rng.below(2)]));
            studio.instances.push(root);
            if rng.below(3) != 0 {
                // The viewport camera stands under the root, or in a folder
                // somebody made for it.
                let folder = (rng.below(5) == 0).then(|| {
                    studio.instances.push(SettingsBytecodeInstance::new(
                        "debug:0_60".into(),
                        "Cams".into(),
                        "Folder".into(),
                        Some(0),
                    ));
                    studio.instances.len() - 1
                });
                let position = studio.instances.len();
                let mut own = SettingsBytecodeInstance::new(
                    match (rng.below(3), camera) {
                        (0, Some(camera)) => files.instances[camera].settings_id.clone(),
                        (1, _) => "item-2".into(),
                        _ => "debug:0_77".into(),
                    },
                    "Camera".into(),
                    "Camera".into(),
                    Some(folder.unwrap_or(0)),
                );
                own.properties
                    .insert("FieldOfView".into(), json!([70.0, 55.0][rng.below(2)]));
                if let (0, Some(camera)) = (rng.below(2), camera)
                    && let Some(unique) = files.instances[camera].properties.get("UniqueId")
                {
                    own.properties.insert("UniqueId".into(), unique.clone());
                }
                studio.instances.push(own);
                if rng.below(4) != 0 {
                    studio.instances[0].properties.insert(
                        "CurrentCamera".into(),
                        json!({"_type": "Ref", "instanceIndex": position + 1}),
                    );
                }
            }
            if rng.below(2) == 0 {
                let mut terrain = SettingsBytecodeInstance::new(
                    if rng.below(2) == 0 {
                        "terrain".into()
                    } else {
                        "debug:0_3".into()
                    },
                    "Terrain".into(),
                    "Terrain".into(),
                    Some(0),
                );
                terrain
                    .properties
                    .insert("WaterTransparency".into(), json!(rng.below(4) as f64 / 4.0));
                studio.instances.push(terrain);
            }
        }
        // Studio holds a little of its own beside that: an instance the
        // files know nothing of, the namesake of one of theirs, one of
        // theirs under another id, or one of theirs as it is.
        let mut kept = false;
        if !studio.instances.is_empty() && rng.below(3) == 0 {
            for index in 0..1 + rng.below(2) {
                let theirs = &files.instances[1 + rng.below(files.instances.len() - 1)];
                let mut own = match rng.below(4) {
                    0 => SettingsBytecodeInstance::new(
                        format!("debug:0_{}", 900 + index),
                        "Marker".into(),
                        "Folder".into(),
                        Some(0),
                    ),
                    1 => {
                        let mut own = SettingsBytecodeInstance::new(
                            format!("debug:0_{}", 900 + index),
                            theirs.name.clone(),
                            theirs.class_name.clone(),
                            Some(0),
                        );
                        own.properties.insert("Transparency".into(), json!(0.125));
                        own
                    }
                    2 => {
                        let mut own = theirs.clone();
                        own.settings_id = format!("debug:0_{}", 900 + index);
                        own.parent_index = Some(0);
                        own
                    }
                    _ => {
                        kept |= theirs.parent_index == Some(0);
                        let mut own = theirs.clone();
                        own.parent_index = Some(0);
                        own
                    }
                };
                own.properties
                    .retain(|_, value| value.get("_type").and_then(Value::as_str) != Some("Ref"));
                studio.instances.push(own);
            }
        }
        let path = PathBuf::from("src/Workspace/__roblox_sync_settings.renium");
        let snapshot = |document: &SettingsBytecode| {
            Ok::<_, String>(ProjectSnapshot {
                entries: BTreeMap::from([(
                    path.clone(),
                    SnapshotEntry::File(
                        encode_settings_bytecode(document).map_err(|error| format!("{error:#}"))?,
                    ),
                )]),
            })
        };
        let (Ok(desired), Ok(observed)) = (snapshot(&files), snapshot(&studio)) else {
            return Ok(());
        };
        let paths = project_replacement_paths(&desired, &observed);
        let project_root = crate::tests::support::temp_dir(&format!("bare-store-{seed}"));
        let store = project_root.join(&path);
        std::fs::create_dir_all(store.parent().unwrap()).map_err(|error| error.to_string())?;
        if let Some(SnapshotEntry::File(bytes)) = desired.entries.get(&path) {
            std::fs::write(&store, bytes).map_err(|error| error.to_string())?;
        }
        type Pushed = (Vec<String>, Option<PreparedEditorSettingsChange>, bool);
        let pushed = |read: StoresRead| -> Result<Pushed, String> {
            let prepare = || {
                let mut prepared = HashMap::new();
                let unchanged = prepare_project_replacement_from(
                    &desired,
                    &observed,
                    &paths,
                    &mut prepared,
                    read,
                )
                .map_err(|error| format!("prepare: {error:#}"))?;
                Ok::<_, String>((prepared, unchanged))
            };
            let (prepared, unchanged) = prepare()?;
            let mut said = vec![format!("unchanged {}", unchanged.contains(&path))];
            let unread =
                prepared.get(&path).is_some_and(|change| {
                    change.current.instances.iter().any(|instance| {
                        crate::settings::held::stands_unchanged(&instance.properties)
                    })
                });
            if let Some(change) = prepared.get(&path) {
                said.push(format!(
                    "native {} equivalent {} managed {} saved {:?}",
                    service_has_only_native_containers(&change.previous),
                    settings_documents_equivalent(&change.current, &change.previous),
                    super::verify::differs_only_in_externally_managed_fields(
                        &change.current,
                        &change.previous
                    ),
                    crate::settings::equivalence::saved_field_differences(
                        &change.current,
                        &change.previous
                    ),
                ));
            }
            let plan = reconciliation_push_plan_for_paths_with_prepared_settings(
                &observed, &desired, &paths, &prepared, true, false,
            );
            said.push(match &plan {
                Ok(plan) => plan_text(plan),
                Err(error) => format!("no plan: {error:#}"),
            });
            if let Ok(plan) = plan
                && !plan.is_empty()
            {
                let (mut prepared, _) = prepare()?;
                let mut args = crate::cli::PushEditorChangesArgs::new(
                    crate::cli::ProjectSourceArgs {
                        project_root: project_root.clone(),
                        src_root: PathBuf::from("src"),
                    },
                    crate::cli::BridgeConnectionArgs::local(0.1),
                );
                args.changed_paths.clone_from(&plan.changed_paths);
                args.target_settings_ids
                    .clone_from(&plan.target_settings_ids);
                args.exact_target_ids = true;
                args.targets_extend_paths = true;
                args.override_packages = true;
                let documents = prepared
                    .remove(&path)
                    .map(|change| ("Workspace".to_string(), Arc::new(change.current)))
                    .into_iter()
                    .collect::<HashMap<_, _>>();
                let initial = documents
                    .iter()
                    .filter(|(service, _)| plan.initial_native_services.contains(*service))
                    .map(|(service, document)| (service.clone(), Arc::clone(document)))
                    .collect::<HashMap<_, _>>();
                said.push(
                    match crate::editor::sync::collect_prepared_editor_changes(
                        &args,
                        documents,
                        plan.initial_native_services.clone(),
                    )
                    .map_err(|error| format!("collect: {error:#}"))
                    .and_then(|mut changes| {
                        super::push_plan::amend_reconciled_changes(&mut changes, plan)
                            .map_err(|error| format!("amend: {error:#}"))?;
                        super::push_plan::stage_native_bootstrap_services(&mut changes, &initial)
                            .map_err(|error| format!("stage: {error:#}"))?;
                        Ok(format!(
                            "{}\nnative {:?}",
                            request_text(&changes)?,
                            changes.native_property_documents.keys().collect::<Vec<_>>()
                        ))
                    }) {
                        Ok(sent) => sent,
                        Err(error) => format!("not sent: {error}"),
                    },
                );
            }
            let mut change = prepared.into_iter().next().map(|(_, change)| change);
            if let (Some(change), Some(SnapshotEntry::File(bytes))) =
                (&mut change, desired.entries.get(&path))
            {
                // Some of them first, as the fields beside an import are read.
                let some = (0..change.current.instances.len())
                    .filter(|index| index % 3 == seed as usize % 3)
                    .collect::<Vec<_>>();
                let partly =
                    crate::settings::equivalence::read_instances(&change.current, bytes, &some)
                        .map_err(|error| format!("read some: {error:#}"))?;
                for index in some {
                    change.current.instances[index] = partly.instances[index].clone();
                }
                crate::settings::equivalence::read_unread_instances(&mut change.current, bytes)
                    .map_err(|error| format!("read: {error:#}"))?;
            }
            Ok((said, change, unread))
        };
        let (expected, expected_change, _) = pushed(StoresRead::Whole)?;
        let (said, change, unread) = pushed(StoresRead::Changed)?;
        let _ = std::fs::remove_dir_all(&project_root);
        if said != expected {
            return Err(format!(
                "from the instances the push reads ({unread}):\n{}\nfrom the whole documents:\n{}",
                said.join("\n"),
                expected.join("\n")
            ));
        }
        match (&change, &expected_change) {
            (Some(change), Some(expected)) => {
                // An instance of Studio's copy the push left unread stands,
                // bare, for the whole one, which the files hold the same
                // under its id.
                let edited = (expected.current.instances.iter().enumerate())
                    .map(|(index, instance)| (instance.settings_id.as_str(), index))
                    .collect::<HashMap<_, _>>();
                let stands_for = |index: usize, read: &SettingsBytecodeInstance| {
                    let held = &expected.previous.instances[index];
                    let mut bare = held.bare();
                    if let Some(unique_id) = held.properties.get("UniqueId") {
                        bare.properties.insert("UniqueId".into(), unique_id.clone());
                    }
                    crate::settings::held::stand_unchanged(&mut bare.properties);
                    *read == bare
                        && edited.get(held.settings_id.as_str()).is_some_and(|at| {
                            settings_instances_equal(
                                &expected.current,
                                *at,
                                &expected.previous,
                                index,
                            )
                        })
                };
                if change.previous.version != expected.previous.version
                    || change.previous.instances.len() != expected.previous.instances.len()
                    || (change.previous.instances.iter().enumerate()).any(|(index, read)| {
                        *read != expected.previous.instances[index] && !stands_for(index, read)
                    })
                {
                    return Err("Studio's copy is another".to_string());
                }
                if let Some(index) = (0..expected.current.instances.len()).find(|&index| {
                    change.current.instances.get(index) != expected.current.instances.get(index)
                }) {
                    return Err(format!(
                        "read afterwards, instance {index} is\n{:?}\nand whole it is\n{:?}",
                        change.current.instances.get(index),
                        expected.current.instances[index]
                    ));
                }
                if change.current.instances.len() != expected.current.instances.len() {
                    return Err("read afterwards the files' copy has other instances".to_string());
                }
            }
            (None, None) => {}
            _ => return Err("only one of the two prepared a change".to_string()),
        }
        if kept {
            with_one_kept.fetch_add(1, Ordering::Relaxed);
        }
        if unread {
            in_part.fetch_add(1, Ordering::Relaxed);
            if studio.instances.len() > 1 {
                with_containers.fetch_add(1, Ordering::Relaxed);
            }
            if empty {
                emptied.fetch_add(1, Ordering::Relaxed);
            }
        } else {
            whole.fetch_add(1, Ordering::Relaxed);
        }
        Ok(())
    });
    if std::env::var_os("RENIUM_SIM_SEED").is_none() {
        let counts = [&in_part, &whole, &with_containers, &emptied]
            .map(|count| count.load(Ordering::Relaxed));
        assert!(
            counts[0] >= 100 && counts[2] >= 50 && counts[3] >= 5,
            "prepared in part {} ({} of them with containers in Studio, {} with an empty Studio), whole {}",
            counts[0],
            counts[2],
            counts[3],
            counts[1]
        );
    }
}

/// A capture leaves out the value a new instance of the class holds, which
/// the files may spell out. Only Studio's word for that value makes the two
/// one, for a value the plugin reads, and the captured instance stays as it
/// was captured.
#[test]
fn a_value_a_capture_left_out_compares_as_what_a_new_instance_holds() {
    let store = |fidelity: Option<&str>| {
        let root =
            SettingsBytecodeInstance::new("1".into(), "Workspace".into(), "Workspace".into(), None);
        let mut mesh =
            SettingsBytecodeInstance::new("2".into(), "Mesh".into(), "MeshPart".into(), Some(0));
        if let Some(name) = fidelity {
            let value = json!({"_type": "EnumItem", "name": name});
            mesh.properties.insert("CollisionFidelity".into(), value);
        }
        SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: vec![root, mesh],
        }
    };
    let (desired, mut observed) = (store(Some("Default")), store(None));
    let studio_says = |name: &str| {
        let value = json!({"_type": "EnumItem", "enumType": "CollisionFidelity", "name": name});
        NewInstanceValues::knowing("MeshPart", "CollisionFidelity", value)
    };
    let mut differs = |values: NewInstanceValues<'_>| {
        values
            .mismatch(None, (&desired, 1), (&mut observed, 1))
            .is_some()
    };
    assert!(!differs(studio_says("Default")));
    assert!(differs(studio_says("Box")));
    assert!(differs(NewInstanceValues::default()));
    assert!(observed.instances[1].properties.is_empty());

    let database = rbx_reflection_database::get().unwrap();
    let plugin_reads = |class_name: &str, name: &str| {
        let native = crate::rbx::decode::native_property_filter(database, class_name);
        crate::rbx::decode::plugin_reads_captured_property(
            database,
            class_name,
            name,
            false,
            Some(&native.output_names),
        )
    };
    assert!(plugin_reads("MeshPart", "CollisionFidelity"));
    assert!(!plugin_reads("Part", "Transparency"));
}

/// Moves the last instance of a store to `ahead`, as Studio holds a terrain
/// of its own ahead of the service's other children. Only a child of the
/// root that no other instance shares its name with is moved.
fn holds_last_child_ahead(store: &mut SettingsBytecode, ahead: usize) -> bool {
    fn place_references(value: &mut Value, place: &dyn Fn(usize) -> usize) {
        match value {
            Value::Object(fields) => {
                if let Some(index) = fields.get("instanceIndex").and_then(Value::as_u64) {
                    fields.insert("instanceIndex".into(), json!(place(index as usize - 1) + 1));
                }
                for value in fields.values_mut() {
                    place_references(value, place);
                }
            }
            Value::Array(items) => {
                for value in items {
                    place_references(value, place);
                }
            }
            _ => {}
        }
    }
    let last = store.instances.len() - 1;
    if last <= ahead
        || store.instances[last].parent_index != Some(0)
        || store.instances[..last]
            .iter()
            .any(|instance| instance.name == store.instances[last].name)
    {
        return false;
    }
    let place = |index: usize| match index {
        index if index == last => ahead,
        index if index >= ahead => index + 1,
        index => index,
    };
    let instance = store.instances.pop().unwrap();
    store.instances.insert(ahead, instance);
    for instance in &mut store.instances {
        instance.parent_index = instance.parent_index.map(place);
        let values = instance.properties.values_mut();
        for value in values.chain(instance.attributes.values_mut()) {
            place_references(value, &place);
        }
    }
    true
}

/// A large store is verified a range of rows at a time, with neither copy
/// whole in memory. Whatever Studio holds afterwards, that verification says
/// what the whole documents say, and the store it settles holds what theirs
/// holds: every instance under the files' id, with Studio's values, the
/// files' value of a field the push left pending, and the files' viewport
/// camera.
#[test]
fn a_pushed_store_verified_by_its_rows_is_verified_as_its_whole_documents_verify_it() {
    use crate::editor::types::PendingImportFields;
    use crate::tests::support::{SimRng, run_seeds};
    use std::sync::atomic::{AtomicUsize, Ordering};

    let (by_changed, reordered) = (AtomicUsize::new(0), AtomicUsize::new(0));
    let (
        verified,
        refused,
        with_pending,
        with_camera,
        settled,
        left_out_verified,
        left_out_refused,
    ) = (
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
    );
    run_seeds(300, |seed, _| {
        let mut rng = SimRng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
        let mut files = changed_instances_store(&mut rng);
        // A reference by an id alone, as a store holds one whose path nobody
        // recorded.
        if rng.below(3) == 0 {
            for instance in &mut files.instances {
                if let Some(pointer) = instance
                    .properties
                    .get_mut("Pointer")
                    .and_then(Value::as_object_mut)
                {
                    pointer.remove("pathSegments");
                    pointer.remove("pathOrdinals");
                }
            }
        }
        let files = files;
        // Studio before the push: a place of its own, or the files' store
        // with a few other values.
        let mut before = files.clone();
        if rng.below(2) == 0 {
            before
                .instances
                .truncate(1 + usize::from(files.instances[1].class_name == "Camera"));
            before.instances[0]
                .properties
                .insert("Gravity".into(), json!(50.0));
        } else {
            for _ in 0..1 + rng.below(3) {
                change_an_instance(&mut rng, &mut before);
            }
        }
        // Studio after the push: the files' instances under ids and
        // UniqueIds of its own.
        let mut after = files.clone();
        let own_ids = rng.below(4) != 0;
        let renamed = after
            .instances
            .iter()
            .enumerate()
            .map(|(index, instance)| {
                let id = if own_ids && instance.settings_id.starts_with("debug:") {
                    format!("debug:9_{}", 300 + index)
                } else {
                    instance.settings_id.clone()
                };
                (instance.settings_id.clone(), id)
            })
            .collect::<HashMap<_, _>>();
        let placed = after
            .instances
            .iter()
            .map(|instance| renamed[&instance.settings_id].clone())
            .collect::<Vec<_>>();
        for (index, instance) in after.instances.iter_mut().enumerate() {
            instance
                .settings_id
                .clone_from(&renamed[&instance.settings_id]);
            // Studio states by an id a reference the files state by the
            // place of its instance.
            if rng.below(5) == 0
                && let Some(place) = instance
                    .properties
                    .get("Target")
                    .and_then(|target| target.get("instanceIndex"))
                    .and_then(Value::as_u64)
                && let Some(id) = placed.get(place as usize - 1)
            {
                instance
                    .properties
                    .insert("Target".into(), json!({"_type": "Ref", "settingsId": id}));
            }
            if instance.properties.contains_key("UniqueId") && rng.below(3) != 0 {
                instance.properties.insert(
                    "UniqueId".into(),
                    json!({"_type": "UniqueId", "value": format!("{:024x}{:08x}", 11, index + 1)}),
                );
            }
            // A reference by an id names the instance under Studio's id.
            if let Some(pointer) = instance.properties.get_mut("Pointer")
                && let Some(id) = pointer.get("settingsId").and_then(Value::as_str)
                && let Some(own) = renamed.get(id)
            {
                pointer["settingsId"] = json!(own);
            }
        }
        let mut pending = Vec::new();
        let mut left_out = None;
        let kind = rng.below(10);
        let last = after.instances.len() - 1;
        match kind {
            // The capture left out a value the files spell out: the one a
            // new instance of the class holds, or another.
            9 => {
                let index = 1 + rng.below(last);
                if let Some(value) = after.instances[index].properties.remove("Transparency") {
                    let same = rng.below(3) != 0;
                    let held = if same { value } else { json!(0.0625) };
                    let class_name = &files.instances[index].class_name;
                    let values = NewInstanceValues::knowing(class_name, "Transparency", held);
                    left_out = Some((values, same));
                }
            }
            // One value is another.
            0 => {
                after.instances[1 + rng.below(last)]
                    .properties
                    .insert("Transparency".into(), json!(0.875));
            }
            // A field the push reported pending is not there yet.
            1 | 2 | 8 => {
                let index = 1 + rng.below(last);
                let unique = files
                    .instances
                    .iter()
                    .filter(|other| {
                        other.parent_index == files.instances[index].parent_index
                            && other.name == files.instances[index].name
                    })
                    .count()
                    == 1;
                if unique
                    && files.instances[index].parent_index == Some(0)
                    && let Some(value) = after.instances[index].properties.remove("Transparency")
                {
                    pending.push(PendingImportFields {
                        service: "Workspace".into(),
                        settings_id: (kind == 1)
                            .then(|| files.instances[index].settings_id.clone()),
                        class_name: files.instances[index].class_name.clone(),
                        path_segments: vec![
                            "Workspace".into(),
                            files.instances[index].name.clone(),
                        ],
                        path_ordinals: vec![1, 1],
                        properties: Map::from_iter([("Transparency".to_string(), value)]),
                    });
                }
            }
            // An instance is missing.
            3 => {
                let removed = after.instances.len() - 1;
                if after
                    .instances
                    .iter()
                    .all(|instance| instance.parent_index != Some(removed))
                {
                    after.instances.pop();
                }
            }
            // The viewport camera holds what Studio shows.
            4 | 5 => {
                if let Some(camera) = after
                    .instances
                    .iter_mut()
                    .find(|instance| instance.class_name == "Camera")
                {
                    camera.properties.insert("FieldOfView".into(), json!(33.0));
                }
            }
            // Studio still holds a value the files no longer state.
            7 => {
                let index = 1 + rng.below(last);
                if let Some(held) = before.instances.iter_mut().find(|instance| {
                    instance.settings_id == files.instances[index].settings_id
                        && instance.properties.contains_key("Anchored")
                }) {
                    held.properties.insert("Reflectance".into(), json!(0.5));
                    after.instances[index]
                        .properties
                        .insert("Reflectance".into(), json!(0.5));
                }
            }
            // Two instances hold each other's values.
            6 => {
                let (left, right) = (1 + rng.below(last), 1 + rng.below(last));
                if left != right {
                    let (left_values, right_values) = (
                        after.instances[left].properties.clone(),
                        after.instances[right].properties.clone(),
                    );
                    after.instances[left].properties = right_values;
                    after.instances[right].properties = left_values;
                }
            }
            _ => {}
        }
        let ahead = 1 + usize::from(files.instances[1].class_name == "Camera");
        let moved = rng.below(3) == 0 && holds_last_child_ahead(&mut after, ahead);
        let path = PathBuf::from("src/Workspace/__roblox_sync_settings.renium");
        let entry = |document: &SettingsBytecode| {
            encode_settings_bytecode(document)
                .map(SnapshotEntry::File)
                .map_err(|error| format!("{error:#}"))
        };
        let (Ok(desired), Ok(observed), Ok(studio)) =
            (entry(&files), entry(&after), entry(&before))
        else {
            return Ok(());
        };
        let snapshot = |entry: &SnapshotEntry| ProjectSnapshot {
            entries: BTreeMap::from([(path.clone(), entry.clone())]),
        };
        let (project, held) = (snapshot(&desired), snapshot(&studio));
        let paths = project_replacement_paths(&project, &held);
        // Studio's copy from before whole, or read by the instances the
        // push changed.
        let read = if seed % 3 == 0 {
            StoresRead::ChangedOfBoth
        } else {
            StoresRead::Whole
        };
        let prepared = || {
            let mut prepared = HashMap::new();
            prepare_project_replacement_from(&project, &held, &paths, &mut prepared, read)
                .map_err(|error| format!("prepare: {error:#}"))?;
            Ok::<_, String>(
                prepared
                    .remove(&path)
                    .map(|change| PreparedPushVerification {
                        previous: change.previous,
                        desired: (seed % 2 == 0).then(|| Arc::new(change.current)),
                    }),
            )
        };
        let settle = rng.below(4) != 0 && read == StoresRead::Whole;
        let changed_alone = !settle
            && pending.is_empty()
            && prepared()?.is_some_and(|prepared| {
                crate::settings::equivalence::holds_unread_instances(&prepared.previous)
            });
        // A store the push asked nothing of: Studio held it as the files do.
        let unasked = settle && seed % 5 == 4;
        let outcome = |whole_documents: bool| {
            let prepared = match unasked {
                true => None,
                false => match prepared()? {
                    Some(prepared) => Some(prepared),
                    None => return Ok(None),
                },
            };
            super::verify::settings_delta_outcome(
                &path,
                (
                    if unasked { &desired } else { &studio },
                    &desired,
                    &observed,
                ),
                (prepared, !unasked),
                (&pending, left_out.as_ref().map(|(values, _)| values)),
                (settle, whole_documents),
            )
            .map(Some)
            .map_err(|error| format!("{error:#}"))
        };
        let (by_rows, whole) = (outcome(false), outcome(true));
        let (by_rows, whole) = match (by_rows, whole) {
            (Ok(Some(by_rows)), Ok(Some(whole))) => (by_rows, whole),
            (Ok(None), Ok(None)) => return Ok(()),
            (Err(by_rows), Err(whole)) if by_rows == whole => return Ok(()),
            (by_rows, whole) => {
                return Err(format!(
                    "by rows {:?}, whole {:?}",
                    by_rows.map(|outcome| outcome.map(|outcome| outcome.0)),
                    whole.map(|outcome| outcome.map(|outcome| outcome.0))
                ));
            }
        };
        if by_rows.0 != whole.0 {
            return Err(format!(
                "by rows the store says {:?}, whole it says {:?}",
                by_rows.0, whole.0
            ));
        }
        match (&left_out, whole.0.is_some()) {
            (Some((_, true)), false) => left_out_verified.fetch_add(1, Ordering::Relaxed),
            (Some((_, false)), true) => left_out_refused.fetch_add(1, Ordering::Relaxed),
            _ => 0,
        };
        if changed_alone {
            by_changed.fetch_add(1, Ordering::Relaxed);
        }
        if whole.0.is_some() {
            refused.fetch_add(1, Ordering::Relaxed);
        } else {
            verified.fetch_add(1, Ordering::Relaxed);
            if moved {
                reordered.fetch_add(1, Ordering::Relaxed);
            }
            if !pending.is_empty() {
                with_pending.fetch_add(1, Ordering::Relaxed);
            }
            if matches!(kind, 4 | 5) {
                with_camera.fetch_add(1, Ordering::Relaxed);
            }
        }
        match (by_rows.1, whole.1) {
            (Some(by_rows), Some(whole)) => {
                let document = |bytes: Vec<u8>| {
                    settings_document(Some(&SnapshotEntry::File(bytes)))
                        .map_err(|error| format!("{error:#}"))
                };
                let (by_rows, whole) = (document(by_rows)?, document(whole)?);
                if let Some(index) = (0..whole.instances.len().max(by_rows.instances.len()))
                    .find(|&index| by_rows.instances.get(index) != whole.instances.get(index))
                {
                    return Err(format!(
                        "settled by rows, instance {index} is\n{:?}\nand settled whole it is\n{:?}",
                        by_rows.instances.get(index),
                        whole.instances.get(index)
                    ));
                }
                settled.fetch_add(1, Ordering::Relaxed);
            }
            (None, None) => {}
            (by_rows, whole) => {
                return Err(format!(
                    "by rows a store is settled: {}, whole: {}",
                    by_rows.is_some(),
                    whole.is_some()
                ));
            }
        }
        Ok(())
    });
    if std::env::var_os("RENIUM_SIM_SEED").is_none() {
        let counts = [
            &verified,
            &refused,
            &with_pending,
            &with_camera,
            &settled,
            &left_out_verified,
            &left_out_refused,
            &by_changed,
        ]
        .map(|count| count.load(Ordering::Relaxed));
        let reordered = reordered.load(Ordering::Relaxed);
        assert!(reordered >= 10, "{reordered} verified in another order");
        let unprepared = super::verify::rows::UNPREPARED.load(Ordering::Relaxed);
        assert!(
            unprepared >= 10,
            "{unprepared} verified without a copy from before"
        );
        assert!(
            counts[0] >= 80
                && counts[1] >= 20
                && counts[2] >= 3
                && counts[3] >= 10
                && counts[4] >= 60
                && counts[5] >= 1
                && counts[6] >= 1
                && counts[7] >= 8,
            "{} judged from the instances the push changed, verified {} ({} with a pending field, {} with another viewport camera, {} settled, {} with a value the capture left out), refused {} ({} for a value the capture left out)",
            counts[7],
            counts[0],
            counts[2],
            counts[3],
            counts[4],
            counts[5],
            counts[1],
            counts[6]
        );
    }
}

/// Studio holds two namesakes in the other order behind a terrain of its
/// own, and the value a push asked of one of them did not arrive: read by
/// its rows, the store is refused as its whole documents refuse it.
#[test]
fn a_lost_write_among_reordered_namesakes_is_refused_by_rows() {
    let store = |rows: &[(&str, &str, f64, f64)]| {
        let mut instances = vec![SettingsBytecodeInstance::new(
            "debug:0".into(),
            "Workspace".into(),
            "Workspace".into(),
            None,
        )];
        for (index, (name, class, transparency, reflectance)) in rows.iter().enumerate() {
            let id = format!("debug:{}", index + 1);
            let mut instance =
                SettingsBytecodeInstance::new(id, (*name).into(), (*class).into(), Some(0));
            if *class == "Part" {
                let values = &mut instance.properties;
                values.insert("Transparency".into(), json!(transparency));
                values.insert("Reflectance".into(), json!(reflectance));
            }
            instances.push(instance);
        }
        let document = SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances,
        };
        SnapshotEntry::File(encode_settings_bytecode(&document).unwrap())
    };
    let terrain = ("Terrain", "Terrain", 0.0, 0.0);
    let before = store(&[("A", "Part", 0.0, 0.25), ("A", "Part", 0.5, 0.75), terrain]);
    let files = store(&[("A", "Part", 0.5, 0.25), ("A", "Part", 0.5, 0.75), terrain]);
    let after = store(&[terrain, ("A", "Part", 0.5, 0.75), ("A", "Part", 0.0, 0.25)]);
    let path = PathBuf::from("src/Workspace/__roblox_sync_settings.renium");
    let snapshot = |entry: &SnapshotEntry| ProjectSnapshot {
        entries: BTreeMap::from([(path.clone(), entry.clone())]),
    };
    let (project, held) = (snapshot(&files), snapshot(&before));
    let paths = project_replacement_paths(&project, &held);
    let outcome = |whole_documents: bool| {
        let mut prepared = HashMap::new();
        prepare_project_replacement_from(&project, &held, &paths, &mut prepared, StoresRead::Whole)
            .unwrap();
        let change = prepared.remove(&path).unwrap();
        let prepared = PreparedPushVerification {
            previous: change.previous,
            desired: None,
        };
        super::verify::settings_delta_outcome(
            &path,
            (&before, &files, &after),
            (Some(prepared), true),
            (&[], None),
            (false, whole_documents),
        )
        .unwrap()
        .0
    };
    let (by_rows, whole) = (outcome(false), outcome(true));
    assert!(whole.is_some(), "the whole documents verify a lost write");
    assert_eq!(by_rows, whole);
}

/// A script file the store knows nothing of adds its instance to the store,
/// and an edited store is written whole: the instances a document left in
/// its store file are read before the edit, and the import takes the edited
/// document.
#[test]
fn a_new_script_in_a_store_read_in_part_is_written_with_every_instance() {
    let mut instances = vec![SettingsBytecodeInstance::new(
        "1".into(),
        "Workspace".into(),
        "Workspace".into(),
        None,
    )];
    for index in 0..16 {
        let mut part = SettingsBytecodeInstance::new(
            format!("part-{index}"),
            format!("Part{index}"),
            "Part".into(),
            Some(0),
        );
        part.properties
            .insert("Transparency".into(), json!(index as f64 / 16.0));
        instances.push(part);
    }
    let files = SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances,
    };
    let studio = SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances: vec![SettingsBytecodeInstance::new(
            "debug:0_1".into(),
            "Workspace".into(),
            "Workspace".into(),
            None,
        )],
    };
    let path = PathBuf::from("src/Workspace/__roblox_sync_settings.renium");
    let snapshot = |document: &SettingsBytecode| ProjectSnapshot {
        entries: BTreeMap::from([(
            path.clone(),
            SnapshotEntry::File(encode_settings_bytecode(document).unwrap()),
        )]),
    };
    let (desired, observed) = (snapshot(&files), snapshot(&studio));
    let paths = project_replacement_paths(&desired, &observed);
    let mut prepared = HashMap::new();
    prepare_project_replacement_from(
        &desired,
        &observed,
        &paths,
        &mut prepared,
        StoresRead::Changed,
    )
    .unwrap();
    let current = prepared.remove(&path).unwrap().current;
    assert!(crate::settings::equivalence::holds_unread_instances(
        &current
    ));
    let project_root = crate::tests::support::temp_dir("new-script-in-a-store-read-in-part");
    let store = project_root.join(&path);
    std::fs::create_dir_all(store.parent().unwrap()).unwrap();
    let SnapshotEntry::File(bytes) = &desired.entries[&path] else {
        unreachable!()
    };
    std::fs::write(&store, bytes).unwrap();
    let script = PathBuf::from("src/Workspace/Fresh.server.luau");
    std::fs::write(project_root.join(&script), "print(1)\n").unwrap();
    let mut args = crate::cli::PushEditorChangesArgs::new(
        crate::cli::ProjectSourceArgs {
            project_root: project_root.clone(),
            src_root: PathBuf::from("src"),
        },
        crate::cli::BridgeConnectionArgs::local(0.1),
    );
    args.changed_paths = vec![path.clone(), script];
    args.override_packages = true;
    let changes = crate::editor::sync::collect_prepared_editor_changes(
        &args,
        HashMap::from([("Workspace".to_string(), Arc::new(current))]),
        HashSet::from(["Workspace".to_string()]),
    )
    .unwrap();
    let _ = std::fs::remove_dir_all(&project_root);
    let written = &changes
        .settings_writes
        .first()
        .expect("the store with the new script is written")
        .document;
    assert_eq!(written.instances.len(), files.instances.len() + 1);
    assert!(!crate::settings::equivalence::holds_unread_instances(
        written
    ));
    for (index, instance) in files.instances.iter().enumerate().skip(1) {
        assert_eq!(
            written.instances[index].properties.get("Transparency"),
            instance.properties.get("Transparency")
        );
    }
    let imported = &changes.native_property_documents["Workspace"];
    assert!(!crate::settings::equivalence::holds_unread_instances(
        imported
    ));
    assert_eq!(imported.instances.len(), files.instances.len() + 1);
}

#[cfg(any(windows, target_os = "macos"))]
#[test]
fn a_scoped_answer_reads_its_services_their_touched_instances_and_unknowns() {
    let answer = json!({
        "services": ["ReplicatedStorage", "ServerStorage", "Workspace"],
        "touched": {
            "ReplicatedStorage": [
                {"pathSegments": ["ReplicatedStorage", "Box"], "pathOrdinals": [1, 2], "kind": "children"},
            ],
            "ServerStorage": [{"pathSegments": ["ServerStorage"], "pathOrdinals": [1]}],
            "Workspace": "whole",
        },
    });
    let ProofScope::Services { services, touched } = ProofScope::parse(&answer) else {
        panic!("a scoped answer read as unknown");
    };
    assert_eq!(
        services,
        BTreeSet::from(["ReplicatedStorage", "ServerStorage", "Workspace"].map(String::from))
    );
    assert_eq!(
        touched["ReplicatedStorage"],
        Some(vec![TouchedInstance {
            path_segments: vec!["ReplicatedStorage".into(), "Box".into()],
            path_ordinals: vec![1, 2],
            kind: "children".into(),
            fields: None,
        }])
    );
    assert_eq!(
        touched["ServerStorage"], None,
        "an entry it cannot read leaves the whole service"
    );
    assert_eq!(touched["Workspace"], None);
    assert_eq!(
        ProofScope::parse(&json!({"unknown": "proof retired"})),
        ProofScope::Unknown("proof retired".into())
    );
    assert!(matches!(
        ProofScope::parse(&Value::Null),
        ProofScope::Unknown(_)
    ));
}

/// Values alone moved no path another store names, so no store is read to
/// look for one: a search for no service would read each of them whole.
#[cfg(any(windows, target_os = "macos"))]
#[test]
fn no_store_is_read_for_what_refers_to_no_service() -> Result<()> {
    let store = PathBuf::from("instances/Workspace.renium");
    let project = ProjectSnapshot {
        entries: BTreeMap::from([(store.clone(), SnapshotEntry::File(b"no store".to_vec()))]),
    };
    let stores = BTreeMap::from([("Workspace".to_string(), store)]);
    assert!(referring_services(&project, &stores, &BTreeSet::new())?.is_empty());
    let lighting = BTreeSet::from(["Lighting".to_string()]);
    assert!(referring_services(&project, &stores, &lighting).is_err());
    Ok(())
}

/// A pull answered from a baseline writes no sourcemap for the services it
/// does not capture, so it answers only while the sourcemap is the one the
/// last pull on the baseline left. A push of script sources keeps that
/// sourcemap where Studio holds them byte for byte; one of a store or of
/// other files does not, nor does a settlement that kept files back.
#[cfg(any(windows, target_os = "macos"))]
#[test]
fn a_baseline_answers_a_pull_only_while_the_sourcemap_is_as_its_pull_left_it() -> Result<()> {
    let root = crate::tests::support::temp_dir("sourcemap-as-left");
    let sourcemap = PathBuf::from("sourcemap.json");
    fs::write(root.join(&sourcemap), b"{\"name\":\"left\"}")?;
    let mut published = crate::snapshot::export::PublishedProjectChanges::default();
    let kept = Box::new(BTreeMap::from([(
        sourcemap.clone(),
        crate::snapshot::export::file_publish_state(b"{\"name\":\"kept\"}"),
    )]));
    assert!(
        settled_sourcemap(&published, Some(kept.clone())) == Some(kept),
        "a pull that wrote no sourcemap keeps the record it had"
    );
    published.publish_paths = vec![PathBuf::from("src"), sourcemap.clone()];
    published.settled =
        crate::snapshot::export::collect_publish_hashes(&root, std::slice::from_ref(&sourcemap))?;
    let (module, store) = (
        PathBuf::from("src/ReplicatedStorage/Module.luau"),
        crate::system::files::service_settings_path(Path::new("src/ReplicatedStorage")),
    );
    let project = |module_bytes: &[u8], store_bytes: &[u8]| ProjectSnapshot {
        entries: BTreeMap::from([
            (module.clone(), SnapshotEntry::File(module_bytes.to_vec())),
            (store.clone(), SnapshotEntry::File(store_bytes.to_vec())),
        ]),
    };
    let mut held = VerifiedFullPush {
        connections: Vec::new(),
        input: Value::Null,
        project: project(b"return 1", b"store"),
        paths: vec![PathBuf::from("src")],
        proof: Value::Null,
        expires: Instant::now(),
        sourcemap: settled_sourcemap(&published, None),
        #[cfg(windows)]
        relay: None,
    };
    assert!(held.sourcemap.is_some() && sourcemap_as_left(&root, &held)?);
    let verified = |inexact: u64| Map::from_iter([("sourceInexact".to_string(), json!(inexact))]);
    let exact = verified(0);
    assert!(sourcemap_after_push(&held, &project(b"return 2", b"store"), &exact) == held.sourcemap);
    assert!(
        sourcemap_after_push(&held, &project(b"return 2\n", b"store"), &verified(1)).is_none(),
        "Studio holds the script without the file's final line break"
    );
    assert!(sourcemap_after_push(&held, &project(b"return 2", b"store"), &Map::new()).is_none());
    assert!(sourcemap_after_push(&held, &held.project, &Map::new()) == held.sourcemap);
    assert!(sourcemap_after_push(&held, &project(b"return 1", b"renamed"), &exact).is_none());
    let mut added = project(b"return 1", b"store");
    added.entries.insert(
        PathBuf::from("src/ReplicatedStorage/Other.luau"),
        SnapshotEntry::File(b"return 3".to_vec()),
    );
    assert!(sourcemap_after_push(&held, &added, &exact).is_none());
    let pulled = super::import_settle::pulled_sourcemap;
    assert!(pulled(&published) == held.sourcemap);
    published.preserved.push(module.clone());
    assert!(pulled(&published).is_none(), "a file kept back");
    published.preserved.clear();
    published.read_only_links_restored.push(json!({}));
    assert!(pulled(&published).is_none(), "a link restored");
    published.read_only_links_restored.clear();
    published.studio_overrides.insert(module.clone(), None);
    assert!(pulled(&published).is_none(), "a Studio override kept");
    fs::write(root.join(&sourcemap), b"{\"name\":\"edited\"}")?;
    assert!(!sourcemap_as_left(&root, &held)?);
    fs::remove_file(root.join(&sourcemap))?;
    assert!(!sourcemap_as_left(&root, &held)?);
    held.sourcemap = None;
    assert!(!sourcemap_as_left(&root, &held)?);
    Ok(())
}
