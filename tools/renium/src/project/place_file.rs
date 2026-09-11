use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{Result, bail};
use rbx_dom_weak::WeakDom as RbxWeakDom;
use rbx_dom_weak::types::{Ref as RbxRef, Variant as RbxVariant};
use serde_json::{Map, Value, json};

use crate::app::output::print_json_output;
use crate::cli::{ComparePlaceArgs, QueryPlaceArgs};
use crate::editor::sync::is_lua_source_class;
use crate::project::config;
use crate::rbx::model::{RbxPlaceFormat, build_rbx_place, rbx_dom_instance_path_parts};
use crate::system::text::normalized_source_bytes;

#[derive(Clone, Eq, Ord, PartialEq, PartialOrd)]
struct ScriptRecord {
    class_name: String,
    source: Vec<u8>,
}

type ScriptGroups = BTreeMap<Vec<String>, Vec<ScriptRecord>>;

struct Descendants<'a> {
    dom: &'a RbxWeakDom,
    pending: Vec<RbxRef>,
}

impl Iterator for Descendants<'_> {
    type Item = RbxRef;

    fn next(&mut self) -> Option<Self::Item> {
        while let Some(referent) = self.pending.pop() {
            let Some(instance) = self.dom.get_by_ref(referent) else {
                continue;
            };
            self.pending.extend(instance.children().iter().copied());
            return Some(referent);
        }
        None
    }
}

pub(super) fn place_dom(path: &Path) -> Result<RbxWeakDom> {
    if !path.is_file() {
        bail!("Place file does not exist: {}", path.display());
    }
    RbxPlaceFormat::from_path(path)?.read(path)
}

fn descendants(dom: &RbxWeakDom) -> Descendants<'_> {
    Descendants {
        dom,
        pending: dom.root().children().to_vec(),
    }
}

fn script_source(dom: &RbxWeakDom, referent: RbxRef) -> Option<&str> {
    let instance = dom.get_by_ref(referent)?;
    if !is_lua_source_class(instance.class.as_str()) {
        return None;
    }
    match instance
        .properties
        .iter()
        .find(|(name, _)| name.as_str() == "Source")
        .map(|(_, value)| value)
    {
        Some(RbxVariant::String(source)) => Some(source),
        None => Some(""),
        _ => None,
    }
}

fn query_result(dom: &RbxWeakDom, referent: RbxRef) -> Value {
    let instance = dom
        .get_by_ref(referent)
        .expect("query results contain live referents");
    let (path, ordinals) = rbx_dom_instance_path_parts(dom, referent);
    let mut result = Map::from_iter([
        ("path".to_string(), json!(path)),
        ("name".to_string(), json!(instance.name)),
        ("className".to_string(), json!(instance.class.as_str())),
    ]);
    if ordinals.iter().any(|ordinal| *ordinal != 1) {
        result.insert("ordinals".to_string(), json!(ordinals));
    }
    Value::Object(result)
}

pub(crate) fn query_place(args: QueryPlaceArgs) -> Result<()> {
    let query = args
        .query
        .as_deref()
        .map(str::trim)
        .filter(|query| !query.is_empty())
        .map(str::to_ascii_lowercase);
    let name = args.name.as_deref().filter(|name| !name.is_empty());
    let class_name = args
        .class_name
        .as_deref()
        .filter(|class_name| !class_name.is_empty());
    let source = args.source.as_deref().filter(|source| !source.is_empty());
    if query.is_none() && name.is_none() && class_name.is_none() && source.is_none() {
        bail!("Provide a query, --name, --class, or --source");
    }

    let dom = place_dom(&args.input)?;
    let limit = if args.all {
        usize::MAX
    } else {
        args.limit.get()
    };
    let mut matches = Vec::new();
    let mut truncated = false;
    for referent in descendants(&dom) {
        let Some(instance) = dom.get_by_ref(referent) else {
            continue;
        };
        if name.is_some_and(|name| instance.name != name)
            || class_name.is_some_and(|class_name| instance.class.as_str() != class_name)
            || source.is_some_and(|source| {
                script_source(&dom, referent).is_none_or(|candidate| !candidate.contains(source))
            })
            || query.as_ref().is_some_and(|query| {
                !instance.name.to_ascii_lowercase().contains(query)
                    && !instance.class.as_str().to_ascii_lowercase().contains(query)
            })
        {
            continue;
        }
        if matches.len() == limit {
            truncated = true;
            break;
        }
        matches.push(query_result(&dom, referent));
    }

    print_json_output(
        &json!({
            "ok": true,
            "input": args.input,
            "matches": matches,
            "truncated": truncated,
        }),
        args.pretty,
    )
}

fn projected_services(root: &Path) -> Result<Vec<String>> {
    let mut services = Vec::new();
    for entry in super::storage::service_directories(root)? {
        if let Some(name) = entry.file_name().and_then(|name| name.to_str())
            && !name.starts_with('.')
        {
            services.push(name.to_string());
        }
    }
    services.sort();
    Ok(services)
}

fn script_groups(dom: &RbxWeakDom, services: &BTreeSet<String>) -> ScriptGroups {
    let mut groups = ScriptGroups::new();
    for referent in descendants(dom) {
        let Some(source) = script_source(dom, referent) else {
            continue;
        };
        let (path, _) = rbx_dom_instance_path_parts(dom, referent);
        if !path
            .first()
            .is_some_and(|service| services.contains(service))
        {
            continue;
        }
        let Some(instance) = dom.get_by_ref(referent) else {
            continue;
        };
        groups.entry(path).or_default().push(ScriptRecord {
            class_name: instance.class.to_string(),
            source: normalized_source_bytes(source.as_bytes()).collect(),
        });
    }
    for scripts in groups.values_mut() {
        scripts.sort();
    }
    groups
}

fn remove_exact_matches(project: &mut Vec<ScriptRecord>, place: &mut Vec<ScriptRecord>) -> usize {
    let mut matched = 0;
    let mut index = 0;
    while index < project.len() {
        if let Ok(place_index) = place.binary_search(&project[index]) {
            project.remove(index);
            place.remove(place_index);
            matched += 1;
        } else {
            index += 1;
        }
    }
    matched
}

fn take_same_class(scripts: &mut Vec<ScriptRecord>, class_name: &str) -> Option<ScriptRecord> {
    let index = scripts
        .iter()
        .position(|script| script.class_name == class_name)?;
    Some(scripts.remove(index))
}

fn difference_value(
    kind: &str,
    path: &[String],
    project: Option<&ScriptRecord>,
    place: Option<&ScriptRecord>,
) -> Value {
    let mut value = Map::from_iter([
        ("kind".to_string(), json!(kind)),
        ("path".to_string(), json!(path)),
    ]);
    if let Some(project) = project {
        value.insert("projectClass".to_string(), json!(project.class_name));
    }
    if let Some(place) = place {
        value.insert("placeClass".to_string(), json!(place.class_name));
    }
    Value::Object(value)
}

pub(crate) fn compare_place(args: ComparePlaceArgs, project: Option<&Path>) -> Result<()> {
    let started = std::time::Instant::now();
    let read = |path: &Path| {
        if args.full {
            // Full comparison already normalizes exact reflection defaults away.
            // Omit them during decoding instead of allocating then dropping them.
            RbxPlaceFormat::from_path(path)?.read_for_comparison(path)
        } else {
            place_dom(path)
        }
    };
    let (place_dom, mut project_dom, project_path, service_set) =
        if let Some(against) = &args.against {
            let (before, target) = rayon::join(|| read(&args.input), || read(against));
            let before = before?;
            let target = target?;
            let services = [&before, &target]
                .into_iter()
                .flat_map(|dom| {
                    dom.root()
                        .children()
                        .iter()
                        .filter_map(|id| dom.get_by_ref(*id).map(|node| node.name.clone()))
                })
                .collect::<BTreeSet<_>>();
            (before, target, against.clone(), services)
        } else {
            let before = read(&args.input)?;
            let loaded = config::load_project(project, None)?;
            let projection = config::stage_project(&loaded)?;
            let services = projected_services(projection.root())?;
            let service_set = services.iter().cloned().collect::<BTreeSet<_>>();
            let dom = build_rbx_place(projection.root(), services, None, false, false, false)?.dom;
            (before, dom, loaded.path, service_set)
        };
    crate::app::output::log_global(
        4,
        format_args!(
            "[renium] place comparison input decoding: {:.1}ms",
            started.elapsed().as_secs_f64() * 1000.0
        ),
    );
    if args.full {
        let not_serialized = if args.against.is_none() {
            super::place_diff::omit_unsaved_project_properties(&mut project_dom)?
        } else {
            BTreeMap::new()
        };
        let mut result = super::place_diff::compare(&place_dom, &project_dom, &service_set, &args)?;
        if !not_serialized.is_empty() {
            result["notSerializedProjectProperties"] = json!(not_serialized);
        }
        result["input"] = json!(args.input);
        result["target"] = json!(project_path);
        result["targetKind"] = json!(if args.against.is_some() {
            "place"
        } else {
            "project"
        });
        let started = std::time::Instant::now();
        rayon::join(|| drop(place_dom), || drop(project_dom));
        crate::app::output::log_global(
            4,
            format_args!(
                "[renium] place comparison input release: {:.1}ms",
                started.elapsed().as_secs_f64() * 1000.0
            ),
        );
        return print_json_output(&result, args.pretty);
    }
    let project_scripts = script_groups(&project_dom, &service_set);
    let place_scripts = script_groups(&place_dom, &service_set);
    let project_count = project_scripts.values().map(Vec::len).sum::<usize>();
    let place_count = place_scripts.values().map(Vec::len).sum::<usize>();

    let paths = project_scripts
        .keys()
        .chain(place_scripts.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    let mut matching = 0;
    let mut changed = 0;
    let mut missing_from_place = 0;
    let mut extra_in_place = 0;
    let mut differences = Vec::new();
    let limit = if args.all {
        usize::MAX
    } else {
        args.limit.get()
    };

    let mut record_difference = |difference: Value| {
        if differences.len() < limit {
            differences.push(difference);
        }
    };
    for path in paths {
        let mut project = project_scripts.get(&path).cloned().unwrap_or_default();
        let mut place = place_scripts.get(&path).cloned().unwrap_or_default();
        matching += remove_exact_matches(&mut project, &mut place);

        let mut index = 0;
        while index < project.len() {
            let Some(place_script) = take_same_class(&mut place, &project[index].class_name) else {
                index += 1;
                continue;
            };
            let project_script = project.remove(index);
            changed += 1;
            record_difference(difference_value(
                "sourceChanged",
                &path,
                Some(&project_script),
                Some(&place_script),
            ));
        }

        while !project.is_empty() && !place.is_empty() {
            let project_script = project.remove(0);
            let place_script = place.remove(0);
            changed += 1;
            record_difference(difference_value(
                "classChanged",
                &path,
                Some(&project_script),
                Some(&place_script),
            ));
        }
        for project_script in project {
            missing_from_place += 1;
            record_difference(difference_value(
                "missingFromPlace",
                &path,
                Some(&project_script),
                None,
            ));
        }
        for place_script in place {
            extra_in_place += 1;
            record_difference(difference_value(
                "extraInPlace",
                &path,
                None,
                Some(&place_script),
            ));
        }
    }

    let difference_count = changed + missing_from_place + extra_in_place;
    print_json_output(
        &json!({
            "ok": true,
            "matches": difference_count == 0,
            "input": args.input,
            "project": project_path,
            "scope": "scripts",
            "targetKind": if args.against.is_some() { "place" } else { "project" },
            "services": service_set,
            "projectScripts": project_count,
            "placeScripts": place_count,
            "matchingScripts": matching,
            "changed": changed,
            "missingFromPlace": missing_from_place,
            "extraInPlace": extra_in_place,
            "differences": differences,
            "truncated": difference_count > differences.len(),
        }),
        args.pretty,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use rbx_dom_weak::InstanceBuilder;

    fn script(class_name: &str, name: &str, source: &str) -> InstanceBuilder {
        InstanceBuilder::new(class_name)
            .with_name(name)
            .with_property("Source", source)
    }

    #[test]
    fn script_comparison_ignores_line_endings_and_order_for_duplicates() {
        let mut left = vec![
            ScriptRecord {
                class_name: "ModuleScript".to_string(),
                source: normalized_source_bytes(b"return 1\r\n").collect(),
            },
            ScriptRecord {
                class_name: "ModuleScript".to_string(),
                source: normalized_source_bytes(b"return 2\n").collect(),
            },
        ];
        let mut right = vec![
            ScriptRecord {
                class_name: "ModuleScript".to_string(),
                source: normalized_source_bytes(b"return 2\r\n").collect(),
            },
            ScriptRecord {
                class_name: "ModuleScript".to_string(),
                source: normalized_source_bytes(b"return 1\n").collect(),
            },
        ];
        left.sort();
        right.sort();
        assert_eq!(remove_exact_matches(&mut left, &mut right), 2);
        assert!(left.is_empty());
        assert!(right.is_empty());
    }

    #[test]
    fn place_queries_script_source_without_studio() {
        let mut dom = RbxWeakDom::new(InstanceBuilder::new("DataModel"));
        let service = dom.insert(dom.root_ref(), InstanceBuilder::new("ReplicatedStorage"));
        let script = dom.insert(service, script("ModuleScript", "Reward", "return 'daily'"));
        assert_eq!(script_source(&dom, script), Some("return 'daily'"));
        assert_eq!(
            query_result(&dom, script)["path"],
            json!(["ReplicatedStorage", "Reward"])
        );
    }
}
