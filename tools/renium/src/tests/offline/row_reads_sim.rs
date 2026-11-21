//! Seeded runs of the commands that read a store in blocks by its rows where
//! they read any other store whole: the merge of three stores, the repack of
//! one, the export of a model, and a build that goes into a base place read
//! ahead of it. The whole documents are the oracle.
//!
//! Each seed generates the stores of a project as the simulation of the
//! structure edits does and writes them in blocks to two projects, ours and
//! theirs, now and then as another writer's. Each of the two then runs a few
//! of that simulation's commands and a few edits of a value, which often meet
//! on one instance, and now and then gives an instance another UniqueId. Four
//! laws hold:
//!
//! 1. `merge_stores_by_rows` of the three files of a service gives what the
//!    merge of their whole documents gives, or nothing: the same conflicts,
//!    and the file a whole encode of the merged document gives. `rbx vcm`
//!    prints and writes for the files in blocks what it does for the same
//!    documents in version 11 stores, which it merges whole.
//! 2. A store this writer wrote with its references by name left as they
//!    stood is encoded again as the whole encode of its document, block by
//!    block and by `rbx bpack`.
//! 3. `rbx bem` and `rbx mep` print and write the same model from a store in
//!    blocks as from the version 11 store of its document.
//! 4. A build goes into a base place read ahead of it as into the place read
//!    whole, whichever services the read is told the build brings.
//!
//! `crate::tests::support::run_seeds` names the variables that run more
//! seeds or replay one.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::structure_edits_sim::{KINDS, SERVICES, command, fill, project, run_on, tree};
use super::*;
use crate::project::version_control::{
    ROWS_NOT_READ, VcMergeConflict, merge_settings_documents, merge_stores_by_rows,
};
use crate::settings::bytecode::blocks::encode_again_cut;
use crate::settings::bytecode::blocks::sim::{
    FORMATS, block_rows_of, foreign_file, in_blocks, in_dense_blocks,
};
use crate::settings::bytecode::{
    decode_settings_bytecode, encode_settings_bytecode,
    encode_settings_bytecode_with_dense_references,
};
use crate::tests::support::{SimRng, run_seeds};

const SEEDS: u64 = 24;
const DONE_NAMES: [&str; 6] = [
    "merge by rows",
    "merge by rows with conflicts",
    "merge left to the whole documents",
    "repack that changed a file",
    "model",
    "build into a base",
];
/// How often each of `DONE_NAMES` went through.
static DONE: [AtomicUsize; DONE_NAMES.len()] = [const { AtomicUsize::new(0) }; DONE_NAMES.len()];

fn text(error: anyhow::Error) -> String {
    format!("{error:#}")
}

/// What a run printed, and the file it wrote at `written`.
type Ran = ((Option<String>, Value), Option<Vec<u8>>);

fn ran(fixture: &Fixture, args: &[&str], written: &str) -> Ran {
    let _ = fs::remove_file(written);
    let outcome = fixture.json(args);
    ((outcome.error, outcome.value), fs::read(written).ok())
}

/// Law 1 for the base, ours and theirs of one service.
fn merged(scratch: &Fixture, files: [&[u8]; 3], prefer: Option<bool>) -> Result<(), String> {
    let documents = files.map(|file| decode_settings_bytecode(file).unwrap());
    let [base, ours, theirs] = &documents;
    let (whole, conflicts) = merge_settings_documents(base, ours, theirs, prefer);
    let said = |conflicts: &[VcMergeConflict]| {
        (conflicts.iter())
            .map(|conflict| (conflict.path.clone(), conflict.detail.clone()))
            .collect::<Vec<_>>()
    };
    let done = match merge_stores_by_rows(files[0], files[1], files[2], prefer).map_err(text)? {
        None => 2,
        Some((_, _, found)) if said(&found) != said(&conflicts) => {
            return Err("the merge by rows found other conflicts".to_string());
        }
        Some(_) if !conflicts.is_empty() => 1,
        Some((file, instances, _)) => {
            let expected = in_blocks(&whole, block_rows_of(files[1])).map_err(text)?;
            if file != expected || instances != whole.instances.len() {
                return Err("the merge by rows is not the whole encode of the merge".to_string());
            }
            0
        }
    };
    DONE[done].fetch_add(1, Ordering::Relaxed);
    let out = scratch.path("merged.renium");
    let run = |whole: bool| -> Result<Ran, String> {
        let mut paths = Vec::new();
        for (name, (file, document)) in ["base", "ours", "theirs"]
            .into_iter()
            .zip(files.iter().zip(&documents))
        {
            let bytes = match whole {
                true => (FORMATS[0].encode)(document).map_err(text)?,
                false => file.to_vec(),
            };
            paths.push(scratch.path(&format!("{name}.renium")));
            fs::write(&paths[paths.len() - 1], bytes).unwrap();
        }
        let side = prefer.map(|ours| if ours { "ours" } else { "theirs" });
        let mut args: Vec<&str> = vec!["vcm", &paths[0], &paths[1], &paths[2], "-o", &out];
        args.extend(["--path", "store"]);
        args.extend(side.iter().flat_map(|side| ["--prefer", *side]));
        Ok(ran(scratch, &args, &out))
    };
    if run(true)? != run(false)? {
        return Err("rbx vcm gave the stores in blocks another merge".to_string());
    }
    Ok(())
}

/// Law 2 for a generated document, which names its own instances by id and
/// by path, written to `side` as the store of `service`.
fn repacked(
    side: &Fixture,
    (service, document): (&str, &SettingsBytecode),
    block_rows: usize,
) -> Result<(), String> {
    let whole_of = |file: &[u8], block_rows: usize| {
        decode_settings_bytecode(file).and_then(|document| in_blocks(&document, block_rows))
    };
    let Ok(dense) = in_dense_blocks(document, block_rows) else {
        return Ok(());
    };
    let again = encode_again_cut(&dense, block_rows).map_err(text)?;
    if again != Some(whole_of(&dense, block_rows).map_err(text)?) {
        return Err(format!(
            "{service}: its blocks encoded again are not its whole encode"
        ));
    }
    let dense = encode_settings_bytecode_with_dense_references(document).map_err(text)?;
    let whole = encode_settings_bytecode(&decode_settings_bytecode(&dense).unwrap()).unwrap();
    fs::create_dir_all(side.store(service).parent().unwrap()).unwrap();
    fs::write(side.store(service), &dense).unwrap();
    let (store, root) = (side.store_arg(service), side.path(""));
    let printed = side.json(&["bpack", &store, "-r", &root]);
    let changed = usize::from(dense != whole);
    if printed.error.is_some()
        || printed.value["changedFiles"] != json!(changed)
        || fs::read(side.store(service)).unwrap() != whole
    {
        return Err(format!(
            "{service}: rbx bpack did not give its whole encode"
        ));
    }
    DONE[3].fetch_add(changed, Ordering::Relaxed);
    Ok(())
}

/// Law 3 for one instance of `side`, a mesh as often as any other, whose
/// stores it leaves in version 11.
fn exported(rng: &mut SimRng, side: &Fixture, log: &mut Vec<String>) -> Result<(), String> {
    let service = SERVICES[rng.below(SERVICES.len())];
    if !side.store(service).is_file() {
        return Ok(());
    }
    let document = side.read_store(service);
    let meshes = (document.instances.iter())
        .filter(|instance| instance.class_name == "MeshPart")
        .collect::<Vec<_>>();
    let id = match rng.below(2) {
        0 if !meshes.is_empty() => &meshes[rng.below(meshes.len())].settings_id,
        _ => &document.instances[rng.below(document.instances.len())].settings_id,
    };
    let (model, store, root) = (
        side.path("model.rbxm"),
        side.store_arg(service),
        side.path(""),
    );
    let args: Vec<&str> = match rng.below(2) {
        0 => vec!["bem", "-f", &store, "-i", id, "-o", &model],
        _ => vec!["mep", service, "-i", id, "-o", &model, "-r", &root],
    };
    log.push(args.join(" "));
    let blocks = ran(side, &args, &model);
    for service in SERVICES
        .into_iter()
        .filter(|service| side.store(service).is_file())
    {
        let whole = (FORMATS[0].encode)(&side.read_store(service)).map_err(text)?;
        fs::write(side.store(service), whole).unwrap();
    }
    let whole = ran(side, &args, &model);
    DONE[4].fetch_add(usize::from(whole.1.is_some()), Ordering::Relaxed);
    if blocks != whole {
        return Err(format!(
            "the model of {id} in {service} differs: {:?}",
            (blocks.0, whole.0)
        ));
    }
    Ok(())
}

/// Law 4 for the fixture project and a place that holds, beside what the
/// project's services hold, a service the project lacks that names a part of
/// one it has, the hull of a mesh of the project, and now and then a second
/// Workspace or a store less. One place in four is an XML file.
fn built_into_a_base(rng: &mut SimRng) -> Result<(), String> {
    use crate::rbx::model::{PlaceBase, build_rbx_place, merge_build_into_read_base};
    use rbx_dom_weak::types::{ContentId, SharedString};
    use rbx_dom_weak::{InstanceBuilder, WeakDom};

    let fixture = Fixture::new("rows-base");
    let mut place = sample_place_dom();
    let root = place.root_ref();
    let named = |place: &WeakDom, name: &str| {
        let found = place.descendants().find(|instance| instance.name == name);
        found.unwrap().referent()
    };
    let (door, scripts) = (named(&place, "Door"), named(&place, "ServerScriptService"));
    place.insert(scripts, InstanceBuilder::new("Folder").with_name("Jobs"));
    // A mesh the project holds without the hull the place holds for it.
    let mut workspace = fixture.read_store("Workspace");
    let hull = settings_instance("hull", "Hull", "MeshPart", Some(0));
    let mesh = "rbxassetid://7";
    (workspace.instances).push(with_values(hull, &[("MeshId", json!(mesh))], &[]));
    fixture.write_store("Workspace", &workspace);
    let hull = (InstanceBuilder::new("MeshPart").with_name("Hull"))
        .with_property("MeshId", ContentId::from(mesh))
        .with_property("PhysicalConfigData", SharedString::new(vec![7, 7, 7]));
    place.insert(named(&place, "Workspace"), hull);
    let lighting = place.insert(root, InstanceBuilder::new("Lighting"));
    let outside = InstanceBuilder::new("ObjectValue").with_name("Outside");
    place.insert(
        lighting,
        outside.with_property("Value", rbx_dom_weak::types::Variant::Ref(door)),
    );
    let note = InstanceBuilder::new("StringValue").with_name("Note");
    place.insert(lighting, note.with_property("Value", "kept"));
    if rng.below(2) == 0 {
        let twin = place.insert(root, InstanceBuilder::new("Workspace").with_name("Twin"));
        let kept = InstanceBuilder::new("Part").with_name("Kept");
        place.insert(twin, kept.with_property("Anchored", true));
    }
    if rng.below(2) == 0 {
        fs::remove_file(fixture.store("ServerScriptService")).unwrap();
    }
    let base = (fixture.root).join(["base.rbxl", "base.rbxlx"][usize::from(rng.below(4) == 0)]);
    write_place_file(&base, &place);
    let services = ["Workspace", "ReplicatedStorage", "ServerScriptService"];
    let built = |brought: Vec<&str>| -> anyhow::Result<Vec<u8>> {
        let services = services.map(str::to_string).to_vec();
        let src = fixture.root.join("src");
        let mut build = build_rbx_place(&src, services, None, false, false, false)?;
        let dom = std::mem::replace(&mut build.dom, WeakDom::new(InstanceBuilder::new("Empty")));
        let brought = brought.into_iter().map(str::to_string).collect();
        let read = PlaceBase::read(&base, &brought)?;
        let (dom, roots) = merge_build_into_read_base(&build, dom, read)?;
        let mut bytes = Vec::new();
        rbx_binary::to_writer(&mut bytes, &dom, &roots)?;
        Ok(bytes)
    };
    let whole = built(Vec::new()).map_err(text)?;
    let stored = services
        .into_iter()
        .filter(|service| fixture.store(service).is_file());
    for (told, ahead) in [
        ("it brings", built(stored.collect())),
        (
            "the place holds",
            built([&services[..], &["Lighting"]].concat()),
        ),
    ] {
        if ahead.map_err(text)? != whole {
            return Err(format!(
                "a base read ahead for the services {told} took the build otherwise"
            ));
        }
    }
    DONE[5].fetch_add(1, Ordering::Relaxed);
    Ok(())
}

fn run_seed(seed: u64, log: &mut Vec<String>) -> Result<(), String> {
    let mut rng = SimRng(seed ^ 0x0b_10c5);
    let block_rows = [2, 3, 5, 8, 13, 4096][rng.below(6)];
    let mut stores = BTreeMap::new();
    for service in SERVICES {
        let rows = 12 + rng.below(50);
        stores.insert(service, tree(&mut rng, service, rows));
    }
    fill(&mut rng, &mut stores);
    let unique_id =
        |rng: &mut SimRng| json!({"_type": "UniqueId", "value": format!("{:032x}", rng.next())});
    // Meshes under models that scale them, whose export reads those models.
    for document in stores.values_mut() {
        for instance in &mut document.instances {
            if rng.below(3) == 0 {
                (instance.properties).insert("UniqueId".to_string(), unique_id(&mut rng));
            }
            match instance.class_name.as_str() {
                "Model" if rng.below(3) > 0 => {
                    (instance.properties).insert("Scale".to_string(), json!(2.0));
                }
                "Part" if rng.below(2) == 0 => {
                    instance.class_name = "MeshPart".to_string();
                    (instance.properties).insert("Size".to_string(), json!([4.0, 2.0, 6.0]));
                }
                _ => {}
            }
        }
    }
    // A value that gives a place of the store outside a reference.
    if rng.below(4) == 0 {
        let document = stores.get_mut(SERVICES[rng.below(SERVICES.len())]).unwrap();
        let row = 1 + rng.below(document.instances.len() - 1);
        let plan = json!({"instanceIndex": 1 + rng.below(4), "step": 2});
        (document.instances[row].properties).insert("Plan".to_string(), plan);
    }
    // Now and then another writer wrote the stores, which are then merged
    // whole wherever no command wrote one of them again.
    let foreign = rng.below(6) == 0;
    let sides = [project("rows-ours"), project("rows-theirs")];
    let mut base = BTreeMap::new();
    for (service, document) in &stores {
        let mut file = in_blocks(document, block_rows).map_err(text)?;
        if foreign {
            file = foreign_file(&file);
        }
        for side in &sides {
            fs::create_dir_all(side.store(service).parent().unwrap()).unwrap();
            fs::write(side.store(service), &file).unwrap();
        }
        base.insert(*service, file);
    }
    log.push(format!("blocks of {block_rows} rows, foreign: {foreign}"));
    for (side, name) in sides.iter().zip(["ours", "theirs"]) {
        for _ in 0..rng.below(4) {
            let kind = rng.below(KINDS.len());
            if let Some(args) = command(&mut rng, side, kind, "<root>") {
                log.push(format!("{name}: {}", args.join(" ")));
                run_on(side, &args);
            }
        }
        for _ in 0..1 + rng.below(4) {
            let service = SERVICES[rng.below(SERVICES.len())];
            let id = &stores[service].instances[rng.below(6)].settings_id;
            let value = [
                "first".to_string(),
                "second".to_string(),
                "long ".repeat(80),
            ];
            let (store, value) = (side.store_arg(service), &value[rng.below(3)]);
            log.push(format!(
                "{name}: a Label of {} bytes for {id} in {service}",
                value.len()
            ));
            side.json(&["bs", "-f", &store, "-i", id, "-p", "Label", "--str", value]);
        }
        // Another UniqueId alone, which no digest of the row tells.
        let renewed = (rng.below(3) == 0).then(|| side.store(SERVICES[rng.below(SERVICES.len())]));
        if let Some(store) = renewed
            && let Ok(file) = fs::read(&store)
        {
            let mut document = decode_settings_bytecode(&file).unwrap();
            let row = rng.below(document.instances.len().min(6));
            let renewed = unique_id(&mut rng);
            (document.instances[row].properties).insert("UniqueId".to_string(), renewed);
            fs::write(
                &store,
                in_blocks(&document, block_rows_of(&file)).map_err(text)?,
            )
            .unwrap();
            log.push(format!(
                "{name}: another UniqueId for row {row} of {}",
                store.display()
            ));
        }
    }
    let prefer = [None, Some(true), Some(false)][rng.below(3)];
    let mut failures = Vec::new();
    for service in SERVICES {
        // A move of a service's last instance to another service removes its
        // store, which leaves that side no file to merge.
        let stored = |side: &Fixture| fs::read(side.store(service)).ok();
        let (Some(ours), Some(theirs)) = (stored(&sides[0]), stored(&sides[1])) else {
            continue;
        };
        let law = merged(&sides[0], [&base[service], &ours, &theirs], prefer);
        failures.extend(
            (law.err()).map(|failure| format!("{service}, preferring {prefer:?}: {failure}")),
        );
    }
    let service = SERVICES[rng.below(SERVICES.len())];
    failures.extend(repacked(&sides[1], (service, &stores[service]), block_rows).err());
    failures.extend(exported(&mut rng, &sides[0], log).err());
    failures.extend(built_into_a_base(&mut rng).err());
    match failures.is_empty() {
        true => Ok(()),
        false => Err(failures.join("; ")),
    }
}

#[test]
fn stores_read_by_their_rows_give_what_their_whole_documents_give() {
    run_seeds(SEEDS, run_seed);
    if std::env::var_os("RENIUM_SIM_SEED").is_some() {
        return;
    }
    for (name, done) in DONE_NAMES.into_iter().zip(&DONE) {
        assert!(done.load(Ordering::Relaxed) > 0, "no {name} went through");
    }
    let unread = ROWS_NOT_READ.load(Ordering::Relaxed);
    assert!(unread > 0, "every merge by rows read every instance");
}
