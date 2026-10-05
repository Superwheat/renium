use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand, ValueEnum};
use serde_json::{Map, Value, json};

use super::parameters::{absolutize_files, assignments};
use super::routes::{Access, RouteArgs};
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
    #[arg(
        long,
        global = true,
        default_value = "ROBLOX_API_KEY",
        help = "Read the API key from this environment variable"
    )]
    key_env: String,
    #[arg(
        long,
        global = true,
        value_name = "NAME",
        help = "Use this stored API key"
    )]
    key: Option<String>,
    #[arg(
        long,
        global = true,
        value_name = "ENV",
        help = "Send the OAuth token in this environment variable instead of a key"
    )]
    oauth_env: Option<String>,
    #[arg(long, global = true, help = "Send requests without credentials")]
    anonymous: bool,
    #[arg(
        long,
        global = true,
        value_name = "ID",
        help = "Universe for {universe} in paths (default: the project's experience)"
    )]
    universe: Option<i64>,
    #[arg(
        long,
        global = true,
        value_name = "ID",
        help = "Place for {place} in paths (default: the project's place; a numeric --place works too)"
    )]
    place_id: Option<i64>,
    #[command(subcommand)]
    command: OpenCloudCommand,
}

#[derive(Subcommand)]
enum OpenCloudCommand {
    #[command(
        about = "Show the active API key's scopes, or store keys with add, list, remove and use"
    )]
    Key(KeyArgs),
    #[command(about = "List the experiences the key can reach, or find one by name")]
    Games(GamesArgs),
    #[command(about = "Download a place the key can reach, optionally into a new project")]
    Fetch(FetchArgs),
    #[command(about = "List native Open Cloud operations")]
    Routes(super::routes::RoutesArgs),
    #[command(
        about = "Manage persistent data stores",
        after_help = super::routes::category_help("data")
    )]
    Data(super::routes::RouteArgs),
    #[command(
        about = "Manage ordered data stores",
        after_help = super::routes::category_help("ordered")
    )]
    Ordered(super::routes::RouteArgs),
    #[command(
        about = "Manage queues and sorted memory maps",
        after_help = super::routes::category_help("memory")
    )]
    Memory(super::routes::RouteArgs),
    #[command(
        about = "Read or update the current universe",
        after_help = super::routes::category_help("universe")
    )]
    Universe(super::routes::RouteArgs),
    #[command(
        about = "Read or update the current place",
        after_help = super::routes::category_help("place")
    )]
    Place(super::routes::RouteArgs),
    #[command(
        about = "Manage user restrictions",
        after_help = super::routes::category_help("restriction")
    )]
    Restriction(super::routes::RouteArgs),
    #[command(
        about = "Manage universe secrets",
        after_help = super::routes::category_help("secret")
    )]
    Secret(super::routes::RouteArgs),
    #[command(
        about = "Send experience notifications",
        after_help = super::routes::category_help("notification")
    )]
    Notification(super::routes::RouteArgs),
    #[command(
        about = "Manage advertising campaigns",
        after_help = super::routes::category_help("advertising")
    )]
    Advertising(super::routes::RouteArgs),
    #[command(
        about = "Query experience analytics",
        after_help = super::routes::category_help("analytics")
    )]
    Analytics(super::routes::RouteArgs),
    #[command(
        about = "Generate user avatar thumbnails",
        after_help = super::routes::category_help("avatar")
    )]
    Avatar(super::routes::RouteArgs),
    #[command(
        about = "Manage experience badges",
        after_help = super::routes::category_help("badge")
    )]
    Badge(super::routes::RouteArgs),
    #[command(
        about = "Manage experience experiments",
        after_help = super::routes::category_help("experiment")
    )]
    Experiment(super::routes::RouteArgs),
    #[command(
        about = "Manage experience events",
        after_help = super::routes::category_help("event")
    )]
    Event(super::routes::RouteArgs),
    #[command(
        about = "Use Roblox generative services",
        after_help = super::routes::category_help("ai")
    )]
    Ai(super::routes::RouteArgs),
    #[command(
        about = "Manage matchmaking configuration",
        after_help = super::routes::category_help("matchmaking")
    )]
    Matchmaking(super::routes::RouteArgs),
    #[command(
        about = "Manage personalized thumbnails",
        after_help = super::routes::category_help("thumbnail")
    )]
    Thumbnail(super::routes::RouteArgs),
    #[command(
        about = "Read users, inventories, and subscriptions",
        after_help = super::routes::category_help("user")
    )]
    User(super::routes::RouteArgs),
    #[command(
        about = "Manage groups and memberships",
        after_help = super::routes::category_help("group")
    )]
    Group(super::routes::RouteArgs),
    #[command(
        about = "Manage localized experience content",
        after_help = super::routes::category_help("localization")
    )]
    Localization(super::routes::RouteArgs),
    #[command(
        about = "Manage followed experiences",
        after_help = super::routes::category_help("interaction")
    )]
    Interaction(super::routes::RouteArgs),
    #[command(
        about = "Manage Team Create",
        after_help = super::routes::category_help("team")
    )]
    Team(super::routes::RouteArgs),
    #[command(
        about = "Manage uploaded assets",
        after_help = super::routes::category_help("asset")
    )]
    Asset(super::routes::RouteArgs),
    #[command(
        name = "creator-store",
        about = "Manage Creator Store products",
        after_help = super::routes::category_help("creator-store")
    )]
    CreatorStore(super::routes::RouteArgs),
    #[command(
        about = "Manage game passes",
        after_help = super::routes::category_help("pass")
    )]
    Pass(super::routes::RouteArgs),
    #[command(
        about = "Manage experience configuration repositories",
        after_help = super::routes::category_help("config")
    )]
    Config(super::routes::RouteArgs),
    #[command(
        about = "Run Open Cloud Luau tasks",
        after_help = super::routes::category_help("luau")
    )]
    Luau(super::routes::RouteArgs),
    #[command(
        about = "Manage live experience servers",
        after_help = super::routes::category_help("server")
    )]
    Server(super::servers::ServerArgs),
    #[command(about = "Call any Roblox Open Cloud endpoint")]
    Request(Box<OpenCloudRequestArgs>),
    #[command(about = "Run a batch from JSON on stdin or disk")]
    Batch(OpenCloudBatchArgs),
    #[command(subcommand, about = "Manage developer products")]
    Product(super::products::DeveloperProductCommand),
    #[command(about = "Upload images through Open Cloud")]
    ImageUpload(ImageUploadArgs),
}

#[derive(Args)]
struct KeyArgs {
    #[command(subcommand)]
    action: Option<KeyAction>,
}

#[derive(Subcommand)]
enum KeyAction {
    #[command(
        about = "Store a key read from stdin (hidden prompt on a terminal); the first key becomes the default"
    )]
    Add {
        #[arg(value_name = "NAME")]
        name: String,
    },
    #[command(about = "List stored keys without their secrets")]
    List,
    #[command(about = "Remove a stored key")]
    Remove {
        #[arg(value_name = "NAME")]
        name: String,
    },
    #[command(about = "Make a stored key the default")]
    Use {
        #[arg(value_name = "NAME")]
        name: String,
    },
}

#[derive(Args)]
struct GamesArgs {
    #[arg(
        value_name = "NAME",
        help = "Name, universe ID or place ID to look for"
    )]
    query: Option<String>,
}

#[derive(Args)]
struct FetchArgs {
    #[arg(
        value_name = "NAME",
        help = "Experience name; or pass --universe ID / --place-id ID"
    )]
    name: Option<String>,
    #[arg(short, long, value_name = "FILE", help = "Where to write the .rbxl")]
    output: Option<PathBuf>,
    #[arg(
        short = 'r',
        long = "project-root",
        value_name = "DIR",
        help = "Import the place into this project folder, creating it when missing"
    )]
    project_root: Option<PathBuf>,
    #[arg(
        long,
        value_name = "N",
        help = "Download this saved version instead of the current one (see `rbx oc place history`)"
    )]
    version: Option<u64>,
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
    super::keys::select(args.key.clone());
    let (selected_universe, selected_place) =
        selector_ids(app::context::place_selector().as_deref());
    let universe = args.universe.or(selected_universe);
    let place_id = args.place_id.or(selected_place);
    let identity = || discover_identity(project, universe, place_id);
    let key_env = args.key_env.clone();
    let oauth_env = args.oauth_env.clone();
    let anonymous = args.anonymous;
    let access = |identity| Access::new(identity, &key_env, oauth_env.as_deref(), anonymous);
    let native = |category, mut route: RouteArgs| {
        let action = super::routes::resolve_action(category, route.action.as_deref())?;
        route.action = Some(action.to_string());
        super::routes::run(category, &access(identity()?), route, |_| {})
    };
    let result = match args.command {
        OpenCloudCommand::Key(key) => match key.action {
            None => {
                if anonymous || oauth_env.is_some() {
                    bail!("cloud key requires an API key");
                }
                super::introspect_key(&key_env).map_err(cloud_error)?
            }
            Some(KeyAction::Add { name }) => {
                let secret = super::keys::read_secret_from_stdin()?;
                super::keys::add(&name, &secret)?
            }
            Some(KeyAction::List) => super::keys::list()?,
            Some(KeyAction::Remove { name }) => super::keys::remove(&name)?,
            Some(KeyAction::Use { name }) => super::keys::set_default(&name)?,
        },
        OpenCloudCommand::Games(games) => {
            if anonymous || oauth_env.is_some() {
                bail!("cloud games requires an API key");
            }
            super::discovery::games_command(&key_env, games.query.as_deref())?
        }
        OpenCloudCommand::Fetch(fetch) => {
            if anonymous {
                bail!("cloud fetch requires an API key");
            }
            let identity = if fetch.name.is_some() {
                CloudIdentity {
                    game_id: universe,
                    place_id,
                }
            } else {
                identity()?
            };
            super::discovery::fetch_command(
                identity,
                &key_env,
                oauth_env.as_deref(),
                super::discovery::FetchRequest {
                    name: fetch.name,
                    output: fetch.output,
                    project_root: fetch.project_root,
                    version: fetch.version,
                },
            )?
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
        OpenCloudCommand::Server(server) => {
            let server = super::servers::prepare(server)?;
            super::servers::run(&access(identity()?), server)?
        }
        OpenCloudCommand::Request(request) => request_command(
            identity()?,
            &args.key_env,
            args.oauth_env.as_deref(),
            args.anonymous,
            *request,
        )?,
        OpenCloudCommand::Batch(batch) => batch_command(
            identity()?,
            &args.key_env,
            args.oauth_env.as_deref(),
            args.anonymous,
            batch,
        )?,
        OpenCloudCommand::Product(command) => {
            if args.anonymous {
                bail!("Developer product commands require API key or OAuth authentication");
            }
            let identity = identity()?;
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
    app::output::print_json_output(&result, false)
}

/// A numeric global `--place` (placeId or gameId:placeId) names the place
/// for Open Cloud the way `--place-id` does; an alias stays a project
/// selector.
fn selector_ids(selector: Option<&str>) -> (Option<i64>, Option<i64>) {
    let Some(selector) = selector.map(str::trim) else {
        return (None, None);
    };
    let positive = |text: &str| text.parse::<i64>().ok().filter(|id| *id > 0);
    match selector.split_once(':') {
        Some((game, place)) => match (positive(game), positive(place)) {
            (Some(game), Some(place)) => (Some(game), Some(place)),
            _ => (None, None),
        },
        None => (None, positive(selector)),
    }
}

pub(crate) fn discover_identity(
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
    if identity.game_id.is_none() || identity.place_id.is_none() {
        let bound = bound_studio_target(&loaded.root);
        identity.game_id = identity.game_id.or(bound.0);
        identity.place_id = identity.place_id.or(bound.1);
    }
    Ok(identity)
}

/// The place a single-place project is bound to in Studio, remembered by
/// `rbx so`/`sx` in `.renium/studio-target.json`, so Open Cloud commands run
/// against it without `--universe` and `--place-id`.
fn bound_studio_target(root: &Path) -> (Option<i64>, Option<i64>) {
    let path = root.join(".renium").join("studio-target.json");
    let Ok(bytes) = fs::read(&path) else {
        return (None, None);
    };
    let Ok(value) = serde_json::from_slice::<Value>(&bytes) else {
        return (None, None);
    };
    let id = |key: &str| value.get(key).and_then(Value::as_i64).filter(|id| *id > 0);
    (id("gameId"), id("placeId"))
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
    absolutize_files(&mut files)?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bound_studio_target_supplies_the_universe_and_place() {
        let root = std::env::temp_dir().join(format!("renium-oc-target-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join(".renium")).unwrap();
        assert_eq!(bound_studio_target(&root), (None, None));
        fs::write(
            root.join(".renium").join("studio-target.json"),
            br#"{"gameId": 10765011239, "placeId": 127769757912519}"#,
        )
        .unwrap();
        assert_eq!(
            bound_studio_target(&root),
            (Some(10765011239), Some(127769757912519))
        );
        fs::write(
            root.join(".renium").join("studio-target.json"),
            b"{\"gameId\": 0}",
        )
        .unwrap();
        assert_eq!(bound_studio_target(&root), (None, None));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_numeric_place_selector_is_a_place_id() {
        assert_eq!(
            selector_ids(Some("112966546347918")),
            (None, Some(112966546347918))
        );
        assert_eq!(
            selector_ids(Some(" 8420907710:112966546347918 ")),
            (Some(8420907710), Some(112966546347918))
        );
        assert_eq!(selector_ids(Some("lobby")), (None, None));
        assert_eq!(selector_ids(Some("main:lobby")), (None, None));
        assert_eq!(selector_ids(Some("0")), (None, None));
        assert_eq!(selector_ids(None), (None, None));
    }
}
