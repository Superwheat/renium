use super::*;
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{BufReader, BufWriter, Cursor};
use std::path::{Path, PathBuf};
use std::thread;

use crate::bridge_server::{
    BridgeInfoPayload, BridgeServer, BridgeTarget, MAX_BRIDGE_REASSEMBLY_BYTES, RuntimePinKey,
    parse_bridge_raw_chunk,
};
use crate::bytecode_api::{
    bytecode_get_property, bytecode_set_property, bytecode_set_source, ensure_service_store_exists,
    lock_existing_service_store, resolve_bytecode_settings_file,
};
use crate::bytecode_edit::{
    bytecode_clone_instance, bytecode_desync_package_link, bytecode_remove_instance,
};
use crate::bytecode_explorer::editor_target_settings_ids;
use crate::command_args::VcInitArgs;
use crate::command_line::{
    ApplyEditorDeleteArgs, BridgeConnectionArgs, BytecodeCloneInstanceArgs,
    BytecodeDesyncPackageLinkArgs, BytecodeExportModelArgs, BytecodeExportPlaceArgs,
    BytecodeFileArgs, BytecodeGetPropertyArgs, BytecodeInstanceSelectorArgs,
    BytecodeRemoveInstanceArgs, BytecodeSetPropertyArgs, BytecodeSetSourceArgs, EditorMutationArgs,
    EditorReviewDecisionArgs, PlaceDesyncPackageLinkArgs, ProjectSourceArgs, PushEditorChangesArgs,
};
use crate::editor_diff::{
    append_editor_instance_reconcile, append_editor_property_changes,
    append_editor_target_inline_source_changes, append_editor_target_instance_upserts,
    push_editor_instance_change,
};
use crate::editor_document::{
    document_instance_index_by_settings_id, ensure_editor_source_target_in_bytecode,
};
use crate::editor_paths::{
    build_editor_source_paths_by_index, infer_editor_source_path_spec, run_context_name,
    script_file_names_for_run_context,
};
use crate::editor_review::{
    editor_review_payload, is_externally_managed_editor_property,
    is_externally_managed_protected_write, is_user_facing_protected_write,
    normalize_editor_bridge_value, patch_place_protected_writes, protected_write_matches_previous,
    protected_write_rows_with_previous_values,
};
use crate::editor_sync::{
    EditorSettingsTransaction, collect_direct_editor_delete_change, collect_editor_changes,
};
use crate::editor_types::{
    EditorChangeSet, EditorInstanceChange, EditorInstanceDescriptor, EditorInstancePath,
    EditorPropertyFilter,
};
use crate::file_io::{
    SERVICE_SETTINGS_FILE_NAME, ensure_existing_ancestor_inside, normalized_child_stem_key,
    sanitize_name, service_settings_path, set_path_readonly, strip_extended_prefix,
    unique_child_stem, validate_filesystem_instance_name, write_bytes_if_changed,
    write_json_streaming,
};
use crate::local_transport::{BoundedLineRead, normalize_loopback_host, read_bounded_line};
use crate::native_editor::{
    encode_service_root_property_values, merge_live_service_root_property_values,
    rbx_dom_service_root_property_values,
};
use crate::package_links::RENIUM_DIR_GITIGNORE;
use crate::place_packages::place_desync_package_link;
use crate::property_schema::{
    EnumValueNameMap, MATERIAL_SERVICE_CLASS, MESH_INITIAL_SIZE_PROPERTY,
    MESH_SIZE_TRANSPORT_PROPERTY, PropertySchemaEntry, TRIANGLE_MESH_PART_CLASS, TYPE_ID_AXES,
    TYPE_ID_CONTENT_ID, TYPE_ID_ENUM_ITEM, TYPE_ID_FACES, TYPE_ID_NUMBER, TYPE_ID_NUMBER_RANGE,
    TYPE_ID_PHYSICAL_PROPERTIES, TYPE_ID_RAY, TYPE_ID_VECTOR3, USE_2022_MATERIALS_PROPERTY,
    collect_rbx_dom_properties_for_class,
};
use crate::rbx_decode::json_number_f64;
use crate::rbx_encode::{
    collect_rbx_subtree_preorder, json_f64, json_to_rbx_axes, json_to_rbx_color_sequence,
    json_to_rbx_faces, json_to_rbx_ray, model_property_name_is_skipped, rbx_model_top_level_refs,
    synthesized_mesh_initial_size_for_rbx_export_class,
};
use crate::rbx_model::{
    BytecodeModelExportRefs, bytecode_export_model, bytecode_export_place,
    rbx_dom_instance_by_path_unique,
};
use crate::settings_bytecode::{
    SETTINGS_BINARY_VERSION, SettingsBytecode, SettingsBytecodeInstance, is_default_property_value,
    write_service_settings_binary_file, write_var_u64,
};
use crate::snapshot_codec::{
    decode_compact_v5_value, parse_compact_v5_instance_items, parse_source_range_batch,
};
use crate::snapshot_export::{parse_bridge_chunk, parse_place_guard_config};
use crate::snapshot_import::{
    quarantine_stale_import_paths, state_with_preserved_material_service_settings,
};
use crate::snapshot_types::{ServiceState, SnapshotInstance};
use crate::sourcemap::path_to_sourcemap_relative;
use crate::studio_automation::{studio_device_resolution, validate_luau_syntax};
use crate::test_support::{settings_document, settings_instance, temp_dir};
use crate::version_control::{
    merge_settings_documents, settings_doc_to_json_tree, settings_doc_to_text, vc_init,
};
use clap::Parser;
use rbx_dom_weak::types::{
    Color3 as RbxColor3, Content as RbxContent, ContentId as RbxContentId, Variant as RbxVariant,
    Vector3 as RbxVector3,
};
use rbx_dom_weak::{InstanceBuilder as RbxInstanceBuilder, WeakDom as RbxWeakDom};
use rbx_reflection::ReflectionDatabase;
use serde::Serialize;
use serde_json::{Map, Value};

fn apply_editor_settings_writes(changes: &EditorChangeSet) -> Result<()> {
    let transaction = EditorSettingsTransaction::apply(changes)?;
    transaction.commit();
    Ok(())
}

fn collect_and_apply_editor_changes(
    project_root: &Path,
    changed_paths: Vec<PathBuf>,
    bridge: BridgeConnectionArgs,
) -> EditorChangeSet {
    let changes = collect_editor_changes(&PushEditorChangesArgs {
        changed_paths,
        ..PushEditorChangesArgs::new(
            ProjectSourceArgs {
                project_root: project_root.to_path_buf(),
                src_root: PathBuf::from("src"),
            },
            bridge,
        )
    })
    .unwrap();
    apply_editor_settings_writes(&changes).unwrap();
    changes
}

fn get_source_property_args(settings_file: &Path) -> BytecodeGetPropertyArgs {
    BytecodeGetPropertyArgs::try_parse_from([
        "bytecode-get-property",
        "-f",
        settings_file.to_string_lossy().as_ref(),
        "-n",
        "Mod",
        "-p",
        "Source",
    ])
    .unwrap()
}

fn synthesized_mesh_initial_size_for_rbx_export(
    document: &SettingsBytecode,
    index: usize,
    database: &ReflectionDatabase<'_>,
) -> Option<RbxVector3> {
    let instance = document.instances.get(index)?;
    synthesized_mesh_initial_size_for_rbx_export_class(
        document,
        index,
        rbx_class_is_a(database, &instance.class_name, TRIANGLE_MESH_PART_CLASS),
    )
}

fn single_mesh_document(class_name: &str, properties: Map<String, Value>) -> SettingsBytecode {
    let mut mesh = settings_instance("mesh", class_name, class_name, Some(0));
    mesh.properties = properties;
    settings_document(vec![
        settings_instance("root", "Workspace", "Workspace", None),
        mesh,
    ])
}

fn rbx_class_is_a(
    database: &ReflectionDatabase<'_>,
    class_name: &str,
    superclass_name: &str,
) -> bool {
    let Some(class_descriptor) = database.classes.get(class_name) else {
        return class_name == superclass_name;
    };
    database
        .superclasses_iter(class_descriptor)
        .any(|class| class.name == superclass_name)
}

#[test]
fn bridge_info_reads_runtime_identity() {
    let info: BridgeInfoPayload = serde_json::from_value(json!({
        "runtimeId": "runtime-a",
        "bridgeRole": "edit",
    }))
    .unwrap();
    assert_eq!(info.runtime_id, "runtime-a");
}

#[test]
fn place_guard_rejects_typos_and_empty_allowlists() {
    let path = Path::new("renium.config.json");
    assert!(
        parse_place_guard_config(r#"{"allowedPlaceId":[123]}"#, path)
            .err()
            .expect("unknown fields should be rejected")
            .to_string()
            .contains("Invalid place guard JSON")
    );
    assert!(
        parse_place_guard_config(r#"{"allowedPlaceIds":[],"allowedGameIds":[]}"#, path)
            .err()
            .expect("empty allowlists should be rejected")
            .to_string()
            .contains("must contain at least one")
    );
    let parsed =
        parse_place_guard_config(r#"{"allowedPlaceIds":[123],"allowedGameIds":[]}"#, path).unwrap();
    assert_eq!(parsed.allowed_place_ids, vec![123]);
}

#[test]
fn script_file_names_follow_script_run_context() {
    assert_eq!(
        script_file_names_for_run_context("Script", Some("Client")),
        Some(("init.client.luau", ".client.luau"))
    );
    assert_eq!(
        script_file_names_for_run_context("Script", Some("Plugin")),
        Some(("init.plugin.luau", ".plugin.luau"))
    );
    assert_eq!(
        script_file_names_for_run_context("Script", Some("Server")),
        Some(("init.server.luau", ".server.luau"))
    );
    assert_eq!(
        script_file_names_for_run_context("LocalScript", None),
        Some(("init.client.luau", ".client.luau"))
    );
}

#[test]
fn runtime_pin_keys_normalize_player_selectors() {
    assert_eq!(
        BridgeServer::runtime_pin_key(BridgeTarget::Client, Some("  PlayerOne ")),
        RuntimePinKey {
            target: BridgeTarget::Client,
            player: Some("playerone".to_string()),
        }
    );
}

#[test]
fn stale_import_paths_are_quarantined_not_deleted() {
    let root = temp_dir("import-quarantine");
    let service_dir = root.join("src").join("Workspace");
    let stale_dir = service_dir.join("Old");
    let stale_file = service_dir.join("loose.luau");
    fs::create_dir_all(&stale_dir).unwrap();
    fs::write(stale_dir.join("child.luau"), "return 1").unwrap();
    fs::write(&stale_file, "return 2").unwrap();

    let backup = quarantine_stale_import_paths(
        &service_dir,
        &[stale_dir.join("child.luau"), stale_file.clone()],
        std::slice::from_ref(&stale_dir),
    )
    .unwrap()
    .unwrap();

    assert!(!stale_dir.exists());
    assert!(!stale_file.exists());
    assert_eq!(
        fs::read_to_string(backup.join("Old").join("child.luau")).unwrap(),
        "return 1"
    );
    assert_eq!(
        fs::read_to_string(backup.join("loose.luau")).unwrap(),
        "return 2"
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn studio_bridge_modules_parse_as_luau() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../plugin_ws_bridge");
    for name in [
        "BridgeContent.module.lua",
        "BridgeConnection.module.lua",
        "BridgeEditorSync.module.lua",
        "BridgePluginRuntime.module.lua",
        "BridgeStudioChanges.module.lua",
    ] {
        let path = root.join(name);
        let source = fs::read_to_string(&path).unwrap();
        let result = thread::Builder::new()
            .name(format!("parse-{name}"))
            .stack_size(64 * 1024 * 1024)
            .spawn(move || validate_luau_syntax(&source))
            .unwrap()
            .join()
            .unwrap();
        result.unwrap_or_else(|error| panic!("{}: {error:#}", path.display()));
    }
}

#[test]
fn binary_roundtrip_keeps_parent_sensitive_instance_classes() {
    let mut dom = RbxWeakDom::new(RbxInstanceBuilder::new("DataModel"));
    let workspace = dom.insert(dom.root_ref(), RbxInstanceBuilder::new("Workspace"));
    let part = dom.insert(
        workspace,
        RbxInstanceBuilder::new("Part").with_name("RigPart"),
    );
    dom.insert(
        part,
        RbxInstanceBuilder::new("Attachment").with_name("Socket"),
    );
    dom.insert(part, RbxInstanceBuilder::new("Bone").with_name("Joint"));
    let model = dom.insert(
        workspace,
        RbxInstanceBuilder::new("Model").with_name("Character"),
    );
    let humanoid = dom.insert(model, RbxInstanceBuilder::new("Humanoid"));
    dom.insert(humanoid, RbxInstanceBuilder::new("Animator"));
    let mesh = dom.insert(
        workspace,
        RbxInstanceBuilder::new("MeshPart").with_name("Clothing"),
    );
    dom.insert(mesh, RbxInstanceBuilder::new("WrapLayer"));
    dom.insert(mesh, RbxInstanceBuilder::new("WrapTarget"));

    let mut bytes = Vec::new();
    rbx_binary::to_writer(&mut bytes, &dom, &[workspace]).unwrap();
    let decoded = rbx_binary::from_reader(bytes.as_slice()).unwrap();
    let classes = decoded
        .descendants()
        .map(|instance| instance.class.to_string())
        .collect::<HashSet<_>>();
    for expected in ["Attachment", "Bone", "Animator", "WrapLayer", "WrapTarget"] {
        assert!(classes.contains(expected), "missing {expected}");
    }
}

#[test]
fn luau_syntax_validation_accepts_agent_snippets() {
    validate_luau_syntax(
            "local total: number = 0\nfor _, value in { 1, 2, 3 } do\n\ttotal += value\nend\nreturn if total > 0 then `total={total}` else \"empty\"",
        )
        .unwrap();
}

#[test]
fn luau_syntax_validation_rejects_invalid_code() {
    let error = validate_luau_syntax("local =").unwrap_err().to_string();
    assert!(error.starts_with("Invalid Luau syntax at "));
}

#[test]
fn bridge_daemon_defaults_to_persistent_mode() {
    let cli = Cli::try_parse_from(["renium", "bridge-daemon"]).unwrap();
    let Commands::BridgeDaemon(args) = cli.command else {
        panic!("bridge-daemon parsed as another command");
    };
    assert!(!args.editor_stdio);

    let cli = Cli::try_parse_from(["renium", "bridge-daemon", "--editor-stdio"]).unwrap();
    let Commands::BridgeDaemon(args) = cli.command else {
        panic!("bridge-daemon parsed as another command");
    };
    assert!(args.editor_stdio);
}

#[test]
fn editor_review_decision_defaults_to_apply() {
    let args = EditorReviewDecisionArgs::try_parse_from(["editor-review-decision"]).unwrap();
    assert_eq!(args.decision, "apply");
    assert!(args.review_id.is_none());
    assert!(
        EditorReviewDecisionArgs::try_parse_from(["editor-review-decision", "invalid"]).is_err()
    );
}

#[test]
fn unique_child_stem_avoids_case_insensitive_and_suffix_collisions() {
    let mut used = HashSet::new();
    let mut suffixes = HashMap::new();
    let names = ["Foo", "foo", "foo_2", "Foo"];
    let stems = names
        .iter()
        .map(|name| unique_child_stem(name, &mut used, &mut suffixes))
        .collect::<Vec<_>>();

    assert_eq!(stems, vec!["Foo", "foo_2", "foo_2_2", "Foo_3"]);
    let normalized = stems
        .iter()
        .map(|stem| normalized_child_stem_key(stem))
        .collect::<HashSet<_>>();
    assert_eq!(normalized.len(), stems.len());
}

#[test]
fn sanitize_name_blocks_windows_device_names_with_extensions() {
    assert_eq!(sanitize_name("CON"), "_CON");
    assert_eq!(sanitize_name("CON.txt"), "_CON.txt");
    assert_eq!(sanitize_name("Lpt1.config"), "_Lpt1.config");
    assert_eq!(sanitize_name("Normal.txt"), "Normal.txt");
}

#[test]
fn sanitize_name_caps_component_length() {
    let long = "L".repeat(300);
    assert_eq!(sanitize_name(&long).chars().count(), 100);
    let dot_at_cut = format!("{}.{}", "D".repeat(99), "tail".repeat(60));
    assert_eq!(sanitize_name(&dot_at_cut), "D".repeat(99));
}

#[test]
fn set_property_rejects_class_name() {
    let args = BytecodeSetPropertyArgs::try_parse_from([
        "bytecode-set-property",
        "Workspace",
        "-n",
        "Anything",
        "-p",
        "ClassName",
        "--str",
        "Folder",
    ])
    .unwrap();
    let err = bytecode_set_property(args).unwrap_err();
    assert!(err.to_string().contains("read-only"), "{err}");
}

#[test]
fn get_property_source_falls_back_to_externalized_mirror() {
    let dir = temp_dir("get-source-mirror");
    let service_dir = dir.join("src").join("ReplicatedStorage");
    fs::create_dir_all(&service_dir).unwrap();
    let settings_file = service_settings_path(&service_dir);
    settings_document(vec![
        settings_instance("root", "ReplicatedStorage", "ReplicatedStorage", None),
        settings_instance("editor:1", "Mod", "ModuleScript", Some(0)),
    ])
    .write_file(&settings_file)
    .unwrap();
    fs::write(service_dir.join("Mod.luau"), "return 123\n").unwrap();
    assert!(bytecode_get_property(get_source_property_args(&settings_file)).is_ok());

    fs::remove_file(service_dir.join("Mod.luau")).unwrap();
    let err = bytecode_get_property(get_source_property_args(&settings_file)).unwrap_err();
    assert!(err.to_string().contains("Property not found"), "{err}");
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn set_source_reads_from_source_file() {
    let dir = temp_dir("set-source-file");
    let service_dir = dir.join("src").join("ReplicatedStorage");
    fs::create_dir_all(&service_dir).unwrap();
    let settings_file = service_settings_path(&service_dir);
    settings_document(vec![
        settings_instance("root", "ReplicatedStorage", "ReplicatedStorage", None),
        settings_instance("editor:1", "Mod", "ModuleScript", Some(0)),
    ])
    .write_file(&settings_file)
    .unwrap();
    let src_file = dir.join("payload.luau");
    fs::write(&src_file, "return 999\n").unwrap();
    let settings_arg = settings_file.to_string_lossy().into_owned();
    let src_arg = src_file.to_string_lossy().into_owned();

    let args = BytecodeSetSourceArgs::try_parse_from([
        "bytecode-set-source",
        "-f",
        &settings_arg,
        "-n",
        "Mod",
        "--source-file",
        &src_arg,
    ])
    .unwrap();
    bytecode_set_source(args).unwrap();
    assert_eq!(
        fs::read_to_string(service_dir.join("Mod.luau")).unwrap(),
        "return 999\n"
    );

    let args = BytecodeSetSourceArgs::try_parse_from([
        "bytecode-set-source",
        "-f",
        &settings_arg,
        "-n",
        "Mod",
        "--source-file",
        &src_arg,
        "--str",
        "x",
    ])
    .unwrap();
    assert!(
        bytecode_set_source(args)
            .unwrap_err()
            .to_string()
            .contains("not both")
    );
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn parse_bridge_raw_chunk_preserves_payload() {
    let payload = r#"{"items":[1,"two",{"three":3}]}"#;
    let frame = format!("RBS2 42 1 33 32 12.5 3.25 1\n{payload}");
    let (id, chunk) = parse_bridge_raw_chunk(frame).unwrap().unwrap();

    assert_eq!(id, 42);
    assert_eq!(chunk.next_start, 33);
    assert_eq!(chunk.total, 32);
    assert_eq!(chunk.chunk, payload);
    assert_eq!(chunk.plugin_server_ms, Some(12.5));
    assert_eq!(chunk.plugin_encode_ms, Some(3.25));
    assert!(chunk.serialization_complete);
}

#[test]
fn bridge_host_is_strictly_loopback() {
    assert_eq!(normalize_loopback_host("127.0.0.1").unwrap(), "127.0.0.1");
    assert_eq!(normalize_loopback_host(" localhost ").unwrap(), "127.0.0.1");
    assert_eq!(normalize_loopback_host("[::1]").unwrap(), "::1");
    assert!(normalize_loopback_host("0.0.0.0").is_err());
    assert!(normalize_loopback_host("192.168.1.25").is_err());
    assert!(normalize_loopback_host("example.test").is_err());
}

#[test]
fn bridge_chunk_rejects_invalid_cursor_and_oversize_payload() {
    assert!(
        parse_bridge_chunk(json!({
            "start": 1,
            "nextStart": 1,
            "total": 4,
            "chunk": "x",
        }))
        .is_err()
    );
    assert!(
        parse_bridge_chunk(json!({
            "start": 1,
            "nextStart": 2,
            "total": MAX_BRIDGE_REASSEMBLY_BYTES + 1,
            "chunk": "x",
        }))
        .is_err()
    );
}

#[test]
fn duplicate_bridge_role_keys_match_original_targets() {
    assert_eq!(BridgeServer::bridge_role_key_base("edit#duplicate"), "edit");
    assert!(BridgeServer::role_matches_target(
        "edit#duplicate",
        BridgeTarget::Edit
    ));
    assert!(BridgeServer::role_matches_target(
        "edit#duplicate",
        BridgeTarget::Main
    ));
    assert!(BridgeServer::role_matches_target(
        "play-client#duplicate",
        BridgeTarget::Client
    ));
    assert!(!BridgeServer::role_matches_target(
        "play-client#duplicate",
        BridgeTarget::Main
    ));
    assert!(!BridgeServer::role_matches_target(
        "play-server#duplicate",
        BridgeTarget::Edit
    ));
}

#[test]
fn studio_device_resolution_accepts_standard_dimensions() {
    assert_eq!(studio_device_resolution("1179x2556").unwrap(), (1179, 2556));
    assert_eq!(studio_device_resolution("874X402").unwrap(), (874, 402));
    assert!(studio_device_resolution("1179").is_err());
    assert!(studio_device_resolution("0x2556").is_err());
}

#[test]
fn parse_source_range_batch_reads_key_source_pairs() {
    let parsed = parse_source_range_batch(json!({
        "items": ["alpha", "print('a')", "beta", "print('b')"]
    }))
    .unwrap();

    assert_eq!(
        parsed.by_key.get("alpha").map(String::as_str),
        Some("print('a')")
    );
    assert_eq!(
        parsed.by_key.get("beta").map(String::as_str),
        Some("print('b')")
    );
}

#[test]
fn parse_source_range_batch_reads_numeric_index_pairs() {
    let parsed = parse_source_range_batch(json!({
        "items": [1, "print('a')", 2, "print('b')"]
    }))
    .unwrap();

    assert_eq!(
        parsed.by_index.get(&1).map(String::as_str),
        Some("print('a')")
    );
    assert_eq!(
        parsed.by_index.get(&2).map(String::as_str),
        Some("print('b')")
    );
}

#[test]
fn write_var_u64_matches_expected_bytes() {
    let cases = [
        (0_u64, vec![0x00]),
        (1_u64, vec![0x01]),
        (127_u64, vec![0x7f]),
        (128_u64, vec![0x80, 0x01]),
        (300_u64, vec![0xac, 0x02]),
        (16_384_u64, vec![0x80, 0x80, 0x01]),
    ];

    for (value, expected) in cases {
        let mut out = Vec::new();
        write_var_u64(&mut out, value).unwrap();
        assert_eq!(out, expected, "unexpected varint bytes for {value}");
    }
}

fn sample_editor_settings_document() -> SettingsBytecode {
    SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances: vec![
            settings_instance("root", "Workspace", "Workspace", None),
            settings_instance("folder", "Folder", "Folder", Some(0)),
        ],
    }
}

fn assert_rbx_vector3_close(value: RbxVector3, expected_x: f32, expected_y: f32, expected_z: f32) {
    let epsilon = 0.0001_f32;
    assert!(
        (value.x - expected_x).abs() <= epsilon,
        "x: {} != {}",
        value.x,
        expected_x
    );
    assert!(
        (value.y - expected_y).abs() <= epsilon,
        "y: {} != {}",
        value.y,
        expected_y
    );
    assert!(
        (value.z - expected_z).abs() <= epsilon,
        "z: {} != {}",
        value.z,
        expected_z
    );
}

fn assert_rbx_color3_close(value: RbxColor3, expected_r: f32, expected_g: f32, expected_b: f32) {
    let epsilon = 0.0001_f32;
    assert!(
        (value.r - expected_r).abs() <= epsilon,
        "r: {} != {}",
        value.r,
        expected_r
    );
    assert!(
        (value.g - expected_g).abs() <= epsilon,
        "g: {} != {}",
        value.g,
        expected_g
    );
    assert!(
        (value.b - expected_b).abs() <= epsilon,
        "b: {} != {}",
        value.b,
        expected_b
    );
}

fn vector3_json(x: f32, y: f32, z: f32) -> Value {
    json!({ "_type": "Vector3", "x": x, "y": y, "z": z })
}

fn sample_service_state(service: &str, properties: Map<String, Value>) -> ServiceState {
    ServiceState {
        instances: vec![SnapshotInstance {
            path: service.to_string(),
            path_segments: vec![service.to_string()],
            name: service.to_string(),
            class_name: service.into(),
            properties,
            instance_index: Some(1),
            ..Default::default()
        }],
        native_properties_by_instance: None,
        children_by_index: vec![Vec::new()],
        source_in_subtree: vec![false],
        script_count_in_subtree: vec![0],
        subtree_sizes: vec![1],
        service_root_index: 0,
        class_defaults_by_class: HashMap::new(),
        properties_default_elided: false,
        dense_index_topology: true,
    }
}

#[test]
fn json_to_rbx_color_sequence_reads_legacy_color_arrays() {
    let sequence = json_to_rbx_color_sequence(&json!({
        "ColorSequence": {
            "keypoints": [
                { "time": 0, "color": [0.25, 0.5, 1.0] },
                { "time": 1, "value": { "r": 1.0, "g": 0.125, "b": 0.0 } }
            ]
        }
    }))
    .expect("legacy color arrays should decode");

    assert_eq!(sequence.keypoints.len(), 2);
    assert_eq!(sequence.keypoints[0].time, 0.0);
    assert_rbx_color3_close(sequence.keypoints[0].color, 0.25, 0.5, 1.0);
    assert_eq!(sequence.keypoints[1].time, 1.0);
    assert_rbx_color3_close(sequence.keypoints[1].color, 1.0, 0.125, 0.0);
}

#[test]
fn rbxlx_export_skips_lighting_clock_time_but_keeps_time_of_day() {
    assert!(model_property_name_is_skipped("ClockTime"));
    assert!(!model_property_name_is_skipped("TimeOfDay"));
}

#[test]
fn material_service_settings_preserve_use_2022_materials_when_bridge_omits_it() {
    let project_root = temp_dir("preserve-use-2022-materials");
    let service_dir = project_root.join("MaterialService");
    fs::create_dir_all(&service_dir).unwrap();
    let settings_path = service_settings_path(&service_dir);
    settings_document(vec![SettingsBytecodeInstance {
        settings_id: "1".to_string(),
        name: MATERIAL_SERVICE_CLASS.to_string(),
        class_name: MATERIAL_SERVICE_CLASS.to_string(),
        parent_index: None,
        properties: Map::from_iter([(USE_2022_MATERIALS_PROPERTY.to_string(), json!(true))]),
        attributes: Map::new(),
    }])
    .write_file(&settings_path)
    .unwrap();

    let incoming = sample_service_state(MATERIAL_SERVICE_CLASS, Map::new());
    let preserved = state_with_preserved_material_service_settings(
        MATERIAL_SERVICE_CLASS,
        &incoming,
        &settings_path,
    )
    .unwrap()
    .expect("existing protected value should be preserved");

    assert_eq!(
        preserved.instances[0]
            .properties
            .get(USE_2022_MATERIALS_PROPERTY),
        Some(&json!(true))
    );

    let incoming_false = sample_service_state(
        MATERIAL_SERVICE_CLASS,
        Map::from_iter([(USE_2022_MATERIALS_PROPERTY.to_string(), json!(false))]),
    );
    assert!(
        state_with_preserved_material_service_settings(
            MATERIAL_SERVICE_CLASS,
            &incoming_false,
            &settings_path,
        )
        .unwrap()
        .is_none(),
        "an explicit bridge value should not be overwritten"
    );

    let _ = fs::remove_dir_all(project_root);
}

fn rbx_dom_property_json(value_type: &str, tags: &[&str], serialization: &str) -> Value {
    json!({
        "DataType": { "Value": value_type },
        "Tags": tags,
        "Kind": {
            "Canonical": {
                "Serialization": serialization,
            }
        }
    })
}

#[test]
fn rbx_dom_schema_keeps_serializing_read_only_package_id() {
    let classes: Map<String, Value> = serde_json::from_value(json!({
        "Instance": {
            "Properties": {}
        },
        "PackageLink": {
            "Superclass": "Instance",
            "Properties": {
                "PackageAssetName": {
                    "DataType": { "Value": "String" },
                    "Tags": ["NotReplicated", "NotScriptable", "ReadOnly"],
                    "Kind": { "Canonical": { "Serialization": "DoesNotSerialize" } }
                },
                "PackageId": {
                    "DataType": { "Value": "ContentId" },
                    "Tags": ["NotReplicated", "ReadOnly"],
                    "Kind": {
                        "Canonical": {
                            "Serialization": { "SerializesAs": "PackageIdSerialize" }
                        }
                    }
                },
                "PackageIdSerialize": {
                    "DataType": { "Value": "ContentId" },
                    "Tags": ["Hidden", "NotScriptable"],
                    "Kind": { "Alias": { "AliasFor": "PackageId" } }
                }
            }
        }
    }))
    .unwrap();

    let mut memo = HashMap::new();
    let mut visiting = HashSet::new();
    let entries =
        collect_rbx_dom_properties_for_class("PackageLink", &classes, &mut memo, &mut visiting);

    let package_id = entries
        .iter()
        .find(|entry| entry.name == "PackageId")
        .expect(
            "PackageLink.PackageId should be exported because it serializes as PackageIdSerialize",
        );
    assert_eq!(package_id.type_id, TYPE_ID_CONTENT_ID);
    assert!(
        !entries
            .iter()
            .any(|entry| entry.name == "PackageIdSerialize")
    );
    assert!(!entries.iter().any(|entry| entry.name == "PackageAssetName"));
}

#[test]
fn service_settings_preserve_package_link_instances_and_package_id() {
    let project_root = temp_dir("package-link");
    let settings_path = project_root
        .join("Workspace")
        .join(SERVICE_SETTINGS_FILE_NAME);

    let state = ServiceState {
        instances: vec![
            SnapshotInstance {
                path: "Workspace".to_string(),
                path_segments: vec!["Workspace".to_string()],
                name: "Workspace".to_string(),
                class_name: "Workspace".into(),
                instance_index: Some(1),
                ..Default::default()
            },
            SnapshotInstance {
                path: "Workspace.PackagedModel".to_string(),
                path_segments: vec!["Workspace".to_string(), "PackagedModel".to_string()],
                name: "PackagedModel".to_string(),
                class_name: "Model".into(),
                parent_path: Some("Workspace".to_string()),
                instance_index: Some(2),
                parent_index: Some(1),
                ..Default::default()
            },
            SnapshotInstance {
                path: "Workspace.PackagedModel.PackageLink".to_string(),
                path_segments: vec![
                    "Workspace".to_string(),
                    "PackagedModel".to_string(),
                    "PackageLink".to_string(),
                ],
                name: "PackageLink".to_string(),
                class_name: "PackageLink".into(),
                properties: Map::from_iter([(
                    "PackageId".to_string(),
                    json!("rbxassetid://123456789"),
                )]),
                parent_path: Some("Workspace.PackagedModel".to_string()),
                instance_index: Some(3),
                parent_index: Some(2),
                ..Default::default()
            },
        ],
        native_properties_by_instance: None,
        children_by_index: vec![vec![1], vec![2], Vec::new()],
        source_in_subtree: vec![false, false, false],
        script_count_in_subtree: vec![0, 0, 0],
        subtree_sizes: vec![3, 2, 1],
        service_root_index: 0,
        class_defaults_by_class: HashMap::new(),
        properties_default_elided: false,
        dense_index_topology: true,
    };

    write_service_settings_binary_file(&settings_path, &state).unwrap();
    let decoded = SettingsBytecode::read_file(&settings_path).unwrap();

    let package_link = decoded
        .instances
        .iter()
        .find(|instance| instance.class_name == "PackageLink")
        .expect("PackageLink instance should survive settings sync");
    assert_eq!(package_link.name, "PackageLink");
    assert_eq!(package_link.parent_index, Some(1));
    assert_eq!(
        package_link.properties.get("PackageId"),
        Some(&json!("rbxassetid://123456789"))
    );

    let _ = fs::remove_dir_all(project_root);
}

#[test]
fn bytecode_desync_package_link_removes_direct_package_link_child() {
    let project_root = temp_dir("desync-package-link");
    let service_dir = project_root.join("src").join("Workspace");
    fs::create_dir_all(&service_dir).unwrap();
    let settings_path = service_settings_path(&service_dir);
    settings_document(vec![
        settings_instance("root", "Workspace", "Workspace", None),
        settings_instance("garage", "Garage", "Model", Some(0)),
        SettingsBytecodeInstance {
            settings_id: "package-link".to_string(),
            name: "PackageLink".to_string(),
            class_name: "PackageLink".to_string(),
            parent_index: Some(1),
            properties: Map::from_iter([(
                "PackageId".to_string(),
                json!("rbxassetid://123456789"),
            )]),
            attributes: Map::new(),
        },
        settings_instance("door", "Door", "Part", Some(1)),
    ])
    .write_file(&settings_path)
    .unwrap();

    bytecode_desync_package_link(BytecodeDesyncPackageLinkArgs {
        input: BytecodeFileArgs::settings_file(settings_path.clone()),
        service: "Workspace".to_string(),
        selector: BytecodeInstanceSelectorArgs {
            path_segments_json: Some("Workspace.Garage".to_string()),
            ..Default::default()
        },
        pretty: false,
    })
    .unwrap();

    let decoded = SettingsBytecode::read_file(&settings_path).unwrap();
    assert!(
        decoded
            .instances
            .iter()
            .all(|instance| instance.class_name != "PackageLink")
    );
    let door = decoded
        .instances
        .iter()
        .find(|instance| instance.settings_id == "door")
        .expect("ordinary children should remain");
    assert_eq!(door.parent_index, Some(1));

    let _ = fs::remove_dir_all(project_root);
}

#[test]
fn place_desync_package_link_writes_copy_without_package_link() {
    let project_root = temp_dir("place-desync-package-link");
    fs::create_dir_all(&project_root).unwrap();
    let input_path = project_root.join("input.rbxlx");
    let output_path = project_root.join("output.rbxlx");

    let mut dom = RbxWeakDom::new(RbxInstanceBuilder::new("DataModel"));
    let workspace_ref = dom.insert(dom.root_ref(), RbxInstanceBuilder::new("Workspace"));
    let garage_ref = dom.insert(
        workspace_ref,
        RbxInstanceBuilder::new("Model").with_name("Garage"),
    );
    dom.insert(
        garage_ref,
        RbxInstanceBuilder::new("PackageLink")
            .with_property("PackageId", RbxContentId::from("rbxassetid://123456789")),
    );
    dom.insert(
        garage_ref,
        RbxInstanceBuilder::new("Part").with_name("Door"),
    );
    let input = File::create(&input_path).unwrap();
    rbx_xml::to_writer_default(BufWriter::new(input), &dom, &[workspace_ref]).unwrap();

    place_desync_package_link(PlaceDesyncPackageLinkArgs {
        input: input_path,
        output: output_path.clone(),
        path_segments_json: "Workspace.Garage".to_string(),
        path_ordinals_json: "[]".to_string(),
        output_format: None,
        pretty: false,
    })
    .unwrap();

    let output_dom = read_exported_rbx_dom(&output_path, "rbxlx");
    let garage_path = vec!["Workspace".to_string(), "Garage".to_string()];
    let output_garage_ref =
        rbx_dom_instance_by_path_unique(&output_dom, &garage_path, &[]).unwrap();
    let output_garage = output_dom.get_by_ref(output_garage_ref).unwrap();
    assert!(output_garage.children().iter().any(|child_ref| {
        output_dom
            .get_by_ref(*child_ref)
            .is_some_and(|child| child.name == "Door")
    }));
    assert!(output_garage.children().iter().all(|child_ref| {
        output_dom
            .get_by_ref(*child_ref)
            .is_none_or(|child| child.class.as_str() != "PackageLink")
    }));

    let _ = fs::remove_dir_all(project_root);
}

fn read_exported_rbx_dom(output_path: &Path, format: &str) -> RbxWeakDom {
    let file = File::open(output_path).unwrap();
    let reader = BufReader::new(file);
    match format {
        "rbxm" | "rbxl" => rbx_binary::from_reader(reader).unwrap(),
        "rbxmx" | "rbxlx" => rbx_xml::from_reader_default(reader).unwrap(),
        other => panic!("unsupported test format {other}"),
    }
}

fn exported_mesh_instance(dom: &RbxWeakDom) -> &rbx_dom_weak::Instance {
    let mut refs = Vec::new();
    for root_ref in rbx_model_top_level_refs(dom) {
        collect_rbx_subtree_preorder(dom, root_ref, &mut refs);
    }
    refs.iter()
        .filter_map(|referent| dom.get_by_ref(*referent))
        .find(|instance| instance.class.as_str() == "MeshPart")
        .expect("exported MeshPart should exist")
}

fn assert_exported_mesh_initial_size_without_mesh_size(
    dom: &RbxWeakDom,
    expected_x: f32,
    expected_y: f32,
    expected_z: f32,
) {
    let mesh = exported_mesh_instance(dom);
    let initial_size = mesh
        .properties
        .iter()
        .find_map(|(name, value)| (name.as_str() == MESH_INITIAL_SIZE_PROPERTY).then_some(value))
        .expect("exported MeshPart should include InitialSize");
    match initial_size {
        RbxVariant::Vector3(value) => {
            assert_rbx_vector3_close(*value, expected_x, expected_y, expected_z)
        }
        other => panic!("InitialSize should be Vector3, got {:?}", other.ty()),
    }
    assert!(
        mesh.properties
            .iter()
            .all(|(name, _)| name.as_str() != MESH_SIZE_TRANSPORT_PROPERTY),
        "exported MeshPart should not serialize MeshSize"
    );
}

#[test]
fn rbx_dom_schema_includes_mesh_size_for_triangle_mesh_descendants() {
    let classes: Map<String, Value> = serde_json::from_value(json!({
            "BasePart": {
                "Properties": {
                    "Size": rbx_dom_property_json("Vector3", &[], "Serializes")
                }
            },
            "TriangleMeshPart": {
                "Superclass": "BasePart",
                "Properties": {
                    "MeshSize": rbx_dom_property_json("Vector3", &["NotReplicated", "ReadOnly"], "DoesNotSerialize")
                }
            },
            "MeshPart": {
                "Superclass": "TriangleMeshPart",
                "Properties": {}
            },
            "PartOperation": {
                "Superclass": "TriangleMeshPart",
                "Properties": {}
            },
            "UnionOperation": {
                "Superclass": "PartOperation",
                "Properties": {}
            }
        }))
        .unwrap();

    let mut memo = HashMap::new();
    let mut visiting = HashSet::new();
    for class_name in [
        TRIANGLE_MESH_PART_CLASS,
        "MeshPart",
        "PartOperation",
        "UnionOperation",
    ] {
        let entries =
            collect_rbx_dom_properties_for_class(class_name, &classes, &mut memo, &mut visiting);
        let mesh_size = entries
            .iter()
            .find(|entry| entry.name == MESH_SIZE_TRANSPORT_PROPERTY)
            .unwrap_or_else(|| panic!("{class_name} should include MeshSize"));
        assert_eq!(mesh_size.type_id, TYPE_ID_VECTOR3, "{class_name}");
        assert_eq!(mesh_size.enum_type, None, "{class_name}");
    }
}

#[test]
fn synthesized_mesh_initial_size_uses_cumulative_model_scale() {
    let mut outer_model_properties = Map::new();
    outer_model_properties.insert("Scale".to_string(), json!(0.25));
    let mut inner_model_properties = Map::new();
    inner_model_properties.insert("Scale".to_string(), json!(0.5));
    let mut mesh_properties = Map::new();
    mesh_properties.insert(
        "Size".to_string(),
        json!({ "_type": "Vector3", "x": 2.0, "y": 4.0, "z": 6.0 }),
    );

    let document = settings_document(vec![
        settings_instance("root", "Workspace", "Workspace", None),
        SettingsBytecodeInstance {
            settings_id: "outer".to_string(),
            name: "Outer".to_string(),
            class_name: "Model".to_string(),
            parent_index: Some(0),
            properties: outer_model_properties,
            attributes: Map::new(),
        },
        SettingsBytecodeInstance {
            settings_id: "inner".to_string(),
            name: "Inner".to_string(),
            class_name: "Model".to_string(),
            parent_index: Some(1),
            properties: inner_model_properties,
            attributes: Map::new(),
        },
        SettingsBytecodeInstance {
            settings_id: "mesh".to_string(),
            name: "Mesh".to_string(),
            class_name: "MeshPart".to_string(),
            parent_index: Some(2),
            properties: mesh_properties,
            attributes: Map::new(),
        },
    ]);
    let database = rbx_reflection_database::get().unwrap();

    let initial_size =
        synthesized_mesh_initial_size_for_rbx_export(&document, 3, database).unwrap();

    assert_rbx_vector3_close(initial_size, 16.0, 32.0, 48.0);
}

#[test]
fn synthesized_mesh_initial_size_prefers_mesh_size_over_scale_fallback() {
    let mut model_properties = Map::new();
    model_properties.insert("Scale".to_string(), json!(0.25));
    let mut mesh_properties = Map::new();
    mesh_properties.insert("Size".to_string(), vector3_json(2.0, 4.0, 6.0));
    mesh_properties.insert(
        MESH_SIZE_TRANSPORT_PROPERTY.to_string(),
        vector3_json(7.0, 8.0, 9.0),
    );

    let document = settings_document(vec![
        settings_instance("root", "Workspace", "Workspace", None),
        SettingsBytecodeInstance {
            settings_id: "model".to_string(),
            name: "ScaledModel".to_string(),
            class_name: "Model".to_string(),
            parent_index: Some(0),
            properties: model_properties,
            attributes: Map::new(),
        },
        SettingsBytecodeInstance {
            settings_id: "union".to_string(),
            name: "Union".to_string(),
            class_name: "UnionOperation".to_string(),
            parent_index: Some(1),
            properties: mesh_properties,
            attributes: Map::new(),
        },
    ]);
    let database = rbx_reflection_database::get().unwrap();

    let initial_size =
        synthesized_mesh_initial_size_for_rbx_export(&document, 2, database).unwrap();

    assert_rbx_vector3_close(initial_size, 7.0, 8.0, 9.0);
}

#[test]
fn synthesized_mesh_initial_size_repairs_zero_initial_size_from_mesh_size() {
    let mut mesh_properties = Map::new();
    mesh_properties.insert(
        MESH_INITIAL_SIZE_PROPERTY.to_string(),
        vector3_json(0.0, 0.0, 0.0),
    );
    mesh_properties.insert(
        MESH_SIZE_TRANSPORT_PROPERTY.to_string(),
        vector3_json(3.0, 6.0, 9.0),
    );

    let document = single_mesh_document("PartOperation", mesh_properties);
    let database = rbx_reflection_database::get().unwrap();

    let initial_size =
        synthesized_mesh_initial_size_for_rbx_export(&document, 1, database).unwrap();

    assert_rbx_vector3_close(initial_size, 3.0, 6.0, 9.0);
}

#[test]
fn synthesized_mesh_initial_size_preserves_nonzero_initial_size() {
    let mut mesh_properties = Map::new();
    mesh_properties.insert(
        MESH_INITIAL_SIZE_PROPERTY.to_string(),
        vector3_json(1.0, 2.0, 3.0),
    );
    mesh_properties.insert(
        MESH_SIZE_TRANSPORT_PROPERTY.to_string(),
        vector3_json(7.0, 8.0, 9.0),
    );

    let document = single_mesh_document("MeshPart", mesh_properties);
    let database = rbx_reflection_database::get().unwrap();

    assert!(
        synthesized_mesh_initial_size_for_rbx_export(&document, 1, database).is_none(),
        "non-zero InitialSize should block MeshSize repair"
    );
}

#[test]
fn synthesized_mesh_initial_size_uses_size_without_studio_mesh_size() {
    let mut mesh_properties = Map::new();
    mesh_properties.insert("Size".to_string(), vector3_json(4.0, 5.0, 6.0));

    let document = single_mesh_document("MeshPart", mesh_properties);
    let database = rbx_reflection_database::get().unwrap();

    let initial_size =
        synthesized_mesh_initial_size_for_rbx_export(&document, 1, database).unwrap();

    assert_rbx_vector3_close(initial_size, 4.0, 5.0, 6.0);
}

fn write_mesh_export_fixture(
    tag: &str,
    model_scale: Option<f64>,
    mesh_properties: Map<String, Value>,
) -> (PathBuf, PathBuf) {
    let project_root = temp_dir(tag);
    let service_dir = project_root.join("src").join("Workspace");
    fs::create_dir_all(&service_dir).unwrap();
    let settings_path = service_settings_path(&service_dir);
    let mut model_properties = Map::new();
    if let Some(scale) = model_scale {
        model_properties.insert("Scale".to_string(), json!(scale));
    }
    settings_document(vec![
        settings_instance("root", "Workspace", "Workspace", None),
        SettingsBytecodeInstance {
            settings_id: "model".to_string(),
            name: "Model".to_string(),
            class_name: "Model".to_string(),
            parent_index: Some(0),
            properties: model_properties,
            attributes: Map::new(),
        },
        SettingsBytecodeInstance {
            settings_id: "mesh".to_string(),
            name: "Mesh".to_string(),
            class_name: "MeshPart".to_string(),
            parent_index: Some(1),
            properties: mesh_properties,
            attributes: Map::new(),
        },
    ])
    .write_file(&settings_path)
    .unwrap();
    (project_root, settings_path)
}

fn assert_mesh_exports(project_root: &Path, settings_path: &Path, expected: (f32, f32, f32)) {
    for format in ["rbxmx", "rbxm"] {
        let output_path = project_root.join(format!("mesh-model.{format}"));
        bytecode_export_model(BytecodeExportModelArgs {
            input: BytecodeFileArgs::settings_file(settings_path.to_path_buf()),
            service: "Workspace".to_string(),
            selector: BytecodeInstanceSelectorArgs::by_settings_id(Some("model".to_string())),
            output: output_path.clone(),
            format: Some(format.to_string()),
            pretty: false,
        })
        .unwrap();
        let dom = read_exported_rbx_dom(&output_path, format);
        assert_exported_mesh_initial_size_without_mesh_size(
            &dom, expected.0, expected.1, expected.2,
        );
    }
    for format in ["rbxlx", "rbxl"] {
        let output_path = project_root.join(format!("mesh-place.{format}"));
        bytecode_export_place(BytecodeExportPlaceArgs {
            project: ProjectSourceArgs {
                project_root: project_root.to_path_buf(),
                src_root: PathBuf::from("src"),
            },
            services: "Workspace".to_string(),
            output: output_path.clone(),
            format: Some(format.to_string()),
            pretty: false,
        })
        .unwrap();
        let dom = read_exported_rbx_dom(&output_path, format);
        assert_exported_mesh_initial_size_without_mesh_size(
            &dom, expected.0, expected.1, expected.2,
        );
    }
}

#[test]
fn bytecode_export_uses_size_for_file_created_meshes_in_all_formats() {
    let mut properties = Map::new();
    properties.insert("Size".to_string(), vector3_json(4.0, 5.0, 6.0));
    let (project_root, settings_path) =
        write_mesh_export_fixture("file-created-mesh", None, properties);
    assert_mesh_exports(&project_root, &settings_path, (4.0, 5.0, 6.0));
    let _ = fs::remove_dir_all(project_root);
}

#[test]
fn bytecode_export_uses_transport_mesh_size_in_all_formats() {
    let mut properties = Map::new();
    properties.insert("Size".to_string(), vector3_json(2.0, 4.0, 6.0));
    properties.insert(
        MESH_INITIAL_SIZE_PROPERTY.to_string(),
        vector3_json(0.0, 0.0, 0.0),
    );
    properties.insert(
        MESH_SIZE_TRANSPORT_PROPERTY.to_string(),
        vector3_json(7.0, 8.0, 9.0),
    );
    let (project_root, settings_path) =
        write_mesh_export_fixture("transport-mesh-size", Some(0.25), properties);
    assert_mesh_exports(&project_root, &settings_path, (7.0, 8.0, 9.0));
    let _ = fs::remove_dir_all(project_root);
}

#[test]
fn append_editor_property_changes_skips_mesh_size_transport_property() {
    let mut mesh_properties = Map::new();
    mesh_properties.insert("meshSize".to_string(), vector3_json(7.0, 8.0, 9.0));
    mesh_properties.insert("Size".to_string(), vector3_json(1.0, 2.0, 3.0));

    let document = settings_document(vec![
        settings_instance("root", "Workspace", "Workspace", None),
        SettingsBytecodeInstance {
            settings_id: "mesh".to_string(),
            name: "Mesh".to_string(),
            class_name: "MeshPart".to_string(),
            parent_index: Some(0),
            properties: mesh_properties,
            attributes: Map::new(),
        },
    ]);

    let mut changes = EditorChangeSet::default();
    append_editor_property_changes(
        &mut changes,
        &document,
        "Workspace",
        &HashMap::new(),
        &EditorPropertyFilter::default(),
        rbx_reflection_database::get().unwrap(),
    );

    assert_eq!(changes.property_changes.len(), 1);
    let properties = &changes.property_changes[0].properties;
    assert!(properties.contains_key("Size"));
    assert!(
        properties
            .keys()
            .all(|name| !name.eq_ignore_ascii_case(MESH_SIZE_TRANSPORT_PROPERTY)),
        "editor property pushes should skip MeshSize"
    );
}

#[test]
fn default_property_elision_never_skips_mesh_size_transport_property() {
    let mesh_size = vector3_json(7.0, 8.0, 9.0);
    let mut mesh_defaults = Map::new();
    mesh_defaults.insert(MESH_SIZE_TRANSPORT_PROPERTY.to_string(), mesh_size.clone());
    let mut class_defaults_by_class = HashMap::new();
    class_defaults_by_class.insert("MeshPart".to_string(), mesh_defaults);

    let state = ServiceState {
        instances: Vec::new(),
        native_properties_by_instance: None,
        children_by_index: Vec::new(),
        source_in_subtree: Vec::new(),
        script_count_in_subtree: Vec::new(),
        subtree_sizes: Vec::new(),
        service_root_index: 0,
        class_defaults_by_class,
        properties_default_elided: false,
        dense_index_topology: false,
    };

    assert!(
        !is_default_property_value(&state, "MeshPart", "meshSize", &mesh_size),
        "MeshSize transport metadata should never be default-elided"
    );
}

#[test]
fn editor_targets_skip_service_roots() {
    let document = settings_document(vec![
        settings_instance("editor:0", "Workspace", "Workspace", None),
        settings_instance("editor:1", "Folder", "Folder", Some(0)),
        settings_instance("manual", "Manual", "Folder", Some(1)),
        settings_instance("editor:orphan-root", "Lighting", "Lighting", None),
    ]);

    assert_eq!(
        editor_target_settings_ids(&document, "Workspace", "editor:"),
        vec!["editor:1".to_string()]
    );
}

#[test]
fn targeted_instance_upserts_include_ancestors_but_not_root() {
    let document = settings_document(vec![
        settings_instance("editor:0", "Workspace", "Workspace", None),
        settings_instance("editor:1", "Parent", "Folder", Some(0)),
        settings_instance("editor:2", "Child", "Folder", Some(1)),
    ]);
    let filter = EditorPropertyFilter {
        settings_ids: HashSet::from(["editor:2".to_string()]),
        property_names: HashSet::new(),
    };
    let mut changes = EditorChangeSet::default();

    append_editor_target_instance_upserts(&mut changes, &document, "Workspace", &filter);

    assert_eq!(changes.instance_changes.len(), 1);
    assert_eq!(changes.instance_changes[0].mode, "upsertInstances");
    assert_eq!(
        changes.instance_changes[0]
            .instances
            .iter()
            .map(|instance| instance.settings_id.as_str())
            .collect::<Vec<_>>(),
        vec!["editor:1", "editor:2"]
    );
    assert_eq!(
        changes.instance_changes[0].instances[0].path_segments.len(),
        2
    );
    assert_eq!(
        changes.instance_changes[0].instances[1].path_segments.len(),
        3
    );
}

#[test]
fn targeted_inline_source_changes_include_selected_package_scripts() {
    let document = settings_document(vec![
        settings_instance("editor:0", "Workspace", "Workspace", None),
        settings_instance("editor:1", "PackageRoot", "Folder", Some(0)),
        SettingsBytecodeInstance {
            settings_id: "editor:2".to_string(),
            name: "Child".to_string(),
            class_name: "ModuleScript".to_string(),
            parent_index: Some(1),
            properties: Map::from_iter([(
                "Source".to_string(),
                Value::String("return 42".to_string()),
            )]),
            attributes: Map::new(),
        },
    ]);
    let filter = EditorPropertyFilter {
        settings_ids: HashSet::from(["editor:2".to_string()]),
        property_names: HashSet::new(),
    };
    let mut changes = EditorChangeSet::default();

    append_editor_target_inline_source_changes(&mut changes, &document, "Workspace", &filter);

    assert_eq!(changes.source_changes.len(), 1);
    assert_eq!(
        changes.source_changes[0].settings_id.as_deref(),
        Some("editor:2")
    );
    assert_eq!(
        changes.source_changes[0].path_segments,
        vec!["Workspace", "PackageRoot", "Child"]
    );
    assert_eq!(
        changes.source_changes[0].source.as_deref(),
        Some("return 42")
    );
}

#[test]
fn direct_editor_delete_change_does_not_require_existing_bytecode_match() {
    let changes = collect_direct_editor_delete_change(ApplyEditorDeleteArgs {
        target: EditorMutationArgs {
            project: ProjectSourceArgs {
                project_root: PathBuf::from("."),
                src_root: PathBuf::from("src"),
            },
            bridge: BridgeConnectionArgs::local(1.0),
            service: "Workspace".to_string(),
            settings_id: Some("already-removed".to_string()),
            class_name: "Folder".to_string(),
            path_segments_json: serde_json::to_string(&vec!["Workspace", "DeletedFolder"]).unwrap(),
            path_ordinals_json: serde_json::to_string(&vec![1_usize, 1_usize]).unwrap(),
            override_packages: false,
        },
    })
    .unwrap();

    assert_eq!(changes.instance_changes.len(), 1);
    let change = &changes.instance_changes[0];
    assert_eq!(change.mode, "deleteInstances");
    assert!(!change.allow_deletes);
    assert_eq!(change.instances.len(), 1);
    assert_eq!(change.instances[0].settings_id, "already-removed");
    assert_eq!(
        change.instances[0].path_segments,
        vec!["Workspace", "DeletedFolder"]
    );
    assert!(changes.source_changes.is_empty());
    assert!(changes.property_changes.is_empty());
}

#[test]
fn clone_instance_rebinds_internal_ref_properties() {
    let project_root = temp_dir("clone-ref");
    let service_dir = project_root.join("src").join("Workspace");
    fs::create_dir_all(&service_dir).unwrap();
    let settings_path = service_settings_path(&service_dir);

    let mut model_properties = Map::new();
    model_properties.insert(
        "PrimaryPart".to_string(),
        json!({ "_type": "Ref", "instanceIndex": 3 }),
    );
    model_properties.insert(
        "ExternalRef".to_string(),
        json!({ "_type": "Ref", "instanceIndex": 6 }),
    );
    let mut weld_properties = Map::new();
    weld_properties.insert(
        "Part0".to_string(),
        json!({ "_type": "Ref", "instanceIndex": 3 }),
    );
    weld_properties.insert(
        "Part1".to_string(),
        json!({ "_type": "Ref", "instanceIndex": 4 }),
    );

    let document = settings_document(vec![
        settings_instance("root", "Workspace", "Workspace", None),
        SettingsBytecodeInstance {
            settings_id: "model".to_string(),
            name: "Model".to_string(),
            class_name: "Model".to_string(),
            parent_index: Some(0),
            properties: model_properties,
            attributes: Map::new(),
        },
        settings_instance("part-a", "PartA", "Part", Some(1)),
        settings_instance("part-b", "PartB", "Part", Some(1)),
        SettingsBytecodeInstance {
            settings_id: "weld".to_string(),
            name: "Weld".to_string(),
            class_name: "WeldConstraint".to_string(),
            parent_index: Some(1),
            properties: weld_properties,
            attributes: Map::new(),
        },
        settings_instance("outside", "Outside", "Part", Some(0)),
    ]);
    document.write_file(&settings_path).unwrap();

    bytecode_clone_instance(BytecodeCloneInstanceArgs {
        input: BytecodeFileArgs::settings_file(settings_path.clone()),
        service: "Workspace".to_string(),
        selector: BytecodeInstanceSelectorArgs::by_settings_id(Some("model".to_string())),
        parent_index: Some(0),
        parent_settings_id: None,
        parent_name: None,
        parent_class_name: None,
        pretty: false,
    })
    .unwrap();

    let cloned = SettingsBytecode::read_file(&settings_path).unwrap();
    let cloned_model_index = cloned
        .instances
        .iter()
        .position(|instance| instance.name == "Model Copy")
        .unwrap();
    let cloned_part_a_index = cloned
        .instances
        .iter()
        .position(|instance| {
            instance.parent_index == Some(cloned_model_index) && instance.name == "PartA"
        })
        .unwrap();
    let cloned_part_b_index = cloned
        .instances
        .iter()
        .position(|instance| {
            instance.parent_index == Some(cloned_model_index) && instance.name == "PartB"
        })
        .unwrap();
    let cloned_weld_index = cloned
        .instances
        .iter()
        .position(|instance| {
            instance.parent_index == Some(cloned_model_index) && instance.name == "Weld"
        })
        .unwrap();

    assert_ref_index(
        cloned.instances[cloned_model_index]
            .properties
            .get("PrimaryPart"),
        cloned_part_a_index + 1,
    );
    assert_ref_index(
        cloned.instances[cloned_model_index]
            .properties
            .get("ExternalRef"),
        6,
    );
    assert_ref_index(
        cloned.instances[cloned_weld_index].properties.get("Part0"),
        cloned_part_a_index + 1,
    );
    assert_ref_index(
        cloned.instances[cloned_weld_index].properties.get("Part1"),
        cloned_part_b_index + 1,
    );

    let _ = fs::remove_dir_all(project_root);
}

fn assert_ref_index(value: Option<&Value>, expected_one_based_index: usize) {
    let object = value
        .and_then(Value::as_object)
        .expect("expected Ref object");
    assert_eq!(object.get("_type").and_then(Value::as_str), Some("Ref"));
    assert_eq!(
        object.get("instanceIndex").and_then(Value::as_u64),
        Some(expected_one_based_index as u64)
    );
}

#[test]
fn reconcile_instance_changes_skip_service_root() {
    let document = settings_document(vec![
        settings_instance("editor:0", "Workspace", "Workspace", None),
        settings_instance("editor:1", "Folder", "Folder", Some(0)),
    ]);
    let mut changes = EditorChangeSet::default();

    append_editor_instance_reconcile(&mut changes, &document, "Workspace");

    assert_eq!(changes.instance_changes.len(), 1);
    assert_eq!(changes.instance_changes[0].instances.len(), 1);
    assert_eq!(
        changes.instance_changes[0].instances[0].settings_id,
        "editor:1"
    );
}

#[test]
fn reconcile_instance_changes_describe_ambiguous_siblings() {
    let document = settings_document(vec![
        settings_instance("root", "Workspace", "Workspace", None),
        SettingsBytecodeInstance {
            settings_id: "first".to_string(),
            name: "Value".to_string(),
            class_name: "StringValue".to_string(),
            parent_index: Some(0),
            properties: Map::from_iter([
                ("Value".to_string(), json!("first")),
                (
                    "Target".to_string(),
                    json!({ "_type": "Ref", "instanceIndex": 3 }),
                ),
            ]),
            attributes: Map::from_iter([("Marker".to_string(), json!("one"))]),
        },
        SettingsBytecodeInstance {
            settings_id: "second".to_string(),
            name: "Value".to_string(),
            class_name: "StringValue".to_string(),
            parent_index: Some(0),
            properties: Map::from_iter([("Value".to_string(), json!("second"))]),
            attributes: Map::from_iter([("Marker".to_string(), json!("two"))]),
        },
        SettingsBytecodeInstance {
            settings_id: "unique".to_string(),
            name: "Unique".to_string(),
            class_name: "StringValue".to_string(),
            parent_index: Some(0),
            properties: Map::from_iter([("Value".to_string(), json!("unique"))]),
            attributes: Map::new(),
        },
    ]);
    let mut changes = EditorChangeSet::default();

    append_editor_instance_reconcile(&mut changes, &document, "Workspace");

    let instances = &changes.instance_changes[0].instances;
    let first = instances
        .iter()
        .find(|instance| instance.settings_id == "first")
        .unwrap();
    assert!(first.ambiguous_siblings);
    assert_eq!(first.match_properties.get("Value"), Some(&json!("first")));
    assert!(!first.match_properties.contains_key("Target"));
    assert_eq!(first.match_attributes.get("Marker"), Some(&json!("one")));
    let unique = instances
        .iter()
        .find(|instance| instance.settings_id == "unique")
        .unwrap();
    assert!(!unique.ambiguous_siblings);
    assert!(unique.match_properties.is_empty());
    let payload = serde_json::to_value(first).unwrap();
    assert_eq!(payload.get("ambiguousSiblings"), Some(&json!(true)));
    assert_eq!(
        payload.pointer("/matchProperties/Value"),
        Some(&json!("first"))
    );
}

#[test]
fn editor_review_payload_keeps_every_instance_row() {
    let instances = (0..5001)
        .map(|index| EditorInstanceDescriptor {
            settings_id: format!("editor:{index}"),
            path_segments: vec!["Workspace".to_string(), format!("Part{index}")],
            path_ordinals: Vec::new(),
            class_name: "Part".to_string(),
            ..EditorInstanceDescriptor::default()
        })
        .collect();
    let changes = EditorChangeSet {
        instance_changes: vec![EditorInstanceChange {
            mode: "reconcileService".to_string(),
            service: "Workspace".to_string(),
            allow_deletes: true,
            instances,
            preserve_instances: Vec::new(),
        }],
        ..EditorChangeSet::default()
    };

    let (change_count, rows) = editor_review_payload(&changes);

    assert_eq!(change_count, 5001);
    assert_eq!(rows.len(), 5001);
    assert!(rows.iter().all(|row| {
        row.get("entries")
            .and_then(Value::as_array)
            .and_then(|entries| entries.first())
            .and_then(|entry| entry.get("kind"))
            == Some(&json!("instanceReconcile"))
    }));
}

#[test]
fn editor_ref_transport_preserves_duplicate_path_ordinals() {
    let paths = vec![
        Some(EditorInstancePath {
            path_segments: vec!["Workspace".to_string()],
            path_ordinals: vec![1],
        }),
        Some(EditorInstancePath {
            path_segments: vec!["Workspace".to_string(), "Ads".to_string()],
            path_ordinals: vec![1, 1],
        }),
        Some(EditorInstancePath {
            path_segments: vec!["Workspace".to_string(), "Ads".to_string(), "Ad".to_string()],
            path_ordinals: vec![1, 1, 1],
        }),
        Some(EditorInstancePath {
            path_segments: vec!["Workspace".to_string(), "Ads".to_string(), "Ad".to_string()],
            path_ordinals: vec![1, 1, 2],
        }),
    ];
    let normalized = normalize_editor_bridge_value(
        &json!({
            "_type": "Ref",
            "instanceIndex": 4,
            "pathSegments": ["Workspace", "Wrong"],
        }),
        None,
        &paths,
        &["root", "ads", "first-ad", "second-ad"],
    );

    assert_eq!(
        normalized.get("pathSegments"),
        Some(&json!(["Workspace", "Ads", "Ad"]))
    );
    assert_eq!(normalized.get("pathOrdinals"), Some(&json!([1, 1, 2])));
    assert_eq!(normalized.get("settingsId"), Some(&json!("second-ad")));
}

#[test]
fn editor_source_path_spec_infers_init_script_instance() {
    let spec = infer_editor_source_path_spec(
        Path::new("project/src"),
        "ServerScriptService",
        Path::new("project/src/ServerScriptService/Parent/Child/init.server.luau"),
    )
    .unwrap();

    assert_eq!(spec.class_name, "Script");
    assert_eq!(spec.instance_name, "Child");
    assert_eq!(spec.parent_components, vec!["Parent"]);
    assert_eq!(
        spec.path_segments,
        vec!["ServerScriptService", "Parent", "Child"]
    );
}

#[test]
fn editor_source_path_spec_infers_plugin_run_context() {
    let spec = infer_editor_source_path_spec(
        Path::new("project/src"),
        "ServerStorage",
        Path::new("project/src/ServerStorage/Tools/init.plugin.luau"),
    )
    .unwrap();

    assert_eq!(spec.class_name, "Script");
    assert_eq!(spec.run_context.as_deref(), Some("Plugin"));
    assert_eq!(spec.instance_name, "Tools");

    let mut document = settings_document(vec![settings_instance(
        "root",
        "ServerStorage",
        "ServerStorage",
        None,
    )]);
    let ensured = ensure_editor_source_target_in_bytecode(&mut document, &spec).unwrap();
    let target_index = document_instance_index_by_settings_id(
        &document,
        ensured.target.settings_id.as_deref().unwrap(),
    )
    .unwrap();
    assert_eq!(
        document.instances[target_index]
            .properties
            .get("RunContext"),
        Some(&json!({
            "_type": "EnumItem",
            "enumType": "Enum.RunContext",
            "name": "Plugin",
        }))
    );
    assert_eq!(
        run_context_name(
            document.instances[target_index]
                .properties
                .get("RunContext")
                .unwrap()
        ),
        Some("Plugin")
    );
}

#[test]
fn editor_source_target_creates_missing_bytecode_script() {
    let mut document = sample_editor_settings_document();
    let spec = infer_editor_source_path_spec(
        Path::new("project/src"),
        "Workspace",
        Path::new("project/src/Workspace/Folder/NewModule.luau"),
    )
    .unwrap();

    let ensured = ensure_editor_source_target_in_bytecode(&mut document, &spec).unwrap();

    assert!(ensured.changed);
    let target = ensured.target;
    assert_eq!(target.settings_id.as_deref(), Some("editor:2"));
    assert_eq!(
        target.path_segments,
        vec!["Workspace", "Folder", "NewModule"]
    );
    assert_eq!(ensured.upsert_instances.len(), 2);
    assert_eq!(document.instances[2].name, "NewModule");
    assert_eq!(document.instances[2].class_name, "ModuleScript");
    assert_eq!(document.instances[2].parent_index, Some(1));

    let ensured_again = ensure_editor_source_target_in_bytecode(&mut document, &spec).unwrap();

    assert!(!ensured_again.changed);
    assert_eq!(ensured_again.target.settings_id, target.settings_id);
    assert_eq!(document.instances.len(), 3);
}

#[test]
fn collect_editor_changes_imports_orphan_source_script() {
    let root = temp_dir("orphan-source-import");
    let source_path = root
        .join("src")
        .join("ServerScriptService")
        .join("Orphan.server.luau");
    fs::create_dir_all(source_path.parent().unwrap()).unwrap();
    fs::write(&source_path, "print('orphan')").unwrap();

    let changes = collect_and_apply_editor_changes(
        &root,
        vec![source_path],
        BridgeConnectionArgs {
            ports: "8781".to_string(),
            ..BridgeConnectionArgs::local(0.0)
        },
    );

    assert!(changes.instance_changes.iter().any(|change| {
        change.mode == "upsertInstances"
            && change.service == "ServerScriptService"
            && change
                .instances
                .iter()
                .any(|instance| instance.path_segments == ["ServerScriptService", "Orphan"])
    }));
    assert!(changes.source_changes.iter().any(|change| {
        change.service == "ServerScriptService"
            && change.path_segments == ["ServerScriptService", "Orphan"]
            && change.source.as_deref() == Some("print('orphan')")
    }));
    let settings = SettingsBytecode::read_file(&service_settings_path(
        &root.join("src").join("ServerScriptService"),
    ))
    .unwrap();
    assert!(
        settings
            .instances
            .iter()
            .any(|instance| { instance.name == "Orphan" && instance.class_name == "Script" })
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn editor_source_target_creates_missing_parent_folders() {
    let mut document = sample_editor_settings_document();
    let spec = infer_editor_source_path_spec(
        Path::new("project/src"),
        "Workspace",
        Path::new("project/src/Workspace/NewFolder/NewServer.server.luau"),
    )
    .unwrap();

    let ensured = ensure_editor_source_target_in_bytecode(&mut document, &spec).unwrap();

    assert!(ensured.changed);
    let target = ensured.target;
    assert_eq!(target.class_name, "Script");
    assert_eq!(
        target.path_segments,
        vec!["Workspace", "NewFolder", "NewServer"]
    );
    assert_eq!(ensured.upsert_instances.len(), 2);
    assert_eq!(document.instances[2].name, "NewFolder");
    assert_eq!(document.instances[2].class_name, "Folder");
    assert_eq!(document.instances[2].parent_index, Some(0));
    assert_eq!(document.instances[3].name, "NewServer");
    assert_eq!(document.instances[3].class_name, "Script");
    assert_eq!(document.instances[3].parent_index, Some(2));
}

#[test]
fn starter_player_script_containers_keep_real_classes() {
    let mut document = settings_document(vec![settings_instance(
        "root",
        "StarterPlayer",
        "StarterPlayer",
        None,
    )]);
    let spec = infer_editor_source_path_spec(
        Path::new("project/src"),
        "StarterPlayer",
        Path::new("project/src/StarterPlayer/StarterCharacterScripts/ArmsInVehicles.client.luau"),
    )
    .unwrap();

    let ensured = ensure_editor_source_target_in_bytecode(&mut document, &spec).unwrap();

    assert!(ensured.changed);
    assert_eq!(document.instances[1].name, "StarterCharacterScripts");
    assert_eq!(document.instances[1].class_name, "StarterCharacterScripts");
    assert_eq!(document.instances[2].name, "ArmsInVehicles");
    assert_eq!(document.instances[2].class_name, "LocalScript");
    assert_eq!(
        ensured.target.path_segments,
        vec!["StarterPlayer", "StarterCharacterScripts", "ArmsInVehicles"]
    );
}

#[test]
fn deleted_source_file_demotes_script_to_folder_without_deleting_subtree() {
    let project_root = temp_dir("deleted-source");
    let service_dir = project_root.join("src").join("ReplicatedFirst");
    fs::create_dir_all(service_dir.join("LoadingScreen")).unwrap();

    let document = settings_document(vec![
        settings_instance("root", "ReplicatedFirst", "ReplicatedFirst", None),
        settings_instance("script", "LoadingScreen", "LocalScript", Some(0)),
        settings_instance("frame", "Frame", "Frame", Some(1)),
        SettingsBytecodeInstance {
            settings_id: "reference".to_string(),
            name: "Reference".to_string(),
            class_name: "ObjectValue".to_string(),
            parent_index: Some(0),
            properties: Map::from_iter([(
                "Value".to_string(),
                json!({ "_type": "Ref", "instanceIndex": 2 }),
            )]),
            attributes: Map::new(),
        },
    ]);
    let settings_path = service_settings_path(&service_dir);
    document.write_file(&settings_path).unwrap();
    let changes = collect_and_apply_editor_changes(
        &project_root,
        vec![PathBuf::from(
            "src/ReplicatedFirst/LoadingScreen/init.client.luau",
        )],
        BridgeConnectionArgs::local(1.0),
    );

    assert_eq!(changes.instance_changes.len(), 1);
    assert_eq!(changes.instance_changes[0].mode, "replaceInstances");
    assert_eq!(
        changes.instance_changes[0].instances[0].settings_id,
        "script"
    );
    assert_eq!(
        changes.instance_changes[0].instances[0].class_name,
        "Folder"
    );
    assert!(changes.source_changes.is_empty());
    assert_eq!(changes.history_entries.len(), 1);
    assert_eq!(
        changes.history_entries[0].settings_id.as_deref(),
        Some("script")
    );
    let reference_change = changes
        .property_changes
        .iter()
        .find(|change| change.settings_id.as_deref() == Some("reference"))
        .unwrap();
    assert_eq!(
        reference_change
            .properties
            .get("Value")
            .and_then(Value::as_object)
            .and_then(|value| value.get("settingsId")),
        Some(&json!("script"))
    );
    let after = SettingsBytecode::read_file(&settings_path).unwrap();
    assert_eq!(after.instances.len(), 4);
    assert_eq!(after.instances[1].name, "LoadingScreen");
    assert_eq!(after.instances[1].class_name, "Folder");
    assert_eq!(after.instances[2].name, "Frame");
    assert_eq!(after.instances[2].parent_index, Some(1));

    let _ = fs::remove_dir_all(project_root);
}

#[test]
fn restored_init_source_reclasses_folder_back_to_script() {
    let project_root = temp_dir("restored-source");
    let service_dir = project_root.join("src").join("ReplicatedFirst");
    fs::create_dir_all(service_dir.join("LoadingScreen")).unwrap();
    fs::write(
        service_dir.join("LoadingScreen").join("init.client.luau"),
        "print('restored')",
    )
    .unwrap();

    let document = settings_document(vec![
        settings_instance("root", "ReplicatedFirst", "ReplicatedFirst", None),
        settings_instance("script", "LoadingScreen", "Folder", Some(0)),
        settings_instance("frame", "Frame", "Frame", Some(1)),
    ]);
    let settings_path = service_settings_path(&service_dir);
    document.write_file(&settings_path).unwrap();

    let changes = collect_and_apply_editor_changes(
        &project_root,
        vec![PathBuf::from(
            "src/ReplicatedFirst/LoadingScreen/init.client.luau",
        )],
        BridgeConnectionArgs::local(1.0),
    );

    assert_eq!(changes.instance_changes.len(), 1);
    assert_eq!(changes.instance_changes[0].mode, "replaceInstances");
    assert_eq!(
        changes.instance_changes[0].instances[0].class_name,
        "LocalScript"
    );
    assert_eq!(changes.source_changes.len(), 1);
    let after = SettingsBytecode::read_file(&settings_path).unwrap();
    assert_eq!(after.instances[1].class_name, "LocalScript");
    assert_eq!(after.instances[2].parent_index, Some(1));

    let _ = fs::remove_dir_all(project_root);
}

#[test]
fn parse_compact_v5_instance_items_accept_rows_without_properties() {
    let property_schema_by_class = HashMap::new();
    let class_names = vec!["Part".to_string()];
    let strings = vec![
        "PartA".to_string(),
        "PartB".to_string(),
        "Speed".to_string(),
    ];

    let parsed = parse_compact_v5_instance_items(
        json!([[1, 0, false], [2, 0, false, [3, TYPE_ID_NUMBER, 42]]]),
        &strings,
        1,
        &property_schema_by_class,
        &HashMap::new(),
        &class_names,
    )
    .unwrap();

    assert_eq!(parsed.len(), 2);
    assert_eq!(parsed[0].name, "PartA");
    assert!(parsed[0].properties.is_empty());
    assert!(parsed[0].attributes.is_empty());
    assert_eq!(parsed[1].name, "PartB");
    assert_eq!(parsed[1].attributes.get("Speed"), Some(&json!(42)));
}

#[test]
fn compact_v5_axes_faces_ray_round_trip() {
    let strings: Vec<String> = Vec::new();
    let enum_names = EnumValueNameMap::new();

    let axes =
        decode_compact_v5_value(TYPE_ID_AXES, None, json!(5), &strings, &enum_names).unwrap();
    assert_eq!(axes, json!({"_type": "Axes", "axes": ["X", "Z"]}));
    assert_eq!(json_to_rbx_axes(&axes).unwrap().bits(), 5);

    let faces =
        decode_compact_v5_value(TYPE_ID_FACES, None, json!(63), &strings, &enum_names).unwrap();
    assert_eq!(
        faces,
        json!({"_type": "Faces", "faces": ["Right", "Top", "Back", "Left", "Bottom", "Front"]})
    );
    assert_eq!(json_to_rbx_faces(&faces).unwrap().bits(), 63);

    let ray = decode_compact_v5_value(
        TYPE_ID_RAY,
        None,
        json!([1.0, 2.0, 3.0, 4.0, 5.0, 6.0]),
        &strings,
        &enum_names,
    )
    .unwrap();
    assert_eq!(
        ray,
        json!({
            "_type": "Ray",
            "origin": {"x": 1.0, "y": 2.0, "z": 3.0},
            "direction": {"x": 4.0, "y": 5.0, "z": 6.0},
        })
    );
    let rbx_ray = json_to_rbx_ray(&ray).unwrap();
    assert_eq!(rbx_ray.origin, RbxVector3::new(1.0, 2.0, 3.0));
    assert_eq!(rbx_ray.direction, RbxVector3::new(4.0, 5.0, 6.0));
}

#[test]
fn parse_compact_v5_instance_items_expand_schema_driven_values() {
    let mut property_schema_by_class = HashMap::new();
    property_schema_by_class.insert(
        "Part".to_string(),
        vec![
            PropertySchemaEntry {
                name: "Position".to_string(),
                type_id: TYPE_ID_VECTOR3,
                enum_type: None,
            },
            PropertySchemaEntry {
                name: "Material".to_string(),
                type_id: TYPE_ID_ENUM_ITEM,
                enum_type: Some("Enum.Material".to_string()),
            },
        ],
    );
    let class_names = vec!["Part".to_string()];
    let strings = vec![
        "PartA".to_string(),
        "Speed".to_string(),
        "Plastic".to_string(),
        "gamepadEnterKeyCode".to_string(),
        "Enum.KeyCode".to_string(),
        "ButtonL2".to_string(),
    ];

    let enum_value_names_by_type = HashMap::from([(
        "Enum.Material".to_string(),
        HashMap::from([(256, "Plastic".to_string())]),
    )]);
    let parsed = parse_compact_v5_instance_items(
        json!([[
            1,
            0,
            false,
            [2, TYPE_ID_NUMBER, 42, 4, TYPE_ID_ENUM_ITEM, [5, 6]],
            [3],
            [[1.0, 2.0, 3.0], 256]
        ]]),
        &strings,
        1,
        &property_schema_by_class,
        &enum_value_names_by_type,
        &class_names,
    )
    .unwrap();

    assert_eq!(parsed.len(), 1);
    assert_eq!(parsed[0].name, "PartA");
    assert_eq!(parsed[0].attributes.get("Speed"), Some(&json!(42)));
    assert_eq!(
        parsed[0].attributes.get("gamepadEnterKeyCode"),
        Some(&json!({
            "_type": "EnumItem",
            "enumType": "Enum.KeyCode",
            "name": "ButtonL2",
        }))
    );
    assert_eq!(
        parsed[0].properties.get("Position"),
        Some(&json!({
            "_type": "Vector3",
            "x": 1.0,
            "y": 2.0,
            "z": 3.0,
        }))
    );
    assert_eq!(
        parsed[0].properties.get("Material"),
        Some(&json!({
            "_type": "EnumItem",
            "enumType": "Enum.Material",
            "name": "Plastic",
        }))
    );
}

#[test]
fn parse_compact_v5_instance_items_expand_numeric_enum_values() {
    let mut property_schema_by_class = HashMap::new();
    property_schema_by_class.insert(
        "Part".to_string(),
        vec![PropertySchemaEntry {
            name: "Material".to_string(),
            type_id: TYPE_ID_ENUM_ITEM,
            enum_type: Some("Enum.Material".to_string()),
        }],
    );
    let mut enum_value_names_by_type = EnumValueNameMap::new();
    enum_value_names_by_type.insert(
        "Enum.Material".to_string(),
        HashMap::from([(256, "Plastic".to_string())]),
    );
    let class_names = vec!["Part".to_string()];
    let strings = vec!["PartA".to_string()];

    let parsed = parse_compact_v5_instance_items(
        json!([[1, 0, false, 1, [256]]]),
        &strings,
        1,
        &property_schema_by_class,
        &enum_value_names_by_type,
        &class_names,
    )
    .unwrap();

    assert_eq!(
        parsed[0].properties.get("Material"),
        Some(&json!({
            "_type": "EnumItem",
            "enumType": "Enum.Material",
            "name": "Plastic",
        }))
    );
}

#[test]
fn parse_compact_v5_instance_items_accept_single_mask_word_number() {
    let mut property_schema_by_class = HashMap::new();
    property_schema_by_class.insert(
        "Part".to_string(),
        vec![PropertySchemaEntry {
            name: "Position".to_string(),
            type_id: TYPE_ID_VECTOR3,
            enum_type: None,
        }],
    );
    let class_names = vec!["Part".to_string()];
    let strings = vec!["PartA".to_string()];

    let parsed = parse_compact_v5_instance_items(
        json!([[1, 0, false, 1, [[1.0, 2.0, 3.0]]]]),
        &strings,
        1,
        &property_schema_by_class,
        &HashMap::new(),
        &class_names,
    )
    .unwrap();

    assert_eq!(parsed.len(), 1);
    assert_eq!(
        parsed[0].properties.get("Position"),
        Some(&json!({
            "_type": "Vector3",
            "x": 1.0,
            "y": 2.0,
            "z": 3.0,
        }))
    );
}

#[test]
fn parse_compact_v5_instance_items_expand_number_range_and_physical_properties() {
    let mut property_schema_by_class = HashMap::new();
    property_schema_by_class.insert(
        "Part".to_string(),
        vec![
            PropertySchemaEntry {
                name: "Lifetime".to_string(),
                type_id: TYPE_ID_NUMBER_RANGE,
                enum_type: None,
            },
            PropertySchemaEntry {
                name: "DefaultPhysicalProperties".to_string(),
                type_id: TYPE_ID_PHYSICAL_PROPERTIES,
                enum_type: None,
            },
            PropertySchemaEntry {
                name: "CustomPhysicalProperties".to_string(),
                type_id: TYPE_ID_PHYSICAL_PROPERTIES,
                enum_type: None,
            },
        ],
    );
    let class_names = vec!["Part".to_string()];
    let strings = vec!["PartA".to_string()];

    let parsed = parse_compact_v5_instance_items(
        json!([[
            1,
            0,
            false,
            7,
            [[0.5, 1.5], false, [1.0, 0.3, 0.5, 1.0, 1.0, 0.25]]
        ]]),
        &strings,
        1,
        &property_schema_by_class,
        &HashMap::new(),
        &class_names,
    )
    .unwrap();

    assert_eq!(
        parsed[0].properties.get("Lifetime"),
        Some(&json!({
            "_type": "NumberRange",
            "min": 0.5,
            "max": 1.5,
        }))
    );
    assert_eq!(
        parsed[0].properties.get("DefaultPhysicalProperties"),
        Some(&json!({
            "_type": "PhysicalProperties",
            "customPhysics": false,
        }))
    );
    assert_eq!(
        parsed[0].properties.get("CustomPhysicalProperties"),
        Some(&json!({
            "_type": "PhysicalProperties",
            "customPhysics": true,
            "density": 1.0,
            "friction": 0.3,
            "elasticity": 0.5,
            "frictionWeight": 1.0,
            "elasticityWeight": 1.0,
            "acousticAbsorption": 0.25,
        }))
    );
}

fn vc_test_instance(
    id: &str,
    name: &str,
    class: &str,
    parent: Option<usize>,
    props: &[(&str, Value)],
) -> SettingsBytecodeInstance {
    let mut properties = Map::new();
    for (key, value) in props {
        properties.insert((*key).to_string(), value.clone());
    }
    SettingsBytecodeInstance {
        settings_id: id.into(),
        name: name.into(),
        class_name: class.into(),
        parent_index: parent,
        properties,
        attributes: Map::new(),
    }
}

#[test]
fn vc_merge_merges_disjoint_property_edits() {
    let base = settings_document(vec![
        vc_test_instance("root", "ReplicatedStorage", "ReplicatedStorage", None, &[]),
        vc_test_instance(
            "c",
            "Part",
            "Part",
            Some(0),
            &[("A", json!(1)), ("B", json!(1))],
        ),
    ]);
    let ours = settings_document(vec![
        vc_test_instance("root", "ReplicatedStorage", "ReplicatedStorage", None, &[]),
        vc_test_instance(
            "c",
            "Part",
            "Part",
            Some(0),
            &[("A", json!(2)), ("B", json!(1))],
        ),
    ]);
    let theirs = settings_document(vec![
        vc_test_instance("root", "ReplicatedStorage", "ReplicatedStorage", None, &[]),
        vc_test_instance(
            "c",
            "Part",
            "Part",
            Some(0),
            &[("A", json!(1)), ("B", json!(3))],
        ),
    ]);
    let (merged, conflicts) = merge_settings_documents(&base, &ours, &theirs, None);
    assert!(conflicts.is_empty());
    let child = merged
        .instances
        .iter()
        .find(|instance| instance.settings_id == "c")
        .expect("merged child");
    assert_eq!(child.properties.get("A"), Some(&json!(2)));
    assert_eq!(child.properties.get("B"), Some(&json!(3)));
}

#[test]
fn vc_merge_conflicts_on_same_property_and_prefer_resolves() {
    let base = settings_document(vec![
        vc_test_instance("root", "S", "Folder", None, &[]),
        vc_test_instance("c", "Part", "Part", Some(0), &[("A", json!(1))]),
    ]);
    let ours = settings_document(vec![
        vc_test_instance("root", "S", "Folder", None, &[]),
        vc_test_instance("c", "Part", "Part", Some(0), &[("A", json!(2))]),
    ]);
    let theirs = settings_document(vec![
        vc_test_instance("root", "S", "Folder", None, &[]),
        vc_test_instance("c", "Part", "Part", Some(0), &[("A", json!(3))]),
    ]);
    let (_, conflicts) = merge_settings_documents(&base, &ours, &theirs, None);
    assert_eq!(conflicts.len(), 1);
    assert!(conflicts[0].detail.contains("property A"));

    let (merged, conflicts) = merge_settings_documents(&base, &ours, &theirs, Some(false));
    assert!(conflicts.is_empty());
    let child = merged
        .instances
        .iter()
        .find(|instance| instance.settings_id == "c")
        .unwrap();
    assert_eq!(child.properties.get("A"), Some(&json!(3)));
}

#[test]
fn vc_merge_keeps_parallel_additions_with_colliding_ids() {
    let base = settings_document(vec![vc_test_instance("root", "S", "Folder", None, &[])]);
    let ours = settings_document(vec![
        vc_test_instance("root", "S", "Folder", None, &[]),
        vc_test_instance("editor:1", "OursChild", "Folder", Some(0), &[]),
    ]);
    let theirs = settings_document(vec![
        vc_test_instance("root", "S", "Folder", None, &[]),
        vc_test_instance("editor:1", "TheirsChild", "Folder", Some(0), &[]),
    ]);
    let (merged, conflicts) = merge_settings_documents(&base, &ours, &theirs, None);
    assert!(conflicts.is_empty());
    assert_eq!(merged.instances.len(), 3);
    let names: Vec<&str> = merged
        .instances
        .iter()
        .map(|instance| instance.name.as_str())
        .collect();
    assert!(names.contains(&"OursChild"));
    assert!(names.contains(&"TheirsChild"));
    let ids: HashSet<&str> = merged
        .instances
        .iter()
        .map(|instance| instance.settings_id.as_str())
        .collect();
    assert_eq!(ids.len(), 3, "colliding additions must get distinct ids");
}

#[test]
fn vc_merge_applies_clean_deletions_and_flags_delete_modify() {
    let base = settings_document(vec![
        vc_test_instance("root", "S", "Folder", None, &[]),
        vc_test_instance("c", "Part", "Part", Some(0), &[("A", json!(1))]),
    ]);
    let ours = base.clone();
    let theirs = settings_document(vec![vc_test_instance("root", "S", "Folder", None, &[])]);
    let (merged, conflicts) = merge_settings_documents(&base, &ours, &theirs, None);
    assert!(conflicts.is_empty());
    assert_eq!(merged.instances.len(), 1);

    let ours_modified = settings_document(vec![
        vc_test_instance("root", "S", "Folder", None, &[]),
        vc_test_instance("c", "Part", "Part", Some(0), &[("A", json!(9))]),
    ]);
    let (_, conflicts) = merge_settings_documents(&base, &ours_modified, &theirs, None);
    assert_eq!(conflicts.len(), 1);
    let (merged, conflicts) = merge_settings_documents(&base, &ours_modified, &theirs, Some(false));
    assert!(conflicts.is_empty());
    assert_eq!(merged.instances.len(), 1, "prefer theirs applies deletion");
    let (merged, conflicts) = merge_settings_documents(&base, &ours_modified, &theirs, Some(true));
    assert!(conflicts.is_empty());
    assert_eq!(merged.instances.len(), 2, "prefer ours keeps modification");
}

#[test]
fn vc_textconv_output_is_deterministic_and_masks_source() {
    let doc = settings_document(vec![
        vc_test_instance("root", "S", "Folder", None, &[]),
        vc_test_instance(
            "s",
            "Boot",
            "Script",
            Some(0),
            &[
                ("Source", json!("print(1)\nprint(2)\n")),
                ("RunContext", json!("Server")),
            ],
        ),
    ]);
    let first = settings_doc_to_text(&doc);
    let second = settings_doc_to_text(&doc);
    assert_eq!(first, second);
    assert!(first.contains("= S/Boot [Script] id=s"));
    assert!(first.contains("RunContext = \"Server\""));
    assert!(first.contains("Source = <2 lines,"));
    assert!(!first.contains("print(1)"), "source body must be masked");
}

#[test]
fn view_json_tree_nests_children_and_surfaces_source() {
    let doc = settings_document(vec![
        vc_test_instance("root", "Svc", "Folder", None, &[]),
        vc_test_instance("a", "A", "Folder", Some(0), &[("X", json!(1))]),
        vc_test_instance(
            "s",
            "Mod",
            "ModuleScript",
            Some(1),
            &[
                ("Source", json!("return 1")),
                ("RunContext", json!("Server")),
            ],
        ),
    ]);
    let tree = settings_doc_to_json_tree(&doc, Path::new("nowhere/store.renium"));
    assert_eq!(tree["instanceCount"], json!(3));
    let roots = tree["roots"].as_array().unwrap();
    assert_eq!(roots.len(), 1);
    assert_eq!(roots[0]["name"], json!("Svc"));
    assert_eq!(roots[0]["childCount"], json!(1));
    let a = &roots[0]["children"][0];
    assert_eq!(a["name"], json!("A"));
    assert_eq!(a["properties"]["X"], json!(1));
    let s = &a["children"][0];
    assert_eq!(s["className"], json!("ModuleScript"));
    assert_eq!(s["source"], json!("return 1"));
    assert!(s["properties"].get("Source").is_none());
    assert_eq!(s["properties"]["RunContext"], json!("Server"));
}

#[test]
fn vc_init_writes_policy_files_idempotently() {
    let dir = temp_dir("vc-init");
    vc_init(VcInitArgs {
        project_root: dir.clone(),
        skip_git: true,
        remote: None,
        git_path: "git".into(),
        pretty: false,
    })
    .unwrap();
    let attributes = fs::read_to_string(dir.join(".gitattributes")).unwrap();
    assert!(attributes.contains("*.renium binary diff=renium merge=renium"));
    let ignore = fs::read_to_string(dir.join(".gitignore")).unwrap();
    assert!(ignore.contains("/sourcemap.json"));
    let renium_ignore = fs::read_to_string(dir.join(".renium").join(".gitignore")).unwrap();
    assert_eq!(renium_ignore, RENIUM_DIR_GITIGNORE);

    vc_init(VcInitArgs {
        project_root: dir.clone(),
        skip_git: true,
        remote: None,
        git_path: "git".into(),
        pretty: false,
    })
    .unwrap();
    assert_eq!(fs::read_to_string(dir.join(".gitignore")).unwrap(), ignore);
    assert_eq!(
        fs::read_to_string(dir.join(".gitattributes")).unwrap(),
        attributes
    );
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn ensure_service_store_seeds_service_root_and_is_idempotent() {
    let dir = temp_dir("ensure-store");
    let settings = service_settings_path(&dir.join("src").join("ReplicatedStorage"));
    assert!(!settings.exists());
    ensure_service_store_exists(&settings, "ReplicatedStorage").unwrap();
    let doc = SettingsBytecode::read_file(&settings).unwrap();
    assert_eq!(doc.instances.len(), 1);
    assert_eq!(doc.instances[0].name, "ReplicatedStorage");
    assert_eq!(doc.instances[0].class_name, "ReplicatedStorage");
    assert!(doc.instances[0].parent_index.is_none());
    ensure_service_store_exists(&settings, "ReplicatedStorage").unwrap();
    assert_eq!(
        SettingsBytecode::read_file(&settings)
            .unwrap()
            .instances
            .len(),
        1
    );
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn missing_store_lock_error_is_actionable() {
    let dir = temp_dir("missing-store-msg");
    let settings = service_settings_path(&dir.join("src").join("Workspace"));
    let err = lock_existing_service_store(&settings)
        .map(drop)
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("No synced Renium store for service 'Workspace'"),
        "{err}"
    );
    assert!(err.contains("rbx ba Workspace"), "{err}");
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn bytecode_remove_instance_removes_source_files_and_empty_dirs() {
    let dir = temp_dir("remove-source-files");
    let service_dir = dir.join("src").join("ReplicatedStorage");
    fs::create_dir_all(&service_dir).unwrap();
    let settings_path = service_dir.join("__roblox_sync_settings.renium");
    let document = settings_document(vec![
        settings_instance("root", "ReplicatedStorage", "ReplicatedStorage", None),
        settings_instance("pkg", "Pkg", "Folder", Some(0)),
        settings_instance("script", "Child", "Script", Some(1)),
    ]);
    document.write_file(&settings_path).unwrap();
    let source_paths =
        build_editor_source_paths_by_index(&document, "ReplicatedStorage", &service_dir);
    let source_path = source_paths[2].as_ref().unwrap();
    fs::create_dir_all(source_path.parent().unwrap()).unwrap();
    fs::write(source_path, "print('delete me')").unwrap();
    assert!(source_path.exists());

    bytecode_remove_instance(BytecodeRemoveInstanceArgs {
        input: BytecodeFileArgs::settings_file(settings_path.clone()),
        selector: BytecodeInstanceSelectorArgs::by_settings_id(Some("pkg".into())),
        no_recursive: false,
        pretty: false,
    })
    .unwrap();

    assert!(!source_path.exists());
    assert!(!service_dir.join("Pkg").exists());
    let updated = SettingsBytecode::read_file(&settings_path).unwrap();
    assert_eq!(updated.instances.len(), 1);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn filesystem_instance_name_validation_rejects_paths_and_reserved_names() {
    assert_eq!(
        validate_filesystem_instance_name("ReplicatedStorage", "service").unwrap(),
        "ReplicatedStorage"
    );
    for invalid in ["", ".", "..", "../escape", r"..\escape", "CON", "Name."] {
        assert!(
            validate_filesystem_instance_name(invalid, "service").is_err(),
            "{invalid:?} should be rejected"
        );
    }
}

#[test]
fn bytecode_service_resolution_rejects_parent_directory() {
    assert!(
        resolve_bytecode_settings_file(None, Some(".."), None, Path::new("."), Path::new("src"),)
            .is_err()
    );
}

#[test]
fn existing_target_ancestor_must_stay_inside_root() {
    let root = temp_dir("ancestor");
    let outside = temp_dir("ancestor-outside");

    assert!(
        ensure_existing_ancestor_inside(&root, &root.join("missing").join("file"), "target")
            .is_ok()
    );
    assert!(ensure_existing_ancestor_inside(&root, &outside, "target").is_err());
    let _ = fs::remove_dir_all(root);
    let _ = fs::remove_dir_all(outside);
}

#[test]
fn bounded_line_reader_drains_oversized_requests() {
    let input = format!("{}\nvalid\n", "x".repeat(9));
    let mut reader = Cursor::new(input.into_bytes());
    let mut line = String::new();

    assert_eq!(
        read_bounded_line(&mut reader, &mut line, 8).unwrap(),
        BoundedLineRead::TooLong
    );
    assert_eq!(
        read_bounded_line(&mut reader, &mut line, 8).unwrap(),
        BoundedLineRead::Line
    );
    assert_eq!(line, "valid\n");
}

#[test]
fn strip_extended_prefix_handles_unc_paths() {
    if cfg!(windows) {
        assert_eq!(
            strip_extended_prefix(PathBuf::from(r"\\?\UNC\server\share\dir\f.luau")),
            PathBuf::from(r"\\server\share\dir\f.luau")
        );
        assert_eq!(
            strip_extended_prefix(PathBuf::from(r"\\?\C:\dir\f.luau")),
            PathBuf::from(r"C:\dir\f.luau")
        );
        assert_eq!(
            strip_extended_prefix(PathBuf::from(r"\\server\share\dir")),
            PathBuf::from(r"\\server\share\dir")
        );
    }
}

#[test]
fn nonfinite_float_json_roundtrip() {
    let nan = json_number_f64(f64::NAN);
    assert!(json_f64(&nan).unwrap().is_nan());
    let inf = json_number_f64(f64::INFINITY);
    assert_eq!(json_f64(&inf), Some(f64::INFINITY));
    let neg = json_number_f64(f64::NEG_INFINITY);
    assert_eq!(json_f64(&neg), Some(f64::NEG_INFINITY));
    assert_eq!(json_f64(&json!(1.5)), Some(1.5));
    assert_eq!(json_f64(&json!("inf")), None);
}

#[test]
fn sourcemap_relative_tolerates_root_form_mismatch() {
    if cfg!(windows) {
        assert_eq!(
            path_to_sourcemap_relative(
                Path::new(r"C:\proj"),
                Path::new(r"\\?\C:\proj\src\Workspace\A.luau")
            ),
            "src/Workspace/A.luau"
        );
        assert_eq!(
            path_to_sourcemap_relative(Path::new(r"c:\Proj"), Path::new(r"C:\proj\src\B.luau")),
            "src/B.luau"
        );
    } else {
        assert_eq!(
            path_to_sourcemap_relative(Path::new("/proj"), Path::new("/proj/src/C.luau")),
            "src/C.luau"
        );
    }
}

#[test]
fn write_bytes_if_changed_tolerates_readonly_mirrors() {
    let root = temp_dir("rw");
    let path = root.join("m.luau");
    fs::write(&path, "v1").unwrap();
    set_path_readonly(&path, true).unwrap();
    write_bytes_if_changed(&path, b"v2").unwrap();
    assert_eq!(fs::read_to_string(&path).unwrap(), "v2");
    assert!(fs::metadata(&path).unwrap().permissions().readonly());
    let _ = set_path_readonly(&path, false);
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn streaming_json_failure_preserves_the_existing_file() {
    struct BrokenJson;

    impl Serialize for BrokenJson {
        fn serialize<S>(&self, _serializer: S) -> std::result::Result<S::Ok, S::Error>
        where
            S: serde::Serializer,
        {
            Err(serde::ser::Error::custom(
                "intentional serialization failure",
            ))
        }
    }

    let root = temp_dir("atomic-json");
    let path = root.join("cache.json");
    fs::write(&path, "existing\n").unwrap();

    assert!(write_json_streaming(&path, &BrokenJson).is_err());
    assert_eq!(fs::read_to_string(&path).unwrap(), "existing\n");
    assert!(fs::read_dir(&root).unwrap().all(|entry| {
        !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .ends_with(".renium-tmp")
    }));
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn empty_full_reconcile_is_not_discarded() {
    let mut changes = EditorChangeSet::default();
    push_editor_instance_change(
        &mut changes,
        "reconcileService",
        "StarterGui",
        true,
        Vec::new(),
    );
    assert_eq!(changes.instance_changes.len(), 1);
    assert!(changes.instance_changes[0].instances.is_empty());
}

#[test]
fn protected_place_writes_patch_binary_properties_and_attributes() {
    let dir = temp_dir("protected-place-writes");
    let path = dir.join("place.rbxl");
    let mut dom = RbxWeakDom::new(RbxInstanceBuilder::new("DataModel"));
    let mut builder = RbxInstanceBuilder::new("MaterialService").with_name("MaterialService");
    builder.add_property("Use2022Materials", RbxVariant::Bool(false));
    let service = dom.insert(dom.root_ref(), builder);
    let output = File::create(&path).unwrap();
    rbx_binary::to_writer(BufWriter::new(output), &dom, &[service]).unwrap();
    let rows = vec![
        json!({
            "kind": "property",
            "pathSegments": ["MaterialService"],
            "pathOrdinals": [1],
            "name": "Use2022Materials",
            "value": true,
        }),
        json!({
            "kind": "attribute",
            "pathSegments": ["MaterialService"],
            "pathOrdinals": [1],
            "name": "ReniumTest",
            "value": 4,
        }),
    ];
    let review_rows = protected_write_rows_with_previous_values(&path, &rows).unwrap();
    assert_eq!(review_rows[0].get("oldValueKnown"), Some(&json!(true)));
    assert_eq!(review_rows[0].get("oldValue"), Some(&json!(false)));
    assert_eq!(review_rows[1].get("oldValueKnown"), Some(&json!(true)));
    assert_eq!(review_rows[1].get("oldValueMissing"), Some(&json!(true)));
    assert_eq!(patch_place_protected_writes(&path, &rows).unwrap(), 2);
    let input = File::open(&path).unwrap();
    let output_dom = rbx_binary::from_reader(BufReader::new(input)).unwrap();
    let output_service = output_dom
        .get_by_ref(output_dom.root().children()[0])
        .unwrap();
    assert_eq!(
        output_service.properties.get(&"Use2022MaterialsXml".into()),
        Some(&RbxVariant::Bool(true))
    );
    let attributes = match output_service.properties.get(&"Attributes".into()) {
        Some(RbxVariant::Attributes(attributes)) => attributes,
        other => panic!("unexpected attributes: {other:?}"),
    };
    assert_eq!(
        attributes.get("ReniumTest"),
        Some(&RbxVariant::Float64(4.0))
    );
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn protected_review_reads_migrated_mesh_id_value() {
    let dir = temp_dir("protected-mesh-review");
    let path = dir.join("place.rbxl");
    let mesh = RbxInstanceBuilder::new("MeshPart")
        .with_name("Mesh")
        .with_property(
            "MeshContent",
            RbxContent::from_uri("rbxassetid://93436871821239"),
        );
    let dom = RbxWeakDom::new(mesh);
    let output = File::create(&path).unwrap();
    rbx_binary::to_writer(BufWriter::new(output), &dom, &[dom.root_ref()]).unwrap();
    let rows = vec![json!({
        "kind": "property",
        "pathSegments": ["Mesh"],
        "pathOrdinals": [1],
        "name": "MeshId",
        "value": "rbxassetid://131536866771677",
    })];
    let review_rows = protected_write_rows_with_previous_values(&path, &rows).unwrap();
    assert_eq!(
        review_rows[0].get("oldValue"),
        Some(&json!("rbxassetid://93436871821239"))
    );
    assert!(review_rows[0].get("oldValueMissing").is_none());
    assert_eq!(patch_place_protected_writes(&path, &rows).unwrap(), 1);
    let patched_rows = protected_write_rows_with_previous_values(&path, &rows).unwrap();
    assert_eq!(
        patched_rows[0].get("oldValue"),
        Some(&json!("rbxassetid://131536866771677"))
    );
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn live_snapshot_preserves_unrelated_service_root_properties() {
    let database = rbx_reflection_database::get().unwrap();
    let mut dom = RbxWeakDom::new(RbxInstanceBuilder::new("DataModel"));
    let mut lighting = RbxInstanceBuilder::new("Lighting").with_name("Lighting");
    lighting.add_property("Brightness", RbxVariant::Float32(1.75));
    dom.insert(dom.root_ref(), lighting);
    let mut players = RbxInstanceBuilder::new("Players").with_name("Players");
    players.add_property("CharacterAutoLoads", RbxVariant::Bool(false));
    dom.insert(dom.root_ref(), players);
    let service_names = HashSet::from(["Lighting".to_string(), "Players".to_string()]);
    let mut values = rbx_dom_service_root_property_values(&dom, &service_names, database);
    let mut live_lighting = Map::new();
    live_lighting.insert("Brightness".to_string(), json!(3.5));
    merge_live_service_root_property_values(
        "Lighting",
        values.get_mut("Lighting").unwrap(),
        &live_lighting,
        database,
    );
    assert_eq!(
        values
            .get("Players")
            .and_then(|properties| properties.get("CharacterAutoLoads")),
        Some(&json!(false))
    );
    let mut live_players = Map::new();
    live_players.insert("MaxPlayers".to_string(), json!(60));
    live_players.insert("PreferredPlayers".to_string(), json!(60));
    merge_live_service_root_property_values(
        "Players",
        values.get_mut("Players").unwrap(),
        &live_players,
        database,
    );
    assert!(!values["Players"].contains_key("MaxPlayersInternal"));
    assert!(!values["Players"].contains_key("PreferredPlayersInternal"));
    let refs = BytecodeModelExportRefs::default();
    let encoded =
        encode_service_root_property_values("Players", &values["Players"], database, &refs);
    assert_eq!(
        encoded.get(&rbx_dom_weak::Ustr::from("CharacterAutoLoads")),
        Some(&RbxVariant::Bool(false))
    );
}

#[test]
fn game_settings_properties_are_not_sent_or_patched() {
    let path = vec!["Players".to_string()];
    assert!(is_externally_managed_editor_property(
        "Players",
        "Players",
        &path,
        "MaxPlayers"
    ));
    assert!(is_externally_managed_protected_write(&json!({
        "service": "Players",
        "className": "Players",
        "pathSegments": ["Players"],
        "name": "PreferredPlayers",
    })));
    assert!(!is_externally_managed_editor_property(
        "Players",
        "Players",
        &path,
        "CharacterAutoLoads"
    ));
    assert!(protected_write_matches_previous(&json!({
        "oldValueKnown": true,
        "oldValue": {"_type": "EnumItem", "name": "Improved", "value": 1},
        "value": {"_type": "EnumItem", "name": "Improved"},
    })));
    assert!(!protected_write_matches_previous(&json!({
        "oldValueKnown": true,
        "oldValueMissing": true,
        "value": false,
    })));
}

#[test]
fn protected_review_only_keeps_user_facing_properties() {
    let database = rbx_reflection_database::get().unwrap();
    assert!(!is_user_facing_protected_write(
        &json!({
            "kind": "attribute",
            "className": "Lighting",
            "name": "RBX_OriginalTechnologyOnFileLoad",
        }),
        database
    ));
    assert!(!is_user_facing_protected_write(
        &json!({
            "kind": "property",
            "className": "Lighting",
            "name": "ExtendLightRangeTo120",
        }),
        database
    ));
    assert!(!is_user_facing_protected_write(
        &json!({
            "kind": "property",
            "className": "Workspace",
            "name": "CollisionGroupData",
        }),
        database
    ));
    assert!(!is_user_facing_protected_write(
        &json!({
            "kind": "property",
            "className": "Lighting",
            "name": "Technology",
        }),
        database
    ));
    assert!(is_user_facing_protected_write(
        &json!({
            "kind": "property",
            "className": "MeshPart",
            "name": "MeshId",
        }),
        database
    ));
    assert!(is_user_facing_protected_write(
        &json!({
            "kind": "attribute",
            "className": "Part",
            "name": "CreatorNote",
        }),
        database
    ));
}
