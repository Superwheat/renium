use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::ffi::OsStr;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand, ValueEnum};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use walkdir::WalkDir;

use crate::project_config::{self, LoadedProject, PROJECT_FILE_NAME};

const PROJECT_SCHEMA: &str = include_str!("../schemas/renium.project.schema.json");
const CLI_DOCS: &str = include_str!("../README.md");
const AGENT_INSTRUCTIONS: &str = include_str!("../../renium-vscode-extension/resources/AGENTS.md");
const CLAUDE_INSTRUCTIONS: &str = include_str!("../../renium-vscode-extension/resources/CLAUDE.md");

#[derive(Args, Debug)]
pub struct InitArgs {
    #[arg(value_name = "PATH", default_value = ".")]
    pub path: PathBuf,
    #[arg(long, value_delimiter = ',', value_name = "FEATURE")]
    pub with: Vec<InitFeature>,
    #[arg(long, action = clap::ArgAction::SetTrue)]
    pub preview: bool,
}

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum InitFeature {
    Git,
    Wally,
    Selene,
    Docs,
    RobloxTs,
}

#[derive(Args, Debug, Clone)]
pub struct BuildArgs {
    #[arg(short, long, value_name = "PATH")]
    pub output: Option<PathBuf>,
    #[arg(long, value_name = "PROJECT")]
    pub project: Option<PathBuf>,
    #[arg(long, action = clap::ArgAction::SetTrue)]
    pub watch: bool,
    #[arg(long, action = clap::ArgAction::SetTrue)]
    pub sourcemap: bool,
    #[arg(long, action = clap::ArgAction::SetTrue)]
    pub plugin: bool,
    #[arg(long, value_name = "SERVICE.INSTANCE")]
    pub target: Option<String>,
    #[arg(long, value_enum, default_value_t = ToolPolicy::Auto)]
    pub wally: ToolPolicy,
    #[arg(long = "ts", value_enum, default_value_t = ToolPolicy::Auto)]
    pub typescript: ToolPolicy,
}

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolPolicy {
    Auto,
    Always,
    Never,
}

#[derive(Args, Debug)]
pub struct DoctorArgs {
    #[arg(long, default_value = ".")]
    pub root: PathBuf,
    #[arg(long, value_name = "PROJECT")]
    pub project: Option<PathBuf>,
    #[arg(long, action = clap::ArgAction::SetTrue)]
    pub json: bool,
    #[arg(long, value_name = "PATH")]
    pub bundle: Option<PathBuf>,
}

#[derive(Args, Debug)]
pub struct DocsArgs {
    pub topic: Option<String>,
    #[arg(long, action = clap::ArgAction::SetTrue)]
    pub serve: bool,
    #[arg(long, default_value_t = 0)]
    pub port: u16,
}

#[derive(Args, Debug)]
pub struct DaemonArgs {
    #[command(subcommand)]
    pub command: DaemonCommand,
}

#[derive(Subcommand, Debug)]
pub enum DaemonCommand {
    List,
    Status(DaemonTargetArgs),
    Stop(DaemonTargetArgs),
    Clean,
}

#[derive(Args, Debug)]
pub struct DaemonTargetArgs {
    #[arg(default_value = "default")]
    pub name: String,
    #[arg(long, action = clap::ArgAction::SetTrue)]
    pub all: bool,
    #[arg(long, action = clap::ArgAction::SetTrue)]
    pub force: bool,
}

#[derive(Args, Debug)]
pub struct StudioArgs {
    #[arg(value_name = "FILE")]
    pub file: Option<PathBuf>,
    #[arg(long, value_name = "PROJECT")]
    pub project: Option<PathBuf>,
    #[arg(long, action = clap::ArgAction::SetTrue)]
    pub check: bool,
}

#[derive(Args, Debug)]
pub struct UploadArgs {
    #[arg(short, long, value_name = "PATH")]
    pub input: Option<PathBuf>,
    #[arg(long)]
    pub place_id: Option<u64>,
    #[arg(long)]
    pub universe_id: Option<u64>,
    #[arg(long, default_value = "ROBLOX_API_KEY")]
    pub api_key_env: String,
    #[arg(long, value_name = "PROJECT")]
    pub project: Option<PathBuf>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct InitPlan {
    root: PathBuf,
    create: Vec<String>,
    keep: Vec<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct DoctorCheck {
    name: String,
    status: &'static str,
    detail: String,
    action: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DaemonDiscovery {
    #[serde(default = "default_daemon_schema")]
    schema_version: u32,
    #[serde(default = "default_daemon_name")]
    name: String,
    host: String,
    control_port: u16,
    #[serde(default)]
    bridge_ports: Vec<u16>,
    pid: u32,
    updated_unix_ms: u128,
}

fn default_daemon_schema() -> u32 {
    1
}

fn default_daemon_name() -> String {
    "default".to_string()
}

pub fn run_init(args: InitArgs) -> Result<()> {
    let root = absolute_path(&args.path);
    let source_root = detect_init_source_root(&root)?;
    let files = init_files(&root, &source_root, &args.with)?;
    let mut create = Vec::new();
    let mut keep = Vec::new();
    for (path, _) in &files {
        if path.exists() {
            keep.push(relative_display(&root, path));
        } else {
            create.push(relative_display(&root, path));
        }
    }
    let plan = InitPlan {
        root: root.clone(),
        create,
        keep,
    };
    if args.preview {
        return crate::emit_global_output(
            &serde_json::to_value(&plan)?,
            &format!(
                "Would initialize {}: create {}, keep {}",
                plan.root.display(),
                plan.create.len(),
                plan.keep.len()
            ),
        );
    }
    let root_existed = root.exists();
    let git_was_absent = !root.join(".git").exists();
    let node_modules_was_absent = !root.join("node_modules").exists();
    let package_locks = [
        "package-lock.json",
        "pnpm-lock.yaml",
        "yarn.lock",
        "bun.lock",
        "bun.lockb",
    ]
    .into_iter()
    .map(|name| {
        let path = root.join(name);
        let original = if path.is_file() {
            Some(fs::read(&path)?)
        } else {
            None
        };
        Ok((path, original))
    })
    .collect::<Result<Vec<_>>>()?;
    let mut created_files = Vec::new();
    let mut created_directories = Vec::new();
    let result = (|| -> Result<()> {
        if !root_existed {
            fs::create_dir_all(&root)
                .with_context(|| format!("Failed to create {}", root.display()))?;
            created_directories.push(root.clone());
        }
        for (path, bytes) in files {
            if path.exists() {
                continue;
            }
            if let Some(parent) = path.parent() {
                create_directories_tracked(parent, &mut created_directories)?;
            }
            atomic_write(&path, &bytes)?;
            created_files.push(path);
        }
        for directory in init_directories(&root, &source_root) {
            if directory.exists() {
                continue;
            }
            create_directories_tracked(&directory, &mut created_directories)?;
        }
        if args.with.contains(&InitFeature::Git) && !root.join(".git").exists() {
            run_checked(
                Command::new("git").arg("init").current_dir(&root),
                "git init",
            )?;
        }
        if args.with.contains(&InitFeature::RobloxTs) {
            validate_roblox_ts_project(&root)?;
            let package_manager = project_package_manager(&root)?;
            run_checked(
                Command::new(&package_manager)
                    .arg("install")
                    .current_dir(&root),
                &format!(
                    "{} install",
                    package_manager
                        .file_stem()
                        .and_then(OsStr::to_str)
                        .unwrap_or("package manager")
                ),
            )?;
        }
        Ok(())
    })();
    if let Err(error) = result {
        if git_was_absent {
            let _ = fs::remove_dir_all(root.join(".git"));
        }
        if node_modules_was_absent {
            let _ = fs::remove_dir_all(root.join("node_modules"));
        }
        for (path, original) in package_locks {
            if let Some(original) = original {
                let _ = atomic_write(&path, &original);
            } else {
                let _ = fs::remove_file(path);
            }
        }
        for path in created_files.into_iter().rev() {
            let _ = fs::remove_file(path);
        }
        created_directories.sort_by_key(|path| std::cmp::Reverse(path.components().count()));
        for path in created_directories {
            let _ = fs::remove_dir(path);
        }
        return Err(error);
    }
    crate::emit_global_output(
        &serde_json::to_value(&plan)?,
        &format!(
            "Initialized {}: created {}, kept {}",
            plan.root.display(),
            plan.create.len(),
            plan.keep.len()
        ),
    )
}

fn project_package_manager(root: &Path) -> Result<PathBuf> {
    let locked = [
        ("pnpm-lock.yaml", "pnpm"),
        ("yarn.lock", "yarn"),
        ("bun.lock", "bun"),
        ("bun.lockb", "bun"),
        ("package-lock.json", "npm"),
    ]
    .into_iter()
    .find_map(|(lock, command)| root.join(lock).is_file().then_some(command));
    let requested = locked.map(str::to_string).or_else(|| {
        env::var("npm_config_user_agent")
            .ok()
            .and_then(|agent| agent.split('/').next().map(str::to_string))
            .filter(|manager| matches!(manager.as_str(), "npm" | "pnpm" | "yarn" | "bun"))
    });
    if let Some(manager) = requested {
        return find_command(&manager)
            .with_context(|| format!("{manager} is selected for this project but is not on PATH"));
    }
    ["npm", "pnpm", "yarn", "bun"]
        .into_iter()
        .find_map(find_command)
        .context("No supported package manager was found; install npm, pnpm, Yarn, or Bun")
}

fn validate_roblox_ts_project(root: &Path) -> Result<()> {
    let package = root.join("package.json");
    if !package_uses_roblox_ts(&package)? {
        bail!(
            "{} must declare roblox-ts in dependencies/devDependencies or expose an rbxtsc script before initialization can continue",
            package.display()
        );
    }
    let tsconfig = root.join("tsconfig.json");
    let value: Value = serde_json::from_slice(&fs::read(&tsconfig)?)
        .with_context(|| format!("Invalid {}", tsconfig.display()))?;
    if !value.is_object() {
        bail!("{} must contain a JSON object", tsconfig.display());
    }
    Ok(())
}

fn create_directories_tracked(path: &Path, created: &mut Vec<PathBuf>) -> Result<()> {
    let mut missing = Vec::new();
    let mut current = path;
    while !current.exists() {
        missing.push(current.to_path_buf());
        current = current
            .parent()
            .with_context(|| format!("{} has no existing parent", path.display()))?;
    }
    for directory in missing.into_iter().rev() {
        fs::create_dir(&directory)
            .with_context(|| format!("Failed to create {}", directory.display()))?;
        created.push(directory);
    }
    Ok(())
}

pub fn run_build(args: BuildArgs, global_project: Option<&Path>) -> Result<()> {
    crate::set_global_stream_output(args.watch);
    let mut loaded =
        project_config::load_project(args.project.as_deref().or(global_project), None)?;
    let output = args.output.clone().unwrap_or_else(|| {
        let name = loaded
            .project
            .name
            .as_deref()
            .map(safe_file_stem)
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| "place".to_string());
        let model = args.plugin || args.target.is_some() || loaded.project.build_target.is_some();
        loaded
            .root
            .join("build")
            .join(format!("{name}.{}", if model { "rbxm" } else { "rbxl" }))
    });
    let output_extension = output
        .extension()
        .and_then(OsStr::to_str)
        .unwrap_or_default()
        .to_ascii_lowercase();
    if args.plugin && !matches!(output_extension.as_str(), "rbxm" | "rbxmx") {
        bail!("--plugin requires an .rbxm or .rbxmx output");
    }
    if !args.watch {
        build_once(&loaded, &args, &output, true, None)?;
        return Ok(());
    }
    crate::log_global(3, format_args!("Watching {}", loaded.root.display()));
    watch_build(&mut loaded, &args, &output)
}

pub fn run_doctor(args: DoctorArgs, global_project: Option<&Path>) -> Result<()> {
    let root = absolute_path(&args.root);
    let mut checks = Vec::new();
    match project_config::load_project(args.project.as_deref().or(global_project), Some(&root)) {
        Ok(project) => match project_config::validate_project(&project)
            .and_then(|()| project_config::stage_project(&project).map(drop))
        {
            Ok(()) => checks.push(DoctorCheck {
                name: "project".to_string(),
                status: "ok",
                detail: format!("{} compiles successfully", project.path.display()),
                action: None,
            }),
            Err(error) => checks.push(DoctorCheck {
                name: "project".to_string(),
                status: "error",
                detail: error.to_string(),
                action: Some(
                    "Fix the reported project path or field, then run `rbx doctor` again"
                        .to_string(),
                ),
            }),
        },
        Err(error) => checks.push(DoctorCheck {
            name: "project".to_string(),
            status: if args.project.is_some()
                || global_project.is_some()
                || project_marker_exists_from(&root)
            {
                "error"
            } else {
                "warn"
            },
            detail: error.to_string(),
            action: Some(format!(
                "Run `rbx init {}` or pass --project",
                root.display()
            )),
        }),
    }
    let config = project_config::load_merged_config(&root);
    checks.push(match config {
        Ok(_) => DoctorCheck {
            name: "configuration".to_string(),
            status: "ok",
            detail: "Configuration scopes merge successfully".to_string(),
            action: None,
        },
        Err(error) => DoctorCheck {
            name: "configuration".to_string(),
            status: "error",
            detail: error.to_string(),
            action: Some("Run `rbx config list --origins` to locate the invalid file".to_string()),
        },
    });
    for tool in ["git", "wally", "lune", "node", "rbxtsc"] {
        checks.push(match find_command(tool) {
            Some(path) => DoctorCheck {
                name: format!("tool:{tool}"),
                status: "ok",
                detail: path.display().to_string(),
                action: None,
            },
            None => DoctorCheck {
                name: format!("tool:{tool}"),
                status: if matches!(tool, "git" | "node") {
                    "warn"
                } else {
                    "optional"
                },
                detail: "Not found on PATH".to_string(),
                action: Some(tool_install_action(tool)),
            },
        });
    }
    let plugin = crate::roblox_plugins_dir().map(|dir| dir.join(crate::PLUGIN_ASSET_NAME));
    checks.push(match plugin {
        Ok(path) if path.is_file() => match fs::read(&path)
            .with_context(|| format!("Failed to read {}", path.display()))
            .and_then(|bytes| crate::validate_rbxm(&bytes))
        {
            Ok(()) => DoctorCheck {
                name: "studioPlugin".to_string(),
                status: "ok",
                detail: format!("{} matches Renium {}", path.display(), crate::BUILD_VERSION),
                action: None,
            },
            Err(error) => DoctorCheck {
                name: "studioPlugin".to_string(),
                status: "error",
                detail: error.to_string(),
                action: Some("Run `rbx setup` to repair the Studio plugin".to_string()),
            },
        },
        Ok(path) => DoctorCheck {
            name: "studioPlugin".to_string(),
            status: "warn",
            detail: format!("Not installed at {}", path.display()),
            action: Some("Run `rbx setup`".to_string()),
        },
        Err(error) => DoctorCheck {
            name: "studioPlugin".to_string(),
            status: "optional",
            detail: error.to_string(),
            action: None,
        },
    });
    let daemons = read_daemon_discoveries()?;
    let live_count = daemons
        .iter()
        .filter(|(_, daemon)| crate::is_process_alive(daemon.pid))
        .count();
    checks.push(DoctorCheck {
        name: "daemon".to_string(),
        status: if live_count > 0 { "ok" } else { "warn" },
        detail: format!("{live_count} running, {} discovery file(s)", daemons.len()),
        action: (live_count == 0).then(|| "Run `rbx bd` or start Renium in VS Code".to_string()),
    });
    let result = json!({
        "ok": checks.iter().all(|check| check.status != "error"),
        "version": crate::BUILD_VERSION,
        "gitHash": crate::BUILD_GIT_HASH,
        "root": root,
        "checks": checks,
    });
    if let Some(bundle) = args.bundle {
        write_doctor_bundle(&bundle, &result, &root)?;
    }
    if args.json {
        crate::emit_global_output(&result, &serde_json::to_string_pretty(&result)?)?;
    } else {
        let text = checks
            .iter()
            .flat_map(|check| {
                let mut lines = vec![format!(
                    "{:<10} {:<18} {}",
                    check.status, check.name, check.detail
                )];
                if let Some(action) = &check.action {
                    lines.push(format!("{:<10} {:<18} {}", "", "", action));
                }
                lines
            })
            .collect::<Vec<_>>()
            .join("\n");
        crate::emit_global_output(&result, &text)?;
    }
    if checks.iter().any(|check| check.status == "error") {
        bail!("Renium doctor found errors");
    }
    Ok(())
}

fn project_marker_exists_from(start: &Path) -> bool {
    let mut current = if start.is_file() {
        start
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf()
    } else {
        start.to_path_buf()
    };
    loop {
        if current.join(PROJECT_FILE_NAME).is_file()
            || current
                .join(project_config::PROJECT_JSON_FILE_NAME)
                .is_file()
        {
            return true;
        }
        if !current.pop() {
            return false;
        }
    }
}

pub fn run_docs(args: DocsArgs) -> Result<()> {
    let text = docs_topic(args.topic.as_deref())?;
    if !args.serve {
        return crate::emit_global_output(
            &json!({
                "ok": true,
                "topic": args.topic,
                "text": text,
            }),
            &text,
        );
    }
    let listener = TcpListener::bind(("127.0.0.1", args.port))
        .context("Failed to start the documentation server")?;
    let address = listener.local_addr()?;
    println!("Renium docs: http://{address}");
    let html = format!(
        "<!doctype html><meta charset=utf-8><title>Renium docs</title><style>body{{font:15px/1.55 system-ui;max-width:1000px;margin:3rem auto;padding:0 1rem;color:#ddd;background:#181818}}pre{{white-space:pre-wrap}}</style><pre>{}</pre>",
        html_escape(&text)
    );
    for stream in listener.incoming() {
        let mut stream = stream?;
        let mut request = [0u8; 4096];
        let _ = stream.read(&mut request);
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            html.len(),
            html
        );
        stream.write_all(response.as_bytes())?;
    }
    Ok(())
}

pub fn run_daemon(args: DaemonArgs) -> Result<()> {
    match args.command {
        DaemonCommand::List => {
            let values = daemon_status_values(None)?;
            let text = daemon_status_text(&values);
            crate::emit_global_output(&json!({ "daemons": values }), &text)
        }
        DaemonCommand::Status(target) => {
            let values = daemon_status_values((!target.all).then_some(target.name.as_str()))?;
            if target.all {
                let text = daemon_status_text(&values);
                return crate::emit_global_output(&json!({ "daemons": values }), &text);
            }
            let Some(value) = values.into_iter().next() else {
                bail!("No daemon named '{}' was found", target.name);
            };
            let text = daemon_status_text(std::slice::from_ref(&value));
            crate::emit_global_output(&value, &text)
        }
        DaemonCommand::Stop(target) => {
            if target.all {
                stop_all_daemons(target.force)
            } else {
                let detail = stop_named_daemon(&target.name, target.force)?;
                crate::emit_global_output(
                    &json!({
                        "ok": true,
                        "stopped": [target.name],
                        "detail": detail,
                    }),
                    &detail,
                )
            }
        }
        DaemonCommand::Clean => {
            let mut removed = Vec::new();
            for (path, daemon) in read_daemon_discoveries()? {
                if !crate::is_process_alive(daemon.pid) {
                    fs::remove_file(&path)
                        .with_context(|| format!("Failed to remove {}", path.display()))?;
                    removed.push(path);
                }
            }
            crate::emit_global_output(
                &json!({ "ok": true, "removed": removed }),
                &format!("Removed {} stale daemon discovery file(s)", removed.len()),
            )
        }
    }
}

fn daemon_status_text(values: &[Value]) -> String {
    if values.is_empty() {
        return "No Renium daemons found".to_string();
    }
    values
        .iter()
        .map(|value| {
            format!(
                "{} pid={} alive={} responsive={} {}:{}",
                value
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown"),
                value.get("pid").and_then(Value::as_u64).unwrap_or(0),
                value.get("alive").and_then(Value::as_bool).unwrap_or(false),
                value
                    .get("responsive")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                value
                    .get("host")
                    .and_then(Value::as_str)
                    .unwrap_or("127.0.0.1"),
                value
                    .get("controlPort")
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn run_studio(args: StudioArgs, global_project: Option<&Path>) -> Result<()> {
    let project = args.project.as_deref().or(global_project);
    if !args.check {
        let result = launch_studio(args.file.as_deref(), project)?;
        let file = result
            .get("file")
            .and_then(Value::as_str)
            .context("Studio launch result omitted the file")?;
        return crate::emit_global_output(&result, &format!("Opened {file} in Studio"));
    }
    let executable = studio_executable()?;
    let file = resolve_studio_file(args.file.as_deref(), project, false)?;
    let Some(file) = file else {
        let result = json!({
            "ok": false,
            "resolvable": false,
            "executable": executable,
            "file": Value::Null,
        });
        crate::emit_global_output(
            &result,
            "Studio is installed, but no Studio file can be resolved",
        )?;
        bail!("No Studio file exists and --check does not build one");
    };
    let result = json!({
        "ok": true,
        "executable": executable,
        "file": file,
        "exists": file.is_file(),
    });
    crate::emit_global_output(
        &result,
        &format!("Studio is available at {}", executable.display()),
    )
}

pub fn launch_studio(file: Option<&Path>, project: Option<&Path>) -> Result<Value> {
    let executable = studio_executable()?;
    let file = resolve_studio_file(file, project, true)?
        .context("No Studio file exists and one could not be built")?;
    Command::new(&executable)
        .arg(&file)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("Failed to launch {}", executable.display()))?;
    Ok(json!({
        "ok": true,
        "executable": executable,
        "file": file,
        "exists": file.is_file(),
    }))
}

pub fn run_upload(args: UploadArgs, global_project: Option<&Path>) -> Result<()> {
    let place_id = args
        .place_id
        .context("--place-id is required for Open Cloud place publishing")?;
    let universe_id = args
        .universe_id
        .context("--universe-id is required for Open Cloud place publishing")?;
    let api_key =
        env::var(&args.api_key_env).with_context(|| format!("{} is not set", args.api_key_env))?;
    if api_key.trim().is_empty() {
        bail!("{} is empty", args.api_key_env);
    }
    let loaded = project_config::load_project(args.project.as_deref().or(global_project), None)?;
    validate_experience_upload(&loaded.root, universe_id, place_id)?;
    let temporary = loaded
        .root
        .join(".renium/build")
        .join(format!("upload-{place_id}.rbxl"));
    let input = if let Some(input) = args.input {
        let input = absolute_path(&input);
        if !input.is_file() {
            bail!("Upload input does not exist: {}", input.display());
        }
        input
    } else {
        build_once(
            &loaded,
            &BuildArgs {
                output: Some(temporary.clone()),
                project: Some(loaded.path.clone()),
                watch: false,
                sourcemap: true,
                plugin: false,
                target: None,
                wally: ToolPolicy::Auto,
                typescript: ToolPolicy::Auto,
            },
            &temporary,
            false,
            None,
        )?;
        temporary.clone()
    };
    let content_type = match input.extension().and_then(OsStr::to_str) {
        Some("rbxl") => "application/octet-stream",
        Some("rbxlx") => "application/xml",
        _ => bail!("Place upload input must end in .rbxl or .rbxlx"),
    };
    let url = format!(
        "https://apis.roblox.com/universes/v1/{universe_id}/places/{place_id}/versions?versionType=Published"
    );
    let header_file =
        env::temp_dir().join(format!("renium-upload-header-{}.txt", std::process::id()));
    fs::write(
        &header_file,
        format!(
            "x-api-key: {}\nContent-Type: {content_type}\n",
            api_key.trim()
        ),
    )?;
    let output = Command::new("curl")
        .args(["-fsSL", "--request", "POST", "--header"])
        .arg(format!("@{}", header_file.display()))
        .arg("--data-binary")
        .arg(format!("@{}", input.display()))
        .arg(&url)
        .output();
    let _ = fs::remove_file(&header_file);
    if input == temporary {
        let _ = fs::remove_file(&temporary);
    }
    let output = output.context("Failed to run curl")?;
    if !output.status.success() {
        bail!(
            "Open Cloud place publishing failed with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let response: Value = serde_json::from_slice(&output.stdout)
        .context("Open Cloud returned an invalid JSON response")?;
    crate::emit_global_output(
        &json!({
            "ok": true,
            "universeId": universe_id,
            "placeId": place_id,
            "input": input,
            "response": response,
        }),
        &format!("Uploaded place {place_id} in universe {universe_id}"),
    )
}

fn detect_init_source_root(root: &Path) -> Result<PathBuf> {
    for name in [PROJECT_FILE_NAME, project_config::PROJECT_JSON_FILE_NAME] {
        let path = root.join(name);
        if path.is_file() {
            return Ok(project_config::load_project(Some(&path), None)?
                .project
                .source_root);
        }
    }
    let mut rojo_projects = if root.is_dir() {
        fs::read_dir(root)?
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| {
                path.file_name()
                    .and_then(OsStr::to_str)
                    .is_some_and(|name| name.ends_with(".project.json"))
            })
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    rojo_projects.sort();
    for project in rojo_projects {
        let value: Value = serde_json::from_slice(&fs::read(&project)?)
            .with_context(|| format!("Invalid Rojo project {}", project.display()))?;
        let mut roots = BTreeSet::new();
        collect_rojo_source_roots(&value, &mut roots);
        if roots.len() == 1 {
            return Ok(PathBuf::from(
                roots
                    .into_iter()
                    .next()
                    .unwrap_or_else(|| "src".to_string()),
            ));
        }
    }
    for candidate in ["src", "source", "Source", "game"] {
        if root.join(candidate).is_dir() {
            return Ok(PathBuf::from(candidate));
        }
    }
    Ok(PathBuf::from("src"))
}

fn collect_rojo_source_roots(value: &Value, roots: &mut BTreeSet<String>) {
    match value {
        Value::Array(values) => {
            for value in values {
                collect_rojo_source_roots(value, roots);
            }
        }
        Value::Object(object) => {
            if let Some(path) = object.get("$path").and_then(Value::as_str)
                && let Some(component) =
                    Path::new(path)
                        .components()
                        .find_map(|component| match component {
                            std::path::Component::Normal(value) => {
                                value.to_str().map(str::to_string)
                            }
                            _ => None,
                        })
            {
                roots.insert(component);
            }
            for value in object.values() {
                collect_rojo_source_roots(value, roots);
            }
        }
        _ => {}
    }
}

fn path_text(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn init_files(
    root: &Path,
    source_root: &Path,
    features: &[InitFeature],
) -> Result<Vec<(PathBuf, Vec<u8>)>> {
    let name = root
        .file_name()
        .and_then(OsStr::to_str)
        .filter(|name| !name.is_empty())
        .unwrap_or("Renium project");
    let project = json!({
        "$schema": "https://raw.githubusercontent.com/Superwheat/renium/main/tools/renium/schemas/renium.project.schema.json",
        "schemaVersion": 1,
        "name": name,
        "sourceRoot": path_text(source_root),
        "tree": {},
        "scriptExtension": "preserve",
        "exportNaming": {
            "serverSuffix": ".server",
            "clientSuffix": ".client",
            "moduleSuffix": "",
            "pluginSuffix": ".plugin",
            "clientRunContextSuffix": ".run-client"
        }
    });
    let mut files = vec![
        (
            root.join("AGENTS.md"),
            AGENT_INSTRUCTIONS.as_bytes().to_vec(),
        ),
        (
            root.join("CLAUDE.md"),
            CLAUDE_INSTRUCTIONS.as_bytes().to_vec(),
        ),
        (
            root.join(PROJECT_FILE_NAME),
            (serde_json::to_string_pretty(&project)? + "\n").into_bytes(),
        ),
        (
            root.join(".vscode/renium.project.schema.json"),
            PROJECT_SCHEMA.as_bytes().to_vec(),
        ),
    ];
    let features = features.iter().copied().collect::<BTreeSet<_>>();
    if features.contains(&InitFeature::Git) {
        files.push((
            root.join(".gitignore"),
            b".renium-cache/\n.renium/diagnostics/\nbuild/\nnode_modules/\n".to_vec(),
        ));
    }
    if features.contains(&InitFeature::Wally) {
        files.push((
            root.join("wally.toml"),
            format!(
                "[package]\nname = \"local/{}\"\nversion = \"0.1.0\"\nrealm = \"shared\"\n\n[dependencies]\n",
                safe_file_stem(name)
            )
            .into_bytes(),
        ));
    }
    if features.contains(&InitFeature::Selene) {
        files.push((
            root.join("selene.toml"),
            b"std = \"roblox\"\nexclude = [\"Packages/**\"]\n".to_vec(),
        ));
    }
    if features.contains(&InitFeature::Docs) {
        files.push((
            root.join("README.md"),
            format!("# {name}\n\nRun `rbx build` to create a Roblox place from this project.\n")
                .into_bytes(),
        ));
    }
    if features.contains(&InitFeature::RobloxTs) {
        files.push((
            root.join("package.json"),
            format!(
                "{{\n  \"name\": \"{}\",\n  \"private\": true,\n  \"scripts\": {{\n    \"build\": \"rbxtsc\",\n    \"watch\": \"rbxtsc -w\"\n  }},\n  \"devDependencies\": {{\n    \"@rbxts/compiler-types\": \"3.0.0-types.0\",\n    \"@rbxts/types\": \"1.0.940\",\n    \"roblox-ts\": \"3.0.0\"\n  }}\n}}\n",
                safe_file_stem(name)
            )
            .into_bytes(),
        ));
        files.push((
            root.join("tsconfig.json"),
            b"{\n  \"compilerOptions\": {\n    \"allowSyntheticDefaultImports\": true,\n    \"downlevelIteration\": true,\n    \"module\": \"commonjs\",\n    \"moduleResolution\": \"node\",\n    \"noLib\": true,\n    \"strict\": true,\n    \"target\": \"es6\",\n    \"typeRoots\": [\"node_modules/@rbxts\"]\n  }\n}\n".to_vec(),
        ));
    }
    Ok(files)
}

fn init_directories(root: &Path, source_root: &Path) -> Vec<PathBuf> {
    vec![root.join(source_root)]
}

fn build_once(
    loaded: &LoadedProject,
    args: &BuildArgs,
    output: &Path,
    emit: bool,
    changed_sources: Option<&[PathBuf]>,
) -> Result<()> {
    run_optional_toolchains(loaded, args)?;
    let projection = if args.wally == ToolPolicy::Never {
        if let Some(changed_sources) = changed_sources {
            project_config::stage_project_cached(loaded, changed_sources)?
        } else {
            project_config::stage_project(loaded)?
        }
    } else {
        project_config::stage_project(loaded)?
    };
    if args.sourcemap || loaded.root.join("sourcemap.json").exists() {
        crate::generate_project_sourcemap_for_projection(loaded, &projection)?;
    }
    let source_root = projection.root().to_path_buf();
    let extension = output
        .extension()
        .and_then(OsStr::to_str)
        .unwrap_or(if args.plugin { "rbxm" } else { "rbxl" })
        .to_ascii_lowercase();
    let temporary = output.with_file_name(format!(
        ".{}.{}.tmp.{}",
        output
            .file_stem()
            .and_then(OsStr::to_str)
            .unwrap_or("renium-build"),
        std::process::id(),
        extension
    ));
    let model_output = matches!(extension.as_str(), "rbxm" | "rbxmx");
    let command_target = args
        .target
        .as_ref()
        .map(|target| project_config::ProjectTarget::Shorthand(target.clone()));
    let build_target = command_target
        .as_ref()
        .or(loaded.project.build_target.as_ref());
    if model_output && build_target.is_none() {
        bail!(
            "Model and plugin builds require --target or project buildTarget so one subtree is exported"
        );
    }
    if !model_output && build_target.is_some() {
        bail!("buildTarget can only be used with .rbxm or .rbxmx output");
    }
    let instances = match extension.as_str() {
        "rbxl" | "rbxlx" | "rbxm" | "rbxmx" => {
            build_project_file(&source_root, &temporary, &extension, build_target)?
        }
        _ => bail!("Build output must end in .rbxl, .rbxlx, .rbxm, or .rbxmx"),
    };
    replace_file(&temporary, output)?;
    if !emit {
        return Ok(());
    }
    crate::emit_global_output(
        &json!({
            "ok": true,
            "output": output,
            "format": extension,
            "instances": instances,
        }),
        &format!("Built {}", output.display()),
    )
}

fn build_project_file(
    src_root: &Path,
    output: &Path,
    format: &str,
    target: Option<&project_config::ProjectTarget>,
) -> Result<usize> {
    let target_segments = target.map(project_config::ProjectTarget::segments);
    let target_ordinals = target.map(project_config::ProjectTarget::ordinals);
    let mut services = if let Some(segments) = target_segments.as_ref() {
        vec![segments[0].clone()]
    } else {
        fs::read_dir(src_root)?
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
            .filter_map(|entry| entry.file_name().to_str().map(str::to_string))
            .filter(|name| !name.starts_with('.'))
            .collect::<Vec<_>>()
    };
    services.sort();
    let build = crate::build_rbx_place(src_root, services, None, false, false, false)?;
    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent)?;
    }
    let file = fs::File::create(output)?;
    let mut writer = std::io::BufWriter::new(file);
    let roots = if let Some(segments) = target_segments.as_ref() {
        vec![
            crate::rbx_dom_instance_by_path_unique(
                &build.dom,
                segments,
                target_ordinals.as_deref().unwrap_or_default(),
            )
            .with_context(|| {
                format!(
                    "Build target '{}' does not resolve uniquely",
                    segments.join(".")
                )
            })?,
        ]
    } else {
        build
            .service_roots
            .iter()
            .map(|(_, referent)| *referent)
            .collect::<Vec<_>>()
    };
    let instances = roots
        .iter()
        .map(|root| rbx_subtree_size(&build.dom, *root))
        .sum();
    match format {
        "rbxl" | "rbxm" => rbx_binary::to_writer(&mut writer, &build.dom, &roots)?,
        "rbxlx" | "rbxmx" => rbx_xml::to_writer_default(&mut writer, &build.dom, &roots)?,
        _ => unreachable!(),
    }
    writer.flush()?;
    writer.get_ref().sync_all()?;
    Ok(instances)
}

fn rbx_subtree_size(dom: &rbx_dom_weak::WeakDom, root: rbx_dom_weak::types::Ref) -> usize {
    let mut count = 0;
    let mut pending = vec![root];
    while let Some(referent) = pending.pop() {
        let Some(instance) = dom.get_by_ref(referent) else {
            continue;
        };
        count += 1;
        pending.extend(instance.children().iter().copied());
    }
    count
}

fn run_optional_toolchains(loaded: &LoadedProject, args: &BuildArgs) -> Result<()> {
    let wally_manifest = loaded.root.join("wally.toml");
    if should_run_tool(args.wally, wally_manifest.is_file()) {
        if !wally_manifest.is_file() {
            bail!("--wally always requires {}", wally_manifest.display());
        }
        crate::sync_wally_packages_result(crate::SyncWallyPackagesArgs {
            project_root: loaded.root.clone(),
            src_root: loaded.project.source_root.clone(),
            manifest: PathBuf::from("wally.toml"),
            wally_path: "wally".to_string(),
            rojo_path: "rojo".to_string(),
            packages_dir: PathBuf::from("Packages"),
            target_service: "ReplicatedStorage".to_string(),
            target_name: "Packages".to_string(),
            realms: "shared,server,dev".to_string(),
            server_packages_dir: PathBuf::from("ServerPackages"),
            server_target_service: "ServerStorage".to_string(),
            server_target_name: "ServerPackages".to_string(),
            dev_packages_dir: PathBuf::from("DevPackages"),
            dev_target_service: "ReplicatedStorage".to_string(),
            dev_target_name: "DevPackages".to_string(),
            force: false,
            skip_install: false,
            pretty: false,
        })?;
    }
    let package = loaded.root.join("package.json");
    let roblox_ts_detected = package_uses_roblox_ts(&package)?;
    if should_run_tool(args.typescript, roblox_ts_detected) {
        if !package.is_file() {
            bail!("--ts always requires {}", package.display());
        }
        let (executable, command_args) = roblox_ts_command(loaded, false)?;
        run_checked(
            Command::new(executable)
                .args(command_args)
                .current_dir(&loaded.root),
            "roblox-ts build",
        )?;
    }
    Ok(())
}

fn roblox_ts_executable(loaded: &LoadedProject) -> Result<PathBuf> {
    let local = loaded.root.join(if cfg!(windows) {
        "node_modules/.bin/rbxtsc.cmd"
    } else {
        "node_modules/.bin/rbxtsc"
    });
    if local.is_file() {
        return Ok(local);
    }
    find_command("rbxtsc")
        .context("roblox-ts was requested but rbxtsc is not installed locally or on PATH")
}

fn roblox_ts_package_script(root: &Path, watch: bool) -> Result<Option<String>> {
    let package = root.join("package.json");
    let value: Value = serde_json::from_slice(&fs::read(&package)?)
        .with_context(|| format!("Invalid {}", package.display()))?;
    let Some(scripts) = value.get("scripts").and_then(Value::as_object) else {
        return Ok(None);
    };
    let preferred = if watch { "watch" } else { "build" };
    if scripts
        .get(preferred)
        .and_then(Value::as_str)
        .is_some_and(|command| {
            command
                .split_whitespace()
                .any(|part| part.contains("rbxtsc"))
        })
    {
        return Ok(Some(preferred.to_string()));
    }
    Ok(scripts.iter().find_map(|(name, command)| {
        let command = command.as_str()?;
        let has_rbxtsc = command
            .split_whitespace()
            .any(|part| part.contains("rbxtsc"));
        let is_watch = command
            .split_whitespace()
            .any(|part| matches!(part, "-w" | "--watch"));
        (has_rbxtsc && is_watch == watch).then(|| name.clone())
    }))
}

fn roblox_ts_command(loaded: &LoadedProject, watch: bool) -> Result<(PathBuf, Vec<String>)> {
    if let Some(script) = roblox_ts_package_script(&loaded.root, watch)? {
        let manager = project_package_manager(&loaded.root)?;
        return Ok((manager, vec!["run".to_string(), script]));
    }
    if let Ok(executable) = roblox_ts_executable(loaded) {
        let args = watch.then(|| "--watch".to_string()).into_iter().collect();
        return Ok((executable, args));
    }
    let manager = project_package_manager(&loaded.root)?;
    let name = manager
        .file_stem()
        .and_then(OsStr::to_str)
        .unwrap_or_default()
        .to_ascii_lowercase();
    let mut args = match name.as_str() {
        "npm" => vec!["exec".to_string(), "--".to_string(), "rbxtsc".to_string()],
        "pnpm" | "yarn" => vec!["exec".to_string(), "rbxtsc".to_string()],
        "bun" => vec!["x".to_string(), "rbxtsc".to_string()],
        _ => bail!("Unsupported package manager {}", manager.display()),
    };
    if watch {
        args.push("--watch".to_string());
    }
    Ok((manager, args))
}

struct RobloxTsWatch {
    child: Child,
    output_threads: Vec<thread::JoinHandle<()>>,
    output: mpsc::Receiver<String>,
    output_dropped: Arc<AtomicBool>,
    incremental_error: bool,
    cycle_unreliable: bool,
    cycle_active: bool,
    projection_blocked: bool,
}

#[derive(Default)]
struct RobloxTsDrain {
    successful_cycle: bool,
    overflowed: bool,
}

fn spawn_tool_output_reader(
    stream: impl Read + Send + 'static,
    sender: mpsc::SyncSender<String>,
    output_dropped: Arc<AtomicBool>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        for line in BufReader::new(stream).lines().map_while(Result::ok) {
            crate::log_global(3, format_args!("{line}"));
            match sender.try_send(line) {
                Ok(()) => {}
                Err(mpsc::TrySendError::Full(_)) => {
                    output_dropped.store(true, Ordering::Release);
                }
                Err(mpsc::TrySendError::Disconnected(_)) => break,
            }
        }
    })
}

fn roblox_ts_error_count(line: &str) -> Option<usize> {
    let found = line.find("found ")? + "found ".len();
    let digits = line[found..]
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>();
    if digits.is_empty()
        || !line[found + digits.len()..]
            .trim_start()
            .starts_with("error")
    {
        return None;
    }
    digits.parse().ok()
}

fn roblox_ts_error_line(line: &str) -> bool {
    line.contains("error ts")
        || line.contains("[error]")
        || line.starts_with("error:")
        || line.contains(" compilation failed")
        || line.contains("failed to compile")
}

#[cfg(unix)]
fn configure_watched_process(command: &mut Command) {
    use std::os::unix::process::CommandExt;

    unsafe {
        command.pre_exec(|| {
            if libc::setpgid(0, 0) == 0 {
                Ok(())
            } else {
                Err(std::io::Error::last_os_error())
            }
        });
    }
}

#[cfg(windows)]
fn configure_watched_process(command: &mut Command) {
    use std::os::windows::process::CommandExt;

    command.creation_flags(0x00000200 | 0x08000000);
}

#[cfg(unix)]
fn terminate_watched_process(child: &mut Child) {
    let pid = child.id() as i32;
    unsafe {
        libc::kill(-pid, libc::SIGTERM);
    }
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        let group_alive = unsafe {
            libc::kill(-pid, 0) == 0
                || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
        };
        if !group_alive {
            let _ = child.wait();
            return;
        }
        let _ = child.try_wait();
        thread::sleep(Duration::from_millis(25));
    }
    unsafe {
        libc::kill(-pid, libc::SIGKILL);
    }
    let _ = child.wait();
}

#[cfg(windows)]
fn terminate_watched_process(child: &mut Child) {
    let _ = Command::new("taskkill")
        .args(["/PID", &child.id().to_string(), "/T", "/F"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let _ = child.kill();
    let _ = child.wait();
}

impl RobloxTsWatch {
    fn start(loaded: &LoadedProject, args: &BuildArgs) -> Result<Option<Self>> {
        let package = loaded.root.join("package.json");
        let detected = package_uses_roblox_ts(&package)?;
        if !should_run_tool(args.typescript, detected) {
            return Ok(None);
        }
        if !package.is_file() {
            bail!("--ts always requires {}", package.display());
        }
        let (executable, command_args) = roblox_ts_command(loaded, true)?;
        let mut command = Command::new(executable);
        command
            .args(command_args)
            .current_dir(&loaded.root)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        configure_watched_process(&mut command);
        let mut child = command.spawn().context("Failed to start roblox-ts watch")?;
        let (sender, output) = mpsc::sync_channel(1_024);
        let output_dropped = Arc::new(AtomicBool::new(false));
        let mut output_threads = Vec::new();
        if let Some(stdout) = child.stdout.take() {
            output_threads.push(spawn_tool_output_reader(
                stdout,
                sender.clone(),
                Arc::clone(&output_dropped),
            ));
        }
        if let Some(stderr) = child.stderr.take() {
            output_threads.push(spawn_tool_output_reader(
                stderr,
                sender,
                Arc::clone(&output_dropped),
            ));
        }
        Ok(Some(Self {
            child,
            output_threads,
            output,
            output_dropped,
            incremental_error: false,
            cycle_unreliable: false,
            cycle_active: false,
            projection_blocked: false,
        }))
    }

    fn is_running(&mut self) -> Result<bool> {
        if let Some(status) = self.child.try_wait()? {
            crate::log_global(2, format_args!("roblox-ts watch exited with {status}"));
            return Ok(false);
        }
        Ok(true)
    }

    fn wait_initial(&mut self) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(60);
        let mut saw_error = false;
        loop {
            if !self.is_running()? {
                bail!("roblox-ts watch exited before its initial compilation completed");
            }
            if self.output_dropped.load(Ordering::Acquire) {
                bail!("roblox-ts watch produced too much output to verify its initial compilation");
            }
            match self.output.recv_timeout(Duration::from_millis(100)) {
                Ok(line) => {
                    let line = line.to_ascii_lowercase();
                    if let Some(errors) = roblox_ts_error_count(&line) {
                        if errors == 0 {
                            return Ok(());
                        }
                        bail!("roblox-ts initial compilation reported {errors} error(s)");
                    }
                    saw_error |= roblox_ts_error_line(&line);
                    if (line.contains("compilation complete")
                        || line.contains("compiled successfully"))
                        && !saw_error
                    {
                        return Ok(());
                    }
                    if line.contains("watching for file changes") && saw_error {
                        bail!("roblox-ts initial compilation failed");
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    bail!("roblox-ts watch closed its output before becoming ready");
                }
            }
            if Instant::now() >= deadline {
                bail!("roblox-ts watch did not finish its initial compilation within 60 seconds");
            }
        }
    }

    fn drain_incremental(&mut self) -> RobloxTsDrain {
        let mut drain = RobloxTsDrain::default();
        if self.output_dropped.load(Ordering::Acquire) {
            self.incremental_error = true;
            self.cycle_unreliable = true;
            self.projection_blocked = true;
            drain.overflowed = true;
            crate::log_global(
                2,
                format_args!(
                    "roblox-ts produced too much output; the current compile cycle is unreliable"
                ),
            );
        }
        while let Ok(line) = self.output.try_recv() {
            let normalized = line.to_ascii_lowercase();
            if normalized.contains("starting compilation")
                || normalized.contains("starting incremental compilation")
                || normalized.contains("file change detected")
            {
                self.incremental_error = false;
                self.cycle_active = true;
                self.projection_blocked = true;
            }
            let mut surfaced = false;
            if let Some(errors) = roblox_ts_error_count(&normalized) {
                if errors > 0 {
                    self.incremental_error = true;
                    crate::log_global(
                        2,
                        format_args!("roblox-ts compilation reported {errors} error(s)"),
                    );
                    surfaced = true;
                }
                let succeeded = self.cycle_active
                    && errors == 0
                    && !self.incremental_error
                    && !self.cycle_unreliable;
                drain.successful_cycle |= succeeded;
                if self.cycle_active {
                    self.projection_blocked = !succeeded;
                    self.cycle_active = false;
                }
            }
            if roblox_ts_error_line(&normalized) {
                self.incremental_error = true;
                if !surfaced {
                    crate::log_global(2, format_args!("roblox-ts: {line}"));
                }
            }
            if normalized.contains("compilation complete")
                || normalized.contains("compiled successfully")
                || normalized.contains("watching for file changes")
            {
                let succeeded =
                    self.cycle_active && !self.incremental_error && !self.cycle_unreliable;
                if self.cycle_active && !succeeded {
                    crate::log_global(2, format_args!("roblox-ts compilation failed"));
                }
                drain.successful_cycle |= succeeded;
                if self.cycle_active {
                    self.projection_blocked = !succeeded;
                    self.cycle_active = false;
                }
            }
        }
        if self.output_dropped.load(Ordering::Acquire) {
            self.incremental_error = true;
            self.cycle_unreliable = true;
            self.projection_blocked = true;
            drain.overflowed = true;
            drain.successful_cycle = false;
        }
        drain
    }

    fn projection_blocked(&self) -> bool {
        self.projection_blocked
    }
}

fn roblox_ts_watch_desired(loaded: &LoadedProject, args: &BuildArgs) -> Result<bool> {
    Ok(should_run_tool(
        args.typescript,
        package_uses_roblox_ts(&loaded.root.join("package.json"))?,
    ))
}

fn start_ready_roblox_ts_watch(
    loaded: &LoadedProject,
    args: &BuildArgs,
) -> Result<Option<RobloxTsWatch>> {
    let Some(mut process) = RobloxTsWatch::start(loaded, args)? else {
        return Ok(None);
    };
    process.wait_initial()?;
    Ok(Some(process))
}

fn roblox_ts_retry_delay(failures: u32) -> Duration {
    Duration::from_millis(250_u64.saturating_mul(1_u64 << failures.min(4)))
}

struct RobloxTsConfigGraph {
    files: BTreeSet<PathBuf>,
    output_roots: BTreeSet<PathBuf>,
}

impl RobloxTsConfigGraph {
    fn fingerprint(&self) -> Vec<(PathBuf, Option<Vec<u8>>)> {
        self.files
            .iter()
            .map(|path| (path.clone(), fs::read(path).ok()))
            .collect()
    }
}

fn resolve_tsconfig_candidate(path: PathBuf) -> Option<PathBuf> {
    let candidates = if path.extension().is_some() {
        vec![path]
    } else {
        vec![
            path.clone(),
            path.with_extension("json"),
            path.join("tsconfig.json"),
        ]
    };
    candidates
        .into_iter()
        .find(|candidate| candidate.is_file())
        .map(|candidate| absolute_path(&candidate))
}

fn resolve_tsconfig_package(
    from: &Path,
    specifier: &str,
    files: &mut BTreeSet<PathBuf>,
) -> Result<Option<PathBuf>> {
    let segments = specifier.split('/').collect::<Vec<_>>();
    let package_parts = if specifier.starts_with('@') { 2 } else { 1 };
    if segments.len() < package_parts {
        return Ok(None);
    }
    let package_name = segments[..package_parts].join("/");
    let subpath = segments[package_parts..].join("/");
    for ancestor in from.ancestors() {
        let package_root = ancestor.join("node_modules").join(&package_name);
        if !package_root.is_dir() {
            continue;
        }
        if !subpath.is_empty() {
            return Ok(resolve_tsconfig_candidate(package_root.join(subpath)));
        }
        let package_json = package_root.join("package.json");
        if package_json.is_file() {
            files.insert(absolute_path(&package_json));
            let value: Value = serde_json::from_slice(&fs::read(&package_json)?)
                .with_context(|| format!("Invalid {}", package_json.display()))?;
            if let Some(config) = value.get("tsconfig").and_then(Value::as_str)
                && let Some(path) = resolve_tsconfig_candidate(package_root.join(config))
            {
                return Ok(Some(path));
            }
        }
        return Ok(resolve_tsconfig_candidate(
            package_root.join("tsconfig.json"),
        ));
    }
    Ok(None)
}

fn resolve_tsconfig_specifier(
    config: &Path,
    specifier: &str,
    files: &mut BTreeSet<PathBuf>,
) -> Result<PathBuf> {
    let parent = config
        .parent()
        .context("tsconfig has no parent directory")?;
    let path = Path::new(specifier);
    let resolved = if path.is_absolute() || specifier.starts_with('.') {
        resolve_tsconfig_candidate(parent.join(path))
    } else {
        resolve_tsconfig_package(parent, specifier, files)?
    };
    resolved.with_context(|| {
        format!(
            "Could not resolve tsconfig dependency '{specifier}' from {}",
            config.display()
        )
    })
}

fn roblox_ts_config_graph(root: &Path) -> Result<RobloxTsConfigGraph> {
    fn visit(
        path: PathBuf,
        graph: &mut RobloxTsConfigGraph,
        visiting: &mut BTreeSet<PathBuf>,
        outputs: &mut BTreeMap<PathBuf, Option<PathBuf>>,
    ) -> Result<Option<PathBuf>> {
        let path = absolute_path(&path);
        if let Some(output) = outputs.get(&path) {
            return Ok(output.clone());
        }
        if !visiting.insert(path.clone()) {
            bail!("tsconfig dependency cycle includes {}", path.display());
        }
        graph.files.insert(path.clone());
        let text = fs::read_to_string(&path)
            .with_context(|| format!("Failed to read {}", path.display()))?;
        let value = project_config::parse_jsonc_value(&text)
            .with_context(|| format!("Invalid {}", path.display()))?;
        let mut output = None;
        let extends = match value.get("extends") {
            Some(Value::String(value)) => vec![value.as_str()],
            Some(Value::Array(values)) => values.iter().filter_map(Value::as_str).collect(),
            _ => Vec::new(),
        };
        for specifier in extends {
            let dependency = resolve_tsconfig_specifier(&path, specifier, &mut graph.files)?;
            if let Some(inherited) = visit(dependency, graph, visiting, outputs)? {
                output = Some(inherited);
            }
        }
        if let Some(out_dir) = value
            .get("compilerOptions")
            .and_then(Value::as_object)
            .and_then(|options| options.get("outDir"))
            .and_then(Value::as_str)
        {
            output = Some(absolute_path(
                &path
                    .parent()
                    .context("tsconfig has no parent directory")?
                    .join(out_dir),
            ));
        }
        if let Some(references) = value.get("references").and_then(Value::as_array) {
            for reference in references {
                let Some(specifier) = reference.get("path").and_then(Value::as_str) else {
                    continue;
                };
                let dependency = resolve_tsconfig_candidate(
                    path.parent()
                        .context("tsconfig has no parent directory")?
                        .join(specifier),
                )
                .with_context(|| {
                    format!(
                        "Could not resolve tsconfig project reference '{specifier}' from {}",
                        path.display()
                    )
                })?;
                if let Some(reference_output) = visit(dependency, graph, visiting, outputs)? {
                    graph.output_roots.insert(reference_output);
                }
            }
        }
        visiting.remove(&path);
        if let Some(output) = output.as_ref() {
            graph.output_roots.insert(output.clone());
        }
        outputs.insert(path, output.clone());
        Ok(output)
    }

    let mut graph = RobloxTsConfigGraph {
        files: BTreeSet::from([absolute_path(&root.join("package.json"))]),
        output_roots: BTreeSet::new(),
    };
    let root_config = root.join("tsconfig.json");
    if root_config.is_file() {
        visit(
            root_config,
            &mut graph,
            &mut BTreeSet::new(),
            &mut BTreeMap::new(),
        )?;
    }
    Ok(graph)
}

fn roblox_ts_event_is_related(paths: &[PathBuf], output_roots: &[PathBuf]) -> bool {
    paths.iter().any(|path| {
        let path = absolute_path(path);
        path.extension()
            .and_then(OsStr::to_str)
            .is_some_and(|extension| {
                matches!(extension.to_ascii_lowercase().as_str(), "ts" | "tsx")
            })
            || output_roots
                .iter()
                .any(|root| path == *root || path.starts_with(root))
    })
}

fn build_after_roblox_ts(loaded: &mut LoadedProject, args: &BuildArgs, output: &Path) -> bool {
    let mut projection_args = args.clone();
    projection_args.typescript = ToolPolicy::Never;
    match build_once(loaded, &projection_args, output, true, None) {
        Ok(()) => true,
        Err(error) => {
            crate::log_global(
                2,
                format_args!("Build after roblox-ts compilation failed: {error:#}"),
            );
            false
        }
    }
}

impl Drop for RobloxTsWatch {
    fn drop(&mut self) {
        terminate_watched_process(&mut self.child);
        for handle in self.output_threads.drain(..) {
            let _ = handle.join();
        }
    }
}

fn package_uses_roblox_ts(path: &Path) -> Result<bool> {
    if !path.is_file() {
        return Ok(false);
    }
    let value: Value = serde_json::from_slice(&fs::read(path)?)
        .with_context(|| format!("Invalid {}", path.display()))?;
    let dependency = ["dependencies", "devDependencies"]
        .into_iter()
        .any(|section| {
            value
                .get(section)
                .and_then(Value::as_object)
                .is_some_and(|dependencies| dependencies.contains_key("roblox-ts"))
        });
    let script = value
        .get("scripts")
        .and_then(Value::as_object)
        .is_some_and(|scripts| {
            scripts.values().filter_map(Value::as_str).any(|command| {
                command
                    .split_whitespace()
                    .any(|part| part.contains("rbxtsc"))
            })
        });
    Ok(dependency || script)
}

fn should_run_tool(policy: ToolPolicy, detected: bool) -> bool {
    match policy {
        ToolPolicy::Auto => detected,
        ToolPolicy::Always => true,
        ToolPolicy::Never => false,
    }
}

#[derive(Default)]
struct ProjectWatchInputs {
    files: BTreeSet<PathBuf>,
    directories: BTreeSet<PathBuf>,
    ignored: BTreeSet<PathBuf>,
}

fn project_watch_inputs(loaded: &LoadedProject) -> Result<ProjectWatchInputs> {
    let mut inputs = ProjectWatchInputs::default();
    let mut visited = BTreeSet::new();
    project_watch_inputs_into(loaded, &mut inputs, &mut visited)?;
    Ok(inputs)
}

fn project_watch_inputs_into(
    loaded: &LoadedProject,
    inputs: &mut ProjectWatchInputs,
    visited: &mut BTreeSet<PathBuf>,
) -> Result<()> {
    let key = absolute_path(&loaded.path);
    if !visited.insert(key.clone()) {
        return Ok(());
    }
    inputs.files.insert(key);
    inputs.directories.insert(absolute_path(
        &loaded.root.join(&loaded.project.source_root),
    ));
    let mut nested = Vec::new();
    for (_, node) in project_config::project_tree_nodes(&loaded.project.tree) {
        if let Some(path) = node.path {
            let path = absolute_path(&loaded.root.join(path));
            if path.is_dir() || (!path.exists() && path.extension().is_none()) {
                inputs.directories.insert(path);
            } else {
                inputs.files.insert(path.clone());
                nested.push(path);
            }
        }
    }
    for mount in &loaded.project.mounts {
        let path = absolute_path(&loaded.root.join(&mount.source));
        if path.is_dir() || (!path.exists() && path.extension().is_none()) {
            inputs.directories.insert(path);
        } else {
            inputs.files.insert(path.clone());
            nested.push(path);
        }
    }
    for adapter in &loaded.project.adapters {
        let source = absolute_path(&loaded.root.join(&adapter.source));
        inputs.files.insert(source.clone());
        nested.push(source);
        if let Some(output) = adapter.output.as_deref() {
            inputs
                .ignored
                .insert(absolute_path(&loaded.root.join(output)));
        }
    }
    inputs.files.extend(
        ["wally.toml", "wally.lock"]
            .into_iter()
            .map(|path| absolute_path(&loaded.root.join(path))),
    );
    inputs
        .files
        .extend(roblox_ts_config_graph(&loaded.root)?.files);
    inputs
        .ignored
        .insert(absolute_path(&loaded.root.join("sourcemap.json")));
    for path in nested {
        let name = path
            .file_name()
            .and_then(OsStr::to_str)
            .unwrap_or_default()
            .to_ascii_lowercase();
        if (name.ends_with(".project.json") || name.ends_with(".project.jsonc")) && path.is_file() {
            let nested = project_config::load_project(Some(&path), None)?;
            project_watch_inputs_into(&nested, inputs, visited)?;
        }
    }
    Ok(())
}

fn configure_project_watcher(
    watcher: &mut RecommendedWatcher,
    previous: &mut BTreeMap<PathBuf, bool>,
    inputs: &ProjectWatchInputs,
) -> Result<()> {
    let mut roots = BTreeMap::new();
    for (root, recursive) in inputs
        .files
        .iter()
        .map(|path| (path, false))
        .chain(inputs.directories.iter().map(|path| (path, true)))
    {
        let mut watch_root = root.clone();
        while !watch_root.exists() {
            let Some(parent) = watch_root.parent() else {
                break;
            };
            watch_root = parent.to_path_buf();
        }
        if !watch_root.exists() {
            continue;
        }
        let recursive = recursive || watch_root != *root;
        roots
            .entry(watch_root)
            .and_modify(|existing| *existing |= recursive)
            .or_insert(recursive);
    }
    for (root, recursive) in previous.iter() {
        if roots.get(root) != Some(recursive) {
            let _ = watcher.unwatch(root);
        }
    }
    for (root, recursive) in &roots {
        if previous.get(root) == Some(recursive) {
            continue;
        }
        watcher
            .watch(
                root,
                if *recursive {
                    RecursiveMode::Recursive
                } else {
                    RecursiveMode::NonRecursive
                },
            )
            .with_context(|| format!("Failed to watch {}", root.display()))?;
    }
    *previous = roots;
    Ok(())
}

fn build_watch_event_is_relevant(
    paths: &[PathBuf],
    output: &Path,
    inputs: &ProjectWatchInputs,
) -> bool {
    let output = absolute_path(output);
    paths.iter().any(|path| {
        let path = absolute_path(path);
        let temporary_output = path.parent() == output.parent()
            && path
                .file_name()
                .and_then(OsStr::to_str)
                .is_some_and(|name| {
                    let stem = output
                        .file_stem()
                        .and_then(OsStr::to_str)
                        .unwrap_or("renium-build");
                    name.starts_with(&format!(".{stem}."))
                        && name.contains(".tmp.")
                        && path.extension() == output.extension()
                });
        let transaction_backup = path.parent() == output.parent()
            && path
                .file_name()
                .and_then(OsStr::to_str)
                .is_some_and(|name| {
                    let output_name = output
                        .file_name()
                        .and_then(OsStr::to_str)
                        .unwrap_or("renium-build");
                    name.starts_with(&format!(".{output_name}.")) && name.ends_with(".previous")
                });
        if path == output
            || temporary_output
            || transaction_backup
            || inputs
                .ignored
                .iter()
                .any(|ignored| path == *ignored || path.starts_with(ignored))
        {
            return false;
        }
        let excluded = path.components().any(|component| {
            component.as_os_str().to_str().is_some_and(|name| {
                matches!(name, ".git" | ".renium" | ".renium-cache" | "node_modules")
            })
        });
        !excluded
            && (inputs.files.contains(&path)
                || inputs
                    .directories
                    .iter()
                    .any(|directory| path == *directory || path.starts_with(directory)))
    })
}

fn build_args_for_watch_event(
    args: &BuildArgs,
    loaded: &LoadedProject,
    paths: &[PathBuf],
    typescript_config_files: &BTreeSet<PathBuf>,
) -> BuildArgs {
    let paths = paths
        .iter()
        .map(|path| absolute_path(path))
        .collect::<Vec<_>>();
    let project_changed = paths.contains(&absolute_path(&loaded.path));
    let wally_changed = project_changed
        || ["wally.toml", "wally.lock"]
            .into_iter()
            .map(|path| absolute_path(&loaded.root.join(path)))
            .any(|path| paths.contains(&path));
    let typescript_changed = project_changed
        || paths
            .iter()
            .any(|path| typescript_config_files.contains(path))
        || paths.iter().any(|path| {
            path.extension()
                .and_then(OsStr::to_str)
                .is_some_and(|extension| {
                    matches!(extension.to_ascii_lowercase().as_str(), "ts" | "tsx")
                })
        });
    let mut selected = args.clone();
    if selected.wally == ToolPolicy::Auto && !wally_changed {
        selected.wally = ToolPolicy::Never;
    }
    if selected.typescript == ToolPolicy::Auto && !typescript_changed {
        selected.typescript = ToolPolicy::Never;
    }
    selected
}

fn watch_build(loaded: &mut LoadedProject, args: &BuildArgs, output: &Path) -> Result<()> {
    let mut inputs = project_watch_inputs(loaded)?;
    let mut typescript_watch_desired = roblox_ts_watch_desired(loaded, args)?;
    let mut typescript_watch = start_ready_roblox_ts_watch(loaded, args)?;
    let initial_typescript_graph = roblox_ts_config_graph(&loaded.root)?;
    let mut typescript_output_roots = initial_typescript_graph
        .output_roots
        .iter()
        .cloned()
        .collect::<Vec<_>>();
    let mut typescript_config_files = initial_typescript_graph.files;
    let mut typescript_config_fingerprint = RobloxTsConfigGraph {
        files: typescript_config_files.clone(),
        output_roots: typescript_output_roots.iter().cloned().collect(),
    }
    .fingerprint();
    let mut initial_args = args.clone();
    if typescript_watch.is_some() {
        initial_args.typescript = ToolPolicy::Never;
    }
    build_once(loaded, &initial_args, output, true, None)?;
    let (sender, receiver) = mpsc::sync_channel(4_096);
    let watch_overflowed = Arc::new(AtomicBool::new(false));
    let callback_overflowed = Arc::clone(&watch_overflowed);
    let mut watcher = notify::recommended_watcher(move |event| match sender.try_send(event) {
        Ok(()) => {}
        Err(mpsc::TrySendError::Full(_)) => {
            callback_overflowed.store(true, Ordering::Release);
        }
        Err(mpsc::TrySendError::Disconnected(_)) => {}
    })?;
    let mut watched = BTreeMap::new();
    configure_project_watcher(&mut watcher, &mut watched, &inputs)?;
    let mut typescript_restart_failures = 0_u32;
    let mut typescript_retry_at = Instant::now();
    let mut typescript_projection_pending = false;
    loop {
        let mut rescan_required = false;
        let mut typescript_cycle_completed = false;
        let mut typescript_output_overflowed = false;
        let event = loop {
            match receiver.recv_timeout(Duration::from_millis(500)) {
                Ok(Ok(event)) => break Some(event),
                Ok(Err(error)) => {
                    crate::log_global(2, format_args!("Build watcher failed: {error}"));
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if let Some(process) = typescript_watch.as_mut() {
                        let drain = process.drain_incremental();
                        if drain.overflowed {
                            typescript_output_overflowed = true;
                        } else if drain.successful_cycle {
                            typescript_restart_failures = 0;
                            typescript_projection_pending =
                                !build_after_roblox_ts(loaded, args, output);
                        }
                    }
                    if typescript_output_overflowed {
                        drop(typescript_watch.take());
                        typescript_restart_failures = typescript_restart_failures.saturating_add(1);
                        typescript_retry_at =
                            Instant::now() + roblox_ts_retry_delay(typescript_restart_failures);
                        typescript_projection_pending = true;
                        typescript_output_overflowed = false;
                    } else if typescript_watch
                        .as_mut()
                        .map(RobloxTsWatch::is_running)
                        .transpose()?
                        == Some(false)
                    {
                        drop(typescript_watch.take());
                        typescript_restart_failures = typescript_restart_failures.saturating_add(1);
                        typescript_retry_at =
                            Instant::now() + roblox_ts_retry_delay(typescript_restart_failures);
                        typescript_projection_pending = true;
                    }
                    if typescript_watch_desired
                        && typescript_watch.is_none()
                        && Instant::now() >= typescript_retry_at
                    {
                        match start_ready_roblox_ts_watch(loaded, args) {
                            Ok(Some(process)) => {
                                typescript_watch = Some(process);
                                if typescript_projection_pending {
                                    typescript_projection_pending =
                                        !build_after_roblox_ts(loaded, args, output);
                                }
                            }
                            Ok(None) => {
                                typescript_watch_desired = false;
                                typescript_restart_failures = 0;
                            }
                            Err(error) => {
                                typescript_restart_failures =
                                    typescript_restart_failures.saturating_add(1);
                                typescript_retry_at = Instant::now()
                                    + roblox_ts_retry_delay(typescript_restart_failures);
                                crate::log_global(
                                    2,
                                    format_args!("roblox-ts watch restart failed: {error:#}"),
                                );
                            }
                        }
                    }
                    if watch_overflowed.swap(false, Ordering::AcqRel) {
                        rescan_required = true;
                        break None;
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    bail!("Project watcher stopped");
                }
            }
        };
        if let Some(process) = typescript_watch.as_mut() {
            let drain = process.drain_incremental();
            typescript_cycle_completed |= drain.successful_cycle;
            typescript_output_overflowed |= drain.overflowed;
        }
        let mut paths = event.map(|event| event.paths).unwrap_or_default();
        let batch_deadline = Instant::now() + Duration::from_millis(500);
        while Instant::now() < batch_deadline
            && let Ok(event) = receiver.recv_timeout(
                Duration::from_millis(100)
                    .min(batch_deadline.saturating_duration_since(Instant::now())),
            )
        {
            match event {
                Ok(event) => paths.extend(event.paths),
                Err(error) => crate::log_global(2, format_args!("Build watcher failed: {error}")),
            }
        }
        rescan_required |= watch_overflowed.swap(false, Ordering::AcqRel);
        if let Some(process) = typescript_watch.as_mut() {
            let drain = process.drain_incremental();
            typescript_cycle_completed |= drain.successful_cycle;
            typescript_output_overflowed |= drain.overflowed;
        }
        if typescript_output_overflowed {
            drop(typescript_watch.take());
            typescript_restart_failures = typescript_restart_failures.saturating_add(1);
            typescript_retry_at =
                Instant::now() + roblox_ts_retry_delay(typescript_restart_failures);
            typescript_projection_pending = true;
            continue;
        }
        paths.sort();
        paths.dedup();
        configure_project_watcher(&mut watcher, &mut watched, &inputs)?;
        if !rescan_required && !build_watch_event_is_relevant(&paths, output, &inputs) {
            continue;
        }
        let typescript_config_path_changed = paths
            .iter()
            .any(|path| typescript_config_files.contains(path));
        let current_typescript_graph = roblox_ts_config_graph(&loaded.root);
        let current_typescript_config_fingerprint = current_typescript_graph
            .as_ref()
            .ok()
            .map(RobloxTsConfigGraph::fingerprint);
        let typescript_configuration_changed = typescript_config_path_changed
            || rescan_required
                && current_typescript_config_fingerprint
                    .as_ref()
                    .is_none_or(|fingerprint| *fingerprint != typescript_config_fingerprint);
        let project_graph_changed = rescan_required
            || paths.iter().any(|path| {
                let name = path
                    .file_name()
                    .and_then(OsStr::to_str)
                    .unwrap_or_default()
                    .to_ascii_lowercase();
                name.ends_with(".project.json") || name.ends_with(".project.jsonc")
            });
        if project_graph_changed {
            match project_config::load_project(Some(&loaded.path), None) {
                Ok(current) => {
                    *loaded = current;
                    inputs = project_watch_inputs(loaded)?;
                    configure_project_watcher(&mut watcher, &mut watched, &inputs)?;
                }
                Err(error) => {
                    crate::log_global(2, format_args!("Build configuration failed: {error:#}"));
                    continue;
                }
            }
        }
        if typescript_configuration_changed {
            drop(typescript_watch.take());
            let current_typescript_graph = match current_typescript_graph {
                Ok(graph) => graph,
                Err(error) => {
                    crate::log_global(
                        2,
                        format_args!("roblox-ts configuration is not ready: {error:#}"),
                    );
                    continue;
                }
            };
            let desired = match roblox_ts_watch_desired(loaded, args) {
                Ok(desired) => desired,
                Err(error) => {
                    crate::log_global(
                        2,
                        format_args!("roblox-ts configuration is not ready: {error:#}"),
                    );
                    continue;
                }
            };
            typescript_config_fingerprint = current_typescript_graph.fingerprint();
            typescript_config_files = current_typescript_graph.files;
            typescript_output_roots = current_typescript_graph.output_roots.into_iter().collect();
            typescript_projection_pending = desired;
            if desired {
                match start_ready_roblox_ts_watch(loaded, args) {
                    Ok(Some(process)) => {
                        typescript_watch = Some(process);
                        typescript_watch_desired = true;
                        typescript_restart_failures = 0;
                        typescript_retry_at = Instant::now();
                    }
                    Ok(None) => {
                        typescript_watch_desired = false;
                        typescript_projection_pending = false;
                    }
                    Err(error) => {
                        typescript_watch_desired = true;
                        typescript_restart_failures =
                            typescript_restart_failures.saturating_add(1).max(1);
                        typescript_retry_at =
                            Instant::now() + roblox_ts_retry_delay(typescript_restart_failures);
                        crate::log_global(
                            2,
                            format_args!("roblox-ts watch restart failed: {error:#}"),
                        );
                        continue;
                    }
                }
            } else {
                typescript_watch_desired = false;
            }
            inputs = project_watch_inputs(loaded)?;
            configure_project_watcher(&mut watcher, &mut watched, &inputs)?;
            if typescript_watch.is_some() {
                typescript_projection_pending = !build_after_roblox_ts(loaded, args, output);
            } else {
                match build_once(loaded, args, output, true, None) {
                    Ok(()) => typescript_projection_pending = false,
                    Err(error) => crate::log_global(
                        2,
                        format_args!(
                            "Build after roblox-ts configuration change failed: {error:#}"
                        ),
                    ),
                }
            }
            continue;
        }
        if typescript_cycle_completed {
            typescript_restart_failures = 0;
            typescript_projection_pending = !build_after_roblox_ts(loaded, args, output);
            continue;
        }
        if typescript_watch.is_some()
            && roblox_ts_event_is_related(&paths, &typescript_output_roots)
        {
            continue;
        }
        if typescript_watch
            .as_ref()
            .is_some_and(RobloxTsWatch::projection_blocked)
        {
            typescript_projection_pending = true;
            continue;
        }
        let force_full_build = rescan_required || typescript_projection_pending;
        let mut selected =
            build_args_for_watch_event(args, loaded, &paths, &typescript_config_files);
        if typescript_watch.is_some() {
            selected.typescript = ToolPolicy::Never;
        }
        let changed_paths = (!force_full_build).then_some(paths.as_slice());
        if let Err(error) = build_once(loaded, &selected, output, true, changed_paths) {
            crate::log_global(2, format_args!("Build failed: {error:#}"));
        } else {
            typescript_projection_pending = false;
        }
    }
}

fn write_doctor_bundle(path: &Path, result: &Value, root: &Path) -> Result<()> {
    let directory = if path.extension().is_some() {
        path.with_extension("")
    } else {
        path.to_path_buf()
    };
    fs::create_dir_all(&directory)?;
    atomic_write(
        &directory.join("doctor.json"),
        (serde_json::to_string_pretty(result)? + "\n").as_bytes(),
    )?;
    if let Ok(project) = project_config::load_project(None, Some(root)) {
        let text = fs::read_to_string(&project.path)?;
        atomic_write(&directory.join(PROJECT_FILE_NAME), text.as_bytes())?;
    }
    let environment = json!({
        "os": env::consts::OS,
        "arch": env::consts::ARCH,
        "version": crate::BUILD_VERSION,
        "gitHash": crate::BUILD_GIT_HASH,
    });
    atomic_write(
        &directory.join("environment.json"),
        (serde_json::to_string_pretty(&environment)? + "\n").as_bytes(),
    )?;
    crate::log_global(
        3,
        format_args!("Wrote diagnostic bundle {}", directory.display()),
    );
    Ok(())
}

fn docs_topic(topic: Option<&str>) -> Result<String> {
    let Some(topic) = topic.map(str::trim).filter(|topic| !topic.is_empty()) else {
        return Ok(CLI_DOCS.to_string());
    };
    let needle = topic.to_ascii_lowercase();
    let lines = CLI_DOCS.lines().collect::<Vec<_>>();
    let start = lines
        .iter()
        .position(|line| {
            line.trim_start_matches('#')
                .trim()
                .to_ascii_lowercase()
                .contains(&needle)
        })
        .with_context(|| format!("No bundled documentation topic matches '{topic}'"))?;
    let level = lines[start].chars().take_while(|ch| *ch == '#').count();
    let end = lines
        .iter()
        .enumerate()
        .skip(start + 1)
        .find(|(_, line)| {
            let next_level = line.chars().take_while(|ch| *ch == '#').count();
            next_level > 0 && next_level <= level
        })
        .map(|(index, _)| index)
        .unwrap_or(lines.len());
    Ok(lines[start..end].join("\n") + "\n")
}

fn html_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn daemon_status_values(name: Option<&str>) -> Result<Vec<Value>> {
    let mut result = Vec::new();
    for (path, daemon) in read_daemon_discoveries()? {
        if name.is_some_and(|name| daemon.name != name) {
            continue;
        }
        let alive = crate::is_process_alive(daemon.pid);
        let endpoint = daemon_endpoint(&daemon).ok();
        let responsive = endpoint
            .and_then(|endpoint| {
                TcpStream::connect_timeout(&endpoint, Duration::from_millis(250)).ok()
            })
            .is_some();
        result.push(json!({
            "name": daemon.name,
            "pid": daemon.pid,
            "alive": alive,
            "responsive": responsive,
            "host": daemon.host,
            "controlPort": daemon.control_port,
            "bridgePorts": daemon.bridge_ports,
            "discoveryFile": path,
        }));
    }
    Ok(result)
}

fn stop_named_daemon(name: &str, force: bool) -> Result<String> {
    let Some((path, daemon)) = read_daemon_discoveries()?
        .into_iter()
        .find(|(_, daemon)| daemon.name == name)
    else {
        bail!("No daemon named '{name}' was found");
    };
    if !crate::is_process_alive(daemon.pid) {
        fs::remove_file(&path)?;
        return Ok(format!("Removed stale daemon discovery {}", path.display()));
    }
    if !force {
        let endpoint = daemon_endpoint(&daemon).with_context(|| {
            format!("Daemon '{name}' has no valid control endpoint; use --force to stop it")
        })?;
        TcpStream::connect_timeout(&endpoint, Duration::from_millis(250)).with_context(|| {
            format!("Daemon '{name}' is not responding; use --force to stop it")
        })?;
    }
    terminate_recorded_daemon(daemon.pid)?;
    let deadline = Instant::now() + Duration::from_secs(1);
    while Instant::now() < deadline && crate::is_process_alive(daemon.pid) {
        thread::sleep(Duration::from_millis(50));
    }
    if crate::is_process_alive(daemon.pid) {
        bail!("Daemon '{name}' did not exit within one second");
    }
    if path.exists() {
        fs::remove_file(&path)?;
    }
    Ok(format!("Stopped daemon '{name}'"))
}

#[cfg(windows)]
fn terminate_recorded_daemon(pid: u32) -> Result<()> {
    let status = Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .status()
        .context("Failed to run taskkill")?;
    if !status.success() {
        bail!("taskkill failed with {status}");
    }
    Ok(())
}

#[cfg(unix)]
fn terminate_recorded_daemon(pid: u32) -> Result<()> {
    let pid = i32::try_from(pid).context("Daemon PID is outside the supported range")?;
    let result = unsafe { libc::kill(pid, libc::SIGKILL) };
    if result != 0 {
        return Err(std::io::Error::last_os_error()).context("Failed to stop daemon process");
    }
    Ok(())
}

fn stop_all_daemons(force: bool) -> Result<()> {
    let stopped = stop_all_daemons_internal(force)?;
    crate::emit_global_output(
        &json!({ "ok": true, "stopped": stopped }),
        &format!("Stopped {} daemon(s)", stopped.len()),
    )
}

pub(crate) fn stop_all_daemons_for_update() -> Result<()> {
    let discoveries = read_daemon_discoveries()?
        .into_iter()
        .map(|(_, daemon)| daemon)
        .collect::<Vec<_>>();
    stop_all_daemons_internal(false)?;
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        let recorded_occupied = discoveries.iter().any(|daemon| {
            std::iter::once(daemon.control_port)
                .chain(daemon.bridge_ports.iter().copied())
                .any(|port| {
                    daemon
                        .host
                        .parse::<std::net::IpAddr>()
                        .ok()
                        .is_none_or(|host| TcpListener::bind((host, port)).is_err())
                })
        });
        let fixed_occupied = [8780_u16, 8781, 8782]
            .into_iter()
            .any(|port| TcpListener::bind(("127.0.0.1", port)).is_err());
        if !recorded_occupied && !fixed_occupied {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!("Renium daemon ports 8780-8782 are still in use");
        }
        thread::sleep(Duration::from_millis(50));
    }
}

fn stop_all_daemons_internal(force: bool) -> Result<Vec<String>> {
    let names = read_daemon_discoveries()?
        .into_iter()
        .map(|(_, daemon)| daemon.name)
        .collect::<BTreeSet<_>>();
    let mut stopped = Vec::new();
    let mut errors = Vec::new();
    for name in names {
        match stop_named_daemon(&name, force) {
            Ok(_) => stopped.push(name),
            Err(error) => errors.push(format!("{name}: {error:#}")),
        }
    }
    if !errors.is_empty() {
        bail!("Some daemons could not be stopped: {}", errors.join("; "));
    }
    Ok(stopped)
}

fn read_daemon_discoveries() -> Result<Vec<(PathBuf, DaemonDiscovery)>> {
    let mut paths = BTreeSet::new();
    for path in crate::daemon_discovery_paths() {
        if let Some(parent) = path.parent()
            && parent.is_dir()
        {
            for entry in fs::read_dir(parent)? {
                let entry = entry?;
                let path = entry.path();
                if path
                    .file_name()
                    .and_then(OsStr::to_str)
                    .is_some_and(|name| {
                        name == "daemon.json"
                            || (name.starts_with("daemon-") && name.ends_with(".json"))
                    })
                {
                    paths.insert(path);
                }
            }
        }
        paths.insert(path);
    }
    let mut result = Vec::new();
    for path in paths {
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<DaemonDiscovery>(&text) else {
            continue;
        };
        result.push((path, value));
    }
    result.sort_by(|left, right| left.1.name.cmp(&right.1.name));
    Ok(result)
}

fn daemon_endpoint(daemon: &DaemonDiscovery) -> Result<SocketAddr> {
    let endpoint = format!("{}:{}", daemon.host, daemon.control_port);
    let address: SocketAddr = endpoint
        .parse()
        .with_context(|| format!("Invalid daemon endpoint {endpoint}"))?;
    if !address.ip().is_loopback() {
        bail!("Daemon endpoint is not loopback: {address}");
    }
    Ok(address)
}

fn resolve_studio_file(
    file: Option<&Path>,
    project: Option<&Path>,
    allow_build: bool,
) -> Result<Option<PathBuf>> {
    if let Some(file) = file {
        let file = absolute_path(file);
        if !file.is_file() {
            bail!("Studio file does not exist: {}", file.display());
        }
        if !matches!(
            file.extension()
                .and_then(OsStr::to_str)
                .map(str::to_ascii_lowercase)
                .as_deref(),
            Some("rbxl" | "rbxlx" | "rbxm" | "rbxmx")
        ) {
            bail!("Studio files must end in .rbxl, .rbxlx, .rbxm, or .rbxmx");
        }
        return Ok(Some(file));
    }
    let loaded = project_config::load_project(project, None)?;
    project_config::validate_project(&loaded)?;
    let mut candidates = WalkDir::new(&loaded.root)
        .into_iter()
        .filter_entry(|entry| {
            !matches!(
                entry.file_name().to_str(),
                Some(".git" | ".renium" | ".renium-cache" | "node_modules" | "snapshots")
            )
        })
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_file())
        .map(|entry| entry.into_path())
        .filter(|path| {
            matches!(
                path.extension()
                    .and_then(OsStr::to_str)
                    .map(str::to_ascii_lowercase)
                    .as_deref(),
                Some("rbxl" | "rbxlx" | "rbxm" | "rbxmx")
            )
        })
        .collect::<Vec<_>>();
    candidates.sort();
    if candidates.len() == 1 {
        return Ok(Some(candidates.remove(0)));
    }
    if !candidates.is_empty() {
        bail!(
            "More than one Studio file exists under {}: {}",
            loaded.root.display(),
            candidates
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    let output = loaded.root.join(".renium/build/studio.rbxl");
    if !allow_build {
        return Ok(None);
    }
    build_once(
        &loaded,
        &BuildArgs {
            output: Some(output.clone()),
            project: Some(loaded.path.clone()),
            watch: false,
            sourcemap: true,
            plugin: false,
            target: None,
            wally: ToolPolicy::Auto,
            typescript: ToolPolicy::Auto,
        },
        &output,
        false,
        None,
    )?;
    Ok(Some(output))
}

fn studio_executable() -> Result<PathBuf> {
    if cfg!(windows) {
        let local = env::var_os("LOCALAPPDATA").context("LOCALAPPDATA is not set")?;
        let versions = PathBuf::from(local).join("Roblox/Versions");
        let mut candidates = fs::read_dir(&versions)
            .with_context(|| format!("Failed to inspect {}", versions.display()))?
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path().join("RobloxStudioBeta.exe"))
            .filter(|path| path.is_file())
            .collect::<Vec<_>>();
        candidates.sort_by_key(|path| {
            fs::metadata(path)
                .and_then(|metadata| metadata.modified())
                .ok()
        });
        return candidates
            .pop()
            .context("Roblox Studio is not installed in the local Versions directory");
    }
    if cfg!(target_os = "macos") {
        let mut candidates = Vec::new();
        #[cfg(target_os = "macos")]
        {
            if let Ok(managed) = crate::studio_native_serializer::managed_studio_path() {
                candidates.push(managed.join("Contents/MacOS/ReniumStudio"));
            }
        }
        candidates.push(PathBuf::from(
            "/Applications/RobloxStudio.app/Contents/MacOS/RobloxStudio",
        ));
        if let Some(home) = env::var_os("HOME") {
            candidates.push(
                PathBuf::from(home)
                    .join("Applications/RobloxStudio.app/Contents/MacOS/RobloxStudio"),
            );
        }
        for path in candidates {
            if path.is_file() {
                return Ok(path);
            }
        }
        bail!("Roblox Studio was not found in the managed or standard application locations");
    }
    bail!("Roblox Studio is only available on Windows and macOS")
}

fn validate_experience_upload(root: &Path, universe_id: u64, place_id: u64) -> Result<()> {
    let path = root.join("renium.experience.json");
    if !path.is_file() {
        return Ok(());
    }
    let value: Value = serde_json::from_slice(&fs::read(&path)?)
        .with_context(|| format!("Invalid {}", path.display()))?;
    let configured_universe = value
        .get("gameId")
        .and_then(Value::as_u64)
        .context("renium.experience.json is missing gameId")?;
    if configured_universe != 0 && configured_universe != universe_id {
        bail!(
            "Universe ID {universe_id} does not match renium.experience.json gameId {configured_universe}"
        );
    }
    let places = value
        .get("places")
        .and_then(Value::as_object)
        .context("renium.experience.json is missing places")?;
    let known = places
        .values()
        .filter_map(|place| place.get("placeId").and_then(Value::as_u64))
        .any(|configured| configured == place_id);
    if !known {
        bail!("Place ID {place_id} is not listed in {}", path.display());
    }
    Ok(())
}

fn run_checked(command: &mut Command, label: &str) -> Result<()> {
    command.stdin(Stdio::null());
    let tool = label.split_whitespace().next().unwrap_or(label);
    let output = command.output().with_context(|| {
        format!(
            "Failed to start {label}. {}",
            tool_install_action(if tool == "npm" { "node" } else { tool })
        )
    })?;
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
        bail!(
            "{label} exited with {}. {}{}",
            output.status,
            tool_install_action(if tool == "npm" { "node" } else { tool }),
            if detail.is_empty() {
                String::new()
            } else {
                format!(": {detail}")
            }
        );
    }
    for bytes in [&output.stdout, &output.stderr] {
        let text = String::from_utf8_lossy(bytes);
        for line in text.lines().filter(|line| !line.trim().is_empty()) {
            crate::log_global(3, format_args!("{label}: {line}"));
        }
    }
    Ok(())
}

fn find_command(name: &str) -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    let extensions = if cfg!(windows) {
        vec![".exe", ".cmd", ".bat", ""]
    } else {
        vec![""]
    };
    for directory in env::split_paths(&path) {
        for extension in &extensions {
            let candidate = directory.join(format!("{name}{extension}"));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

fn tool_install_action(tool: &str) -> String {
    match tool {
        "git" => "Install Git and restart the terminal".to_string(),
        "node" => "Install the Node.js version listed in .node-version".to_string(),
        "wally" | "lune" => format!("Install {tool} through Aftman or add it to PATH"),
        "rbxtsc" => "Install roblox-ts in the project when TypeScript is used".to_string(),
        _ => format!("Install {tool} or add it to PATH"),
    }
}

fn relative_display(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn absolute_path(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    }
}

fn safe_file_stem(value: &str) -> String {
    let mut output = String::new();
    let mut pending_separator = false;
    for character in value.to_ascii_lowercase().chars() {
        if character.is_ascii_alphanumeric() {
            if pending_separator && !output.is_empty() {
                output.push('-');
            }
            output.push(character);
            pending_separator = false;
        } else {
            pending_separator = !output.is_empty();
        }
    }
    output
}

fn replace_file(source: &Path, target: &Path) -> Result<()> {
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }
    #[cfg(not(windows))]
    {
        return fs::rename(source, target)
            .with_context(|| format!("Failed to replace {}", target.display()));
    }
    #[cfg(windows)]
    {
        let backup = target.with_file_name(format!(
            ".{}.previous",
            target
                .file_name()
                .and_then(OsStr::to_str)
                .unwrap_or("renium")
        ));
        if !target.exists() && backup.is_file() {
            fs::rename(&backup, target).with_context(|| {
                format!(
                    "Failed to recover {} from {}",
                    target.display(),
                    backup.display()
                )
            })?;
        }
        if !target.exists() {
            return fs::rename(source, target)
                .with_context(|| format!("Failed to replace {}", target.display()));
        }
        if backup.is_file() {
            fs::remove_file(&backup)?;
        }
        fs::rename(target, &backup)
            .with_context(|| format!("Failed to preserve {}", target.display()))?;
        if let Err(error) = fs::rename(source, target) {
            if let Err(restore_error) = fs::rename(&backup, target) {
                return Err(error)
                    .with_context(|| format!("Failed to replace {}", target.display()))
                    .context(format!(
                        "Failed to restore {} from {}: {restore_error}",
                        target.display(),
                        backup.display()
                    ));
            }
            return Err(error).with_context(|| format!("Failed to replace {}", target.display()));
        }
        if !target.is_file() {
            bail!("Replacement target was not committed: {}", target.display());
        }
        if let Err(error) = fs::remove_file(&backup) {
            crate::log_global(
                2,
                format_args!(
                    "Build succeeded, but backup cleanup failed for {}: {error}",
                    backup.display()
                ),
            );
        }
        Ok(())
    }
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let temporary = path.with_file_name(format!(
        ".{}.{}.tmp",
        path.file_name().and_then(OsStr::to_str).unwrap_or("renium"),
        std::process::id()
    ));
    {
        let mut file = fs::File::create(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    replace_file(&temporary, path)
}
