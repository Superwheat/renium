//! Static PE evidence only. RVAs are observations, never a callable binding.
use super::*;
use iced_x86::{Decoder, DecoderOptions, FlowControl, Instruction, OpKind};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};

const ANCHOR_LIMIT: usize = 8192;
pub(super) const FUNCTION_BYTES: usize = 65536;

// A separate profile avoids exhausting the broad catalog (8,168 anchors on one
// observed image) before the request/response path receives a graph budget.
pub(super) const HTTP_TERMS: &[&str] = &[
    "teleportasync",
    "teleportservice",
    "request context",
    "requestcontext",
    "placelauncher",
    "gamejoin",
    "join-game",
    "join-place",
    "joinblob",
    "joinscript",
    "teleportjoin",
    "teleportoptions",
    "teleportdata",
    "assetdelivery",
    "asset-delivery",
    "serverplaceid",
    "httpclient",
    "httprequest",
    "httpresponse",
    "winhttp",
    "curl_easy",
    "curl_multi",
    "response code is not",
    "localplacefile",
];

pub(super) fn http_import(row: &Value) -> bool {
    let module = row["module"]
        .as_str()
        .unwrap_or_default()
        .to_ascii_lowercase();
    let symbol = row["symbol"]
        .as_str()
        .unwrap_or_default()
        .to_ascii_lowercase();
    ["winhttp", "wininet", "libcurl", "ssl", "ws2_32"]
        .iter()
        .any(|term| module.contains(term))
        || ["winhttp", "internet", "http", "curl_", "ssl_"]
            .iter()
            .any(|term| symbol.starts_with(term))
}

const TERMS: &[(&str, &[&str])] = &[
    (
        "reflection",
        &[
            "studiotestservice",
            "executeplaymode",
            "executemultiplayertest",
            "executerunmode",
            "testarguments",
            "testargs",
            "gettestargs",
        ],
    ),
    (
        "controller",
        &[
            "playcontroller",
            "playtest",
            "testsession",
            "playmode",
            "runmode",
            "stoptest",
            "stopplay",
            "playhere",
            "play here",
            "actionplay",
            "actionstop",
            "simulationstart",
            "simulationstop",
            "robloxidedoc",
            "robloxplaydoc",
        ],
    ),
    (
        "input",
        &[
            "startserver",
            "startclient",
            "localplacefile",
            "placefilename",
            "teamtest",
            "team test",
            "openandplay",
            "editfile",
            "studiotestservicemode",
            "testserver",
            "testclient",
            "datamodel",
            "cloneplace",
        ],
    ),
    (
        "identity",
        &[
            "placeid",
            "universeid",
            "gameid",
            "jobid",
            "jointicket",
            "joindata",
            "teleportdata",
            "authenticationticket",
            "gamejoin",
            "clientticket",
        ],
    ),
    (
        "services",
        &[
            "datastore",
            "memorystore",
            "messagingservice",
            "messaging-service",
            "badge",
            "gamepass",
            "game-pass",
        ],
    ),
];

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Anchor {
    pub(super) rva: usize,
    encoding: &'static str,
    pub(super) text: String,
    pub(super) groups: Vec<&'static str>,
}

fn add_anchor(
    anchors: &mut Vec<Anchor>,
    rva: usize,
    text: String,
    encoding: &'static str,
    extra: &[String],
) -> Result<()> {
    let folded = text.to_ascii_lowercase();
    let mut groups = TERMS
        .iter()
        .filter(|_| extra.is_empty())
        .filter_map(|(group, terms)| {
            terms
                .iter()
                .any(|term| folded.contains(term))
                .then_some(*group)
        })
        .collect::<Vec<_>>();
    if extra
        .iter()
        .any(|term| folded.contains(&term.to_ascii_lowercase()))
    {
        groups.push("requested");
    }
    if !groups.is_empty() {
        anyhow::ensure!(
            anchors.len() < ANCHOR_LIMIT,
            "Playtest string anchor budget exceeded; repeat with a narrower --playtest-anchor filter"
        );
        anchors.push(Anchor {
            rva,
            encoding,
            text,
            groups,
        });
    }
    Ok(())
}

pub(super) fn anchors(image: &PeImage<'_>, extra: &[String]) -> Result<Vec<Anchor>> {
    anyhow::ensure!(
        extra.len() <= 32 && extra.iter().all(|s| (3..=256).contains(&s.len())),
        "Use at most 32 anchors, each 3 through 256 bytes"
    );
    let mut result = Vec::new();
    for section in image
        .sections
        .iter()
        .filter(|s| s.characteristics & 0x2000_0000 == 0)
    {
        let bytes = slice(image.bytes, section.raw_offset, section.raw_size)?;
        let mut at = 0;
        while at < bytes.len() {
            let start = at;
            while at < bytes.len() && (32..=126).contains(&bytes[at]) {
                at += 1;
            }
            if (4..=2048).contains(&(at - start)) && bytes.get(at) == Some(&0) {
                add_anchor(
                    &mut result,
                    section.virtual_address + start,
                    String::from_utf8(bytes[start..at].to_vec())?,
                    "ascii",
                    extra,
                )?;
            }
            at += 1;
        }
        // Scan both alignments: UTF-16 literals need not start at section+2n.
        for alignment in 0..2 {
            let mut at = alignment;
            while at + 1 < bytes.len() {
                let start = at;
                while at + 1 < bytes.len() && (32..=126).contains(&bytes[at]) && bytes[at + 1] == 0
                {
                    at += 2;
                }
                if (4..=2048).contains(&((at - start) / 2))
                    && bytes.get(at..at + 2) == Some(&[0, 0])
                {
                    let text = bytes[start..at]
                        .iter()
                        .step_by(2)
                        .map(|b| char::from(*b))
                        .collect();
                    add_anchor(
                        &mut result,
                        section.virtual_address + start,
                        text,
                        "utf16le",
                        extra,
                    )?;
                }
                at += 2;
            }
        }
    }
    result.sort_by_key(|a| a.rva);
    Ok(result)
}

pub(super) fn pointer_references(
    image: &PeImage<'_>,
    targets: &BTreeSet<usize>,
) -> Result<BTreeMap<usize, Vec<usize>>> {
    let mut refs: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for section in image
        .sections
        .iter()
        .filter(|s| s.characteristics & 0x2000_0000 == 0)
    {
        for at in (0..section.raw_size.saturating_sub(7)).step_by(8) {
            let value = read_u64(image.bytes, section.raw_offset + at)? as usize;
            if let Some(rva) = value
                .checked_sub(image.image_base)
                .filter(|rva| targets.contains(rva))
            {
                refs.entry(rva)
                    .or_default()
                    .push(section.virtual_address + at);
            }
        }
    }
    Ok(refs)
}

pub(super) fn rtti(image: &PeImage<'_>, strings: &[Anchor]) -> Result<Vec<Value>> {
    // MSVC x64 complete-object locator ABI, not a Studio object field layout.
    let types: BTreeMap<_, _> = strings
        .iter()
        .filter(|a| a.encoding == "ascii" && a.text.starts_with(".?A"))
        .filter_map(|a| a.rva.checked_sub(16).map(|rva| (rva, &a.text)))
        .collect();
    let mut locators = BTreeMap::new();
    for section in image
        .sections
        .iter()
        .filter(|s| s.characteristics & 0x2000_0000 == 0)
    {
        for at in (0..section.raw_size.saturating_sub(23)).step_by(4) {
            let offset = section.raw_offset + at;
            let rva = section.virtual_address + at;
            if read_u32(image.bytes, offset)? != 1
                || read_u32(image.bytes, offset + 20)? as usize != rva
            {
                continue;
            }
            let type_rva = read_u32(image.bytes, offset + 12)? as usize;
            if let Some(name) = types.get(&type_rva) {
                let hierarchy = read_u32(image.bytes, offset + 16)? as usize;
                if image.rva_to_offset(hierarchy).is_err() {
                    continue;
                }
                locators.insert(rva, json!({"type":name,"typeDescriptorRva":type_rva,"locatorRva":rva,
                    "offsetToTop":read_u32(image.bytes,offset+4)?,"constructorDisplacement":read_u32(image.bytes,offset+8)?,"hierarchyRva":hierarchy}));
            }
        }
    }
    let pointers = pointer_references(image, &locators.keys().copied().collect())?;
    let mut result = Vec::new();
    for (locator, row) in locators {
        for pointer in pointers.get(&locator).into_iter().flatten() {
            let table = pointer + 8;
            let mut row = row.clone();
            let slots = static_vtable(image, table)?;
            if slots.is_empty() {
                continue;
            }
            row["vtableRva"] = json!(table);
            row["slots"] = json!(slots);
            row["slotBudgetReached"] = json!(slots.len() == 256);
            result.push(row);
        }
    }
    Ok(result)
}

fn static_vtable(image: &PeImage<'_>, table: usize) -> Result<Vec<Value>> {
    let mut slots = Vec::new();
    for slot in 0..256 {
        let Ok(offset) = image.rva_to_offset(table + slot * 8) else {
            break;
        };
        let Some(target) = (read_u64(image.bytes, offset)? as usize).checked_sub(image.image_base)
        else {
            break;
        };
        if image.require_executable_rva(target).is_err() {
            break;
        }
        slots.push(json!({"slot":slot,"byteOffset":slot*8,"targetRva":target}));
    }
    Ok(slots)
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Edge {
    pub(super) at: usize,
    pub(super) target: usize,
}

pub(super) fn leaf_calls(bytes: &[u8], begin: usize, end: usize) -> Vec<Edge> {
    Decoder::with_ip(64, bytes, begin as u64, DecoderOptions::NONE)
        .into_iter()
        .filter(|i| {
            i.op0_kind() == OpKind::NearBranch64
                && matches!(
                    i.flow_control(),
                    FlowControl::Call
                        | FlowControl::UnconditionalBranch
                        | FlowControl::ConditionalBranch
                )
        })
        .filter_map(|i| {
            let target = i.near_branch_target() as usize;
            (!(begin..end).contains(&target)).then_some(Edge {
                at: i.ip() as usize,
                target,
            })
        })
        .collect()
}

#[derive(Default)]
pub(super) struct Index {
    pub(super) functions: BTreeMap<usize, usize>,
    pub(super) calls: BTreeMap<usize, Vec<Edge>>,
    pub(super) callers: BTreeMap<usize, Vec<Edge>>,
    pub(super) references: BTreeMap<usize, Vec<Edge>>,
    pub(super) invalid: BTreeSet<usize>,
}

fn reference(i: &Instruction, image_base: usize) -> Option<usize> {
    if i.is_ip_rel_memory_operand() {
        return Some(i.ip_rel_memory_address() as usize);
    }
    (0..i.op_count()).find_map(|op| {
        (i.op_kind(op) == OpKind::Immediate64)
            .then(|| (i.immediate64() as usize).checked_sub(image_base))
            .flatten()
    })
}

pub(super) fn index(image: &PeImage<'_>, targets: &BTreeSet<usize>) -> Result<Index> {
    let mut result = Index::default();
    let table = image.section(b".pdata")?;
    for at in (0..table.raw_size.saturating_sub(11)).step_by(12) {
        let begin = read_u32(image.bytes, table.raw_offset + at)? as usize;
        let end = read_u32(image.bytes, table.raw_offset + at + 4)? as usize;
        if begin == 0 && end == 0 {
            continue;
        }
        anyhow::ensure!(begin < end, "Invalid playtest unwind range");
        image.require_executable_rva(begin)?;
        image.require_executable_rva(end - 1)?;
        result.functions.insert(begin, end);
    }
    for (&begin, &end) in &result.functions {
        let offset = image.rva_to_offset(begin)?;
        let bytes = slice(image.bytes, offset, end - begin)?;
        for i in Decoder::with_ip(64, bytes, begin as u64, DecoderOptions::NONE) {
            if i.is_invalid() {
                result.invalid.insert(begin);
            }
            if let Some(target) =
                reference(&i, image.image_base).filter(|rva| targets.contains(rva))
            {
                result.references.entry(target).or_default().push(Edge {
                    at: i.ip() as usize,
                    target: begin,
                });
            }
            if i.op0_kind() == OpKind::NearBranch64
                && matches!(
                    i.flow_control(),
                    FlowControl::Call
                        | FlowControl::UnconditionalBranch
                        | FlowControl::ConditionalBranch
                )
            {
                let target = i.near_branch_target() as usize;
                if !(begin..end).contains(&target) && image.require_executable_rva(target).is_ok() {
                    result.calls.entry(begin).or_default().push(Edge {
                        at: i.ip() as usize,
                        target,
                    });
                    result.callers.entry(target).or_default().push(Edge {
                        at: i.ip() as usize,
                        target: begin,
                    });
                }
            }
        }
    }
    Ok(result)
}

pub(super) fn code_range(image: &PeImage<'_>, index: &Index, rva: usize) -> Result<(usize, usize)> {
    image.require_executable_rva(rva)?;
    if let Some((&begin, &end)) = index
        .functions
        .range(..=rva)
        .next_back()
        .filter(|(_, end)| rva < **end)
    {
        anyhow::ensure!(
            end - begin <= FUNCTION_BYTES,
            "Function exceeds 64 KiB evidence budget"
        );
        return Ok((begin, end));
    }
    // Leaf/thunk without unwind metadata: stop at the first terminator; do not
    // invent a boundary for calls or conditional control flow.
    let offset = image.rva_to_offset(rva)?;
    let section = image
        .sections
        .iter()
        .find(|s| (s.raw_offset..s.raw_offset + s.raw_size).contains(&offset))
        .context("Missing code section")?;
    let bytes = slice(
        image.bytes,
        offset,
        256.min(section.raw_offset + section.raw_size - offset),
    )?;
    for i in Decoder::with_ip(64, bytes, rva as u64, DecoderOptions::NONE) {
        anyhow::ensure!(!i.is_invalid(), "Invalid leaf instruction");
        match i.flow_control() {
            FlowControl::Return
            | FlowControl::UnconditionalBranch
            | FlowControl::IndirectBranch => return Ok((rva, i.next_ip() as usize)),
            FlowControl::Next => {}
            _ => break,
        }
    }
    bail!("No unwind or bounded leaf boundary for RVA {rva:#x}")
}

pub(super) fn operands(bytes: &[u8], rva: usize) -> Vec<Value> {
    Decoder::with_ip(64, bytes, rva as u64, DecoderOptions::NONE).into_iter().filter(|i| i.op_kinds().any(|op| op == OpKind::Memory) || i.flow_control() != FlowControl::Next)
        .map(|i| json!({"rva":i.ip(),"length":i.len(),"mnemonic":format!("{:?}",i.mnemonic()),"flow":format!("{:?}",i.flow_control()),
            "base":format!("{:?}",i.memory_base()),"index":format!("{:?}",i.memory_index()),"scale":i.memory_index_scale(),
            "displacement":i.memory_displacement64(),"width":i.memory_size().size(),
            "ripTarget":i.is_ip_rel_memory_operand().then(|| i.ip_rel_memory_address()),
            "directTarget":(i.op0_kind()==OpKind::NearBranch64).then(|| i.near_branch_target()),
            "operands":(0..i.op_count()).map(|n| json!({"kind":format!("{:?}",i.op_kind(n)),"register":format!("{:?}",i.op_register(n))})).collect::<Vec<_>>() })).collect()
}

pub(super) fn imports(image: &PeImage<'_>) -> Result<Value> {
    use super::super::probes::{directory, image_string};
    let (rva, size) = directory(image, 1)?;
    if rva == 0 {
        return Ok(json!([]));
    }
    let table = image.rva_to_offset(rva)?;
    let mut result = Vec::new();
    for at in (0..size.saturating_sub(19)).step_by(20) {
        let offset = table + at;
        let name = read_u32(image.bytes, offset + 12)? as usize;
        if name == 0 {
            break;
        }
        let module = image_string(image, name)?;
        let names = image.rva_to_offset(read_u32(image.bytes, offset)? as usize)?;
        let iat = read_u32(image.bytes, offset + 16)? as usize;
        let mut terminated = false;
        for slot in 0..65536 {
            let name = read_u64(image.bytes, names + slot * 8)?;
            if name == 0 {
                terminated = true;
                break;
            }
            let symbol = if name >> 63 != 0 {
                json!({"ordinal":name & 0xffff})
            } else {
                json!(image_string(image, name as usize + 2)?)
            };
            result.push(json!({"module":module,"symbol":symbol,"iatRva":iat+slot*8}));
        }
        anyhow::ensure!(terminated, "Unterminated PE import table");
    }
    Ok(json!(result))
}
