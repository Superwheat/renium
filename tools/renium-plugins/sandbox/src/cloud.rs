//! Only dedicated, private universes owned by the configured user or group are
//! accepted. Cloud calls go through Renium's Open Cloud client with a stored key
//! name or an environment variable name, so the secret never reaches this plugin
//! or its journal. Cleanup marks entries deleted; Roblox retains old revisions.
use crate::{Pool, Slot};
use renium_plugin_sdk::{Context, PluginContext, Result, Value, bail};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::path::Path;
use std::time::Duration;

// Renium retries throttled requests itself, waiting up to 15 seconds each time.
const CALL_TIMEOUT: Duration = Duration::from_secs(75);
pub(super) const PUBLISH_TIMEOUT: Duration = Duration::from_secs(150);

/// An Open Cloud reply: a 2xx body, or the HTTP status of a refusal.
pub(super) enum Reply {
    Body(Value),
    Status(u16),
}

pub(super) trait Transport {
    fn send(&self, method: &str, path: &str, query: &[(&str, &str)]) -> Result<Reply>;
    fn publish(&self, universe: u64, place: u64, file: &Path) -> Result<Value>;
}

/// Sends every request through `rbx oc`, which holds the credential.
pub(super) struct Renium<'a> {
    ctx: &'a PluginContext,
    credential: [String; 2],
}

impl<'a> Renium<'a> {
    pub(super) fn new(ctx: &'a PluginContext, pool: &Pool) -> Result<Self> {
        let credential = match (&pool.key_name, &pool.key_env) {
            (Some(name), None) => ["--key".to_owned(), name.clone()],
            (None, Some(env)) => ["--key-env".to_owned(), env.clone()],
            _ => bail!("The pool must name exactly one of keyName or keyEnv"),
        };
        Ok(Self { ctx, credential })
    }

    fn call(&self, mut args: Vec<String>, timeout: Duration) -> Result<Reply> {
        args.extend(self.credential.iter().cloned());
        let refs = args.iter().map(String::as_str).collect::<Vec<_>>();
        match self.ctx.renium_timeout(&refs, timeout) {
            Ok(value) => Ok(Reply::Body(value)),
            Err(error) => match http_status(&format!("{error:#}")) {
                Some(status) => Ok(Reply::Status(status)),
                None => Err(error.context(
                    "Sandbox cloud request failed; the slot stays reserved. Check the key name and connectivity before resuming",
                )),
            },
        }
    }
}

impl Transport for Renium<'_> {
    fn send(&self, method: &str, path: &str, query: &[(&str, &str)]) -> Result<Reply> {
        let mut args = vec![
            "oc".to_owned(),
            "request".to_owned(),
            method.to_ascii_lowercase(),
            format!("/cloud/v2/{path}"),
        ];
        for (name, value) in query.iter().filter(|(_, value)| !value.is_empty()) {
            args.push("-q".to_owned());
            args.push(format!("{name}={value}"));
        }
        Ok(match self.call(args, CALL_TIMEOUT)? {
            Reply::Body(response) => {
                let status = response["status"]
                    .as_u64()
                    .context("Open Cloud response omitted its status")?;
                if (200..300).contains(&status) {
                    Reply::Body(response.get("body").cloned().unwrap_or(Value::Null))
                } else {
                    Reply::Status(u16::try_from(status).unwrap_or(u16::MAX))
                }
            }
            refused => refused,
        })
    }

    fn publish(&self, universe: u64, place: u64, file: &Path) -> Result<Value> {
        let args = vec![
            "oc".to_owned(),
            "place".to_owned(),
            "publish".to_owned(),
            file.to_str()
                .context("Blank place path is not UTF-8")?
                .to_owned(),
            "--universe".to_owned(),
            universe.to_string(),
            "--place-id".to_owned(),
            place.to_string(),
            "-q".to_owned(),
            "versionType=Published".to_owned(),
        ];
        match self.call(args, PUBLISH_TIMEOUT)? {
            Reply::Body(value) => Ok(value),
            Reply::Status(status) => bail!(
                "Blank-place publishing returned HTTP {status}; the slot remains reserved. Check the key's universe-places write scope"
            ),
        }
    }
}

// Renium reports a refused request as "Open Cloud request N returned HTTP 404".
fn http_status(text: &str) -> Option<u16> {
    let (_, rest) = text.split_once("returned HTTP ")?;
    rest.get(..3)?
        .parse()
        .ok()
        .filter(|status| (400..600).contains(status))
}

pub(super) struct Client<'a> {
    transport: Box<dyn Transport + 'a>,
    slot: &'a Slot,
}

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct Cleanup {
    stage: Stage,
    stores: Vec<String>,
    cursor: String,
    index: usize,
    pending: VecDeque<String>,
    page_loaded: bool,
    next_cursor: String,
    memory_operation: Option<String>,
    pub(super) deleted: u64,
}

#[derive(Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
enum Stage {
    #[default]
    ListStores,
    ClearStandard,
    ClearOrdered,
    VerifyStores,
    VerifyStandard,
    VerifyOrdered,
    FlushMemory,
    WaitMemory,
    Done,
}

impl Cleanup {
    /// The flush was accepted and Roblox is still finishing it.
    pub(super) fn awaiting_flush(&self) -> bool {
        self.stage == Stage::WaitMemory
    }
}

impl<'a> Client<'a> {
    pub(super) fn new(ctx: &'a PluginContext, pool: &Pool, slot: &'a Slot) -> Result<Self> {
        Ok(Self::with_transport(
            Box::new(Renium::new(ctx, pool)?),
            slot,
        ))
    }

    pub(super) fn with_transport(transport: Box<dyn Transport + 'a>, slot: &'a Slot) -> Self {
        Self { transport, slot }
    }

    fn request(
        &self,
        method: &str,
        resource: &str,
        query: &[(&str, &str)],
        allow_missing: bool,
    ) -> Result<Value> {
        let prefix = format!("universes/{}", self.slot.universe_id);
        if resource != prefix && !resource.starts_with(&format!("{prefix}/")) {
            bail!("Cloud resource is outside this sandbox universe");
        }
        let url = url::Url::parse(&format!("https://apis.roblox.com/cloud/v2/{resource}"))?;
        if url.host_str() != Some("apis.roblox.com")
            || !(url.path() == format!("/cloud/v2/{prefix}")
                || url.path().starts_with(&format!("/cloud/v2/{prefix}/")))
            || url.query().is_some()
            || url.fragment().is_some()
            || resource.split('/').any(|s| matches!(s, "." | ".."))
        {
            bail!("Unexpected sandbox cloud resource path");
        }
        let value = match self.transport.send(method, resource, query)? {
            Reply::Status(404) if allow_missing => return Ok(Value::Null),
            Reply::Status(code) => bail!(
                "Sandbox cloud request returned HTTP {code}; the slot stays reserved. Check its key scopes or rate limit before resuming."
            ),
            Reply::Body(value) => value,
        };
        if value.is_null() {
            if method == "GET" {
                bail!("Cloud read returned an empty response; absence was not verified");
            }
            return Ok(Value::Null);
        }
        if method == "GET" && !value.is_object() {
            bail!("Cloud read did not return an object");
        }
        Ok(value)
    }

    pub(super) fn validate_slot(&self, owner: &str) -> Result<()> {
        let universe = self.request(
            "GET",
            &format!("universes/{}", self.slot.universe_id),
            &[],
            false,
        )?;
        let expected_place = format!(
            "universes/{}/places/{}",
            self.slot.universe_id, self.slot.place_id
        );
        let actual_owner = universe["group"]
            .as_str()
            .or_else(|| universe["user"].as_str());
        if actual_owner != Some(owner)
            || universe["visibility"].as_str() != Some("PRIVATE")
            || universe["rootPlace"].as_str() != Some(expected_place.as_str())
        {
            bail!(
                "Sandbox must be the root place of a PRIVATE experience owned by {owner}; this universe is owned by {} and {}",
                actual_owner.unwrap_or("nobody readable"),
                universe["visibility"]
                    .as_str()
                    .unwrap_or("of unknown visibility")
            );
        }
        Ok(())
    }

    pub(super) fn publish_blank(&self, file: &Path) -> Result<()> {
        let result = self
            .transport
            .publish(self.slot.universe_id, self.slot.place_id, file)?;
        if result["versionNumber"].as_u64().is_none() {
            bail!("Blank-place publish omitted versionNumber; do not release this slot");
        }
        Ok(())
    }

    pub(super) fn cleanup_step(&self, state: &mut Cleanup) -> Result<bool> {
        match state.stage {
            Stage::ListStores | Stage::VerifyStores => {
                let resource = format!("universes/{}/data-stores", self.slot.universe_id);
                let page = self.request(
                    "GET",
                    &resource,
                    &[("maxPageSize", "100"), ("pageToken", &state.cursor)],
                    false,
                )?;
                for store in array(&page, "dataStores")? {
                    state.stores.push(
                        store["path"]
                            .as_str()
                            .context("DataStore omitted its resource path")?
                            .to_owned(),
                    );
                }
                let next = cursor(&page)?;
                if !next.is_empty() && next == state.cursor {
                    bail!("Cloud pagination stopped advancing; slot remains reserved");
                }
                state.cursor = next;
                if state.cursor.is_empty() {
                    state.stage = if state.stage == Stage::ListStores {
                        Stage::ClearStandard
                    } else {
                        Stage::VerifyStandard
                    };
                }
            }
            Stage::ClearStandard | Stage::VerifyStandard => {
                if state.index == state.stores.len() {
                    state.index = 0;
                    state.stage = if state.stage == Stage::ClearStandard {
                        Stage::ClearOrdered
                    } else {
                        Stage::VerifyOrdered
                    };
                } else {
                    let path = format!("{}/scopes/-/entries", state.stores[state.index]);
                    let verify = state.stage == Stage::VerifyStandard;
                    self.entries(state, &path, "dataStoreEntries", verify)?;
                }
            }
            Stage::ClearOrdered | Stage::VerifyOrdered => {
                if state.index == self.slot.ordered_stores.len() {
                    state.index = 0;
                    if state.stage == Stage::ClearOrdered {
                        state.stores.clear();
                        state.stage = Stage::VerifyStores;
                    } else {
                        state.stage = Stage::FlushMemory;
                    }
                } else {
                    let store = &self.slot.ordered_stores[state.index];
                    let path = format!(
                        "universes/{}/ordered-data-stores/{}/scopes/{}/entries",
                        self.slot.universe_id,
                        segment(&store.name),
                        segment(&store.scope)
                    );
                    let verify = state.stage == Stage::VerifyOrdered;
                    self.entries(state, &path, "orderedDataStoreEntries", verify)?;
                }
            }
            Stage::FlushMemory => {
                let response = self.request(
                    "POST",
                    &format!("universes/{}/memory-store:flush", self.slot.universe_id),
                    &[],
                    false,
                )?;
                check_operation(&response)?;
                if response["done"].as_bool() == Some(true) {
                    state.stage = Stage::Done;
                } else {
                    state.memory_operation = Some(
                        operation_resource(
                            response["path"]
                                .as_str()
                                .context("Memory flush omitted its operation path")?,
                        )
                        .to_owned(),
                    );
                    state.stage = Stage::WaitMemory;
                }
            }
            Stage::WaitMemory => {
                let response = self.request(
                    "GET",
                    operation_resource(
                        state
                            .memory_operation
                            .as_deref()
                            .context("Missing memory operation")?,
                    ),
                    &[],
                    false,
                )?;
                check_operation(&response)?;
                if response["done"].as_bool() == Some(true) {
                    state.stage = Stage::Done;
                }
            }
            Stage::Done => return Ok(true),
        }
        Ok(state.stage == Stage::Done)
    }

    fn entries(
        &self,
        state: &mut Cleanup,
        resource: &str,
        field: &str,
        verify: bool,
    ) -> Result<()> {
        if let Some(path) = state.pending.front() {
            // DELETE is idempotent. Persisting after confirmation makes interruption safe.
            self.request("DELETE", path, &[], true)?;
            let result = self.request("GET", path, &[], true)?;
            if !result.is_null() && result["state"].as_str() != Some("DELETED") {
                bail!("A deleted test entry is still active; the slot remains reserved");
            }
            state.pending.pop_front();
            state.deleted += 1;
            return Ok(());
        }
        if state.page_loaded {
            state.page_loaded = false;
            state.cursor = std::mem::take(&mut state.next_cursor);
            if state.cursor.is_empty() {
                state.index += 1;
            }
            return Ok(());
        }
        let page = self.request(
            "GET",
            resource,
            &[("maxPageSize", "100"), ("pageToken", &state.cursor)],
            field == "orderedDataStoreEntries",
        )?;
        if page.is_null() {
            // A declared ordered store may not have been created by this test run.
            state.cursor.clear();
            state.index += 1;
            return Ok(());
        }
        let entries = array(&page, field)?;
        if verify && !entries.is_empty() {
            bail!(
                "Data reappeared during cleanup verification; the slot remains reserved for investigation"
            );
        }
        for entry in entries {
            let path = entry["path"]
                .as_str()
                .context("DataStore entry omitted its resource path")?;
            // Do not let a malformed entry path turn an entry deletion into a store deletion.
            let prefix = resource
                .strip_suffix("/scopes/-/entries")
                .map(|store| format!("{store}/scopes/"));
            let valid = if let Some(prefix) = prefix {
                path.starts_with(&prefix)
                    && path[prefix.len()..].split('/').count() == 3
                    && path[prefix.len()..].split('/').nth(1) == Some("entries")
            } else {
                path.strip_prefix(&format!("{resource}/"))
                    .is_some_and(|id| !id.is_empty() && !id.contains('/'))
            };
            if !valid {
                bail!("Unexpected entry resource path; refusing deletion");
            }
            state.pending.push_back(path.to_owned());
        }
        state.next_cursor = cursor(&page)?;
        if !state.next_cursor.is_empty() && state.next_cursor == state.cursor {
            bail!("Entry pagination stopped advancing");
        }
        state.page_loaded = true;
        Ok(())
    }
}

// Roblox returns long-running operation paths with the API prefix, for example
// "cloud/v2/universes/1/memory-store/operations/ID"; requests take the resource.
fn operation_resource(path: &str) -> &str {
    path.strip_prefix("cloud/v2/").unwrap_or(path)
}

fn segment(value: &str) -> String {
    url::form_urlencoded::byte_serialize(value.as_bytes())
        .collect::<String>()
        .replace('+', "%20")
}

fn array<'a>(response: &'a Value, field: &str) -> Result<&'a [Value]> {
    let object = response
        .as_object()
        .context("Expected cloud response object")?;
    match object.get(field) {
        None => Ok(&[]), // Protobuf JSON omits empty repeated fields.
        Some(Value::Array(values)) => Ok(values),
        _ => bail!("Invalid cloud collection {field}"),
    }
}

fn cursor(response: &Value) -> Result<String> {
    match response.get("nextPageToken") {
        None => Ok(String::new()),
        Some(Value::String(value)) => Ok(value.clone()),
        _ => bail!("Invalid cloud page token"),
    }
}

// A finished flush is `{"done": true}`: its empty result is omitted, and only
// an `error` field reports failure.
fn check_operation(response: &Value) -> Result<()> {
    if response.get("error").is_some() {
        bail!("MemoryStore flush failed; the slot remains reserved");
    }
    if response.get("done").is_some_and(|done| !done.is_boolean()) {
        bail!("MemoryStore flush reported an unreadable state; the slot remains reserved");
    }
    Ok(())
}

#[cfg(test)]
#[path = "cloud_tests.rs"]
mod tests;
