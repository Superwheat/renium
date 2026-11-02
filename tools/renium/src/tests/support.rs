use std::fs;
use std::path::{Path, PathBuf};

use serde_json::json;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::settings::bytecode::{
    SETTINGS_BINARY_VERSION, SettingsBytecode, SettingsBytecodeInstance,
};

pub(crate) fn settings_document(instances: Vec<SettingsBytecodeInstance>) -> SettingsBytecode {
    SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances,
    }
}

pub(crate) fn settings_instance(
    settings_id: impl Into<String>,
    name: impl Into<String>,
    class_name: impl Into<String>,
    parent_index: Option<usize>,
) -> SettingsBytecodeInstance {
    SettingsBytecodeInstance::new(
        settings_id.into(),
        name.into(),
        class_name.into(),
        parent_index,
    )
}

pub(crate) fn temp_dir(tag: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!("renium-test-{tag}-{timestamp}-{sequence}"));
    fs::create_dir_all(&path).unwrap();
    crate::system::files::canonical_path(&path)
        .map(crate::system::files::strip_extended_prefix)
        .unwrap_or(path)
}

/// A project whose ReplicatedStorage.Shared is a packed read-only link, as
/// `rbx lkp` leaves it: the store carries Util's text inline, the package
/// under links/ holds the same subtree, and Util has no source file. Returns
/// the ReplicatedStorage service directory.
pub(crate) fn write_packed_link_project(root: &Path) -> PathBuf {
    let service_dir = root.join("src").join("ReplicatedStorage");
    fs::create_dir_all(root.join("instances")).unwrap();
    fs::create_dir_all(&service_dir).unwrap();
    fs::create_dir_all(root.join("links")).unwrap();
    fs::write(root.join("renium.project.jsonc"), br#"{"schemaVersion":1}"#).unwrap();
    let mut util = settings_instance("editor:2", "Util", "ModuleScript", Some(1));
    util.properties
        .insert("Source".to_string(), json!("return { version = 1 }"));
    settings_document(vec![
        settings_instance("editor:0", "ReplicatedStorage", "ReplicatedStorage", None),
        settings_instance("editor:1", "Shared", "Folder", Some(0)),
        util,
    ])
    .write_file(&root.join("instances").join("ReplicatedStorage.renium"))
    .unwrap();
    let mut packed = settings_instance("editor:2", "Util", "ModuleScript", Some(0));
    packed
        .properties
        .insert("Source".to_string(), json!("return { version = 1 }"));
    settings_document(vec![
        settings_instance("editor:1", "Shared", "Folder", None),
        packed,
    ])
    .write_file(&root.join("links").join("shared.renium"))
    .unwrap();
    fs::write(
        root.join("renium-link.json"),
        br#"{"version":1,"links":[{"id":"shared","readOnly":true,"source":{"type":"local","path":"links/shared.renium"},"targets":[{"service":"ReplicatedStorage","path":["ReplicatedStorage","Shared"]}]}],"broken":[]}"#,
    )
    .unwrap();
    service_dir
}
