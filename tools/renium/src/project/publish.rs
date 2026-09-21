use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use clap::Args;
use serde_json::{Value, json};

use super::{config, experience, workflows};
use crate::app;
use crate::automation::{BoundContext, commands::daemon_result, op};
use crate::cli::BridgeConnectionArgs;
use crate::cloud;
use crate::rbx::model::RbxPlaceFormat;
use crate::studio::bridge::{BridgeApplicationError, BridgeServer, BridgeTarget};
use crate::system::files::{absolutize_for_daemon, create_unique_directory};

const MAX_PLACE_BYTES: u64 = 100 * 1024 * 1024;
const PUBLISH_SECONDS: u64 = 120;

#[derive(Args)]
pub(crate) struct PublishArgs {
    #[arg(
        long,
        help = "Build and upload project files using an Open Cloud API key instead of Studio"
    )]
    open_cloud: bool,
    #[arg(
        long,
        requires = "open_cloud",
        value_name = "PLACE.rbxl|PLACE.rbxlx",
        help = "Upload this place file instead of building the project"
    )]
    file: Option<PathBuf>,
    #[arg(long, requires = "open_cloud", value_parser = clap::value_parser!(i64).range(1..))]
    universe: Option<i64>,
    #[arg(long, requires = "open_cloud", value_parser = clap::value_parser!(i64).range(1..))]
    place_id: Option<i64>,
    #[arg(
        long,
        requires = "open_cloud",
        value_name = "ENV",
        help = "API-key environment variable (default ROBLOX_API_KEY)"
    )]
    key_env: Option<String>,
    #[arg(
        long,
        help = "Validate and show the source and destination without publishing or checking cloud permissions"
    )]
    dry_run: bool,
    #[command(flatten)]
    bridge: BridgeConnectionArgs,
}

pub(crate) fn run(args: PublishArgs, project: Option<&Path>) -> Result<()> {
    let result = if args.open_cloud {
        open_cloud(&args, project)?
    } else {
        daemon_result(
            op::PLACE_PUBLISH,
            project,
            json!({ "dryRun": args.dry_run }),
            !args.dry_run,
            Some(&args.bridge),
        )?
    };
    app::output::print_json_output(&result, false)
}

pub(crate) fn studio_result(
    context: &BoundContext,
    parameters: &Value,
    bridge: &BridgeServer,
) -> Result<Value> {
    let runtime = context
        .runtime_id
        .as_deref()
        .context("No bound Edit runtime")?;
    if let Some(expected) = parameters.get("runtimeId").and_then(Value::as_str) {
        ensure!(
            expected == runtime,
            "Studio changed after publish review; review the selected place again"
        );
    }
    let result = bridge
        .call_for_runtime_with_timeout(
            "publishPlace",
            json!({
                "dryRun": parameters.get("dryRun").and_then(Value::as_bool).unwrap_or(false),
                "runtimeId": runtime,
                "gameId": context.game_id,
                "placeId": context.place_id,
            }),
            BridgeTarget::Edit,
            runtime,
            Some(Duration::from_secs(PUBLISH_SECONDS)),
        )
        .map_err(studio_failure)?;
    crate::app::output::ensure_plugin_api_ok(&result)?;
    Ok(result)
}

fn studio_failure(error: anyhow::Error) -> anyhow::Error {
    if let Some(application) = error.downcast_ref::<BridgeApplicationError>() {
        if application.message.contains("Unknown method: publishPlace") {
            return anyhow::anyhow!(
                "The selected Studio is running an older Renium plugin; update the plugin and reopen that Studio before publishing"
            );
        }
        return error;
    }
    error.context("Studio publish response was not confirmed. Check the place's Version History before trying again; the request is not automatically repeated")
}

fn selected_project(
    project: Option<&Path>,
    selector: Option<&str>,
) -> Result<Option<config::LoadedProject>> {
    let start = match project {
        Some(path) if path.is_file() => path.parent().unwrap_or(Path::new(".")).to_path_buf(),
        Some(path) => path.to_path_buf(),
        None => std::env::current_dir()?,
    };
    let place = experience::resolve_experience_place(&start, selector)?;
    match place {
        Some(place) => {
            let explicit = project
                .filter(|path| path.is_file() && path.parent() == Some(place.root.as_path()));
            config::try_load_project(explicit.or(Some(&place.root)), None)
        }
        None => config::try_load_project(project, None),
    }
}

fn open_cloud(args: &PublishArgs, project: Option<&Path>) -> Result<Value> {
    let loaded = if args.file.is_none() || args.universe.is_none() || args.place_id.is_none() {
        selected_project(project, app::context::place_selector().as_deref())?
    } else {
        None
    };
    if args.file.is_none() && loaded.is_none() {
        bail!("No Renium project found to build; select a place project or pass --file PLACE.rbxl");
    }
    let identity = cloud::command::discover_identity(
        loaded
            .as_ref()
            .map(|loaded| loaded.path.as_path())
            .or(project),
        args.universe,
        args.place_id,
    )?;
    let game_id = identity.game_id.context(
        "Publishing requires a universe ID; configure the experience or pass --universe ID",
    )?;
    let place_id = identity
        .place_id
        .context("Publishing requires a place ID; select --place ALIAS or pass --place-id ID")?;
    if !args.dry_run {
        cloud::CloudAuth::from_env(
            false,
            args.key_env.as_deref().unwrap_or("ROBLOX_API_KEY"),
            None,
            "publish",
        )
        .map_err(cloud::command::cloud_error)?;
    }
    let root = loaded
        .as_ref()
        .map(|loaded| loaded.root.clone())
        .or_else(|| {
            project.map(|path| {
                if path.is_dir() {
                    path.to_path_buf()
                } else {
                    path.parent().unwrap_or(Path::new(".")).to_path_buf()
                }
            })
        })
        .unwrap_or(std::env::current_dir()?);
    super::version_control::ensure_renium_local_state_ignored(&root)?;
    let directory = create_unique_directory(&root.join(".renium"), "publish-")?;
    let result = (|| {
        let (file, source) = if let Some(file) = &args.file {
            let format = RbxPlaceFormat::from_path(file)?;
            validate_size(fs::metadata(file)?.len())?;
            let snapshot = directory.join(format!("place.{}", format.label()));
            fs::copy(file, &snapshot)
                .with_context(|| format!("Could not snapshot {}", file.display()))?;
            (snapshot, absolutize_for_daemon(file))
        } else {
            let loaded = loaded.as_ref().expect("project checked above");
            let output = directory.join("place.rbxl");
            workflows::build_once(
                loaded,
                &workflows::BuildArgs {
                    output: Some(output.clone()),
                    project: Some(loaded.path.clone()),
                    watch: false,
                    sourcemap: false,
                    plugin: false,
                    target: None,
                    wally: workflows::ToolPolicy::Auto,
                    typescript: workflows::ToolPolicy::Auto,
                },
                &output,
                false,
                None,
            )?;
            (output, loaded.path.clone())
        };
        let bytes = validate_file(&file)?;
        let mut result = json!({
            "ok": true, "published": false, "dryRun": args.dry_run,
            "source": "open-cloud", "input": source, "gameId": game_id,
            "placeId": place_id, "bytes": bytes,
            "url": format!("https://www.roblox.com/games/{place_id}"),
        });
        if !args.dry_run {
            let response = cloud::execute_one(identity, args.key_env.as_deref().unwrap_or("ROBLOX_API_KEY"), None, false,
                cloud_request(&file))
                .map_err(cloud::command::cloud_error)
                .context("Publish was not confirmed. Check Version History before retrying; the upload is not automatically repeated")?;
            result["versionNumber"] = json!(published_version(&response)?);
            result["published"] = json!(true);
        }
        Ok(result)
    })();
    let cleanup = fs::remove_dir_all(&directory);
    if let Err(error) = cleanup {
        crate::log_global(
            2,
            format_args!(
                "Could not remove publish staging {}: {error}",
                directory.display()
            ),
        );
    }
    result
}

fn validate_size(bytes: u64) -> Result<()> {
    ensure!(
        bytes > 0 && bytes <= MAX_PLACE_BYTES,
        "Place file must be nonempty and no larger than 100 MiB"
    );
    Ok(())
}

fn validate_file(path: &Path) -> Result<u64> {
    let bytes = fs::metadata(path)?.len();
    validate_size(bytes)?;
    let dom = RbxPlaceFormat::from_path(path)?.read(path)?;
    let database = rbx_reflection_database::get()?;
    let mut unsupported = BTreeMap::<String, usize>::new();
    for instance in dom.descendants() {
        let mut class = instance.class.as_str();
        loop {
            if matches!(
                class,
                "EditableImage"
                    | "EditableMesh"
                    | "PartOperation"
                    | "SurfaceAppearance"
                    | "BaseWrap"
            ) {
                *unsupported.entry(instance.class.to_string()).or_default() += 1;
                break;
            }
            let Some(parent) = database
                .classes
                .get(class)
                .and_then(|descriptor| descriptor.superclass)
            else {
                break;
            };
            class = parent;
        }
    }
    ensure!(
        unsupported.is_empty(),
        "Open Cloud cannot reliably update these instances: {}. Publish from Studio instead",
        unsupported
            .iter()
            .map(|(class, count)| format!("{class} ({count})"))
            .collect::<Vec<_>>()
            .join(", ")
    );
    Ok(bytes)
}

fn cloud_request(file: &Path) -> Value {
    json!({
        "method": "POST", "path": "/universes/v1/{universe}/places/{place}/versions",
        "query": { "versionType": "Published" }, "rawFile": file,
        "contentType": if file.extension().is_some_and(|ext| ext == "rbxlx") { "application/xml" } else { "application/octet-stream" },
        "timeoutSeconds": PUBLISH_SECONDS,
    })
}

fn published_version(response: &Value) -> Result<u64> {
    response.pointer("/body/versionNumber").and_then(|value| value.as_u64().or_else(|| value.as_str()?.parse().ok()))
        .filter(|version| *version > 0)
        .context("Roblox did not return a published version number. Check Version History before retrying")
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use rbx_dom_weak::{InstanceBuilder, WeakDom};

    fn args(arguments: &[&str]) -> PublishArgs {
        let command = crate::cli::Cli::try_parse_from(arguments).unwrap().command;
        let crate::cli::Commands::Publish(args) = command else {
            panic!("publish not parsed")
        };
        args
    }

    fn write_place(path: &Path, classes: &[&str]) {
        let mut dom = WeakDom::new(InstanceBuilder::new("DataModel"));
        let service = dom.insert(dom.root_ref(), InstanceBuilder::new("Workspace"));
        for class in classes {
            dom.insert(service, InstanceBuilder::new(*class));
        }
        let file = fs::File::create(path).unwrap();
        if path.extension().is_some_and(|ext| ext == "rbxlx") {
            rbx_xml::to_writer_default(file, &dom, &[service]).unwrap();
        } else {
            rbx_binary::to_writer(file, &dom, &[service]).unwrap();
        }
    }

    #[test]
    fn publish_defaults_to_studio_and_requires_explicit_cloud_options() {
        assert!(!args(&["rbx", "publish"]).open_cloud);
        assert!(args(&["rbx", "publish", "--dry-run"]).dry_run);
        assert!(args(&["rbx", "publish", "--open-cloud"]).open_cloud);
        for options in [
            vec!["--file", "place.rbxl"],
            vec!["--universe", "123"],
            vec!["--key-env", "KEY"],
            vec!["--place-id", "456"],
        ] {
            let mut input = vec!["rbx", "publish"];
            input.extend(options);
            assert!(crate::cli::Cli::try_parse_from(input).is_err());
        }
        assert!(
            crate::cli::Cli::try_parse_from(["rbx", "publish", "--open-cloud", "--place-id", "0"])
                .is_err()
        );
    }

    #[test]
    fn publish_upload_shape_and_completion_are_explicit() {
        for (extension, mime) in [
            ("rbxl", "application/octet-stream"),
            ("rbxlx", "application/xml"),
        ] {
            let file = PathBuf::from(format!("place.{extension}"));
            let request = cloud_request(&file);
            assert_eq!(request["method"], "POST");
            assert_eq!(request["query"]["versionType"], "Published");
            assert_eq!(request["rawFile"], json!(file));
            assert_eq!(request["contentType"], mime);
        }
        assert_eq!(
            published_version(&json!({"body":{"versionNumber":12}})).unwrap(),
            12
        );
        assert_eq!(
            published_version(&json!({"body":{"versionNumber":"13"}})).unwrap(),
            13
        );
        for response in [
            json!({}),
            json!({"body":{"versionNumber":0}}),
            json!({"body":{"versionNumber":-1}}),
        ] {
            assert!(published_version(&response).is_err());
        }
        assert!(validate_size(0).is_err());
        assert!(validate_size(MAX_PLACE_BYTES + 1).is_err());
        assert!(validate_size(MAX_PLACE_BYTES).is_ok());
        let rejected = studio_failure(
            BridgeApplicationError {
                method: "publishPlace".into(),
                message: "This Studio place is unpublished".into(),
            }
            .into(),
        );
        assert!(!rejected.to_string().contains("Version History"));
        let old = studio_failure(
            BridgeApplicationError {
                method: "publishPlace".into(),
                message: "Unknown method: publishPlace".into(),
            }
            .into(),
        );
        assert!(old.to_string().contains("reopen"));
        assert!(
            studio_failure(anyhow::anyhow!("connection closed"))
                .to_string()
                .contains("Version History")
        );
    }

    #[test]
    fn publish_cloud_validates_binary_xml_and_unsupported_subclasses() {
        let root = crate::tests::support::temp_dir("publish-fidelity");
        for extension in ["rbxl", "rbxlx"] {
            let file = root.join(format!("place.{extension}"));
            write_place(&file, &["Part"]);
            assert!(validate_file(&file).is_ok());
            for class in [
                "SurfaceAppearance",
                "UnionOperation",
                "WrapLayer",
                "WrapTarget",
            ] {
                write_place(&file, &[class]);
                let error = validate_file(&file).unwrap_err().to_string();
                assert!(error.contains(class), "{error}");
            }
            fs::write(&file, "not a place").unwrap();
            assert!(validate_file(&file).is_err());
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn publish_cloud_dry_run_needs_no_credentials_and_removes_staging() {
        let root = crate::tests::support::temp_dir("publish-dry-run");
        let project = root.join("renium.project.jsonc");
        fs::write(
            &project,
            r#"{"schemaVersion":1,"name":"Publish test","sourceRoot":"src"}"#,
        )
        .unwrap();
        fs::create_dir(root.join("src")).unwrap();
        crate::bytecode::ensure_service_store_exists(
            &root.join("instances/Workspace.renium"),
            "Workspace",
        )
        .unwrap();
        let mut options = args(&[
            "rbx",
            "publish",
            "--open-cloud",
            "--dry-run",
            "--universe",
            "123",
            "--place-id",
            "456",
            "--key-env",
            "RENIUM_PUBLISH_TEST_UNSET",
        ]);
        let result = open_cloud(&options, Some(&project)).unwrap();
        assert_eq!(result["published"], false);
        assert_eq!(result["placeId"], 456);
        let file = root.join("place.rbxl");
        write_place(&file, &["Part"]);
        options.file = Some(file.clone());
        assert!(
            !open_cloud(&options, Some(&project)).unwrap()["published"]
                .as_bool()
                .unwrap()
        );
        write_place(&file, &["UnionOperation"]);
        assert!(
            open_cloud(&options, Some(&project))
                .unwrap_err()
                .to_string()
                .contains("UnionOperation")
        );
        assert!(!fs::read_dir(root.join(".renium")).unwrap().any(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("publish-")
        }));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn publish_selects_one_place_and_never_builds_the_experience_root() {
        let root = crate::tests::support::temp_dir("publish-places");
        fs::write(root.join("renium.experience.json"), r#"{"gameId":123,"places":{"lobby":{"placeId":456,"root":"places/lobby"},"race":{"placeId":789,"root":"places/race"}}}"#).unwrap();
        for name in ["lobby", "race"] {
            let place = root.join("places").join(name);
            fs::create_dir_all(place.join("src")).unwrap();
            fs::write(
                place.join("renium.project.jsonc"),
                format!(r#"{{"schemaVersion":1,"name":"{name}","sourceRoot":"src"}}"#),
            )
            .unwrap();
        }
        assert!(selected_project(Some(&root), None).is_err());
        for selector in ["race", "789", "123:789"] {
            let project = selected_project(Some(&root), Some(selector))
                .unwrap()
                .unwrap();
            assert_eq!(project.project.name.as_deref(), Some("race"));
        }
        assert!(selected_project(Some(&root), Some("unknown")).is_err());
        let race = root.join("places/race");
        assert_eq!(
            selected_project(Some(&race), None)
                .unwrap()
                .unwrap()
                .project
                .name
                .as_deref(),
            Some("race")
        );
        fs::remove_dir_all(root).unwrap();
    }
}
