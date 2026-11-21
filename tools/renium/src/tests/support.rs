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

/// The generator the simulations draw from: one seed gives the same run on
/// every machine.
pub(crate) struct SimRng(pub(crate) u64);

impl SimRng {
    pub(crate) fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut value = self.0;
        value = (value ^ (value >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        value = (value ^ (value >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        value ^ (value >> 31)
    }

    pub(crate) fn below(&mut self, bound: usize) -> usize {
        (self.next() % bound as u64) as usize
    }

    pub(crate) fn pick<T: Clone>(&mut self, items: &[T]) -> Option<T> {
        (!items.is_empty()).then(|| items[self.below(items.len())].clone())
    }

    pub(crate) fn shuffle<T>(&mut self, items: &mut [T]) {
        for index in (1..items.len()).rev() {
            items.swap(index, self.below(index + 1));
        }
    }
}

/// Runs a simulation once per seed and fails with every seed that did not
/// hold. `RENIUM_SIM_SEEDS=N` runs N seeds instead of `default_seeds`,
/// `RENIUM_SIM_FIRST=S` starts at seed S, and `RENIUM_SIM_SEED=S` replays one
/// seed and prints its steps. Gives the number of seeds it ran.
pub(crate) fn run_seeds(
    default_seeds: u64,
    run: impl Fn(u64, &mut Vec<String>) -> Result<(), String> + Sync,
) -> u64 {
    let number = |name: &str| {
        std::env::var(name)
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
    };
    let replay = number("RENIUM_SIM_SEED");
    let first = replay.or(number("RENIUM_SIM_FIRST")).unwrap_or(0);
    let count = match replay {
        Some(_) => 1,
        None => number("RENIUM_SIM_SEEDS").unwrap_or(default_seeds),
    };
    // The code under simulation runs its own work on the shared thread pool,
    // so the seeds are spread over a few plain threads instead of that pool.
    let next = AtomicU64::new(first);
    let failures = std::sync::Mutex::new(Vec::new());
    let workers = number("RENIUM_SIM_THREADS").map_or_else(
        || std::thread::available_parallelism().map_or(2, |cores| cores.get().div_ceil(4)),
        |threads| threads as usize,
    );
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| {
                loop {
                    let seed = next.fetch_add(1, Ordering::Relaxed);
                    if seed >= first + count {
                        break;
                    }
                    let mut log = Vec::new();
                    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        run(seed, &mut log)
                    }))
                    .unwrap_or_else(|_| Err("it panicked".to_string()));
                    if let Err(failure) = outcome {
                        let steps = match replay {
                            Some(_) => format!("{}\n", log.join("\n")),
                            None => String::new(),
                        };
                        failures
                            .lock()
                            .unwrap()
                            .push((seed, format!("{steps}seed {seed}: {failure}")));
                    }
                }
            });
        }
    });
    let mut failures = failures.into_inner().unwrap();
    failures.sort();
    assert!(
        failures.is_empty(),
        "{} of {count} seeds failed (replay one with RENIUM_SIM_SEED):\n{}",
        failures.len(),
        failures
            .iter()
            .take(12)
            .map(|(_, text)| text.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    );
    count
}

/// Raw saved names, including both a service-name collision and a same-Name
/// root before the service. The Workspace reference selects the second Part.
pub(crate) fn renamed_service_place() -> rbx_dom_weak::WeakDom {
    rbx_xml::from_reader_default(
        br#"<roblox version="4">
          <Item class="Folder" referent="alias"><Properties><string name="Name">Vault</string></Properties></Item>
          <Item class="ServerStorage" referent="service">
            <Properties><string name="Name">Vault</string><int64 name="SourceAssetId">42</int64></Properties>
            <Item class="Folder" referent="parent"><Properties><string name="Name">InService</string></Properties>
              <Item class="Part" referent="first"><Properties><string name="Name">Renamed</string><bool name="Anchored">false</bool></Properties></Item>
              <Item class="Part" referent="target"><Properties><string name="Name">Renamed</string><bool name="Anchored">true</bool></Properties></Item>
            </Item>
          </Item>
          <Item class="Folder" referent="namesake">
            <Properties><string name="Name">ServerStorage</string><int64 name="SourceAssetId">99</int64></Properties>
            <Item class="Folder" referent="decoy-parent"><Properties><string name="Name">InService</string></Properties>
              <Item class="Part" referent="decoy-first"><Properties><string name="Name">Renamed</string><bool name="Anchored">false</bool></Properties></Item>
              <Item class="Part" referent="decoy-second"><Properties><string name="Name">Renamed</string><bool name="Anchored">false</bool></Properties></Item>
            </Item>
          </Item>
          <Item class="Workspace" referent="workspace"><Properties><string name="Name">Workspace</string></Properties>
            <Item class="ObjectValue" referent="reference"><Properties><string name="Name">External</string><Ref name="Value">target</Ref></Properties></Item>
          </Item>
        </roblox>"#
            .as_slice(),
    )
    .expect("raw renamed-service place")
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
