use super::*;
#[cfg(any(windows, target_os = "macos"))]
use crate::automation::property_access::{Decision, Operation};
use crate::automation::property_access::{Intent, Mode, Scope, WRITE_WARNING};
use crate::system::LockRecover;
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
    #[command(about = "Call an engine function with exact, runtime-scoped approval")]
    Call {
        target: String,
        function: String,
        #[arg(default_value = "[]", value_parser = function_arguments)]
        arguments: Value,
        #[arg(long, value_delimiter = ',')]
        #[serde(default)]
        ords: Vec<usize>,
    },
    #[command(about = "Call one function with up to 32 argument arrays under one exact approval")]
    Batch {
        target: String,
        function: String,
        #[arg(value_parser = function_arguments)]
        arguments: Value,
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

fn function_arguments(raw: &str) -> Result<Value, String> {
    if raw.len() > 60 * 1024 {
        return Err("Function arguments exceed 60 KiB".into());
    }
    let value: Value = serde_json::from_str(raw)
        .map_err(|error| format!("Arguments must be a JSON array: {error}"))?;
    if !value.is_array() {
        return Err("Arguments must be a JSON array".into());
    }
    Ok(value)
}

#[cfg(any(windows, target_os = "macos", test))]
fn batch_arguments(value: &Value) -> Result<Vec<Vec<Value>>> {
    let arrays = value
        .as_array()
        .context("Batch arguments must be an array of argument arrays")?;
    anyhow::ensure!(
        (1..=32).contains(&arrays.len()) && serde_json::to_vec(value)?.len() <= 60 * 1024,
        "Function batches require 1–32 calls and at most 60 KiB of arguments"
    );
    arrays
        .iter()
        .map(|value| {
            value
                .as_array()
                .cloned()
                .context("Every batch item must be an argument array")
        })
        .collect()
}

#[cfg(any(windows, target_os = "macos", test))]
fn ordered_function_batch(
    arguments: &[Vec<Value>],
    mut call: impl FnMut(&[Value]) -> Result<String>,
) -> Value {
    let mut results = Vec::with_capacity(arguments.len());
    let mut stopped = false;
    let mut bytes = 0;
    for (index, arguments) in arguments.iter().enumerate() {
        if stopped || bytes >= 1024 * 1024 {
            results.push(json!({"index":index,"status":"not-executed","reason":if stopped {"earlier-call-failed"} else {"response-limit"}}));
            continue;
        }
        let item = match call(arguments) {
            Ok(value) => json!({"index":index,"status":"applied","value":value}),
            Err(error) => {
                stopped = true;
                json!({"index":index,"status":"unconfirmed","error":format!("{error:#}")})
            }
        };
        bytes += item.to_string().len();
        results.push(item);
    }
    json!(results)
}

fn value_text(value: &Value) -> Result<String> {
    match value {
        Value::String(text) if text.len() <= 65536 => Ok(text.clone()),
        Value::Bool(_) | Value::Number(_) => Ok(value.to_string()),
        _ => bail!("Property value must be text (up to 64 KiB), a boolean, or a number"),
    }
}

/// Studio text for a stored value, as the native property writer takes it.
/// Stores keep integers as floats, so an integer field gets integer text.
#[cfg(any(windows, target_os = "macos", test))]
pub(crate) fn saved_field_text(value: &Value, class_name: &str, property: &str) -> Result<String> {
    match value {
        Value::Number(number) => {
            let integer_field = rbx_reflection_database::get()
                .ok()
                .and_then(|database| {
                    crate::rbx::encode::rbx_property_descriptor(database, class_name, property)
                })
                .is_some_and(|descriptor| {
                    matches!(
                        descriptor.data_type,
                        rbx_reflection::DataType::Value(
                            rbx_dom_weak::types::VariantType::Int32
                                | rbx_dom_weak::types::VariantType::Int64
                        )
                    )
                });
            match number.as_f64() {
                Some(float) if integer_field && float.fract() == 0.0 => {
                    Ok(format!("{}", float as i64))
                }
                _ => value_text(value),
            }
        }
        Value::String(_) | Value::Bool(_) => value_text(value),
        Value::Object(object)
            if object.get("_type").and_then(Value::as_str) == Some("EnumItem") =>
        {
            object
                .get("name")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .context("Enum value has no name")
        }
        _ => bail!("only text, numbers, booleans and enum values can be written natively"),
    }
}

/// Writes one saved field the plugin cannot set, through the same native
/// property writer as `rbx access write`, on the pinned Edit runtime. Returns
/// whether Studio's value changed.
#[cfg(any(windows, target_os = "macos"))]
pub(crate) fn write_saved_field(
    bridge: &BridgeServer,
    path: &[String],
    property: &str,
    value: &Value,
) -> Result<bool> {
    let info = bridge.cached_bridge_info_for_target(BridgeTarget::Edit)?;
    let pid = bridge.studio_pid_for_runtime(BridgeTarget::Edit, &info.runtime_id)?;
    let title = crate::studio::native::serializer::target_name(pid, &info.place_name)?;
    let mut native = crate::studio::native::serializer::prepare_property(
        pid,
        &title,
        path,
        &[],
        property,
        Duration::from_secs(3),
    )?;
    let text = saved_field_text(value, &native.class_name, property)?;
    let before = native.read()?;
    if crate::automation::property_access::property_text_matches(
        &native.class_name,
        property,
        &text,
        &before,
    ) {
        return Ok(false);
    }
    native.ensure_writable()?;
    native.write(&text)?;
    Ok(true)
}

#[cfg(test)]
mod saved_field_text_tests {
    use super::*;

    #[test]
    fn integer_fields_get_integer_text_and_enums_their_name() {
        assert_eq!(
            saved_field_text(&json!(64.0), "Workspace", "StreamingMinRadius").unwrap(),
            "64"
        );
        assert_eq!(
            saved_field_text(&json!(0.7), "Terrain", "GrassLength").unwrap(),
            "0.7"
        );
        assert_eq!(
            saved_field_text(
                &json!({"_type": "EnumItem", "name": "Future"}),
                "Lighting",
                "Technology"
            )
            .unwrap(),
            "Future"
        );
        assert_eq!(
            saved_field_text(&json!(true), "Players", "BanningEnabled").unwrap(),
            "true"
        );
        assert!(saved_field_text(&json!([1, 2]), "Part", "Size").is_err());
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
pub(crate) fn write_saved_field(
    _bridge: &BridgeServer,
    _path: &[String],
    _property: &str,
    _value: &Value,
) -> Result<bool> {
    bail!("the native property writer is unavailable on this platform")
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
    let mut result = daemon_result(
        op::PROPERTY_ACCESS,
        None,
        parameters,
        false,
        Some(&args.bridge),
    )?;
    crate::app::output::strip_empty(&mut result);
    print_json_output(&result, false)?;
    if result["status"] == "partial" {
        bail!(
            "Function batch stopped; completed results and the unconfirmed call are shown above. Inspect before retrying"
        );
    }
    Ok(())
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
    let mut policy = state.property_access.lock_recover();
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
            Action::Call {
                target,
                function,
                arguments,
                ords,
            } => {
                let arguments = arguments
                    .as_array()
                    .context("Function arguments must be a JSON array")?
                    .clone();
                (target, function, ords, Operation::Call { arguments })
            }
            Action::Batch {
                target,
                function,
                arguments,
                ords,
            } => {
                let arguments = batch_arguments(&arguments)?;
                (target, function, ords, Operation::CallBatch { arguments })
            }
            _ => unreachable!("permission action handled before native invocation"),
        }
    };
    if property.is_empty() || property.len() > 256 {
        bail!("Property name must contain 1–256 bytes");
    }
    let path = target_parts(&target, &ordinals)?;
    // Native targeting needs the document caption on Windows and the bridge's
    // DataModel name on macOS, without adding macOS Accessibility permissions.
    let info = bridge.cached_bridge_info_for_target(BridgeTarget::Edit)?;
    anyhow::ensure!(
        info.runtime_id == scope.runtime,
        "Protected property runtime was replaced"
    );
    let title = crate::studio::native::serializer::target_name(scope.pid, &info.place_name)?;
    let mut native = crate::studio::native::serializer::prepare_property(
        scope.pid,
        &title,
        &path,
        &ordinals,
        if matches!(
            operation,
            Operation::Call { .. } | Operation::CallBatch { .. }
        ) {
            "Name"
        } else {
            &property
        },
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
    if let Operation::Call { arguments } = &intent.operation {
        native.prepare_function(scope.pid, &title, &intent.property, arguments)?;
    }
    if let Operation::CallBatch { arguments } = &intent.operation {
        for item in arguments {
            crate::studio::native::serializer::validate_function_arguments(
                &native.class_name,
                &intent.property,
                item,
            )?;
        }
        native.prepare_function(scope.pid, &title, &intent.property, &arguments[0])?;
    }
    if let Some(approved) = approved {
        if intent != approved {
            bail!("Property target changed since approval; request access again");
        }
    } else {
        let decision = state.property_access.lock_recover().check(scope, &intent)?;
        if matches!(decision, Decision::ApprovalRequired { .. }) {
            return Ok(serde_json::to_value(decision)?);
        }
    }
    // The pinned runtime must still be alive; never retarget an approved request.
    if bridge.studio_pid_for_runtime(BridgeTarget::Edit, &scope.runtime)? != scope.pid {
        bail!("Protected property runtime was replaced");
    }
    if matches!(intent.operation, Operation::Call { .. }) {
        let value = native.call_function(Duration::from_secs(30))?;
        return Ok(
            json!({"status":"applied","path":intent.path,"function":intent.property,
            "className":intent.class_name,"value":value,"runtimeId":scope.runtime}),
        );
    }
    if let Operation::CallBatch { arguments } = &intent.operation {
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut result = json!({"status":"applied","path":intent.path,"function":intent.property,
            "className":intent.class_name,"runtimeId":scope.runtime});
        result["results"] = ordered_function_batch(arguments, |arguments| {
            anyhow::ensure!(
                bridge.studio_pid_for_runtime(BridgeTarget::Edit, &scope.runtime)? == scope.pid,
                "Function batch runtime was replaced"
            );
            native.prepare_function(scope.pid, &title, &intent.property, arguments)?;
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .context("Function batch exceeded its 30-second deadline")?;
            native.call_function(remaining)
        });
        if result["results"]
            .as_array()
            .is_some_and(|items| items.iter().any(|item| item["status"] != "applied"))
        {
            result["status"] = json!("partial");
        }
        return Ok(result);
    }
    let before = native.read()?;
    let mut packages = Vec::new();
    let result = match &intent.operation {
        Operation::Call { .. } | Operation::CallBatch { .. } => {
            unreachable!("function handled above")
        }
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

    #[test]
    fn function_cli_accepts_one_typed_json_array() -> Result<()> {
        use clap::FromArgMatches;
        let matches = PropertyAccessArgs::augment_args(clap::Command::new("access"))
            .try_get_matches_from([
                "access",
                "call",
                "HttpRbxApiService",
                "GetAsyncFullUrl",
                r#"["https://apis.roblox.com/x",0,0]"#,
            ])?;
        let args = PropertyAccessArgs::from_arg_matches(&matches)?;
        assert_eq!(
            serde_json::to_value(args.action)?["arguments"],
            json!(["https://apis.roblox.com/x", 0, 0])
        );
        assert!(function_arguments("{}").is_err());
        Ok(())
    }

    #[test]
    fn batches_validate_before_execution_and_stop_without_replaying() {
        assert!(batch_arguments(&json!([])).is_err());
        assert!(batch_arguments(&json!([["one"], "two"])).is_err());
        assert!(batch_arguments(&json!(vec![vec!["x"]; 33])).is_err());
        let arguments = batch_arguments(&json!([[1], [2], [3]])).unwrap();
        let mut invoked = Vec::new();
        let results = ordered_function_batch(&arguments, |arguments| {
            invoked.push(arguments[0].clone());
            if arguments[0] == 2 {
                bail!("response lost")
            }
            Ok("first response".into())
        });
        assert_eq!(invoked, vec![json!(1), json!(2)]);
        assert_eq!(results[0]["value"], "first response");
        assert_eq!(results[1]["status"], "unconfirmed");
        assert_eq!(results[2]["status"], "not-executed");
    }
}
