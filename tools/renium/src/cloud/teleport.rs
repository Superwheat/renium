//! Resolve access and the newest saved version on every travel, including cache hits.
use std::io::Read;
use std::path::Path;

use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};

use super::discovery::{PlaceVersion, place_history};
use super::transport::{CloudIdentity, execute_one};

pub(crate) const MAX_BYTES: u64 = 128 * 1024 * 1024;
const UNIVERSE_LIMIT: &str =
    "Playtest teleport currently supports destinations in the source universe";

#[derive(Clone, Copy, Debug)]
pub(crate) struct SavedVersion {
    pub universe: i64,
    pub place: i64,
    pub version: u64,
}

pub(crate) struct Destination {
    pub saved: SavedVersion,
    pub display_name: String,
    location: String,
}

pub(crate) fn latest_saved(versions: &[PlaceVersion]) -> Result<u64> {
    versions
        .iter()
        .map(|version| version.number)
        .max()
        .filter(|version| *version > 0)
        .context("Teleport requires readable saved-version history; no delivery-copy fallback")
}

fn name_with(universe: i64, place: i64, send: impl Fn(Value) -> Result<Value>) -> Result<String> {
    let metadata = send(json!({
        "method": "GET", "path": "/cloud/v2/universes/{universe}/places/{place}",
        "pathParams": { "universe": universe, "place": place },
    })).with_context(|| format!("Cannot read place {place} in source universe {universe}. {UNIVERSE_LIMIT}; same-universe destinations require the stored key's place-read permission"))?;
    ensure!(
        metadata["status"] == 200,
        "Cannot read place {place} in source universe {universe}. {UNIVERSE_LIMIT}; check the stored key's place-read permission for same-universe destinations"
    );
    ensure!(
        metadata["body"]["path"] == format!("universes/{universe}/places/{place}"),
        "{UNIVERSE_LIMIT}; destination membership did not match"
    );
    let display_name = metadata["body"]["displayName"]
        .as_str()
        .filter(|name| {
            !name.trim().is_empty() && name.len() <= 1024 && !name.chars().any(char::is_control)
        })
        .context("Destination metadata returned no valid displayName for the Studio title")?
        .to_owned();
    Ok(display_name)
}

fn resolve_with(
    universe: i64,
    place: i64,
    send: impl Fn(Value) -> Result<Value>,
) -> Result<Destination> {
    let display_name = name_with(universe, place, &send)?;
    let history = place_history(
        |request| {
            send(request)
                .ok()
                .filter(|response| response["status"] == 200)
        },
        place,
        |rows| !rows.is_empty(),
    )
    .context(
        "Cannot read destination saved versions; check the stored key's version-history permission",
    )?;
    let version = latest_saved(&history)?;
    // Reauthorize delivery even on a cache hit: removing the key's read access
    // must not make previously downloaded bytes a permission bypass.
    let delivery = send(json!({
        "method":"GET", "path":"/asset-delivery-api/v1/assetId/{assetId}/version/{versionNumber}",
        "pathParams":{"assetId":place,"versionNumber":version},
    }))
    .context("Cannot read the destination saved version with the stored Open Cloud key")?;
    ensure!(
        delivery["status"]
            .as_u64()
            .is_some_and(|status| (200..300).contains(&status)),
        "Destination saved-version delivery was denied; check the stored Open Cloud key's read permission"
    );
    let location = delivery
        .pointer("/body/location")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .context("Saved-version delivery returned no download location")?
        .to_owned();
    Ok(Destination {
        display_name,
        saved: SavedVersion {
            universe,
            place,
            version,
        },
        location,
    })
}

pub(crate) fn resolve(universe: i64, place: i64) -> Result<Destination> {
    let identity = CloudIdentity {
        game_id: Some(universe),
        place_id: Some(place),
    };
    resolve_with(universe, place, |request| {
        execute_one(identity, "ROBLOX_API_KEY", None, false, request)
            .map_err(super::command::cloud_error)
    })
}

pub(crate) fn display_name(universe: i64, place: i64) -> Result<String> {
    let identity = CloudIdentity {
        game_id: Some(universe),
        place_id: Some(place),
    };
    name_with(universe, place, |request| {
        execute_one(identity, "ROBLOX_API_KEY", None, false, request)
            .map_err(super::command::cloud_error)
    })
}

fn copy_bounded(reader: impl Read, output: &Path, limit: u64) -> Result<()> {
    let mut file = std::fs::File::create(output)?;
    let bytes = std::io::copy(&mut reader.take(limit + 1), &mut file)?;
    ensure!(bytes <= limit, "Teleport place exceeds 128 MiB");
    Ok(())
}

impl Destination {
    pub(crate) fn download(&self, output: &Path) -> Result<()> {
        let response = super::transport::agent()
            .get(&self.location)
            .call()
            // Delivery URLs may be signed; never put one in a loading screen or log.
            .map_err(|_| {
                anyhow::anyhow!("Saved-version download failed; retry or check Open Cloud access")
            })?;
        copy_bounded(response.into_reader(), output, MAX_BYTES)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    #[test]
    fn teleport_download_chooses_saved_over_published_and_refuses_unknown() {
        assert_eq!(
            latest_saved(&[
                PlaceVersion {
                    number: 9,
                    published: true
                },
                PlaceVersion {
                    number: 10,
                    published: false
                },
            ])
            .unwrap(),
            10
        );
        assert!(latest_saved(&[]).is_err());
        assert!(
            latest_saved(&[PlaceVersion {
                number: 0,
                published: false
            }])
            .is_err()
        );
    }

    #[test]
    fn teleport_resolves_any_readable_same_universe_place_and_pins_delivery() {
        let requests = RefCell::new(Vec::new());
        let destination = resolve_with(7, 99, |request| {
            requests.borrow_mut().push(request.clone());
            Ok(match request["path"].as_str().unwrap() {
                "/cloud/v2/universes/{universe}/places/{place}" => json!({"status":200,"body":{"path":"universes/7/places/99","displayName":"99"}}),
                "/place-version-history-api/v1/{place}/history" => json!({"status":200,"body":{"placeVersions":[{"version":10,"isPublished":false},{"version":9,"isPublished":true}]}}),
                "/asset-delivery-api/v1/assetId/{assetId}/version/{versionNumber}" => json!({"status":200,"body":{"location":"unused-offline"}}),
                _ => panic!("Unexpected request: {request}"),
            })
        }).unwrap();
        assert_eq!(destination.saved.version, 10);
        assert_eq!(destination.display_name, "99");
        assert_eq!(
            requests.borrow().last().unwrap()["pathParams"]["versionNumber"],
            10
        );
    }

    #[test]
    fn teleport_refuses_denied_metadata_and_wrong_universe_with_visible_reason() {
        for metadata in [
            json!({"status":403}),
            json!({"status":200,"body":{"path":"universes/8/places/99"}}),
        ] {
            let error = resolve_with(7, 99, |_| Ok(metadata.clone())).err().unwrap();
            assert!(error.to_string().contains(UNIVERSE_LIMIT));
        }
    }

    #[test]
    fn teleport_title_requires_authenticated_valid_metadata() {
        for name in [
            Value::Null,
            json!(""),
            json!("\n"),
            json!("x\0y"),
            json!("x".repeat(1025)),
        ] {
            let error = resolve_with(7, 99, |_| {
                Ok(json!({"status":200,"body":{
                    "path":"universes/7/places/99","displayName":name
                }}))
            })
            .err()
            .unwrap();
            assert!(error.to_string().contains("displayName"));
        }
    }

    #[test]
    fn teleport_rechecks_history_and_delivery_access_before_cached_content() {
        for denied in [1, 2] {
            let requests = RefCell::new(0);
            let result = resolve_with(7, 99, |_| {
                let index = *requests.borrow();
                *requests.borrow_mut() += 1;
                let mut response = match index {
                    0 => {
                        json!({"status":200,"body":{"path":"universes/7/places/99","displayName":"99"}})
                    }
                    1 => json!({"status":200,"body":{"placeVersions":[{"version":10}]}}),
                    2 => json!({"status":200,"body":{"location":"unused-offline"}}),
                    _ => panic!("Unexpected fallback request"),
                };
                if index == denied {
                    response["status"] = json!(403);
                }
                Ok(response)
            });
            assert!(result.is_err());
            assert_eq!(*requests.borrow(), denied + 1);
        }
    }

    #[test]
    fn teleport_download_stops_at_the_size_limit() {
        let root = crate::tests::support::temp_dir("teleport-download");
        let _cleanup = crate::system::files::OnDrop::new(|| {
            let _ = std::fs::remove_dir_all(&root);
        });
        let file = root.join("place.rbxl");
        assert!(copy_bounded(&b"12345"[..], &file, 4).is_err());
        assert_eq!(file.metadata().unwrap().len(), 5);
        copy_bounded(&b"1234"[..], &file, 4).unwrap();
    }
}
