//! The record each conflict producer makes, and what a decision by its id
//! does.

use super::*;

fn text(text: &str) -> SnapshotEntry {
    SnapshotEntry::File(text.as_bytes().to_vec())
}

fn instance(
    id: &str,
    name: &str,
    class_name: &str,
    parent: Option<usize>,
    properties: Value,
) -> SettingsBytecodeInstance {
    let mut instance =
        SettingsBytecodeInstance::new(id.into(), name.into(), class_name.into(), parent);
    instance.properties = properties.as_object().unwrap().clone();
    instance
}

fn document(instances: Vec<SettingsBytecodeInstance>) -> SettingsBytecode {
    SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances,
    }
}

fn decided(
    preference: ConflictPreference,
    decisions: &[(&str, ConflictSide)],
) -> ConflictDecisions {
    ConflictDecisions {
        preference,
        by_id: decisions
            .iter()
            .map(|(id, side)| ((*id).to_string(), *side))
            .collect(),
    }
}

fn is_conflict_id(id: &str) -> bool {
    id.len() == 8
        && id
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

const STORE: &str = "instances/Workspace.renium";

/// Merges one Workspace store's three documents with the given decisions.
fn store_merge(
    documents: [&SettingsBytecode; 3],
    decisions: ConflictDecisions,
) -> (SettingsBytecode, Result<Vec<Conflict>>) {
    let mut log = ConflictLog::new(decisions);
    let merged = merge_reconciliation_settings_documents(
        Path::new(STORE),
        documents,
        &mut log,
        &HashSet::new(),
        &HashSet::new(),
    );
    (merged, log.finish())
}

fn root() -> SettingsBytecodeInstance {
    instance("ws", "Workspace", "Workspace", None, json!({}))
}

fn named<'a>(document: &'a SettingsBytecode, name: &str) -> Option<&'a SettingsBytecodeInstance> {
    document
        .instances
        .iter()
        .find(|instance| instance.name == name)
}

#[test]
fn a_script_both_sides_edited_is_a_record_with_each_side_s_diff() {
    let path = Path::new("src/ServerScriptService/Main.luau");
    let base = text("local a = 1\nlocal b = 2\nreturn a\n");
    let files = text("local a = 10\nlocal b = 2\nreturn a\n");
    let studio = text("local a = 20\nlocal b = 2\nreturn a\n");
    let mut log = ConflictLog::new(ConflictPreference::None);
    let kept = merge_conflicting_entry(
        path,
        Some(&base),
        Some(&files),
        Some(&studio),
        false,
        &mut log,
    );
    assert!(kept == Some(files.clone()));
    let conflicts = log.finish().unwrap();
    let [conflict] = conflicts.as_slice() else {
        panic!("{conflicts:?}");
    };
    println!(
        "script conflict: {}",
        serde_json::to_string(conflict).unwrap()
    );
    assert!(is_conflict_id(&conflict.id), "{}", conflict.id);
    assert_eq!(conflict.kind, ConflictKind::Script);
    assert_eq!(conflict.path, "src/ServerScriptService/Main.luau");
    assert!(conflict.decidable);
    assert_eq!(
        conflict.files_diff,
        [
            "@@ -1,2 +1,2 @@",
            "-local a = 1",
            "+local a = 10",
            " local b = 2"
        ]
    );
    assert_eq!(
        conflict.studio_diff,
        [
            "@@ -1,2 +1,2 @@",
            "-local a = 1",
            "+local a = 20",
            " local b = 2"
        ]
    );
    assert!(conflict.base.is_none() && conflict.files.is_none() && conflict.absent.is_empty());
    let json = serde_json::to_value(conflict).unwrap();
    assert!(json.get("decidable").is_none() && json.get("instance").is_none());

    for (side, expected) in [
        (ConflictSide::Studio, &studio),
        (ConflictSide::Editor, &files),
    ] {
        let mut log = ConflictLog::new(decided(ConflictPreference::None, &[(&conflict.id, side)]));
        let kept = merge_conflicting_entry(
            path,
            Some(&base),
            Some(&files),
            Some(&studio),
            false,
            &mut log,
        );
        assert!(kept.as_ref() == Some(expected), "{side:?}");
        assert!(log.finish().unwrap().is_empty());
    }
}

#[test]
fn a_script_without_a_last_sync_shows_the_first_lines_of_each_side() {
    let long = (1..=70)
        .map(|line| format!("-- {line}\n"))
        .collect::<String>();
    let mut log = ConflictLog::new(ConflictPreference::None);
    merge_conflicting_entry(
        Path::new("src/A.luau"),
        None,
        Some(&text(&long)),
        Some(&text("return 1\n")),
        true,
        &mut log,
    );
    let conflict = &log.open()[0];
    assert_eq!(conflict.absent, ["base"]);
    assert_eq!(conflict.studio, Some(json!(["return 1"])));
    let files = conflict.files.as_ref().unwrap().as_array().unwrap();
    assert_eq!(files.len(), 61);
    assert_eq!(files[0], "-- 1");
    assert_eq!(files[60], "… 10 more lines");
}

#[test]
fn a_file_one_side_removed_and_the_other_edited_is_a_removal() {
    let mut log = ConflictLog::new(ConflictPreference::None);
    merge_conflicting_entry(
        Path::new("src/Notes.txt"),
        Some(&text("a\n")),
        None,
        Some(&text("b\n")),
        false,
        &mut log,
    );
    let conflict = &log.open()[0];
    assert_eq!(conflict.kind, ConflictKind::Removal);
    assert_eq!(conflict.absent, ["files"]);
    assert_eq!(conflict.studio_diff, ["@@ -1 +1 @@", "-a", "+b"]);
    assert_eq!(
        conflict.detail,
        "src/Notes.txt: Studio changed it and the files removed it"
    );
}

#[test]
fn a_folder_against_a_file_takes_no_decision() {
    let path = Path::new("src/Notes.txt");
    let entries = (
        Some(SnapshotEntry::Directory),
        Some(text("files")),
        Some(text("studio")),
    );
    let mut log = ConflictLog::new(ConflictPreference::None);
    merge_conflicting_entry(
        path,
        entries.0.as_ref(),
        entries.1.as_ref(),
        entries.2.as_ref(),
        false,
        &mut log,
    );
    let conflict = log.open()[0].clone();
    assert!(!conflict.decidable);
    assert_eq!(conflict.base, Some(json!("a folder")));
    let mut log = ConflictLog::new(decided(
        ConflictPreference::None,
        &[(&conflict.id, ConflictSide::Studio)],
    ));
    let kept = merge_conflicting_entry(
        path,
        entries.0.as_ref(),
        entries.1.as_ref(),
        entries.2.as_ref(),
        false,
        &mut log,
    );
    assert!(kept == entries.1, "a refused decision changes nothing");
    let error = log.finish().unwrap_err().to_string();
    assert!(
        error.contains(&conflict.id) && error.contains("cannot be settled by choosing a side"),
        "{error}"
    );
}

#[test]
fn a_script_renamed_on_one_side_and_edited_on_both_is_a_record_at_the_files_path() {
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
    let [conflict] = conflicts.as_slice() else {
        panic!("{conflicts:?}");
    };
    assert_eq!(conflict.kind, ConflictKind::Script);
    assert_eq!(conflict.path, "src/ReplicatedStorage/A.luau");
    assert_eq!(
        conflict.files_diff,
        ["@@ -1 +1 @@", "-return 'base'", "+return 'edit'"]
    );
    assert_eq!(
        conflict.studio_diff,
        ["@@ -1 +1 @@", "-return 'base'", "+return 'studio'"]
    );
    let (merged, open, _) = merge_snapshots_with_changes(
        Some(&baseline),
        &editor,
        &studio,
        decided(
            ConflictPreference::None,
            &[(&conflict.id, ConflictSide::Studio)],
        ),
        None,
    )
    .unwrap();
    assert!(open.is_empty(), "{open:?}");
    assert_eq!(
        script_text(&merged, "B.luau").as_deref(),
        Some("return 'studio'")
    );
}

#[test]
fn a_store_missing_from_the_files_can_only_be_decided_for_studio() {
    let store = reference_merge_store(vec![root()]);
    let baseline = ProjectSnapshot {
        entries: BTreeMap::from([(PathBuf::from(STORE), store.clone())]),
    };
    let editor = ProjectSnapshot::default();
    let studio = baseline.clone();
    let (_, conflicts) =
        merge_snapshots(Some(&baseline), &editor, &studio, ConflictPreference::None).unwrap();
    let [conflict] = conflicts.as_slice() else {
        panic!("{conflicts:?}");
    };
    assert_eq!(conflict.kind, ConflictKind::Store);
    assert_eq!(conflict.only, Some(ConflictSide::Studio));
    assert_eq!(conflict.absent, ["files"]);
    let refused = merge_snapshots_with_changes(
        Some(&baseline),
        &editor,
        &studio,
        decided(
            ConflictPreference::None,
            &[(&conflict.id, ConflictSide::Editor)],
        ),
        None,
    )
    .err()
    .unwrap()
    .to_string();
    assert!(
        refused.contains("can only be decided for studio") && refused.contains(&conflict.id),
        "{refused}"
    );
    let (merged, open, changes) = merge_snapshots_with_changes(
        Some(&baseline),
        &editor,
        &studio,
        decided(
            ConflictPreference::None,
            &[(&conflict.id, ConflictSide::Studio)],
        ),
        None,
    )
    .unwrap();
    assert!(open.is_empty(), "{open:?}");
    assert!(merged.entries[Path::new(STORE)] == store);
    assert!(changes.editor.contains(Path::new(STORE)));
}

#[test]
fn a_property_both_sides_changed_names_the_three_values_by_side() {
    let base = document(vec![
        root(),
        instance("door", "Door", "Part", Some(0), json!({"Transparency": 0})),
    ]);
    let mut files = base.clone();
    files.instances[1]
        .properties
        .insert("Transparency".into(), json!(0.5));
    let mut studio = base.clone();
    studio.instances[1]
        .properties
        .insert("Transparency".into(), json!(0.2));
    let (_, conflicts) = store_merge([&base, &files, &studio], ConflictPreference::None.into());
    let conflicts = conflicts.unwrap();
    let [conflict] = conflicts.as_slice() else {
        panic!("{conflicts:?}");
    };
    println!(
        "property conflict: {}",
        serde_json::to_string(conflict).unwrap()
    );
    assert_eq!(conflict.kind, ConflictKind::Property);
    assert_eq!(conflict.path, STORE);
    assert_eq!(
        conflict.instance,
        Some(ConflictInstance {
            id: "door".into(),
            path: "Workspace/Door".into()
        })
    );
    assert_eq!(conflict.field.as_deref(), Some("Transparency"));
    assert_eq!(
        (&conflict.base, &conflict.files, &conflict.studio),
        (&Some(json!(0)), &Some(json!(0.5)), &Some(json!(0.2)))
    );
    assert_eq!(
        conflict.detail,
        "Workspace/Door: property Transparency: Studio has 0.2, the files have 0.5, the last sync had 0"
    );
    for (side, value) in [(ConflictSide::Studio, 0.2), (ConflictSide::Editor, 0.5)] {
        let (merged, open) = store_merge(
            [&base, &files, &studio],
            decided(ConflictPreference::None, &[(&conflict.id, side)]),
        );
        assert!(open.unwrap().is_empty());
        assert_eq!(
            named(&merged, "Door").unwrap().properties["Transparency"],
            value
        );
    }
}

#[test]
fn a_long_value_is_cut_with_its_length() {
    let long = "x".repeat(3_000);
    let base = document(vec![
        root(),
        instance("note", "Note", "StringValue", Some(0), json!({"Value": ""})),
    ]);
    let mut files = base.clone();
    files.instances[1]
        .properties
        .insert("Value".into(), json!(long));
    let mut studio = base.clone();
    studio.instances[1]
        .properties
        .insert("Value".into(), json!("short"));
    let (_, conflicts) = store_merge([&base, &files, &studio], ConflictPreference::None.into());
    let conflict = &conflicts.unwrap()[0];
    let shown = conflict.files.as_ref().unwrap().as_str().unwrap();
    assert!(shown.ends_with("… (3002 characters, cut)"), "{shown}");
    assert_eq!(
        shown.chars().count(),
        2_000 + "… (3002 characters, cut)".chars().count()
    );
    assert_eq!(conflict.studio, Some(json!("short")));
}

#[test]
fn a_rename_and_a_move_on_both_sides_are_records_of_their_own_kind() {
    let base = document(vec![
        root(),
        instance("a", "A", "Folder", Some(0), json!({})),
        instance("b", "B", "Folder", Some(0), json!({})),
        instance("door", "Door", "Part", Some(0), json!({})),
    ]);
    let mut files = base.clone();
    files.instances[3].name = "FilesDoor".into();
    files.instances[3].parent_index = Some(1);
    let mut studio = base.clone();
    studio.instances[3].name = "StudioDoor".into();
    studio.instances[3].parent_index = Some(2);
    let (_, conflicts) = store_merge([&base, &files, &studio], ConflictPreference::None.into());
    let conflicts = conflicts.unwrap();
    let kinds = conflicts
        .iter()
        .map(|conflict| conflict.kind)
        .collect::<BTreeSet<_>>();
    assert_eq!(
        kinds,
        BTreeSet::from([ConflictKind::Name, ConflictKind::Parent])
    );
    let name = conflicts
        .iter()
        .find(|conflict| conflict.kind == ConflictKind::Name)
        .unwrap();
    assert_eq!(
        (&name.base, &name.files, &name.studio),
        (
            &Some(json!("Door")),
            &Some(json!("FilesDoor")),
            &Some(json!("StudioDoor"))
        )
    );
    let parent = conflicts
        .iter()
        .find(|conflict| conflict.kind == ConflictKind::Parent)
        .unwrap();
    assert_eq!(
        (&parent.base, &parent.files, &parent.studio),
        (
            &Some(json!("Workspace")),
            &Some(json!("Workspace/A")),
            &Some(json!("Workspace/B"))
        )
    );
    // Each takes its own side.
    let (merged, open) = store_merge(
        [&base, &files, &studio],
        decided(
            ConflictPreference::None,
            &[
                (&name.id, ConflictSide::Editor),
                (&parent.id, ConflictSide::Studio),
            ],
        ),
    );
    assert!(open.unwrap().is_empty());
    let door = named(&merged, "FilesDoor").unwrap();
    assert_eq!(
        merged.instances[door.parent_index.unwrap()].name,
        "B",
        "the name came from the files, the parent from Studio"
    );
}

#[test]
fn an_instance_one_side_edited_and_the_other_removed_is_a_removal_with_the_edit() {
    let base = document(vec![
        root(),
        instance("door", "Door", "Part", Some(0), json!({"Transparency": 0})),
    ]);
    let files = document(vec![root()]);
    let mut studio = base.clone();
    studio.instances[1]
        .properties
        .insert("Transparency".into(), json!(0.2));
    let (merged, conflicts) =
        store_merge([&base, &files, &studio], ConflictPreference::None.into());
    let conflicts = conflicts.unwrap();
    let [conflict] = conflicts.as_slice() else {
        panic!("{conflicts:?}");
    };
    assert!(
        named(&merged, "Door").is_some(),
        "an open removal keeps the edit"
    );
    assert_eq!(conflict.kind, ConflictKind::Removal);
    assert_eq!(conflict.absent, ["files"]);
    assert_eq!(conflict.base, Some(json!({"Transparency": 0})));
    assert_eq!(conflict.studio, Some(json!({"Transparency": 0.2})));
    assert_eq!(
        conflict.detail,
        "Workspace/Door: Studio changed it and the files removed it"
    );
    let (merged, open) = store_merge(
        [&base, &files, &studio],
        decided(
            ConflictPreference::None,
            &[(&conflict.id, ConflictSide::Editor)],
        ),
    );
    assert!(open.unwrap().is_empty());
    assert!(named(&merged, "Door").is_none());

    // The other way round names the other sides.
    let (_, conflicts) = store_merge([&base, &studio, &files], ConflictPreference::None.into());
    let conflict = &conflicts.unwrap()[0];
    assert_eq!(conflict.absent, ["studio"]);
    assert_eq!(
        conflict.detail,
        "Workspace/Door: the files changed it and Studio removed it"
    );
}

#[test]
fn an_instance_added_under_a_parent_the_other_side_removed_follows_its_decision() {
    let base = document(vec![
        root(),
        instance("folder", "Folder", "Folder", Some(0), json!({})),
    ]);
    let files = document(vec![root()]);
    let mut studio = base.clone();
    studio
        .instances
        .push(instance("child", "Child", "Part", Some(1), json!({})));
    let (merged, conflicts) =
        store_merge([&base, &files, &studio], ConflictPreference::None.into());
    let conflicts = conflicts.unwrap();
    let [conflict] = conflicts.as_slice() else {
        panic!("{conflicts:?}");
    };
    assert!(named(&merged, "Child").is_none());
    assert_eq!(conflict.kind, ConflictKind::Removal);
    assert_eq!(
        conflict.instance.as_ref().unwrap().path,
        "Workspace/Folder/Child"
    );
    assert_eq!(conflict.studio, Some(json!("Workspace/Folder")));
    assert_eq!(conflict.absent, ["files"]);
    assert_eq!(
        conflict.detail,
        "Workspace/Folder/Child: Studio put it under a parent the files removed; deciding studio keeps it and brings Workspace/Folder back, deciding editor removes it"
    );
    let (merged, open) = store_merge(
        [&base, &files, &studio],
        decided(
            ConflictPreference::None,
            &[(&conflict.id, ConflictSide::Studio)],
        ),
    );
    assert!(open.unwrap().is_empty());
    let child = named(&merged, "Child").unwrap();
    assert_eq!(merged.instances[child.parent_index.unwrap()].name, "Folder");
    let (merged, open) = store_merge(
        [&base, &files, &studio],
        decided(
            ConflictPreference::None,
            &[(&conflict.id, ConflictSide::Editor)],
        ),
    );
    assert!(open.unwrap().is_empty());
    assert!(named(&merged, "Child").is_none() && named(&merged, "Folder").is_none());
}

/// The files edit P and add Y under it; Studio deletes P.
fn removed_parent_with_an_added_child() -> [SettingsBytecode; 3] {
    let base = document(vec![
        root(),
        instance("p", "P", "Folder", Some(0), json!({})),
    ]);
    let mut files = base.clone();
    files.instances[1]
        .attributes
        .insert("Edited".into(), json!(true));
    files
        .instances
        .push(instance("y", "Y", "Folder", Some(1), json!({})));
    let studio = document(vec![root()]);
    [base, files, studio]
}

#[test]
fn deciding_a_removal_leaves_the_orphan_under_it_open() {
    let [base, files, studio] = removed_parent_with_an_added_child();
    let documents = [&base, &files, &studio];
    let open = store_merge(documents, ConflictPreference::None.into())
        .1
        .unwrap();
    assert_eq!(open.len(), 2, "{open:#?}");
    let id_of = |instance: &str| {
        open.iter()
            .find(|conflict| conflict.instance.as_ref().unwrap().id == instance)
            .unwrap()
            .id
            .clone()
    };
    let (removal, orphan) = (id_of("p"), id_of("y"));
    let after = store_merge(
        documents,
        decided(
            ConflictPreference::None,
            &[(&removal, ConflictSide::Studio)],
        ),
    )
    .1
    .unwrap();
    assert!(
        after.iter().any(|conflict| conflict.id == orphan),
        "the orphan has no decision of its own: {after:#?}"
    );
    // Both decided alike: P and Y go.
    let (merged, after) = store_merge(
        documents,
        decided(
            ConflictPreference::None,
            &[
                (&removal, ConflictSide::Studio),
                (&orphan, ConflictSide::Studio),
            ],
        ),
    );
    assert!(after.unwrap().is_empty());
    assert!(named(&merged, "P").is_none() && named(&merged, "Y").is_none());
}

#[test]
fn contradicting_decisions_lose_no_work_and_the_record_says_why() {
    let [base, files, studio] = removed_parent_with_an_added_child();
    let documents = [&base, &files, &studio];
    let open = store_merge(documents, ConflictPreference::None.into())
        .1
        .unwrap();
    let orphan = open
        .iter()
        .find(|conflict| conflict.instance.as_ref().unwrap().id == "y")
        .unwrap();
    assert!(
        orphan.detail.ends_with(
            "deciding studio removes it, deciding editor keeps it and brings Workspace/P back"
        ),
        "{}",
        orphan.detail
    );
    let removal = open
        .iter()
        .find(|conflict| conflict.id != orphan.id)
        .unwrap();
    // P removed as Studio did, Y kept as the files have it: Y keeps its
    // parent, with the files' edit of it.
    let (merged, after) = store_merge(
        documents,
        decided(
            ConflictPreference::None,
            &[
                (&removal.id, ConflictSide::Studio),
                (&orphan.id, ConflictSide::Editor),
            ],
        ),
    );
    assert!(after.unwrap().is_empty());
    let p = named(&merged, "P").expect("Y's decision brings P back");
    assert_eq!(p.attributes["Edited"], json!(true));
    let y = named(&merged, "Y").unwrap();
    assert_eq!(merged.instances[y.parent_index.unwrap()].name, "P");
}

#[test]
fn deciding_a_cycle_leaves_a_reference_conflict_open() {
    let base = document(vec![
        root(),
        instance("a", "A", "ObjectValue", Some(0), json!({})),
        instance("b", "B", "Folder", Some(0), json!({})),
        instance("t", "T", "Part", Some(0), json!({})),
    ]);
    let files = document(vec![
        root(),
        instance("a", "A", "ObjectValue", Some(0), json!({})),
        instance("b", "B", "Folder", Some(1), json!({})),
    ]);
    let studio = document(vec![
        root(),
        instance("b", "B", "Folder", Some(0), json!({})),
        instance(
            "a",
            "A",
            "ObjectValue",
            Some(1),
            json!({"Value": {"_type": "Ref", "settingsId": "t"}}),
        ),
        instance("t", "T", "Part", Some(0), json!({})),
    ]);
    let documents = [&base, &files, &studio];
    let open = store_merge(documents, ConflictPreference::None.into())
        .1
        .unwrap();
    let cycle = open
        .iter()
        .find(|conflict| conflict.detail.contains("cycle"))
        .expect("a cycle conflict");
    let reference = open
        .iter()
        .find(|conflict| conflict.kind == ConflictKind::Reference)
        .expect("a reference conflict");
    let after = store_merge(
        documents,
        decided(
            ConflictPreference::None,
            &[(&cycle.id, ConflictSide::Studio)],
        ),
    )
    .1
    .unwrap();
    assert!(
        after.iter().any(|conflict| conflict.id == reference.id),
        "the reference has no decision of its own: {after:#?}"
    );
    let (merged, after) = store_merge(
        documents,
        decided(
            ConflictPreference::None,
            &[
                (&cycle.id, ConflictSide::Studio),
                (&reference.id, ConflictSide::Studio),
            ],
        ),
    );
    assert!(after.unwrap().is_empty());
    assert!(
        named(&merged, "T").is_some(),
        "Studio's side keeps the target"
    );
}

#[test]
fn moves_on_both_sides_that_close_a_cycle_are_one_parent_record() {
    let base = document(vec![
        root(),
        instance("a", "A", "Folder", Some(0), json!({})),
        instance("b", "B", "Folder", Some(0), json!({})),
    ]);
    let mut files = base.clone();
    files.instances[2].parent_index = Some(1);
    let mut studio = base.clone();
    studio.instances[1].parent_index = Some(2);
    let (_, conflicts) = store_merge([&base, &files, &studio], ConflictPreference::None.into());
    let conflicts = conflicts.unwrap();
    let [conflict] = conflicts.as_slice() else {
        panic!("{conflicts:?}");
    };
    assert_eq!(conflict.kind, ConflictKind::Parent);
    assert_eq!(conflict.instance.as_ref().unwrap().id, "a");
    assert!(
        conflict.detail.contains("cycle among"),
        "{}",
        conflict.detail
    );
    let (merged, open) = store_merge(
        [&base, &files, &studio],
        decided(
            ConflictPreference::None,
            &[(&conflict.id, ConflictSide::Studio)],
        ),
    );
    assert!(open.unwrap().is_empty());
    let a = named(&merged, "A").unwrap();
    let b = named(&merged, "B").unwrap();
    assert_eq!(merged.instances[a.parent_index.unwrap()].name, "B");
    assert_eq!(merged.instances[b.parent_index.unwrap()].name, "Workspace");
}

#[test]
fn a_reference_to_an_instance_the_other_side_removed_is_a_reference_record() {
    let base = document(vec![
        root(),
        instance("target", "Target", "Part", Some(0), json!({})),
        instance("pointer", "Pointer", "ObjectValue", Some(0), json!({})),
    ]);
    let mut files = base.clone();
    files.instances[2].properties.insert(
        "Value".into(),
        json!({"_type": "Ref", "settingsId": "target"}),
    );
    let studio = document(vec![
        root(),
        instance("pointer", "Pointer", "ObjectValue", Some(0), json!({})),
    ]);
    let (_, conflicts) = store_merge([&base, &files, &studio], ConflictPreference::None.into());
    let conflicts = conflicts.unwrap();
    let [conflict] = conflicts.as_slice() else {
        panic!("{conflicts:?}");
    };
    assert_eq!(conflict.kind, ConflictKind::Reference);
    assert_eq!(conflict.field.as_deref(), Some("Value"));
    assert_eq!(conflict.instance.as_ref().unwrap().id, "pointer");
    assert_eq!(conflict.files, Some(json!("Workspace/Target")));
    assert_eq!(conflict.absent, ["studio"]);
    let (merged, open) = store_merge(
        [&base, &files, &studio],
        decided(
            ConflictPreference::None,
            &[(&conflict.id, ConflictSide::Editor)],
        ),
    );
    assert!(open.unwrap().is_empty());
    assert!(
        named(&merged, "Target").is_some(),
        "the files' side keeps the target"
    );
    let (merged, open) = store_merge(
        [&base, &files, &studio],
        decided(
            ConflictPreference::None,
            &[(&conflict.id, ConflictSide::Studio)],
        ),
    );
    assert!(open.unwrap().is_empty());
    assert!(named(&merged, "Target").is_none());
    let reference = &named(&merged, "Pointer").unwrap().properties["Value"];
    assert!(reference.get("settingsId").is_none(), "{reference}");
}

#[test]
fn a_reference_into_a_store_the_other_side_emptied_is_a_reference_record() {
    let reference = json!({
        "_type": "Ref",
        "settingsId": "target",
        "pathSegments": ["Workspace", "Target"],
        "pathOrdinals": [1, 1],
    });
    let snapshot = |with_target: bool, value: Option<&Value>| {
        let mut workspace = vec![root()];
        if with_target {
            workspace.push(instance("target", "Target", "Part", Some(0), json!({})));
        }
        let mut pointer = instance("pointer", "Pointer", "ObjectValue", Some(0), json!({}));
        if let Some(value) = value {
            pointer.properties.insert("Value".into(), value.clone());
        }
        ProjectSnapshot {
            entries: BTreeMap::from([
                (PathBuf::from(STORE), reference_merge_store(workspace)),
                (
                    PathBuf::from("instances/ReplicatedStorage.renium"),
                    reference_merge_store(vec![
                        instance(
                            "rs",
                            "ReplicatedStorage",
                            "ReplicatedStorage",
                            None,
                            json!({}),
                        ),
                        pointer,
                    ]),
                ),
            ]),
        }
    };
    let baseline = snapshot(true, None);
    let editor = snapshot(true, Some(&reference));
    let studio = snapshot(false, None);
    let (_, conflicts) =
        merge_snapshots(Some(&baseline), &editor, &studio, ConflictPreference::None).unwrap();
    let [conflict] = conflicts.as_slice() else {
        panic!("{conflicts:?}");
    };
    assert_eq!(conflict.kind, ConflictKind::Reference);
    assert_eq!(conflict.path, "instances/ReplicatedStorage.renium");
    assert_eq!(
        conflict.instance,
        Some(ConflictInstance {
            id: "pointer".into(),
            path: "ReplicatedStorage/Pointer".into()
        })
    );
    assert_eq!(conflict.field.as_deref(), Some("Value"));
    assert_eq!(conflict.files, Some(json!("Workspace.Target")));
    assert_eq!(conflict.absent, ["studio"]);
    let (merged, open, _) = merge_snapshots_with_changes(
        Some(&baseline),
        &editor,
        &studio,
        decided(
            ConflictPreference::None,
            &[(&conflict.id, ConflictSide::Editor)],
        ),
        None,
    )
    .unwrap();
    assert!(open.is_empty(), "{open:?}");
    let workspace = settings_document(merged.entries.get(Path::new(STORE))).unwrap();
    assert!(named(&workspace, "Target").is_some());
}

#[test]
fn a_first_pairing_difference_is_one_record_per_field() {
    let editor = document(vec![
        root(),
        instance(
            "door",
            "Door",
            "Part",
            Some(0),
            json!({"Transparency": 0.5, "Anchored": true}),
        ),
    ]);
    let mut studio = editor.clone();
    studio.instances[1].properties = json!({"Transparency": 0.2, "Anchored": false})
        .as_object()
        .unwrap()
        .clone();
    let align = |decisions: ConflictDecisions| {
        let (mut files, mut live) = (editor.clone(), studio.clone());
        let mut log = ConflictLog::new(decisions);
        align_first_pairing(Path::new(STORE), &mut files, &mut live, &mut log);
        (files, live, log.finish())
    };
    let (_, _, conflicts) = align(ConflictPreference::None.into());
    let conflicts = conflicts.unwrap();
    assert_eq!(
        conflicts
            .iter()
            .map(|conflict| (conflict.kind, conflict.field.as_deref().unwrap()))
            .collect::<Vec<_>>(),
        [
            (ConflictKind::Property, "Anchored"),
            (ConflictKind::Property, "Transparency")
        ]
    );
    for conflict in &conflicts {
        assert_eq!(conflict.absent, ["base"]);
        assert_eq!(conflict.instance.as_ref().unwrap().path, "Workspace/Door");
    }
    let id = |field: &str| {
        conflicts
            .iter()
            .find(|conflict| conflict.field.as_deref() == Some(field))
            .unwrap()
            .id
            .clone()
    };
    let (files, live, open) = align(decided(
        ConflictPreference::None,
        &[
            (&id("Anchored"), ConflictSide::Studio),
            (&id("Transparency"), ConflictSide::Editor),
        ],
    ));
    assert!(open.unwrap().is_empty());
    for record in [&files.instances[1], &live.instances[1]] {
        assert_eq!(record.properties["Anchored"], false);
        assert_eq!(record.properties["Transparency"], 0.5);
    }
}

#[test]
fn a_first_pairing_class_clash_takes_no_decision() {
    let editor = document(vec![
        root(),
        instance("thing", "Thing", "Part", Some(0), json!({})),
    ]);
    let mut studio = editor.clone();
    studio.instances[1].class_name = "Folder".into();
    let mut log = ConflictLog::new(ConflictPreference::Studio);
    align_first_pairing(Path::new(STORE), &mut editor.clone(), &mut studio, &mut log);
    let open = log.open();
    let clash = open
        .iter()
        .find(|conflict| conflict.detail.contains("different classes"))
        .unwrap();
    assert_eq!(clash.kind, ConflictKind::Pairing);
    assert!(!clash.decidable);
    assert_eq!(
        (&clash.files, &clash.studio),
        (&Some(json!("Part")), &Some(json!("Folder")))
    );
}

#[test]
fn a_package_link_record_refuses_a_decision() {
    let link = instance("link", "PackageLink", "PackageLink", Some(1), json!({}));
    let base = document(vec![
        root(),
        instance("car", "Car", "Model", Some(0), json!({})),
        link,
    ]);
    let unlinked = document(vec![
        root(),
        instance("car", "Car", "Model", Some(0), json!({})),
    ]);
    let id = {
        let mut log = ConflictLog::new(ConflictPreference::None);
        pairing::protect_package_links(
            Path::new(STORE),
            Some(&base),
            &mut base.clone(),
            &mut unlinked.clone(),
            &mut log,
        );
        let open = log.open();
        assert_eq!(
            open[0].instance.as_ref().unwrap().path,
            "Workspace/Car/PackageLink"
        );
        open[0].id.clone()
    };
    let mut log = ConflictLog::new(decided(
        ConflictPreference::None,
        &[(&id, ConflictSide::Editor)],
    ));
    pairing::protect_package_links(
        Path::new(STORE),
        Some(&base),
        &mut base.clone(),
        &mut unlinked.clone(),
        &mut log,
    );
    let error = log.finish().unwrap_err().to_string();
    assert!(
        error.contains(&id) && error.contains("removes PackageLink"),
        "{error}"
    );
}

/// A pair record written while conflicts were sentences, field for field.
#[derive(Serialize)]
struct SentenceRecord {
    version: u8,
    identity: PairIdentity,
    mode: PairMode,
    conflict_preference: ConflictPreference,
    runtime_settings: Map<String, Value>,
    baseline: Option<store::StoredSnapshot>,
    head: Option<RecoveryHead>,
    conflicts: Vec<String>,
    resolution_required: bool,
    last_runtime_id: Option<String>,
    local_file_stamp: Option<LocalFileStamp>,
    local_file_digest: Option<String>,
    studio_checkpoint: Option<StudioCheckpoint>,
}

#[test]
fn a_record_with_sentence_conflicts_still_loads_and_keeps_writes_held() {
    let identity = PairIdentity {
        experience: "experience".into(),
        project: "project".into(),
        fingerprint: String::new(),
        game_id: None,
        place_id: None,
        local_file: None,
    };
    let sentences = |conflicts: &[&str]| SentenceRecord {
        version: SENTENCE_CONFLICTS_RECORD_VERSION,
        identity: identity.clone(),
        mode: PairMode::Verify,
        conflict_preference: ConflictPreference::None,
        runtime_settings: Map::new(),
        baseline: None,
        head: None,
        conflicts: conflicts.iter().map(|line| (*line).to_string()).collect(),
        resolution_required: true,
        last_runtime_id: Some("runtime".into()),
        local_file_stamp: None,
        local_file_digest: Some("digest".into()),
        studio_checkpoint: None,
    };
    let bytes = rmp_serde::to_vec(&sentences(&[
        "src/A.luau changed on both sides",
        "instances/Workspace.renium: Workspace/Door: property Transparency: ours=0.2 theirs=0.5 (base=0)",
    ]))
    .unwrap();
    let mut record: PairRecord = rmp_serde::from_slice(&bytes).unwrap();
    record.upgrade();
    assert_eq!(record.version, RECORD_VERSION);
    assert_eq!(record.local_file_digest.as_deref(), Some("digest"));
    assert!(record.resolution_required);
    assert!(record.differences.is_empty());
    assert_eq!(record.conflicts.len(), 2);
    for conflict in &record.conflicts {
        assert!(!conflict.decidable && is_conflict_id(&conflict.id));
    }
    assert!(record.has_open_review(), "the sentences still hold writes");
    assert!(
        record.conflicts[0].covered_by_paths(&[PathBuf::from("src/A.luau")]),
        "a scoped push still settles a sentence"
    );

    let bytes = rmp_serde::to_vec(&sentences(&[
        "Studio and project files differ",
        "push: src/A.luau",
        "pull: instances/Workspace.renium",
        "3 more paths",
    ]))
    .unwrap();
    let mut record: PairRecord = rmp_serde::from_slice(&bytes).unwrap();
    record.upgrade();
    assert!(record.conflicts.is_empty());
    assert_eq!(
        record.differences,
        [
            "Studio and project files differ",
            "push: src/A.luau",
            "pull: instances/Workspace.renium",
            "3 more paths"
        ]
    );

    // A record of this version keeps its records whole.
    let mut log = ConflictLog::new(ConflictPreference::None);
    merge_conflicting_entry(
        Path::new("src/A.luau"),
        Some(&text("a\n")),
        Some(&text("b\n")),
        Some(&text("c\n")),
        false,
        &mut log,
    );
    record.conflicts = log.finish().unwrap();
    let decoded: PairRecord = rmp_serde::from_slice(&rmp_serde::to_vec(&record).unwrap()).unwrap();
    assert_eq!(
        serde_json::to_value(&decoded.conflicts).unwrap(),
        serde_json::to_value(&record.conflicts).unwrap()
    );
}
