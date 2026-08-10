use anyhow::{Result, bail};
use rbx_dom_weak::WeakDom as RbxWeakDom;
use rbx_dom_weak::types::{Ref as RbxRef, Variant as RbxVariant};
use serde_json::{Value, json};

use super::bytecode_api::{high_level_path_ordinals, parse_path_segments};
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
    let mut dom = input_format.read(&args.input)?;

    let path_segments = parse_path_segments(&args.path_segments_json)?;
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

    let top_level_refs = rbx_model_top_level_refs(&dom);
    output_format.write(&args.output, &dom, &top_level_refs)?;

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
