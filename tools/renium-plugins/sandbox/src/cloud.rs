//! Only dedicated, private, group-owned universes are accepted. Credentials never
//! enter the journal. Cleanup marks entries deleted; Roblox retains old revisions.
use crate::{Pool, Slot};
use renium_plugin_sdk::{Context, Result, Value, bail};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::time::Duration;

pub(super) struct Client<'a> {
    agent: ureq::Agent,
    key: String,
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

impl<'a> Client<'a> {
    pub(super) fn new(pool: &Pool, slot: &'a Slot) -> Result<Self> {
        let key = std::env::var(&pool.key_env).with_context(|| {
            format!(
                "Set {} to an API key scoped ONLY to the throwaway universes",
                pool.key_env
            )
        })?;
        if key.trim().is_empty() {
            bail!("Sandbox API key is empty");
        }
        Ok(Self {
            agent: ureq::AgentBuilder::new()
                .timeout(Duration::from_secs(10))
                .redirects(0)
                .build(),
            key,
            slot,
        })
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
        let mut request = self
            .agent
            .request(method, url.as_str())
            .set("x-api-key", &self.key);
        for (key, value) in query {
            request = request.query(key, value);
        }
        let response = match request.call() {
            Ok(response) => response,
            Err(ureq::Error::Status(404, _)) if allow_missing => return Ok(Value::Null),
            Err(ureq::Error::Status(code, _)) => bail!(
                "Sandbox cloud request returned HTTP {code}; the slot stays reserved. Check its key scopes or rate limit before resuming."
            ),
            Err(_) => bail!(
                "Sandbox cloud request failed or timed out; the slot stays reserved. Check connectivity before resuming."
            ),
        };
        if !(200..300).contains(&response.status()) {
            bail!("Unexpected cloud redirect/status; no redirected request was sent");
        }
        let mut bytes = Vec::new();
        response
            .into_reader()
            .take(4_194_305)
            .read_to_end(&mut bytes)?;
        if bytes.len() > 4_194_304 {
            bail!("Sandbox cloud response exceeded 4 MiB");
        }
        if bytes.is_empty() {
            if method == "GET" {
                bail!("Cloud read returned an empty response; absence was not verified");
            }
            return Ok(Value::Null);
        }
        let value: Value =
            serde_json::from_slice(&bytes).context("Malformed sandbox cloud response")?;
        if method == "GET" && !value.is_object() {
            bail!("Cloud read did not return an object");
        }
        Ok(value)
    }

    pub(super) fn validate_slot(&self, group: u64) -> Result<()> {
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
        if universe["group"].as_str() != Some(format!("groups/{group}").as_str())
            || universe["visibility"].as_str() != Some("PRIVATE")
            || universe["rootPlace"].as_str() != Some(expected_place.as_str())
        {
            bail!(
                "Sandbox must be the root place of a PRIVATE experience owned by the configured throwaway group"
            );
        }
        Ok(())
    }

    pub(super) fn publish_blank(&self, file: &Path) -> Result<()> {
        let url = format!(
            "https://apis.roblox.com/universes/v1/{}/places/{}/versions?versionType=Published",
            self.slot.universe_id, self.slot.place_id
        );
        let response = self
            .agent
            .post(&url)
            .timeout(Duration::from_secs(60))
            .set("x-api-key", &self.key)
            .set("Content-Type", "application/octet-stream")
            .send(File::open(file)?)
            .map_err(|_| {
                renium_plugin_sdk::anyhow!(
                    "Blank-place publishing failed or timed out; the slot remains reserved"
                )
            })?;
        let result: Value = response.into_json()?;
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
                        response["path"]
                            .as_str()
                            .context("Memory flush omitted its operation path")?
                            .to_owned(),
                    );
                    state.stage = Stage::WaitMemory;
                }
            }
            Stage::WaitMemory => {
                let response = self.request(
                    "GET",
                    state
                        .memory_operation
                        .as_deref()
                        .context("Missing memory operation")?,
                    &[],
                    false,
                )?;
                check_operation(&response)?;
                if response["done"].as_bool() == Some(true) {
                    state.stage = Stage::Done;
                } else {
                    bail!(
                        "MemoryStore flush is still pending; slot stays reserved. Resume release later, without busy polling."
                    );
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

fn check_operation(response: &Value) -> Result<()> {
    if response.get("error").is_some() {
        bail!("MemoryStore flush failed; the slot remains reserved");
    }
    if response["done"].as_bool() == Some(true) && response.get("response").is_none() {
        bail!("MemoryStore flush reported completion without a result");
    }
    Ok(())
}

#[cfg(test)]
#[path = "cloud_tests.rs"]
mod tests;
