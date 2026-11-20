//! A seeded simulation of the views that read a store or a place file some
//! rows at a time. Each is held to what the same view gives of the whole
//! document: the text and the JSON tree of a store, the text of a place
//! file, and the scripts a comparison reads of a place file.

use std::fs;
use std::path::Path;

use rbx_dom_weak::types::{Attributes, Ref, Variant, Vector3};
use rbx_dom_weak::{InstanceBuilder, WeakDom};
use serde_json::json;

use super::place_diff;
use super::place_file::{PlaceScripts, place_dom};
use super::version_control::{
    settings_doc_to_json_tree, settings_doc_to_text, store_json_by_rows, text_by_rows,
};
use crate::app::output::JsonParts;
use crate::editor::paths::build_editor_source_paths_by_index;
use crate::editor::sync::is_lua_source_class;
use crate::rbx::model::RbxPlaceFormat;
use crate::settings::bytecode::blocks::RowReader;
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
    let at = rng.below(bytes.len());
    bytes[at] ^= 1 << rng.below(8);
    let by_rows = RowReader::decode(&bytes).and_then(|(_, reader)| reader.check());
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
    let mut alone = told(PlaceScripts::of_file(&file))?;
    let mut of_dom = PlaceScripts::of_dom(&told(place_dom(&file))?);
    alone.scripts.sort();
    of_dom.scripts.sort();
    if alone.services != of_dom.services || alone.scripts != of_dom.scripts {
        return Err("the scripts read alone are not the scripts of the whole place".into());
    }
    Ok(())
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
        place_views(&mut rng, &root, log)
    });
}
