use super::*;
#[cfg(any(windows, target_os = "macos"))]
use crate::automation::property_access::{Decision, Operation};
use crate::automation::property_access::{Intent, Mode, Scope, WRITE_WARNING};
use clap::{Args, Subcommand};

#[derive(Args)]
pub(crate) struct PropertyAccessArgs {
    #[command(subcommand)]
    action: Action,
    #[arg(
        short,
        long,
        global = true,
        help = "Target this play client's permission mode"
    )]
    player: Option<String>,
    #[command(flatten)]
    bridge: BridgeConnectionArgs,
}

#[derive(Subcommand, serde::Serialize, serde::Deserialize)]
#[serde(
    tag = "action",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
enum Action {
    #[command(about = "Show or set protected-property access for this Studio runtime")]
    Mode {
        #[arg(value_enum)]
        mode: Option<Mode>,
        #[arg(
            long,
            help = "Acknowledge protected-write risks after the user's explicit request"
        )]
        #[serde(default)]
        accept_risk: bool,
    },
    #[command(about = "Read an engine property, requesting exact approval when needed")]
    Read {
        target: String,
        property: String,
        #[arg(long, value_delimiter = ',')]
        #[serde(default)]
        ords: Vec<usize>,
    },
    #[command(about = "Write and verify an engine property; respects the selected access mode")]
    Write {
        target: String,
        property: String,
        #[arg(value_parser = property_value, allow_hyphen_values = true)]
        value: Value,
        #[arg(long, value_delimiter = ',')]
        #[serde(default)]
        ords: Vec<usize>,
    },
    #[command(about = "Approve and execute one exact pending property request")]
    Approve {
        #[arg(allow_hyphen_values = true)]
        request_id: String,
    },
    #[command(about = "Reject a pending property request")]
    Reject {
        #[arg(allow_hyphen_values = true)]
        request_id: String,
    },
}

fn property_value(raw: &str) -> Result<Value, String> {
    let value = serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.into()));
    value_text(&value).map_err(|error| error.to_string())?;
    Ok(value)
}

fn value_text(value: &Value) -> Result<String> {
    match value {
        Value::String(text) if text.len() <= 65536 => Ok(text.clone()),
        Value::Bool(_) | Value::Number(_) => Ok(value.to_string()),
        _ => bail!("Property value must be text (up to 64 KiB), a boolean, or a number"),
    }
}

pub(crate) fn command(args: PropertyAccessArgs) -> Result<()> {
    if let Action::Mode { mode, accept_risk } = &args.action {
        if *accept_risk && *mode != Some(Mode::ReadWrite) {
            bail!("--accept-risk applies only to read-write mode");
        }
        if *mode == Some(Mode::ReadWrite) {
            eprintln!("{WRITE_WARNING}");
            if !accept_risk {
                bail!("Read-write mode requires --accept-risk");
            }
        }
    }
    let mut parameters = serde_json::to_value(args.action)?;
    parameters["player"] = json!(args.player);
    let result = daemon_result(
        op::PROPERTY_ACCESS,
        None,
        parameters,
        false,
        Some(&args.bridge),
    )?;
    print_json_output(&result, false)
}

#[cfg(any(windows, target_os = "macos", test))]
fn target_parts(raw: &str, ords: &[usize]) -> Result<Vec<String>> {
    let parts = if raw.trim_start().starts_with('[') {
        serde_json::from_str::<Vec<String>>(raw).context("Target JSON must be a string array")?
    } else {
        raw.split('.').map(str::to_owned).collect()
    };
    if parts.is_empty()
        || parts.len() > 64
        || parts.iter().any(String::is_empty)
        || (!ords.is_empty() && ords.len() != parts.len())
        || ords.contains(&0)
    {
        bail!("Target needs 1–64 nonempty path segments and matching positive ordinals");
    }
    Ok(parts)
}

pub(crate) fn result(
    parameters: &Value,
    context: &crate::automation::BoundContext,
    state: &crate::automation::State,
    bridge: &BridgeServer,
    wait_seconds: f64,
) -> Result<Value> {
    let mut parameters = parameters.clone();
    let object = parameters
        .as_object_mut()
        .context("Expected property-access options")?;
    object.remove("bridgeWaitSeconds");
    object.remove("bridgePorts");
    let player = object
        .remove("player")
        .map(serde_json::from_value::<Option<String>>)
        .transpose()?
        .flatten();
    let action: Action = serde_json::from_value(parameters)?;
    let target = if player.is_some() {
        BridgeTarget::Client
    } else {
        BridgeTarget::Edit
    };
    if let Some(player) = &player {
        if player.trim().is_empty() || player == "0" {
            bail!("Choose a play client name or positive index");
        }
        wait_for_player_bridge(bridge, player, wait_seconds)?;
    } else {
        bridge.wait_for_target(wait_seconds, target)?;
    }
    let pin = bridge.runtime_pin_for_selector(target, player.as_deref())?;
    let scope = Scope {
        project: context.project.clone(),
        pid: bridge.studio_pid_for_runtime(target, &pin.runtime_id)?,
        runtime: pin.runtime_id,
    };
    let active = bridge
        .list_bridge_clients()
        .iter()
        .filter_map(|entry| entry["runtimeId"].as_str().map(str::to_owned))
        .collect::<Vec<_>>();
    let mut policy = state
        .property_access
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    policy.retain_runtimes(&active);
    match action {
        Action::Mode { mode, accept_risk } => {
            if let Some(mode) = mode {
                policy.set_mode(&scope, mode, accept_risk)?;
            } else if accept_risk {
                bail!("Risk acknowledgement applies only to read-write mode");
            }
            let mode = policy.mode(&scope);
            Ok(
                json!({"mode":mode,"defaultMode":Mode::Ask,"runtimeId":scope.runtime,"pid":scope.pid,
                "scope":"selected-runtime","ordinaryEditsUnaffected":true,
                "warning":if mode == Mode::ReadWrite {Some(WRITE_WARNING)} else {None}}),
            )
        }
        Action::Reject { request_id } => {
            policy.reject(&scope, &request_id)?;
            Ok(json!({"status":"rejected","requestId":request_id}))
        }
        action => {
            let approved = if let Action::Approve { request_id } = &action {
                Some(policy.approve(&scope, request_id)?)
            } else {
                None
            };
            drop(policy);
            if player.is_some() {
                bail!(
                    "Native property calls require Edit mode until client DataModel identity can be verified"
                );
            }
            perform(action, approved, &scope, state, bridge)
        }
    }
}

#[cfg(any(windows, target_os = "macos"))]
fn perform(
    action: Action,
    approved: Option<Intent>,
    scope: &Scope,
    state: &crate::automation::State,
    bridge: &BridgeServer,
) -> Result<Value> {
    let (target, property, ordinals, operation) = if let Some(intent) = &approved {
        (
            intent.path.clone(),
            intent.property.clone(),
            intent.ordinals.clone(),
            intent.operation.clone(),
        )
    } else {
        match action {
            Action::Read {
                target,
                property,
                ords,
            } => (target, property, ords, Operation::Read),
            Action::Write {
                target,
                property,
                ords,
                value,
            } => {
                value_text(&value)?;
                (target, property, ords, Operation::Write { value })
            }
            _ => unreachable!("permission action handled before native invocation"),
        }
    };
    if property.is_empty() || property.len() > 256 {
        bail!("Property name must contain 1–256 bytes");
    }
    let path = target_parts(&target, &ordinals)?;
    // The authenticated bridge already supplies the DataModel name. Reading a
    // window title would add an unrelated Accessibility permission on macOS.
    let info = bridge.cached_bridge_info_for_target(BridgeTarget::Edit)?;
    anyhow::ensure!(
        info.runtime_id == scope.runtime,
        "Protected property runtime was replaced"
    );
    let title = info.place_name;
    let mut native = crate::studio::native::serializer::prepare_property(
        scope.pid,
        &title,
        &path,
        &ordinals,
        &property,
        Duration::from_secs(3),
    )?;
    let intent = Intent {
        path: target,
        ordinals,
        property,
        operation,
        class_name: native.class_name.clone(),
        instance_id: native.instance_id.clone(),
    };
    if let Some(approved) = approved {
        if intent != approved {
            bail!("Property target changed since approval; request access again");
        }
    } else {
        let decision = state
            .property_access
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .check(scope, &intent)?;
        if matches!(decision, Decision::ApprovalRequired { .. }) {
            return Ok(serde_json::to_value(decision)?);
        }
    }
    // The pinned runtime must still be alive; never retarget an approved request.
    if bridge.studio_pid_for_runtime(BridgeTarget::Edit, &scope.runtime)? != scope.pid {
        bail!("Protected property runtime was replaced");
    }
    let before = native.read()?;
    let mut packages = Vec::new();
    let result = match &intent.operation {
        Operation::Read => Ok(before.clone()),
        Operation::Write { value } => {
            let text = value_text(value)?;
            if crate::automation::property_access::property_text_matches(
                &intent.class_name,
                &intent.property,
                &text,
                &before,
            ) {
                Ok(before.clone())
            } else {
                native.ensure_writable()?;
                prepare_packages(
                    bridge,
                    scope,
                    &title,
                    &path,
                    &intent,
                    native.remaining()?,
                    &mut packages,
                )
                .and_then(|()| native.write(&text))
            }
        }
    };
    let result = result.and_then(|value| {
        sample_completed_write(bridge, scope, &path, &intent)?;
        Ok(value)
    });
    match result {
        Ok(value) => Ok(
            json!({"status":"applied","path":intent.path,"property":intent.property,
            "className":intent.class_name,"value":value,"encoding":"studio-text","changed":before!=value,
            "runtimeId":scope.runtime,"autoDesyncedPackages":packages}),
        ),
        Err(error) if !packages.is_empty() => Err(error.context(format!(
            "Packages marked Changed before the failure: {}",
            packages.join(", ")
        ))),
        Err(error) => Err(error),
    }
}

#[cfg(any(windows, target_os = "macos"))]
fn sample_completed_write(
    bridge: &BridgeServer,
    scope: &Scope,
    path: &[String],
    intent: &Intent,
) -> Result<()> {
    if intent.class_name != "MeshPart"
        || intent.property != "CollisionFidelity"
        || !matches!(intent.operation, Operation::Write { .. })
    {
        return Ok(());
    }
    // Cooking may finish after both the original and deferred engine signals.
    // Sample after verified completion, before another transaction captures its guard.
    // This is a user edit: record it normally rather than suppressing or acknowledging it.
    let result = bridge.call_for_runtime_with_timeout(
        "sampleStudioProperty",
        json!({ "pathSegments": path, "pathOrdinals": intent.ordinals,
            "className": intent.class_name, "property": intent.property }),
        BridgeTarget::Edit,
        &scope.runtime,
        Some(Duration::from_secs(2)),
    )?;
    crate::app::output::ensure_plugin_api_ok(&result)
}

#[cfg(any(windows, target_os = "macos"))]
fn prepare_packages(
    bridge: &BridgeServer,
    scope: &Scope,
    title: &str,
    path: &[String],
    intent: &Intent,
    timeout: Duration,
    changed: &mut Vec<String>,
) -> Result<()> {
    use crate::studio::native::serializer::{PackageAction, PackageTarget, run_package_action};
    let include_self = !crate::editor::sync::package_root_property_is_override(
        &intent.class_name,
        &intent.property,
    );
    let targets = [
        json!({"service":path[0],"pathSegments":path,"pathOrdinals":intent.ordinals,"includeSelf":include_self}),
    ];
    let started = Instant::now();
    let roots = crate::editor::sync::discover_editor_mutation_packages_with_timeout(
        bridge,
        &targets,
        Some(&scope.runtime),
        Some(timeout),
    )?;
    for root in roots {
        let remaining = timeout
            .checked_sub(started.elapsed())
            .context("Package preparation exceeded its deadline")?;
        let result = run_package_action(
            scope.pid,
            title,
            &PackageTarget {
                path_segments: root.path_segments,
                path_ordinals: root.path_ordinals,
                expected_version: root.expected_version,
            },
            PackageAction::Desync,
            remaining,
        )?;
        if result.changed {
            changed.push(result.path);
        }
    }
    Ok(())
}

#[cfg(not(any(windows, target_os = "macos")))]
fn perform(
    _action: Action,
    _approved: Option<Intent>,
    _scope: &Scope,
    _state: &crate::automation::State,
    _bridge: &BridgeServer,
) -> Result<Value> {
    bail!("Protected property native access is not yet implemented on this platform")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_accepts_url_safe_request_ids_and_negative_property_values() -> Result<()> {
        use clap::FromArgMatches;
        for action in ["approve", "reject"] {
            let matches = PropertyAccessArgs::augment_args(clap::Command::new("access"))
                .try_get_matches_from(["access", action, "-G_test-id"])?;
            let args = PropertyAccessArgs::from_arg_matches(&matches)?;
            assert_eq!(
                serde_json::to_value(args.action)?["requestId"],
                "-G_test-id"
            );
        }
        let matches = PropertyAccessArgs::augment_args(clap::Command::new("access"))
            .try_get_matches_from(["access", "write", "Workspace.Value", "Value", "-3.5"])?;
        let args = PropertyAccessArgs::from_arg_matches(&matches)?;
        assert_eq!(serde_json::to_value(args.action)?["value"], -3.5);
        Ok(())
    }

    #[test]
    fn property_requests_cannot_supply_identity_trust_or_extra_mode_fields() {
        for extra in ["trusted", "className", "instanceId", "mode"] {
            let mut request =
                json!({"action":"read","target":"Workspace","property":"StreamingEnabled"});
            request[extra] = json!(true);
            assert!(serde_json::from_value::<Action>(request).is_err());
        }
        assert!(target_parts("[\"Workspace\",\"Name.with.dots\"]", &[1, 2]).is_ok());
        assert!(target_parts("Workspace.Name", &[1]).is_err());
        assert!(target_parts("Workspace.Name", &[1, 0]).is_err());
        assert!(value_text(&json!({"script":"not executable"})).is_err());
    }
}
