//! Shared payloads held once.
//!
//! A store file holds the text of a SharedString once, however many of its
//! instances have it: the cooked collision data of a mesh stands in every
//! part of that mesh. A document holds its values by instance, so the same
//! text would stand in it thousands of times and take most of its memory.
//! The documents a pull or a push compares, plans from and writes hold such
//! a payload once for the process and carry a handle for it.
//!
//! Only a property's own value is ever a handle, never a value inside
//! another or an attribute. A handle never leaves the process: whatever
//! writes a value to a store file, to Studio, to a record or to an output
//! resolves it first. Its key is drawn anew by every process, so no file,
//! no user and no other process can state one.
//!
//! A payload is released once nothing has used it for a while. A handle
//! whose payload is gone resolves to nothing, and nothing writes it.

use std::ops::Deref;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, OnceLock, PoisonError, RwLock};
use std::time::Instant;

use ahash::AHashMap;
use serde_json::{Map, Value};

/// From this size on the text of a payload is held once. A test holds
/// nearly every payload, so that the ones other tests carry go through here.
const MIN_HELD_TEXT: usize = if cfg!(test) { 4 } else { 256 };

/// The type whose payloads are held: by its nature one value of many
/// instances. A payload of one instance, like a terrain grid that changes
/// with every edit, gains nothing here.
const HELD_TYPE: &str = "SharedString";

/// A payload nothing used for this long is released.
const RELEASED_AFTER_SECONDS: u64 = 30 * 60;

/// The least time between two looks for payloads to release.
const SWEEP_EVERY_SECONDS: u64 = 60;

pub(crate) struct Payload {
    value: Value,
    digest: OnceLock<[u8; 16]>,
    /// When it was last used, in seconds of the process, never zero.
    used: AtomicU64,
}

impl Payload {
    fn text(&self) -> &str {
        self.value["base64"].as_str().unwrap_or_default()
    }

    fn touch(&self) {
        self.used.store(now(), Ordering::Relaxed);
    }
}

#[derive(Default)]
struct Store {
    /// The handles of the payloads whose text has one hash.
    by_text: AHashMap<u64, Vec<u64>>,
    payloads: AHashMap<u64, Arc<Payload>>,
    /// A handle is never given twice.
    next: u64,
    swept: u64,
}

static STORE: LazyLock<RwLock<Store>> = LazyLock::new(Default::default);
static HASHER: LazyLock<ahash::RandomState> = LazyLock::new(ahash::RandomState::new);
static STARTED: LazyLock<Instant> = LazyLock::new(Instant::now);

/// The key of a handle, which only this process knows.
static KEY: LazyLock<String> = LazyLock::new(|| {
    format!(
        "held:{:016x}{:016x}",
        HASHER.hash_one(std::process::id()),
        ahash::RandomState::new().hash_one(STARTED.elapsed().as_nanos())
    )
});

/// The property of an instance that stands for its unchanged counterpart
/// (see `stand_unchanged`), which only this process knows.
static UNCHANGED: LazyLock<String> = LazyLock::new(|| format!("unchanged:{}", &KEY[5..]));

/// A push that changes a few instances of a large store reads the store
/// Studio holds only where it differs from the files: every other instance
/// of that document stands for the instance of the files at its place, which
/// it equals value for value, and holds none of its values. A comparison
/// takes such an instance for equal to its counterpart, and nothing writes
/// one.
pub(crate) fn stand_unchanged(properties: &mut Map<String, Value>) {
    properties.insert(UNCHANGED.clone(), Value::Bool(true));
}

pub(crate) fn stands_unchanged(properties: &Map<String, Value>) -> bool {
    properties.contains_key(UNCHANGED.as_str())
}

fn now() -> u64 {
    STARTED.elapsed().as_secs() + 1
}

/// The text of a value that is a shared payload and nothing more, of a size
/// that is held once.
fn payload_text(value: &Value) -> Option<&str> {
    let object = value.as_object()?;
    if object.len() != 2 || object.get("_type")?.as_str()? != HELD_TYPE {
        return None;
    }
    object
        .get("base64")?
        .as_str()
        .filter(|text| text.len() >= MIN_HELD_TEXT)
}

fn handle(id: u64) -> Value {
    Value::Object(Map::from_iter([
        ("_type".to_string(), Value::String(HELD_TYPE.to_string())),
        (KEY.clone(), Value::from(id)),
    ]))
}

impl Store {
    fn known(&self, hash: u64, text: &str) -> Option<u64> {
        self.by_text.get(&hash)?.iter().copied().find(|id| {
            self.payloads.get(id).is_some_and(|held| {
                let same = held.text() == text;
                if same {
                    held.touch();
                }
                same
            })
        })
    }

    /// Releases the payloads nothing used for a while, as of `now`.
    fn sweep(&mut self, now: u64) {
        if now < self.swept + SWEEP_EVERY_SECONDS {
            return;
        }
        self.swept = now;
        self.payloads
            .retain(|_, held| held.used.load(Ordering::Relaxed) + RELEASED_AFTER_SECONDS > now);
        let payloads = &self.payloads;
        self.by_text.retain(|_, ids| {
            ids.retain(|id| payloads.contains_key(id));
            !ids.is_empty()
        });
    }
}

fn known(text: &str) -> Option<u64> {
    STORE
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .known(HASHER.hash_one(text), text)
}

fn insert(value: Value) -> Value {
    let Some(text) = payload_text(&value) else {
        return value;
    };
    let hash = HASHER.hash_one(text);
    let mut store = STORE.write().unwrap_or_else(PoisonError::into_inner);
    if let Some(id) = store.known(hash, text) {
        return handle(id);
    }
    store.sweep(now());
    let id = store.next;
    store.next += 1;
    store.by_text.entry(hash).or_default().push(id);
    store.payloads.insert(
        id,
        Arc::new(Payload {
            value,
            digest: OnceLock::new(),
            used: AtomicU64::new(now()),
        }),
    );
    handle(id)
}

/// The value a document holds for a property's `value`: a handle where it is
/// a payload held once, the value itself anywhere else.
pub(crate) fn held(value: Value) -> Value {
    match payload_text(&value).and_then(known) {
        Some(id) => handle(id),
        None => insert(value),
    }
}

/// `held` of a value another holder keeps: its text is copied only the first
/// time the process sees it.
pub(crate) fn held_copy(value: &Value) -> Value {
    match payload_text(value) {
        Some(text) => match known(text) {
            Some(id) => handle(id),
            None => insert(value.clone()),
        },
        None => value.clone(),
    }
}

/// Whether an object is a handle, whether or not its payload is still held.
pub(crate) fn is_handle(object: &Map<String, Value>) -> bool {
    object.len() == 2
        && object.get("_type").and_then(Value::as_str) == Some(HELD_TYPE)
        && object.get(KEY.as_str()).is_some_and(Value::is_u64)
}

fn held_payload_of(object: &Map<String, Value>) -> Option<Arc<Payload>> {
    if !is_handle(object) {
        return None;
    }
    let id = object.get(KEY.as_str())?.as_u64()?;
    let held = STORE
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .payloads
        .get(&id)
        .cloned()?;
    held.touch();
    Some(held)
}

/// A value as it reads: the payload a handle stands for, kept while this is,
/// and any other value as it is. A handle whose payload is gone stays the
/// handle, which nothing writes.
pub(crate) enum Resolved<'a> {
    Plain(&'a Value),
    Held(Arc<Payload>),
}

impl Deref for Resolved<'_> {
    type Target = Value;

    fn deref(&self) -> &Value {
        match self {
            Self::Plain(value) => value,
            Self::Held(held) => &held.value,
        }
    }
}

impl Resolved<'_> {
    /// Whether both are handles of one payload.
    pub(crate) fn same_payload(&self, other: &Self) -> bool {
        matches!((self, other), (Self::Held(left), Self::Held(right)) if Arc::ptr_eq(left, right))
    }
}

pub(crate) fn resolve(value: &Value) -> Resolved<'_> {
    match value.as_object().and_then(held_payload_of) {
        Some(held) => Resolved::Held(held),
        None => Resolved::Plain(value),
    }
}

/// The payload an object stands for where it is a handle.
pub(crate) fn payload_object(object: &Map<String, Value>) -> Option<Resolved<'static>> {
    held_payload_of(object).map(Resolved::Held)
}

/// The digest `digest` gives the payload a handle stands for, taken once
/// while the payload is held. `None` for a value that is no such handle.
pub(crate) fn digest_once(
    value: &Value,
    digest: impl FnOnce(&Value) -> [u8; 16],
) -> Option<[u8; 16]> {
    let held = value.as_object().and_then(held_payload_of)?;
    Some(*held.digest.get_or_init(|| digest(&held.value)))
}

/// Whether two values a property holds are one value, a handle standing for
/// its payload.
pub(crate) fn same(left: &Value, right: &Value) -> bool {
    left == right || {
        let (left, right) = (resolve(left), resolve(right));
        left.same_payload(&right) || *left == *right
    }
}

/// Whether two instances hold the same properties, a handle standing for its
/// payload.
pub(crate) fn same_properties(left: &Map<String, Value>, right: &Map<String, Value>) -> bool {
    left == right
        || left.len() == right.len()
            && left
                .iter()
                .all(|(name, value)| right.get(name).is_some_and(|other| same(value, other)))
}

/// Every handle in `value`, at any depth, replaced by its payload.
pub(crate) fn resolve_all(value: &mut Value) {
    if let Some(held) = value.as_object().and_then(held_payload_of) {
        *value = held.value.clone();
        return;
    }
    match value {
        Value::Array(items) => items.iter_mut().for_each(resolve_all),
        Value::Object(object) => object.values_mut().for_each(resolve_all),
        _ => {}
    }
}

/// Whether `value` holds a handle at any depth.
pub(crate) fn holds_a_handle(value: &Value) -> bool {
    match value {
        Value::Array(items) => items.iter().any(holds_a_handle),
        Value::Object(object) => is_handle(object) || object.values().any(holds_a_handle),
        _ => false,
    }
}

/// A value as it is written out: every handle in it, at any depth, as its
/// payload, without a copy of the value. A handle whose payload is gone is
/// not written.
pub(crate) struct AsPayloads<'a>(pub(crate) &'a Value);

impl serde::Serialize for AsPayloads<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match &*resolve(self.0) {
            Value::Array(items) => serializer.collect_seq(items.iter().map(AsPayloads)),
            Value::Object(object) if is_handle(object) => Err(serde::ser::Error::custom(
                "a value is the handle of a payload that is no longer held",
            )),
            Value::Object(object) => {
                serializer.collect_map(object.iter().map(|(name, value)| (name, AsPayloads(value))))
            }
            scalar => scalar.serialize(serializer),
        }
    }
}

/// An instance's properties as they are written out: each handle as its
/// payload.
pub(crate) fn serialize_properties<S: serde::Serializer>(
    properties: &Map<String, Value>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    if stands_unchanged(properties) {
        return Err(serde::ser::Error::custom(
            "an instance that stands for its unchanged counterpart is not written",
        ));
    }
    serializer.collect_map(
        properties
            .iter()
            .map(|(name, value)| (name, AsPayloads(value))),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn payload_of(kind: &str, byte: u8, len: usize) -> Value {
        json!({ "_type": kind, "base64": String::from_utf8(vec![byte; len]).unwrap() })
    }

    fn is_a_handle(value: &Value) -> bool {
        value.as_object().is_some_and(is_handle)
    }

    #[test]
    fn a_shared_payload_of_some_size_is_held_once_and_resolves_to_itself() {
        let value = payload_of(HELD_TYPE, b'Q', MIN_HELD_TEXT);
        let (first, second) = (held(value.clone()), held_copy(&value));
        assert_eq!(first, second);
        assert!(is_a_handle(&first));
        assert!(resolve(&first).same_payload(&resolve(&second)));
        assert_eq!(*resolve(&first), value);
        let other = held(payload_of(HELD_TYPE, b'P', MIN_HELD_TEXT));
        assert_ne!(first, other);
        assert!(same(&first, &value) && !same(&first, &other));
        assert!(!resolve(&value).same_payload(&resolve(&value)));
    }

    #[test]
    fn what_is_no_shared_payload_of_that_size_stays_as_it_is() {
        for value in [
            payload_of(HELD_TYPE, b'Q', MIN_HELD_TEXT - 1),
            // A payload of one instance, like a terrain grid.
            payload_of("BinaryString", b'Q', MIN_HELD_TEXT * 8),
            json!({"_type": HELD_TYPE, "base64": "Q".repeat(MIN_HELD_TEXT), "more": 1}),
            json!({"base64": "Q".repeat(MIN_HELD_TEXT), "other": 1}),
            json!("Q".repeat(MIN_HELD_TEXT)),
            // What a file, a user or another process can state is no handle.
            json!({"_type": HELD_TYPE, "held": 0}),
        ] {
            assert_eq!(held(value.clone()), value);
            assert_eq!(held_copy(&value), value);
            assert!(!is_a_handle(&value) && !holds_a_handle(&value));
            assert!(matches!(resolve(&value), Resolved::Plain(_)));
        }
    }

    #[test]
    fn a_handle_whose_payload_is_gone_is_not_written() {
        let mut gone = held(payload_of(HELD_TYPE, b'G', MIN_HELD_TEXT + 1));
        gone[KEY.as_str()] = json!(u64::MAX);
        assert!(is_a_handle(&gone));
        assert!(matches!(resolve(&gone), Resolved::Plain(_)));
        assert!(serde_json::to_value(AsPayloads(&json!([1, gone.clone()]))).is_err());
        let properties = Map::from_iter([("Data".to_string(), gone.clone())]);
        let mut out = Vec::new();
        assert!(
            serialize_properties(&properties, &mut serde_json::Serializer::new(&mut out)).is_err()
        );
        let mut nested = json!({"list": [gone.clone()]});
        resolve_all(&mut nested);
        assert!(holds_a_handle(&nested));
    }

    #[test]
    fn a_payload_nothing_used_for_a_while_is_released() {
        let value = payload_of(HELD_TYPE, b'T', MIN_HELD_TEXT + 2);
        let handle = held(value.clone());
        let id = handle[KEY.as_str()].as_u64().unwrap();
        {
            let mut store = STORE.write().unwrap_or_else(PoisonError::into_inner);
            // Used before anything a running test holds, which is never zero.
            store.payloads[&id].used.store(0, Ordering::Relaxed);
            store.swept = 0;
            store.sweep(SWEEP_EVERY_SECONDS - 1);
            assert!(
                store.payloads.contains_key(&id),
                "too soon after the last look"
            );
            store.sweep(RELEASED_AFTER_SECONDS);
            assert!(!store.payloads.contains_key(&id));
            assert!(!store.by_text.values().flatten().any(|held| *held == id));
        }
        // The handle of a released payload resolves to nothing, and the
        // same payload is held anew under another handle.
        assert!(matches!(resolve(&handle), Resolved::Plain(_)));
        let again = held(value.clone());
        assert_ne!(again, handle);
        assert_eq!(*resolve(&again), value);
    }

    #[test]
    fn a_document_decoded_for_a_pull_or_a_push_holds_its_payloads_by_handle() {
        use crate::settings::bytecode::{
            SETTINGS_BINARY_VERSION, SettingsBytecode, SettingsBytecodeInstance,
            decode_settings_bytecode, decode_settings_bytecode_holding, encode_settings_bytecode,
        };
        let payload = payload_of(HELD_TYPE, b'S', MIN_HELD_TEXT * 3);
        let mut document = SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: vec![SettingsBytecodeInstance::new(
                "1".into(),
                "Workspace".into(),
                "Workspace".into(),
                None,
            )],
        };
        for index in 0..3 {
            let mut part = SettingsBytecodeInstance::new(
                format!("part-{index}"),
                "Part".into(),
                "MeshPart".into(),
                Some(0),
            );
            part.properties
                .insert("PhysicalConfigData".into(), payload.clone());
            part.properties
                .insert("Nested".into(), json!({"inner": payload.clone(), "n": 1}));
            document.instances.push(part);
        }
        let bytes = encode_settings_bytecode(&document).unwrap();
        let plain = decode_settings_bytecode(&bytes).unwrap();
        let held = decode_settings_bytecode_holding(&bytes).unwrap();
        let first = held.instances[1].properties["PhysicalConfigData"].clone();
        for (plain, held) in plain.instances.iter().zip(&held.instances).skip(1) {
            // Any other decode gives the payload itself.
            assert_eq!(plain.properties["PhysicalConfigData"], payload);
            let handle = &held.properties["PhysicalConfigData"];
            assert!(is_a_handle(handle));
            assert!(resolve(handle).same_payload(&resolve(&first)));
            // Only a property's own value is a handle.
            assert_eq!(held.properties["Nested"]["inner"], payload);
        }
        assert!(held == plain && held == document);
        assert!(encode_settings_bytecode(&held).unwrap() == bytes);
        assert_eq!(
            serde_json::to_value(&held).unwrap(),
            serde_json::to_value(&plain).unwrap()
        );
        // A handle no payload stands behind is never written to a store.
        let mut gone = held.clone();
        gone.instances[1].properties["PhysicalConfigData"][KEY.as_str()] = json!(u64::MAX);
        assert!(encode_settings_bytecode(&gone).is_err());
        assert!(serde_json::to_value(&gone).is_err());
    }

    #[test]
    fn every_handle_in_a_value_resolves_and_serializes_as_its_payload() {
        let value = payload_of(HELD_TYPE, b'R', MIN_HELD_TEXT + 3);
        let handle = held(value.clone());
        let mut nested = json!({"list": [handle.clone(), 1], "plain": {"held": 2}});
        assert!(holds_a_handle(&nested));
        assert_eq!(
            serde_json::to_value(AsPayloads(&nested)).unwrap(),
            json!({"list": [value.clone(), 1], "plain": {"held": 2}})
        );
        resolve_all(&mut nested);
        assert!(!holds_a_handle(&nested));
        assert_eq!(
            nested,
            json!({"list": [value.clone(), 1], "plain": {"held": 2}})
        );
        let properties = Map::from_iter([("Data".to_string(), handle.clone())]);
        let mut out = Vec::new();
        serialize_properties(&properties, &mut serde_json::Serializer::new(&mut out)).unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&out).unwrap(),
            json!({"Data": value})
        );
        assert!(same_properties(
            &properties,
            &Map::from_iter([("Data".to_string(), value.clone())])
        ));
        let mut calls = 0;
        for _ in 0..2 {
            assert_eq!(
                digest_once(&handle, |held| {
                    calls += 1;
                    assert_eq!(*held, value);
                    [7; 16]
                }),
                Some([7; 16])
            );
        }
        assert_eq!(calls, 1);
        assert_eq!(digest_once(&value, |_| [1; 16]), None);
    }
}
