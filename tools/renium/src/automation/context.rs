use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use super::{BoundContext, Failure, State, StudioReopenTarget};
pub(crate) use crate::app::context::Selection;
#[cfg(any(windows, target_os = "macos"))]
use crate::editor::review::local_place_path_for_pid;
use crate::project::experience::{
    AmbiguousExperiencePlace, ExperiencePlace, resolve_experience_place,
};
use crate::project::{config, workflows};
#[cfg(any(windows, target_os = "macos"))]
use crate::studio::bridge::BridgeTarget;
use crate::studio::bridge::{BRIDGE_ROLE_EDIT, BridgeInfoPayload, BridgeServer};
use crate::studio::target::place_matches;
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
                "n": entry.get("studioName").or_else(|| entry.get("placeName")),
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
            ..BridgeInfoPayload::default()
        },
        selector,
    )
}

fn client_selector(entry: &Value) -> Option<String> {
    let place_id = entry
        .get("placeId")
        .and_then(Value::as_i64)
        .filter(|id| *id > 0)?;
    Some(
        entry
            .get("gameId")
            .and_then(Value::as_i64)
            .filter(|id| *id > 0)
            .map_or_else(
                || place_id.to_string(),
                |game_id| format!("{game_id}:{place_id}"),
            ),
    )
}

fn runtime_selector(
    requested: Option<&str>,
    identity: Option<&ExperiencePlace>,
    saved_target: Option<&StudioReopenTarget>,
) -> String {
    // Manifest aliases/names select a project, not Studio's mutable display name.
    if let Some(requested) = requested
        && !identity.is_some_and(|place| place.matches_selector(requested))
    {
        return requested.to_string();
    }
    let place_id = identity
        .and_then(|place| place.place_id)
        .filter(|id| *id > 0);
    let game_id = identity
        .and_then(|place| place.game_id)
        .filter(|id| *id > 0);
    match (game_id, place_id) {
        (Some(game_id), Some(place_id)) => format!("{game_id}:{place_id}"),
        (_, Some(place_id)) => place_id.to_string(),
        _ => saved_target
            .and_then(|target| match (target.game_id, target.place_id) {
                (Some(game_id), Some(place_id)) => Some(format!("{game_id}:{place_id}")),
                (_, Some(place_id)) => Some(place_id.to_string()),
                _ => None,
            })
            .or_else(|| requested.map(str::to_string))
            .or_else(|| identity.map(|place| place.alias.clone()))
            .unwrap_or_default(),
    }
}

fn client_matches_saved_target(
    bridge: &BridgeServer,
    entry: &Value,
    target: &StudioReopenTarget,
) -> bool {
    if let Some(place_id) = target.place_id {
        return entry.get("placeId").and_then(Value::as_i64) == Some(place_id)
            && target.game_id.is_none_or(|game_id| {
                entry.get("gameId").and_then(Value::as_i64) == Some(game_id)
            });
    }
    let Some(expected_file) = target.file.as_deref() else {
        return false;
    };
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let _ = (bridge, entry, expected_file);
        false
    }
    #[cfg(any(windows, target_os = "macos"))]
    {
        let Some(runtime_id) = entry.get("runtimeId").and_then(Value::as_str) else {
            return false;
        };
        let Ok(pid) = bridge.studio_pid_for_runtime(BridgeTarget::Edit, runtime_id) else {
            return false;
        };
        local_place_path_for_pid(pid).is_some_and(|file| {
            canonical_path(&file).unwrap_or(file)
                == canonical_path(expected_file).unwrap_or_else(|_| expected_file.to_path_buf())
        })
    }
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

fn studio_candidates(
    bridge: &BridgeServer,
    selector: &str,
) -> std::result::Result<Vec<Value>, Failure> {
    let clients = bridge.list_bridge_clients();
    #[cfg(any(windows, target_os = "macos"))]
    {
        let mut titles = std::collections::HashMap::new();
        studio_candidates_with_current_titles(
            &clients,
            selector,
            |runtime| {
                let pid = bridge.studio_pid_for_runtime(BridgeTarget::Edit, runtime)?;
                // A just-closed runtime can remain in the socket inventory until
                // its reader observes EOF. It no longer has a window to inspect.
                if !crate::daemon::is_process_alive(pid) {
                    return Ok(None);
                }
                if let Some(title) = titles.get(&pid) {
                    return Ok(Some(String::clone(title)));
                }
                let title = crate::studio::input::studio_window_title(pid)
                    .with_context(|| format!("Could not read Studio {pid}'s window name"));
                // The process can exit during the accessibility query too.
                if title.is_err() && !crate::daemon::is_process_alive(pid) {
                    return Ok(None);
                }
                let title = title?;
                titles.insert(pid, title.clone());
                Ok(Some(title))
            },
            || bridge.list_bridge_clients(),
        )
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    Ok(studio_candidates_from(&clients, selector))
}

#[cfg(any(windows, target_os = "macos", test))]
fn studio_candidates_with_current_titles(
    clients: &[Value],
    selector: &str,
    mut window_title: impl FnMut(&str) -> Result<Option<String>>,
    current_clients: impl FnOnce() -> Vec<Value>,
) -> std::result::Result<Vec<Value>, Failure> {
    let mut failures = Vec::new();
    let mut inspected = false;
    let mut candidates = studio_candidates_with_titles(clients, selector, |runtime| {
        inspected = true;
        match window_title(runtime) {
            Ok(title) => title,
            Err(error) => {
                failures.push((runtime.to_string(), error));
                None
            }
        }
    });
    if !inspected {
        return Ok(candidates);
    }
    // A different place can close while its title is being inspected. Only
    // current runtimes can contribute either a match or a lookup failure.
    let current = studio_candidates_from(&current_clients(), "")
        .into_iter()
        .filter_map(|entry| entry["runtimeId"].as_str().map(str::to_owned))
        .collect::<HashSet<_>>();
    candidates.retain(|entry| {
        entry["runtimeId"]
            .as_str()
            .is_some_and(|id| current.contains(id))
    });
    if let Some((_, error)) = failures.into_iter().find(|(id, _)| current.contains(id)) {
        return Err(Failure::new(
            "studio_name_unavailable",
            format!(
                "{error:#}. Use a place ID or configured alias if window-name access is unavailable"
            ),
            false,
            "studios",
        ));
    }
    Ok(candidates)
}

#[cfg(any(windows, target_os = "macos", test))]
fn studio_candidates_with_titles(
    clients: &[Value],
    selector: &str,
    mut window_title: impl FnMut(&str) -> Option<String>,
) -> Vec<Value> {
    let requested = selector.trim();
    if requested.is_empty()
        || requested.parse::<i64>().is_ok()
        || requested.split_once(':').is_some_and(|(game, place)| {
            game.parse::<i64>().is_ok() && place.parse::<i64>().is_ok()
        })
    {
        return studio_candidates_from(clients, selector);
    }
    // A published document can be called DTE while game.Name is still Place1.
    // Only inspect the OS-verified owner of a connected Edit runtime; never
    // use focus or choose one of several matching documents.
    let mut candidates = Vec::new();
    for mut entry in studio_candidates_from(clients, "") {
        let Some(title) = entry["runtimeId"].as_str().and_then(&mut window_title) else {
            continue;
        };
        let Some(name) = title.strip_suffix(" - Roblox Studio").map(str::trim) else {
            continue;
        };
        let info = BridgeInfoPayload {
            place_name: name.to_string(),
            ..Default::default()
        };
        if place_matches(&info, selector) {
            entry["studioName"] = json!(name);
            candidates.push(entry);
        }
    }
    candidates
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
        resource_lease: None,
        place_id: None,
        game_id: None,
        selector: String::new(),
        runtime_id: None,
        plugin_build: None,
        fingerprint,
    });
    let protected = context.resource_lease.is_some();
    let mut response = serde_json::to_value(context)
        .map_err(|error| Failure::new("internal", error.to_string(), false, "bind"))?;
    if protected {
        response["resourceLeaseProtected"] = json!(true);
    }
    Ok(response)
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
    let resource_lease = object
        .get("resourceLease")
        .filter(|value| !value.is_null())
        .map(|value| serde_json::from_value::<renium_plugin_sdk::lease::Claim>(value.clone()))
        .transpose()
        .map_err(|error| Failure::new("bad_req", error.to_string(), false, "bind"))?;
    let requested_runtime = string(object, "runtime");
    let selected_root = if explicit_project.is_none() {
        let selected = match resolve_experience_place(&root, requested_place.as_deref()) {
            Ok(place) => place,
            Err(error) if requested_place.is_none() => {
                let mut connected = studio_candidates_from(&bridge.list_bridge_clients(), "");
                if let Some(runtime) = requested_runtime.as_deref() {
                    connected.retain(|entry| {
                        entry.get("runtimeId").and_then(Value::as_str) == Some(runtime)
                    });
                }
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
    if let Err(error) = workflows::ensure_agent_instructions(&experience) {
        eprintln!(
            "[renium] warning: could not refresh project instructions in {}: {error:#}",
            experience.display()
        );
    }
    let manifest_game_id = identity.as_ref().and_then(|place| place.game_id);
    let manifest_place_id = identity.as_ref().and_then(|place| place.place_id);
    let saved_target = (|| -> Result<Option<StudioReopenTarget>> {
        let enabled_target = super::live::saved_studio_target_for_root(&project_root)?;
        if enabled_target.is_some() || manifest_place_id.is_some() {
            return Ok(enabled_target);
        }
        // Stopping Live Sync removes its enabled marker, not the project's
        // established pairing. Reuse that identity without starting sync.
        super::reconcile::saved_studio_target_for_root(&project_root, &experience)
    })()
    .map_err(|error| {
        Failure::new(
            "no_project",
            format!("{error:#}"),
            false,
            "project-validate",
        )
    })?;
    let selector = runtime_selector(
        requested_place.as_deref(),
        identity.as_ref(),
        saved_target.as_ref(),
    );
    let mut candidates = if object.get("projectOnly").and_then(Value::as_bool) == Some(true) {
        Vec::new()
    } else {
        studio_candidates(bridge, &selector)?
    };
    if let Some(runtime) = requested_runtime.as_deref() {
        candidates.retain(|entry| entry.get("runtimeId").and_then(Value::as_str) == Some(runtime));
    } else if requested_place.is_none()
        && manifest_place_id.is_none()
        && let Some(target) = saved_target.as_ref()
    {
        candidates.retain(|entry| client_matches_saved_target(bridge, entry, target));
    }
    if candidates.len() > 1 {
        return Err(ambiguous_studios(&candidates));
    }
    let candidate = candidates.first();
    // Names select a runtime once. Keep subsequent bridge calls/reconnects
    // on its published identity, not the mutable title or DataModel name.
    let selector = candidate
        .and_then(client_selector)
        .filter(|_| !selector.is_empty())
        .unwrap_or(selector);
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
        .or(manifest_place_id)
        .or_else(|| saved_target.as_ref().and_then(|target| target.place_id));
    crate::plugins::verify_place_lease(place_id, resource_lease.as_ref())
        .map_err(|error| Failure::new("resource_owned", format!("{error:#}"), false, "bind"))?;
    let game_id = candidate
        .and_then(|entry| entry.get("gameId"))
        .and_then(Value::as_i64)
        .or(manifest_game_id)
        .or_else(|| saved_target.as_ref().and_then(|target| target.game_id));
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
        resource_lease,
        selector,
        runtime_id,
        plugin_build,
        fingerprint,
    });
    serde_json::to_value(context)
        .map_err(|error| Failure::new("internal", error.to_string(), false, "bind"))
}

pub(super) fn resolve_project(
    state: &State,
    id: u64,
) -> std::result::Result<BoundContext, Failure> {
    let context = state
        .context(id)
        .ok_or_else(|| Failure::new("stale_cx", "Context is no longer valid", false, "bind"))?;
    crate::plugins::verify_place_lease(context.place_id, context.resource_lease.as_ref())
        .map_err(|error| Failure::new("resource_owned", format!("{error:#}"), false, "bind"))?;
    let fingerprint = fingerprint(Path::new(&context.project), Path::new(&context.experience))
        .map_err(|_| {
            state.remove_context(id);
            Failure::new("stale_cx", "Project identity changed", false, "bind")
        })?;
    if fingerprint != context.fingerprint {
        state.remove_context(id);
        return Err(Failure::new(
            "stale_cx",
            "Project identity changed",
            false,
            "bind",
        ));
    }
    Ok(context)
}

pub(super) fn resolve(
    state: &State,
    bridge: &BridgeServer,
    id: u64,
) -> std::result::Result<BoundContext, Failure> {
    let mut context = resolve_project(state, id)?;
    if let Some(runtime_id) = context.runtime_id.as_deref() {
        let candidates = studio_candidates_from(&bridge.list_bridge_clients(), "");
        if let Some(candidate) = candidates
            .iter()
            .find(|entry| entry.get("runtimeId").and_then(Value::as_str) == Some(runtime_id))
        {
            if candidate.get("bridgeBuildUnix").and_then(Value::as_i64) != context.plugin_build {
                return Err(Failure::new(
                    "stale_cx",
                    "The selected Studio plugin build changed",
                    true,
                    "bind",
                ));
            }
        } else {
            let candidates = studio_candidates(bridge, &context.selector)?;
            if candidates.len() > 1 {
                return Err(ambiguous_studios(&candidates));
            }
            let Some(candidate) = candidates.first() else {
                return Err(Failure::new(
                    "no_studio",
                    "The selected Studio runtime disconnected",
                    true,
                    "studios",
                ));
            };
            let replacement_id = candidate
                .get("runtimeId")
                .and_then(Value::as_str)
                .context("Replacement Studio runtime omitted its identity")
                .map_err(|error| Failure::new("no_studio", error.to_string(), true, "studios"))?;
            context = state
                .attach_context_runtime(
                    id,
                    replacement_id.to_string(),
                    candidate.get("bridgeBuildUnix").and_then(Value::as_i64),
                )
                .unwrap_or(context);
        }
    } else {
        let candidates = studio_candidates(bridge, &context.selector)?;
        if candidates.len() > 1 {
            return Err(ambiguous_studios(&candidates));
        }
        if let Some(candidate) = candidates.first()
            && let Some(runtime_id) = candidate.get("runtimeId").and_then(Value::as_str)
        {
            context = state
                .attach_context_runtime(
                    id,
                    runtime_id.to_string(),
                    candidate.get("bridgeBuildUnix").and_then(Value::as_i64),
                )
                .unwrap_or(context);
        }
    }
    Ok(context)
}

pub(crate) fn select(context: &BoundContext) -> Selection {
    crate::app::context::select_automation(
        context.runtime_id.clone(),
        PathBuf::from(&context.project),
        (!context.selector.is_empty()).then(|| context.selector.clone()),
    )
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn studio_window_names_resolve_place1_without_guessing_or_changing_model_names() {
        let clients = vec![
            json!({"runtimeId":"dte", "role":"edit", "gameId":10, "placeId":20, "placeName":"Place1"}),
            json!({"runtimeId":"baseplate", "role":"edit", "gameId":10, "placeId":30, "placeName":"Place1"}),
            json!({"runtimeId":"player", "role":"play-client", "placeName":"DTE"}),
        ];
        let lookup = |runtime: &str| {
            Some(format!(
                "{} - Roblox Studio",
                if runtime == "dte" { "DTE" } else { "Baseplate" }
            ))
        };
        let matches = studio_candidates_with_titles(&clients, " dTe ", lookup);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0]["runtimeId"], "dte");
        assert_eq!(matches[0]["placeName"], "Place1");
        assert_eq!(matches[0]["studioName"], "DTE");
        assert!(studio_candidates_from(&clients, "Place1").is_empty());
        let pinned = client_selector(&matches[0]).unwrap();
        assert_eq!(pinned, "10:20");
        assert_eq!(
            studio_candidates_from(&clients, &pinned)[0]["runtimeId"],
            "dte"
        );
        assert!(studio_candidates_with_titles(&clients, "missing", lookup).is_empty());
        assert!(studio_candidates_with_titles(&clients, "DTE", |_| None).is_empty());
        assert!(studio_candidates_with_titles(&clients, "DTE", |_| Some("DTE".into())).is_empty());
        for selector in ["", "20", "10:20", "40", "10:40"] {
            studio_candidates_with_titles(&clients, selector, |_| {
                panic!("IDs and unfiltered inventory must not read window titles")
            });
        }
        assert!(client_selector(&json!({"gameId":0, "placeId":0})).is_none());
    }

    #[test]
    fn studio_window_names_override_model_names_and_preserve_ambiguity() {
        let clients = vec![
            json!({"runtimeId":"one", "role":"edit", "placeId":20, "placeName":"DTE"}),
            json!({"runtimeId":"two", "role":"edit", "placeId":30, "placeName":"Place1"}),
            json!({"runtimeId":"two", "role":"edit", "placeId":30, "placeName":"Place1"}),
        ];
        let selected = studio_candidates_with_titles(&clients, "DTE", |runtime| {
            Some(format!(
                "{} - Roblox Studio",
                if runtime == "one" { "Baseplate" } else { "DTE" }
            ))
        });
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0]["runtimeId"], "two");
        assert!(studio_candidates_with_titles(&clients, "DTE", |_| None).is_empty());
        let candidates =
            studio_candidates_with_titles(&clients, "DTE", |_| Some("DTE - Roblox Studio".into()));
        assert_eq!(candidates.len(), 2);
        assert_eq!(ambiguous_studios(&candidates).0.c, "ambiguous_place");
    }

    #[test]
    fn studio_window_lookup_excludes_runtimes_closed_during_the_query() {
        let clients = vec![
            json!({"runtimeId":"closing", "role":"edit", "placeId":0}),
            json!({"runtimeId":"wanted", "role":"edit", "placeId":0}),
        ];
        for closing_title in [
            Ok(Some("Fixture - Roblox Studio".into())),
            Err(anyhow::anyhow!("no accessibility windows")),
        ] {
            let mut closing_title = Some(closing_title);
            let candidates = studio_candidates_with_current_titles(
                &clients,
                "Fixture",
                |id| {
                    if id == "closing" {
                        closing_title.take().unwrap()
                    } else {
                        Ok(Some("Fixture - Roblox Studio".into()))
                    }
                },
                || vec![clients[1].clone()],
            )
            .unwrap_or_else(|error| panic!("{}", error.0.m));
            assert_eq!(candidates.len(), 1);
            assert_eq!(candidates[0]["runtimeId"], "wanted");
        }
        // A live but inaccessible document still makes name targeting unsafe.
        let failure = studio_candidates_with_current_titles(
            &clients,
            "Fixture",
            |id| {
                if id == "closing" {
                    Err(anyhow::anyhow!("accessibility permission denied"))
                } else {
                    Ok(Some("Fixture - Roblox Studio".into()))
                }
            },
            || clients.clone(),
        )
        .unwrap_err();
        assert_eq!(failure.0.c, "studio_name_unavailable");
        let candidates = studio_candidates_with_current_titles(
            &clients,
            "Fixture",
            |_| Ok(Some("Fixture - Roblox Studio".into())),
            || clients.clone(),
        )
        .unwrap_or_else(|error| panic!("{}", error.0.m));
        assert_eq!(candidates.len(), 2);
        for selector in ["", "20", "10:20"] {
            let _ = studio_candidates_with_current_titles(
                &clients,
                selector,
                |_| panic!("ID targeting must not inspect titles"),
                || panic!("ID targeting needs only the original inventory"),
            );
        }
    }

    #[test]
    fn manifest_aliases_bind_by_id_even_when_studio_reports_place1() {
        let root = crate::tests::support::temp_dir("context-alias");
        fs::create_dir(root.join("baseplate")).unwrap();
        fs::create_dir(root.join("lobby")).unwrap();
        fs::write(
            root.join("renium.experience.json"),
            json!({"gameId": 10, "places": {
                "baseplate": {"placeId": 20, "name": "Published Baseplate", "root": "baseplate"},
                "lobby": {"placeId": 30, "root": "lobby"}
            }})
            .to_string(),
        )
        .unwrap();
        let clients = vec![
            json!({"runtimeId":"wanted", "role":"edit", "gameId":10, "placeId":20, "placeName":"Place1"}),
            json!({"runtimeId":"other", "role":"edit", "gameId":10, "placeId":30, "placeName":"Baseplate"}),
        ];
        for requested in [
            "baseplate",
            " BASEPLATE ",
            "Published Baseplate",
            "20",
            "10:20",
        ] {
            let identity = resolve_experience_place(&root, Some(requested))
                .unwrap()
                .unwrap();
            let selector = runtime_selector(Some(requested), Some(&identity), None);
            assert_eq!(selector, "10:20", "{requested}");
            let matches = studio_candidates_from(&clients, &selector);
            assert_eq!(matches.len(), 1);
            assert_eq!(matches[0]["runtimeId"], "wanted");
            // An explicit target outside this project is not silently redirected.
            assert_eq!(
                runtime_selector(Some("10:30"), Some(&identity), None),
                "10:30"
            );
        }
        let mut identity = resolve_experience_place(&root, Some("baseplate"))
            .unwrap()
            .unwrap();
        identity.game_id = None;
        assert_eq!(
            runtime_selector(Some("baseplate"), Some(&identity), None),
            "20"
        );
        identity.place_id = None;
        let saved = StudioReopenTarget {
            file: None,
            game_id: Some(10),
            place_id: Some(20),
        };
        assert_eq!(
            runtime_selector(Some("baseplate"), Some(&identity), Some(&saved)),
            "10:20"
        );
        assert_eq!(
            runtime_selector(Some("baseplate"), Some(&identity), None),
            "baseplate"
        );
        assert_eq!(runtime_selector(Some("Place1"), None, None), "Place1");
        fs::remove_dir_all(root).unwrap();
    }
}
