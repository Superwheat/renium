use super::*;
use crate::OrderedStore;
use renium_plugin_sdk::json;
use std::sync::{Arc, Mutex};

type Scripted = (&'static str, String, u16, Value);

// Replays scripted Open Cloud replies in order; an unexpected request fails
// the test instead of reaching Renium or the network.
struct Fake {
    replies: Arc<Mutex<VecDeque<Scripted>>>,
}

impl Transport for Fake {
    fn send(&self, method: &str, path: &str, query: &[(&str, &str)]) -> Result<Reply> {
        let (expected_method, expected, status, body) = self
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .expect("Unexpected cloud request");
        assert_eq!(method, expected_method);
        let (expected_path, token) = expected.split_once('?').unwrap_or((&expected, ""));
        assert_eq!(path, expected_path);
        if method == "GET" && (path.ends_with("/entries") || path.ends_with("/data-stores")) {
            assert_eq!(query, [("maxPageSize", "100"), ("pageToken", token)]);
        }
        Ok(if (200..300).contains(&status) {
            Reply::Body(body)
        } else {
            Reply::Status(status)
        })
    }

    fn publish(&self, _: u64, _: u64, _: &Path) -> Result<Value> {
        panic!("Unexpected publish")
    }
}

fn client(slot: &Slot, replies: Vec<Scripted>) -> (Client<'_>, Arc<Mutex<VecDeque<Scripted>>>) {
    let replies = Arc::new(Mutex::new(VecDeque::from(replies)));
    let transport = Fake {
        replies: replies.clone(),
    };
    (Client::with_transport(Box::new(transport), slot), replies)
}

fn slot() -> Slot {
    Slot {
        name: "fixture".into(),
        universe_id: 12,
        place_id: 34,
        ordered_stores: vec![OrderedStore {
            name: "scores é".into(),
            scope: "season 1".into(),
        }],
    }
}

#[test]
fn validates_a_private_root_owned_by_the_configured_account_before_mutation() {
    let slot = slot();
    for (owner_field, owner) in [("group", "groups/56"), ("user", "users/78")] {
        let mut valid = json!({"visibility":"PRIVATE", "rootPlace":"universes/12/places/34"});
        valid[owner_field] = json!(owner);
        for (field, value) in [
            (
                owner_field,
                if owner_field == "group" {
                    "groups/57"
                } else {
                    "users/79"
                },
            ),
            ("visibility", "PUBLIC"),
            ("rootPlace", "universes/12/places/35"),
        ] {
            let mut invalid = valid.clone();
            invalid[field] = json!(value);
            let (checked, replies) =
                client(&slot, vec![("GET", "universes/12".into(), 200, invalid)]);
            assert!(checked.validate_slot(owner).is_err());
            assert!(replies.lock().unwrap().is_empty());
        }
        let (accepted, _) = client(
            &slot,
            vec![("GET", "universes/12".into(), 200, valid.clone())],
        );
        accepted.validate_slot(owner).unwrap();
        let other = if owner_field == "group" {
            "users/56"
        } else {
            "groups/78"
        };
        let (checked, _) = client(&slot, vec![("GET", "universes/12".into(), 200, valid)]);
        assert!(checked.validate_slot(other).is_err());
    }
}

#[test]
fn refused_renium_requests_keep_their_http_status() {
    assert_eq!(
        http_status(
            "Renium command failed (exit code: 1): {\"error\":\"Open Cloud request 0 returned HTTP 404\\n{}\",\"ok\":false}"
        ),
        Some(404)
    );
    assert_eq!(http_status("returned HTTP 429 later"), Some(429));
    assert_eq!(http_status("returned HTTP 200"), None);
    assert_eq!(http_status("connection reset"), None);
}

#[test]
fn cleanup_resumes_across_pages_throttling_deletes_and_async_flush() {
    let slot = slot();
    let stores = "universes/12/data-stores";
    let store = format!("{stores}/saves");
    let entries = format!("{store}/scopes/-/entries");
    let first = format!("{store}/scopes/global/entries/caf%C3%A9");
    let second = format!("{store}/scopes/season%202/entries/player%2Fone");
    let ordered = "universes/12/ordered-data-stores/scores%20%C3%A9/scopes/season%201/entries";
    let score = format!("{ordered}/alice");
    let operation = "universes/12/memory-store/operations/flush-1";
    let (client, replies) = client(
        &slot,
        vec![
            (
                "GET",
                stores.into(),
                200,
                json!({"nextPageToken":"stores-2"}),
            ),
            (
                "GET",
                format!("{stores}?stores-2"),
                200,
                json!({"dataStores":[{"path":store}]}),
            ),
            (
                "GET",
                entries.clone(),
                200,
                json!({"nextPageToken":"entries-2"}),
            ),
            (
                "GET",
                format!("{entries}?entries-2"),
                200,
                json!({"dataStoreEntries":[{"path":first},{"path":second}]}),
            ),
            ("DELETE", first.clone(), 204, Value::Null),
            ("GET", first, 200, json!({"state":"DELETED"})),
            ("DELETE", second.clone(), 429, json!({"error":"throttled"})),
            ("DELETE", second.clone(), 204, Value::Null),
            ("GET", second, 404, Value::Null),
            (
                "GET",
                ordered.into(),
                200,
                json!({"orderedDataStoreEntries":[{"path":score}]}),
            ),
            ("DELETE", score.clone(), 204, Value::Null),
            ("GET", score, 404, Value::Null),
            (
                "GET",
                stores.into(),
                200,
                json!({"dataStores":[{"path":store}]}),
            ),
            ("GET", entries, 200, json!({})),
            ("GET", ordered.into(), 200, json!({})),
            (
                "POST",
                "universes/12/memory-store:flush".into(),
                200,
                json!({"path":format!("cloud/v2/{operation}"),"done":false}),
            ),
            (
                "GET",
                operation.into(),
                200,
                json!({"path":operation,"done":false}),
            ),
            (
                "GET",
                operation.into(),
                200,
                json!({"path":format!("cloud/v2/{operation}"),"done":true}),
            ),
        ],
    );
    let mut state = Cleanup::default();
    let mut failures = Vec::new();
    let mut complete = false;
    for _ in 0..40 {
        match client.cleanup_step(&mut state) {
            Ok(true) => {
                complete = true;
                break;
            }
            Ok(false) => {}
            Err(error) => failures.push(error.to_string()),
        }
        // Each step can be resumed by a new process from its durable journal.
        state = serde_json::from_value(serde_json::to_value(&state).unwrap()).unwrap();
    }
    assert!(complete);
    assert_eq!(state.deleted, 3);
    assert!(state.pending.is_empty());
    assert!(replies.lock().unwrap().is_empty());
    // Throttling is an error to resume from; a flush still finishing is progress.
    assert_eq!(failures.len(), 1);
    assert!(failures[0].contains("429"));
}

#[test]
fn malformed_or_cross_universe_resources_never_reach_transport() {
    let slot = slot();
    let (client, _) = client(&slot, vec![]);
    for path in [
        "universes/13/data-stores",
        "universes/12/../13/data-stores",
        "universes/12/data-stores?escape=1",
    ] {
        assert!(client.request("DELETE", path, &[], false).is_err());
    }
}

#[test]
fn verification_rejects_remaining_data_and_incomplete_flush() {
    let slot = slot();
    let path = "universes/12/data-stores/saves/scopes/-/entries";
    let (client, _) = client(
        &slot,
        vec![(
            "GET",
            path.into(),
            200,
            json!({"dataStoreEntries":[{"path":"remaining"}]}),
        )],
    );
    let mut state = Cleanup::default();
    assert!(
        client
            .entries(&mut state, path, "dataStoreEntries", true)
            .unwrap_err()
            .to_string()
            .contains("reappeared")
    );
    assert!(
        check_operation(
            &json!({"done":true,"path":"cloud/v2/universes/12/memory-store/operations/x"})
        )
        .is_ok()
    );
    assert!(check_operation(&json!({"done":"yes"})).is_err());
    assert!(check_operation(&json!({"done":true,"error":{"code":500}})).is_err());
}

#[test]
fn lost_delete_confirmation_keeps_the_entry_reserved_until_verified() {
    let slot = slot();
    let path = "universes/12/data-stores/saves/scopes/global/entries/key";
    let (client, replies) = client(
        &slot,
        vec![
            ("DELETE", path.into(), 204, Value::Null),
            ("GET", path.into(), 503, Value::Null),
            ("DELETE", path.into(), 404, Value::Null),
            ("GET", path.into(), 404, Value::Null),
        ],
    );
    let mut state = Cleanup {
        pending: VecDeque::from([path.to_owned()]),
        ..Cleanup::default()
    };
    assert!(
        client
            .entries(&mut state, "unused", "dataStoreEntries", false)
            .is_err()
    );
    assert_eq!(state.pending.front().unwrap(), path);
    assert_eq!(state.deleted, 0);
    state = serde_json::from_value(serde_json::to_value(state).unwrap()).unwrap();
    client
        .entries(&mut state, "unused", "dataStoreEntries", false)
        .unwrap();
    assert_eq!(state.deleted, 1);
    assert!(state.pending.is_empty() && replies.lock().unwrap().is_empty());
}
