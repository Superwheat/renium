use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::settings_bytecode::{
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
    path
}
