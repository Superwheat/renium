//! Seeded simulation of a Live Sync push of a store the files hold edited.
//!
//! Each seed builds a store, edits a copy of it as the files do (values, a
//! run of instances added or removed, a rename, another class, a move, an
//! instance under another id at its place; at times with script files,
//! another store or another file changed beside it) and prepares the push
//! twice: from the instances that differ (`edited_store_alone`) and from the
//! whole documents, which say what is right. The prepared change, the plan
//! and what the write step sends to Studio are the same both ways.
//!
//! `crate::tests::support::run_seeds` names the variables that run more
//! seeds or replay one.

use super::*;
use crate::tests::support::{SimRng, run_seeds};
use std::sync::atomic::{AtomicUsize, Ordering};

const SEEDS: u64 = 512;
const STORE: &str = "files/Workspace/__roblox_sync_settings.renium";
const OTHER_STORE: &str = "files/ReplicatedStorage/__roblox_sync_settings.renium";

/// What the files changed, by `seed % 8`.
const KINDS: [&str; 8] = [
    "values",
    "a run",
    "values and a run",
    "a rename, at times to another class",
    "a move to the end",
    "a move under another parent",
    "another id at the same place",
    "values, a rename and a run",
];

/// The rows of a block, by two bits of the seed.
const BLOCK_ROWS: [usize; 4] = [1, 3, 8, 4096];

#[derive(Debug, Default)]
struct Counts {
    /// The stores prepared from the instances that differ, by kind and by
    /// block size.
    alone: [AtomicUsize; 8],
    alone_in_blocks: [AtomicUsize; 4],
    /// Those whose plan was carried out from them, with a request sent.
    sent: AtomicUsize,
    /// Of those, the ones beside another store that changed.
    sent_beside_a_store: AtomicUsize,
    /// Those whose plan was made again from the whole documents.
    planned_whole: AtomicUsize,
    /// The stores beside a changed file that is no store and no script.
    beside_another_file: AtomicUsize,
    /// The changes both ways refuse.
    refused: AtomicUsize,
    /// The edits that leave a reference no file states: not simulated.
    unstored: AtomicUsize,
}

fn edit(seed: u64, rng: &mut SimRng, store: &mut SettingsBytecode) {
    let values = |rng: &mut SimRng, store: &mut SettingsBytecode| {
        for _ in 0..1 + rng.below(3) {
            change_an_instance(rng, store);
        }
    };
    let rename = |rng: &mut SimRng, store: &mut SettingsBytecode| {
        let instance = 1 + rng.below(store.instances.len() - 1);
        let instance = &mut store.instances[instance];
        instance.name = format!("Renamed{}", rng.below(3));
        if rng.below(3) == 0 {
            let class = ["Folder", "Model"][usize::from(instance.class_name == "Folder")];
            instance.class_name = class.into();
        }
    };
    match seed % 8 {
        0 => values(rng, store),
        1 => add_or_remove_a_run(rng, store),
        2 => {
            values(rng, store);
            add_or_remove_a_run(rng, store);
        }
        3 => rename(rng, store),
        4 => move_a_leaf_to_the_end(rng, store),
        5 => {
            let count = store.instances.len();
            let (moved, under) = (1 + rng.below(count - 1), rng.below(count));
            let mut above = Some(under);
            while let Some(index) = above.filter(|index| *index != moved) {
                above = store.instances[index].parent_index;
            }
            if above.is_none() {
                store.instances[moved].parent_index = Some(under);
            }
        }
        6 => {
            let index = 1 + rng.below(store.instances.len() - 1);
            store.instances[index].settings_id = format!("anew-{index}");
            if rng.below(2) == 0 {
                values(rng, store);
            }
        }
        _ => {
            values(rng, store);
            rename(rng, store);
            add_or_remove_a_run(rng, store);
        }
    }
}

/// Whether a change prepared from the instances that differ is the change
/// the whole documents give: each instance it read is theirs, and each it
/// left unread stands, bare, for one the plan finds the same in both copies
/// under one id. Says whether it left any unread.
fn same_change(
    alone: Option<&PreparedEditorSettingsChange>,
    whole: Option<&PreparedEditorSettingsChange>,
) -> Result<bool, String> {
    use crate::settings::held::{stand_unchanged, stands_unchanged};
    let (alone, whole) = match (alone, whole) {
        (Some(alone), Some(whole)) => (alone, whole),
        (None, None) => return Ok(false),
        _ => return Err("one way prepares a store the other does not".into()),
    };
    let unread = |instance: &SettingsBytecodeInstance| stands_unchanged(&instance.properties);
    for (copy, read, held) in [
        ("earlier", &alone.previous, &whole.previous),
        ("edited", &alone.current, &whole.current),
    ] {
        if read.instances.len() != held.instances.len() {
            return Err(format!("the {copy} copy holds other instances"));
        }
        for (index, (read, held)) in read.instances.iter().zip(&held.instances).enumerate() {
            let mut expected = held.clone();
            if unread(read) {
                expected = held.bare();
                if let Some(unique_id) = held.properties.get("UniqueId") {
                    expected
                        .properties
                        .insert("UniqueId".into(), unique_id.clone());
                }
                stand_unchanged(&mut expected.properties);
            }
            if *read != expected {
                return Err(format!(
                    "instance {index} of the {copy} copy is {read:?}, of the whole document {expected:?}"
                ));
            }
        }
    }
    let edited = (whole.current.instances.iter().enumerate())
        .map(|(index, instance)| (instance.settings_id.as_str(), index))
        .collect::<HashMap<_, _>>();
    let mut left = 0;
    for (index, instance) in alone.previous.instances.iter().enumerate() {
        if !unread(instance) {
            continue;
        }
        left += 1;
        let alike = edited.get(instance.settings_id.as_str()).is_some_and(|at| {
            unread(&alone.current.instances[*at])
                && settings_instances_equal(&whole.current, *at, &whole.previous, index)
        });
        if !alike {
            return Err(format!(
                "instance {index} of the earlier copy was left unread, and the files hold it otherwise: {:?}",
                whole.previous.instances[index]
            ));
        }
    }
    if left
        != alone
            .current
            .instances
            .iter()
            .filter(|now| unread(now))
            .count()
    {
        return Err("the two copies leave other instances unread".into());
    }
    Ok(left > 0)
}

fn simulate(seed: u64, counts: &Counts) -> Result<(), String> {
    use crate::settings::bytecode::blocks::sim::in_blocks;
    use crate::settings::equivalence::holds_unread_instances;

    let mut rng = SimRng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
    let blocks = (seed >> 4) as usize % 4;
    // A store with a reference no file states is made again.
    let mut before = loop {
        let store = changed_instances_store(&mut rng);
        if in_blocks(&store, BLOCK_ROWS[blocks]).is_ok() {
            break store;
        }
    };
    // An older file states a property under another name, and some stores
    // hold a package.
    if rng.below(5) == 0 {
        for instance in &mut before.instances {
            if instance.class_name == "Part"
                && let Some(size) = instance.properties.remove("Size")
            {
                instance.properties.insert("size".into(), size);
            }
        }
    }
    if rng.below(8) == 0 {
        let under = 1 + rng.below(before.instances.len() - 1);
        let mut link = SettingsBytecodeInstance::new(
            "link".into(),
            "PackageLink".into(),
            "PackageLink".into(),
            Some(under),
        );
        link.properties
            .insert("PackageId".into(), json!("rbxassetid://77"));
        before.instances.push(link);
    }
    // An instance names by its id one the files do not hold yet: the next a
    // store edit adds here, or one another store is about to hold.
    let count = before.instances.len();
    if rng.below(3) == 0 {
        let later = json!({"_type": "Ref", "settingsId": format!("editor:{count}")});
        before.instances[1 + rng.below(count - 1)]
            .properties
            .insert("Later".into(), later);
    }
    // Another store changes in the same push, by two more bits of the seed:
    // it gains the instance two of this one name, or a value alone.
    let beside = [Some(true), Some(false), None, None][(seed >> 6) as usize % 4];
    for _ in 0..2 * usize::from(beside.is_some()) {
        let elsewhere = json!({
            "_type": "Ref",
            "settingsId": "fresh",
            "pathSegments": ["ReplicatedStorage", "Fresh"],
            "pathOrdinals": [1, 1],
        });
        before.instances[1 + rng.below(count - 1)]
            .properties
            .insert("Elsewhere".into(), elsewhere);
    }
    let other_store = |edited: bool| {
        let instance = |id: &str, name: &str, class: &str, parent| {
            SettingsBytecodeInstance::new(id.into(), name.into(), class.into(), parent)
        };
        let service = "ReplicatedStorage";
        let mut instances = vec![
            instance("shared", service, service, None),
            instance("kept", "Kept", "Folder", Some(0)),
        ];
        match beside.filter(|_| edited) {
            Some(true) => instances.push(instance("fresh", "Fresh", "Folder", Some(0))),
            Some(false) => drop(instances[1].attributes.insert("Level".into(), json!(1))),
            None => {}
        }
        SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances,
        }
    };
    let mut now = before.clone();
    edit(seed, &mut rng, &mut now);
    // A file that states no camera of the viewport takes the earlier one.
    if rng.below(6) == 0 {
        now.instances[0].properties.remove("CurrentCamera");
    }
    let (scripts, _) = scripts_beside(seed >> 3, &mut now, &mut before);
    let another_file = rng.below(10) == 0;
    let store = PathBuf::from(STORE);
    let other = PathBuf::from(OTHER_STORE);
    let snapshot = |document: &SettingsBytecode, edited: bool| {
        let stored = |document: &SettingsBytecode| {
            in_blocks(document, BLOCK_ROWS[blocks])
                .map(SnapshotEntry::File)
                .map_err(|error| format!("{error:#}"))
        };
        let mut entries = BTreeMap::from([(store.clone(), stored(document)?)]);
        if beside.is_some() {
            entries.insert(other.clone(), stored(&other_store(edited))?);
        }
        for (name, files, earlier) in &scripts {
            if let Some(text) = if edited { files } else { earlier } {
                entries.insert(
                    store.with_file_name(name),
                    SnapshotEntry::File(text.clone().into_bytes()),
                );
            }
        }
        if another_file {
            entries.insert(
                store.with_file_name("notes.txt"),
                SnapshotEntry::File(vec![u8::from(edited)]),
            );
        }
        Ok::<_, String>(ProjectSnapshot { entries })
    };
    let previous = snapshot(&before, false)?;
    // An edit can leave a reference no file states.
    let Ok(current) = snapshot(&now, true) else {
        counts.unstored.fetch_add(1, Ordering::Relaxed);
        return Ok(());
    };
    let scopes = [PathBuf::from("files")];
    let changed = (previous.entries.keys().chain(current.entries.keys()))
        .filter(|path| {
            !entries_equivalent(
                path,
                previous.entries.get(*path),
                current.entries.get(*path),
            )
        })
        .cloned()
        .collect::<HashSet<_>>();
    let prepare = |by_changed: bool| {
        prepare_editor_settings_changes_from(&previous, &current, &scopes, by_changed)
            .map_err(|error| format!("{error:#}"))
    };
    let (mut whole, mut alone) = match (prepare(false), prepare(true)) {
        (Ok(whole), Ok(alone)) => (whole, alone),
        (Err(whole), Err(alone)) if whole == alone => {
            counts.refused.fetch_add(1, Ordering::Relaxed);
            return Ok(());
        }
        (whole, alone) => {
            return Err(format!(
                "the instances that differ refuse the change with {:?}, the whole documents with {:?}",
                alone.err(),
                whole.err()
            ));
        }
    };
    same_change(alone.get(&other), whole.get(&other))?;
    let stands = same_change(alone.get(&store), whole.get(&store))?;
    if stands && another_file {
        return Err("read by its changed instances beside a file that is no store".into());
    }
    let planned_from = |prepared: &mut HashMap<PathBuf, PreparedEditorSettingsChange>| {
        incremental_push_plan((&previous, &current, &scopes), &changed, prepared, false)
            .map_err(|error| format!("no plan: {error:#}"))
    };
    let (expected, planned) = (planned_from(&mut whole), planned_from(&mut alone));
    let carried = (alone.get(&store)).is_some_and(|change| holds_unread_instances(&change.current));
    let unplanned = carried && !planned.as_ref().is_ok_and(plan_reads_changed_alone);
    // What the write step sends to Studio for a plan, made from the
    // documents the push prepared beside the files as it stages them.
    let project_root = crate::tests::support::temp_dir(&format!("live-store-edit-{seed}"));
    let staged = current.entries.keys().cloned().collect();
    apply_snapshot_paths(&project_root, &staged, &current).map_err(|error| format!("{error:#}"))?;
    let pushed = |plan: Result<ReconcilePushPlan, String>,
                  prepared: &mut HashMap<PathBuf, PreparedEditorSettingsChange>| {
        let plan = match plan {
            Ok(plan) => plan,
            Err(error) => return vec![error],
        };
        let mut said = vec![plan_text(&plan)];
        if plan.target_settings_ids.is_empty() && plan.changed_paths.is_empty() {
            return said;
        }
        let mut args = crate::cli::PushEditorChangesArgs::new(
            crate::cli::ProjectSourceArgs {
                project_root: project_root.clone(),
                src_root: PathBuf::from("files"),
            },
            crate::cli::BridgeConnectionArgs::local(0.1),
        );
        args.changed_paths.clone_from(&plan.changed_paths);
        args.target_settings_ids
            .clone_from(&plan.target_settings_ids);
        args.exact_target_ids = true;
        args.targets_extend_paths = true;
        args.override_packages = true;
        let documents = (prepared.drain())
            .map(|(path, change)| {
                let service = path.parent().and_then(Path::file_name).unwrap_or_default();
                let service = service.to_string_lossy().into_owned();
                (service, Arc::new(change.current))
            })
            .collect();
        let sent =
            crate::editor::sync::collect_prepared_editor_changes(&args, documents, HashSet::new())
                .map_err(|error| format!("collect: {error:#}"))
                .and_then(|mut changes| {
                    amend_reconciled_changes(&mut changes, plan)
                        .map_err(|error| format!("amend: {error:#}"))?;
                    request_text(&changes)
                });
        said.push(sent.unwrap_or_else(|error| format!("not sent: {error}")));
        said
    };
    let (expected, said) = (pushed(expected, &mut whole), pushed(planned, &mut alone));
    let _ = std::fs::remove_dir_all(&project_root);
    if said != expected {
        return Err(format!(
            "from the instances that differ ({stands}, {carried}):\n{}\nfrom the whole documents:\n{}",
            said.join("\n"),
            expected.join("\n")
        ));
    }
    if unplanned {
        return Err("a plan the instances that differ do not carry out kept them unread".into());
    }
    if another_file {
        counts.beside_another_file.fetch_add(1, Ordering::Relaxed);
    }
    if stands {
        counts.alone[(seed % 8) as usize].fetch_add(1, Ordering::Relaxed);
        counts.alone_in_blocks[blocks].fetch_add(1, Ordering::Relaxed);
        if !carried {
            counts.planned_whole.fetch_add(1, Ordering::Relaxed);
        } else if said.len() == 2 && said[1].contains("\"properties\":{\"") {
            counts.sent.fetch_add(1, Ordering::Relaxed);
            if beside.is_some() {
                counts.sent_beside_a_store.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
    Ok(())
}

#[test]
fn a_store_edit_prepared_from_its_changed_instances_is_pushed_as_the_whole_documents_push_it() {
    let counts = Counts::default();
    run_seeds(SEEDS, |seed, _| simulate(seed, &counts));
    if std::env::var_os("RENIUM_SIM_SEED").is_some() {
        return;
    }
    let of = |count: &AtomicUsize| count.load(Ordering::Relaxed);
    let least = |counts: &[AtomicUsize], least: &[usize]| {
        let counts = counts.iter().zip(least);
        counts.into_iter().all(|(count, least)| of(count) >= *least)
    };
    assert!(
        least(&counts.alone, &[18, 24, 24, 23, 9, 22, 0, 20])
            && least(&counts.alone_in_blocks, &[33; 4])
            && of(&counts.sent) >= 50
            && of(&counts.sent_beside_a_store) >= 8
            && of(&counts.planned_whole) >= 50
            && of(&counts.beside_another_file) >= 16
            && of(&counts.refused) >= 2
            && of(&counts.unstored) <= 70,
        "too few of a kind ({KINDS:?}, in blocks of {BLOCK_ROWS:?} rows): {counts:?}"
    );
}
