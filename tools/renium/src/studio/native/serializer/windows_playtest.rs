//! Structurally qualified Play loading, teleport dispatch and arrival bindings.
//! Diagnostic collection remains read-only; each executor requires its own permit.
use super::*;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeSet, VecDeque};

#[path = "windows_playtest_caption.rs"]
pub(super) mod caption;
#[path = "windows_playtest_client.rs"]
pub(super) mod client;

#[path = "windows_playtest_dispatch.rs"]
pub(super) mod dispatch;
#[path = "windows_playtest_gui.rs"]
pub(super) mod gui;
#[path = "windows_playtest_identity.rs"]
mod identity;
#[path = "windows_playtest_image.rs"]
mod image;
#[cfg(test)]
#[path = "windows_playtest_integration.rs"]
mod integration;
#[path = "windows_playtest_join.rs"]
pub(super) mod join;
#[path = "windows_playtest_live.rs"]
mod live;
#[path = "windows_playtest_loader.rs"]
pub(super) mod loader;
#[path = "windows_playtest_owner.rs"]
pub(super) mod owner;
#[path = "windows_playtest_server.rs"]
pub(super) mod server;
#[cfg(test)]
#[path = "windows_playtest_tests.rs"]
mod tests;
#[path = "windows_playtest_types.rs"]
mod types;

#[cfg(test)]
#[test]
#[ignore = "requires the explicitly owned Studio process"]
fn owned_teleport_gui_binding() -> Result<()> {
    let pid = std::env::var("RENIUM_TELEPORT_TEST_PID")?.parse()?;
    let memory = ProcessMemory::open_read_only(pid)?;
    let loaded = modules(pid)?;
    let studio = &loaded[0];
    println!(
        "{}",
        json!({"base":studio.base,"binding":gui::probe(&memory, studio)?})
    );
    Ok(())
}

const TOTAL_CODE_BYTES: usize = 16 * 1024 * 1024;

fn outcome(result: Result<Value>) -> Value {
    match result {
        Ok(value) => json!({"status":"observed","value":value}),
        Err(error) => json!({"status":"unresolved","reason":format!("{error:#}")}),
    }
}

fn target_rvas(value: &Value, result: &mut Vec<usize>) {
    match value {
        Value::Array(rows) => {
            for row in rows {
                target_rvas(row, result);
            }
        }
        Value::Object(fields) => {
            for (key, value) in fields {
                if key == "targetRva" {
                    if let Some(rva) = value.as_u64() {
                        result.push(rva as usize);
                    }
                } else {
                    target_rvas(value, result);
                }
            }
        }
        _ => {}
    }
}

fn function_record(
    memory: &ProcessMemory,
    studio: &ModuleEntry,
    pe: &PeImage<'_>,
    index: &image::Index,
    rva: usize,
) -> Result<Value> {
    let (begin, end) = image::code_range(pe, index, rva)?;
    let offset = pe.rva_to_offset(begin)?;
    let bytes = slice(pe.bytes, offset, end - begin)?;
    let loaded = memory.read_vec(studio.base + begin, bytes.len());
    let loaded_matches = loaded.as_ref().is_ok_and(|value| value == bytes);
    let calls = index
        .calls
        .get(&begin)
        .cloned()
        .unwrap_or_else(|| image::leaf_calls(bytes, begin, end));
    let operands = image::operands(bytes, begin);
    let live_objects = live::referenced_objects(memory, studio, pe, &operands);
    Ok(json!({"rva":begin,"requestedRva":rva,"endRva":end,
        "boundary":if index.functions.contains_key(&begin) {"x64-unwind"} else {"bounded-leaf"},
        "diskCodeBase64":base64::encode(bytes),"diskSha256":format!("{:x}",Sha256::digest(bytes)),
        "loadedMatchesDisk":loaded_matches,"loadedReadError":loaded.err().map(|e| format!("{e:#}")),
        "decodeInvalid":index.invalid.contains(&begin),
        "calls":calls,"callers":index.callers.get(&begin),
        "operands":operands,"referencedLiveObjects":live_objects}))
}

fn graph(
    memory: &ProcessMemory,
    studio: &ModuleEntry,
    pe: &PeImage<'_>,
    index: &image::Index,
    roots: Vec<usize>,
    args: &crate::cli::NativeProbeArgs,
) -> Value {
    let mut seen = BTreeSet::new();
    let mut queue = roots
        .into_iter()
        .map(|rva| (rva, 0))
        .collect::<VecDeque<_>>();
    let mut records = Vec::new();
    let mut bytes = 0;
    while records.len() < usize::from(args.playtest_max_functions) && bytes < TOTAL_CODE_BYTES {
        let Some((rva, depth)) = queue.pop_front() else {
            break;
        };
        let begin = image::code_range(pe, index, rva).map_or(rva, |(begin, _)| begin);
        if !seen.insert(begin) {
            continue;
        }
        let record = function_record(memory, studio, pe, index, rva);
        if let Ok(value) = &record {
            bytes += value["endRva"]
                .as_u64()
                .unwrap_or(0)
                .saturating_sub(begin as u64) as usize;
            if depth < args.playtest_depth {
                let calls = value["calls"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|edge| edge["target"].as_u64().map(|rva| rva as usize));
                let callers = index
                    .callers
                    .get(&begin)
                    .into_iter()
                    .flatten()
                    .map(|edge| edge.target);
                for target in calls.chain(callers) {
                    if !seen.contains(&target) && pe.require_executable_rva(target).is_ok() {
                        queue.push_back((target, depth + 1));
                    }
                }
            }
        }
        records.push(json!({"requestedRva":rva,"depth":depth,"evidence":outcome(record)}));
    }
    let frontier = queue
        .into_iter()
        .map(|(rva, _)| rva)
        .filter(|rva| !seen.contains(rva))
        .collect::<BTreeSet<_>>();
    json!({"functions":records,"codeBytes":bytes,"frontierRvas":frontier,"truncated":!frontier.is_empty(),
        "functionLimit":args.playtest_max_functions,"depthLimit":args.playtest_depth,"byteLimit":TOTAL_CODE_BYTES,
        "indexFunctionCount":index.functions.len(),"indexInvalidFunctionRvas":index.invalid,
        "boundaryPolicy":"unwind ranges plus bounded leaf/thunks; no inferred ABI or indirect call target"})
}

fn static_evidence(
    memory: &ProcessMemory,
    studio: &ModuleEntry,
    pe: &PeImage<'_>,
    reflection: &Value,
    args: &crate::cli::NativeProbeArgs,
) -> Result<Value> {
    let filters = if args.playtest_http && args.playtest_anchor.is_empty() {
        image::HTTP_TERMS
            .iter()
            .map(|term| (*term).to_owned())
            .collect()
    } else {
        args.playtest_anchor.clone()
    };
    let anchors = image::anchors(pe, &filters)?;
    let rtti = image::rtti(pe, &anchors)?;
    let imports = outcome(image::imports(pe));
    let transport_imports = imports["value"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|row| image::http_import(row))
        .cloned()
        .collect::<Vec<_>>();
    let mut targets: BTreeSet<_> = anchors.iter().map(|a| a.rva).collect();
    if args.playtest_http {
        targets.extend(
            transport_imports
                .iter()
                .filter_map(|row| row["iatRva"].as_u64().map(|rva| rva as usize)),
        );
    }
    targets.extend(
        rtti.iter()
            .filter_map(|row| row["vtableRva"].as_u64().map(|rva| rva as usize)),
    );
    let first = image::pointer_references(pe, &targets)?;
    targets.extend(first.values().flatten());
    let second = image::pointer_references(pe, &targets)?;
    targets.extend(second.values().flatten());
    let index = image::index(pe, &targets)?;
    let mut roots = args.playtest_rva.clone();
    // Explicit RVAs select a focused follow-up; the full anchor/reference index
    // remains available, but does not consume the requested graph's budget.
    if roots.is_empty() {
        if args.playtest_http {
            // Keep binding candidates but do not seed unrelated service vtables.
            for report in reflection.as_array().into_iter().flatten() {
                for service in report["value"]["services"].as_array().into_iter().flatten() {
                    if service["class"] == "TeleportService" {
                        for member in service["members"].as_array().into_iter().flatten() {
                            target_rvas(&member["evidence"]["value"]["memberBinding"], &mut roots);
                        }
                    }
                }
            }
            for import in &transport_imports {
                if let Some(rva) = import["iatRva"].as_u64() {
                    roots.extend(
                        index
                            .references
                            .get(&(rva as usize))
                            .into_iter()
                            .flatten()
                            .map(|edge| edge.target),
                    );
                }
            }
        } else {
            target_rvas(reflection, &mut roots);
        }
        // Prefer reflection/controller/task entry strings before service URLs.
        for group in [
            "reflection",
            "controller",
            "input",
            "identity",
            "services",
            "requested",
        ] {
            for anchor in anchors.iter().filter(|a| a.groups.contains(&group)) {
                let mut refs = vec![anchor.rva];
                refs.extend(first.get(&anchor.rva).into_iter().flatten());
                let next = refs
                    .iter()
                    .flat_map(|rva| second.get(rva).into_iter().flatten().copied())
                    .collect::<Vec<_>>();
                refs.extend(next);
                for target in refs {
                    roots.extend(
                        index
                            .references
                            .get(&target)
                            .into_iter()
                            .flatten()
                            .map(|edge| edge.target),
                    );
                }
            }
        }
        target_rvas(&json!(rtti), &mut roots);
    }
    let mut unique = BTreeSet::new();
    roots.retain(|rva| unique.insert(*rva));
    let graph = graph(memory, studio, pe, &index, roots, args);
    Ok(
        json!({"anchors":anchors,"rtti":rtti,"staticPointerReferences":second,"codeReferences":index.references,
        "graph":graph,"imports":imports,"transportImports":transport_imports,
        "profile":if args.playtest_http {"teleport-http"} else {"playtest"},
        "scope":"Studio PE image; indirect/external edges remain unresolved",
        "pointerReferenceDepth":2,"executableScan":"all x64 unwind functions"}),
    )
}

pub(in super::super) fn discover(args: &crate::cli::NativeProbeArgs) -> Result<Value> {
    let pid = args.pid.context("Playtest discovery requires --pid")?;
    anyhow::ensure!(args.launch.is_none(), "Discovery cannot launch Studio");
    let identity =
        crate::app::update::process_start_identity(pid).context("Probe process is not alive")?;
    let memory = ProcessMemory::open_read_only(pid)?;
    let loaded = modules(pid)?;
    let studio = loaded.first().context("Process has no loaded image")?;
    anyhow::ensure!(
        studio.name.eq_ignore_ascii_case("RobloxStudioBeta.exe"),
        "Probe target is not Roblox Studio"
    );
    let mapped = observation::ImageView::open(&studio.path)?;
    let pe = PeImage::parse(mapped.bytes())?;
    anyhow::ensure!(
        read_u16(pe.bytes, read_u32(pe.bytes, 0x3c)? as usize + 4)? == 0x8664,
        "Playtest discovery requires a Windows x64 image"
    );
    verify_loaded_image(&memory, studio, pe.image_stamp)?;
    let reflection = outcome(live::reflection(&memory, studio, pid));
    let objects = if args.playtest_object.is_empty() {
        Value::Null
    } else {
        outcome(live::objects(&memory, studio, &args.playtest_object))
    };
    let evidence = static_evidence(&memory, studio, &pe, &json!([reflection, objects]), args)?;
    let command_line =
        crate::studio::performance::process_command_line(pid).map(|line| live::command_line(&line));
    let unchanged = crate::app::update::process_start_identity(pid).as_deref() == Some(&identity);
    anyhow::ensure!(unchanged, "Probe process identity changed during discovery");
    verify_loaded_image(&memory, studio, pe.image_stamp)?;
    let deployment = studio
        .path
        .parent()
        .and_then(Path::file_name)
        .and_then(OsStr::to_str)
        .filter(|s| s.starts_with("version-") && s[8..].bytes().all(|b| b.is_ascii_hexdigit()));
    Ok(
        json!({"schemaVersion":2,"ok":true,"readOnly":true,"nativeSwitchReady":false,
        "pid":pid,"processIdentity":identity,"build":super::super::studio_build_number(pid).ok(),"deployment":deployment,
        "image":{"name":studio.name,"base":studio.base,"size":studio.size,"stamp":pe.image_stamp,
            "sha256":format!("{:x}",Sha256::digest(mapped.bytes()))},
        "reflection":reflection,"objects":objects,"commandLine":command_line,"evidence":evidence,
        "loadedModules":loaded.iter().map(|module| json!({"name":module.name,"base":module.base,"size":module.size})).collect::<Vec<_>>(),
        "httpInterception":{"implemented":false,"requestBodiesCaptured":false,
            "lead":"Follow TeleportAsync into request construction, transport and completion; correlate a live request before selecting any hook",
            "responseContracts":["HTTP status and JSON/error envelope","join/placelauncher response parser and polling states",
                "local server endpoint, engine-issued join ticket and client bootstrap","exact saved-version asset stream and metadata",
                "same-window test controller ownership, destination service initialization and source Stop"]},
        "unresolvedContracts":["controller argument ABI and thread/async lifetime","clone versus saved-file reload and alternate input ownership",
            "destination identity before service creation","DataStore/MemoryStore request field data flow; Messaging/badge/pass contexts",
            "engine join ticket and client join data before scripts","same-window client presentation and hidden task viability",
            "teleport HTTP request ABI, response schema and local-session acceptance",
            "unchanged source Edit document and Stop/cancel restoration"],
        "followUp":"Compare Edit, Play server/client and stopped snapshots. Expand reported RVAs with --playtest-rva and --playtest-depth. No discovery report grants write permission."}),
    )
}
