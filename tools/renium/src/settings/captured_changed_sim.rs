use super::*;
use crate::settings::equivalence::{
    SettingsAlignment, align_pulled_settings_documents_to_reference,
};

/// A service's store with cameras, nested instances, same-named siblings,
/// references, nested enum items and ids of the kinds a capture gives:
/// Studio's debug ids, bare numbers and ids a store gave. `unique` says how
/// the instances hold a UniqueId: all, none, most, or some of them the same.
fn store(rng: &mut SimRng, ids: usize, unique: usize, workspace: bool) -> SettingsBytecode {
    let count = 6 + rng.below(18);
    let service = if workspace {
        "Workspace"
    } else {
        "ReplicatedStorage"
    };
    let mut root = SettingsBytecodeInstance::new("1".into(), service.into(), service.into(), None);
    if workspace && rng.below(8) != 0 {
        root.properties.insert(
            "CurrentCamera".into(),
            json!({"_type": "Ref", "instanceIndex": 2}),
        );
    }
    let mut instances = vec![root];
    for index in 1..count {
        let id = match if ids == 3 { rng.below(3) } else { ids } {
            0 => format!("debug:0_{}", 100 + index),
            1 => format!("editor:{index}"),
            _ => format!("{:x}", 0xa00 + index),
        };
        let parent = if index < 3 || rng.below(3) == 0 {
            0
        } else {
            2 + rng.below(index - 2)
        };
        let (name, class) = match (index, rng.below(7)) {
            (1, _) => ("Camera".to_string(), "Camera"),
            (_, 0) => ("duplicate".to_string(), "Part"),
            (_, 1) => (format!("child-{index}"), "Folder"),
            (_, 2) if index > 3 => ("Spare".to_string(), "Camera"),
            (_, 3) => (format!("label-{index}"), "TextLabel"),
            _ => (format!("child-{index}"), "Part"),
        };
        let mut row = SettingsBytecodeInstance::new(id, name, class.into(), Some(parent));
        let serial = match unique {
            0 => Some(index + 1),
            1 => None,
            2 => (rng.below(4) != 0).then_some(index + 1),
            _ => Some(if rng.below(4) == 0 { 2 } else { index + 1 }),
        };
        if let Some(serial) = serial {
            row.properties.insert(
                "UniqueId".into(),
                json!({"_type": "UniqueId", "value": format!("{serial:032x}")}),
            );
        }
        if class == "Camera" {
            row.properties.insert("FieldOfView".into(), json!(70.0));
        } else if class == "TextLabel" {
            text_fit(rng, &mut row);
        } else {
            row.properties
                .insert("Transparency".into(), json!(rng.below(8) as f64 / 8.0));
            row.properties
                .insert("Anchored".into(), json!(rng.below(2) == 0));
            row.properties.insert(
                "Color".into(),
                json!({"_type": "Color3", "r": 0.5, "g": rng.below(4) as f64 / 4.0, "b": 1.0}),
            );
        }
        if rng.below(4) == 0 {
            row.properties
                .insert("Target".into(), reference(rng, service, count));
        }
        // A part's size, at times under the name it once went by.
        if class == "Part" && rng.below(5) == 0 {
            row.properties.insert(
                if rng.below(2) == 0 { "size" } else { "Size" }.into(),
                size(1 + rng.below(4)),
            );
        }
        if rng.below(4) == 0 {
            row.properties
                .insert("Items".into(), enum_items(rng.below(2) == 0));
        }
        if rng.below(3) == 0 {
            row.attributes.insert("Level".into(), json!(rng.below(9)));
        }
        if rng.below(60) == 0 {
            row.attributes.insert("Look".into(), wrapped(rng));
        }
        if rng.below(8) == 0 {
            row.properties.insert(
                "MaxForce".into(),
                json!({"_type": "Float", "value": (["inf", "-inf", "nan"][rng.below(3)])}),
            );
        }
        instances.push(row);
    }
    SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances,
    }
}

fn size(x: usize) -> Value {
    json!({"_type": "Vector3", "x": x as f64, "y": 1.0, "z": 2.0})
}

/// A reference as a capture gives it: mostly by position, at times by an id
/// or a path, into this store or into another service.
fn reference(rng: &mut SimRng, service: &str, count: usize) -> Value {
    let target = 1 + rng.below(count);
    match rng.below(80) {
        0 => json!({"_type": "Ref", "settingsId": format!("editor:{target}")}),
        1 => json!({"_type": "Ref", "referent": format!("editor:{target}")}),
        2 => json!({
            "_type": "Ref",
            "pathSegments": [service, format!("child-{target}")],
            "pathOrdinals": [1, 1],
        }),
        3 | 4 => {
            json!({"_type": "Ref", "pathSegments": ["Lighting", "Sun"], "pathOrdinals": [1, 1]})
        }
        5 => json!({
            "_type": "Ref",
            "pathSegments": ["Lighting", "Sun"],
            "pathOrdinals": [1, 1],
            "debugId": "0_9",
            "settingsId": "debug:0_9",
        }),
        6 => json!({
            "_type": "Ref",
            "pathSegments": [service, format!("child-{target}")],
            "pathOrdinals": [1, 1],
            "debugId": format!("0_{}", 100 + target),
            "settingsId": format!("debug:0_{}", 100 + target),
        }),
        _ => json!({"_type": "Ref", "instanceIndex": target}),
    }
}

/// An attribute in one of the forms Studio's plugin hands over without a
/// type, and one with it.
fn wrapped(rng: &mut SimRng) -> Value {
    match rng.below(6) {
        5 => json!({"Font": {
            "family": "rbxasset://fonts/families/Arial.json",
            "weight": 400,
            "style": "Normal",
        }}),
        0 => json!({"BrickColor": 21}),
        1 => json!({"NumberSequence": {"keypoints": [
            {"time": 0, "value": 1, "envelope": 0},
            {"time": 1, "value": 1, "envelope": 0},
        ]}}),
        2 => json!({
            "family": "rbxasset://fonts/families/Arial.json",
            "weight": 400,
            "style": "Normal",
        }),
        3 => json!({"ColorSequence": {"keypoints": [
            {"time": 0, "color": [1, 0, 0]},
            {"time": 1, "color": [0, 0, 1]},
        ]}}),
        _ => json!({"_type": "BrickColor", "number": 21}),
    }
}

/// A label's two text properties, of which Studio forces one by the other.
fn text_fit(rng: &mut SimRng, row: &mut SettingsBytecodeInstance) {
    let scaled = rng.below(2) == 0;
    row.properties.insert("TextScaled".into(), json!(scaled));
    // Studio turns the wrapping on with the scaling; a capture seldom holds
    // the one without the other.
    match rng.below(if scaled { 12 } else { 3 }) {
        0 => {
            row.properties.remove("TextWrapped");
        }
        wrapped => {
            row.properties
                .insert("TextWrapped".into(), json!(wrapped != 1));
        }
    }
}

/// A value that holds an enum item inside, with its type or without.
fn enum_items(typed: bool) -> Value {
    let mut item = json!({"_type": "EnumItem", "name": "Plastic"});
    if typed {
        item["enumType"] = json!("Material");
    }
    json!({"items": [item]})
}

/// The capture of a document, with Studio's debug ids where Studio holds
/// them: beside the instance, not as an id a store gave. A `sparse` capture
/// numbers its instances by another rule than their order.
fn captured_as(document: &SettingsBytecode, native: bool, sparse: bool) -> ServiceState {
    let mut state = captured(document, native);
    if sparse {
        state.dense_index_topology = false;
        super::sparse_indices(&mut state);
    }
    state
}

fn captured(document: &SettingsBytecode, native: bool) -> ServiceState {
    let mut state = captured_state(document, native);
    let mut typed = state.native_properties_by_instance.take();
    for (index, row) in state.instances.iter_mut().enumerate() {
        // Every third of Studio's ids comes along as text, the way a place
        // read from a file carries a pulled store's.
        if index % 3 != 0
            && let Some(debug) = row
                .transported_settings_id
                .as_deref()
                .and_then(|id| id.strip_prefix("debug:"))
        {
            row.debug_id = Some(debug.to_string());
            row.transported_settings_id = None;
        }
        // A typed capture holds a force without limit as the float it is, and
        // at times the record's own name among the typed values.
        if let Some(typed) = typed.as_mut().and_then(|typed| typed.get_mut(index)) {
            if let Some(force) = row
                .properties
                .get("MaxForce")
                .and_then(crate::rbx::decode::nonfinite_float_from_json)
            {
                row.properties.remove("MaxForce");
                typed.push(NativeSettingsProperty {
                    name: "MaxForce".into(),
                    value: if index % 2 == 0 {
                        NativeSettingsValue::Float32(force as f32)
                    } else {
                        NativeSettingsValue::Float64(force)
                    },
                });
            }
            if index % 7 == 5 {
                typed.push(NativeSettingsProperty {
                    name: "Name".into(),
                    value: NativeSettingsValue::String(row.name.clone()),
                });
            }
            // Studio hands over what is no number with whatever bits it has.
            let no_number = f32::from_bits(0xffc0_0000);
            if index % 5 == 3 {
                typed.push(NativeSettingsProperty {
                    name: "Velocity".into(),
                    value: NativeSettingsValue::Vector3([no_number, 1.0, f32::NAN]),
                });
            }
            if index % 5 == 4 {
                let mut frame = [no_number; 12];
                frame[..3].copy_from_slice(&[1.0, 2.0, 3.0]);
                typed.push(NativeSettingsProperty {
                    name: "Pivot".into(),
                    value: NativeSettingsValue::CFrame(frame),
                });
            }
        }
    }
    state.native_properties_by_instance = typed;
    state
}

/// The references of a document after the instance at `at` came or went.
fn shift_references(document: &mut SettingsBytecode, at: usize, added: bool) {
    let moved = |index: usize| match (added, index.cmp(&at)) {
        (true, std::cmp::Ordering::Less) | (false, std::cmp::Ordering::Less) => Some(index),
        (true, _) => Some(index + 1),
        (false, std::cmp::Ordering::Equal) => None,
        (false, std::cmp::Ordering::Greater) => Some(index - 1),
    };
    for row in &mut document.instances {
        row.parent_index = row.parent_index.map(|parent| moved(parent).unwrap_or(0));
        for name in ["Target", "CurrentCamera"] {
            let target = row.properties.get(name).and_then(|value| {
                value
                    .get("instanceIndex")?
                    .as_u64()
                    .map(|index| index as usize - 1)
            });
            if let Some(target) = target {
                match moved(target) {
                    Some(target) => {
                        row.properties.insert(
                            name.into(),
                            json!({"_type": "Ref", "instanceIndex": target + 1}),
                        );
                    }
                    None => {
                        row.properties.remove(name);
                    }
                }
            }
        }
    }
}

/// A new Studio session numbers its debug ids anew, at times with numbers
/// the session before gave to other instances, and gives every instance a
/// new UniqueId when it opens the place from its file.
fn new_session(rng: &mut SimRng, document: &mut SettingsBytecode) {
    let offset = if rng.below(2) == 0 {
        1 + rng.below(3)
    } else {
        1_000 + rng.below(1_000)
    };
    let session = (rng.below(2) == 0).then(|| 1 + rng.below(1 << 20));
    for row in &mut document.instances {
        if let Some(number) = row
            .settings_id
            .strip_prefix("debug:0_")
            .and_then(|number| number.parse::<usize>().ok())
        {
            row.settings_id = format!("debug:0_{}", number + offset);
        }
        if let (Some(session), Some(serial)) = (
            session,
            row.properties
                .get("UniqueId")
                .and_then(|unique| unique["value"].as_str())
                .map(|serial| serial[24..].to_string()),
        ) {
            row.properties.insert(
                "UniqueId".into(),
                json!({"_type": "UniqueId", "value": format!("{session:024x}{serial}")}),
            );
        }
    }
}

/// One change a Studio session makes between two pulls. Most leave every
/// instance where it stands; the others a pull has to align whole.
fn edit(rng: &mut SimRng, document: &mut SettingsBytecode) {
    let count = document.instances.len();
    let at = 2 + rng.below(count - 2);
    let other = 2 + rng.below(count - 2);
    let shared = document.instances[other]
        .properties
        .get("UniqueId")
        .cloned();
    let service = document.instances[0].name.clone();
    let row = &mut document.instances[at];
    match rng.below(60) {
        0..=6 => {
            row.properties
                .insert("Transparency".into(), json!(rng.below(64) as f64 / 64.0));
        }
        7 => {
            let value = row
                .properties
                .get("Transparency")
                .and_then(Value::as_f64)
                .unwrap_or(0.5);
            row.properties
                .insert("Transparency".into(), json!(value + 1e-9));
        }
        8..=10 => {
            row.properties
                .insert("Anchored".into(), json!(rng.below(2) == 0));
        }
        11..=13 => {
            row.attributes.insert("Level".into(), json!(rng.below(9)));
        }
        14 => {
            row.attributes.remove("Level");
        }
        15 => {
            row.properties.remove("Transparency");
        }
        16..=19 => {
            document.instances[1]
                .properties
                .insert("FieldOfView".into(), json!(40.0 + rng.below(50) as f64));
        }
        20 => {
            row.properties.insert(
                "Color".into(),
                json!({"_type": "Color3", "r": 0.25, "g": rng.below(4) as f64 / 4.0, "b": 1.0}),
            );
        }
        21 => row.name.push('x'),
        22 | 23 => {
            row.properties
                .insert("Target".into(), reference(rng, &service, count));
        }
        24 | 25 => {
            document.instances[0]
                .properties
                .insert("Gravity".into(), json!(rng.below(200)));
        }
        26 => {
            row.properties.insert(
                "UniqueId".into(),
                json!({"_type": "UniqueId", "value": format!("{:032x}", 0x5000 + rng.below(64))}),
            );
        }
        27 => match shared {
            Some(shared) => {
                row.properties.insert("UniqueId".into(), shared);
            }
            None => {
                row.properties.remove("UniqueId");
            }
        },
        28..=30 => {
            row.properties
                .insert("Items".into(), enum_items(rng.below(2) == 0));
        }
        31 => {
            row.attributes.insert(
                "Kind".into(),
                enum_items(rng.below(2) == 0)["items"][0].clone(),
            );
        }
        32 | 54 | 55 => new_session(rng, document),
        33..=35 => {
            let at = 2 + rng.below(count - 1);
            shift_references(document, at, true);
            let mut added = SettingsBytecodeInstance::new(
                match rng.below(3) {
                    0 => format!("debug:0_{}", 5_000 + rng.below(1 << 20)),
                    1 => format!("editor:new{}", rng.below(1 << 20)),
                    _ => format!("{:x}", 0xb000 + rng.below(1 << 20)),
                },
                "Added".into(),
                "Folder".into(),
                Some(0),
            );
            if rng.below(3) != 0 {
                added.properties.insert(
                    "UniqueId".into(),
                    json!({"_type": "UniqueId", "value": format!("{:032x}", 0x9000 + rng.below(1 << 20))}),
                );
            }
            document.instances.insert(at, added);
        }
        36 | 37 => {
            // Only an instance nothing stands under goes.
            if count > 6
                && !document
                    .instances
                    .iter()
                    .any(|row| row.parent_index == Some(at))
            {
                document.instances.remove(at);
                shift_references(document, at, false);
            }
        }
        // Two same-named siblings trade what they hold.
        38 | 39 => {
            let twin = (2..count).find(|&twin| {
                twin != at && {
                    let (row, twin) = (&document.instances[at], &document.instances[twin]);
                    (row.parent_index, &row.name, &row.class_name)
                        == (twin.parent_index, &twin.name, &twin.class_name)
                }
            });
            if let Some(twin) = twin {
                let (low, high) = document.instances.split_at_mut(at.max(twin));
                let (left, right) = (&mut low[at.min(twin)], &mut high[0]);
                for name in ["Transparency", "Anchored", "Color", "Items"] {
                    let (mine, theirs) =
                        (left.properties.remove(name), right.properties.remove(name));
                    if let Some(value) = theirs {
                        left.properties.insert(name.into(), value);
                    }
                    if let Some(value) = mine {
                        right.properties.insert(name.into(), value);
                    }
                }
                std::mem::swap(&mut left.attributes, &mut right.attributes);
            }
        }
        // Two instances trade their UniqueIds.
        40 | 41 => {
            if at != other {
                let (low, high) = document.instances.split_at_mut(at.max(other));
                let (left, right) = (&mut low[at.min(other)], &mut high[0]);
                let (mine, theirs) = (
                    left.properties.remove("UniqueId"),
                    right.properties.remove("UniqueId"),
                );
                if let Some(value) = theirs {
                    left.properties.insert("UniqueId".into(), value);
                }
                if let Some(value) = mine {
                    right.properties.insert("UniqueId".into(), value);
                }
            }
        }
        42 | 43 => {
            document.instances[1]
                .properties
                .insert("Items".into(), enum_items(rng.below(2) == 0));
        }
        44..=46 => {
            if row.class_name == "TextLabel" {
                text_fit(rng, row);
            } else if rng.below(6) == 0 {
                row.attributes.insert("Look".into(), wrapped(rng));
            } else {
                row.attributes.insert("Level".into(), json!(rng.below(9)));
            }
        }
        47 => {
            row.attributes
                .insert("RBX_Marker".into(), json!(rng.below(4)));
        }
        // A part's size changes, or goes by its other name.
        48 | 56 => {
            if row.class_name == "Part" {
                let held = ["size", "Size"]
                    .into_iter()
                    .find_map(|name| row.properties.remove(name));
                row.properties.insert(
                    if rng.below(2) == 0 { "size" } else { "Size" }.into(),
                    match held {
                        Some(held) if rng.below(2) == 0 => held,
                        _ => size(1 + rng.below(4)),
                    },
                );
            }
        }
        49 => {
            document.instances[1].properties.insert(
                "CameraSubject".into(),
                json!({"_type": "Ref", "instanceIndex": 1 + rng.below(count)}),
            );
        }
        50 => {
            row.attributes.remove("Look");
        }
        // The viewport goes to another camera, or to none.
        51..=53 => {
            let spare = (2..count).find(|&index| document.instances[index].class_name == "Camera");
            let root = &mut document.instances[0];
            match spare.filter(|_| rng.below(4) != 0) {
                Some(spare) if root.class_name == "Workspace" => {
                    let current = root
                        .properties
                        .get("CurrentCamera")
                        .and_then(|camera| camera.get("instanceIndex")?.as_u64());
                    let target = if current == Some(spare as u64 + 1) {
                        2
                    } else {
                        spare + 1
                    };
                    root.properties.insert(
                        "CurrentCamera".into(),
                        json!({"_type": "Ref", "instanceIndex": target}),
                    );
                }
                _ => {
                    root.properties.remove("CurrentCamera");
                }
            }
        }
        _ => {
            if row.class_name == "Camera" {
                row.properties
                    .insert("FieldOfView".into(), json!(30.0 + rng.below(60) as f64));
            } else {
                row.class_name = "Folder".into();
            }
        }
    }
}

/// Wherever a pull answers from the changed instances alone, it answers what
/// the alignment of the whole documents answers, to the byte, over a run of
/// pulls that each start from the file and the rows the pull before left.
#[test]
fn a_pull_of_changed_values_answers_what_the_whole_alignment_answers() {
    use std::sync::atomic::AtomicUsize;
    let (kept, written, whole) = (
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
    );
    let by_instance = AtomicUsize::new(0);
    let (renumbered, renumbered_from_memory) = (AtomicUsize::new(0), AtomicUsize::new(0));
    let (renamed, renamed_and_changed) = (AtomicUsize::new(0), AtomicUsize::new(0));
    let (grown, shrunk) = (AtomicUsize::new(0), AtomicUsize::new(0));
    run_seeds(1_200, |seed, steps| {
        let mut rng = SimRng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
        let native = seed % 2 == 0;
        let sparse = seed % 11 == 0;
        let mut document = store(
            &mut rng,
            (seed % 4) as usize,
            (seed / 4 % 4) as usize,
            seed % 5 != 0,
        );
        let state = captured_as(&document, native, sparse);
        let (Ok(mut file), Ok(first)) = (
            encode_service_settings_binary(&state),
            pulled_digest_from_service_state(&state),
        ) else {
            return Ok(());
        };
        let mut rows = Some(first.rows);
        for round in 0..6 {
            // Every few pulls Studio opened the place again before whatever
            // else changed in it.
            if rng.below(5) == 0 {
                new_session(&mut rng, &mut document);
            }
            for _ in 0..1 + rng.below(3) {
                edit(&mut rng, &mut document);
            }
            let state = captured_as(&document, native, sparse);
            let (Ok(now), Ok(observed), Ok(reference)) = (
                pulled_digest_from_service_state(&state),
                settings_bytecode_from_service_state(&state),
                decode_settings_bytecode(&file),
            ) else {
                return Ok(());
            };
            let answered = match &rows {
                Some(before) => pull_changed_values(&state, before, &now.rows, &file)
                    .map_err(|error| format!("round {round}: {error:#}"))?,
                None => None,
            };
            // Where a state says the two encoders write it alike, they do.
            if now.rows.encodes_as_its_document()
                && encode_service_settings_binary(&state).ok()
                    != encode_settings_bytecode(&observed).ok()
            {
                return Err(format!(
                    "round {round}: the state's encoding is not its document's"
                ));
            }
            // A file compared with no memory of it answers the same.
            let unremembered = pull_changed_document_rows(&state, &now.rows, &file)
                .map_err(|error| format!("round {round}: {error:#}"))?;
            // Studio numbered the instances anew since the file was written.
            let other_ids = !reference
                .instances
                .iter()
                .map(|instance| &instance.settings_id)
                .eq(observed
                    .instances
                    .iter()
                    .map(|instance| &instance.settings_id));
            // It also gave most of them another UniqueId.
            let other_unique_ids = reference.instances.len() == observed.instances.len()
                && reference
                    .instances
                    .iter()
                    .zip(&observed.instances)
                    .filter(|(stored, captured)| {
                        stored.properties.get("UniqueId") != captured.properties.get("UniqueId")
                    })
                    .count()
                    * 2
                    > observed.instances.len();
            if other_unique_ids && let Some((answer, _)) = &unremembered {
                renamed.fetch_add(1, Ordering::Relaxed);
                let values_changed = reference.instances.iter().zip(&observed.instances).any(
                    |(stored, captured)| {
                        let without = |row: &SettingsBytecodeInstance| {
                            let mut properties = row.properties.clone();
                            properties.remove("UniqueId");
                            (properties, row.attributes.clone())
                        };
                        without(stored) != without(captured)
                    },
                );
                if values_changed && matches!(answer, SettingsAlignment::Changed(_)) {
                    renamed_and_changed.fetch_add(1, Ordering::Relaxed);
                }
            }
            // The state holds an instance more than the file, or one less.
            if unremembered.is_some() {
                match observed.instances.len().cmp(&reference.instances.len()) {
                    std::cmp::Ordering::Greater => grown.fetch_add(1, Ordering::Relaxed),
                    std::cmp::Ordering::Less => shrunk.fetch_add(1, Ordering::Relaxed),
                    std::cmp::Ordering::Equal => 0,
                };
            }
            if other_ids {
                if unremembered.is_some() {
                    renumbered.fetch_add(1, Ordering::Relaxed);
                }
                if answered.is_some() {
                    renumbered_from_memory.fetch_add(1, Ordering::Relaxed);
                }
            }
            let aligned = align_pulled_settings_documents_to_reference(
                reference,
                observed,
                &|| encode_service_settings_binary(&state),
                std::time::Instant::now(),
            );
            match (&unremembered, &aligned) {
                (None, _) => {}
                (Some(_), Err(error)) => {
                    return Err(format!(
                        "round {round}: compared by instance where the alignment fails: {error:#}"
                    ));
                }
                (Some((answer, _)), Ok(aligned)) => {
                    let same = match (answer, &aligned.alignment) {
                        (SettingsAlignment::Equivalent, SettingsAlignment::Equivalent) => true,
                        (SettingsAlignment::Changed(left), SettingsAlignment::Changed(right)) => {
                            left == right
                        }
                        _ => false,
                    };
                    if !same {
                        return Err(format!(
                            "round {round}: compared by instance, another answer than the alignment's"
                        ));
                    }
                    by_instance.fetch_add(1, Ordering::Relaxed);
                }
            }
            let aligned = match aligned {
                Ok(aligned) => aligned,
                Err(error) if answered.is_none() => {
                    steps.push(format!("round {round}: the alignment fails: {error:#}"));
                    return Ok(());
                }
                Err(error) => {
                    return Err(format!(
                        "round {round}: answered where the alignment fails: {error:#}"
                    ));
                }
            };
            match (&answered, &aligned.alignment) {
                (None, _) => {
                    whole.fetch_add(1, Ordering::Relaxed);
                }
                (Some(SettingsAlignment::Equivalent), SettingsAlignment::Equivalent) => {
                    kept.fetch_add(1, Ordering::Relaxed);
                }
                (Some(SettingsAlignment::Changed(left)), SettingsAlignment::Changed(right))
                    if left == right =>
                {
                    written.fetch_add(1, Ordering::Relaxed);
                }
                (Some(SettingsAlignment::Changed(_)), SettingsAlignment::Changed(_)) => {
                    return Err(format!(
                        "round {round}: other bytes than the alignment writes"
                    ));
                }
                (Some(SettingsAlignment::Equivalent), SettingsAlignment::Changed(_)) => {
                    return Err(format!("round {round}: kept a file the alignment rewrites"));
                }
                (Some(SettingsAlignment::Changed(_)), SettingsAlignment::Equivalent) => {
                    return Err(format!("round {round}: rewrote a file the alignment keeps"));
                }
            }
            steps.push(format!(
                "round {round}: {} ({})",
                match aligned.alignment {
                    SettingsAlignment::Equivalent => "kept",
                    SettingsAlignment::Changed(_) => "written",
                },
                if answered.is_some() {
                    "from the changed instances"
                } else {
                    "by the whole alignment"
                }
            ));
            // What the pull remembers for the next one.
            rows = match (&aligned.alignment, answered.is_some(), &unremembered) {
                (SettingsAlignment::Equivalent, true, _) => rows,
                (_, true, _) => Some(now.rows),
                (SettingsAlignment::Equivalent, false, Some((_, same))) => same.then_some(now.rows),
                (SettingsAlignment::Changed(_), false, Some(_)) => Some(now.rows),
                (_, false, None) => aligned.stored_as_observed.then_some(now.rows),
            };
            if let SettingsAlignment::Changed(bytes) = aligned.alignment {
                file = bytes;
            }
        }
        Ok(())
    });
    if std::env::var_os("RENIUM_SIM_SEED").is_none()
        && std::env::var_os("RENIUM_SIM_SEEDS").is_none()
    {
        let (kept, written, whole) = (
            kept.load(Ordering::Relaxed),
            written.load(Ordering::Relaxed),
            whole.load(Ordering::Relaxed),
        );
        assert!(
            kept >= 100
                && written >= 400
                && whole >= 500
                && by_instance.load(Ordering::Relaxed) >= 1_000
                && renumbered.load(Ordering::Relaxed) >= 150
                && renumbered_from_memory.load(Ordering::Relaxed) >= 25
                && renamed.load(Ordering::Relaxed) >= 25
                && renamed_and_changed.load(Ordering::Relaxed) >= 10
                && grown.load(Ordering::Relaxed) >= 40
                && shrunk.load(Ordering::Relaxed) >= 10,
            "kept {kept}, written {written}, by the whole alignment {whole}, under other ids {} and {} of them from memory, under other UniqueIds {} and {} of them with other values, with an instance more {} and less {}",
            renumbered.load(Ordering::Relaxed),
            renumbered_from_memory.load(Ordering::Relaxed),
            renamed.load(Ordering::Relaxed),
            renamed_and_changed.load(Ordering::Relaxed),
            grown.load(Ordering::Relaxed),
            shrunk.load(Ordering::Relaxed),
        );
    }
}

/// Two parts of one name under one parent, in the session that numbers its
/// instances from `session`, each with what `held` gives it.
fn twins(session: usize, unique_ids: bool, held: [f64; 2]) -> SettingsBytecode {
    let root = SettingsBytecodeInstance::new(
        "1".into(),
        "ReplicatedStorage".into(),
        "ReplicatedStorage".into(),
        None,
    );
    let mut instances = vec![root];
    for (index, transparency) in held.into_iter().enumerate() {
        let mut twin = SettingsBytecodeInstance::new(
            format!("debug:0_{}", session * 100 + index),
            "Twin".into(),
            "Part".into(),
            Some(0),
        );
        twin.properties
            .insert("Transparency".into(), json!(transparency));
        if unique_ids {
            twin.properties.insert(
                "UniqueId".into(),
                json!({"_type": "UniqueId", "value": format!("{session:024x}{:08x}", index + 1)}),
            );
        }
        instances.push(twin);
    }
    SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances,
    }
}

/// The same twins, each with a part under it that holds what `held` gives.
fn twins_with_leaves(session: usize, held: [f64; 2]) -> SettingsBytecode {
    let mut document = twins(session, true, [0.5, 0.5]);
    for (index, transparency) in held.into_iter().enumerate() {
        let mut leaf = SettingsBytecodeInstance::new(
            format!("debug:0_{}", session * 100 + 10 + index),
            "Leaf".into(),
            "Part".into(),
            Some(index + 1),
        );
        leaf.properties
            .insert("Transparency".into(), json!(transparency));
        leaf.properties.insert(
            "UniqueId".into(),
            json!({"_type": "UniqueId", "value": format!("{session:024x}{:08x}", index + 11)}),
        );
        document.instances.push(leaf);
    }
    document
}

/// The ids a written store holds its instances under.
fn stored_ids(file: &[u8]) -> Vec<String> {
    decode_settings_bytecode(file)
        .unwrap()
        .instances
        .into_iter()
        .map(|instance| instance.settings_id)
        .collect()
}

/// Same-named siblings that hold each other's values in a place Studio
/// opened again are each other, as far as anything says: the alignment
/// gives each the id of the stored one it holds the values of, which a
/// comparison of the instances at their places cannot know.
#[test]
fn namesakes_that_traded_their_values_take_the_whole_alignment() {
    let file =
        encode_service_settings_binary(&captured(&twins(1, true, [0.25, 0.75]), false)).unwrap();
    let state = captured(&twins(2, true, [0.75, 0.25]), false);
    let now = pulled_digest_from_service_state(&state).unwrap();
    assert!(
        pull_changed_document_rows(&state, &now.rows, &file)
            .unwrap()
            .is_none()
    );
    let aligned = align_pulled_settings_documents_to_reference(
        decode_settings_bytecode(&file).unwrap(),
        settings_bytecode_from_service_state(&state).unwrap(),
        &|| encode_service_settings_binary(&state),
        std::time::Instant::now(),
    )
    .unwrap();
    let SettingsAlignment::Changed(written) = aligned.alignment else {
        panic!("the alignment keeps the store");
    };
    let (before, after) = (stored_ids(&file), stored_ids(&written));
    assert_eq!((&after[1], &after[2]), (&before[2], &before[1]));
    // Siblings that only took other values stand where they stood.
    let state = captured(&twins(2, true, [0.25, 0.75]), false);
    let now = pulled_digest_from_service_state(&state).unwrap();
    let (answer, same) = pull_changed_document_rows(&state, &now.rows, &file)
        .unwrap()
        .expect("instances that hold the stored values are compared at their places");
    assert!(!same);
    let SettingsAlignment::Changed(written) = answer else {
        panic!("other UniqueIds are written");
    };
    assert_eq!(stored_ids(&written), before);
}

/// What stands under same-named siblings is told apart no better than they
/// are: where two such parts hold each other's values in a place Studio
/// opened again, the alignment takes each sibling, with what stands under
/// it, for the stored one its part holds the values of.
#[test]
fn instances_under_namesakes_take_the_whole_alignment() {
    let file =
        encode_service_settings_binary(&captured(&twins_with_leaves(1, [0.25, 0.75]), false))
            .unwrap();
    let state = captured(&twins_with_leaves(2, [0.75, 0.25]), false);
    let now = pulled_digest_from_service_state(&state).unwrap();
    assert!(
        pull_changed_document_rows(&state, &now.rows, &file)
            .unwrap()
            .is_none()
    );
    let aligned = align_pulled_settings_documents_to_reference(
        decode_settings_bytecode(&file).unwrap(),
        settings_bytecode_from_service_state(&state).unwrap(),
        &|| encode_service_settings_binary(&state),
        std::time::Instant::now(),
    )
    .unwrap();
    let SettingsAlignment::Changed(written) = aligned.alignment else {
        panic!("the alignment keeps the store");
    };
    let (before, after) = (stored_ids(&file), stored_ids(&written));
    assert_eq!(
        (&after[1], &after[2], &after[3], &after[4]),
        (&before[2], &before[1], &before[4], &before[3])
    );
}

/// Under other ids than the store's, instances that nothing tells apart
/// but what they hold are the alignment's to match once one of them holds
/// other values, also where a pull remembers the store.
#[test]
fn instances_nothing_tells_apart_take_the_whole_alignment_under_other_ids() {
    let file =
        encode_service_settings_binary(&captured(&twins(1, false, [0.25, 0.75]), false)).unwrap();
    // The place opened again: nothing changed but the ids.
    let reopened = captured(&twins(2, false, [0.25, 0.75]), false);
    let before = pulled_digest_from_service_state(&reopened).unwrap();
    let (answer, same) = pull_changed_document_rows(&reopened, &before.rows, &file)
        .unwrap()
        .expect("a store that differs in its ids alone is compared by its instances");
    assert!(matches!(answer, SettingsAlignment::Equivalent) && same);
    // Then the two traded their values.
    let traded = captured(&twins(2, false, [0.75, 0.25]), false);
    let now = pulled_digest_from_service_state(&traded).unwrap();
    assert!(
        pull_changed_values(&traded, &before.rows, &now.rows, &file)
            .unwrap()
            .is_none()
    );
    assert!(
        pull_changed_document_rows(&traded, &now.rows, &file)
            .unwrap()
            .is_none()
    );
    // With a UniqueId of its own each is told by itself.
    let file =
        encode_service_settings_binary(&captured(&twins(1, true, [0.25, 0.75]), false)).unwrap();
    let mut reopened = twins(2, true, [0.25, 0.75]);
    let mut changed = twins(2, true, [0.25, 0.5]);
    for document in [&mut reopened, &mut changed] {
        for (index, twin) in document.instances.iter_mut().enumerate().skip(1) {
            twin.properties.insert(
                "UniqueId".into(),
                json!({"_type": "UniqueId", "value": format!("{:024x}{index:08x}", 1)}),
            );
        }
    }
    let reopened = captured(&reopened, false);
    let before = pulled_digest_from_service_state(&reopened).unwrap();
    let changed = captured(&changed, false);
    let now = pulled_digest_from_service_state(&changed).unwrap();
    let answer = pull_changed_values(&changed, &before.rows, &now.rows, &file)
        .unwrap()
        .expect("instances with a UniqueId of their own are compared from memory");
    let SettingsAlignment::Changed(written) = answer else {
        panic!("another value is written");
    };
    assert_eq!(stored_ids(&written), stored_ids(&file));
}

/// A state with one run of instances more than a store, or less, is told by
/// where the run starts and that every instance past it stands under the
/// stored one's name, class and parent; a state that differs otherwise is
/// none.
#[test]
fn a_run_of_instances_more_or_less_is_told_from_names_and_parents() {
    let stored = twins_with_leaves(1, [0.25, 0.75]);
    let moved = |document: &SettingsBytecode| {
        let state = captured(document, false);
        captured::Moved::between(
            &stored.instances,
            &collect_service_settings_binary_instances(&state),
        )
    };
    assert!(matches!(moved(&stored), Some(None)));
    // A folder comes between the root and the first twin.
    let mut grown = stored.clone();
    grown.instances.insert(
        1,
        SettingsBytecodeInstance::new("new".into(), "New".into(), "Folder".into(), Some(0)),
    );
    for row in grown.instances.iter_mut().skip(2) {
        row.parent_index = row
            .parent_index
            .map(|parent| parent + usize::from(parent > 0));
    }
    let run = moved(&grown).flatten().expect("one run more");
    assert_eq!(
        (0..6).map(|index| run.in_store(index)).collect::<Vec<_>>(),
        [Some(0), None, Some(1), Some(2), Some(3), Some(4)]
    );
    // The last leaf goes.
    let mut shrunk = stored.clone();
    shrunk.instances.pop();
    let run = moved(&shrunk).flatten().expect("one run less");
    assert_eq!(
        (0..4).map(|index| run.in_store(index)).collect::<Vec<_>>(),
        [Some(0), Some(1), Some(2), Some(3)]
    );
    // An instance past the run under another name, or another parent.
    let mut renamed = grown.clone();
    renamed.instances[4].name = "Other".into();
    assert!(moved(&renamed).is_none());
    let mut reparented = grown;
    reparented.instances[5].parent_index = Some(2);
    assert!(moved(&reparented).is_none());
}

/// Instances that nothing tells apart but what they hold are the alignment's
/// to match where a place Studio opened again also holds a new instance and
/// two of them traded their values: a UniqueId that is not the stored one
/// tells nothing.
#[test]
fn namesakes_beside_a_new_instance_take_the_whole_alignment() {
    let file =
        encode_service_settings_binary(&captured(&twins(1, true, [0.25, 0.75]), false)).unwrap();
    let with_new = |held: [f64; 2]| {
        let mut document = twins(2, true, held);
        document.instances.push(SettingsBytecodeInstance::new(
            "debug:0_299".into(),
            "New".into(),
            "Folder".into(),
            Some(0),
        ));
        captured(&document, false)
    };
    let traded = with_new([0.75, 0.25]);
    let now = pulled_digest_from_service_state(&traded).unwrap();
    assert!(
        pull_changed_document_rows(&traded, &now.rows, &file)
            .unwrap()
            .is_none()
    );
    // With the values they had, each is the stored one at its place.
    let grown = with_new([0.25, 0.75]);
    let now = pulled_digest_from_service_state(&grown).unwrap();
    let (answer, _) = pull_changed_document_rows(&grown, &now.rows, &file)
        .unwrap()
        .expect("a new instance beside instances that hold the stored values");
    let SettingsAlignment::Changed(written) = answer else {
        panic!("a store with a new instance is written");
    };
    let mut ids = stored_ids(&file);
    ids.push("debug:0_299".into());
    assert_eq!(stored_ids(&written), ids);
}

/// A stored viewport camera goes back into a store as the store holds it.
/// Where it points at an instance by position and the state holds another
/// run of instances than the store, that position is another instance's, or
/// none: the alignment says what becomes of it.
#[test]
fn a_stored_camera_that_points_at_an_instance_takes_the_whole_alignment_beside_a_run() {
    let workspace = |subject: bool| {
        let mut root =
            SettingsBytecodeInstance::new("1".into(), "Workspace".into(), "Workspace".into(), None);
        root.properties.insert(
            "CurrentCamera".into(),
            json!({"_type": "Ref", "instanceIndex": 2}),
        );
        let mut camera = SettingsBytecodeInstance::new(
            "debug:0_101".into(),
            "Camera".into(),
            "Camera".into(),
            Some(0),
        );
        camera.properties.insert(
            "FieldOfView".into(),
            json!(if subject { 70.0 } else { 40.0 }),
        );
        let mut instances = vec![root, camera];
        if subject {
            instances[1].properties.insert(
                "CameraSubject".into(),
                json!({"_type": "Ref", "instanceIndex": 3}),
            );
            instances.push(SettingsBytecodeInstance::new(
                "debug:0_102".into(),
                "Subject".into(),
                "Part".into(),
                Some(0),
            ));
        }
        SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances,
        }
    };
    let file = encode_service_settings_binary(&captured(&workspace(true), false)).unwrap();
    let state = captured(&workspace(false), false);
    let now = pulled_digest_from_service_state(&state).unwrap();
    assert!(
        pull_changed_document_rows(&state, &now.rows, &file)
            .unwrap()
            .is_none()
    );
}

/// A UniqueId tells an instance only where one stored instance holds it.
/// Where two do and one of them went, what is left is the stored one it
/// holds the values of, which the alignment finds by those values.
#[test]
fn a_unique_id_two_stored_instances_hold_tells_none_of_them() {
    let mut stored = twins(1, true, [0.5, 0.25]);
    let shared = stored.instances[1].properties["UniqueId"].clone();
    stored.instances[2]
        .properties
        .insert("UniqueId".into(), shared.clone());
    let file = encode_settings_bytecode(&stored).unwrap();
    // The first of them went; the one that stays holds the second's values.
    let mut left = twins(2, true, [0.25, 0.25]);
    left.instances.pop();
    left.instances[1]
        .properties
        .insert("UniqueId".into(), shared);
    let state = captured(&left, false);
    let now = pulled_digest_from_service_state(&state).unwrap();
    assert!(
        pull_changed_document_rows(&state, &now.rows, &file)
            .unwrap()
            .is_none()
    );
    let aligned = align_pulled_settings_documents_to_reference(
        decode_settings_bytecode(&file).unwrap(),
        settings_bytecode_from_service_state(&state).unwrap(),
        &|| encode_service_settings_binary(&state),
        std::time::Instant::now(),
    )
    .unwrap();
    let SettingsAlignment::Changed(written) = aligned.alignment else {
        panic!("the alignment keeps the store");
    };
    assert_eq!(stored_ids(&written)[1], stored_ids(&file)[2]);
}

/// A new instance keeps the id it came with unless the store holds an
/// instance under that id, or a new instance before it took it: Studio
/// gives out again the numbers of an earlier session.
#[test]
fn a_new_instance_under_an_id_the_store_holds_takes_another() {
    let file =
        encode_service_settings_binary(&captured(&twins(1, true, [0.25, 0.75]), false)).unwrap();
    let before = stored_ids(&file);
    let mut grown = twins(2, true, [0.25, 0.75]);
    for id in [&before[1], &before[2]] {
        grown.instances.push(SettingsBytecodeInstance::new(
            id.clone(),
            "New".into(),
            "Folder".into(),
            Some(0),
        ));
    }
    let state = captured(&grown, false);
    let now = pulled_digest_from_service_state(&state).unwrap();
    let (answer, _) = pull_changed_document_rows(&state, &now.rows, &file)
        .unwrap()
        .expect("new instances beside instances that hold the stored values");
    let SettingsAlignment::Changed(written) = answer else {
        panic!("a store with new instances is written");
    };
    assert_eq!(
        stored_ids(&written),
        [
            &before[0],
            &before[1],
            &before[2],
            "reconcile:0",
            "reconcile:1"
        ]
    );
    let aligned = align_pulled_settings_documents_to_reference(
        decode_settings_bytecode(&file).unwrap(),
        settings_bytecode_from_service_state(&state).unwrap(),
        &|| encode_service_settings_binary(&state),
        std::time::Instant::now(),
    )
    .unwrap();
    assert!(matches!(aligned.alignment, SettingsAlignment::Changed(bytes) if bytes == written));
}

/// A root is told by being the only one, unless a store holds another
/// instance under the root's id: the alignment then takes the root for that
/// one.
#[test]
fn a_root_stored_under_another_id_takes_the_whole_alignment() {
    let mut stored = twins(1, true, [0.25, 0.75]);
    stored.instances[0].settings_id = "root".into();
    stored.instances[1].settings_id = "1".into();
    let file = encode_settings_bytecode(&stored).unwrap();
    // The same UniqueIds under other ids, and the twins traded their values.
    let mut traded = twins(2, true, [0.75, 0.25]);
    for (index, twin) in traded.instances.iter_mut().enumerate().skip(1) {
        twin.properties.insert(
            "UniqueId".into(),
            stored.instances[index].properties["UniqueId"].clone(),
        );
    }
    let state = captured(&traded, false);
    let now = pulled_digest_from_service_state(&state).unwrap();
    assert!(
        pull_changed_document_rows(&state, &now.rows, &file)
            .unwrap()
            .is_none()
    );
    // Stored under the root's own id it is the root, and the twins are told
    // by their UniqueIds.
    stored.instances[0].settings_id = "1".into();
    stored.instances[1].settings_id = "debug:0_100".into();
    let file = encode_settings_bytecode(&stored).unwrap();
    let (answer, _) = pull_changed_document_rows(&state, &now.rows, &file)
        .unwrap()
        .expect("a sole root under its id and instances with their UniqueIds stand");
    let SettingsAlignment::Changed(written) = answer else {
        panic!("other values are written");
    };
    assert_eq!(stored_ids(&written), stored_ids(&file));
}

/// A property under the name it once went by is the same value under the
/// name of today, and another one as the two instances hold it.
#[test]
fn an_instance_is_compared_as_held_and_under_todays_names() {
    let part = |name: &str, x: usize| {
        let mut part =
            SettingsBytecodeInstance::new("a".into(), "Part".into(), "Part".into(), Some(0));
        part.properties.insert(name.into(), size(x));
        part
    };
    let compared = |stored: &SettingsBytecodeInstance, captured: &SettingsBytecodeInstance| {
        crate::settings::equivalence::pulled_instance_values_unchanged(stored, captured, None)
            .unwrap()
    };
    assert_eq!(
        compared(&part("Size", 4), &part("Size", 4)),
        Some((true, true))
    );
    assert_eq!(
        compared(&part("Size", 4), &part("Size", 3)),
        Some((false, false))
    );
    assert_eq!(
        compared(&part("size", 4), &part("Size", 4)),
        Some((false, true))
    );
    assert_eq!(
        compared(&part("size", 4), &part("size", 3)),
        Some((false, false))
    );
}

/// The rows of a store file read alone are the rows its whole decoding gives.
#[test]
fn rows_read_alone_are_the_rows_of_the_whole_store() {
    run_seeds(48, |seed, _| {
        let mut rng = SimRng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
        let document = store(
            &mut rng,
            (seed % 4) as usize,
            (seed / 4 % 4) as usize,
            seed % 3 != 0,
        );
        let Ok(bytes) = encode_service_settings_binary(&captured(&document, seed % 2 == 0)) else {
            return Ok(());
        };
        let whole = decode_settings_bytecode(&bytes).map_err(|error| format!("{error:#}"))?;
        let mut rows = (0..whole.instances.len())
            .filter(|_| rng.below(3) == 0)
            .collect::<Vec<_>>();
        rng.shuffle(&mut rows);
        let (alone, ids) =
            property_edit::decode_rows(&bytes, &rows).map_err(|error| format!("{error:#}"))?;
        for (row, instance) in rows.iter().zip(&alone) {
            if *instance != whole.instances[*row] {
                return Err(format!("row {row} differs"));
            }
        }
        let stored_ids = whole
            .instances
            .iter()
            .map(|instance| instance.settings_id.clone())
            .collect::<Vec<_>>();
        if ids != stored_ids {
            return Err("the ids read with the rows are not the store's".into());
        }
        if property_edit::decode_rows(&bytes, &[whole.instances.len()]).is_ok() {
            return Err("a row past the end was read".into());
        }
        // The digests taken column by column are the digests of the rows.
        let kept = (!whole.instances.is_empty()).then(|| rng.below(whole.instances.len()));
        let property_edit::StoreRows {
            digests,
            unique_ids,
            kept: alone,
            ids,
            ..
        } = property_edit::row_digests(&bytes, |_| Some((kept, None)))
            .map_err(|error| format!("{error:#}"))?
            .ok_or("no digests")?;
        if alone.as_ref() != kept.map(|kept| &whole.instances[kept]) {
            return Err("the row kept with the digests is not the store's".into());
        }
        if ids != stored_ids {
            return Err("the ids read with the digests are not the store's".into());
        }
        if digests.into_iter().zip(unique_ids).collect::<Vec<_>>()
            != whole
                .instances
                .iter()
                .map(captured::document_row_digest)
                .collect::<Vec<_>>()
        {
            return Err("the digests of the columns are not the digests of the rows".into());
        }
        Ok(())
    });
}

/// A capture numbered by another rule than its order is left to the whole
/// alignment: the stored camera's references go by the store's positions.
#[test]
fn a_state_numbered_by_another_rule_is_aligned_whole() {
    let mut rng = SimRng(7);
    let mut document = store(&mut rng, 1, 0, true);
    document.instances[0].properties.insert(
        "CurrentCamera".into(),
        json!({"_type": "Ref", "instanceIndex": 2}),
    );
    for row in &mut document.instances {
        row.properties.remove("Target");
        row.properties.remove("Items");
        row.attributes.clear();
        if row.class_name == "TextLabel" {
            row.properties.remove("TextScaled");
        }
    }
    document.instances[1].properties.insert(
        "CameraSubject".into(),
        json!({"_type": "Ref", "instanceIndex": 3}),
    );
    // Every number one past the place its instance stands at.
    let numbered = |document: &SettingsBytecode, shifted: bool| {
        let mut state = captured(document, false);
        if shifted {
            state.dense_index_topology = false;
            for row in &mut state.instances {
                row.instance_index = row.instance_index.map(|index| index + 1);
                row.parent_index = row.parent_index.map(|index| index + 1);
                visit_reference_objects_mut(&mut row.properties, |reference| {
                    if let Some(index) = reference.get("instanceIndex").and_then(Value::as_u64) {
                        reference.insert("instanceIndex".into(), json!(index + 1));
                    }
                });
            }
        }
        state
    };
    let answers = |sparse: bool| {
        let state = numbered(&document, sparse);
        let file = encode_service_settings_binary(&state).unwrap();
        let before = pulled_digest_from_service_state(&state).unwrap().rows;
        let mut changed = document.clone();
        changed.instances[1]
            .properties
            .insert("FieldOfView".into(), json!(41.0));
        let last = changed.instances.len() - 1;
        changed.instances[last]
            .properties
            .insert("Probe".into(), json!(0.375));
        let state = numbered(&changed, sparse);
        let now = pulled_digest_from_service_state(&state).unwrap().rows;
        pull_changed_values(&state, &before, &now, &file)
            .unwrap()
            .is_some()
    };
    assert!(answers(false));
    assert!(!answers(true));
}
