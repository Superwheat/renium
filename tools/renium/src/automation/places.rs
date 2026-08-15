use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value, json};

use super::BoundContext;
use crate::project::workflows;
use crate::system::files::atomic_write_file;

fn string(object: &Map<String, Value>, key: &str) -> Option<String> {
    object.get(key).and_then(|value| match value {
        Value::String(value) => Some(value.clone()),
        Value::Number(value) => Some(value.to_string()),
        _ => None,
    })
}

fn normalize_alias(value: &str, place_id: i64) -> String {
    let mut output = String::new();
    let mut separator = false;
    for character in value.chars() {
        if character.is_ascii_alphanumeric() {
            if separator && !output.is_empty() {
                output.push('_');
            }
            output.push(character.to_ascii_lowercase());
            separator = false;
        } else if character.is_ascii_whitespace() || character == '_' {
            separator = !output.is_empty();
        }
    }
    if output.is_empty() {
        format!("place{place_id}")
    } else {
        output
    }
}

fn write(path: &Path, manifest: &Value) -> Result<()> {
    atomic_write_file(path, &serde_json::to_vec_pretty(manifest)?)
}

fn read(context: &BoundContext) -> Result<(PathBuf, Value)> {
    let path = Path::new(&context.experience).join("renium.experience.json");
    let manifest = serde_json::from_slice(
        &fs::read(&path).with_context(|| format!("Failed to read {}", path.display()))?,
    )?;
    Ok((path, manifest))
}

pub(super) fn add(context: &BoundContext, parameters: &Value) -> Result<Value> {
    let object = parameters.as_object().context("p must be an object")?;
    let place_id = object
        .get("placeId")
        .and_then(Value::as_i64)
        .context("place-add requires p.placeId")?;
    let game_id = object
        .get("gameId")
        .and_then(Value::as_i64)
        .or(context.game_id)
        .unwrap_or(0);
    let name = string(object, "name").context("place-add requires p.name")?;
    let requested_alias = string(object, "alias").unwrap_or_else(|| name.clone());
    let alias = normalize_alias(&requested_alias, place_id);
    let experience = Path::new(&context.experience);
    let manifest_path = experience.join("renium.experience.json");
    let mut manifest = if manifest_path.is_file() {
        serde_json::from_slice::<Value>(&fs::read(&manifest_path)?)?
    } else {
        let current_name = Path::new(&context.root)
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("main")
            .to_string();
        let current_alias = normalize_alias(&current_name, context.place_id.unwrap_or(0));
        let current_root = experience.join("places").join(&current_alias);
        fs::create_dir_all(&current_root)?;
        for entry in [
            PathBuf::from("renium.project.jsonc"),
            PathBuf::from("renium.project.json"),
            PathBuf::from(&context.source)
                .strip_prefix(&context.root)
                .unwrap_or_else(|_| Path::new("src"))
                .to_path_buf(),
            PathBuf::from(".renium"),
            PathBuf::from("sourcemap.json"),
            PathBuf::from("renium-link.json"),
            PathBuf::from("wally.toml"),
            PathBuf::from("wally.lock"),
            PathBuf::from("Packages"),
        ] {
            let source = experience.join(&entry);
            let destination = current_root.join(&entry);
            if source.exists() {
                if destination.exists() {
                    bail!(
                        "Cannot migrate {} because {} already exists",
                        source.display(),
                        destination.display()
                    );
                }
                if let Some(parent) = destination.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::rename(&source, &destination)
                    .with_context(|| format!("Failed to move {}", source.display()))?;
            }
        }
        let source_root = PathBuf::from(&context.source)
            .strip_prefix(&context.root)
            .unwrap_or_else(|_| Path::new("src"))
            .to_path_buf();
        workflows::initialize_place_root(&current_root, &source_root)?;
        json!({
            "version": 2,
            "gameId": context.game_id.unwrap_or(game_id),
            "startPlace": current_alias,
            "placeOrder": context.place_id.filter(|id| *id > 0).into_iter().collect::<Vec<_>>(),
            "places": {
                (current_alias.clone()): {
                    "placeId": context.place_id.unwrap_or(0),
                    "name": current_name,
                    "root": format!("places/{current_alias}")
                }
            }
        })
    };
    let manifest_game_id = manifest.get("gameId").and_then(Value::as_i64).unwrap_or(0);
    if manifest_game_id > 0 && game_id > 0 && manifest_game_id != game_id {
        bail!("Place gameId {game_id} does not match project gameId {manifest_game_id}");
    }
    let places = manifest
        .get_mut("places")
        .and_then(Value::as_object_mut)
        .context("Experience places must be an object")?;
    if places.contains_key(&alias)
        || places
            .values()
            .any(|place| place.get("placeId").and_then(Value::as_i64) == Some(place_id))
    {
        bail!("Place {place_id} is already configured");
    }
    let relative_root = string(object, "root").unwrap_or_else(|| format!("places/{alias}"));
    let relative_path = Path::new(&relative_root);
    if relative_path.is_absolute()
        || relative_path.components().any(|component| {
            matches!(
                component,
                std::path::Component::ParentDir
                    | std::path::Component::RootDir
                    | std::path::Component::Prefix(_)
            )
        })
    {
        bail!("Place root must be a relative path inside the experience project");
    }
    let place_root = experience.join(&relative_root);
    if !place_root.starts_with(experience) || place_root == experience {
        bail!("Place root must stay inside the experience project");
    }
    workflows::initialize_place_root(&place_root, Path::new("src"))?;
    places.insert(
        alias.clone(),
        json!({ "placeId": place_id, "name": name, "root": relative_root.clone() }),
    );
    let order = manifest
        .get_mut("placeOrder")
        .and_then(Value::as_array_mut)
        .context("Experience placeOrder must be an array")?;
    if place_id > 0 {
        order.push(json!(place_id));
    }
    manifest["version"] = json!(2);
    if manifest_game_id == 0 && game_id > 0 {
        manifest["gameId"] = json!(game_id);
    }
    write(&manifest_path, &manifest)?;
    Ok(json!({ "alias": alias, "placeId": place_id, "root": relative_root }))
}

pub(super) fn rename(context: &BoundContext, parameters: &Value) -> Result<Value> {
    let object = parameters.as_object().context("p must be an object")?;
    let place_id = object
        .get("placeId")
        .and_then(Value::as_i64)
        .context("place-rename requires p.placeId")?;
    let requested = string(object, "alias").context("place-rename requires p.alias")?;
    let alias = normalize_alias(&requested, place_id);
    let (path, mut manifest) = read(context)?;
    let places = manifest
        .get_mut("places")
        .and_then(Value::as_object_mut)
        .context("Experience places must be an object")?;
    if places.contains_key(&alias) {
        bail!("Place alias {alias} already exists");
    }
    let current = places
        .iter()
        .find_map(|(name, place)| {
            (place.get("placeId").and_then(Value::as_i64) == Some(place_id)).then(|| name.clone())
        })
        .context("place-rename placeId is not configured")?;
    if current == alias {
        return Ok(json!({ "alias": alias, "placeId": place_id }));
    }
    let mut place = places
        .remove(&current)
        .expect("configured place disappeared");
    let old_root = place
        .get("root")
        .and_then(Value::as_str)
        .context("Place root is missing")?
        .to_string();
    let expected_old = format!("places/{current}");
    if old_root.replace('\\', "/") == expected_old {
        let new_root = format!("places/{alias}");
        let source = Path::new(&context.experience).join(&old_root);
        let destination = Path::new(&context.experience).join(&new_root);
        if destination.exists() {
            bail!("Place root {} already exists", destination.display());
        }
        fs::rename(&source, &destination)
            .with_context(|| format!("Failed to rename {}", source.display()))?;
        place["root"] = Value::String(new_root);
    }
    places.insert(alias.clone(), place);
    if manifest.get("startPlace").and_then(Value::as_str) == Some(&current) {
        manifest["startPlace"] = Value::String(alias.clone());
    }
    write(&path, &manifest)?;
    Ok(json!({ "alias": alias, "placeId": place_id }))
}

pub(super) fn reorder(context: &BoundContext, parameters: &Value) -> Result<Value> {
    let requested = parameters
        .get("order")
        .and_then(Value::as_array)
        .context("place-reorder requires p.order")?;
    let order = requested
        .iter()
        .map(|value| value.as_i64().context("p.order must contain place IDs"))
        .collect::<Result<Vec<_>>>()?;
    let (path, mut manifest) = read(context)?;
    let configured = manifest
        .get("places")
        .and_then(Value::as_object)
        .context("Experience places must be an object")?
        .values()
        .filter_map(|place| place.get("placeId").and_then(Value::as_i64))
        .filter(|id| *id > 0)
        .collect::<HashSet<_>>();
    let requested_set = order.iter().copied().collect::<HashSet<_>>();
    if order.len() != requested_set.len() || requested_set != configured {
        bail!("p.order must contain every configured published place ID exactly once");
    }
    manifest["placeOrder"] = serde_json::to_value(&order)?;
    write(&path, &manifest)?;
    Ok(json!({ "order": order }))
}
