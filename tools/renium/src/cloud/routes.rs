use std::path::Path;

use anyhow::{Context, Result, bail};
use clap::Args;
use serde_json::{Map, Value, json};

use super::{CloudIdentity, execute_one};
use crate::automation::Failure;
use crate::system::files::absolutize_for_daemon as absolute_path;

#[derive(Args)]
pub(super) struct RouteArgs {
    action: String,
    #[arg(value_name = "VALUE")]
    values: Vec<String>,
    #[arg(short, long, value_name = "NAME=VALUE")]
    query: Vec<String>,
    #[arg(short, long, value_name = "NAME=VALUE")]
    field: Vec<String>,
    #[arg(long, value_name = "NAME=VALUE")]
    form: Vec<String>,
    #[arg(long, value_name = "NAME=PATH")]
    file: Vec<String>,
    #[arg(long)]
    scope: Option<String>,
    #[arg(short, long)]
    limit: Option<u32>,
    #[arg(long)]
    cursor: Option<String>,
    #[arg(long)]
    filter: Option<String>,
    #[arg(long)]
    if_match: Option<String>,
    #[arg(short, long, value_name = "PATH")]
    output: Option<String>,
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
    Json(Option<&'static str>),
    Multipart(&'static str),
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

const fn asset_version(
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
    ($category:literal, $action:literal, $method:literal, $path:literal, [$($operand:expr),* $(,)?], multipart $part:literal) => {
        route!(@make $category, $action, $method, $path, [$($operand),*], [], None, None, BodyMode::Multipart($part))
    };
    ($category:literal, $action:literal, $method:literal, $path:literal, [$($operand:expr),* $(,)?], json $content_type:literal) => {
        route!(@make $category, $action, $method, $path, [$($operand),*], [], None, None, BodyMode::Json(Some($content_type)))
    };
    ($category:literal, $action:literal, $method:literal, $path:literal, [$($operand:expr),* $(,)?], raw $content_type:literal) => {
        route!(@make $category, $action, $method, $path, [$($operand),*], [], None, None, BodyMode::Raw($content_type))
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
    route!("memory", "queue-read", "GET", "/cloud/v2/universes/{universe}/memory-store/queues/{queue_id}/items:read", [path("QUEUE", "queue_id")], page "count", "pageToken"),
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
        "/ads-management/v1/billing-accounts"
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
        "/ads-management/v1/campaigns"
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
        "/ads-management/v1/creatives"
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
        "/legacy-badges/v1/universes/{universe}/badges"
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
        [path("BADGE", "badgeId"), file("FILE", "Files")]
    ),
    route!(
        "experiment",
        "list",
        "GET",
        "/creator-configs-public-api/v1/experimentation/universes/{universe}/experiments"
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
        [path("EXPERIMENT", "experimentId")]
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
        "/virtual-events/v3/universes/{universe}/game-events"
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
        "/matchmaking-api/v1/game-instances/shutdown-all"
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
        "/thumbnail-personalization-api/v1/universes/{universe}/personalization"
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
        "/thumbnail-personalization-api/v1/universes/{universe}/thumbnails"
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
        "/thumbnail-personalization-api/v1/universes/{universe}/thumbnails/uploads"
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
        [path("USER", "user_id")]
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
        ]
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
        ]
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
        ]
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
        [path("LANGUAGE", "language"), file("FILE", "Files")]
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
        [path("LANGUAGE", "language"), file("FILE", "Files")]
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
    route!("localization", "entries", "GET", "/legacy-localization-tables/v1/localization-table/tables/{table_id}/entries", [path("TABLE", "table_id")], page "limit", "cursor"),
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
            asset_version("VERSION", "assetId", "assetVersion")
        ]
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
        "/toolbox-service/v2/assets:search"
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
    route!("creator-store", "saves", "GET", "/toolbox-service/v1/saves"),
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
        [form("NAME", "name")]
    ),
    route!(
        "pass",
        "update",
        "PATCH",
        "/game-passes/v1/universes/{universe}/game-passes/{gamePassId}",
        [path("PASS", "gamePassId")]
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
    route!("config", "revisions", "GET", "/creator-configs-public-api/v1/configs/universes/{universe}/repositories/{repository}/revisions", [path("REPOSITORY", "repository")], page "limit", "cursor"),
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
    route!("server", "restarts", "GET", "/server-management/v1/universes/{universe}/restarts", page "pageSize", "pageToken"),
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
    route!("server", "list", "GET", "/server-management/v1/universes/{universe}/places/{place}/versions/{version}/game-servers", [path("VERSION", "version")], page "pageSize", "pageToken"),
    route!("server", "logs", "GET", "/server-management/v1/universes/{universe}/places/{place}/versions/{version}/game-servers/{job}/logs", [path("VERSION", "version"), path("JOB", "job")], page "pageSize", "pageToken"),
];

pub(super) fn run(
    category: &str,
    identity: CloudIdentity,
    key_env: &str,
    oauth_env: Option<&str>,
    args: RouteArgs,
) -> Result<Value> {
    let request = build_request(category, identity, args)?;
    let response =
        execute_one(identity, key_env, oauth_env, false, request).map_err(cloud_error)?;
    compact_response(response)
}

fn build_request(category: &str, identity: CloudIdentity, args: RouteArgs) -> Result<Value> {
    let route = ROUTES
        .iter()
        .find(|route| route.category == category && route.action == args.action)
        .with_context(|| available_error(category, &args.action))?;
    if args.values.len() != route.operands.len() {
        let usage = route
            .operands
            .iter()
            .map(|operand| operand.label)
            .collect::<Vec<_>>()
            .join(" ");
        bail!(
            "Expected: rbx cloud {} {}{}",
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
    for preset in route.presets {
        assign_target(preset.target, parse_value(preset.value), &mut parts)?;
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
        parts.query.insert(name.to_string(), json!(limit));
    }
    if let Some(cursor) = args.cursor {
        let name = route
            .cursor
            .context("--cursor isn't valid for this operation")?;
        parts.query.insert(name.to_string(), Value::String(cursor));
    }
    if let Some(filter) = args.filter {
        parts
            .query
            .insert("filter".to_string(), Value::String(filter));
    }
    parts.files.extend(assignments(&args.file)?);
    for value in parts.files.values_mut() {
        let path = value.as_str().context("--file values must be paths")?;
        let path = absolute_path(Path::new(path));
        if !path.is_file() {
            bail!("File does not exist: {}", path.display());
        }
        *value = Value::String(path.display().to_string());
    }
    let raw_file = parts
        .raw_file
        .map(|value| checked_file(&value, "raw file"))
        .transpose()?;
    let sends_json = matches!(route.method, "POST" | "PUT" | "PATCH")
        && parts.form.is_empty()
        && parts.files.is_empty()
        && raw_file.is_none();
    let (body, json_parts, content_type) = match route.body_mode {
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
            json_parts.insert(part.to_string(), Value::Object(parts.body));
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
        let usage = std::iter::once(route.action)
            .chain(route.operands.iter().map(|operand| operand.label))
            .collect::<Vec<_>>()
            .join(" ");
        result
            .entry(route.category.to_string())
            .or_insert_with(|| Value::Array(Vec::new()))
            .as_array_mut()
            .expect("route category is always an array")
            .push(Value::String(usage));
    }
    Ok(Value::Object(result))
}

fn available_error(category: &str, action: &str) -> String {
    let actions = ROUTES
        .iter()
        .filter(|route| route.category == category)
        .map(|route| route.action)
        .collect::<Vec<_>>();
    if actions.is_empty() {
        format!("Unknown Open Cloud category '{category}'")
    } else {
        format!(
            "Unknown {category} action '{action}'. Available: {}",
            actions.join(", ")
        )
    }
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
            insert_nested(
                &mut parts.body,
                field,
                Value::String(format!("assets/{asset}/versions/{version}")),
            )?;
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

fn assignments(values: &[String]) -> Result<Map<String, Value>> {
    values
        .iter()
        .map(|value| {
            let (name, value) = assignment(value)?;
            Ok((name.to_string(), value))
        })
        .collect()
}

fn assignment(value: &str) -> Result<(&str, Value)> {
    let (name, value) = value
        .split_once('=')
        .with_context(|| format!("Expected NAME=VALUE, got '{value}'"))?;
    if name.is_empty() {
        bail!("Assignment names cannot be empty");
    }
    Ok((name, parse_value(value)))
}

fn parse_value(value: &str) -> Value {
    serde_json::from_str(value).unwrap_or_else(|_| Value::String(value.to_string()))
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

fn compact_response(response: Value) -> Result<Value> {
    let status = response.get("status").cloned().unwrap_or(json!(200));
    match response.get("body") {
        Some(Value::Null) | None => Ok(json!({ "ok": true, "status": status })),
        Some(body) => Ok(body.clone()),
    }
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
            action: action.to_string(),
            values: values.iter().map(|value| (*value).to_string()).collect(),
            query: Vec::new(),
            field: Vec::new(),
            form: Vec::new(),
            file: Vec::new(),
            scope: None,
            limit: None,
            cursor: None,
            filter: None,
            if_match: None,
            output: None,
        }
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
        }
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
        assert_eq!(
            request["body"],
            json!({"assetVersion":"assets/99/versions/3"})
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
}
