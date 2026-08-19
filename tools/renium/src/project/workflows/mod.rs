use std::collections::BTreeSet;
use std::env;
use std::ffi::OsStr;
use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand, ValueEnum};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use walkdir::WalkDir;

use crate::cli::{ProjectSourceArgs, SyncWallyPackagesArgs};
use crate::project::config::{self, LoadedProject, PROJECT_FILE_NAME};
use crate::system::files::{
    absolutize_for_daemon as absolute_path, atomic_write_file, ends_with_ignore_ascii_case,
    exact_path_key as path_text, write_bytes_if_changed,
};

mod build_watch;

use build_watch::{package_uses_roblox_ts, roblox_ts_command, should_run_tool, watch_build};

const CLI_DOCS: &str = include_str!("../../../README.md");
const AGENT_POINTER: &str =
    include_str!("../../../../renium-vscode-extension/resources/RENIUM.pointer.md");
const AGENT_INSTRUCTIONS_FILE: &str = "renium-agents.md";
const AGENT_GUIDES_DIRECTORY: &str = "renium-guides";
const PROJECT_INSTRUCTIONS_FILE: &str = "RENIUM.md";
const PROJECT_GUIDES_DIRECTORY: &str = "RENIUM";

#[derive(Args)]
pub struct InitArgs {
    #[arg(default_value = ".")]
    pub path: PathBuf,
    #[arg(long, value_delimiter = ',', value_name = "FEATURE")]
    pub with: Vec<InitFeature>,
    #[arg(long)]
    pub preview: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum, PartialOrd, Ord)]
pub enum InitFeature {
    Git,
    Wally,
    Selene,
    Docs,
    RobloxTs,
}

#[derive(Clone, Args)]
pub struct BuildArgs {
    #[arg(short, long, value_name = "PATH")]
    pub output: Option<PathBuf>,
    #[arg(long)]
    pub project: Option<PathBuf>,
    #[arg(long)]
    pub watch: bool,
    #[arg(long)]
    pub sourcemap: bool,
    #[arg(long)]
    pub plugin: bool,
    #[arg(long, value_name = "SERVICE.INSTANCE")]
    pub target: Option<String>,
    #[arg(long, value_enum, default_value_t = ToolPolicy::Auto)]
    pub wally: ToolPolicy,
    #[arg(long = "ts", value_enum, default_value_t = ToolPolicy::Auto)]
    pub typescript: ToolPolicy,
}

#[derive(Clone, Copy, PartialEq, ValueEnum)]
pub enum ToolPolicy {
    Auto,
    Always,
    Never,
}

#[derive(Args)]
pub struct DoctorArgs {
    #[arg(long, default_value = ".")]
    pub root: PathBuf,
    #[arg(long)]
    pub project: Option<PathBuf>,
    #[arg(long)]
    pub json: bool,
    #[arg(long, value_name = "PATH")]
    pub bundle: Option<PathBuf>,
}

#[derive(Args)]
pub struct DocsArgs {
    pub topic: Option<String>,
    #[arg(long)]
    pub serve: bool,
    #[arg(long, default_value_t = 0)]
    pub port: u16,
}

#[derive(Args)]
pub struct DaemonArgs {
    #[command(subcommand)]
    pub command: DaemonCommand,
}

#[derive(Subcommand)]
pub enum DaemonCommand {
    List,
    Status(DaemonTargetArgs),
    Stop(DaemonTargetArgs),
    Clean,
}

#[derive(Args)]
pub struct DaemonTargetArgs {
    #[arg(default_value = "default")]
    pub name: String,
    #[arg(long)]
    pub all: bool,
    #[arg(long)]
    pub force: bool,
}

#[derive(Args)]
pub struct StudioArgs {
    pub file: Option<PathBuf>,
    #[arg(long)]
    pub project: Option<PathBuf>,
    #[arg(long)]
    pub check: bool,
}

#[derive(Args)]
pub struct UploadArgs {
    #[arg(short, long, value_name = "PATH")]
    pub input: Option<PathBuf>,
    #[arg(long)]
    pub place_id: Option<u64>,
    #[arg(long)]
    pub universe_id: Option<u64>,
    #[arg(long, default_value = "ROBLOX_API_KEY")]
    pub api_key_env: String,
    #[arg(long)]
    pub project: Option<PathBuf>,
}

#[derive(Serialize)]
struct InitPlan {
    root: PathBuf,
    create: Vec<String>,
    update: Vec<String>,
    keep: Vec<String>,
    directories: Vec<String>,
}

#[derive(Serialize)]
struct DoctorCheck {
    name: String,
    status: &'static str,
    detail: String,
    action: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DaemonDiscovery {
    #[serde(default = "default_daemon_name")]
    name: String,
    host: String,
    control_port: u16,
    #[serde(default)]
    bridge_ports: Vec<u16>,
    pid: u32,
}

fn default_daemon_name() -> String {
    "default".to_string()
}

pub fn run_init(args: InitArgs) -> Result<()> {
    let root = absolute_path(&args.path);
    let source_root = detect_init_source_root(&root)?;
    let files = init_files(&root, &source_root, &args.with)?;
    let source_directory = root.join(&source_root);
    validate_init_output_types(&files, &source_directory)?;
    let mut create = Vec::new();
    let mut update = Vec::new();
    let mut update_paths = Vec::new();
    let mut keep = Vec::new();
    for (path, bytes) in &files {
        if path.exists() {
            if should_update_instruction_file(path, bytes)? {
                update.push(relative_display(&root, path));
                update_paths.push(path.clone());
            } else {
                keep.push(relative_display(&root, path));
            }
        } else {
            create.push(relative_display(&root, path));
        }
    }
    let plan = InitPlan {
        root: root.clone(),
        create,
        update,
        keep,
        directories: (!source_directory.exists())
            .then(|| relative_display(&root, &source_directory))
            .into_iter()
            .collect(),
    };
    if args.preview {
        return crate::emit_global_output(
            &serde_json::to_value(&plan)?,
            &format_init_plan("Would initialize", &plan, false),
        );
    }
    let root_existed = root.exists();
    let git_was_absent = args.with.contains(&InitFeature::Git) && !root.join(".git").exists();
    let node_modules_was_absent =
        args.with.contains(&InitFeature::RobloxTs) && !root.join("node_modules").exists();
    let package_locks = if args.with.contains(&InitFeature::RobloxTs) {
        [
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
        .collect::<Result<Vec<_>>>()?
    } else {
        Vec::new()
    };
    let mut created_files = Vec::new();
    let mut replaced_files = Vec::new();
    let mut created_directories = Vec::new();
    let result = (|| -> Result<()> {
        if !root_existed {
            fs::create_dir_all(&root)
                .with_context(|| format!("Failed to create {}", root.display()))?;
            created_directories.push(root.clone());
        }
        for (path, bytes) in files {
            if path.exists() {
                if update_paths.contains(&path) {
                    let original = fs::read(&path)
                        .with_context(|| format!("Failed to read {}", path.display()))?;
                    atomic_write_file(&path, &bytes)?;
                    replaced_files.push((path, original));
                }
                continue;
            }
            if let Some(parent) = path.parent() {
                create_directories_tracked(parent, &mut created_directories)?;
            }
            atomic_write_file(&path, &bytes)?;
            created_files.push(path);
        }
        if !source_directory.exists() {
            create_directories_tracked(&source_directory, &mut created_directories)?;
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
                let _ = atomic_write_file(&path, &original);
            } else {
                let _ = fs::remove_file(path);
            }
        }
        for path in created_files.into_iter().rev() {
            let _ = fs::remove_file(path);
        }
        for (path, original) in replaced_files.into_iter().rev() {
            let _ = atomic_write_file(&path, &original);
        }
        created_directories.sort_by_key(|path| std::cmp::Reverse(path.components().count()));
        for path in created_directories {
            let _ = fs::remove_dir(path);
        }
        return Err(error);
    }
    crate::emit_global_output(
        &serde_json::to_value(&plan)?,
        &format_init_plan("Initialized", &plan, true),
    )
}

fn validate_init_output_types(files: &[(PathBuf, Vec<u8>)], source_directory: &Path) -> Result<()> {
    for (path, _) in files {
        if path.exists() && !path.is_file() {
            bail!("Initialization requires a file at {}", path.display());
        }
    }
    if source_directory.exists() && !source_directory.is_dir() {
        bail!(
            "Initialization requires a directory at {}",
            source_directory.display()
        );
    }
    Ok(())
}

fn format_init_plan(action: &str, plan: &InitPlan, completed: bool) -> String {
    let (create, update, keep, directories) = if completed {
        ("created", "updated", "kept", "created directories")
    } else {
        ("create", "update", "keep", "create directories")
    };
    format!(
        "{action} {}: {create} {}, {update} {}, {keep} {}, {directories} {}",
        plan.root.display(),
        format_init_paths(&plan.create),
        format_init_paths(&plan.update),
        format_init_paths(&plan.keep),
        format_init_paths(&plan.directories),
    )
}

fn format_init_paths(paths: &[String]) -> String {
    format!("{} [{}]", paths.len(), paths.join(", "))
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
    crate::app::timing::set_quiet_timings(true);
    crate::set_global_stream_output(args.watch);
    let mut loaded = config::load_project(args.project.as_deref().or(global_project), None)?;
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
    let explicit_project = args.project.as_deref().or(global_project);
    let project_marker_exists = project_marker_exists_from(&root);
    let mut bundle_project = None;
    match config::load_project(explicit_project, Some(&root)) {
        Ok(project) => {
            bundle_project = Some(project.path.clone());
            match config::validate_project(&project)
                .and_then(|()| config::stage_project(&project).map(drop))
            {
                Ok(()) => {
                    checks.push(DoctorCheck {
                        name: "project".to_string(),
                        status: "ok",
                        detail: format!("{} compiles successfully", project.path.display()),
                        action: None,
                    });
                }
                Err(error) => checks.push(DoctorCheck {
                    name: "project".to_string(),
                    status: "error",
                    detail: format!("{error:#}"),
                    action: Some(
                        "Fix the reported project path or field, then run `rbx doctor` again"
                            .to_string(),
                    ),
                }),
            }
        }
        Err(error) => checks.push(DoctorCheck {
            name: "project".to_string(),
            status: if explicit_project.is_some() || project_marker_exists {
                "error"
            } else {
                "warn"
            },
            detail: format!("{error:#}"),
            action: Some(if explicit_project.is_some() {
                "Fix the project passed with --project, then run `rbx doctor` again".to_string()
            } else if project_marker_exists {
                "Fix the existing project file, then run `rbx doctor` again".to_string()
            } else {
                format!("Run `rbx init {}` or pass --project", root.display())
            }),
        }),
    }
    let config = config::load_merged_config(&root);
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
            detail: format!("{error:#}"),
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
    let plugin = crate::app::setup::roblox_plugins_dir()
        .map(|dir| dir.join(crate::app::setup::PLUGIN_ASSET_NAME));
    checks.push(match plugin {
        Ok(path) if path.is_file() => match fs::read(&path)
            .with_context(|| format!("Failed to read {}", path.display()))
            .and_then(|bytes| crate::app::setup::validate_rbxm(&bytes))
        {
            Ok(()) => DoctorCheck {
                name: "studioPlugin".to_string(),
                status: "ok",
                detail: format!(
                    "{} matches Renium {}",
                    path.display(),
                    crate::app::build::VERSION
                ),
                action: None,
            },
            Err(error) => DoctorCheck {
                name: "studioPlugin".to_string(),
                status: "error",
                detail: format!("{error:#}"),
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
            detail: format!("{error:#}"),
            action: None,
        },
    });
    let daemons = read_daemon_discoveries()?;
    let live_count = daemons
        .iter()
        .filter(|(_, daemon)| crate::daemon::is_process_alive(daemon.pid))
        .count();
    checks.push(DoctorCheck {
        name: "daemon".to_string(),
        status: if live_count > 0 { "ok" } else { "warn" },
        detail: format!("{live_count} running, {} discovery file(s)", daemons.len()),
        action: (live_count == 0).then(|| "Run `rbx bd` or start Renium in VS Code".to_string()),
    });
    let result = json!({
        "ok": checks.iter().all(|check| check.status != "error"),
        "version": crate::app::build::VERSION,
        "gitHash": crate::app::build::GIT_HASH,
        "root": root,
        "checks": checks,
    });
    if let Some(bundle) = args.bundle {
        write_doctor_bundle(&bundle, &result, bundle_project.as_deref())?;
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
            || current.join(config::PROJECT_JSON_FILE_NAME).is_file()
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
                if !crate::daemon::is_process_alive(daemon.pid) {
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
    let loaded = config::load_project(args.project.as_deref().or(global_project), None)?;
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
        Some(extension) if extension.eq_ignore_ascii_case("rbxl") => "application/octet-stream",
        Some(extension) if extension.eq_ignore_ascii_case("rbxlx") => "application/xml",
        _ => bail!("Place upload input must end in .rbxl or .rbxlx"),
    };
    let url = format!(
        "https://apis.roblox.com/universes/v1/{universe_id}/places/{place_id}/versions?versionType=Published"
    );
    let response = crate::cloud::upload_file(&url, api_key.trim(), content_type, &input);
    if input == temporary {
        let _ = fs::remove_file(&temporary);
    }
    let response = response?;
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
    for name in [PROJECT_FILE_NAME, config::PROJECT_JSON_FILE_NAME] {
        let path = root.join(name);
        if path.is_file() {
            return Ok(config::load_project(Some(&path), None)?.project.source_root);
        }
    }
    let mut rojo_projects = Vec::new();
    if root.is_dir() {
        for entry in fs::read_dir(root)? {
            let path = entry?.path();
            if path
                .file_name()
                .and_then(OsStr::to_str)
                .is_some_and(|name| ends_with_ignore_ascii_case(name, ".project.json"))
            {
                rojo_projects.push(path);
            }
        }
    }
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

fn init_files(
    root: &Path,
    source_root: &Path,
    features: &[InitFeature],
) -> Result<Vec<(PathBuf, Vec<u8>)>> {
    let renium_instructions = agent_instructions()?;
    let agent_instructions = merged_instruction_file(&root.join("AGENTS.md"))?;
    let claude_instructions = merged_instruction_file(&root.join("CLAUDE.md"))?;
    let name = root
        .file_name()
        .and_then(OsStr::to_str)
        .filter(|name| !name.is_empty())
        .unwrap_or("Renium project");
    let project = minimal_project_file(source_root)?;
    let mut files = vec![
        (root.join("AGENTS.md"), agent_instructions),
        (root.join(PROJECT_INSTRUCTIONS_FILE), renium_instructions),
        (root.join("CLAUDE.md"), claude_instructions),
        (root.join(PROJECT_FILE_NAME), project),
    ];
    files.extend(
        agent_guides()?
            .into_iter()
            .map(|(name, content)| (root.join(PROJECT_GUIDES_DIRECTORY).join(name), content)),
    );
    let features = features.iter().copied().collect::<BTreeSet<_>>();
    if features.contains(&InitFeature::Git) {
        files.push((root.join(".gitignore"), b"build/\nnode_modules/\n".to_vec()));
        files.push((
            root.join(".renium/.gitignore"),
            crate::project::package_links::RENIUM_DIR_GITIGNORE
                .as_bytes()
                .to_vec(),
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

fn minimal_project_file(source_root: &Path) -> Result<Vec<u8>> {
    let mut project = Map::new();
    project.insert("schemaVersion".to_string(), json!(1));
    if source_root != Path::new("src") {
        project.insert("sourceRoot".to_string(), json!(path_text(source_root)));
    }
    Ok((serde_json::to_string_pretty(&project)? + "\n").into_bytes())
}

pub(crate) fn ensure_project_file(root: &Path, source_root: &Path) -> Result<PathBuf> {
    if let Some(loaded) = config::try_load_project(None, Some(root))?
        && loaded.root == root
    {
        return Ok(loaded.path);
    }
    let path = root.join(PROJECT_FILE_NAME);
    if !path.is_file() {
        fs::create_dir_all(root).with_context(|| format!("Failed to create {}", root.display()))?;
        atomic_write_file(&path, &minimal_project_file(source_root)?)?;
    }
    Ok(path)
}

pub(crate) fn initialize_place_root(root: &Path, source_root: &Path) -> Result<PathBuf> {
    fs::create_dir_all(root.join(source_root))
        .with_context(|| format!("Failed to create {}", root.join(source_root).display()))?;
    ensure_project_file(root, source_root)
}

pub(crate) fn refresh_agent_instructions(root: &Path) -> Result<()> {
    for (path, content) in [
        (root.join(PROJECT_INSTRUCTIONS_FILE), agent_instructions()?),
        (
            root.join("AGENTS.md"),
            merged_instruction_file(&root.join("AGENTS.md"))?,
        ),
        (
            root.join("CLAUDE.md"),
            merged_instruction_file(&root.join("CLAUDE.md"))?,
        ),
    ] {
        write_bytes_if_changed(&path, &content)?;
    }
    let destination = root.join(PROJECT_GUIDES_DIRECTORY);
    fs::create_dir_all(&destination)
        .with_context(|| format!("Failed to create {}", destination.display()))?;
    for (name, content) in agent_guides()? {
        write_bytes_if_changed(&destination.join(name), &content)?;
    }
    Ok(())
}

fn agent_instructions() -> Result<Vec<u8>> {
    let installed = env::current_exe()
        .context("Failed to locate the Renium executable")?
        .parent()
        .context("The Renium executable has no parent directory")?
        .join(AGENT_INSTRUCTIONS_FILE);
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join(AGENT_INSTRUCTIONS_FILE);
    let path = [installed, source]
        .into_iter()
        .find(|path| path.is_file())
        .context("Renium is missing renium-agents.md; reinstall Renium")?;
    fs::read(&path).with_context(|| format!("Failed to read {}", path.display()))
}

fn agent_guides_directory() -> Result<PathBuf> {
    let installed = env::current_exe()
        .context("Failed to locate the Renium executable")?
        .parent()
        .context("The Renium executable has no parent directory")?
        .join(AGENT_GUIDES_DIRECTORY);
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join(AGENT_GUIDES_DIRECTORY);
    [installed, source]
        .into_iter()
        .find(|path| path.is_dir())
        .context("Renium is missing its agent topic guides; reinstall Renium")
}

fn agent_guides() -> Result<Vec<(PathBuf, Vec<u8>)>> {
    let directory = agent_guides_directory()?;
    let mut entries = fs::read_dir(&directory)
        .with_context(|| format!("Failed to read {}", directory.display()))?
        .collect::<std::io::Result<Vec<_>>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    let mut guides = Vec::with_capacity(entries.len());
    for entry in entries {
        if entry.file_type()?.is_file()
            && entry.path().extension().and_then(OsStr::to_str) == Some("md")
        {
            guides.push((PathBuf::from(entry.file_name()), fs::read(entry.path())?));
        }
    }
    Ok(guides)
}

fn merged_instruction_file(path: &Path) -> Result<Vec<u8>> {
    if !path.is_file() {
        return Ok(AGENT_POINTER.as_bytes().to_vec());
    }
    let mut current =
        fs::read_to_string(path).with_context(|| format!("Failed to read {}", path.display()))?;
    if path.file_name() == Some(OsStr::new("CLAUDE.md")) && mentions_agents_file(&current) {
        return Ok(current.into_bytes());
    }
    if current.contains(AGENT_POINTER.trim_end()) {
        return Ok(current.into_bytes());
    }
    let old_generated = current
        .trim_end()
        .lines()
        .next_back()
        .is_some_and(is_old_agent_marker);
    if old_generated {
        return Ok(AGENT_POINTER.as_bytes().to_vec());
    }
    if !current.is_empty() && !current.ends_with('\n') {
        current.push('\n');
    }
    if !current.is_empty() && !current.ends_with("\n\n") {
        current.push('\n');
    }
    current.push_str(AGENT_POINTER);
    Ok(current.into_bytes())
}

fn mentions_agents_file(text: &str) -> bool {
    text.as_bytes()
        .windows(b"agents.md".len())
        .any(|window| window.eq_ignore_ascii_case(b"agents.md"))
}

fn is_old_agent_marker(line: &str) -> bool {
    let Some(version) = line.strip_prefix("renium-") else {
        return false;
    };
    let mut parts = version.split('.');
    (0..3).all(|_| {
        parts
            .next()
            .is_some_and(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
    }) && parts.next().is_none()
}

fn should_update_instruction_file(path: &Path, replacement: &[u8]) -> Result<bool> {
    let name = path.file_name().and_then(OsStr::to_str);
    if name != Some("AGENTS.md")
        && name != Some("CLAUDE.md")
        && name != Some(PROJECT_INSTRUCTIONS_FILE)
    {
        return Ok(false);
    }
    let current = fs::read(path).with_context(|| format!("Failed to read {}", path.display()))?;
    Ok(current != replacement)
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
            config::stage_project_cached(loaded, changed_sources)?
        } else {
            config::stage_project(loaded)?
        }
    } else {
        config::stage_project(loaded)?
    };
    if args.sourcemap || loaded.root.join("sourcemap.json").exists() {
        crate::project::sourcemap::generate_project_sourcemap_for_projection(loaded, &projection)?;
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
        .map(|target| config::ProjectTarget::Shorthand(target.clone()));
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
    target: Option<&config::ProjectTarget>,
) -> Result<usize> {
    let target_segments = target.map(config::ProjectTarget::segments);
    let target_ordinals = target.map(config::ProjectTarget::ordinals);
    let mut services = if let Some(segments) = target_segments.as_ref() {
        vec![segments[0].clone()]
    } else {
        let mut services = Vec::new();
        for entry in fs::read_dir(src_root)? {
            let entry = entry?;
            if entry.file_type()?.is_dir()
                && let Some(name) = entry.file_name().to_str()
                && !name.starts_with('.')
            {
                services.push(name.to_string());
            }
        }
        services
    };
    services.sort();
    let build = crate::rbx::model::build_rbx_place(src_root, services, None, false, false, false)?;
    if let Some(parent) = output
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)?;
    }
    let file = fs::File::create(output)?;
    let mut writer = std::io::BufWriter::new(file);
    let roots = if let Some(segments) = target_segments.as_ref() {
        vec![
            crate::rbx::model::rbx_dom_instance_by_path_unique(
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
        crate::project::package_links::sync_wally_packages_result(SyncWallyPackagesArgs {
            project: ProjectSourceArgs {
                project_root: loaded.root.clone(),
                src_root: loaded.project.source_root.clone(),
            },
            manifest: PathBuf::from("wally.toml"),
            wally_path: "wally".to_string(),
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
            details: false,
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

fn write_doctor_bundle(path: &Path, result: &Value, project_path: Option<&Path>) -> Result<()> {
    let directory = if path.extension().is_some() {
        path.with_extension("")
    } else {
        path.to_path_buf()
    };
    fs::create_dir_all(&directory)?;
    atomic_write_file(
        &directory.join("doctor.json"),
        (serde_json::to_string_pretty(result)? + "\n").as_bytes(),
    )?;
    let bundled_project = directory.join(PROJECT_FILE_NAME);
    if let Some(project_path) = project_path {
        let text = fs::read_to_string(project_path)?;
        atomic_write_file(&bundled_project, text.as_bytes())?;
    } else if bundled_project.exists() {
        fs::remove_file(&bundled_project)
            .with_context(|| format!("Failed to remove {}", bundled_project.display()))?;
    }
    let environment = json!({
        "os": env::consts::OS,
        "arch": env::consts::ARCH,
        "version": crate::app::build::VERSION,
        "gitHash": crate::app::build::GIT_HASH,
    });
    atomic_write_file(
        &directory.join("environment.json"),
        (serde_json::to_string_pretty(&environment)? + "\n").as_bytes(),
    )?;
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
        .map_or(lines.len(), |(index, _)| index);
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
        let alive = crate::daemon::is_process_alive(daemon.pid);
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
    if !crate::daemon::is_process_alive(daemon.pid) {
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
    while Instant::now() < deadline && crate::daemon::is_process_alive(daemon.pid) {
        thread::sleep(Duration::from_millis(50));
    }
    if crate::daemon::is_process_alive(daemon.pid) {
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
    for path in crate::daemon::daemon_discovery_paths() {
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
    let loaded = config::load_project(project, None)?;
    config::validate_project(&loaded)?;
    let mut candidates = Vec::new();
    for entry in WalkDir::new(&loaded.root)
        .into_iter()
        .filter_entry(|entry| {
            !matches!(
                entry.file_name().to_str(),
                Some(".git" | ".renium" | "node_modules" | "snapshots")
            )
        })
    {
        let entry = entry?;
        if entry.file_type().is_file() && {
            let path = entry.path();
            matches!(
                path.extension()
                    .and_then(OsStr::to_str)
                    .map(str::to_ascii_lowercase)
                    .as_deref(),
                Some("rbxl" | "rbxlx" | "rbxm" | "rbxmx")
            )
        } {
            candidates.push(entry.into_path());
        }
    }
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
        let entries = fs::read_dir(&versions)
            .with_context(|| format!("Failed to inspect {}", versions.display()))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let mut candidates = entries
            .into_iter()
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
            if let Ok(managed) = crate::studio::native::serializer::managed_studio_path() {
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
    let extensions: &[&str] = if cfg!(windows) {
        &[".exe", ".cmd", ".bat", ""]
    } else {
        &[""]
    };
    for directory in env::split_paths(&path) {
        for extension in extensions {
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
        fs::rename(source, target)
            .with_context(|| format!("Failed to replace {}", target.display()))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::support::temp_dir;

    #[test]
    fn init_keeps_one_pointer_to_the_renium_guide() {
        let root = temp_dir("agent-instructions");
        let agents = root.join("AGENTS.md");
        let claude = root.join("CLAUDE.md");
        fs::write(&agents, "# Old guide\n\nrenium-0.1.4\n").unwrap();
        fs::write(&claude, "Read and follow AgEnTs.Md.\n").unwrap();

        run_init(InitArgs {
            path: root.clone(),
            with: Vec::new(),
            preview: false,
        })
        .unwrap();
        assert_eq!(fs::read_to_string(&agents).unwrap(), AGENT_POINTER);
        assert_eq!(
            fs::read_to_string(&claude).unwrap(),
            "Read and follow AgEnTs.Md.\n"
        );
        assert_eq!(
            fs::read(root.join(PROJECT_INSTRUCTIONS_FILE)).unwrap(),
            agent_instructions().unwrap()
        );
        let guides = agent_guides().unwrap();
        assert!(!guides.is_empty());
        for (name, content) in guides {
            assert_eq!(
                fs::read(root.join(PROJECT_GUIDES_DIRECTORY).join(name)).unwrap(),
                content
            );
        }

        fs::write(&agents, "# Project rules\n\nKeep this.\n").unwrap();
        fs::write(&claude, "# Claude rules\n").unwrap();
        run_init(InitArgs {
            path: root.clone(),
            with: Vec::new(),
            preview: false,
        })
        .unwrap();
        assert_eq!(
            fs::read_to_string(&agents).unwrap(),
            format!("# Project rules\n\nKeep this.\n\n{AGENT_POINTER}")
        );
        assert_eq!(
            fs::read_to_string(&claude).unwrap(),
            format!("# Claude rules\n\n{AGENT_POINTER}")
        );
        let before = [fs::read(&agents).unwrap(), fs::read(&claude).unwrap()];
        run_init(InitArgs {
            path: root.clone(),
            with: Vec::new(),
            preview: false,
        })
        .unwrap();
        assert_eq!(
            [fs::read(&agents).unwrap(), fs::read(&claude).unwrap()],
            before
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn place_roots_get_minimal_independent_projects() {
        let experience = temp_dir("place-projects");
        atomic_write_file(
            &experience.join(PROJECT_FILE_NAME),
            &minimal_project_file(Path::new("src")).unwrap(),
        )
        .unwrap();
        for name in ["place1", "place2"] {
            let root = experience.join("places").join(name);
            let project = initialize_place_root(&root, Path::new("src")).unwrap();
            let value: Value = serde_json::from_slice(&fs::read(project).unwrap()).unwrap();
            assert_eq!(value, json!({ "schemaVersion": 1 }));
            assert!(root.join("src").is_dir());
            assert!(!root.join("AGENTS.md").exists());
        }
        fs::remove_dir_all(experience).unwrap();
    }
}
