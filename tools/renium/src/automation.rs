use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub const PROTOCOL_VERSION: u8 = 1;
pub const REGISTRY_JSON: &str = include_str!("../protocol/opcodes.json");
static REGISTRY: OnceLock<std::result::Result<Vec<Opcode>, String>> = OnceLock::new();

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Opcode {
    pub id: u16,
    pub name: String,
    #[serde(default)]
    pub aliases: Vec<String>,
    #[serde(default)]
    pub review: bool,
}

#[derive(Debug, Deserialize)]
struct Registry {
    version: u8,
    operations: Vec<Opcode>,
}

fn parse_registry() -> Result<Vec<Opcode>> {
    let registry: Registry =
        serde_json::from_str(REGISTRY_JSON).context("Invalid opcode registry")?;
    if registry.version != PROTOCOL_VERSION {
        bail!(
            "Opcode registry version {} does not match protocol version {PROTOCOL_VERSION}",
            registry.version
        );
    }
    let mut ids = HashSet::new();
    let mut names = HashSet::new();
    for operation in &registry.operations {
        if !ids.insert(operation.id) {
            bail!("Duplicate opcode {}", operation.id);
        }
        for name in std::iter::once(&operation.name).chain(&operation.aliases) {
            if !names.insert(name.clone()) {
                bail!("Duplicate operation name {name}");
            }
        }
    }
    Ok(registry.operations)
}

pub fn registry() -> Result<&'static [Opcode]> {
    match REGISTRY.get_or_init(|| parse_registry().map_err(|error| format!("{error:#}"))) {
        Ok(operations) => Ok(operations),
        Err(error) => bail!("{error}"),
    }
}

pub fn opcode_by_id(id: u16) -> Result<&'static Opcode> {
    registry()?
        .iter()
        .find(|operation| operation.id == id)
        .with_context(|| format!("Unknown opcode {id}"))
}

pub fn opcode_by_name(name: &str) -> Result<&'static Opcode> {
    registry()?
        .iter()
        .find(|operation| {
            operation.name == name || operation.aliases.iter().any(|alias| alias == name)
        })
        .with_context(|| format!("Unknown operation {name}"))
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub v: u8,
    pub id: u64,
    pub op: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cx: Option<u64>,
    #[serde(alias = "requestArgs")]
    pub p: Value,
}

impl Request {
    pub fn validate(&self) -> std::result::Result<&'static Opcode, Failure> {
        if self.v != PROTOCOL_VERSION {
            return Err(Failure::new(
                "bad_req",
                format!("Expected protocol version {PROTOCOL_VERSION}"),
                false,
                "cap",
            ));
        }
        if !self.p.is_object() {
            return Err(Failure::new(
                "bad_req",
                "p must be an object",
                false,
                "context",
            ));
        }
        let operation = opcode_by_id(self.op).map_err(|_| {
            Failure::new(
                "bad_op",
                format!("Unknown opcode {}", self.op),
                false,
                "cap",
            )
        })?;
        if self.op != 0 && self.op != 1 && self.cx.is_none() {
            return Err(Failure::new("bad_req", "cx is required", false, "bind"));
        }
        Ok(operation)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProtocolError {
    pub c: String,
    pub m: String,
    pub rt: u8,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub n: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub d: Option<Value>,
}

#[derive(Debug, Clone)]
pub struct Failure(pub ProtocolError);

impl Failure {
    pub fn new(
        code: impl Into<String>,
        message: impl Into<String>,
        retry: bool,
        next: impl Into<String>,
    ) -> Self {
        Self(ProtocolError {
            c: code.into(),
            m: message.into(),
            rt: u8::from(retry),
            n: Some(next.into()),
            d: None,
        })
    }

    pub fn detail(mut self, detail: Value) -> Self {
        self.0.d = Some(detail);
        self
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Response {
    pub v: u8,
    pub id: u64,
    pub ok: u8,
    pub ms: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub r: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub e: Option<ProtocolError>,
}

impl Response {
    pub fn success(id: u64, started: Instant, result: Value) -> Self {
        Self {
            v: PROTOCOL_VERSION,
            id,
            ok: 1,
            ms: started.elapsed().as_secs_f64() * 1000.0,
            r: Some(result),
            e: None,
        }
    }

    pub fn failure(id: u64, started: Instant, failure: Failure) -> Self {
        Self {
            v: PROTOCOL_VERSION,
            id,
            ok: 0,
            ms: started.elapsed().as_secs_f64() * 1000.0,
            r: None,
            e: Some(failure.0),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BoundContext {
    pub id: u64,
    pub initialized: bool,
    pub project: String,
    pub root: String,
    pub experience: String,
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub place_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub game_id: Option<i64>,
    pub selector: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runtime_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plugin_build: Option<i64>,
    pub fingerprint: String,
}

#[derive(Debug, Clone)]
pub struct Review {
    pub context_id: u64,
    pub runtime_id: Option<String>,
    pub operation: u16,
    pub parameters: Value,
    pub created: Instant,
}

pub struct State {
    next_context: AtomicU64,
    next_review: AtomicU64,
    contexts: Mutex<HashMap<u64, BoundContext>>,
    reviews: Mutex<HashMap<String, Review>>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            next_context: AtomicU64::new(1),
            next_review: AtomicU64::new(1),
            contexts: Mutex::new(HashMap::new()),
            reviews: Mutex::new(HashMap::new()),
        }
    }
}

impl State {
    pub fn insert_context(&self, mut context: BoundContext) -> BoundContext {
        context.id = self.next_context.fetch_add(1, Ordering::Relaxed);
        self.contexts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(context.id, context.clone());
        context
    }

    pub fn context(&self, id: u64) -> Option<BoundContext> {
        self.contexts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&id)
            .cloned()
    }

    pub fn remove_context(&self, id: u64) -> bool {
        self.contexts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&id)
            .is_some()
    }

    pub fn prepare_review(
        &self,
        context: &BoundContext,
        operation: u16,
        parameters: Value,
    ) -> String {
        let sequence = self.next_review.fetch_add(1, Ordering::Relaxed);
        let id = format!("r{sequence:x}");
        let review = Review {
            context_id: context.id,
            runtime_id: context.runtime_id.clone(),
            operation,
            parameters,
            created: Instant::now(),
        };
        self.reviews
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(id.clone(), review);
        id
    }

    pub fn take_review(&self, id: &str) -> Option<Review> {
        let review = self
            .reviews
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(id)?;
        (review.created.elapsed() <= Duration::from_secs(300)).then_some(review)
    }

    pub fn reject_review(&self, id: &str) -> bool {
        self.reviews
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(id)
            .is_some()
    }
}

pub fn capabilities() -> Result<Value> {
    Ok(json!({
        "v": PROTOCOL_VERSION,
        "ops": registry()?.iter().map(|operation| json!({
            "id": operation.id,
            "name": operation.name,
            "aliases": operation.aliases,
            "review": operation.review,
        })).collect::<Vec<_>>()
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_is_unique_and_complete() {
        let operations = registry().unwrap();
        assert_eq!(operations.len(), 54);
        assert_eq!(opcode_by_name("bb").unwrap().id, 23);
        assert!(opcode_by_id(31).unwrap().review);
        assert_eq!(opcode_by_id(82).unwrap().name, "review-reject");
    }

    #[test]
    fn compact_response_shape_has_no_legacy_fields() {
        let response = Response::success(7, Instant::now(), json!({}));
        let value = serde_json::to_value(response).unwrap();
        assert_eq!(value["v"], 1);
        assert_eq!(value["ok"], 1);
        assert!(value.get("command").is_none());
        assert!(value.get("elapsedMs").is_none());
    }

    #[test]
    fn requests_require_a_context_and_reject_unknown_fields() {
        let request = Request {
            v: 1,
            id: 1,
            op: 20,
            cx: None,
            p: json!({}),
        };
        assert_eq!(request.validate().unwrap_err().0.c, "bad_req");
        assert!(
            serde_json::from_str::<Request>(r#"{"v":1,"id":1,"op":0,"p":{},"command":"find"}"#)
                .is_err()
        );
    }

    #[test]
    fn review_receipts_are_context_bound_and_single_use() {
        let state = State::default();
        let context = state.insert_context(BoundContext {
            id: 0,
            initialized: true,
            project: "project".to_string(),
            root: "root".to_string(),
            experience: "experience".to_string(),
            source: "source".to_string(),
            place_id: Some(1),
            game_id: Some(2),
            selector: "2:1".to_string(),
            runtime_id: Some("runtime".to_string()),
            plugin_build: Some(3),
            fingerprint: "fingerprint".to_string(),
        });
        let id = state.prepare_review(&context, 11, json!({ "destructive": true }));
        let review = state.take_review(&id).unwrap();
        assert_eq!(review.context_id, context.id);
        assert_eq!(review.runtime_id, context.runtime_id);
        assert_eq!(review.operation, 11);
        assert!(state.take_review(&id).is_none());
    }
}
