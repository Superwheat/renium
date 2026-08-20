use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use super::{BoundContext, Failure, State};
use crate::project::experience::{AmbiguousExperiencePlace, resolve_experience_place};
use crate::project::{config, workflows};
use crate::studio::bridge::{BRIDGE_ROLE_EDIT, BridgeInfoPayload, BridgeServer};
use crate::studio::target::{place_matches, set_place_filter};
use crate::system::files::canonical_path;

fn object(value: &Value) -> std::result::Result<&Map<String, Value>, Failure> {
    value
        .as_object()
        .ok_or_else(|| Failure::new("bad_req", "p must be an object", false, "context"))
}

fn string(object: &Map<String, Value>, key: &str) -> Option<String> {
    object.get(key).and_then(|value| match value {
        Value::String(value) => Some(value.clone()),
        Value::Number(value) => Some(value.to_string()),
        _ => None,
    })
}

fn fingerprint(project: &Path, experience: &Path) -> Result<String> {
    let mut hash = Sha256::new();
    hash.update(project.as_os_str().to_string_lossy().as_bytes());
    if project.is_file() {
        hash.update(
            fs::read(project).with_context(|| format!("Failed to read {}", project.display()))?,
        );
    } else {
        hash.update(b"missing-project");
    }
    let manifest = experience.join("renium.experience.json");
    if manifest.is_file() {
        hash.update(
            fs::read(&manifest)
                .with_context(|| format!("Failed to read {}", manifest.display()))?,
        );
    }
    Ok(format!("{:x}", hash.finalize()))
}

fn bind_project_failure(error: anyhow::Error) -> Failure {
    let code = if error.downcast_ref::<AmbiguousExperiencePlace>().is_some() {
        "ambiguous_place"
    } else {
        "no_project"
    };
    Failure::new(code, format!("{error:#}"), false, "bind")
}

fn ambiguous_studios(candidates: &[Value]) -> Failure {
    let compact = candidates
        .iter()
        .map(|entry| {
            json!({
                "id": entry.get("runtimeId"),
                "n": entry.get("placeName"),
                "p": entry.get("placeId"),
            })
        })
        .collect::<Vec<_>>();
    Failure::new(
        "ambiguous_place",
        "More than one Studio runtime matches this project",
        false,
        "studios",
    )
    .detail(json!({ "candidates": compact }))
}

fn client_matches(entry: &Value, selector: &str) -> bool {
    if selector.trim().is_empty() {
        return true;
    }
    place_matches(
        &BridgeInfoPayload {
            runtime_id: entry
                .get("runtimeId")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            place_id: entry.get("placeId").and_then(Value::as_i64),
            game_id: entry.get("gameId").and_then(Value::as_i64),
            place_name: entry
                .get("placeName")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            ..BridgeInfoPayload::default()
        },
        selector,
    )
}

fn client_selector(entry: &Value) -> Option<String> {
    let place_id = entry.get("placeId").and_then(Value::as_i64)?;
    Some(entry.get("gameId").and_then(Value::as_i64).map_or_else(
        || place_id.to_string(),
        |game_id| format!("{game_id}:{place_id}"),
    ))
}

pub(super) fn studio_candidates_from(clients: &[Value], selector: &str) -> Vec<Value> {
    let mut seen = HashSet::new();
    clients
        .iter()
        .filter(|entry| entry.get("role").and_then(Value::as_str) == Some(BRIDGE_ROLE_EDIT))
        .filter(|entry| client_matches(entry, selector))
        .filter(|entry| {
            let id = entry
                .get("runtimeId")
                .and_then(Value::as_str)
                .unwrap_or_default();
            !id.is_empty() && seen.insert(id.to_string())
        })
        .cloned()
        .collect()
}

fn studio_candidates(bridge: &BridgeServer, selector: &str) -> Vec<Value> {
    studio_candidates_from(&bridge.list_bridge_clients(), selector)
}

pub(super) fn context_clients(clients: Vec<Value>, context: &BoundContext) -> Vec<Value> {
    clients
        .into_iter()
        .filter(|entry| {
            context.runtime_id.as_deref().map_or_else(
                || client_matches(entry, &context.selector),
                |runtime_id| {
                    entry.get("runtimeId").and_then(Value::as_str) == Some(runtime_id)
                        || entry.get("launchEditRuntimeId").and_then(Value::as_str)
                            == Some(runtime_id)
                },
            )
        })
        .collect()
}

fn bootstrap(state: &State, root: &Path) -> std::result::Result<Value, Failure> {
    let project_root = canonical_path(root)
        .map_err(|error| Failure::new("no_project", format!("{error:#}"), false, "project-init"))?;
    if !project_root.is_dir() {
        return Err(Failure::new(
            "no_project",
            "Bootstrap root must be an existing directory",
            false,
            "project-init",
        ));
    }
    let project_path = project_root.join(config::PROJECT_FILE_NAME);
    let fingerprint = fingerprint(&project_path, &project_root)
        .map_err(|error| Failure::new("internal", format!("{error:#}"), false, "context"))?;
    let context = state.insert_context(BoundContext {
        id: 0,
        initialized: false,
        project: project_path.display().to_string(),
        root: project_root.display().to_string(),
        experience: project_root.display().to_string(),
        source: project_root.join("src").display().to_string(),
        place_id: None,
        game_id: None,
        selector: String::new(),
        runtime_id: None,
        plugin_build: None,
        fingerprint,
    });
    serde_json::to_value(context)
        .map_err(|error| Failure::new("internal", error.to_string(), false, "bind"))
}

pub(super) fn bind(
    state: &State,
    bridge: &BridgeServer,
    parameters: &Value,
) -> std::result::Result<Value, Failure> {
    let object = object(parameters)?;
    let root = PathBuf::from(string(object, "root").unwrap_or_else(|| ".".to_string()));
    if !root.is_absolute() {
        return Err(Failure::new(
            "bad_req",
            "bind p.root must be an absolute path",
            false,
            "bind",
        ));
    }
    let root = canonical_path(&root)
        .map_err(|error| Failure::new("no_project", format!("{error:#}"), false, "project-init"))?;
    let explicit_project = string(object, "project").map(PathBuf::from).map(|project| {
        if project.is_absolute() {
            project
        } else {
            root.join(project)
        }
    });
    let requested_place = string(object, "place").filter(|value| !value.trim().is_empty());
    let requested_runtime = string(object, "runtime");
    let mut connected = studio_candidates(bridge, requested_place.as_deref().unwrap_or_default());
    if let Some(runtime) = requested_runtime.as_deref() {
        connected.retain(|entry| entry.get("runtimeId").and_then(Value::as_str) == Some(runtime));
    }
    let selected_root = if explicit_project.is_none() {
        let selected = match resolve_experience_place(&root, requested_place.as_deref()) {
            Ok(place) => place,
            Err(error) if requested_place.is_none() => {
                let mut seen = HashSet::new();
                let mut matches = connected
                    .iter()
                    .filter_map(|entry| {
                        let selector = client_selector(entry)?;
                        let place = resolve_experience_place(&root, Some(&selector))
                            .ok()
                            .flatten()?;
                        seen.insert(place.root.clone())
                            .then(|| (entry.clone(), place))
                    })
                    .collect::<Vec<_>>();
                match matches.len() {
                    0 => return Err(bind_project_failure(error)),
                    1 => matches.pop().map(|(_, place)| place),
                    _ => {
                        let clients = matches
                            .into_iter()
                            .map(|(client, _)| client)
                            .collect::<Vec<_>>();
                        return Err(ambiguous_studios(&clients));
                    }
                }
            }
            Err(error) => return Err(bind_project_failure(error)),
        };
        selected.map_or_else(|| root.clone(), |place| place.root)
    } else {
        root.clone()
    };
    let direct_project = explicit_project
        .clone()
        .unwrap_or_else(|| selected_root.join(config::PROJECT_FILE_NAME));
    if object.get("bootstrap").and_then(Value::as_bool) == Some(true) && !direct_project.is_file() {
        return bootstrap(state, &root);
    }
    let loaded = (|| -> Result<config::LoadedProject> {
        if explicit_project.is_some() {
            return config::load_project(explicit_project.as_deref(), Some(&selected_root));
        }
        if let Some(loaded) = config::try_load_project(None, Some(&selected_root))?
            && loaded.root == selected_root
        {
            return Ok(loaded);
        }
        let project = workflows::initialize_place_root(&selected_root, Path::new("src"))?;
        config::load_project(Some(&project), None)
    })()
    .map_err(|error| Failure::new("no_project", format!("{error:#}"), false, "project-init"))?;
    let project_root = canonical_path(&loaded.root).map_err(|error| {
        Failure::new(
            "no_project",
            format!("{error:#}"),
            false,
            "project-validate",
        )
    })?;
    let project_path = canonical_path(&loaded.path).map_err(|error| {
        Failure::new(
            "no_project",
            format!("{error:#}"),
            false,
            "project-validate",
        )
    })?;
    if let Err(error) = workflows::refresh_agent_instructions(&project_root) {
        eprintln!(
            "[renium] warning: could not refresh project instructions in {}: {error:#}",
            project_root.display()
        );
    }
    let identity = resolve_experience_place(&project_root, None).map_err(|error| {
        Failure::new(
            "no_project",
            format!("{error:#}"),
            false,
            "project-validate",
        )
    })?;
    let experience = identity.as_ref().map_or_else(
        || project_root.clone(),
        |place| place.experience_root.clone(),
    );
    let manifest_game_id = identity.as_ref().and_then(|place| place.game_id);
    let manifest_place_id = identity.as_ref().and_then(|place| place.place_id);
    let alias = identity.as_ref().map(|place| place.alias.clone());
    let selector =
        requested_place
            .clone()
            .unwrap_or_else(|| match (manifest_game_id, manifest_place_id) {
                (Some(game_id), Some(place_id)) if game_id > 0 && place_id > 0 => {
                    format!("{game_id}:{place_id}")
                }
                (_, Some(place_id)) if place_id > 0 => place_id.to_string(),
                _ => alias.clone().unwrap_or_default(),
            });
    let mut candidates = studio_candidates(bridge, &selector);
    if let Some(runtime) = requested_runtime.as_deref() {
        candidates.retain(|entry| entry.get("runtimeId").and_then(Value::as_str) == Some(runtime));
    }
    if candidates.len() > 1 {
        return Err(ambiguous_studios(&candidates));
    }
    let candidate = candidates.first();
    let runtime_id = candidate
        .and_then(|entry| entry.get("runtimeId"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let plugin_build = candidate
        .and_then(|entry| entry.get("bridgeBuildUnix"))
        .and_then(Value::as_i64);
    let place_id = requested_place
        .as_deref()
        .and_then(|value| {
            value
                .rsplit_once(':')
                .map_or(value, |(_, id)| id)
                .parse::<i64>()
                .ok()
        })
        .or_else(|| {
            candidate
                .and_then(|entry| entry.get("placeId"))
                .and_then(Value::as_i64)
        })
        .or(manifest_place_id);
    let game_id = candidate
        .and_then(|entry| entry.get("gameId"))
        .and_then(Value::as_i64)
        .or(manifest_game_id);
    let fingerprint = fingerprint(&project_path, &experience).map_err(|error| {
        Failure::new(
            "no_project",
            format!("{error:#}"),
            false,
            "project-validate",
        )
    })?;
    let context = state.insert_context(BoundContext {
        id: 0,
        initialized: true,
        project: project_path.display().to_string(),
        root: project_root.display().to_string(),
        experience: experience.display().to_string(),
        source: project_root
            .join(&loaded.project.source_root)
            .display()
            .to_string(),
        place_id,
        game_id,
        selector,
        runtime_id,
        plugin_build,
        fingerprint,
    });
    serde_json::to_value(context)
        .map_err(|error| Failure::new("internal", error.to_string(), false, "bind"))
}

pub(super) fn resolve(
    state: &State,
    bridge: &BridgeServer,
    id: u64,
) -> std::result::Result<BoundContext, Failure> {
    let context = state
        .context(id)
        .ok_or_else(|| Failure::new("stale_cx", "Context is no longer valid", false, "bind"))?;
    let fingerprint = fingerprint(Path::new(&context.project), Path::new(&context.experience))
        .map_err(|_| Failure::new("stale_cx", "Project identity changed", false, "bind"))?;
    if fingerprint != context.fingerprint {
        return Err(Failure::new(
            "stale_cx",
            "Project identity changed",
            false,
            "bind",
        ));
    }
    if let Some(runtime_id) = context.runtime_id.as_deref() {
        let candidate = studio_candidates(bridge, &context.selector)
            .into_iter()
            .find(|entry| entry.get("runtimeId").and_then(Value::as_str) == Some(runtime_id))
            .ok_or_else(|| {
                Failure::new(
                    "stale_cx",
                    "The selected Studio runtime disconnected",
                    false,
                    "bind",
                )
            })?;
        if candidate.get("bridgeBuildUnix").and_then(Value::as_i64) != context.plugin_build {
            return Err(Failure::new(
                "stale_cx",
                "The selected Studio plugin build changed",
                false,
                "bind",
            ));
        }
    }
    Ok(context)
}

pub(super) struct Selection;

impl Drop for Selection {
    fn drop(&mut self) {
        set_place_filter(None);
        crate::app::context::clear_automation();
    }
}

pub(super) fn select(context: &BoundContext) -> Selection {
    set_place_filter((!context.selector.is_empty()).then(|| context.selector.clone()));
    crate::app::context::select_automation(
        context.runtime_id.clone(),
        PathBuf::from(&context.project),
    );
    Selection
}

pub(super) fn source_dir(context: &BoundContext) -> Result<PathBuf> {
    let relative = Path::new(&context.source)
        .strip_prefix(&context.root)
        .context("Bound source root is outside the project root")?;
    Ok(if relative.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        relative.to_path_buf()
    })
}

pub(super) fn path(context: &BoundContext, path: PathBuf) -> PathBuf {
    if path.is_absolute() {
        path
    } else {
        Path::new(&context.root).join(path)
    }
}
