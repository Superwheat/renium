use std::collections::HashSet;
use std::fs;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value, json};

use super::BoundContext;
use crate::project::config;
use crate::project::workflows;
use crate::system::files::atomic_write_file;

const PLACE_PROJECT_PATHS: &[&str] = &[
    "renium.project.jsonc",
    "renium.project.json",
    ".renium",
    "sourcemap.json",
    "renium-link.json",
    "wally.toml",
    "wally.lock",
    "Packages",
];

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

fn place_root(experience: &Path, relative_root: &str) -> Result<(String, PathBuf)> {
    let normalized = relative_root.replace('\\', "/");
    let relative = Path::new(&normalized);
    if relative.is_absolute()
        || relative.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        bail!("Place root must be a relative path inside the experience project");
    }
    let root = experience.join(relative);
    if root == experience || !root.starts_with(experience) {
        bail!("Place root must stay inside the experience project");
    }
    Ok((normalized, root))
}

fn rollback_migration(moves: &[(PathBuf, PathBuf)], roots: &[PathBuf]) -> Result<()> {
    let mut errors = Vec::new();
    for (source, destination) in moves.iter().rev() {
        if !destination.exists() {
            continue;
        }
        if source.exists() {
            errors.push(format!("{} already exists", source.display()));
            continue;
        }
        if let Err(error) = fs::rename(destination, source) {
            errors.push(format!(
                "{} -> {}: {error}",
                destination.display(),
                source.display()
            ));
        }
    }
    if errors.is_empty() {
        for root in roots.iter().rev() {
            if root.exists()
                && let Err(error) = fs::remove_dir_all(root)
            {
                errors.push(format!("{}: {error}", root.display()));
            }
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        bail!(
            "Place migration rollback was incomplete: {}",
            errors.join("; ")
        )
    }
}

fn migration_paths(
    experience: &Path,
    root: &Path,
    source_root: &Path,
) -> Result<Vec<(PathBuf, PathBuf)>> {
    if source_root.as_os_str().is_empty() || source_root == Path::new(".") {
        bail!(
            "A project with sourceRoot '.' cannot be converted to a multi-place layout automatically"
        );
    }
    let mut entries = PLACE_PROJECT_PATHS
        .iter()
        .map(PathBuf::from)
        .chain(std::iter::once(source_root.to_path_buf()))
        .collect::<Vec<_>>();
    entries.sort();
    entries.dedup();
    let all_entries = entries.clone();
    entries.retain(|candidate| {
        !all_entries
            .iter()
            .any(|other| other != candidate && candidate.starts_with(other))
    });
    let moves = entries
        .into_iter()
        .filter_map(|entry| {
            let source = experience.join(&entry);
            source.exists().then(|| (source, root.join(entry)))
        })
        .collect::<Vec<_>>();
    for (_, destination) in &moves {
        if destination.exists() {
            bail!(
                "Cannot migrate because {} already exists",
                destination.display()
            );
        }
    }
    Ok(moves)
}

fn run_migration(
    moves: &[(PathBuf, PathBuf)],
    roots: &[PathBuf],
    operation: impl FnOnce() -> Result<()>,
) -> Result<()> {
    for root in roots {
        if root.exists() {
            bail!("Place root {} already exists", root.display());
        }
    }
    for (created, root) in roots.iter().enumerate() {
        if let Err(error) = fs::create_dir_all(root) {
            let cleanup = rollback_migration(&[], &roots[..created]);
            let error =
                anyhow::Error::new(error).context(format!("Failed to create {}", root.display()));
            return match cleanup {
                Ok(()) => Err(error),
                Err(rollback) => Err(error.context(rollback.to_string())),
            };
        }
    }
    let mut moved = 0;
    let result = (|| -> Result<()> {
        for (source, destination) in moves {
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("Failed to create {}", parent.display()))?;
            }
            fs::rename(source, destination)
                .with_context(|| format!("Failed to move {}", source.display()))?;
            moved += 1;
        }
        operation()
    })();
    if let Err(error) = result {
        return match rollback_migration(&moves[..moved], roots) {
            Ok(()) => Err(error),
            Err(rollback) => Err(error.context(rollback.to_string())),
        };
    }
    Ok(())
}

fn convert_single_place(
    context: &BoundContext,
    object: &Map<String, Value>,
    place_id: i64,
    game_id: i64,
    name: String,
    alias: String,
) -> Result<Value> {
    if let Some(current_game_id) = context.game_id
        && current_game_id > 0
        && game_id > 0
        && current_game_id != game_id
    {
        bail!("Place gameId {game_id} does not match project gameId {current_game_id}");
    }
    let experience = Path::new(&context.experience);
    let same_place = context.place_id.is_none_or(|current| current == place_id);
    let current_place_id = context.place_id.unwrap_or(place_id);
    let current_name = if same_place {
        name.clone()
    } else {
        Path::new(&context.root)
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("main")
            .to_string()
    };
    let current_alias = if same_place {
        alias.clone()
    } else {
        normalize_alias(&current_name, current_place_id)
    };
    if !same_place && current_alias == alias {
        bail!("Place alias {alias} is already used by the current project");
    }
    let requested_root = string(object, "root").unwrap_or_else(|| format!("places/{alias}"));
    let current_relative = if same_place {
        requested_root.clone()
    } else {
        format!("places/{current_alias}")
    };
    let (current_relative, current_root) = place_root(experience, &current_relative)?;
    let (requested_relative, requested_root) = if same_place {
        (current_relative.clone(), current_root.clone())
    } else {
        place_root(experience, &requested_root)?
    };
    if !same_place && current_root == requested_root {
        bail!("Place root {requested_relative} is already used by the current project");
    }
    let source_root = super::context::source_dir(context)?;
    let moves = migration_paths(experience, &current_root, &source_root)?;
    let mut roots = vec![current_root.clone()];
    if !same_place {
        roots.push(requested_root.clone());
    }
    let mut places = Map::new();
    places.insert(
        current_alias.clone(),
        json!({
            "placeId": current_place_id,
            "name": current_name,
            "root": current_relative,
        }),
    );
    if !same_place {
        places.insert(
            alias.clone(),
            json!({ "placeId": place_id, "name": name, "root": requested_relative }),
        );
    }
    let mut order = Vec::new();
    if current_place_id > 0 {
        order.push(current_place_id);
    }
    if !same_place && place_id > 0 {
        order.push(place_id);
    }
    let manifest = json!({
        "version": 2,
        "gameId": context.game_id.unwrap_or(game_id),
        "startPlace": current_alias,
        "placeOrder": order,
        "places": places,
    });
    let manifest_path = experience.join("renium.experience.json");
    run_migration(&moves, &roots, || {
        workflows::initialize_place_root(&current_root, &source_root)?;
        if !same_place {
            workflows::initialize_place_root(&requested_root, Path::new("src"))?;
        }
        write(&manifest_path, &manifest)
    })?;
    Ok(json!({
        "alias": alias,
        "placeId": place_id,
        "root": if same_place { current_relative } else { requested_relative },
        "rebind": true,
    }))
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
    if !manifest_path.is_file() {
        return convert_single_place(context, object, place_id, game_id, name, alias);
    }
    let mut manifest = serde_json::from_slice::<Value>(&fs::read(&manifest_path)?)?;
    let manifest_game_id = manifest.get("gameId").and_then(Value::as_i64).unwrap_or(0);
    if manifest_game_id > 0 && game_id > 0 && manifest_game_id != game_id {
        bail!("Place gameId {game_id} does not match project gameId {manifest_game_id}");
    }
    let relative_root = string(object, "root").unwrap_or_else(|| format!("places/{alias}"));
    let (relative_root, place_root) = place_root(experience, &relative_root)?;
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
    if places.values().any(|place| {
        place
            .get("root")
            .and_then(Value::as_str)
            .is_some_and(|root| root.replace('\\', "/") == relative_root)
    }) {
        bail!("Place root {relative_root} is already configured");
    }
    let root_existed = place_root.exists();
    if root_existed
        && !place_root.join(config::PROJECT_FILE_NAME).is_file()
        && !place_root.join(config::PROJECT_JSON_FILE_NAME).is_file()
    {
        bail!(
            "Place root {} exists but is not a Renium project",
            place_root.display()
        );
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
    if let Err(error) = write(&manifest_path, &manifest) {
        if !root_existed {
            fs::remove_dir_all(&place_root).with_context(|| {
                format!(
                    "Failed to write the experience manifest and remove {}",
                    place_root.display()
                )
            })?;
        }
        return Err(error);
    }
    Ok(json!({ "alias": alias, "placeId": place_id, "root": relative_root, "rebind": true }))
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
    let current = places
        .iter()
        .find_map(|(name, place)| {
            (place.get("placeId").and_then(Value::as_i64) == Some(place_id)).then(|| name.clone())
        })
        .context("place-rename placeId is not configured")?;
    if current == alias {
        return Ok(json!({ "alias": alias, "placeId": place_id, "rebind": false }));
    }
    if places.contains_key(&alias) {
        bail!("Place alias {alias} already exists");
    }
    let mut place = places
        .get(&current)
        .cloned()
        .context("Configured place disappeared")?;
    let old_root = place
        .get("root")
        .and_then(Value::as_str)
        .context("Place root is missing")?
        .to_string();
    let expected_old = format!("places/{current}");
    let renamed = if old_root.replace('\\', "/") == expected_old {
        let new_root = format!("places/{alias}");
        let source = Path::new(&context.experience).join(&old_root);
        let destination = Path::new(&context.experience).join(&new_root);
        if destination.exists() {
            bail!("Place root {} already exists", destination.display());
        }
        fs::rename(&source, &destination)
            .with_context(|| format!("Failed to rename {}", source.display()))?;
        place["root"] = Value::String(new_root);
        Some((source, destination))
    } else {
        None
    };
    places.remove(&current);
    places.insert(alias.clone(), place);
    if manifest.get("startPlace").and_then(Value::as_str) == Some(&current) {
        manifest["startPlace"] = Value::String(alias.clone());
    }
    if let Err(error) = write(&path, &manifest) {
        if let Some((source, destination)) = renamed
            && let Err(rollback) = fs::rename(&destination, &source)
        {
            return Err(error.context(format!(
                "Failed to restore {} after the manifest write failed: {rollback}",
                source.display()
            )));
        }
        return Err(error);
    }
    Ok(json!({ "alias": alias, "placeId": place_id, "rebind": true }))
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
    Ok(json!({ "order": order, "rebind": true }))
}
