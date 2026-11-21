//! A seeded simulation of the views that read a store or a place file some
//! rows at a time. Each is held to what the same view gives of the whole
//! document: the text and the JSON tree of a store and of its bytes, the
//! text of a place file, the scripts a comparison reads of a place file, and
//! the full comparison of two places.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs;
use std::path::Path;

use clap::Parser;
use rbx_dom_weak::types::{Attributes, Ref, SharedString, Variant, Vector3};
use rbx_dom_weak::{InstanceBuilder, WeakDom};
use serde_json::json;

use super::place_diff;
use super::place_file::{PlaceScripts, place_dom};
use super::version_control::{
    place_text, settings_doc_to_json_tree, settings_doc_to_text, store_json_by_rows, store_text_of,
    text_by_rows,
};
use crate::app::output::JsonParts;
use crate::cli::ComparePlaceArgs;
use crate::editor::paths::build_editor_source_paths_by_index;
use crate::editor::sync::is_lua_source_class;
use crate::rbx::model::RbxPlaceFormat;
use crate::settings::bytecode::{SettingsBytecode, decode_settings_bytecode};
use crate::tests::support::{SimRng, run_seeds, settings_document, settings_instance, temp_dir};

const NAMES: [&str; 6] = ["same", "same", "a.b", "N", "x y", "\u{dc}n\u{ef}"];
const TURNS: [usize; 5] = [1, 3, 64, 1_000, 8_192];

fn told<T>(result: anyhow::Result<T>) -> Result<T, String> {
    result.map_err(|error| format!("{error:#}"))
}

/// A store's document whose rows stand, on every other draw, in another
/// order than its tree's: scripts with and without a Source or a RunContext,
/// texts the pool holds, numbers of single precision, a path inside the
/// working directory, references among properties, and attributes.
fn store_document(rng: &mut SimRng, count: usize) -> SettingsBytecode {
    let classes = ["Folder", "Part", "Script", "LocalScript", "ModuleScript"];
    let mut instances = vec![settings_instance("id:0", "Workspace", "Workspace", None)];
    for index in 1..count {
        let (name, class) = (NAMES[rng.below(6)], classes[rng.below(5)]);
        let parent = Some(rng.below(index));
        instances.push(settings_instance(
            format!("id:{index}"),
            name,
            class,
            parent,
        ));
    }
    let mut order = (0..count).collect::<Vec<_>>();
    if rng.below(2) == 0 {
        rng.shuffle(&mut order[1..]);
    }
    let mut place = vec![0; count];
    for (now, before) in order.iter().enumerate() {
        place[*before] = now;
    }
    let mut instances = (order.iter())
        .map(|before| {
            let mut instance = instances[*before].clone();
            instance.parent_index = instance.parent_index.map(|parent| place[parent]);
            instance
        })
        .collect::<Vec<_>>();
    let inside = std::env::current_dir().unwrap_or_default();
    for (index, instance) in instances.iter_mut().enumerate() {
        let single = |rng: &mut SimRng| f64::from(rng.below(900) as f32 / 7.0);
        let reference =
            |rng: &mut SimRng| json!({"_type": "Ref", "instanceIndex": rng.below(count) + 1});
        let values = &mut instance.properties;
        values.insert("Size".into(), json!([single(rng), single(rng), 2.5]));
        if rng.below(3) == 0 {
            let text = format!("{}:{}", rng.below(4), "long text ".repeat(40));
            values.insert("Mesh".into(), json!(text));
        }
        if rng.below(4) == 0 {
            values.insert("Target".into(), reference(rng));
        }
        if rng.below(6) == 0 {
            let path = format!("{}/inside\\{index}", inside.display());
            values.insert("Path".into(), json!(path));
        }
        if instance.class_name == "Script" && rng.below(2) == 0 {
            let context = ["Legacy", "Client", "Server", "Plugin"][rng.below(4)];
            values.insert("RunContext".into(), json!(context));
        }
        if is_lua_source_class(&instance.class_name) && rng.below(2) == 0 {
            values.insert("Source".into(), json!(format!("return {index}\r\n")));
        }
        if rng.below(3) == 0 {
            let tagged = &mut instance.attributes;
            tagged.insert("Tag".into(), json!(single(rng)));
            tagged.insert("Note".into(), json!(NAMES[rng.below(6)]));
        }
    }
    settings_document(instances)
}

/// The views of a store written from a drawn document, by its rows against
/// the document whole; and a store with one bit turned, which a read by rows
/// refuses wherever the whole decode does.
fn store_views(rng: &mut SimRng, root: &Path, log: &mut Vec<String>) -> Result<(), String> {
    let count = [1, 2, 40, 700, 4_097, 5_000][rng.below(6)];
    // A turn reads its blocks whole, so a large store takes few turns.
    let at_once = TURNS[rng.below(5)].max(count / 16);
    log.push(format!("store of {count} rows, {at_once} at a time"));
    let file = root.join("Workspace.renium");
    told(store_document(rng, count).write_file(&file))?;
    let whole = told(SettingsBytecode::read_file(&file))?;
    let sources = build_editor_source_paths_by_index(&whole, "Workspace", root);
    for (index, source) in sources.iter().enumerate() {
        let saved = whole.instances[index].properties.contains_key("Source");
        if let Some(source) = source.as_ref().filter(|_| !saved && rng.below(2) == 0) {
            fs::create_dir_all(source.parent().unwrap_or(root)).map_err(|e| e.to_string())?;
            fs::write(source, format!("return {index}\n")).map_err(|e| e.to_string())?;
        }
    }
    let (mut document, reader) = told(crate::bytecode::read_store(&file))?;
    let reader = reader.ok_or("the store was read whole")?;
    let held = document.clone();
    let mut text = String::new();
    told(text_by_rows(&mut document, &reader, at_once, |piece| {
        text.push_str(&piece)
    }))?;
    if text != settings_doc_to_text(&whole) {
        return Err("the text by rows is not the text of the whole store".into());
    }
    for pretty in [false, true] {
        let text_mode = rng.below(2) == 0;
        let parts = JsonParts { text_mode, pretty };
        let mut tree = String::new();
        told(store_json_by_rows(
            &mut document,
            &reader,
            &file,
            &parts,
            at_once,
            |piece| tree.push_str(&piece),
        ))?;
        let expected = told(parts.text(settings_doc_to_json_tree(&whole, &file)))? + "\n";
        if tree != expected {
            return Err(format!(
                "the tree by rows is not the tree of the whole store (text mode {text_mode}, pretty {pretty})"
            ));
        }
    }
    if document != held {
        return Err("the document holds other values after its views".into());
    }
    let mut bytes = fs::read(&file).map_err(|e| e.to_string())?;
    let mut saved = String::new();
    told(store_text_of(&bytes, |piece| saved.push_str(&piece)))?;
    if saved != settings_doc_to_text(&whole) {
        return Err("the text of a store's bytes is not the text of the whole store".into());
    }
    let at = rng.below(bytes.len());
    bytes[at] ^= 1 << rng.below(8);
    let by_rows = SettingsBytecode::decode_whole_by_rows(&bytes);
    if decode_settings_bytecode(&bytes).is_err() && by_rows.is_ok() {
        return Err(format!("a store damaged at byte {at} is read by its rows"));
    }
    Ok(())
}

/// A place of a few services, one under another name and one that is no
/// service, with scripts, parts, references and attributes below them.
fn place(rng: &mut SimRng, count: usize) -> WeakDom {
    let classes = [
        "Folder",
        "Part",
        "Script",
        "LocalScript",
        "ModuleScript",
        "ObjectValue",
    ];
    let mut dom = WeakDom::new(InstanceBuilder::new("DataModel"));
    let root = dom.root_ref();
    let mut made = vec![dom.insert(root, InstanceBuilder::new("Workspace"))];
    for top in ["ReplicatedStorage", "ServerScriptService", "Folder"] {
        if rng.below(2) == 0 {
            let named = InstanceBuilder::new(top).with_name(["Shared", top][rng.below(2)]);
            made.push(dom.insert(root, named));
        }
    }
    for index in 0..count {
        let class = classes[rng.below(6)];
        let mut builder = InstanceBuilder::new(class).with_name(NAMES[rng.below(6)]);
        if class == "Part" {
            let size = Vector3::new(rng.below(90) as f32 / 7.0, 1.0, 2.5);
            builder = builder
                .with_property("Anchored", rng.below(2) == 0)
                .with_property("Size", size);
        }
        if is_lua_source_class(class) && rng.below(3) > 0 {
            builder = builder.with_property("Source", format!("return {index}\r\n"));
        }
        if class == "ObjectValue" {
            let target = [Ref::none(), made[rng.below(made.len())]][rng.below(2)];
            builder = builder.with_property("Value", Variant::Ref(target));
        }
        if rng.below(4) == 0 {
            let tag = Variant::Float64(rng.below(9) as f64);
            builder = builder.with_property("Attributes", Attributes::new().with("Tag", tag));
        }
        made.push(dom.insert(made[rng.below(made.len())], builder));
    }
    dom
}

/// The text of a drawn place by the rows of its DOM against the text of its
/// whole document, and the scripts read of its file alone against those of
/// the file's DOM.
fn place_views(rng: &mut SimRng, root: &Path, log: &mut Vec<String>) -> Result<(), String> {
    let count = [0, 5, 60, 2_500][rng.below(4)];
    let at_once = TURNS[rng.below(5)];
    log.push(format!("place of {count} instances, {at_once} at a time"));
    let dom = place(rng, count);
    let whole = told(place_diff::document(&dom, "place", None, false))?;
    let rows = told(place_diff::dom_rows(&dom, "place", None, false))?;
    let mut text = String::new();
    told(text_by_rows(
        &mut rows.structure(),
        &rows,
        at_once,
        |piece| text.push_str(&piece),
    ))?;
    if text != settings_doc_to_text(&whole) {
        return Err("the text by rows is not the text of the whole place".into());
    }
    let file = root.join("place.rbxl");
    told(RbxPlaceFormat::Binary.write(&file, &dom, dom.root().children()))?;
    // A read gives an instance the file holds no UniqueId for one of its own.
    let without_unique_ids = |text: &str| {
        (text.lines())
            .filter(|line| !line.starts_with("  UniqueId = "))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let mut sparse = String::new();
    told(place_text(&file, at_once, |piece| sparse.push_str(&piece)))?;
    let read = told(place_diff::document(
        &told(place_dom(&file))?,
        "place",
        None,
        false,
    ))?;
    if without_unique_ids(&sparse) != without_unique_ids(&settings_doc_to_text(&read)) {
        return Err("the text of a place file by its rows is not the text of its DOM".into());
    }
    let mut alone = told(PlaceScripts::of_file(&file))?;
    let mut of_dom = PlaceScripts::of_dom(&told(place_dom(&file))?);
    alone.scripts.sort();
    of_dom.scripts.sort();
    if alone.services != of_dom.services || alone.scripts != of_dom.scripts {
        return Err("the scripts read alone are not the scripts of the whole place".into());
    }
    Ok(())
}

/// Two reads of a drawn place file, the second edited: a value, an attribute
/// or a reference changed, an instance added, the values of twins swapped.
/// Where the comparison by rows answers, it answers as the comparison of the
/// whole documents, and it answers for an unchanged place.
fn place_comparisons(rng: &mut SimRng, root: &Path, log: &mut Vec<String>) -> Result<(), String> {
    let file = root.join("compared.rbxl");
    let count = [0, 5, 60, 600][rng.below(4)];
    let dom = place(rng, count);
    told(RbxPlaceFormat::Binary.write(&file, &dom, dom.root().children()))?;
    let read = || told(RbxPlaceFormat::Binary.read_for_comparison(&file));
    let (before, mut after) = (read()?, read()?);
    let instances = (after.descendants().skip(1))
        .map(|node| node.referent())
        .collect::<Vec<_>>();
    let edits = [0, 1, 1, 3][rng.below(4)] * usize::from(!instances.is_empty());
    for _ in 0..edits {
        let id = instances[rng.below(instances.len())];
        let target = instances[rng.below(instances.len())];
        let edit = rng.below(5);
        log.push(format!("edit {edit}"));
        let node = after.get_by_ref_mut(id).ok_or("no instance")?;
        match edit {
            0 => {
                let size = Vector3::new(rng.below(3) as f32, 1.0, 2.5);
                node.properties.insert("Size".into(), size.into());
            }
            1 => {
                let tag = Variant::Float64(rng.below(3) as f64);
                let attributes = Attributes::new().with("Tag", tag);
                node.properties
                    .insert("Attributes".into(), attributes.into());
            }
            2 => {
                node.properties.insert("Value".into(), Variant::Ref(target));
            }
            3 => {
                after.insert(id, InstanceBuilder::new("Folder").with_name("same"));
            }
            _ => {
                let (name, class, parent) = (node.name.clone(), node.class, node.parent());
                let siblings = after
                    .get_by_ref(parent)
                    .map(|parent| parent.children().to_vec());
                let twin = siblings.unwrap_or_default().into_iter().find(|sibling| {
                    let sibling = after.get_by_ref(*sibling).unwrap();
                    sibling.referent() != id && sibling.name == name && sibling.class == class
                });
                if let Some(twin) = twin {
                    let mut take =
                        |id| std::mem::take(&mut after.get_by_ref_mut(id).unwrap().properties);
                    let (mine, theirs) = (take(id), take(twin));
                    after.get_by_ref_mut(id).unwrap().properties = theirs;
                    after.get_by_ref_mut(twin).unwrap().properties = mine;
                }
            }
        }
    }
    let extra = [
        &[][..],
        &["--values"],
        &["--all"],
        &["--class", "Part"],
        &["--props", "Size"],
        &["--limit", "1"],
    ][rng.below(6)];
    let words = ["cmp", "a.rbxl", "b.rbxl", "--full"].iter().chain(extra);
    let mut args = ComparePlaceArgs::try_parse_from(words).map_err(|e| e.to_string())?;
    if rng.below(2) == 0 {
        args.against = args.other.take();
    }
    let services = ([&before, &after].into_iter())
        .flat_map(|dom| {
            dom.root()
                .children()
                .iter()
                .map(|id| &dom.get_by_ref(*id).unwrap().name)
        })
        .cloned()
        .collect::<BTreeSet<_>>();
    let by_rows = told(place_diff::compare_by_rows(
        &before, &after, &services, &args,
    ))?;
    let whole = told(place_diff::compare_whole(&before, &after, &services, &args));
    log.push(format!("{extra:?}, by rows: {}", by_rows.is_some()));
    // Each place read when asked for answers as the two at hand, whichever
    // way it takes, and an unchanged place is read once on each side.
    let second = if edits == 0 {
        file.clone()
    } else {
        let edited = root.join("edited.rbxl");
        told(RbxPlaceFormat::Binary.write(&edited, &after, after.root().children()))?;
        edited
    };
    let reads = std::cell::Cell::new(0);
    let turn = |file: &Path| {
        reads.set(reads.get() + 1);
        RbxPlaceFormat::Binary.read_for_comparison(file)
    };
    let scope = (rng.below(2) == 0).then_some(&services);
    let in_turn = place_diff::compare(&mut || turn(&file), &mut || turn(&second), scope, &args);
    if edits == 0 && reads.get() != 2 {
        return Err("an unchanged place is read more than once on a side".into());
    }
    let at_hand = place_diff::compare_whole(&read()?, &told(turn(&second))?, &services, &args);
    let text = |answer: anyhow::Result<serde_json::Value>| {
        answer
            .map(|value| value.to_string())
            .map_err(|error| format!("{error:#}"))
    };
    if text(in_turn) != text(at_hand) {
        return Err("places read in turn answer otherwise than the two at hand".into());
    }
    match by_rows {
        Some(result) if whole.as_ref().ok() != Some(&result) => {
            Err("the comparison by rows is not the comparison of the whole documents".into())
        }
        None if edits == 0 => Err("an unchanged place is not compared by its rows".into()),
        _ => Ok(()),
    }
}

const MANAGED_CLASSES: [&str; 5] = [
    "MeshPart",
    "Part",
    "WeldConstraint",
    "AnimationRigData",
    "Folder",
];

fn managed_names(class: &str) -> &'static [&'static str] {
    match class {
        "MeshPart" => &[
            "Size",
            "InitialSize",
            "MeshSize",
            "PhysicalConfigData",
            "UnscaledVolume",
        ],
        "Part" => &["Size", "Anchored", "CollisionGroupId"],
        "WeldConstraint" => &["State", "Part0Internal", "Part1Internal"],
        "AnimationRigData" => &["articulatedJoint", "rootMotion"],
        _ => &[],
    }
}

/// A value of a property `managed_place` draws: payloads that are a hull's
/// placeholder, empty, a few bytes or long, sizes, numbers and references.
fn managed_value(rng: &mut SimRng, name: &str, made: &[Ref]) -> Variant {
    let bytes = |rng: &mut SimRng| match rng.below(4) {
        0 => b"CSGPHS\x00BLOCK".to_vec(),
        1 => Vec::new(),
        2 => vec![0xff, rng.below(2) as u8],
        _ => format!("hull {} ", rng.below(2)).repeat(60).into_bytes(),
    };
    match name {
        "Size" | "InitialSize" | "MeshSize" => Vector3::new(rng.below(3) as f32, 1.0, 2.5).into(),
        "PhysicalConfigData" => SharedString::new(bytes(rng)).into(),
        "UnscaledVolume" => Variant::Float32(rng.below(3) as f32),
        "Anchored" => Variant::Bool(rng.below(2) == 0),
        "CollisionGroupId" | "State" => Variant::Int32(rng.below(3) as i32),
        "Part0Internal" | "Part1Internal" => Variant::Ref(made[rng.below(made.len())]),
        _ if rng.below(3) == 0 => Variant::String(String::new()),
        _ => Variant::BinaryString(bytes(rng).into()),
    }
}

/// A place whose instances hold, beside their own values, values the engine
/// manages: cached sizes and collision data, a hull's placeholder, rig
/// tables, welds that name parts. On a draw one instance spells a property
/// twice, with its value or another.
fn managed_place(seed: u64, count: usize) -> WeakDom {
    let mut rng = SimRng(seed);
    let mut dom = WeakDom::new(InstanceBuilder::new("DataModel"));
    let root = dom.root_ref();
    let mut made = vec![dom.insert(root, InstanceBuilder::new("Workspace"))];
    let twice = rng.below(count * 3 + 1);
    for index in 0..count {
        let class = MANAGED_CLASSES[rng.below(MANAGED_CLASSES.len())];
        let mut builder = InstanceBuilder::new(class).with_name(NAMES[rng.below(6)]);
        for name in managed_names(class) {
            if rng.below(3) > 0 {
                builder = builder.with_property(*name, managed_value(&mut rng, name, &made));
            }
        }
        if index == twice
            && let Some(name) = rng.pick(managed_names(class))
        {
            let value = managed_value(&mut rng, name, &made);
            builder = builder.with_property(name.to_lowercase(), value);
        }
        made.push(dom.insert(made[rng.below(made.len())], builder));
    }
    dom
}

/// Edits a place of `managed_place`: a value drawn anew, an instance
/// renamed, added, removed or moved, the values of twins swapped. A
/// reference to a removed instance is cleared, as a save clears it.
fn edit_managed_place(rng: &mut SimRng, dom: &mut WeakDom, log: &mut Vec<String>) {
    let ids = (dom.descendants().skip(1))
        .map(|node| node.referent())
        .collect::<Vec<_>>();
    for _ in 0..[0, 1, 3, 6][rng.below(4)] * usize::from(ids.len() > 1) {
        let (id, target) = (ids[rng.below(ids.len())], ids[rng.below(ids.len())]);
        let Some(node) = dom.get_by_ref(id) else {
            continue;
        };
        let (name, class, parent) = (node.name.clone(), node.class.to_string(), node.parent());
        let edit = rng.below(6);
        log.push(format!("edit {edit}"));
        match edit {
            0 => {
                if let Some(name) = rng.pick(managed_names(&class)) {
                    let value = managed_value(rng, name, &ids);
                    let node = dom.get_by_ref_mut(id).unwrap();
                    node.properties.insert(name.into(), value);
                }
            }
            1 => dom.get_by_ref_mut(id).unwrap().name = NAMES[rng.below(6)].to_string(),
            2 => {
                let size = Vector3::new(rng.below(3) as f32, 1.0, 2.5);
                let part = InstanceBuilder::new("Part").with_name(NAMES[rng.below(6)]);
                dom.insert(id, part.with_property("Size", size));
            }
            3 if parent != dom.root_ref() => dom.destroy(id),
            4 if dom.get_by_ref(target).is_some() && parent != dom.root_ref() => {
                let mut above = target;
                while above.is_some() && above != id {
                    above = dom.get_by_ref(above).unwrap().parent();
                }
                if above != id {
                    dom.transfer_within(id, target);
                }
            }
            5 => {
                let twin = (dom.get_by_ref(parent).unwrap().children().iter())
                    .copied()
                    .find(|sibling| {
                        let sibling = dom.get_by_ref(*sibling).unwrap();
                        sibling.referent() != id
                            && sibling.name == name
                            && sibling.class.as_str() == class
                    });
                if let Some(twin) = twin {
                    let mut take =
                        |id| std::mem::take(&mut dom.get_by_ref_mut(id).unwrap().properties);
                    let (mine, theirs) = (take(id), take(twin));
                    dom.get_by_ref_mut(id).unwrap().properties = theirs;
                    dom.get_by_ref_mut(twin).unwrap().properties = mine;
                }
            }
            _ => {}
        }
    }
    let standing = (dom.descendants())
        .map(|node| node.referent())
        .collect::<HashSet<_>>();
    for id in &standing {
        let node = dom.get_by_ref_mut(*id).unwrap();
        for value in node.properties.values_mut() {
            if let Variant::Ref(target) = value
                && target.is_some()
                && !standing.contains(target)
            {
                *value = Variant::Ref(Ref::none());
            }
        }
    }
}

/// A drawn place against itself edited, compared as the comparison did
/// before it left out the values the engine manages as it read them: by two
/// whole documents that held every value and then gave up those. Lent or
/// given whole, by rows or by documents, the DOMs give that answer byte for
/// byte, or its error.
fn managed_comparisons(rng: &mut SimRng, log: &mut Vec<String>) -> Result<(), String> {
    let count = [0, 4, 60, 300][rng.below(4)];
    let (seed, edits) = (rng.next(), rng.next());
    let after = |log: &mut Vec<String>| {
        let mut dom = managed_place(seed, count);
        edit_managed_place(&mut SimRng(edits), &mut dom, log);
        dom
    };
    let (before, lent) = (managed_place(seed, count), after(log));
    let extra = [
        &[][..],
        &["--props", "PhysicalConfigData"],
        &["--props", "Size,MeshSize,State,rootMotion"],
        &["--all", "--values"],
        &["--class", "MeshPart"],
    ][rng.below(5)];
    let words = ["cmp", "a.rbxl", "b.rbxl", "--full"].iter().chain(extra);
    let mut args = ComparePlaceArgs::try_parse_from(words).map_err(|e| e.to_string())?;
    if rng.below(2) == 0 {
        args.against = args.other.take();
    }
    let services = BTreeSet::from(["Workspace".to_string()]);
    let answer = |result: anyhow::Result<serde_json::Value>| {
        result
            .map(|value| value.to_string())
            .map_err(|error| format!("{error:#}"))
    };
    let documents_whole = || -> anyhow::Result<serde_json::Value> {
        let started = std::time::Instant::now();
        let mut documents = [
            place_diff::document(&before, "before", Some(&services), true)?,
            place_diff::document(&lent, "after", Some(&services), true)?,
        ];
        let mut engine_managed = BTreeMap::new();
        for document in &mut documents {
            let omitted = place_diff::omit_engine_managed_properties(document, &args.props);
            for (name, count) in omitted {
                *engine_managed.entry(name).or_insert(0) += count;
            }
        }
        place_diff::compare_documents(documents, engine_managed, started, &services, &args)
    };
    let expected = answer(documents_whole());
    let by_rows = place_diff::compare_by_rows(&before, &lent, &services, &args);
    log.push(format!(
        "{count} instances, {extra:?}, by rows: {}, {}",
        matches!(by_rows, Ok(Some(_))),
        if expected.is_ok() {
            "answered"
        } else {
            "refused"
        }
    ));
    if answer(place_diff::compare_whole(&before, &lent, &services, &args)) != expected {
        return Err("the whole documents of lent DOMs answer otherwise".into());
    }
    if let Ok(Some(result)) = by_rows
        && Ok(result.to_string()) != expected
    {
        return Err("the comparison by rows answers otherwise".into());
    }
    let given = place_diff::compare(
        &mut || Ok(managed_place(seed, count)),
        &mut || Ok(after(&mut Vec::new())),
        Some(&services),
        &args,
    );
    if answer(given) != expected {
        return Err("DOMs given in turn answer otherwise".into());
    }
    Ok(())
}

#[test]
fn a_comparison_answers_as_documents_holding_every_value_did() {
    run_seeds(48, |seed, log| managed_comparisons(&mut SimRng(seed), log));
}

#[test]
fn views_by_rows_print_what_the_whole_document_prints() {
    run_seeds(24, |seed, log| {
        let mut rng = SimRng(seed);
        let root = temp_dir("view-sim");
        let _cleanup = crate::system::files::OnDrop::new(|| {
            let _ = fs::remove_dir_all(&root);
        });
        store_views(&mut rng, &root, log)?;
        place_views(&mut rng, &root, log)?;
        place_comparisons(&mut rng, &root, log)
    });
}
