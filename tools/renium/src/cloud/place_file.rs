//! Local checks for bytes sent to the Open Cloud place-version endpoint.
//! Parsing is not an engine-load test; keep that limitation in publishing docs.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::Read;
use std::path::Path;

use anyhow::{Context, Result, ensure};

use crate::rbx::model::RbxPlaceFormat;

pub(crate) const MAX_PLACE_BYTES: u64 = 100 * 1024 * 1024;

pub(crate) fn validate_size(bytes: u64) -> Result<()> {
    ensure!(
        bytes > 0 && bytes <= MAX_PLACE_BYTES,
        "Place file must be nonempty and no larger than 100 MiB"
    );
    Ok(())
}

pub(crate) fn content_type(format: RbxPlaceFormat) -> &'static str {
    match format {
        RbxPlaceFormat::Binary => "application/octet-stream",
        RbxPlaceFormat::Xml => "application/xml",
    }
}

/// The upload must send this same buffer, never reopen the file after checking it.
pub(crate) fn read_validated(path: &Path) -> Result<(RbxPlaceFormat, Vec<u8>)> {
    let file = File::open(path).with_context(|| format!("Could not read {}", path.display()))?;
    let metadata = file.metadata()?;
    ensure!(metadata.is_file(), "{} is not a file", path.display());
    validate_size(metadata.len())?;
    let mut bytes = Vec::new();
    // Bound the read too: a file can grow after metadata was obtained.
    file.take(MAX_PLACE_BYTES + 1).read_to_end(&mut bytes)?;
    let format = validate(&bytes)?;
    Ok((format, bytes))
}

pub(crate) fn validate_file(path: &Path) -> Result<u64> {
    let (_, bytes) = read_validated(path)?;
    Ok(bytes.len() as u64)
}

pub(crate) fn validate(bytes: &[u8]) -> Result<RbxPlaceFormat> {
    validate_size(bytes.len() as u64)?;
    let format = RbxPlaceFormat::sniff(bytes).context("Not a Roblox binary or XML place file")?;
    let dom = match format {
        RbxPlaceFormat::Binary => rbx_binary::Deserializer::new()
            .strict_float_types(true)
            .deserialize(bytes)
            .context("Invalid binary place file")?,
        RbxPlaceFormat::Xml => rbx_xml::from_reader(
            bytes,
            rbx_xml::DecodeOptions::new()
                .property_behavior(rbx_xml::DecodePropertyBehavior::ReadUnknown),
        )
        .context("Invalid XML place file")?,
    };
    let database = rbx_reflection_database::get()?;
    let mut unsupported = BTreeMap::<String, usize>::new();
    for instance in dom.descendants() {
        let mut class = instance.class.as_str();
        loop {
            if matches!(
                class,
                "EditableImage"
                    | "EditableMesh"
                    | "PartOperation"
                    | "SurfaceAppearance"
                    | "BaseWrap"
            ) {
                *unsupported.entry(instance.class.to_string()).or_default() += 1;
                break;
            }
            let Some(parent) = database
                .classes
                .get(class)
                .and_then(|descriptor| descriptor.superclass)
            else {
                break;
            };
            class = parent;
        }
    }
    ensure!(
        unsupported.is_empty(),
        "Open Cloud cannot reliably update these instances: {}. Publish from Studio with rbx publish instead",
        unsupported
            .iter()
            .map(|(class, count)| format!("{class} ({count})"))
            .collect::<Vec<_>>()
            .join(", ")
    );
    Ok(format)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rbx_dom_weak::{InstanceBuilder, WeakDom};

    #[test]
    fn valid_binary_and_xml_keep_native_numeric_widths() {
        let mut dom = WeakDom::new(InstanceBuilder::new("DataModel"));
        dom.insert(
            dom.root_ref(),
            InstanceBuilder::new("Camera")
                .with_property("OrthographicSize", 300.0f32)
                .with_property("ViewStretch", 1.0f32),
        );
        dom.insert(
            dom.root_ref(),
            InstanceBuilder::new("NumberValue").with_property("Value", 1.234567890123f64),
        );
        let mut binary = Vec::new();
        rbx_binary::to_writer(&mut binary, &dom, dom.root().children()).unwrap();
        assert_eq!(validate(&binary).unwrap(), RbxPlaceFormat::Binary);
        assert!(validate(&binary[..binary.len() - 1]).is_err());
        let mut xml = Vec::new();
        rbx_xml::to_writer(
            &mut xml,
            &dom,
            dom.root().children(),
            rbx_xml::EncodeOptions::new(),
        )
        .unwrap();
        assert_eq!(validate(&xml).unwrap(), RbxPlaceFormat::Xml);
        assert!(validate(b"<roblox version=\"4\"><Item").is_err());
        assert!(validate(b"not a place").is_err());
        assert!(validate(b"").is_err());
        assert!(validate_size(MAX_PLACE_BYTES).is_ok());
        assert!(validate_size(MAX_PLACE_BYTES + 1).is_err());
    }

    #[test]
    fn legacy_camera_doubles_are_rejected_before_normalization() {
        for property in ["OrthographicSize", "ViewStretch"] {
            let dom = WeakDom::new(
                InstanceBuilder::new("DataModel")
                    .with_child(InstanceBuilder::new("Camera").with_property(property, 300.0f64)),
            );
            let old_database = rbx_reflection::ReflectionDatabase::new();
            let mut bytes = Vec::new();
            rbx_binary::Serializer::new()
                .reflection_database(&old_database)
                .serialize(&mut bytes, &dom, dom.root().children())
                .unwrap();
            assert!(rbx_binary::from_reader(bytes.as_slice()).is_ok());
            let error = format!("{:#}", validate(&bytes).unwrap_err());
            assert!(error.contains(&format!("Camera.{property}")), "{error}");
            assert!(
                error.contains("should be Float32, but it was Float64"),
                "{error}"
            );
        }
    }

    #[test]
    fn unsupported_types_and_subclasses_are_rejected_in_both_formats() {
        for class in [
            "EditableImage",
            "EditableMesh",
            "PartOperation",
            "UnionOperation",
            "SurfaceAppearance",
            "BaseWrap",
            "WrapLayer",
            "WrapTarget",
        ] {
            let dom = WeakDom::new(
                InstanceBuilder::new("DataModel").with_child(InstanceBuilder::new(class)),
            );
            let mut binary = Vec::new();
            rbx_binary::to_writer(&mut binary, &dom, dom.root().children()).unwrap();
            let mut xml = Vec::new();
            rbx_xml::to_writer(
                &mut xml,
                &dom,
                dom.root().children(),
                rbx_xml::EncodeOptions::new(),
            )
            .unwrap();
            for bytes in [binary, xml] {
                let error = format!("{:#}", validate(&bytes).unwrap_err());
                assert!(error.contains(&format!("{class} (1)")), "{error}");
                assert!(error.contains("rbx publish instead"), "{error}");
            }
        }
    }
}
