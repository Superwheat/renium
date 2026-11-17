use super::*;

fn text(text: &str) -> SnapshotEntry {
    SnapshotEntry::File(text.as_bytes().to_vec())
}

fn record(value: Value) -> Map<String, Value> {
    value.as_object().unwrap().clone()
}

#[test]
fn a_sync_merges_only_the_paths_its_sides_hold_differently() {
    let baseline = file_snapshot(&[("a.txt", b"base"), ("b.txt", b"base")]);
    let editor = file_snapshot(&[("a.txt", b"files"), ("b.txt", b"files")]);
    let studio = file_snapshot(&[("a.txt", b"studio"), ("b.txt", b"studio")]);
    let known = HashSet::from([PathBuf::from("b.txt")]);
    let (merged, conflicts, changes) = merge_snapshots_with_changes(
        Some(&baseline),
        &editor,
        &studio,
        ConflictPreference::Studio,
        Some(&known),
    )
    .unwrap();
    assert!(conflicts.is_empty(), "{conflicts:?}");
    assert!(merged.entries[Path::new("a.txt")] == text("files"));
    assert!(merged.entries[Path::new("b.txt")] == text("studio"));
    assert_eq!(changes.editor, HashSet::from([PathBuf::from("b.txt")]));
    assert!(changes.studio.is_empty(), "{:?}", changes.studio);
}

#[test]
fn a_preference_settles_a_conflict_between_files_and_no_other() {
    let path = Path::new("src/Notes.txt");
    let (base, files, other) = (text("base"), text("files"), text("studio"));
    let folder = SnapshotEntry::Directory;
    // (baseline, files, Studio, first pairing, a preference settles it)
    let cases = [
        (None, Some(&files), Some(&other), false, true),
        (Some(&folder), Some(&files), Some(&other), true, true),
        (Some(&base), None, Some(&other), false, true),
        (Some(&base), Some(&files), None, false, true),
        (None, Some(&folder), Some(&other), true, false),
        (None, Some(&files), Some(&folder), true, false),
        (Some(&folder), None, Some(&other), false, false),
        (Some(&base), Some(&folder), Some(&other), false, false),
        (Some(&base), Some(&files), Some(&folder), false, false),
    ];
    for (index, (base, editor, studio, first_pairing, settles)) in cases.into_iter().enumerate() {
        for preference in [
            ConflictPreference::None,
            ConflictPreference::Editor,
            ConflictPreference::Studio,
        ] {
            let mut conflicts = Vec::new();
            let kept = merge_conflicting_entry(
                path,
                base,
                editor,
                studio,
                preference,
                first_pairing,
                &mut conflicts,
            );
            let settled = settles && !matches!(preference, ConflictPreference::None);
            let expected = if settled && matches!(preference, ConflictPreference::Studio) {
                studio
            } else {
                editor
            };
            assert!(kept.as_ref() == expected, "case {index} {preference:?}");
            assert_eq!(
                conflicts,
                if settled {
                    Vec::new()
                } else {
                    vec![format!("{} changed on both sides", path.display())]
                },
                "case {index} {preference:?}"
            );
        }
    }
}

#[test]
fn only_a_script_is_the_same_file_under_other_line_endings() {
    let (unix, windows) = (text("a\nb\n"), text("a\r\nb\r\n"));
    let same = |path: &str, left, right| entries_equivalent(Path::new(path), left, right);
    assert!(same("src/A.luau", Some(&unix), Some(&windows)));
    assert!(!same("src/A.txt", Some(&unix), Some(&windows)));
    assert!(same("src/A.txt", Some(&unix), Some(&unix)));
    let folder = SnapshotEntry::Directory;
    assert!(same("src", Some(&folder), Some(&folder)));
    assert!(same("src", None, None));
    assert!(!same("src", Some(&folder), None));
    assert!(!same("src/A.luau", Some(&unix), Some(&folder)));
}

#[test]
fn a_message_cuts_a_value_longer_than_96_characters() {
    assert_eq!(short_reconciliation_value(None), "<absent>");
    let fits = json!("a".repeat(94));
    assert_eq!(short_reconciliation_value(Some(&fits)), fits.to_string());
    let long = json!("a".repeat(95));
    assert_eq!(
        short_reconciliation_value(Some(&long)),
        format!("\"{}...", "a".repeat(92))
    );
}

#[test]
fn the_first_difference_of_two_records_names_the_field_and_both_values() {
    let difference = |observed: (Value, Value), expected: (Value, Value)| {
        first_record_difference(
            "Part",
            (&record(observed.0), &record(observed.1)),
            (&record(expected.0), &record(expected.1)),
        )
    };
    let same = (json!({"Transparency": 0.5}), json!({"Kind": "a"}));
    assert_eq!(difference(same.clone(), same), None);
    assert_eq!(
        difference(
            (json!({"Transparency": 0.5}), json!({})),
            (json!({"Transparency": 0}), json!({}))
        )
        .as_deref(),
        Some("Studio Transparency is 0.5; the files have 0")
    );
    // The engine writes a script's guid and every RBX_ attribute itself.
    assert_eq!(
        difference(
            (json!({"ScriptGuid": "{a}"}), json!({"RBX_Stamp": 1})),
            (json!({"ScriptGuid": "{b}"}), json!({"RBX_Stamp": 2}))
        ),
        None
    );
    assert_eq!(
        difference(
            (json!({}), json!({"Kind": "a"})),
            (json!({}), json!({"Kind": "b"}))
        )
        .as_deref(),
        Some("Studio attribute Kind is \"a\"; the files have \"b\"")
    );
    assert_eq!(
        difference((json!({}), json!({})), (json!({}), json!({"Kind": "b"}))).as_deref(),
        Some("Studio attribute Kind is <absent>; the files have \"b\"")
    );
}

#[test]
fn the_three_way_difference_says_which_side_changed_the_value() {
    let store = |value: &str| {
        let mut document = twin_document();
        twin_add(
            &mut document,
            0,
            "Flag",
            "StringValue",
            json!({"Value": value}),
        );
        document
    };
    let base = store("base");
    assert_eq!(
        first_three_way_property_difference(&base, &base, &base),
        None
    );
    assert_eq!(
        first_three_way_property_difference(&base, &store("files"), &base).as_deref(),
        Some(
            "Flag (editor-1) Value: baseline=\"base\" editor=\"files\" studio=\"base\" editor_changed=true studio_changed=false"
        )
    );
    assert_eq!(
        first_three_way_property_difference(&base, &base, &store("studio")).as_deref(),
        Some(
            "Flag (editor-1) Value: baseline=\"base\" editor=\"base\" studio=\"studio\" editor_changed=false studio_changed=true"
        )
    );
}

#[test]
fn a_store_only_the_files_have_is_kept_and_goes_to_studio() {
    let store = PathBuf::from("src/ServerStorage/__roblox_sync_settings.renium");
    let document = SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances: vec![
            reference_merge_instance("root", "ServerStorage", "ServerStorage", None, None),
            reference_merge_instance("a", "Kept", "Folder", Some(0), None),
        ],
    };
    let entry = SnapshotEntry::File(encode_settings_bytecode(&document).unwrap());
    let other = file_snapshot(&[("notes.txt", b"shared")]);
    let mut editor = other.clone();
    editor.entries.insert(store.clone(), entry);
    let (merged, conflicts, changes) = merge_snapshots_with_changes(
        Some(&other),
        &editor,
        &other,
        ConflictPreference::None,
        None,
    )
    .unwrap();
    assert!(conflicts.is_empty(), "{conflicts:?}");
    let kept = settings_document(merged.entries.get(&store)).unwrap();
    assert!(settings_documents_equivalent(&kept, &document));
    assert!(changes.studio.contains(&store));
    assert!(!changes.editor.contains(&store));
}

#[test]
fn a_reused_identity_another_store_refers_to_keeps_its_id() {
    let workspace = PathBuf::from("src/Workspace/__roblox_sync_settings.renium");
    let storage = PathBuf::from("src/ReplicatedStorage/__roblox_sync_settings.renium");
    let instance = reference_merge_instance;
    let reference = json!({
        "_type": "Ref",
        "settingsId": "2",
        "pathSegments": ["Workspace", "New"],
        "pathOrdinals": [1, 1],
    });
    let shapes = [
        Some(reference.clone()),
        Some(json!([reference])),
        Some(json!({"Target": reference})),
        Some(json!({"Ref": {"settingsId": "2"}})),
        None,
    ];
    for shape in shapes {
        let holder = instance("holder", "Holder", "ObjectValue", Some(0), shape.as_ref());
        let holders = reference_merge_store(vec![
            instance("rs", "ReplicatedStorage", "ReplicatedStorage", None, None),
            holder,
        ]);
        let snapshot = |child: (&str, &str)| ProjectSnapshot {
            entries: [
                (
                    workspace.clone(),
                    reference_merge_store(vec![
                        instance("ws", "Workspace", "Workspace", None, None),
                        instance("2", child.0, child.1, Some(0), None),
                    ]),
                ),
                (storage.clone(), holders.clone()),
            ]
            .into_iter()
            .collect(),
        };
        let baseline = snapshot(("Old", "Folder"));
        let editor = snapshot(("New", "Part"));
        let (merged, conflicts) = merge_snapshots(
            Some(&baseline),
            &editor,
            &baseline,
            ConflictPreference::None,
        )
        .unwrap();
        assert!(conflicts.is_empty(), "{shape:?}: {conflicts:?}");
        let merged = settings_document(merged.entries.get(&workspace)).unwrap();
        let part = merged
            .instances
            .iter()
            .find(|instance| instance.name == "New")
            .unwrap();
        // Unreferenced, the reused id is released so that the part is a new
        // instance beside the removed folder.
        assert_eq!(part.settings_id == "2", shape.is_some(), "{shape:?}");
    }
}

fn addition(id: &str, name: &str, class_name: &str, unique: bool) -> SettingsBytecodeInstance {
    let mut instance = reference_merge_instance(id, name, class_name, Some(0), None);
    if unique {
        instance.properties.insert(
            "UniqueId".into(),
            json!({"_type": "UniqueId", "value": "0123456789abcdef0123456789abcdef"}),
        );
    }
    instance
}

/// The names both sides' additions to an empty store merge to, the files
/// preferred.
fn merged_additions(
    editor: Vec<SettingsBytecodeInstance>,
    studio: Vec<SettingsBytecodeInstance>,
) -> Vec<String> {
    let store = |additions: Vec<SettingsBytecodeInstance>| {
        let mut document = twin_document();
        document.instances.extend(additions);
        document
    };
    let (merged, conflicts) = merge_reconciliation_settings_documents(
        &store(Vec::new()),
        &store(editor),
        &store(studio),
        ConflictPreference::Editor,
        &HashSet::new(),
        &HashSet::new(),
    );
    assert!(
        conflicts.is_empty(),
        "{:?}",
        conflicts
            .iter()
            .map(|conflict| format!("{}: {}", conflict.path, conflict.detail))
            .collect::<Vec<_>>()
    );
    let mut names = merged
        .instances
        .iter()
        .skip(1)
        .map(|instance| instance.name.clone())
        .collect::<Vec<_>>();
    names.sort();
    names
}

#[test]
fn an_addition_both_sides_hold_under_one_unique_id_is_one_instance() {
    // One side renamed it since.
    assert_eq!(
        merged_additions(
            vec![addition("x", "A", "Part", true)],
            vec![addition("x", "B", "Part", true)]
        ),
        ["A"]
    );
}

#[test]
fn additions_that_share_only_their_id_stay_apart() {
    assert_eq!(
        merged_additions(
            vec![addition("x", "A", "Folder", false)],
            vec![addition("x", "B", "Folder", false)]
        ),
        ["A", "B"]
    );
    // A unique id Studio holds twice names no instance.
    assert_eq!(
        merged_additions(
            vec![addition("x", "A", "Part", true)],
            vec![
                addition("x", "B", "Part", true),
                addition("y", "C", "Part", true)
            ]
        ),
        ["A", "B", "C"]
    );
    // The files hold the unique id under another identity: the instance
    // that reuses Studio's identity there is another one.
    assert_eq!(
        merged_additions(
            vec![
                addition("z", "A", "Part", true),
                addition("x", "Q", "Folder", false)
            ],
            vec![addition("x", "B", "Part", true)]
        ),
        ["A", "B", "Q"]
    );
    // Nor does a unique id pair instances of two classes.
    assert_eq!(
        merged_additions(
            vec![addition("x", "A", "Part", true)],
            vec![addition("x", "B", "Model", true)]
        ),
        ["A", "B"]
    );
}

/// One service holding a folder `Sub` and, when `script` is set, a script `A`
/// in it; `folder` says whether the side holds the folder's directory.
fn folder_snapshot(script: Option<&str>, folder: bool) -> ProjectSnapshot {
    let service = Path::new("src/ReplicatedStorage");
    let mut document = SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances: vec![
            SettingsBytecodeInstance::new(
                "root".into(),
                "ReplicatedStorage".into(),
                "ReplicatedStorage".into(),
                None,
            ),
            SettingsBytecodeInstance::new("sub".into(), "Sub".into(), "Folder".into(), Some(0)),
        ],
    };
    let mut snapshot = ProjectSnapshot::default();
    snapshot
        .entries
        .insert(service.to_path_buf(), SnapshotEntry::Directory);
    if folder {
        snapshot
            .entries
            .insert(service.join("Sub"), SnapshotEntry::Directory);
    }
    if let Some(source) = script {
        document.instances.push(SettingsBytecodeInstance::new(
            "a".into(),
            "A".into(),
            "ModuleScript".into(),
            Some(1),
        ));
        snapshot
            .entries
            .insert(service.join("Sub").join("A.luau"), text(source));
    }
    snapshot.entries.insert(
        service.join("__roblox_sync_settings.renium"),
        SnapshotEntry::File(encode_settings_bytecode(&document).unwrap()),
    );
    snapshot
}

#[test]
fn a_folder_a_side_still_holds_stays_when_its_last_script_is_removed() {
    let folder = Path::new("src/ReplicatedStorage").join("Sub");
    let script = folder.join("A.luau");
    let with_script = folder_snapshot(Some("return 1"), true);
    // Studio removed the script and reports the folder all the same, the way
    // it reports a service's folder that holds no script.
    let kept = folder_snapshot(None, true);
    let (merged, conflicts, changes) = merge_snapshots_with_changes(
        Some(&with_script),
        &with_script,
        &kept,
        ConflictPreference::None,
        None,
    )
    .unwrap();
    assert!(conflicts.is_empty(), "{conflicts:?}");
    assert!(!merged.entries.contains_key(&script));
    assert!(merged.entries.get(&folder) == Some(&SnapshotEntry::Directory));
    assert!(changes.editor.contains(&script));
    assert!(!changes.editor.contains(&folder), "{:?}", changes.editor);
    assert!(!changes.studio.contains(&folder), "{:?}", changes.studio);

    // A side that no longer holds the folder takes it out of the files too.
    let gone = folder_snapshot(None, false);
    let (merged, conflicts, changes) = merge_snapshots_with_changes(
        Some(&with_script),
        &with_script,
        &gone,
        ConflictPreference::None,
        None,
    )
    .unwrap();
    assert!(conflicts.is_empty(), "{conflicts:?}");
    assert!(!merged.entries.contains_key(&script));
    assert!(!merged.entries.contains_key(&folder));
    assert!(changes.editor.contains(&folder), "{:?}", changes.editor);
    assert!(!changes.studio.contains(&folder), "{:?}", changes.studio);
}
