//! Patch a serialized property name without regenerating Studio defaults.
//! cargo run --example patch-reflection-alias -- DATABASE.msgpack CLASS LOGICAL SAVED
use anyhow::{Context, Result, ensure};
use rbx_reflection::{PropertyKind, PropertySerialization, ReflectionDatabase, Scriptability};
use std::{env, fs};

fn main() -> Result<()> {
    let args = env::args().skip(1).collect::<Vec<_>>();
    ensure!(
        args.len() == 4,
        "Expected DATABASE.msgpack CLASS LOGICAL SAVED"
    );
    let bytes = fs::read(&args[0])?;
    let mut database: ReflectionDatabase<'_> = rmp_serde::from_slice(&bytes)?;
    let (logical, saved) = (args[2].as_str(), args[3].as_str());
    let class = database
        .classes
        .get_mut(args[1].as_str())
        .context("Unknown class")?;
    if let Some(existing) = class.properties.get(saved) {
        ensure!(
            matches!(existing.kind, PropertyKind::Alias { alias_for } if alias_for == logical),
            "Saved name already belongs to another property"
        );
    }
    let property = class
        .properties
        .get_mut(logical)
        .context("Unknown logical property")?;
    ensure!(
        matches!(
            property.kind,
            PropertyKind::Canonical {
                serialization: PropertySerialization::Serializes
                    | PropertySerialization::SerializesAs(_)
            }
        ),
        "Expected a serializing canonical property"
    );
    property.kind = PropertyKind::Canonical {
        serialization: PropertySerialization::SerializesAs(saved),
    };
    let mut alias = property.clone();
    alias.name = saved;
    alias.scriptability = Scriptability::None;
    alias.kind = PropertyKind::Alias { alias_for: logical };
    class.properties.insert(saved, alias);
    let packed = rmp_serde::to_vec(&database)?;
    let decoded: ReflectionDatabase<'_> = rmp_serde::from_slice(&packed)?;
    ensure!(
        serde_json::to_value(&database)? == serde_json::to_value(&decoded)?,
        "Reflection conversion lost data"
    );
    fs::write(&args[0], packed)?;
    Ok(())
}
