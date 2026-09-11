use super::*;
use crate::OrderedStore;
use renium_plugin_sdk::json;
use std::sync::{Arc, Mutex};

type Reply = (&'static str, String, u16, Value);

// Exercise the real HTTP adapter and resumable cleanup without cloud credentials.
// Every request is intercepted; an unexpected request fails instead of using the network.
#[allow(clippy::result_large_err)] // ureq's middleware trait fixes the error type.
fn client(slot: &Slot, replies: Vec<Reply>) -> (Client<'_>, Arc<Mutex<VecDeque<Reply>>>) {
    let replies = Arc::new(Mutex::new(VecDeque::from(replies)));
    let pending = replies.clone();
    let agent = ureq::AgentBuilder::new()
        .middleware(move |request: ureq::Request, _: ureq::MiddlewareNext<'_>| {
            let (method, path, status, body) = pending
                .lock()
                .unwrap()
                .pop_front()
                .expect("Unexpected cloud request");
            assert_eq!(request.method(), method);
            let url = url::Url::parse(request.url()).unwrap();
            let (path, token) = path.split_once('?').unwrap_or((&path, ""));
            assert_eq!(url.host_str(), Some("apis.roblox.com"));
            assert_eq!(url.path(), format!("/cloud/v2/{path}"));
            if method == "GET" && (path.ends_with("/entries") || path.ends_with("/data-stores")) {
                assert_eq!(
                    url.query_pairs().collect::<Vec<_>>(),
                    [
                        ("maxPageSize".into(), "100".into()),
                        ("pageToken".into(), token.into())
                    ]
                );
            }
            assert_eq!(request.header("x-api-key"), Some("fixture"));
            let body = if body.is_null() {
                String::new()
            } else {
                body.to_string()
            };
            let response = ureq::Response::new(status, "Fixture", &body)?;
            if status >= 400 {
                Err(ureq::Error::Status(status, response))
            } else {
                Ok(response)
            }
        })
        .build();
    (
        Client {
            agent,
            key: "fixture".into(),
            slot,
        },
        replies,
    )
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
fn validates_private_group_owned_root_before_mutation() {
    let slot = slot();
    let valid =
        json!({"group":"groups/56", "visibility":"PRIVATE", "rootPlace":"universes/12/places/34"});
    for (field, value) in [
        ("group", "groups/57"),
        ("visibility", "PUBLIC"),
        ("rootPlace", "universes/12/places/35"),
    ] {
        let mut invalid = valid.clone();
        invalid[field] = json!(value);
        let (client, replies) = client(&slot, vec![("GET", "universes/12".into(), 200, invalid)]);
        assert!(client.validate_slot(56).is_err());
        assert!(replies.lock().unwrap().is_empty());
    }
    let (client, _) = client(&slot, vec![("GET", "universes/12".into(), 200, valid)]);
    client.validate_slot(56).unwrap();
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
                json!({"path":operation,"done":false}),
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
                json!({"path":operation,"done":true,"response":{}}),
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
    assert_eq!(failures.len(), 2);
    assert!(failures[0].contains("429"));
    assert!(failures[1].contains("still pending"));
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
    assert!(check_operation(&json!({"done":true})).is_err());
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
