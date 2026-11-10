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
    let mut conflicts = Vec::new();
    let _ = align_first_pairing(
        Path::new("instances/Workspace.renium"),
        editor,
        studio,
        ConflictPreference::None,
        &mut conflicts,
    );
    conflicts
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
    stage_native_bootstrap_services(&mut changes, &empty);
    assert_eq!(
        serde_json::to_value(&changes.instance_changes).unwrap(),
        before
    );
    stage_native_bootstrap_services(
        &mut changes,
        &HashMap::from([("Workspace".into(), Arc::clone(&document))]),
    );
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
            document: document("aligned-id"),
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
                        document: document.clone(),
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
fn retention_checks_distinguish_editor_state_and_authored_attributes() {
    for (class, name) in [
        ("Workspace", "InsertPoint"),
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
        conflicts: vec!["stale".into()],
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
        conflicts,
        [format!(
            "{}: Pointer: Value: refers to Workspace.Kit.Target (id=target), which the other side removed",
            replicated_path.display()
        )]
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
    let mut conflicts = Vec::new();
    align_first_pairing(
        Path::new("instances/Workspace.renium"),
        &mut editor,
        &mut studio,
        ConflictPreference::None,
        &mut conflicts,
    );
    assert!(conflicts.is_empty(), "{conflicts:?}");
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
        conflicts,
        [format!(
            "{}: Pointer: Value: refers to Workspace.Kit.Target (id=target), which the other side removed",
            replicated_path.display()
        )]
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
    let mut conflicts = Vec::new();
    pairing::protect_package_links(
        path,
        Some(&base),
        &mut unlinked.clone(),
        &mut unlinked.clone(),
        &mut conflicts,
    );
    assert!(conflicts.is_empty(), "{conflicts:?}");
    let mut conflicts = Vec::new();
    pairing::protect_package_links(
        path,
        Some(&base),
        &mut base.clone(),
        &mut unlinked.clone(),
        &mut conflicts,
    );
    assert_eq!(conflicts.len(), 1, "{conflicts:?}");
    assert!(conflicts[0].contains("removes PackageLink"));
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
            .any(|conflict| conflict.contains("different classes"))
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
    assert!(conflicts[0].contains("deleted here but modified on the other side"));

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
    assert!(conflicts[0].contains("modified here but deleted on the other side"));

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
    let (merged, conflicts) = merge_reconciliation_settings_documents(
        &base,
        &editor,
        &studio,
        ConflictPreference::None,
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
    assert!(conflicts.iter().any(|c| c.detail.contains("Text")));
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
    let mut first_pair_conflicts = Vec::new();
    align_first_pairing(
        Path::new("instances/Workspace.renium"),
        &mut editor.clone(),
        &mut first_pair,
        ConflictPreference::None,
        &mut first_pair_conflicts,
    );
    assert!(!first_pair_conflicts.is_empty());
    assert!(
        first_pair_conflicts.iter().all(
            |conflict| !conflict.contains("ambiguous") && !conflict.contains("different paths")
        ),
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
    let (merged, conflicts) = merge_reconciliation_settings_documents(
        &base,
        &editor,
        &studio,
        ConflictPreference::None,
        &HashSet::new(),
        &HashSet::new(),
    );
    assert_eq!(
        merged.instances.len(),
        3,
        "matched parts must not be duplicated"
    );
    assert!(
        !conflicts.is_empty(),
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
    let mut conflicts = Vec::new();
    align_first_pairing(
        Path::new("instances/Workspace.renium"),
        &mut editor.clone(),
        &mut studio,
        ConflictPreference::None,
        &mut conflicts,
    );
    assert_eq!(conflicts.len(), 1, "{conflicts:?}");
    assert!(conflicts[0].contains("AuthoredCamera"));
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
    let paths = snapshot_differences_prepared(&project, &studio, Some(&mut prepared)).unwrap();
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
            .any(|conflict| conflict.contains("PackageLink"))
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
            conflicts[0].contains("instances/Workspace.renium is missing")
                && conflicts[0].contains("`rbx pl`")
                && conflicts[0].contains("`rbx ps instances/Workspace.renium -u`"),
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
            .any(|conflict| conflict.contains("ReplicatedStorage.Shared.Util")),
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
            .any(|conflict| conflict.contains("ReplicatedStorage.Shared.Util")),
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
