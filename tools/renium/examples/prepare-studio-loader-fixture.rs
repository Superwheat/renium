//! Offline fixture preparation for the disposable native-loader experiment.
use anyhow::{Context, Result};
use rbx_dom_weak::types::Variant;
use sha2::{Digest, Sha256};
use std::{fs, io::BufWriter, path::Path, time::Instant};

fn main() -> Result<()> {
    let started = Instant::now();
    if std::env::args().nth(1).as_deref() == Some("inspect-repetition") {
        return inspect_repetition(started);
    }
    if std::env::args().nth(1).as_deref() == Some("inspect-identities") {
        let input = std::env::args().nth(2).context("Missing RBXL")?;
        let dom = rbx_binary::from_reader(fs::File::open(input)?)?;
        let mut seen = std::collections::HashSet::new();
        let mut missing = std::collections::BTreeMap::<String, usize>::new();
        let mut duplicates = 0usize;
        for item in dom.descendants().skip(1) {
            match item.properties.get(&"UniqueId".into()) {
                Some(Variant::UniqueId(id)) => {
                    if !seen.insert(id.to_string()) {
                        duplicates += 1;
                    }
                }
                _ => {
                    *missing.entry(item.class.to_string()).or_default() += 1;
                }
            }
        }
        println!(
            "{}",
            serde_json::json!({"instances":dom.descendants().count()-1,
            "uniqueIdentities":seen.len(),"duplicates":duplicates,"missingByClass":missing,
            "ms":started.elapsed().as_secs_f64()*1000.})
        );
        if let Some(file) = std::env::args().nth(3) {
            let rows = fs::read(file)?;
            anyhow::ensure!(rows.len().is_multiple_of(72), "Invalid identity row size");
            let ids = rows
                .chunks_exact(72)
                .map(|row| {
                    row[..16]
                        .chunks_exact(4)
                        .map(|word| format!("{:08x}", u32::from_le_bytes(word.try_into().unwrap())))
                        .collect::<String>()
                })
                .collect::<Vec<_>>();
            let parent_by_id = dom
                .descendants()
                .skip(1)
                .filter_map(|item| {
                    let Some(Variant::UniqueId(id)) = item.properties.get(&"UniqueId".into())
                    else {
                        return None;
                    };
                    let parent = dom
                        .get_by_ref(item.parent())
                        .and_then(|parent| parent.properties.get(&"UniqueId".into()))
                        .and_then(|id| match id {
                            Variant::UniqueId(id) => Some(id.to_string()),
                            _ => None,
                        });
                    Some((id.to_string(), parent))
                })
                .collect::<std::collections::HashMap<_, _>>();
            let mut debug_ids = std::collections::HashSet::new();
            let mut remaining = seen;
            for (row, id) in rows.chunks_exact(72).zip(&ids) {
                anyhow::ensure!(
                    remaining.remove(id),
                    "Missing/duplicate native identity {id}"
                );
                let parent = u32::from_le_bytes(row[64..68].try_into().unwrap());
                let expected_parent = if parent == u32::MAX {
                    None
                } else {
                    Some(
                        ids.get(parent as usize)
                            .context("Invalid native parent")?
                            .clone(),
                    )
                };
                anyhow::ensure!(
                    parent_by_id.get(id) == Some(&expected_parent),
                    "Native/file parent mismatch for {id}"
                );
                let text = &row[16..64];
                let debug_id = std::str::from_utf8(
                    &text[..text
                        .iter()
                        .position(|b| *b == 0)
                        .context("Unterminated native debug ID")?],
                )?;
                anyhow::ensure!(
                    !debug_id.is_empty() && debug_ids.insert(debug_id),
                    "Invalid/duplicate debug identity"
                );
            }
            anyhow::ensure!(
                remaining.is_empty(),
                "Serialized instances missing native identities"
            );
            println!(
                "{}",
                serde_json::json!({"exactNativeIdentities":ids.len(),"parentsMatch":true,"uniqueDebugIds":debug_ids.len()})
            );
        }
        return Ok(());
    }
    if std::env::args().nth(1).as_deref() == Some("wrap-service-roots") {
        let input = std::env::args().nth(2).context("Missing input RBXL")?;
        let output = std::env::args().nth(3).context("Missing output RBXL")?;
        let mut dom = rbx_binary::from_reader(fs::File::open(input)?)?;
        let mut retained = Vec::new();
        for id in dom.root().children().to_vec() {
            let instance = dom.get_by_ref_mut(id).unwrap();
            retained.push(serde_json::json!({"name":instance.name,"class":instance.class.as_str(),"properties":instance.properties}));
            instance.class = "Folder".into();
            instance.properties.clear();
        }
        let manifest = Path::new(&output).with_extension("retained.json");
        serde_json::to_writer_pretty(
            fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(manifest)?,
            &retained,
        )?;
        rbx_binary::to_writer(
            BufWriter::new(
                fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&output)?,
            ),
            &dom,
            dom.root().children(),
        )?;
        println!(
            "{}",
            serde_json::json!({"output":output,"instances":dom.descendants().count()-1,
            "retainedServices":retained.len(),"sha256":format!("{:x}",Sha256::digest(fs::read(&output)?))})
        );
        return Ok(());
    }
    if std::env::args().nth(1).as_deref() == Some("strip-package-links") {
        let input = std::env::args().nth(2).context("Missing input RBXL")?;
        let output = std::env::args().nth(3).context("Missing output RBXL")?;
        let mut dom = rbx_binary::from_reader(fs::File::open(input)?)?;
        let mut pending = dom.root().children().to_vec();
        let mut links = Vec::new();
        let mut instances = 0;
        while let Some(id) = pending.pop() {
            let node = dom.get_by_ref(id).unwrap();
            instances += 1;
            if node.class == "PackageLink" {
                anyhow::ensure!(
                    node.children().is_empty(),
                    "PackageLink unexpectedly has children"
                );
                links.push(id);
            }
            pending.extend_from_slice(node.children());
        }
        for id in &links {
            dom.destroy(*id);
        }
        let file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&output)?;
        let mut writer = BufWriter::new(file);
        rbx_binary::to_writer(&mut writer, &dom, dom.root().children())?;
        drop(writer);
        println!(
            "{}",
            serde_json::json!({"removedLinks":links.len(),"beforeInstances":instances,
            "afterInstances":instances-links.len(),"output":output,
            "sha256":format!("{:x}",Sha256::digest(fs::read(&output)?))})
        );
        return Ok(());
    }
    if std::env::args().nth(1).as_deref() == Some("inspect-roots") {
        let file = std::env::args().nth(2).context("Missing inspected RBXL")?;
        let dom = rbx_binary::from_reader(fs::File::open(file)?)?;
        let database = rbx_reflection_database::get()?;
        for id in dom.root().children() {
            let node = dom.get_by_ref(*id).unwrap();
            for name in [
                "ClockTime",
                "TimeOfDay",
                "AutoSimulate",
                "ExpandedTerrain",
                "GravityDirection",
                "PredictiveStreamingMode",
                "SimulationRate",
                "UseInputSink",
                "Wind",
                "WindDirection",
                "PlatformIntegratedChat",
                "EnableVoiceVolumeControls",
            ] {
                if let Some(value) = node.properties.get(&name.into()) {
                    let class = database.classes.get(node.class.as_str());
                    println!(
                        "{}",
                        serde_json::json!({"class":node.class.as_str(),"name":name,"value":value,"descriptor":class.and_then(|c| c.properties.get(name)),"default":class.and_then(|c| database.find_default_property(c,name))})
                    );
                }
            }
        }
        return Ok(());
    }
    let root = Path::new("../../audit/testplace-spans-20260908/3");
    let loaded = std::env::args().nth(1).as_deref() == Some("loaded-source");
    let prefix = if loaded {
        "NativeLoader3"
    } else {
        "NativeLoader2"
    };
    let (source, expected) = if loaded {
        (
            "ReniumTestNativeTimingSource.rbxl",
            "bab910658368939f627d413bbb6b6c76d56bf3a169fcca82195d9e0ba0332237",
        )
    } else {
        (
            "Pulled.rbxl",
            "5b720d9d032326b533b1ab465652e0b36662a4ac18486612fe0b822b791c6e0d",
        )
    };
    let bytes = fs::read(root.join(source))?;
    anyhow::ensure!(
        format!("{:x}", Sha256::digest(&bytes)) == expected,
        "Unexpected source fixture"
    );
    let mut dom = rbx_binary::from_reader(bytes.as_slice())?;
    let workspace = dom
        .root()
        .children()
        .iter()
        .copied()
        .find(|id| dom.get_by_ref(*id).unwrap().class == "Workspace")
        .context("Missing Workspace")?;
    let viewport = match dom
        .get_by_ref(workspace)
        .unwrap()
        .properties
        .get(&"CurrentCamera".into())
    {
        Some(Variant::Ref(id)) => Some(*id),
        _ => None,
    };
    anyhow::ensure!(
        viewport.is_some(),
        "Fixture has no actual CurrentCamera role metadata"
    );
    let mut retained = Vec::new();
    for service_id in dom.root().children().to_vec() {
        let service = dom.get_by_ref(service_id).unwrap().class.to_string();
        for id in dom.get_by_ref(service_id).unwrap().children().to_vec() {
            let node = dom.get_by_ref(id).unwrap();
            // Mirrors the current retained-container policy only for this audit fixture.
            let engine_owned = matches!(
                (service.as_str(), node.class.as_str()),
                ("Workspace", "Terrain")
                    | (
                        "StarterPlayer",
                        "StarterPlayerScripts" | "StarterCharacterScripts"
                    )
                    | (
                        "TextChatService",
                        "ChatWindowConfiguration"
                            | "ChatInputBarConfiguration"
                            | "BubbleChatConfiguration"
                            | "ChannelTabsConfiguration"
                    )
            );
            let plugin_owned = service == "TestService"
                && node.name == "LuauLSP_Settings"
                && node.class == "ModuleScript";
            if !engine_owned && !plugin_owned && Some(id) != viewport {
                continue;
            }
            let payload = if node.children().is_empty() {
                None
            } else {
                let name = format!("{prefix}{}Children.rbxm", node.class);
                let file = fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(root.join(&name))?;
                rbx_binary::to_writer(BufWriter::new(file), &dom, node.children())?;
                Some(name)
            };
            retained.push(serde_json::json!({"path":[service,node.name],"class":node.class.as_str(),"payload":payload,"properties":node.properties}));
            dom.destroy(id);
        }
    }
    if viewport.is_some() {
        dom.get_by_ref_mut(workspace)
            .unwrap()
            .properties
            .remove(&"CurrentCamera".into());
    }
    let manifest = root.join(format!("{prefix}Retained.json"));
    serde_json::to_writer_pretty(
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(manifest)?,
        &retained,
    )?;
    let output = root.join(format!("{prefix}Input.rbxl"));
    let file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&output)?;
    let mut writer = BufWriter::new(file);
    rbx_binary::to_writer(&mut writer, &dom, dom.root().children())?;
    drop(writer);
    println!(
        "{}",
        serde_json::json!({"retainedOutsideProbe":retained.iter().map(|r| &r["path"]).collect::<Vec<_>>(),"path":output,"sha256":format!("{:x}", Sha256::digest(fs::read(&output)?)),"prepareMs":started.elapsed().as_secs_f64()*1000.})
    );
    Ok(())
}

fn inspect_repetition(started: Instant) -> Result<()> {
    use std::collections::{HashMap, HashSet, hash_map::DefaultHasher};
    use std::hash::{Hash, Hasher};
    let input = std::env::args().nth(2).context("Missing RBXL")?;
    let dom = rbx_binary::from_reader(fs::File::open(input)?)?;
    let order = dom
        .descendants()
        .skip(1)
        .map(|n| n.referent())
        .collect::<Vec<_>>();
    let mut hashes = HashMap::<_, u64>::new();
    let mut sizes = HashMap::<_, usize>::new();
    let mut groups = HashMap::<u64, Vec<_>>::new();
    for id in order.iter().rev() {
        let node = dom.get_by_ref(*id).unwrap();
        let mut hash = DefaultHasher::new();
        node.class.hash(&mut hash);
        let mut properties = node.properties.iter().collect::<Vec<_>>();
        properties.sort_unstable_by_key(|(name, _)| name.as_str());
        for (name, value) in properties {
            if matches!(name.as_str(), "UniqueId" | "HistoryId" | "ScriptGuid") {
                continue;
            }
            name.hash(&mut hash);
            if matches!(value, Variant::Ref(_)) {
                // Upper bound only: reference remapping must be proved before cloning.
                0u8.hash(&mut hash);
            } else {
                serde_json::to_vec(value)?.hash(&mut hash);
            }
        }
        let mut size = 1;
        for child in node.children() {
            dom.get_by_ref(*child).unwrap().name.hash(&mut hash);
            hashes[child].hash(&mut hash);
            size += sizes[child];
        }
        let signature = hash.finish();
        hashes.insert(*id, signature);
        sizes.insert(*id, size);
        groups.entry(signature).or_default().push(*id);
    }
    let mut repeats = groups
        .values()
        .filter(|v| v.len() > 1 && sizes[&v[0]] >= 8)
        .collect::<Vec<_>>();
    repeats.sort_unstable_by_key(|v| std::cmp::Reverse(sizes[&v[0]]));
    let mut covered = HashSet::new();
    let mut saved = 0;
    let mut operations = 0;
    let mut top = Vec::new();
    for group in repeats {
        let candidates = group
            .iter()
            .filter(|id| !covered.contains(*id))
            .copied()
            .collect::<Vec<_>>();
        if candidates.len() < 2 {
            continue;
        }
        let size = sizes[&candidates[0]];
        saved += (candidates.len() - 1) * size;
        operations += candidates.len() - 1;
        if top.len() < 20 {
            let first = dom.get_by_ref(candidates[0]).unwrap();
            top.push(serde_json::json!({"name":first.name,"class":first.class.as_str(),"size":size,"copies":candidates.len()}));
        }
        let mut pending = candidates;
        while let Some(id) = pending.pop() {
            if covered.insert(id) {
                pending.extend(dom.get_by_ref(id).unwrap().children());
            }
        }
    }
    println!(
        "{}",
        serde_json::json!({"instances":order.len(),"cloneUpperBoundInstances":saved,"cloneOperations":operations,"top":top,"ms":started.elapsed().as_secs_f64()*1000.})
    );
    Ok(())
}
