use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand, ValueEnum};
use serde_json::{Map, Value, json};

use super::{CloudIdentity, execute_one, execute_with_identity};
use crate::app;
use crate::automation::Failure;
use crate::project::config;
use crate::project::experience::{
    AmbiguousExperiencePlace, resolve_experience_game_id, resolve_experience_place,
};
use crate::system::files::absolutize_for_daemon as absolute_path;

#[derive(Args)]
pub(crate) struct OpenCloudArgs {
    #[arg(long, global = true, default_value = "ROBLOX_API_KEY")]
    key_env: String,
    #[arg(long, global = true, value_name = "ENV")]
    oauth_env: Option<String>,
    #[arg(long, global = true)]
    anonymous: bool,
    #[arg(long, global = true, value_name = "ID")]
    universe: Option<i64>,
    #[arg(long, global = true, value_name = "ID")]
    place_id: Option<i64>,
    #[command(subcommand)]
    command: OpenCloudCommand,
}

#[derive(Subcommand)]
enum OpenCloudCommand {
    #[command(about = "Show the active API key's scopes and resource limits")]
    Key,
    #[command(about = "List native Open Cloud operations")]
    Routes(super::routes::RoutesArgs),
    #[command(about = "Manage persistent data stores")]
    Data(super::routes::RouteArgs),
    #[command(about = "Manage ordered data stores")]
    Ordered(super::routes::RouteArgs),
    #[command(about = "Manage queues and sorted memory maps")]
    Memory(super::routes::RouteArgs),
    #[command(about = "Read or update the current universe")]
    Universe(super::routes::RouteArgs),
    #[command(about = "Read or update the current place")]
    Place(super::routes::RouteArgs),
    #[command(about = "Manage user restrictions")]
    Restriction(super::routes::RouteArgs),
    #[command(about = "Manage universe secrets")]
    Secret(super::routes::RouteArgs),
    #[command(about = "Send experience notifications")]
    Notification(super::routes::RouteArgs),
    #[command(about = "Manage advertising campaigns")]
    Advertising(super::routes::RouteArgs),
    #[command(about = "Query experience analytics")]
    Analytics(super::routes::RouteArgs),
    #[command(about = "Generate user avatar thumbnails")]
    Avatar(super::routes::RouteArgs),
    #[command(about = "Manage experience badges")]
    Badge(super::routes::RouteArgs),
    #[command(about = "Manage experience experiments")]
    Experiment(super::routes::RouteArgs),
    #[command(about = "Manage experience events")]
    Event(super::routes::RouteArgs),
    #[command(about = "Use Roblox generative services")]
    Ai(super::routes::RouteArgs),
    #[command(about = "Manage matchmaking configuration")]
    Matchmaking(super::routes::RouteArgs),
    #[command(about = "Manage personalized thumbnails")]
    Thumbnail(super::routes::RouteArgs),
    #[command(about = "Read users, inventories, and subscriptions")]
    User(super::routes::RouteArgs),
    #[command(about = "Manage groups and memberships")]
    Group(super::routes::RouteArgs),
    #[command(about = "Manage localized experience content")]
    Localization(super::routes::RouteArgs),
    #[command(about = "Manage followed experiences")]
    Interaction(super::routes::RouteArgs),
    #[command(about = "Manage Team Create")]
    Team(super::routes::RouteArgs),
    #[command(about = "Manage uploaded assets")]
    Asset(super::routes::RouteArgs),
    #[command(name = "creator-store", about = "Manage Creator Store products")]
    CreatorStore(super::routes::RouteArgs),
    #[command(about = "Manage game passes")]
    Pass(super::routes::RouteArgs),
    #[command(about = "Manage experience configuration repositories")]
    Config(super::routes::RouteArgs),
    #[command(about = "Run Open Cloud Luau tasks")]
    Luau(super::routes::RouteArgs),
    #[command(about = "Manage live experience servers")]
    Server(super::routes::RouteArgs),
    #[command(about = "Call any Roblox Open Cloud endpoint")]
    Request(Box<OpenCloudRequestArgs>),
    #[command(about = "Run a batch from JSON on stdin or disk")]
    Batch(OpenCloudBatchArgs),
    #[command(subcommand, about = "Manage developer products")]
    Product(super::products::DeveloperProductCommand),
    #[command(about = "Upload images through Open Cloud")]
    ImageUpload(ImageUploadArgs),
}

#[derive(Clone, Copy, ValueEnum)]
enum CloudMethod {
    Get,
    Head,
    Post,
    Put,
    Patch,
    Delete,
}

impl CloudMethod {
    fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Head => "HEAD",
            Self::Post => "POST",
            Self::Put => "PUT",
            Self::Patch => "PATCH",
            Self::Delete => "DELETE",
        }
    }
}

#[derive(Args)]
struct OpenCloudRequestArgs {
    #[arg(ignore_case = true)]
    method: CloudMethod,
    path: String,
    #[arg(long = "param", value_name = "NAME=VALUE")]
    path_params: Vec<String>,
    #[arg(short, long, value_name = "NAME=VALUE")]
    query: Vec<String>,
    #[arg(long, value_name = "NAME=VALUE")]
    field: Vec<String>,
    #[arg(long, value_name = "NAME=VALUE")]
    form: Vec<String>,
    #[arg(long = "json-part", value_name = "NAME=JSON")]
    json_parts: Vec<String>,
    #[arg(long = "url-field", value_name = "NAME=VALUE")]
    url_encoded: Vec<String>,
    #[arg(long, value_name = "NAME=PATH")]
    file: Vec<String>,
    #[arg(long, value_name = "PATH")]
    body_file: Option<PathBuf>,
    #[arg(long, value_name = "MIME")]
    content_type: Option<String>,
    #[arg(short, long, value_name = "PATH")]
    output: Option<PathBuf>,
    #[arg(long, value_name = "NAME=VALUE")]
    header: Vec<String>,
    #[arg(short = 'J', long = "json", value_name = "FILE|-")]
    json: Option<String>,
    #[arg(long)]
    if_match: Option<String>,
    #[arg(long)]
    if_none_match: Option<String>,
}

#[derive(Args)]
struct OpenCloudBatchArgs {
    #[arg(short = 'J', long = "json", value_name = "FILE|-")]
    json: String,
}

#[derive(Args)]
struct ImageUploadArgs {
    #[arg(required = true, num_args = 1..)]
    images: Vec<String>,
    #[arg(long, value_name = "ID")]
    user: Option<u64>,
    #[arg(long, value_name = "ID")]
    group: Option<u64>,
    #[arg(long)]
    name: Option<String>,
    #[arg(long, default_value = "")]
    description: String,
    #[arg(long, default_value_t = 30.0)]
    wait_seconds: f64,
}

pub(crate) fn run(args: OpenCloudArgs, project: Option<&Path>) -> Result<()> {
    let identity = discover_identity(project, args.universe, args.place_id)?;
    let key_env = args.key_env.clone();
    let oauth_env = args.oauth_env.clone();
    let anonymous = args.anonymous;
    let native = |category, route| {
        run_route(
            category,
            identity,
            &key_env,
            oauth_env.as_deref(),
            anonymous,
            route,
        )
    };
    let result = match args.command {
        OpenCloudCommand::Key => {
            if anonymous || oauth_env.is_some() {
                bail!("cloud key requires an API key");
            }
            super::introspect_key(&key_env).map_err(cloud_error)?
        }
        OpenCloudCommand::Routes(routes) => super::routes::list(routes)?,
        OpenCloudCommand::Data(route) => native("data", route)?,
        OpenCloudCommand::Ordered(route) => native("ordered", route)?,
        OpenCloudCommand::Memory(route) => native("memory", route)?,
        OpenCloudCommand::Universe(route) => native("universe", route)?,
        OpenCloudCommand::Place(route) => native("place", route)?,
        OpenCloudCommand::Restriction(route) => native("restriction", route)?,
        OpenCloudCommand::Secret(route) => native("secret", route)?,
        OpenCloudCommand::Notification(route) => native("notification", route)?,
        OpenCloudCommand::Advertising(route) => native("advertising", route)?,
        OpenCloudCommand::Analytics(route) => native("analytics", route)?,
        OpenCloudCommand::Avatar(route) => native("avatar", route)?,
        OpenCloudCommand::Badge(route) => native("badge", route)?,
        OpenCloudCommand::Experiment(route) => native("experiment", route)?,
        OpenCloudCommand::Event(route) => native("event", route)?,
        OpenCloudCommand::Ai(route) => native("ai", route)?,
        OpenCloudCommand::Matchmaking(route) => native("matchmaking", route)?,
        OpenCloudCommand::Thumbnail(route) => native("thumbnail", route)?,
        OpenCloudCommand::User(route) => native("user", route)?,
        OpenCloudCommand::Group(route) => native("group", route)?,
        OpenCloudCommand::Localization(route) => native("localization", route)?,
        OpenCloudCommand::Interaction(route) => native("interaction", route)?,
        OpenCloudCommand::Team(route) => native("team", route)?,
        OpenCloudCommand::Asset(route) => native("asset", route)?,
        OpenCloudCommand::CreatorStore(route) => native("creator-store", route)?,
        OpenCloudCommand::Pass(route) => native("pass", route)?,
        OpenCloudCommand::Config(route) => native("config", route)?,
        OpenCloudCommand::Luau(route) => native("luau", route)?,
        OpenCloudCommand::Server(route) => native("server", route)?,
        OpenCloudCommand::Request(request) => request_command(
            identity,
            &args.key_env,
            args.oauth_env.as_deref(),
            args.anonymous,
            *request,
        )?,
        OpenCloudCommand::Batch(batch) => batch_command(
            identity,
            &args.key_env,
            args.oauth_env.as_deref(),
            args.anonymous,
            batch,
        )?,
        OpenCloudCommand::Product(command) => {
            if args.anonymous {
                bail!("Developer product commands require API key or OAuth authentication");
            }
            let universe = identity.game_id.context(
                "No universe ID is available. Run this in a Renium experience or pass --universe ID",
            )?;
            super::products::run(
                CloudIdentity {
                    game_id: Some(universe),
                    place_id: identity.place_id,
                },
                &args.key_env,
                args.oauth_env.as_deref(),
                command,
            )?
        }
        OpenCloudCommand::ImageUpload(upload) => {
            if args.anonymous {
                bail!("Image upload requires API key or OAuth authentication");
            }
            if upload.user.is_some() == upload.group.is_some() {
                bail!("Image upload requires exactly one of --user ID or --group ID");
            }
            let root = config::try_load_project(project, None)?
                .map_or_else(std::env::current_dir, |loaded| Ok(loaded.root))?;
            super::assets::upload(
                &root,
                &json!({
                    "images": upload.images,
                    "userId": upload.user,
                    "groupId": upload.group,
                    "name": upload.name,
                    "description": upload.description,
                    "keyEnv": args.key_env,
                    "oauthEnv": args.oauth_env,
                    "waitSeconds": upload.wait_seconds,
                    "via": "open-cloud",
                }),
                None,
            )
            .map_err(cloud_error)?
        }
    };
    app::output::print_json_output(&result, true)
}

fn run_route(
    category: &str,
    identity: CloudIdentity,
    key_env: &str,
    oauth_env: Option<&str>,
    anonymous: bool,
    route: super::routes::RouteArgs,
) -> Result<Value> {
    if anonymous {
        bail!("Native Open Cloud commands require API key or OAuth authentication");
    }
    super::routes::run(category, identity, key_env, oauth_env, route)
}

fn discover_identity(
    project: Option<&Path>,
    universe: Option<i64>,
    place_id: Option<i64>,
) -> Result<CloudIdentity> {
    let mut identity = CloudIdentity {
        game_id: universe.filter(|id| *id > 0),
        place_id: place_id.filter(|id| *id > 0),
    };
    if identity.game_id.is_some() && identity.place_id.is_some() {
        return Ok(identity);
    }
    let Some(loaded) = config::try_load_project(project, None)? else {
        return Ok(identity);
    };
    if identity.game_id.is_none() {
        identity.game_id = resolve_experience_game_id(&loaded.root)?;
    }
    if identity.place_id.is_none() {
        let selector = app::context::place_selector();
        match resolve_experience_place(&loaded.root, selector.as_deref()) {
            Ok(Some(place)) => identity.place_id = place.place_id,
            Ok(None) => {}
            Err(error) if error.downcast_ref::<AmbiguousExperiencePlace>().is_some() => {}
            Err(error) => return Err(error),
        }
    }
    Ok(identity)
}

fn request_command(
    identity: CloudIdentity,
    key_env: &str,
    oauth_env: Option<&str>,
    anonymous: bool,
    args: OpenCloudRequestArgs,
) -> Result<Value> {
    if args.json.is_some() && !args.field.is_empty() {
        bail!("Use either --json or --field, not both");
    }
    if args.body_file.is_some() && (args.json.is_some() || !args.field.is_empty()) {
        bail!("Use either --body-file, --json, or --field");
    }
    let body = match args.json.as_deref() {
        Some(source) => Some(read_json(source)?),
        None if !args.field.is_empty() => Some(Value::Object(assignments(&args.field)?)),
        None => None,
    };
    let mut files = assignments(&args.file)?;
    for value in files.values_mut() {
        let path = value.as_str().context("--file values must be paths")?;
        *value = Value::String(absolute_path(Path::new(path)).display().to_string());
    }
    let raw_file = args
        .body_file
        .map(|path| absolute_path(&path).display().to_string());
    let output_file = args
        .output
        .map(|path| absolute_path(&path).display().to_string());
    let request = json!({
        "method": args.method.as_str(),
        "path": args.path,
        "pathParams": assignments(&args.path_params)?,
        "query": assignments(&args.query)?,
        "body": body,
        "form": assignments(&args.form)?,
        "jsonParts": json_assignments(&args.json_parts)?,
        "urlEncoded": assignments(&args.url_encoded)?,
        "files": files,
        "rawFile": raw_file,
        "contentType": args.content_type,
        "outputFile": output_file,
        "headers": assignments(&args.header)?,
        "ifMatch": args.if_match,
        "ifNoneMatch": args.if_none_match,
    });
    execute_one(identity, key_env, oauth_env, anonymous, request).map_err(cloud_error)
}

fn batch_command(
    identity: CloudIdentity,
    key_env: &str,
    oauth_env: Option<&str>,
    anonymous: bool,
    args: OpenCloudBatchArgs,
) -> Result<Value> {
    let mut batch = read_json(&args.json)?;
    let object = batch
        .as_object_mut()
        .context("Cloud batch must be an object")?;
    object
        .entry("keyEnv")
        .or_insert_with(|| Value::String(key_env.to_string()));
    if let Some(oauth_env) = oauth_env {
        object
            .entry("oauthEnv")
            .or_insert_with(|| Value::String(oauth_env.to_string()));
    }
    object.entry("anonymous").or_insert(Value::Bool(anonymous));
    execute_with_identity(identity, &batch).map_err(cloud_error)
}

pub(crate) fn cloud_error(failure: Failure) -> anyhow::Error {
    match failure.0.d {
        Some(detail) => anyhow::anyhow!("{}\n{}", failure.0.m, detail),
        None => anyhow::anyhow!(failure.0.m),
    }
}

fn read_json(source: &str) -> Result<Value> {
    let text = if source == "-" {
        let mut text = String::new();
        io::stdin().read_to_string(&mut text)?;
        text
    } else {
        fs::read_to_string(source).with_context(|| format!("Failed to read {source}"))?
    };
    serde_json::from_str(&text).with_context(|| format!("Invalid JSON in {source}"))
}

fn assignments(values: &[String]) -> Result<Map<String, Value>> {
    values
        .iter()
        .map(|assignment| {
            let (name, value) = assignment
                .split_once('=')
                .with_context(|| format!("Expected NAME=VALUE, got '{assignment}'"))?;
            if name.is_empty() {
                bail!("Assignment names cannot be empty");
            }
            let value =
                serde_json::from_str(value).unwrap_or_else(|_| Value::String(value.to_string()));
            Ok((name.to_string(), value))
        })
        .collect()
}

fn json_assignments(values: &[String]) -> Result<Map<String, Value>> {
    values
        .iter()
        .map(|assignment| {
            let (name, value) = assignment
                .split_once('=')
                .with_context(|| format!("Expected NAME=JSON, got '{assignment}'"))?;
            if name.is_empty() {
                bail!("Assignment names cannot be empty");
            }
            let value = serde_json::from_str(value)
                .with_context(|| format!("Invalid JSON for multipart field {name}"))?;
            Ok((name.to_string(), value))
        })
        .collect()
}
