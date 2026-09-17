use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::{Value, json};

use crate::app::output::print_json_output;
use crate::automation::commands::daemon_result;
use crate::automation::op;
use crate::cli::{CollabAction, CollabArgs, CollabRelayAction};

fn session_root(project: Option<&Path>) -> Result<PathBuf> {
    let root = match project {
        Some(path) => path.to_path_buf(),
        None => std::env::current_dir().context("Could not read the current directory")?,
    };
    std::fs::create_dir_all(&root)
        .with_context(|| format!("Could not create {}", root.display()))?;
    Ok(crate::system::files::strip_extended_prefix(
        std::fs::canonicalize(&root)?,
    ))
}

pub(crate) fn collab_command(args: CollabArgs, project: Option<&Path>) -> Result<()> {
    if let CollabAction::Relay { action } = &args.action {
        let result = match action {
            CollabRelayAction::Deploy => super::relay::deploy()?,
            CollabRelayAction::Set { url } => super::relay::set_default_relay(Some(url))?,
            CollabRelayAction::Clear => super::relay::set_default_relay(None)?,
            CollabRelayAction::Show => json!({ "relay": super::relay::default_relay() }),
        };
        return print_json_output(&result, false);
    }
    let explicit_root = match &args.action {
        CollabAction::Start { root, .. } | CollabAction::Join { root, .. } => root.clone(),
        _ => None,
    };
    let root = session_root(explicit_root.as_deref().or(project))?;
    let root_text = root.display().to_string();
    let invite_only = matches!(args.action, CollabAction::Invite);
    let (operation, parameters) = match args.action {
        CollabAction::Start {
            relay, local, name, ..
        } => (
            op::COLLAB_START,
            json!({ "root": root_text, "relay": relay, "tunnel": !local, "name": name }),
        ),
        CollabAction::Join { invite, name, .. } => (
            op::COLLAB_JOIN,
            json!({ "root": root_text, "invite": invite, "name": name }),
        ),
        CollabAction::Stop => (op::COLLAB_STOP, json!({ "root": root_text })),
        CollabAction::Status | CollabAction::Invite | CollabAction::Relay { .. } => {
            (op::COLLAB_STATUS, json!({ "root": root_text }))
        }
    };
    let result = daemon_result(operation, Some(&root), parameters, false, None)?;
    if invite_only {
        let invite = result
            .get("invite")
            .and_then(Value::as_str)
            .context("No collaboration session is running for this project")?;
        println!("{invite}");
        return Ok(());
    }
    print_json_output(&result, false)
}
