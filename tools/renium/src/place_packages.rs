use std::fs::{self, File};
use std::io::{BufReader, BufWriter};

use anyhow::{Context, Result, bail};
use rbx_dom_weak::WeakDom as RbxWeakDom;
use rbx_dom_weak::types::{Ref as RbxRef, Variant as RbxVariant};
use serde_json::{Value, json};

use super::bytecode_api::{high_level_path_ordinals, high_level_split_path};
use super::command_line::PlaceDesyncPackageLinkArgs;
use super::output::print_json_output;
use super::rbx_encode::rbx_model_top_level_refs;
use super::rbx_model::{
    RbxPlaceFormat, rbx_dom_instance_by_path_unique, rbx_dom_instance_path_segments,
};
pub(super) fn place_desync_package_link(args: PlaceDesyncPackageLinkArgs) -> Result<()> {
    let input_format = RbxPlaceFormat::from_path(&args.input)?;
    let output_format = match args.output_format.as_deref() {
        Some(raw) => RbxPlaceFormat::parse(raw)?,
        None => RbxPlaceFormat::from_path(&args.output).unwrap_or(input_format),
    };
    let input = File::open(&args.input)
        .with_context(|| format!("Failed to read {}", args.input.display()))?;
    let reader = BufReader::new(input);
    let mut dom = match input_format {
        RbxPlaceFormat::Binary => rbx_binary::from_reader(reader)
            .with_context(|| format!("Failed to read {}", args.input.display()))?,
        RbxPlaceFormat::Xml => rbx_xml::from_reader_default(reader)
            .with_context(|| format!("Failed to read {}", args.input.display()))?,
    };

    let path_segments = parse_place_path_segments(&args.path_segments_json)?;
    let path_ordinals = high_level_path_ordinals(Some(&args.path_ordinals_json))?;
    let target_ref = rbx_dom_instance_by_path_unique(&dom, &path_segments, &path_ordinals)?;
    let target_path = rbx_dom_instance_path_segments(&dom, target_ref);
    let package_link_refs = rbx_package_link_refs_for_desync(&dom, target_ref)?;
    let removed_package_links = package_link_refs
        .iter()
        .map(|referent| rbx_package_link_summary(&dom, *referent))
        .collect::<Result<Vec<_>>>()?;
    for referent in &package_link_refs {
        dom.destroy(*referent);
    }

    if let Some(parent) = args.output.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create {}", parent.display()))?;
    }
    let output = File::create(&args.output)
        .with_context(|| format!("Failed to write {}", args.output.display()))?;
    let writer = BufWriter::new(output);
    let top_level_refs = rbx_model_top_level_refs(&dom);
    match output_format {
        RbxPlaceFormat::Binary => rbx_binary::to_writer(writer, &dom, &top_level_refs)
            .with_context(|| format!("Failed to write {}", args.output.display()))?,
        RbxPlaceFormat::Xml => rbx_xml::to_writer_default(writer, &dom, &top_level_refs)
            .with_context(|| format!("Failed to write {}", args.output.display()))?,
    }

    print_json_output(
        &json!({
            "ok": true,
            "input": args.input,
            "output": args.output,
            "inputFormat": input_format.label(),
            "outputFormat": output_format.label(),
            "targetPathSegments": target_path,
            "removedPackageLinks": removed_package_links,
        }),
        args.pretty,
    )
}

fn parse_place_path_segments(raw: &str) -> Result<Vec<String>> {
    let raw = raw.trim();
    if raw.is_empty() {
        bail!("Path target cannot be empty");
    }
    let segments = if raw.starts_with('[') {
        parse_bracket_path_segments(raw).with_context(|| format!("Invalid path JSON: {raw}"))?
    } else {
        high_level_split_path(raw)
    };
    if segments.is_empty() {
        bail!("Path target cannot be empty");
    }
    Ok(segments)
}

pub(super) fn parse_bracket_path_segments(raw: &str) -> Result<Vec<String>> {
    if let Ok(segments) = serde_json::from_str::<Vec<String>>(raw) {
        return Ok(segments);
    }
    let inner = raw
        .trim()
        .strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
        .context("Path must start with '[' and end with ']'")?;
    let segments = inner
        .split(',')
        .map(|segment| segment.trim().trim_matches(['"', '\'']))
        .filter(|segment| !segment.is_empty())
        .map(str::to_string)
        .collect::<Vec<_>>();
    if segments.is_empty() {
        bail!("Path cannot be empty");
    }
    Ok(segments)
}

fn rbx_package_link_refs_for_desync(dom: &RbxWeakDom, target_ref: RbxRef) -> Result<Vec<RbxRef>> {
    let target = dom
        .get_by_ref(target_ref)
        .ok_or_else(|| anyhow::anyhow!("Target referent was not found"))?;
    if target.class.as_str() == "PackageLink" {
        return Ok(vec![target_ref]);
    }
    let links = target
        .children()
        .iter()
        .copied()
        .filter(|child_ref| {
            dom.get_by_ref(*child_ref)
                .is_some_and(|child| child.class.as_str() == "PackageLink")
        })
        .collect::<Vec<_>>();
    if links.is_empty() {
        bail!(
            "{} has no direct PackageLink child",
            rbx_dom_instance_path_segments(dom, target_ref).join(".")
        );
    }
    Ok(links)
}

fn rbx_package_link_summary(dom: &RbxWeakDom, referent: RbxRef) -> Result<Value> {
    let instance = dom
        .get_by_ref(referent)
        .ok_or_else(|| anyhow::anyhow!("PackageLink referent was not found"))?;
    Ok(json!({
        "referent": referent.to_string(),
        "name": instance.name.clone(),
        "className": instance.class.to_string(),
        "pathSegments": rbx_dom_instance_path_segments(dom, referent),
        "packageId": rbx_package_link_property_string(instance, "PackageId")
            .or_else(|| rbx_package_link_property_string(instance, "PackageIdSerialize")),
        "versionId": rbx_package_link_property_string(instance, "VersionId")
            .or_else(|| rbx_package_link_property_string(instance, "VersionIdSerialize")),
    }))
}

fn rbx_package_link_property_string(
    instance: &rbx_dom_weak::Instance,
    name: &str,
) -> Option<String> {
    let value = instance
        .properties
        .iter()
        .find_map(|(key, value)| (key.as_str() == name).then_some(value))?;
    match value {
        RbxVariant::String(value) => Some(value.clone()),
        RbxVariant::ContentId(value) => Some(value.as_str().to_string()),
        RbxVariant::Int32(value) => Some(value.to_string()),
        RbxVariant::Int64(value) => Some(value.to_string()),
        RbxVariant::Float32(value) => Some(value.to_string()),
        RbxVariant::Float64(value) => Some(value.to_string()),
        _ => None,
    }
}
