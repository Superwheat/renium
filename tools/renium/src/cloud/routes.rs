use std::path::Path;
use std::sync::OnceLock;

use anyhow::{Context, Result, bail};
use clap::Args;
use serde_json::{Map, Value, json};

use super::paging::{PAGE_SIZE, Pager, Plan};
use super::parameters::{
    absolutize_files, assignment, assignments, merge_assignments, parse_value,
};
use super::transport::{execute_authorized, placeholder_hint};
use super::{CloudAuth, CloudIdentity};
use crate::automation::Failure;
use crate::system::files::{absolutize_for_daemon as absolute_path, atomic_write_file};

#[derive(Args)]
pub(super) struct RouteArgs {
    #[arg(help = "Operation to run (listed below)")]
    pub(super) action: Option<String>,
    #[arg(value_name = "VALUE", help = "The operation's values, in order")]
    pub(super) values: Vec<String>,
    #[arg(short, long, value_name = "NAME=VALUE", help = "Add a query parameter")]
    query: Vec<String>,
    #[arg(
        short,
        long,
        value_name = "NAME=VALUE",
        help = "Set a JSON body field; dotted names nest, values parse as JSON"
    )]
    field: Vec<String>,
    #[arg(long, value_name = "NAME=VALUE", help = "Set a multipart form field")]
    form: Vec<String>,
    #[arg(long, value_name = "NAME=PATH", help = "Attach a multipart file")]
    file: Vec<String>,
    #[arg(long, help = "Data store scope (default global)")]
    scope: Option<String>,
    #[arg(
        short,
        long,
        value_name = "N",
        help = "Items to return in total; paged operations fetch 100 per request"
    )]
    pub(super) limit: Option<u32>,
    #[arg(long, help = "Start from this page token")]
    pub(super) cursor: Option<String>,
    #[arg(long, help = "Follow page tokens to the last page")]
    pub(super) all: bool,
    #[arg(
        long,
        value_name = "N",
        conflicts_with = "all",
        help = "Fetch at most N pages"
    )]
    pub(super) pages: Option<std::num::NonZeroUsize>,
    #[arg(long, help = "Filter expression passed to the API")]
    filter: Option<String>,
    #[arg(long, help = "Only change the resource when its etag matches")]
    if_match: Option<String>,
    #[arg(
        short,
        long,
        value_name = "PATH",
        help = "Write the response to this file"
    )]
    pub(super) output: Option<String>,
}

#[derive(Args)]
pub(super) struct RoutesArgs {
    category: Option<String>,
}

#[derive(Clone, Copy)]
enum Target {
    Path(&'static str),
    Query(&'static str),
    Body(&'static str),
    BodyList(&'static str),
    RootBody,
    Form(&'static str),
    File(&'static str),
    RawFile,
    PathBody {
        parameter: &'static str,
        field: &'static str,
        prefix: &'static str,
    },
    AssetVersion {
        asset_parameter: &'static str,
        field: &'static str,
    },
}

#[derive(Clone, Copy)]
enum BodyMode {
    None,
    Json(Option<&'static str>),
    Multipart(Option<&'static str>),
    Raw(&'static str),
}

#[derive(Clone, Copy)]
struct Operand {
    label: &'static str,
    target: Target,
}

#[derive(Clone, Copy)]
struct Preset {
    target: Target,
    value: &'static str,
}

struct Route {
    category: &'static str,
    action: &'static str,
    method: &'static str,
    path: &'static str,
    operands: &'static [Operand],
    presets: &'static [Preset],
    limit: Option<&'static str>,
    cursor: Option<&'static str>,
    body_mode: BodyMode,
}

struct RequestParts {
    path: Map<String, Value>,
    query: Map<String, Value>,
    body: Map<String, Value>,
    root_body: Option<Value>,
    form: Map<String, Value>,
    files: Map<String, Value>,
    raw_file: Option<Value>,
}

const fn path(label: &'static str, name: &'static str) -> Operand {
    Operand {
        label,
        target: Target::Path(name),
    }
}

const fn query(label: &'static str, name: &'static str) -> Operand {
    Operand {
        label,
        target: Target::Query(name),
    }
}

const fn body(label: &'static str, name: &'static str) -> Operand {
    Operand {
        label,
        target: Target::Body(name),
    }
}

const fn body_list(label: &'static str, name: &'static str) -> Operand {
    Operand {
        label,
        target: Target::BodyList(name),
    }
}

const fn root_body(label: &'static str) -> Operand {
    Operand {
        label,
        target: Target::RootBody,
    }
}

const fn form(label: &'static str, name: &'static str) -> Operand {
    Operand {
        label,
        target: Target::Form(name),
    }
}

const fn file(label: &'static str, name: &'static str) -> Operand {
    Operand {
        label,
        target: Target::File(name),
    }
}

const fn raw_file(label: &'static str) -> Operand {
    Operand {
        label,
        target: Target::RawFile,
    }
}

const fn path_body(
    label: &'static str,
    parameter: &'static str,
    field: &'static str,
    prefix: &'static str,
) -> Operand {
    Operand {
        label,
        target: Target::PathBody {
            parameter,
            field,
            prefix,
        },
    }
}

const fn asset_version_form(
    label: &'static str,
    asset_parameter: &'static str,
    field: &'static str,
) -> Operand {
    Operand {
        label,
        target: Target::AssetVersion {
            asset_parameter,
            field,
        },
    }
}

const fn q(name: &'static str, value: &'static str) -> Preset {
    Preset {
        target: Target::Query(name),
        value,
    }
}

const fn b(name: &'static str, value: &'static str) -> Preset {
    Preset {
        target: Target::Body(name),
        value,
    }
}

macro_rules! route {
    (@make $category:literal, $action:literal, $method:literal, $path:literal, [$($operand:expr),*], [$($preset:expr),*], $limit:expr, $cursor:expr, $body_mode:expr) => {
        Route {
            category: $category,
            action: $action,
            method: $method,
            path: $path,
            operands: &[$($operand),*],
            presets: &[$($preset),*],
            limit: $limit,
            cursor: $cursor,
            body_mode: $body_mode,
        }
    };
    ($category:literal, $action:literal, $method:literal, $path:literal) => {
        route!(@make $category, $action, $method, $path, [], [], None, None, BodyMode::Json(None))
    };
    ($category:literal, $action:literal, $method:literal, $path:literal, [$($operand:expr),* $(,)?]) => {
        route!(@make $category, $action, $method, $path, [$($operand),*], [], None, None, BodyMode::Json(None))
    };
    ($category:literal, $action:literal, $method:literal, $path:literal, [$($operand:expr),* $(,)?], presets [$($preset:expr),* $(,)?]) => {
        route!(@make $category, $action, $method, $path, [$($operand),*], [$($preset),*], None, None, BodyMode::Json(None))
    };
    ($category:literal, $action:literal, $method:literal, $path:literal, page $limit:literal, $cursor:literal) => {
        route!(@make $category, $action, $method, $path, [], [], Some($limit), Some($cursor), BodyMode::Json(None))
    };
    ($category:literal, $action:literal, $method:literal, $path:literal, [$($operand:expr),* $(,)?], page $limit:literal, $cursor:literal) => {
        route!(@make $category, $action, $method, $path, [$($operand),*], [], Some($limit), Some($cursor), BodyMode::Json(None))
    };
    ($category:literal, $action:literal, $method:literal, $path:literal, limit $limit:literal) => {
        route!(@make $category, $action, $method, $path, [], [], Some($limit), None, BodyMode::Json(None))
    };
    ($category:literal, $action:literal, $method:literal, $path:literal, [$($operand:expr),* $(,)?], limit $limit:literal) => {
        route!(@make $category, $action, $method, $path, [$($operand),*], [], Some($limit), None, BodyMode::Json(None))
    };
    ($category:literal, $action:literal, $method:literal, $path:literal, cursor $cursor:literal) => {
        route!(@make $category, $action, $method, $path, [], [], None, Some($cursor), BodyMode::Json(None))
    };
    ($category:literal, $action:literal, $method:literal, $path:literal, [$($operand:expr),* $(,)?], cursor $cursor:literal) => {
        route!(@make $category, $action, $method, $path, [$($operand),*], [], None, Some($cursor), BodyMode::Json(None))
    };
    ($category:literal, $action:literal, $method:literal, $path:literal, multipart) => {
        route!(@make $category, $action, $method, $path, [], [], None, None, BodyMode::Multipart(None))
    };
    ($category:literal, $action:literal, $method:literal, $path:literal, [$($operand:expr),* $(,)?], multipart) => {
        route!(@make $category, $action, $method, $path, [$($operand),*], [], None, None, BodyMode::Multipart(None))
    };
    ($category:literal, $action:literal, $method:literal, $path:literal, [$($operand:expr),* $(,)?], multipart $part:literal) => {
        route!(@make $category, $action, $method, $path, [$($operand),*], [], None, None, BodyMode::Multipart(Some($part)))
    };
    ($category:literal, $action:literal, $method:literal, $path:literal, [$($operand:expr),* $(,)?], json $content_type:literal) => {
        route!(@make $category, $action, $method, $path, [$($operand),*], [], None, None, BodyMode::Json(Some($content_type)))
    };
    ($category:literal, $action:literal, $method:literal, $path:literal, no_body) => {
        route!(@make $category, $action, $method, $path, [], [], None, None, BodyMode::None)
    };
    ($category:literal, $action:literal, $method:literal, $path:literal, [$($operand:expr),* $(,)?], no_body) => {
        route!(@make $category, $action, $method, $path, [$($operand),*], [], None, None, BodyMode::None)
    };
    ($category:literal, $action:literal, $method:literal, $path:literal, [$($operand:expr),* $(,)?], raw $content_type:literal) => {
        route!(@make $category, $action, $method, $path, [$($operand),*], [], None, None, BodyMode::Raw($content_type))
    };
    ($category:literal, $action:literal, $method:literal, $path:literal, [$($operand:expr),* $(,)?], presets [$($preset:expr),* $(,)?], raw $content_type:literal) => {
        route!(@make $category, $action, $method, $path, [$($operand),*], [$($preset),*], None, None, BodyMode::Raw($content_type))
    };
}

static ROUTES: &[Route] = &[
    route!("data", "stores", "GET", "/cloud/v2/universes/{universe}/data-stores", page "maxPageSize", "pageToken"),
    route!(
        "data",
        "delete-store",
        "DELETE",
        "/cloud/v2/universes/{universe}/data-stores/{data_store_id}",
        [path("STORE", "data_store_id")]
    ),
    route!("data", "entries", "GET", "/cloud/v2/universes/{universe}/data-stores/{data_store_id}/scopes/{scope_id}/entries", [path("STORE", "data_store_id")], page "maxPageSize", "pageToken"),
    route!(
        "data",
        "create",
        "POST",
        "/cloud/v2/universes/{universe}/data-stores/{data_store_id}/scopes/{scope_id}/entries",
        [
            path("STORE", "data_store_id"),
            query("KEY", "id"),
            body("VALUE", "value")
        ]
    ),
    route!(
        "data",
        "get",
        "GET",
        "/cloud/v2/universes/{universe}/data-stores/{data_store_id}/scopes/{scope_id}/entries/{entry_id}",
        [path("STORE", "data_store_id"), path("KEY", "entry_id")]
    ),
    route!(
        "data",
        "update",
        "PATCH",
        "/cloud/v2/universes/{universe}/data-stores/{data_store_id}/scopes/{scope_id}/entries/{entry_id}",
        [
            path("STORE", "data_store_id"),
            path("KEY", "entry_id"),
            body("VALUE", "value")
        ]
    ),
    route!(
        "data",
        "upsert",
        "PATCH",
        "/cloud/v2/universes/{universe}/data-stores/{data_store_id}/scopes/{scope_id}/entries/{entry_id}",
        [
            path("STORE", "data_store_id"),
            path("KEY", "entry_id"),
            body("VALUE", "value")
        ],
        presets[q("allowMissing", "true")]
    ),
    route!(
        "data",
        "delete",
        "DELETE",
        "/cloud/v2/universes/{universe}/data-stores/{data_store_id}/scopes/{scope_id}/entries/{entry_id}",
        [path("STORE", "data_store_id"), path("KEY", "entry_id")]
    ),
    route!(
        "data",
        "increment",
        "POST",
        "/cloud/v2/universes/{universe}/data-stores/{data_store_id}/scopes/{scope_id}/entries/{entry_id}:increment",
        [
            path("STORE", "data_store_id"),
            path("KEY", "entry_id"),
            body("AMOUNT", "amount")
        ]
    ),
    route!("data", "revisions", "GET", "/cloud/v2/universes/{universe}/data-stores/{data_store_id}/scopes/{scope_id}/entries/{entry_id}:listRevisions", [path("STORE", "data_store_id"), path("KEY", "entry_id")], page "maxPageSize", "pageToken"),
    route!(
        "data",
        "undelete",
        "POST",
        "/cloud/v2/universes/{universe}/data-stores/{data_store_id}:undelete",
        [path("STORE", "data_store_id")]
    ),
    route!(
        "data",
        "snapshot",
        "POST",
        "/cloud/v2/universes/{universe}/data-stores:snapshot"
    ),
    route!("ordered", "list", "GET", "/cloud/v2/universes/{universe}/ordered-data-stores/{ordered_data_store_id}/scopes/{scope_id}/entries", [path("STORE", "ordered_data_store_id")], page "maxPageSize", "pageToken"),
    route!(
        "ordered",
        "create",
        "POST",
        "/cloud/v2/universes/{universe}/ordered-data-stores/{ordered_data_store_id}/scopes/{scope_id}/entries",
        [
            path("STORE", "ordered_data_store_id"),
            query("KEY", "id"),
            body("VALUE", "value")
        ]
    ),
    route!(
        "ordered",
        "get",
        "GET",
        "/cloud/v2/universes/{universe}/ordered-data-stores/{ordered_data_store_id}/scopes/{scope_id}/entries/{entry_id}",
        [
            path("STORE", "ordered_data_store_id"),
            path("KEY", "entry_id")
        ]
    ),
    route!(
        "ordered",
        "update",
        "PATCH",
        "/cloud/v2/universes/{universe}/ordered-data-stores/{ordered_data_store_id}/scopes/{scope_id}/entries/{entry_id}",
        [
            path("STORE", "ordered_data_store_id"),
            path("KEY", "entry_id"),
            body("VALUE", "value")
        ]
    ),
    route!(
        "ordered",
        "upsert",
        "PATCH",
        "/cloud/v2/universes/{universe}/ordered-data-stores/{ordered_data_store_id}/scopes/{scope_id}/entries/{entry_id}",
        [
            path("STORE", "ordered_data_store_id"),
            path("KEY", "entry_id"),
            body("VALUE", "value")
        ],
        presets[q("allowMissing", "true")]
    ),
    route!(
        "ordered",
        "delete",
        "DELETE",
        "/cloud/v2/universes/{universe}/ordered-data-stores/{ordered_data_store_id}/scopes/{scope_id}/entries/{entry_id}",
        [
            path("STORE", "ordered_data_store_id"),
            path("KEY", "entry_id")
        ]
    ),
    route!(
        "ordered",
        "increment",
        "POST",
        "/cloud/v2/universes/{universe}/ordered-data-stores/{ordered_data_store_id}/scopes/{scope_id}/entries/{entry_id}:increment",
        [
            path("STORE", "ordered_data_store_id"),
            path("KEY", "entry_id"),
            body("AMOUNT", "amount")
        ]
    ),
    route!(
        "memory",
        "operation",
        "GET",
        "/cloud/v2/universes/{universe}/memory-store/operations/{operation_id}",
        [path("OPERATION", "operation_id")]
    ),
    route!(
        "memory",
        "queue-add",
        "POST",
        "/cloud/v2/universes/{universe}/memory-store/queues/{queue_id}/items",
        [path("QUEUE", "queue_id"), body("VALUE", "data")]
    ),
    route!("memory", "queue-read", "GET", "/cloud/v2/universes/{universe}/memory-store/queues/{queue_id}/items:read", [path("QUEUE", "queue_id")], limit "count"),
    route!(
        "memory",
        "queue-discard",
        "POST",
        "/cloud/v2/universes/{universe}/memory-store/queues/{queue_id}/items:discard",
        [path("QUEUE", "queue_id"), body("READ_ID", "readId")]
    ),
    route!("memory", "sorted-list", "GET", "/cloud/v2/universes/{universe}/memory-store/sorted-maps/{sorted_map_id}/items", [path("MAP", "sorted_map_id")], page "maxPageSize", "pageToken"),
    route!(
        "memory",
        "sorted-create",
        "POST",
        "/cloud/v2/universes/{universe}/memory-store/sorted-maps/{sorted_map_id}/items",
        [
            path("MAP", "sorted_map_id"),
            query("KEY", "id"),
            body("VALUE", "value")
        ]
    ),
    route!(
        "memory",
        "sorted-get",
        "GET",
        "/cloud/v2/universes/{universe}/memory-store/sorted-maps/{sorted_map_id}/items/{item_id}",
        [path("MAP", "sorted_map_id"), path("KEY", "item_id")]
    ),
    route!(
        "memory",
        "sorted-update",
        "PATCH",
        "/cloud/v2/universes/{universe}/memory-store/sorted-maps/{sorted_map_id}/items/{item_id}",
        [
            path("MAP", "sorted_map_id"),
            path("KEY", "item_id"),
            body("VALUE", "value")
        ]
    ),
    route!(
        "memory",
        "sorted-upsert",
        "PATCH",
        "/cloud/v2/universes/{universe}/memory-store/sorted-maps/{sorted_map_id}/items/{item_id}",
        [
            path("MAP", "sorted_map_id"),
            path("KEY", "item_id"),
            body("VALUE", "value")
        ],
        presets[q("allowMissing", "true")]
    ),
    route!(
        "memory",
        "sorted-delete",
        "DELETE",
        "/cloud/v2/universes/{universe}/memory-store/sorted-maps/{sorted_map_id}/items/{item_id}",
        [path("MAP", "sorted_map_id"), path("KEY", "item_id")]
    ),
    route!(
        "memory",
        "flush",
        "POST",
        "/cloud/v2/universes/{universe}/memory-store:flush"
    ),
    route!("universe", "get", "GET", "/cloud/v2/universes/{universe}"),
    route!(
        "universe",
        "update",
        "PATCH",
        "/cloud/v2/universes/{universe}"
    ),
    route!(
        "universe",
        "message",
        "POST",
        "/cloud/v2/universes/{universe}:publishMessage",
        [body("TOPIC", "topic"), body("MESSAGE", "message")]
    ),
    route!(
        "universe",
        "restart",
        "POST",
        "/cloud/v2/universes/{universe}:restartServers"
    ),
    route!(
        "universe",
        "activate",
        "POST",
        "/legacy-develop/v1/universes/{universe}/activate"
    ),
    route!(
        "universe",
        "deactivate",
        "POST",
        "/legacy-develop/v1/universes/{universe}/deactivate"
    ),
    route!(
        "universe",
        "permissions",
        "GET",
        "/legacy-develop/v1/universes/{universe}/permissions"
    ),
    route!(
        "universe",
        "permissions-many",
        "GET",
        "/legacy-develop/v1/universes/multiget/permissions",
        [query("UNIVERSES", "ids")]
    ),
    route!(
        "place",
        "get",
        "GET",
        "/cloud/v2/universes/{universe}/places/{place}"
    ),
    route!(
        "place",
        "update",
        "PATCH",
        "/cloud/v2/universes/{universe}/places/{place}"
    ),
    route!(
        "place",
        "publish",
        "POST",
        "/universes/v1/{universe}/places/{place}/versions",
        [raw_file("FILE")],
        presets [q("versionType", "Published")],
        raw "application/octet-stream"
    ),
    route!(
        "place",
        "contributors",
        "GET",
        "/place-version-history-api/v1/{place}/contributors",
        [],
        page "pageSize", "cursor"
    ),
    route!(
        "place",
        "history",
        "GET",
        "/place-version-history-api/v1/{place}/history",
        [],
        page "pageSize", "cursor"
    ),
    route!(
        "place",
        "version-note",
        "POST",
        "/place-version-history-api/v1/{place}/version/{version}/notes",
        [path("VERSION", "version")]
    ),
    route!(
        "place",
        "instance-get",
        "GET",
        "/cloud/v2/universes/{universe}/places/{place}/instances/{instance_id}",
        [path("INSTANCE", "instance_id")]
    ),
    route!(
        "place",
        "instance-update",
        "PATCH",
        "/cloud/v2/universes/{universe}/places/{place}/instances/{instance_id}",
        [path("INSTANCE", "instance_id")]
    ),
    route!(
        "place",
        "instance-operation",
        "GET",
        "/cloud/v2/universes/{universe}/places/{place}/instances/{instance_id}/operations/{operation_id}",
        [
            path("INSTANCE", "instance_id"),
            path("OPERATION", "operation_id")
        ]
    ),
    route!("place", "instance-children", "GET", "/cloud/v2/universes/{universe}/places/{place}/instances/{instance_id}:listChildren", [path("INSTANCE", "instance_id")], page "maxPageSize", "pageToken"),
    route!("restriction", "list", "GET", "/cloud/v2/universes/{universe}/user-restrictions", page "maxPageSize", "pageToken"),
    route!(
        "restriction",
        "get",
        "GET",
        "/cloud/v2/universes/{universe}/user-restrictions/{user_restriction_id}",
        [path("USER", "user_restriction_id")]
    ),
    route!(
        "restriction",
        "ban",
        "PATCH",
        "/cloud/v2/universes/{universe}/user-restrictions/{user_restriction_id}",
        [
            path_body("USER", "user_restriction_id", "user", "users/"),
            body("REASON", "gameJoinRestriction.displayReason")
        ],
        presets[b("gameJoinRestriction.active", "true")]
    ),
    route!(
        "restriction",
        "unban",
        "PATCH",
        "/cloud/v2/universes/{universe}/user-restrictions/{user_restriction_id}",
        [path_body("USER", "user_restriction_id", "user", "users/")],
        presets[b("gameJoinRestriction.active", "false")]
    ),
    route!("restriction", "logs", "GET", "/cloud/v2/universes/{universe}/user-restrictions:listLogs", page "maxPageSize", "pageToken"),
    route!("restriction", "place-list", "GET", "/cloud/v2/universes/{universe}/places/{place}/user-restrictions", page "maxPageSize", "pageToken"),
    route!(
        "restriction",
        "place-get",
        "GET",
        "/cloud/v2/universes/{universe}/places/{place}/user-restrictions/{user_restriction_id}",
        [path("USER", "user_restriction_id")]
    ),
    route!(
        "restriction",
        "place-ban",
        "PATCH",
        "/cloud/v2/universes/{universe}/places/{place}/user-restrictions/{user_restriction_id}",
        [
            path_body("USER", "user_restriction_id", "user", "users/"),
            body("REASON", "gameJoinRestriction.displayReason")
        ],
        presets[b("gameJoinRestriction.active", "true")]
    ),
    route!(
        "restriction",
        "place-unban",
        "PATCH",
        "/cloud/v2/universes/{universe}/places/{place}/user-restrictions/{user_restriction_id}",
        [path_body("USER", "user_restriction_id", "user", "users/")],
        presets[b("gameJoinRestriction.active", "false")]
    ),
    route!("secret", "list", "GET", "/cloud/v2/universes/{universe}/secrets", page "limit", "cursor"),
    route!(
        "secret",
        "public-key",
        "GET",
        "/cloud/v2/universes/{universe}/secrets/public-key"
    ),
    route!(
        "secret",
        "create",
        "POST",
        "/cloud/v2/universes/{universe}/secrets",
        [
            body("ID", "id"),
            body("ENCRYPTED", "secret"),
            body("KEY_ID", "key_id"),
            body("DOMAIN", "domain")
        ]
    ),
    route!(
        "secret",
        "update",
        "PATCH",
        "/cloud/v2/universes/{universe}/secrets/{secretId}",
        [
            path("ID", "secretId"),
            body("ENCRYPTED", "secret"),
            body("KEY_ID", "key_id"),
            body("DOMAIN", "domain")
        ]
    ),
    route!(
        "secret",
        "delete",
        "DELETE",
        "/cloud/v2/universes/{universe}/secrets/{secretId}",
        [path("ID", "secretId")]
    ),
    route!(
        "notification",
        "send",
        "POST",
        "/cloud/v2/users/{user_id}/notifications",
        [
            path("USER", "user_id"),
            body("MESSAGE_ID", "payload.messageId")
        ]
    ),
    route!(
        "advertising",
        "universes",
        "GET",
        "/ads-management/v1/advertisable-universes"
    ),
    route!(
        "advertising",
        "billing",
        "GET",
        "/ads-management/v1/billing-accounts",
        page "maxPageSize", "pageToken"
    ),
    route!(
        "advertising",
        "billing-get",
        "GET",
        "/ads-management/v1/billing-accounts/{id}",
        [path("ACCOUNT", "id")]
    ),
    route!(
        "advertising",
        "options",
        "GET",
        "/ads-management/v1/campaign-options"
    ),
    route!(
        "advertising",
        "campaigns",
        "GET",
        "/ads-management/v1/campaigns",
        page "maxPageSize", "pageToken"
    ),
    route!(
        "advertising",
        "campaign-create",
        "POST",
        "/ads-management/v1/campaigns"
    ),
    route!(
        "advertising",
        "campaign-get",
        "GET",
        "/ads-management/v1/campaigns/{id}",
        [path("CAMPAIGN", "id")]
    ),
    route!(
        "advertising",
        "campaign-update",
        "PATCH",
        "/ads-management/v1/campaigns/{id}",
        [path("CAMPAIGN", "id")]
    ),
    route!(
        "advertising",
        "campaign-status",
        "POST",
        "/ads-management/v1/campaigns:batchGetStatus"
    ),
    route!(
        "advertising",
        "creatives",
        "GET",
        "/ads-management/v1/creatives",
        page "maxPageSize", "pageToken"
    ),
    route!(
        "analytics",
        "dimensions",
        "POST",
        "/analytics-query-api/v1/universes/{universe}/dimension-values"
    ),
    route!(
        "analytics",
        "metrics",
        "POST",
        "/analytics-query-api/v1/universes/{universe}/metrics"
    ),
    route!(
        "analytics",
        "dimension-operation",
        "GET",
        "/analytics-query-api/v1/universes/{universe}/operations/dimension-values/{operationId}",
        [path("OPERATION", "operationId")]
    ),
    route!(
        "analytics",
        "metrics-operation",
        "GET",
        "/analytics-query-api/v1/universes/{universe}/operations/metrics/{operationId}",
        [path("OPERATION", "operationId")]
    ),
    route!(
        "avatar",
        "thumbnail",
        "GET",
        "/cloud/v2/users/{user_id}:generateThumbnail",
        [path("USER", "user_id")]
    ),
    route!(
        "avatar",
        "avatar-3d",
        "GET",
        "/v1/users/avatar-3d",
        [query("USER", "userId")]
    ),
    route!(
        "avatar",
        "outfit-3d",
        "GET",
        "/v1/users/outfit-3d",
        [query("OUTFIT", "outfitId")]
    ),
    route!(
        "badge",
        "create",
        "POST",
        "/legacy-badges/v1/universes/{universe}/badges",
        multipart
    ),
    route!(
        "badge",
        "update",
        "PATCH",
        "/legacy-badges/v1/badges/{badgeId}",
        [path("BADGE", "badgeId")]
    ),
    route!(
        "badge",
        "icon",
        "POST",
        "/legacy-publish/v1/badges/{badgeId}/icon",
        [path("BADGE", "badgeId"), file("FILE", "Files")],
        multipart
    ),
    route!(
        "experiment",
        "list",
        "GET",
        "/creator-configs-public-api/v1/experimentation/universes/{universe}/experiments",
        limit "maxPageSize"
    ),
    route!(
        "experiment",
        "create",
        "POST",
        "/creator-configs-public-api/v1/experimentation/universes/{universe}/experiments"
    ),
    route!(
        "experiment",
        "get",
        "GET",
        "/creator-configs-public-api/v1/experimentation/universes/{universe}/experiments/{experimentId}",
        [path("EXPERIMENT", "experimentId")]
    ),
    route!(
        "experiment",
        "update",
        "PATCH",
        "/creator-configs-public-api/v1/experimentation/universes/{universe}/experiments/{experimentId}",
        [path("EXPERIMENT", "experimentId")]
    ),
    route!(
        "experiment",
        "delete",
        "DELETE",
        "/creator-configs-public-api/v1/experimentation/universes/{universe}/experiments/{experimentId}",
        [path("EXPERIMENT", "experimentId")]
    ),
    route!(
        "experiment",
        "stats",
        "GET",
        "/creator-configs-public-api/v1/experimentation/universes/{universe}/experiments/{experimentId}/stats",
        [path("EXPERIMENT", "experimentId")]
    ),
    route!(
        "experiment",
        "complete",
        "POST",
        "/creator-configs-public-api/v1/experimentation/universes/{universe}/experiments/{experimentId}:complete",
        [path("EXPERIMENT", "experimentId")]
    ),
    route!(
        "experiment",
        "schedule",
        "POST",
        "/creator-configs-public-api/v1/experimentation/universes/{universe}/experiments/{experimentId}:schedule",
        [path("EXPERIMENT", "experimentId")]
    ),
    route!(
        "experiment",
        "start",
        "POST",
        "/creator-configs-public-api/v1/experimentation/universes/{universe}/experiments/{experimentId}:start",
        [path("EXPERIMENT", "experimentId")],
        no_body
    ),
    route!(
        "experiment",
        "calculate-mde",
        "POST",
        "/creator-configs-public-api/v1/experimentation/universes/{universe}/experiments:calculateMde"
    ),
    route!(
        "experiment",
        "operation",
        "GET",
        "/creator-configs-public-api/v1/experimentation/universes/{universe}/operations/{operationId}",
        [path("OPERATION", "operationId")]
    ),
    route!(
        "event",
        "list",
        "GET",
        "/virtual-events/v3/universes/{universe}/game-events",
        page "pageSize", "pageToken"
    ),
    route!(
        "event",
        "create",
        "POST",
        "/virtual-events/v3/universes/{universe}/game-events"
    ),
    route!(
        "event",
        "get",
        "GET",
        "/virtual-events/v3/game-events/{eventId}",
        [path("EVENT", "eventId")]
    ),
    route!(
        "event",
        "update",
        "PATCH",
        "/virtual-events/v3/game-events/{eventId}",
        [path("EVENT", "eventId")]
    ),
    route!(
        "event",
        "delete",
        "DELETE",
        "/virtual-events/v3/game-events/{eventId}",
        [path("EVENT", "eventId")]
    ),
    route!(
        "ai",
        "translate",
        "POST",
        "/cloud/v2/universes/{universe}:translateText",
        [
            body("TEXT", "text"),
            body_list("LANGUAGES", "targetLanguageCodes")
        ]
    ),
    route!(
        "ai",
        "speech",
        "POST",
        "/cloud/v2/universes/{universe}:generateSpeechAsset",
        [body("TEXT", "text")]
    ),
    route!(
        "matchmaking",
        "status",
        "GET",
        "/matchmaking-api/v1/client-status"
    ),
    route!(
        "matchmaking",
        "status-update",
        "POST",
        "/matchmaking-api/v1/client-status"
    ),
    route!(
        "matchmaking",
        "forecast",
        "POST",
        "/matchmaking-api/v1/game-instances/forecast-update"
    ),
    route!(
        "matchmaking",
        "update-status",
        "GET",
        "/matchmaking-api/v1/game-instances/get-update-status"
    ),
    route!(
        "matchmaking",
        "launch-update",
        "POST",
        "/matchmaking-api/v1/game-instances/launch-update"
    ),
    route!(
        "matchmaking",
        "shutdown",
        "POST",
        "/matchmaking-api/v1/game-instances/shutdown"
    ),
    route!(
        "matchmaking",
        "shutdown-all",
        "POST",
        "/matchmaking-api/v1/game-instances/shutdown-all",
        multipart
    ),
    route!(
        "matchmaking",
        "player-attribute-create",
        "POST",
        "/matchmaking-api/v1/matchmaking/player-attribute"
    ),
    route!(
        "matchmaking",
        "player-attribute-update",
        "PATCH",
        "/matchmaking-api/v1/matchmaking/player-attribute/{attributeId}",
        [path("ATTRIBUTE", "attributeId")]
    ),
    route!(
        "matchmaking",
        "player-attribute-delete",
        "DELETE",
        "/matchmaking-api/v1/matchmaking/player-attribute/{attributeId}",
        [path("ATTRIBUTE", "attributeId")]
    ),
    route!(
        "matchmaking",
        "player-attributes",
        "GET",
        "/matchmaking-api/v1/matchmaking/player-attributes/{universe}"
    ),
    route!(
        "matchmaking",
        "scoring-create",
        "POST",
        "/matchmaking-api/v1/matchmaking/scoring-configuration"
    ),
    route!(
        "matchmaking",
        "scoring-defaults",
        "GET",
        "/matchmaking-api/v1/matchmaking/scoring-configuration/default-weights"
    ),
    route!(
        "matchmaking",
        "scoring-mock",
        "GET",
        "/matchmaking-api/v1/matchmaking/scoring-configuration/generate-mock-servers"
    ),
    route!(
        "matchmaking",
        "scoring-place",
        "POST",
        "/matchmaking-api/v1/matchmaking/scoring-configuration/place"
    ),
    route!(
        "matchmaking",
        "scoring-place-delete",
        "DELETE",
        "/matchmaking-api/v1/matchmaking/scoring-configuration/place/{place}"
    ),
    route!(
        "matchmaking",
        "scoring-get",
        "GET",
        "/matchmaking-api/v1/matchmaking/scoring-configuration/{scoringConfigurationId}",
        [path("CONFIG", "scoringConfigurationId")]
    ),
    route!(
        "matchmaking",
        "scoring-update",
        "PATCH",
        "/matchmaking-api/v1/matchmaking/scoring-configuration/{scoringConfigurationId}",
        [path("CONFIG", "scoringConfigurationId")]
    ),
    route!(
        "matchmaking",
        "scoring-delete",
        "DELETE",
        "/matchmaking-api/v1/matchmaking/scoring-configuration/{scoringConfigurationId}",
        [path("CONFIG", "scoringConfigurationId")]
    ),
    route!(
        "matchmaking",
        "signal-create",
        "POST",
        "/matchmaking-api/v1/matchmaking/scoring-configuration/{scoringConfigurationId}/signals",
        [path("CONFIG", "scoringConfigurationId")]
    ),
    route!(
        "matchmaking",
        "signal-update",
        "PATCH",
        "/matchmaking-api/v1/matchmaking/scoring-configuration/{scoringConfigurationId}/signals/{signalName}",
        [
            path("CONFIG", "scoringConfigurationId"),
            path("SIGNAL", "signalName")
        ]
    ),
    route!(
        "matchmaking",
        "signal-delete",
        "DELETE",
        "/matchmaking-api/v1/matchmaking/scoring-configuration/{scoringConfigurationId}/signals/{signalName}",
        [
            path("CONFIG", "scoringConfigurationId"),
            path("SIGNAL", "signalName")
        ]
    ),
    route!(
        "matchmaking",
        "scoring-list",
        "GET",
        "/matchmaking-api/v1/matchmaking/scoring-configurations/{universe}"
    ),
    route!(
        "matchmaking",
        "scoring-places",
        "GET",
        "/matchmaking-api/v1/matchmaking/scoring-configurations/{universe}/places"
    ),
    route!(
        "matchmaking",
        "server-attribute-create",
        "POST",
        "/matchmaking-api/v1/matchmaking/server-attribute"
    ),
    route!(
        "matchmaking",
        "server-attribute-update",
        "PATCH",
        "/matchmaking-api/v1/matchmaking/server-attribute/{attributeId}",
        [path("ATTRIBUTE", "attributeId")]
    ),
    route!(
        "matchmaking",
        "server-attribute-delete",
        "DELETE",
        "/matchmaking-api/v1/matchmaking/server-attribute/{attributeId}",
        [path("ATTRIBUTE", "attributeId")]
    ),
    route!(
        "matchmaking",
        "server-attributes",
        "GET",
        "/matchmaking-api/v1/matchmaking/server-attributes/{universe}"
    ),
    route!(
        "matchmaking",
        "flags",
        "GET",
        "/matchmaking-api/v1/matchmaking/universe/{universe}/feature-flags"
    ),
    route!(
        "thumbnail",
        "personalization",
        "GET",
        "/thumbnail-personalization-api/v1/universes/{universe}/personalization",
        page "limit", "cursor"
    ),
    route!(
        "thumbnail",
        "personalization-create",
        "POST",
        "/thumbnail-personalization-api/v1/universes/{universe}/personalization/create"
    ),
    route!(
        "thumbnail",
        "personalization-update",
        "POST",
        "/thumbnail-personalization-api/v1/universes/{universe}/personalization/update"
    ),
    route!(
        "thumbnail",
        "list",
        "GET",
        "/thumbnail-personalization-api/v1/universes/{universe}/thumbnails",
        page "limit", "nextCursor"
    ),
    route!(
        "thumbnail",
        "delete",
        "DELETE",
        "/thumbnail-personalization-api/v1/universes/{universe}/thumbnails"
    ),
    route!(
        "thumbnail",
        "upload",
        "POST",
        "/thumbnail-personalization-api/v1/universes/{universe}/thumbnails/uploads",
        [file("FILE", "files")],
        multipart
    ),
    route!(
        "thumbnail",
        "upload-status",
        "GET",
        "/thumbnail-personalization-api/v1/universes/{universe}/thumbnails/uploads/status"
    ),
    route!(
        "user",
        "get",
        "GET",
        "/cloud/v2/users/{user_id}",
        [path("USER", "user_id")]
    ),
    route!(
        "user",
        "operation",
        "GET",
        "/cloud/v2/users/{user_id}/operations/{operation_id}",
        [path("USER", "user_id"), path("OPERATION", "operation_id")]
    ),
    route!("user", "inventory", "GET", "/cloud/v2/users/{user_id}/inventory-items", [path("USER", "user_id")], page "maxPageSize", "pageToken"),
    route!(
        "user",
        "asset-quotas",
        "GET",
        "/cloud/v2/users/{user_id}/asset-quotas",
        [path("USER", "user_id")],
        page "maxPageSize", "pageToken"
    ),
    route!(
        "user",
        "subscription",
        "GET",
        "/cloud/v2/universes/{universe}/subscription-products/{subscription_product_id}/subscriptions/{subscription_id}",
        [
            path("PRODUCT", "subscription_product_id"),
            path("SUBSCRIPTION", "subscription_id")
        ]
    ),
    route!(
        "group",
        "get",
        "GET",
        "/cloud/v2/groups/{group_id}",
        [path("GROUP", "group_id")]
    ),
    route!("group", "join-requests", "GET", "/cloud/v2/groups/{group_id}/join-requests", [path("GROUP", "group_id")], page "maxPageSize", "pageToken"),
    route!(
        "group",
        "join-accept",
        "POST",
        "/cloud/v2/groups/{group_id}/join-requests/{join_request_id}:accept",
        [
            path("GROUP", "group_id"),
            path("REQUEST", "join_request_id")
        ]
    ),
    route!(
        "group",
        "join-decline",
        "POST",
        "/cloud/v2/groups/{group_id}/join-requests/{join_request_id}:decline",
        [
            path("GROUP", "group_id"),
            path("REQUEST", "join_request_id")
        ]
    ),
    route!("group", "members", "GET", "/cloud/v2/groups/{group_id}/memberships", [path("GROUP", "group_id")], page "maxPageSize", "pageToken"),
    route!(
        "group",
        "member-update",
        "PATCH",
        "/cloud/v2/groups/{group_id}/memberships/{membership_id}",
        [
            path("GROUP", "group_id"),
            path("MEMBERSHIP", "membership_id")
        ]
    ),
    route!(
        "group",
        "role-assign",
        "POST",
        "/cloud/v2/groups/{group_id}/memberships/{membership_id}:assignRole",
        [
            path("GROUP", "group_id"),
            path("MEMBERSHIP", "membership_id"),
            body("ROLE", "role")
        ]
    ),
    route!(
        "group",
        "role-unassign",
        "POST",
        "/cloud/v2/groups/{group_id}/memberships/{membership_id}:unassignRole",
        [
            path("GROUP", "group_id"),
            path("MEMBERSHIP", "membership_id"),
            body("ROLE", "role")
        ]
    ),
    route!("group", "roles", "GET", "/cloud/v2/groups/{group_id}/roles", [path("GROUP", "group_id")], page "maxPageSize", "pageToken"),
    route!(
        "group",
        "role",
        "GET",
        "/cloud/v2/groups/{group_id}/roles/{role_id}",
        [path("GROUP", "group_id"), path("ROLE", "role_id")]
    ),
    route!("group", "forum-categories", "GET", "/cloud/v2/groups/{group_id}/forum-categories", [path("GROUP", "group_id")], page "maxPageSize", "pageToken"),
    route!("group", "forum-posts", "GET", "/cloud/v2/groups/{group_id}/forum-categories/{forum_category_id}/posts", [path("GROUP", "group_id"), path("CATEGORY", "forum_category_id")], page "maxPageSize", "pageToken"),
    route!("group", "forum-comments", "GET", "/cloud/v2/groups/{group_id}/forum-categories/{forum_category_id}/posts/{post_id}/comments", [path("GROUP", "group_id"), path("CATEGORY", "forum_category_id"), path("POST", "post_id")], page "maxPageSize", "pageToken"),
    route!(
        "group",
        "can-manage",
        "GET",
        "/legacy-develop/v1/user/groups/canmanage"
    ),
    route!(
        "group",
        "policies",
        "POST",
        "/legacy-groups/v1/groups/policies"
    ),
    route!("group", "audit", "GET", "/legacy-groups/v1/groups/{group_id}/audit-log", [path("GROUP", "group_id")], page "limit", "cursor"),
    route!(
        "group",
        "description",
        "PATCH",
        "/legacy-groups/v1/groups/{group_id}/description",
        [
            path("GROUP", "group_id"),
            body("DESCRIPTION", "description")
        ]
    ),
    route!(
        "group",
        "notification-preference",
        "PATCH",
        "/legacy-groups/v1/groups/{group_id}/notification-preference",
        [path("GROUP", "group_id")]
    ),
    route!(
        "group",
        "settings",
        "GET",
        "/legacy-groups/v1/groups/{group_id}/settings",
        [path("GROUP", "group_id")]
    ),
    route!(
        "group",
        "settings-update",
        "PATCH",
        "/legacy-groups/v1/groups/{group_id}/settings",
        [path("GROUP", "group_id")]
    ),
    route!(
        "group",
        "status",
        "PATCH",
        "/legacy-groups/v1/groups/{group_id}/status",
        [path("GROUP", "group_id"), body("MESSAGE", "message")]
    ),
    route!(
        "group",
        "pending",
        "GET",
        "/legacy-groups/v1/user/groups/pending"
    ),
    route!(
        "interaction",
        "following",
        "GET",
        "/legacy-followings/v2/users/{user_id}/universes",
        [path("USER", "user_id")]
    ),
    route!(
        "interaction",
        "following-v1",
        "GET",
        "/legacy-followings/v1/users/{user_id}/universes",
        [path("USER", "user_id")]
    ),
    route!(
        "interaction",
        "follow",
        "POST",
        "/legacy-followings/v1/users/{user_id}/universes/{universe_id}",
        [path("USER", "user_id"), path("UNIVERSE", "universe_id")]
    ),
    route!(
        "interaction",
        "unfollow",
        "DELETE",
        "/legacy-followings/v1/users/{user_id}/universes/{universe_id}",
        [path("USER", "user_id"), path("UNIVERSE", "universe_id")]
    ),
    route!(
        "interaction",
        "status",
        "GET",
        "/legacy-followings/v1/users/{user_id}/universes/{universe_id}/status",
        [path("USER", "user_id"), path("UNIVERSE", "universe_id")]
    ),
    route!(
        "team",
        "list",
        "GET",
        "/legacy-develop/v1/universes/multiget/teamcreate",
        [query("UNIVERSES", "ids")]
    ),
    route!(
        "team",
        "get",
        "GET",
        "/legacy-develop/v1/universes/{universe}/teamcreate"
    ),
    route!(
        "team",
        "update",
        "PATCH",
        "/legacy-develop/v1/universes/{universe}/teamcreate"
    ),
    route!(
        "team",
        "remove-members",
        "DELETE",
        "/legacy-develop/v1/universes/{universe}/teamcreate/memberships"
    ),
    route!("team", "members", "GET", "/legacy-develop/v1/places/{place}/teamcreate/active_session/members", page "limit", "cursor"),
    route!(
        "team",
        "stop-test",
        "DELETE",
        "/legacy-develop/v2/teamtest/{place}"
    ),
    route!(
        "localization",
        "badge-description",
        "PATCH",
        "/legacy-game-internationalization/v1/badges/{badge_id}/description/language-codes/{language}",
        [
            path("BADGE", "badge_id"),
            path("LANGUAGE", "language"),
            body("DESCRIPTION", "description")
        ]
    ),
    route!(
        "localization",
        "badge-icons",
        "GET",
        "/legacy-game-internationalization/v1/badges/{badge_id}/icons",
        [path("BADGE", "badge_id")]
    ),
    route!(
        "localization",
        "badge-icon-set",
        "POST",
        "/legacy-game-internationalization/v1/badges/{badge_id}/icons/language-codes/{language}",
        [
            path("BADGE", "badge_id"),
            path("LANGUAGE", "language"),
            file("FILE", "Files")
        ],
        multipart
    ),
    route!(
        "localization",
        "badge-icon-delete",
        "DELETE",
        "/legacy-game-internationalization/v1/badges/{badge_id}/icons/language-codes/{language}",
        [path("BADGE", "badge_id"), path("LANGUAGE", "language")]
    ),
    route!(
        "localization",
        "badge-info",
        "GET",
        "/legacy-game-internationalization/v1/badges/{badge_id}/name-description",
        [path("BADGE", "badge_id")]
    ),
    route!(
        "localization",
        "badge-info-set",
        "PATCH",
        "/legacy-game-internationalization/v1/badges/{badge_id}/name-description/language-codes/{language}",
        [
            path("BADGE", "badge_id"),
            path("LANGUAGE", "language"),
            body("NAME", "name"),
            body("DESCRIPTION", "description")
        ]
    ),
    route!(
        "localization",
        "badge-info-delete",
        "DELETE",
        "/legacy-game-internationalization/v1/badges/{badge_id}/name-description/language-codes/{language}",
        [path("BADGE", "badge_id"), path("LANGUAGE", "language")]
    ),
    route!(
        "localization",
        "badge-name",
        "PATCH",
        "/legacy-game-internationalization/v1/badges/{badge_id}/name/language-codes/{language}",
        [
            path("BADGE", "badge_id"),
            path("LANGUAGE", "language"),
            body("NAME", "name")
        ]
    ),
    route!(
        "localization",
        "product-description",
        "PATCH",
        "/legacy-game-internationalization/v1/developer-products/{product_id}/description/language-codes/{language}",
        [
            path("PRODUCT", "product_id"),
            path("LANGUAGE", "language"),
            body("DESCRIPTION", "description")
        ]
    ),
    route!(
        "localization",
        "product-icons",
        "GET",
        "/legacy-game-internationalization/v1/developer-products/{product_id}/icons",
        [path("PRODUCT", "product_id")]
    ),
    route!(
        "localization",
        "product-icon-set",
        "POST",
        "/legacy-game-internationalization/v1/developer-products/{product_id}/icons/language-codes/{language}",
        [
            path("PRODUCT", "product_id"),
            path("LANGUAGE", "language"),
            file("FILE", "Files")
        ],
        multipart
    ),
    route!(
        "localization",
        "product-icon-delete",
        "DELETE",
        "/legacy-game-internationalization/v1/developer-products/{product_id}/icons/language-codes/{language}",
        [path("PRODUCT", "product_id"), path("LANGUAGE", "language")]
    ),
    route!(
        "localization",
        "product-info",
        "GET",
        "/legacy-game-internationalization/v1/developer-products/{product_id}/name-description",
        [path("PRODUCT", "product_id")]
    ),
    route!(
        "localization",
        "product-info-set",
        "PATCH",
        "/legacy-game-internationalization/v1/developer-products/{product_id}/name-description/language-codes/{language}",
        [
            path("PRODUCT", "product_id"),
            path("LANGUAGE", "language"),
            body("NAME", "name"),
            body("DESCRIPTION", "description")
        ]
    ),
    route!(
        "localization",
        "product-info-delete",
        "DELETE",
        "/legacy-game-internationalization/v1/developer-products/{product_id}/name-description/language-codes/{language}",
        [path("PRODUCT", "product_id"), path("LANGUAGE", "language")]
    ),
    route!(
        "localization",
        "product-name",
        "PATCH",
        "/legacy-game-internationalization/v1/developer-products/{product_id}/name/language-codes/{language}",
        [
            path("PRODUCT", "product_id"),
            path("LANGUAGE", "language"),
            body("NAME", "name")
        ]
    ),
    route!(
        "localization",
        "pass-description",
        "PATCH",
        "/legacy-game-internationalization/v1/game-passes/{pass_id}/description/language-codes/{language}",
        [
            path("PASS", "pass_id"),
            path("LANGUAGE", "language"),
            body("DESCRIPTION", "description")
        ]
    ),
    route!(
        "localization",
        "pass-icons",
        "GET",
        "/legacy-game-internationalization/v1/game-passes/{pass_id}/icons",
        [path("PASS", "pass_id")]
    ),
    route!(
        "localization",
        "pass-icon-set",
        "POST",
        "/legacy-game-internationalization/v1/game-passes/{pass_id}/icons/language-codes/{language}",
        [
            path("PASS", "pass_id"),
            path("LANGUAGE", "language"),
            file("FILE", "Files")
        ],
        multipart
    ),
    route!(
        "localization",
        "pass-icon-delete",
        "DELETE",
        "/legacy-game-internationalization/v1/game-passes/{pass_id}/icons/language-codes/{language}",
        [path("PASS", "pass_id"), path("LANGUAGE", "language")]
    ),
    route!(
        "localization",
        "pass-info",
        "GET",
        "/legacy-game-internationalization/v1/game-passes/{pass_id}/name-description",
        [path("PASS", "pass_id")]
    ),
    route!(
        "localization",
        "pass-info-set",
        "PATCH",
        "/legacy-game-internationalization/v1/game-passes/{pass_id}/name-description/language-codes/{language}",
        [
            path("PASS", "pass_id"),
            path("LANGUAGE", "language"),
            body("NAME", "name"),
            body("DESCRIPTION", "description")
        ]
    ),
    route!(
        "localization",
        "pass-info-delete",
        "DELETE",
        "/legacy-game-internationalization/v1/game-passes/{pass_id}/name-description/language-codes/{language}",
        [path("PASS", "pass_id"), path("LANGUAGE", "language")]
    ),
    route!(
        "localization",
        "pass-name",
        "PATCH",
        "/legacy-game-internationalization/v1/game-passes/{pass_id}/name/language-codes/{language}",
        [
            path("PASS", "pass_id"),
            path("LANGUAGE", "language"),
            body("NAME", "name")
        ]
    ),
    route!(
        "localization",
        "game-icon",
        "GET",
        "/legacy-game-internationalization/v1/game-icon/games/{universe}"
    ),
    route!(
        "localization",
        "game-icon-set",
        "POST",
        "/legacy-game-internationalization/v1/game-icon/games/{universe}/language-codes/{language}",
        [path("LANGUAGE", "language"), file("FILE", "Files")],
        multipart
    ),
    route!(
        "localization",
        "game-icon-delete",
        "DELETE",
        "/legacy-game-internationalization/v1/game-icon/games/{universe}/language-codes/{language}",
        [path("LANGUAGE", "language")]
    ),
    route!(
        "localization",
        "thumbnail-alt",
        "POST",
        "/legacy-game-internationalization/v1/game-thumbnails/games/{universe}/language-codes/{language}/alt-text",
        [
            path("LANGUAGE", "language"),
            body("THUMBNAIL", "thumbnailId"),
            body("TEXT", "altText")
        ]
    ),
    route!(
        "localization",
        "thumbnail-image",
        "POST",
        "/legacy-game-internationalization/v1/game-thumbnails/games/{universe}/language-codes/{language}/image",
        [path("LANGUAGE", "language"), file("FILE", "Files")],
        multipart
    ),
    route!(
        "localization",
        "thumbnail-order",
        "POST",
        "/legacy-game-internationalization/v1/game-thumbnails/games/{universe}/language-codes/{language}/images/order",
        [path("LANGUAGE", "language")]
    ),
    route!(
        "localization",
        "thumbnail-delete",
        "DELETE",
        "/legacy-game-internationalization/v1/game-thumbnails/games/{universe}/language-codes/{language}/images/{image_id}",
        [path("LANGUAGE", "language"), path("IMAGE", "image_id")]
    ),
    route!(
        "localization",
        "game-history",
        "POST",
        "/legacy-game-internationalization/v1/name-description/games/translation-history"
    ),
    route!(
        "localization",
        "game-info",
        "PATCH",
        "/legacy-game-internationalization/v1/name-description/games/{universe}"
    ),
    route!(
        "localization",
        "source-language",
        "PATCH",
        "/legacy-game-internationalization/v1/source-language/games/{universe}",
        [query("LANGUAGE", "languageCode")]
    ),
    route!(
        "localization",
        "languages",
        "PATCH",
        "/legacy-game-internationalization/v1/supported-languages/games/{universe}",
        [root_body("LANGUAGES")]
    ),
    route!(
        "localization",
        "automatic-status",
        "GET",
        "/legacy-game-internationalization/v1/supported-languages/games/{universe}/automatic-translation-status"
    ),
    route!(
        "localization",
        "automatic-set",
        "PATCH",
        "/legacy-game-internationalization/v1/supported-languages/games/{universe}/languages/{language}/automatic-translation-status",
        [path("LANGUAGE", "language"), root_body("ENABLED")]
    ),
    route!(
        "localization",
        "image-translation-set",
        "PATCH",
        "/legacy-game-internationalization/v1/supported-languages/games/{universe}/languages/{language}/image-translation-status",
        [path("LANGUAGE", "language"), root_body("ENABLED")]
    ),
    route!(
        "localization",
        "display-translation-set",
        "PATCH",
        "/legacy-game-internationalization/v1/supported-languages/games/{universe}/languages/{language}/universe-display-info-automatic-translation-settings",
        [path("LANGUAGE", "language"), root_body("ENABLED")]
    ),
    route!(
        "localization",
        "display-translation",
        "GET",
        "/legacy-game-internationalization/v1/supported-languages/games/{universe}/universe-display-info-automatic-translation-settings"
    ),
    route!(
        "localization",
        "auto-table",
        "POST",
        "/legacy-localization-tables/v1/autolocalization/games/{universe}/autolocalizationtable"
    ),
    route!(
        "localization",
        "auto-settings",
        "PATCH",
        "/legacy-localization-tables/v1/autolocalization/games/{universe}/settings"
    ),
    route!(
        "localization",
        "metadata",
        "GET",
        "/legacy-localization-tables/v1/autolocalization/metadata"
    ),
    route!(
        "localization",
        "limits",
        "GET",
        "/legacy-localization-tables/v1/localization-table/limits"
    ),
    route!(
        "localization",
        "table",
        "GET",
        "/legacy-localization-tables/v1/localization-table/tables/{table_id}",
        [path("TABLE", "table_id")]
    ),
    route!(
        "localization",
        "table-update",
        "PATCH",
        "/legacy-localization-tables/v1/localization-table/tables/{table_id}",
        [path("TABLE", "table_id")]
    ),
    route!("localization", "entries", "GET", "/legacy-localization-tables/v1/localization-table/tables/{table_id}/entries", [path("TABLE", "table_id")], cursor "cursor"),
    route!(
        "localization",
        "entry-history",
        "POST",
        "/legacy-localization-tables/v1/localization-table/tables/{table_id}/entries/translation-history",
        [path("TABLE", "table_id")]
    ),
    route!(
        "localization",
        "entry-count",
        "GET",
        "/legacy-localization-tables/v1/localization-table/tables/{table_id}/entry-count",
        [path("TABLE", "table_id")]
    ),
    route!(
        "asset",
        "deliver",
        "GET",
        "/asset-delivery-api/v1/assetId/{assetId}",
        [path("ASSET", "assetId")]
    ),
    route!(
        "asset",
        "deliver-version",
        "GET",
        "/asset-delivery-api/v1/assetId/{assetId}/version/{versionNumber}",
        [path("ASSET", "assetId"), path("VERSION", "versionNumber")]
    ),
    route!(
        "asset",
        "permissions",
        "PATCH",
        "/asset-permissions-api/v1/assets/permissions",
        [],
        json "application/json-patch+json"
    ),
    route!(
        "asset",
        "create",
        "POST",
        "/assets/v1/assets",
        [
            body("TYPE", "assetType"),
            body("NAME", "displayName"),
            body("DESCRIPTION", "description"),
            file("FILE", "fileContent")
        ],
        multipart "request"
    ),
    route!(
        "asset",
        "get",
        "GET",
        "/assets/v1/assets/{assetId}",
        [path("ASSET", "assetId")]
    ),
    route!(
        "asset",
        "update",
        "PATCH",
        "/assets/v1/assets/{assetId}",
        [path_body("ASSET", "assetId", "assetId", "")],
        multipart "request"
    ),
    route!("asset", "versions", "GET", "/assets/v1/assets/{assetId}/versions", [path("ASSET", "assetId")], page "maxPageSize", "pageToken"),
    route!(
        "asset",
        "version",
        "GET",
        "/assets/v1/assets/{assetId}/versions/{versionNumber}",
        [path("ASSET", "assetId"), path("VERSION", "versionNumber")]
    ),
    route!(
        "asset",
        "rollback",
        "POST",
        "/assets/v1/assets/{assetId}/versions:rollback",
        [
            path("ASSET", "assetId"),
            asset_version_form("VERSION", "assetId", "assetVersion")
        ],
        multipart
    ),
    route!(
        "asset",
        "archive",
        "POST",
        "/assets/v1/assets/{assetId}:archive",
        [path("ASSET", "assetId")]
    ),
    route!(
        "asset",
        "restore",
        "POST",
        "/assets/v1/assets/{assetId}:restore",
        [path("ASSET", "assetId")]
    ),
    route!(
        "asset",
        "operation",
        "GET",
        "/assets/v1/operations/{operationId}",
        [path("OPERATION", "operationId")]
    ),
    route!(
        "asset",
        "search",
        "GET",
        "/toolbox-service/v2/assets:search",
        page "maxPageSize", "pageToken"
    ),
    route!(
        "asset",
        "toolbox-get",
        "GET",
        "/toolbox-service/v2/assets/{id}",
        [path("ASSET", "id")]
    ),
    route!(
        "asset",
        "thumbnail-3d",
        "GET",
        "/v1/assets-thumbnail-3d",
        [query("ASSET", "assetId")]
    ),
    route!(
        "creator-store",
        "get",
        "GET",
        "/cloud/v2/creator-store-products/{creator_store_product_id}",
        [path("PRODUCT", "creator_store_product_id")]
    ),
    route!(
        "creator-store",
        "create",
        "POST",
        "/cloud/v2/creator-store-products"
    ),
    route!(
        "creator-store",
        "update",
        "PATCH",
        "/cloud/v2/creator-store-products/{creator_store_product_id}",
        [path("PRODUCT", "creator_store_product_id")]
    ),
    route!("creator-store", "saves", "GET", "/toolbox-service/v1/saves", limit "limit"),
    route!(
        "creator-store",
        "save-create",
        "POST",
        "/toolbox-service/v1/saves"
    ),
    route!(
        "creator-store",
        "save-delete",
        "DELETE",
        "/toolbox-service/v1/saves"
    ),
    route!(
        "creator-store",
        "save-delete-batch",
        "POST",
        "/toolbox-service/v1/saves:bulkDelete"
    ),
    route!(
        "creator-store",
        "search",
        "POST",
        "/toolbox-service/v2/assets:search"
    ),
    route!("pass", "list", "GET", "/game-passes/v1/universes/{universe}/game-passes/creator", page "pageSize", "pageToken"),
    route!(
        "pass",
        "get",
        "GET",
        "/game-passes/v1/universes/{universe}/game-passes/{gamePassId}/creator",
        [path("PASS", "gamePassId")]
    ),
    route!(
        "pass",
        "create",
        "POST",
        "/game-passes/v1/universes/{universe}/game-passes",
        [form("NAME", "name")],
        multipart
    ),
    route!(
        "pass",
        "update",
        "PATCH",
        "/game-passes/v1/universes/{universe}/game-passes/{gamePassId}",
        [path("PASS", "gamePassId")],
        multipart
    ),
    route!(
        "config",
        "get",
        "GET",
        "/creator-configs-public-api/v1/configs/universes/{universe}/repositories/{repository}",
        [path("REPOSITORY", "repository")]
    ),
    route!(
        "config",
        "full",
        "GET",
        "/creator-configs-public-api/v1/configs/universes/{universe}/repositories/{repository}/full",
        [path("REPOSITORY", "repository")]
    ),
    route!(
        "config",
        "draft",
        "GET",
        "/creator-configs-public-api/v1/configs/universes/{universe}/repositories/{repository}/draft",
        [path("REPOSITORY", "repository")]
    ),
    route!(
        "config",
        "draft-update",
        "PATCH",
        "/creator-configs-public-api/v1/configs/universes/{universe}/repositories/{repository}/draft",
        [path("REPOSITORY", "repository")]
    ),
    route!(
        "config",
        "draft-overwrite",
        "PUT",
        "/creator-configs-public-api/v1/configs/universes/{universe}/repositories/{repository}/draft:overwrite",
        [path("REPOSITORY", "repository")]
    ),
    route!(
        "config",
        "draft-delete",
        "DELETE",
        "/creator-configs-public-api/v1/configs/universes/{universe}/repositories/{repository}/draft",
        [path("REPOSITORY", "repository")]
    ),
    route!(
        "config",
        "publish",
        "POST",
        "/creator-configs-public-api/v1/configs/universes/{universe}/repositories/{repository}/publish",
        [path("REPOSITORY", "repository")]
    ),
    route!("config", "revisions", "GET", "/creator-configs-public-api/v1/configs/universes/{universe}/repositories/{repository}/revisions", [path("REPOSITORY", "repository")], limit "MaxPageSize"),
    route!(
        "config",
        "restore",
        "POST",
        "/creator-configs-public-api/v1/configs/universes/{universe}/repositories/{repository}/revisions/{revisionId}/restore",
        [
            path("REPOSITORY", "repository"),
            path("REVISION", "revisionId")
        ]
    ),
    route!(
        "luau",
        "input",
        "POST",
        "/cloud/v2/universes/{universe}/luau-execution-session-task-binary-inputs"
    ),
    route!(
        "luau",
        "run",
        "POST",
        "/cloud/v2/universes/{universe}/places/{place}/luau-execution-session-tasks"
    ),
    route!(
        "luau",
        "run-version",
        "POST",
        "/cloud/v2/universes/{universe}/places/{place}/versions/{version_id}/luau-execution-session-tasks",
        [path("VERSION", "version_id")]
    ),
    route!(
        "luau",
        "task",
        "GET",
        "/cloud/v2/universes/{universe}/places/{place}/versions/{version_id}/luau-execution-sessions/{session_id}/tasks/{task_id}",
        [
            path("VERSION", "version_id"),
            path("SESSION", "session_id"),
            path("TASK", "task_id")
        ]
    ),
    route!("luau", "logs", "GET", "/cloud/v2/universes/{universe}/places/{place}/versions/{version_id}/luau-execution-sessions/{session_id}/tasks/{task_id}/logs", [path("VERSION", "version_id"), path("SESSION", "session_id"), path("TASK", "task_id")], page "maxPageSize", "pageToken"),
    route!(
        "server",
        "restarts",
        "GET",
        "/server-management/v1/universes/{universe}/restarts"
    ),
    route!(
        "server",
        "restart",
        "POST",
        "/server-management/v1/universes/{universe}/restarts"
    ),
    route!(
        "server",
        "forecast",
        "GET",
        "/server-management/v1/universes/{universe}/restarts:forecast"
    ),
    route!(
        "server",
        "filter-options",
        "GET",
        "/server-management/v1/universes/{universe}/places/{place}/game-servers:filter-options"
    ),
    route!("server", "list", "GET", "/server-management/v1/universes/{universe}/places/{place}/versions/{version}/game-servers", [path("VERSION", "version")], page "MaxPageSize", "PageToken"),
    route!("server", "logs", "GET", "/server-management/v1/universes/{universe}/places/{place}/versions/{version}/game-servers/{job}/logs", [path("VERSION", "version"), path("JOB", "job")], page "MaxPageSize", "PageToken"),
];

/// Actions that combine several requests; `servers.rs` runs them.
static COMPOSITES: &[(&str, &str, &str)] = &[
    (
        "server",
        "list",
        "active servers and players of the 10 newest versions that have servers, by version; --pages N per version (default 10)",
    ),
    (
        "server",
        "find JOB",
        "the server with this job ID and its version, from the same versions",
    ),
];

static ALIASES: &[(&str, &str, &str)] = &[
    ("universe", "restart-servers", "restart"),
    ("server", "restart-servers", "restart"),
];

const UNIVERSE_NAMES: &[&str] = &[
    "universe",
    "universe_id",
    "universeId",
    "game",
    "game_id",
    "gameId",
];
const PLACE_NAMES: &[&str] = &["place", "place_id", "placeId"];
const DESTRUCTIVE_WORDS: &[&str] = &[
    "restart",
    "shutdown",
    "flush",
    "delete",
    "remove",
    "deactivate",
    "archive",
    "discard",
    "rollback",
];

impl Route {
    fn paged(&self) -> bool {
        self.limit.is_some() && self.cursor.is_some()
    }

    fn destructive(&self) -> bool {
        self.method == "DELETE"
            || (self.method != "GET"
                && self
                    .action
                    .split('-')
                    .any(|word| DESTRUCTIVE_WORDS.contains(&word)))
    }

    fn usage(&self) -> String {
        std::iter::once(self.action)
            .chain(self.operands.iter().map(|operand| operand.label))
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// A resolved request context whose credentials are read on the first send
/// and reused by every request after it.
pub(super) struct Access<'a> {
    pub(super) identity: CloudIdentity,
    key_env: &'a str,
    oauth_env: Option<&'a str>,
    anonymous: bool,
    auth: OnceLock<CloudAuth>,
}

impl<'a> Access<'a> {
    pub(super) fn new(
        identity: CloudIdentity,
        key_env: &'a str,
        oauth_env: Option<&'a str>,
        anonymous: bool,
    ) -> Self {
        Self {
            identity,
            key_env,
            oauth_env,
            anonymous,
            auth: OnceLock::new(),
        }
    }

    pub(super) fn send(&self, request: Value) -> Result<Value> {
        let auth = match self.auth.get() {
            Some(auth) => auth,
            None => {
                let auth =
                    CloudAuth::from_env(self.anonymous, self.key_env, self.oauth_env, "cloud")
                        .map_err(cloud_error)?;
                self.auth.get_or_init(|| auth)
            }
        };
        execute_authorized(self.identity, auth, request).map_err(cloud_error)
    }
}

fn kebab(name: &str) -> String {
    let mut result = String::with_capacity(name.len() + 4);
    let mut after_word = false;
    for character in name.trim().chars() {
        if matches!(character, '_' | ' ') {
            result.push('-');
            after_word = false;
            continue;
        }
        if character.is_ascii_uppercase() && after_word {
            result.push('-');
        }
        after_word = character.is_ascii_lowercase() || character.is_ascii_digit();
        result.push(character.to_ascii_lowercase());
    }
    result
}

fn category_actions(category: &str) -> impl Iterator<Item = &'static str> + '_ {
    ROUTES
        .iter()
        .filter(move |route| route.category == category)
        .map(|route| route.action)
        .chain(
            COMPOSITES
                .iter()
                .filter(move |(name, _, _)| *name == category)
                .filter_map(|(_, usage, _)| usage.split_whitespace().next()),
        )
}

/// The canonical action for what was typed: kebab-cased, aliases resolved,
/// `show` read as `get`, and no action meaning `get` when it takes no values.
pub(super) fn resolve_action(category: &str, action: Option<&str>) -> Result<&'static str> {
    if category_actions(category).next().is_none() {
        bail!("Unknown Open Cloud category '{category}'");
    }
    let Some(action) = action else {
        return ROUTES
            .iter()
            .find(|route| {
                route.category == category && route.action == "get" && route.operands.is_empty()
            })
            .map(|route| route.action)
            .with_context(|| {
                format!(
                    "rbx oc {category} needs an ACTION: {}; `rbx oc {category} --help` shows each with its method and path",
                    category_actions(category).collect::<Vec<_>>().join(", ")
                )
            });
    };
    let name = kebab(action);
    let name = ALIASES
        .iter()
        .find(|(alias_category, alias, _)| *alias_category == category && *alias == name)
        .map_or(name.as_str(), |(_, _, target)| target);
    let name = if name == "show" { "get" } else { name };
    category_actions(category)
        .find(|candidate| *candidate == name)
        .with_context(|| available_error(category, action))
}

fn find_route(category: &str, action: &str) -> Result<&'static Route> {
    ROUTES
        .iter()
        .find(|route| route.category == category && route.action == action)
        .with_context(|| format!("rbx oc {category} {action} is not a single request"))
}

/// The help text listing a category's actions, one line each, read from the
/// route table.
pub(super) fn category_help(category: &str) -> String {
    use std::fmt::Write as _;

    let routes = ROUTES
        .iter()
        .filter(|route| route.category == category)
        .collect::<Vec<_>>();
    let mut rows = routes
        .iter()
        .map(|route| {
            let mut description = format!("{:<6} {}", route.method, route.path);
            if route.paged() {
                description.push_str("  (paged)");
            }
            if route.destructive() {
                description.push_str("  (destructive)");
            }
            (route.usage(), description)
        })
        .collect::<Vec<_>>();
    for (_, usage, summary) in COMPOSITES.iter().filter(|(name, _, _)| *name == category) {
        let action = usage.split_whitespace().next().unwrap_or(usage);
        let at = rows
            .iter()
            .rposition(|(row, _)| row.split_whitespace().next() == Some(action))
            .map_or(rows.len(), |index| index + 1);
        rows.insert(at, ((*usage).to_string(), (*summary).to_string()));
    }
    let width = rows.iter().map(|(usage, _)| usage.len()).max().unwrap_or(0);
    let mut text = String::from("Actions:\n");
    for (usage, description) in rows {
        let _ = writeln!(text, "  {usage:<width$}  {description}");
    }
    let mut sources = Vec::new();
    let uses = |names: &[&str]| {
        routes
            .iter()
            .any(|route| placeholders(route.path).any(|placeholder| names.contains(&placeholder)))
    };
    if uses(UNIVERSE_NAMES) {
        sources.push("{universe} from --universe or the project's experience");
    }
    if uses(PLACE_NAMES) {
        sources.push("{place} from --place-id or the project's place");
    }
    if uses(&["scope_id"]) {
        sources.push("{scope_id} from --scope");
    }
    if !sources.is_empty() {
        let _ = writeln!(text, "\nPath values: {}.", sources.join("; "));
    }
    if routes.iter().any(|route| route.paged()) {
        text.push_str(
            "Paged actions: -l N returns N items in total (100 per request), --all follows every page, --pages N stops after N; \"more\": true means another page exists.\n",
        );
    }
    let mut aliases = vec!["camelCase names work too".to_string()];
    if routes
        .iter()
        .any(|route| route.action == "get" && route.operands.is_empty())
    {
        aliases.insert(0, "no action or show = get".to_string());
    } else if routes.iter().any(|route| route.action == "get") {
        aliases.insert(0, "show = get".to_string());
    }
    aliases.extend(
        ALIASES
            .iter()
            .filter(|(name, _, _)| *name == category)
            .map(|(_, alias, target)| format!("{alias} = {target}")),
    );
    let _ = writeln!(text, "Aliases: {}.", aliases.join(", "));
    text
}

fn placeholders(path: &str) -> impl Iterator<Item = &str> {
    path.split('{')
        .skip(1)
        .filter_map(|part| part.split_once('}').map(|(name, _)| name))
}

pub(super) fn run(
    category: &str,
    access: &Access,
    mut args: RouteArgs,
    shape: impl FnOnce(&mut Value),
) -> Result<Value> {
    let action = resolve_action(category, args.action.as_deref())?;
    let route = find_route(category, action)?;
    let values = args.values.clone();
    let plan = Plan::new(args.limit, args.pages, args.all);
    let writes_file = args.output.is_some();
    let output = if route.paged() {
        args.output.take()
    } else {
        None
    };
    let request = build_request(category, access.identity, args)?;
    let (mut body, more) = match (route.limit, route.cursor) {
        (Some(size), Some(token)) => {
            let pages = Pager { size, token, plan }.collect(
                request,
                |request| Ok(access.send(request.clone())?["body"].take()),
                |_| false,
            )?;
            (pages.body, pages.more)
        }
        _ => (access.send(request)?["body"].take(), false),
    };
    if body.is_null() || body.as_object().is_some_and(Map::is_empty) {
        body = completed(route, access.identity, &values);
    }
    if route.category == "universe"
        && route.action == "get"
        && !writes_file
        && let Some(universe) = access.identity.game_id
        && let Some(object) = body.as_object_mut()
        && let Some(counts) = super::discovery::live_counts(universe)
    {
        object.extend(counts);
    }
    shape(&mut body);
    match output {
        Some(path) => write_output(&path, &body, more),
        None => Ok(body),
    }
}

/// What a request that answered with no body did, so the result still says
/// which action ran and on what.
fn completed(route: &Route, identity: CloudIdentity, values: &[String]) -> Value {
    let mut result = Map::new();
    result.insert("ok".to_string(), Value::Bool(true));
    result.insert(
        "action".to_string(),
        Value::String(route.action.to_string()),
    );
    let names = placeholders(route.path).collect::<Vec<_>>();
    if let Some(universe) = identity.game_id
        && names.iter().any(|name| UNIVERSE_NAMES.contains(name))
    {
        result.insert("universeId".to_string(), json!(universe));
    }
    if let Some(place) = identity.place_id
        && names.iter().any(|name| PLACE_NAMES.contains(name))
    {
        result.insert("placeId".to_string(), json!(place));
    }
    for (operand, value) in route.operands.iter().zip(values) {
        result.insert(camel_case(operand.label), parse_value(value));
    }
    Value::Object(result)
}

fn camel_case(label: &str) -> String {
    label
        .split('_')
        .filter(|part| !part.is_empty())
        .enumerate()
        .map(|(index, part)| {
            let lower = part.to_ascii_lowercase();
            if index == 0 {
                lower
            } else {
                let mut characters = lower.chars();
                characters.next().map_or_else(String::new, |first| {
                    first.to_ascii_uppercase().to_string() + characters.as_str()
                })
            }
        })
        .collect()
}

pub(super) fn write_output(path: &str, body: &Value, more: bool) -> Result<Value> {
    let path = absolute_path(Path::new(path));
    let bytes = serde_json::to_vec(body)?;
    atomic_write_file(&path, &bytes)?;
    let mut result = json!({ "file": path, "bytes": bytes.len() });
    if more {
        result["more"] = Value::Bool(true);
    }
    Ok(result)
}

fn build_request(category: &str, identity: CloudIdentity, args: RouteArgs) -> Result<Value> {
    let action = resolve_action(category, args.action.as_deref())?;
    let route = find_route(category, action)?;
    if args.values.len() != route.operands.len() {
        let usage = route
            .operands
            .iter()
            .map(|operand| operand.label)
            .collect::<Vec<_>>()
            .join(" ");
        bail!(
            "Expected: rbx oc {} {}{}",
            route.category,
            route.action,
            if usage.is_empty() {
                String::new()
            } else {
                format!(" {usage}")
            }
        );
    }

    let mut parts = RequestParts {
        path: Map::new(),
        query: assignments(&args.query)?,
        body: Map::new(),
        root_body: None,
        form: assignments(&args.form)?,
        files: Map::new(),
        raw_file: None,
    };
    // A query preset is the route's default; a `-q` value for the same name wins.
    for preset in route.presets {
        match preset.target {
            Target::Query(name) if parts.query.contains_key(name) => {}
            target => assign_target(target, parse_value(preset.value), &mut parts)?,
        }
    }
    for (operand, value) in route.operands.iter().zip(args.values) {
        assign_target(operand.target, parse_value(&value), &mut parts)?;
    }
    for field in &args.field {
        if parts.root_body.is_some() {
            bail!("Root body values cannot be combined with --field");
        }
        let (name, value) = assignment(field)?;
        insert_nested(&mut parts.body, name, value)?;
    }
    if route.category == "notification" && route.action == "send" {
        let universe = identity.game_id.context(
            "No universe ID is available. Run this in a Renium experience or pass --universe ID",
        )?;
        insert_nested(
            &mut parts.body,
            "source.universe",
            Value::String(format!("universes/{universe}")),
        )?;
    }
    if route.category == "team" && route.action == "stop-test" {
        let universe = identity.game_id.context(
            "No universe ID is available. Run this in a Renium experience or pass --universe ID",
        )?;
        parts.query.insert("gameId".to_string(), json!(universe));
    }
    let request_path = if route.path.contains("{scope_id}") {
        if route.category == "data" && args.scope.is_none() {
            route.path.replace("/scopes/{scope_id}", "")
        } else {
            parts.path.insert(
                "scope_id".to_string(),
                Value::String(args.scope.unwrap_or_else(|| "global".to_string())),
            );
            route.path.to_string()
        }
    } else if args.scope.is_some() {
        bail!("--scope isn't valid for this operation");
    } else {
        route.path.to_string()
    };
    if let Some(limit) = args.limit {
        let name = route
            .limit
            .context("--limit isn't valid for this operation")?;
        let size = if route.paged() {
            limit.min(PAGE_SIZE)
        } else {
            limit
        };
        parts.query.insert(name.to_string(), json!(size));
    }
    if (args.all || args.pages.is_some()) && !route.paged() {
        bail!("--all and --pages work with paged operations only");
    }
    let filled = |name: &str| {
        parts.path.contains_key(name)
            || (UNIVERSE_NAMES.contains(&name) && identity.game_id.is_some())
            || (PLACE_NAMES.contains(&name) && identity.place_id.is_some())
    };
    if let Some(name) = placeholders(&request_path).find(|name| !filled(name)) {
        bail!(
            "rbx oc {} {} needs {{{name}}} for {request_path}: {}",
            route.category,
            route.action,
            placeholder_hint(name)
        );
    }
    if let Some(cursor) = args.cursor {
        let name = route
            .cursor
            .context("--cursor isn't valid for this operation")?;
        parts.query.insert(name.to_string(), Value::String(cursor));
    }
    if let Some(filter) = args.filter {
        parts.query.insert(
            if route.category == "server" {
                "Filter"
            } else {
                "filter"
            }
            .to_string(),
            Value::String(filter),
        );
    }
    merge_assignments(&mut parts.files, &args.file)?;
    absolutize_files(&mut parts.files)?;
    let raw_file = parts
        .raw_file
        .map(|value| checked_file(&value, "raw file"))
        .transpose()?;
    let sends_json = matches!(route.method, "POST" | "PUT" | "PATCH")
        && parts.form.is_empty()
        && parts.files.is_empty()
        && raw_file.is_none();
    let (body, json_parts, content_type) = match route.body_mode {
        BodyMode::None => {
            if parts.root_body.is_some()
                || !parts.body.is_empty()
                || !parts.form.is_empty()
                || !parts.files.is_empty()
                || raw_file.is_some()
            {
                bail!("This operation doesn't accept a body");
            }
            (None, Map::new(), None)
        }
        BodyMode::Json(content_type) => {
            if !parts.body.is_empty() && (!parts.form.is_empty() || !parts.files.is_empty()) {
                bail!("Use either --field or multipart --form/--file values, not both");
            }
            (
                if let Some(root_body) = parts.root_body {
                    Some(root_body)
                } else if sends_json || !parts.body.is_empty() {
                    Some(Value::Object(parts.body))
                } else {
                    None
                },
                Map::new(),
                content_type,
            )
        }
        BodyMode::Multipart(part) => {
            if parts.root_body.is_some() {
                bail!("Multipart operations require named fields");
            }
            if raw_file.is_some() {
                bail!("This operation requires multipart files, not a raw file");
            }
            let mut json_parts = Map::new();
            if let Some(part) = part {
                json_parts.insert(part.to_string(), Value::Object(parts.body));
            } else if !parts.body.is_empty() {
                bail!("This operation uses --form and --file, not --field");
            }
            (None, json_parts, None)
        }
        BodyMode::Raw(content_type) => {
            if raw_file.is_none() {
                bail!("This operation requires a file");
            }
            if parts.root_body.is_some()
                || !parts.body.is_empty()
                || !parts.form.is_empty()
                || !parts.files.is_empty()
            {
                bail!("Raw uploads cannot include JSON or multipart fields");
            }
            (None, Map::new(), Some(content_type))
        }
    };
    Ok(json!({
        "method": route.method,
        "path": request_path,
        "pathParams": parts.path,
        "query": parts.query,
        "body": body,
        "form": parts.form,
        "jsonParts": json_parts,
        "files": parts.files,
        "rawFile": raw_file,
        "contentType": content_type,
        "ifMatch": args.if_match,
        "outputFile": args.output.map(|path| absolute_path(Path::new(&path)).display().to_string()),
    }))
}

pub(super) fn list(args: RoutesArgs) -> Result<Value> {
    if let Some(category) = args.category.as_deref()
        && !ROUTES.iter().any(|route| route.category == category)
    {
        bail!("Unknown Open Cloud category '{category}'");
    }
    let mut result = Map::new();
    for route in ROUTES.iter().filter(|route| {
        args.category
            .as_deref()
            .is_none_or(|value| value == route.category)
    }) {
        result
            .entry(route.category.to_string())
            .or_insert_with(|| Value::Array(Vec::new()))
            .as_array_mut()
            .expect("route category is always an array")
            .push(Value::String(route.usage()));
    }
    Ok(Value::Object(result))
}

fn available_error(category: &str, action: &str) -> String {
    format!(
        "Unknown {category} action '{action}'. Available: {}; `rbx oc {category} --help` shows each with its values, method and path",
        category_actions(category).collect::<Vec<_>>().join(", ")
    )
}

fn assign_target(target: Target, value: Value, parts: &mut RequestParts) -> Result<()> {
    match target {
        Target::Path(name) => {
            parts.path.insert(name.to_string(), scalar_string(value));
        }
        Target::Query(name) => {
            parts.query.insert(name.to_string(), value);
        }
        Target::Body(name) => insert_nested(&mut parts.body, name, value)?,
        Target::BodyList(name) => {
            let value = scalar_string(value);
            let values = value
                .as_str()
                .expect("scalar_string returns a string")
                .split(',')
                .filter(|value| !value.is_empty())
                .map(|value| Value::String(value.to_string()))
                .collect();
            insert_nested(&mut parts.body, name, Value::Array(values))?;
        }
        Target::RootBody => {
            if parts.root_body.replace(value).is_some() {
                bail!("Only one root body value is allowed");
            }
        }
        Target::Form(name) => {
            parts.form.insert(name.to_string(), value);
        }
        Target::File(name) => {
            parts.files.insert(name.to_string(), value);
        }
        Target::RawFile => {
            if parts.raw_file.replace(value).is_some() {
                bail!("Only one raw file is allowed");
            }
        }
        Target::PathBody {
            parameter,
            field,
            prefix,
        } => {
            let value = scalar_string(value);
            let text = value.as_str().expect("scalar_string returns a string");
            parts.path.insert(parameter.to_string(), value.clone());
            insert_nested(
                &mut parts.body,
                field,
                Value::String(format!("{prefix}{text}")),
            )?;
        }
        Target::AssetVersion {
            asset_parameter,
            field,
        } => {
            let asset = parts
                .path
                .get(asset_parameter)
                .and_then(Value::as_str)
                .with_context(|| format!("{asset_parameter} must be supplied first"))?;
            let version = scalar_string(value);
            let version = version.as_str().context("asset version must be a scalar")?;
            parts.form.insert(
                field.to_string(),
                Value::String(format!("assets/{asset}/versions/{version}")),
            );
        }
    }
    Ok(())
}

fn checked_file(value: &Value, label: &str) -> Result<String> {
    let path = value
        .as_str()
        .with_context(|| format!("{label} must be a path"))?;
    let path = absolute_path(Path::new(path));
    if !path.is_file() {
        bail!("File does not exist: {}", path.display());
    }
    Ok(path.display().to_string())
}

fn scalar_string(value: Value) -> Value {
    match value {
        Value::String(value) => Value::String(value),
        Value::Number(value) => Value::String(value.to_string()),
        Value::Bool(value) => Value::String(value.to_string()),
        value => Value::String(value.to_string()),
    }
}

fn insert_nested(map: &mut Map<String, Value>, path: &str, value: Value) -> Result<()> {
    let mut parts = path.split('.').peekable();
    let mut current = map;
    while let Some(part) = parts.next() {
        if part.is_empty() {
            bail!("Field path '{path}' contains an empty name");
        }
        if parts.peek().is_none() {
            current.insert(part.to_string(), value);
            return Ok(());
        }
        let entry = current
            .entry(part.to_string())
            .or_insert_with(|| Value::Object(Map::new()));
        current = entry
            .as_object_mut()
            .with_context(|| format!("Field path '{path}' overlaps a scalar field"))?;
    }
    bail!("Field names cannot be empty")
}

fn cloud_error(failure: Failure) -> anyhow::Error {
    match failure.0.d {
        Some(detail) => anyhow::anyhow!("{}\n{}", failure.0.m, detail),
        None => anyhow::anyhow!(failure.0.m),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    fn args(action: &str, values: &[&str]) -> RouteArgs {
        RouteArgs {
            action: Some(action.to_string()),
            values: values.iter().map(|value| (*value).to_string()).collect(),
            query: Vec::new(),
            field: Vec::new(),
            form: Vec::new(),
            file: Vec::new(),
            scope: None,
            limit: None,
            cursor: None,
            all: false,
            pages: None,
            filter: None,
            if_match: None,
            output: None,
        }
    }

    #[test]
    fn actions_resolve_aliases_case_and_defaults() {
        assert_eq!(
            resolve_action("universe", Some("restart")).unwrap(),
            "restart"
        );
        assert_eq!(
            resolve_action("universe", Some("restart-servers")).unwrap(),
            "restart"
        );
        assert_eq!(
            resolve_action("universe", Some("restartServers")).unwrap(),
            "restart"
        );
        assert_eq!(resolve_action("universe", None).unwrap(), "get");
        assert_eq!(resolve_action("universe", Some("show")).unwrap(), "get");
        assert_eq!(
            resolve_action("memory", Some("queueRead")).unwrap(),
            "queue-read"
        );
        assert_eq!(
            resolve_action("memory", Some("SORTED_LIST")).unwrap(),
            "sorted-list"
        );
        assert_eq!(resolve_action("server", Some("find")).unwrap(), "find");
        let error = resolve_action("universe", Some("reboot"))
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("Available: get, update, message, restart"),
            "{error}"
        );
        let error = resolve_action("data", None).unwrap_err().to_string();
        assert!(error.contains("needs an ACTION"), "{error}");
        assert!(resolve_action("nothing", Some("get")).is_err());
    }

    #[test]
    fn category_help_lists_every_action_with_method_path_and_warnings() {
        for category in ROUTES.iter().map(|route| route.category) {
            let help = category_help(category);
            for route in ROUTES.iter().filter(|route| route.category == category) {
                let line = help
                    .lines()
                    .find(|line| {
                        line.trim_start()
                            .starts_with(&format!("{} ", route.usage()))
                            && line.contains(route.path)
                    })
                    .unwrap_or_else(|| panic!("{category} help misses {}", route.usage()));
                assert!(line.contains(route.method), "{line}");
                assert_eq!(
                    line.contains("(destructive)"),
                    route.destructive(),
                    "{line}"
                );
                assert_eq!(line.contains("(paged)"), route.paged(), "{line}");
            }
        }
        let help = category_help("universe");
        assert!(help.contains("restart-servers = restart"), "{help}");
        assert!(help.contains("{universe} from --universe"), "{help}");
        let help = category_help("server");
        assert!(help.contains("\n  find JOB "), "{help}");
        assert!(help.contains("{place} from --place-id"), "{help}");
        let destructive = ROUTES
            .iter()
            .filter(|route| route.destructive())
            .map(|route| format!("{} {}", route.category, route.action))
            .collect::<Vec<_>>();
        for expected in [
            "universe restart",
            "server restart",
            "data delete",
            "team remove-members",
            "memory flush",
        ] {
            assert!(
                destructive.iter().any(|name| name == expected),
                "{expected}"
            );
        }
        for safe in ["data undelete", "server restarts", "universe activate"] {
            assert!(!destructive.iter().any(|name| name == safe), "{safe}");
        }
    }

    #[test]
    fn missing_universe_or_place_names_the_flag_before_any_request() {
        let error = build_request(
            "team",
            CloudIdentity {
                game_id: Some(1),
                place_id: None,
            },
            args("members", &[]),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("{place}"), "{error}");
        assert!(error.contains("--place-id"), "{error}");
        let error = build_request("universe", CloudIdentity::default(), args("get", &[]))
            .unwrap_err()
            .to_string();
        assert!(error.contains("--universe"), "{error}");
    }

    #[test]
    fn empty_responses_say_what_ran() {
        let identity = CloudIdentity {
            game_id: Some(123),
            place_id: Some(456),
        };
        let restart = find_route("universe", "restart").unwrap();
        assert_eq!(
            completed(restart, identity, &[]),
            json!({"ok": true, "action": "restart", "universeId": 123})
        );
        let discard = find_route("memory", "queue-discard").unwrap();
        assert_eq!(
            completed(discard, identity, &["jobs".to_string(), "r1".to_string()]),
            json!({"ok": true, "action": "queue-discard", "universeId": 123, "queue": "jobs", "readId": "r1"})
        );
        let stop = find_route("team", "stop-test").unwrap();
        assert_eq!(
            completed(stop, identity, &[]),
            json!({"ok": true, "action": "stop-test", "placeId": 456})
        );
    }

    #[test]
    fn paged_limits_start_at_one_page_and_paging_needs_a_paged_route() {
        let identity = CloudIdentity {
            game_id: Some(123),
            place_id: Some(456),
        };
        let mut logs = args("logs", &["2797", "job"]);
        logs.limit = Some(1000);
        let request = build_request("server", identity, logs).unwrap();
        assert_eq!(request["query"], json!({"MaxPageSize": 100}));
        let mut queue = args("queue-read", &["jobs"]);
        queue.limit = Some(500);
        let request = build_request("memory", identity, queue).unwrap();
        assert_eq!(request["query"], json!({"count": 500}));
        let mut get = args("get", &[]);
        get.all = true;
        assert!(build_request("universe", identity, get).is_err());
    }

    #[test]
    fn route_names_are_unique() {
        for (index, route) in ROUTES.iter().enumerate() {
            assert!(
                !ROUTES[..index].iter().any(|other| {
                    other.category == route.category && other.action == route.action
                })
            );
            assert!(route.path.starts_with('/'));

            let mut parameters = HashSet::from(["universe", "place", "scope_id"]);
            for target in route
                .operands
                .iter()
                .map(|operand| operand.target)
                .chain(route.presets.iter().map(|preset| preset.target))
            {
                match target {
                    Target::Path(name) => {
                        parameters.insert(name);
                    }
                    Target::PathBody { parameter, .. } => {
                        parameters.insert(parameter);
                    }
                    _ => {}
                }
            }
            for placeholder in route
                .path
                .split('{')
                .skip(1)
                .filter_map(|part| part.split_once('}').map(|(name, _)| name))
            {
                assert!(
                    parameters.contains(placeholder),
                    "{} {} has no value for {{{placeholder}}}",
                    route.category,
                    route.action
                );
            }

            let file = Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
            let values = route
                .operands
                .iter()
                .map(|operand| match operand.target {
                    Target::File(_) | Target::RawFile => file.display().to_string(),
                    Target::RootBody => "true".to_string(),
                    _ => "1".to_string(),
                })
                .collect::<Vec<_>>();
            let mut request = args(route.action, &[]);
            request.values = values;
            build_request(
                route.category,
                CloudIdentity {
                    game_id: Some(1),
                    place_id: Some(1),
                },
                request,
            )
            .unwrap_or_else(|error| {
                panic!(
                    "{} {} could not build: {error:#}",
                    route.category, route.action
                )
            });
        }
    }

    #[test]
    fn place_publish_defaults_to_a_published_version_unless_told_otherwise() {
        let identity = CloudIdentity {
            game_id: Some(123),
            place_id: Some(456),
        };
        let file = std::env::temp_dir().join("renium-publish-route-test.rbxl");
        std::fs::write(&file, b"<roblox!").unwrap();
        let path = file.to_string_lossy().into_owned();
        let request = build_request("place", identity, args("publish", &[&path])).unwrap();
        assert_eq!(request["query"], json!({"versionType": "Published"}));
        let mut saved = args("publish", &[&path]);
        saved.query = vec!["versionType=Saved".to_string()];
        let request = build_request("place", identity, saved).unwrap();
        assert_eq!(request["query"], json!({"versionType": "Saved"}));
        let _ = std::fs::remove_file(file);
    }

    #[test]
    fn nested_fields_keep_json_types() {
        let mut value = Map::new();
        insert_nested(&mut value, "payload.parameters.level", json!(7)).unwrap();
        assert_eq!(
            value,
            json!({"payload":{"parameters":{"level":7}}})
                .as_object()
                .unwrap()
                .clone()
        );
    }

    #[test]
    fn native_requests_bind_identity_and_values() {
        let identity = CloudIdentity {
            game_id: Some(123),
            place_id: Some(456),
        };

        let request = build_request(
            "data",
            identity,
            args("upsert", &["Players", "42", r#"{"coins":7}"#]),
        )
        .unwrap();
        assert_eq!(
            request,
            json!({
                "method": "PATCH",
                "path": "/cloud/v2/universes/{universe}/data-stores/{data_store_id}/entries/{entry_id}",
                "pathParams": {"data_store_id":"Players", "entry_id":"42"},
                "query": {"allowMissing":true},
                "body": {"value":{"coins":7}},
                "form": {}, "jsonParts": {}, "files": {}, "rawFile": null,
                "contentType": null, "ifMatch": null, "outputFile": null
            })
        );

        let mut scoped = args("get", &["Players", "42"]);
        scoped.scope = Some("profile".to_string());
        let request = build_request("data", identity, scoped).unwrap();
        assert_eq!(
            request["path"],
            "/cloud/v2/universes/{universe}/data-stores/{data_store_id}/scopes/{scope_id}/entries/{entry_id}"
        );
        assert_eq!(request["pathParams"]["scope_id"], "profile");

        let request =
            build_request("notification", identity, args("send", &["42", "daily"])).unwrap();
        assert_eq!(
            request["body"],
            json!({
                "source":{"universe":"universes/123"},
                "payload":{"messageId":"daily"}
            })
        );

        let request = build_request("asset", identity, args("rollback", &["99", "3"])).unwrap();
        assert_eq!(request["body"], Value::Null);
        assert_eq!(request["form"]["assetVersion"], "assets/99/versions/3");

        let mut search = args("search", &[]);
        search.limit = Some(25);
        search.cursor = Some("next".to_string());
        let request = build_request("asset", identity, search).unwrap();
        assert_eq!(
            request["query"],
            json!({"maxPageSize":25,"pageToken":"next"})
        );

        let mut servers = args("list", &["7"]);
        servers.limit = Some(10);
        servers.cursor = Some("page".to_string());
        servers.filter = Some("State=Running".to_string());
        let request = build_request("server", identity, servers).unwrap();
        assert_eq!(
            request["query"],
            json!({"MaxPageSize":10,"PageToken":"page","Filter":"State=Running"})
        );

        let mut revisions = args("revisions", &["flags"]);
        revisions.limit = Some(20);
        let request = build_request("config", identity, revisions).unwrap();
        assert_eq!(request["query"], json!({"MaxPageSize":20}));

        let mut queue = args("queue-read", &["jobs"]);
        queue.limit = Some(30);
        let request = build_request("memory", identity, queue).unwrap();
        assert_eq!(request["query"], json!({"count":30}));

        let mut badge = args("create", &[]);
        badge.form = vec!["name=Example".to_string()];
        let request = build_request("badge", identity, badge).unwrap();
        assert_eq!(request["form"], json!({"name":"Example"}));
        assert_eq!(request["body"], Value::Null);

        let mut badge = args("create", &[]);
        badge.field = vec!["name=Example".to_string()];
        assert!(
            build_request("badge", identity, badge)
                .unwrap_err()
                .to_string()
                .contains("--form and --file")
        );

        let mut permissions = args("permissions", &[]);
        permissions.field = vec![
            "subjectType=User".to_string(),
            "subjectId=42".to_string(),
            "action=Use".to_string(),
            "requests=[{\"assetId\":99}]".to_string(),
        ];
        let request = build_request("asset", identity, permissions).unwrap();
        assert_eq!(request["contentType"], "application/json-patch+json");
        assert_eq!(
            request["body"],
            json!({
                "subjectType":"User",
                "subjectId":42,
                "action":"Use",
                "requests":[{"assetId":99}]
            })
        );

        let file = Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
        let request = build_request(
            "asset",
            identity,
            args(
                "create",
                &[
                    "Model",
                    "Example",
                    "Example asset",
                    &file.display().to_string(),
                ],
            ),
        )
        .unwrap();
        assert_eq!(request["body"], Value::Null);
        assert_eq!(request["jsonParts"]["request"]["assetType"], "Model");
        assert_eq!(request["files"]["fileContent"], file.display().to_string());

        let request = build_request(
            "place",
            identity,
            args("publish", &[&file.display().to_string()]),
        )
        .unwrap();
        assert_eq!(request["rawFile"], file.display().to_string());
        assert_eq!(request["contentType"], "application/octet-stream");

        let request = build_request(
            "localization",
            identity,
            args("automatic-set", &["fr", "true"]),
        )
        .unwrap();
        assert_eq!(request["body"], true);
    }

    #[test]
    fn creator_operations_match_the_announced_open_cloud_contracts() {
        let identity = CloudIdentity {
            game_id: Some(123),
            place_id: Some(456),
        };

        let mut metrics = args("metrics", &[]);
        metrics.field = vec![
            "metric=DailyActiveUsers".to_string(),
            "granularity=OneDay".to_string(),
            "startTime=2026-01-01T00:00:00Z".to_string(),
            "endTime=2026-02-01T00:00:00Z".to_string(),
        ];
        let request = build_request("analytics", identity, metrics).unwrap();
        assert_eq!(request["method"], "POST");
        assert_eq!(
            request["path"],
            "/analytics-query-api/v1/universes/{universe}/metrics"
        );
        assert_eq!(request["body"]["metric"], "DailyActiveUsers");

        let request = build_request("experiment", identity, args("start", &["exp-1"])).unwrap();
        assert_eq!(request["method"], "POST");
        assert_eq!(request["body"], Value::Null);

        let mut events = args("list", &[]);
        events.limit = Some(10);
        events.cursor = Some("next".to_string());
        events.query = vec!["fields=id,title,startTime,visibility".to_string()];
        let request = build_request("event", identity, events).unwrap();
        assert_eq!(
            request["query"],
            json!({
                "fields":"id,title,startTime,visibility",
                "pageSize":10,
                "pageToken":"next"
            })
        );

        let file = Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
        let mut upload = args("upload", &[&file.display().to_string()]);
        upload.file = vec![format!("files={}", file.display())];
        let request = build_request("thumbnail", identity, upload).unwrap();
        assert_eq!(
            request["files"]["files"],
            json!([file.display().to_string(), file.display().to_string()])
        );

        let mut status = args("upload-status", &[]);
        status.query = vec![
            "operationIds=first".to_string(),
            "operationIds=second".to_string(),
        ];
        let request = build_request("thumbnail", identity, status).unwrap();
        assert_eq!(request["query"]["operationIds"], json!(["first", "second"]));
    }
}
