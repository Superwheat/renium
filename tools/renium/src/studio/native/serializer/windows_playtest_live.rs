//! Live reflection reads. No getter, dispatcher, QAction or helper is invoked.
use super::*;

pub(super) fn referenced_objects(
    memory: &ProcessMemory,
    studio: &ModuleEntry,
    pe: &PeImage<'_>,
    operands: &[Value],
) -> Vec<Value> {
    let mut result = Vec::new();
    let globals = operands
        .iter()
        .filter_map(|row| row["ripTarget"].as_u64().map(|rva| rva as usize))
        .collect::<BTreeSet<_>>();
    for global in globals.into_iter().filter(|rva| {
        pe.sections.iter().any(|s| {
            s.characteristics & 0x2000_0000 == 0
                && (s.virtual_address..s.virtual_address + s.virtual_size).contains(rva)
        })
    }) {
        let address = studio.base + global;
        for (kind, object) in [
            ("inline", Some(address)),
            (
                "pointer",
                memory.read_u64(address).ok().map(|value| value as usize),
            ),
        ] {
            if let Some(object) = object
                && let Some(rtti) = read_rtti_type(memory, object, studio.base, studio.size)
            {
                result.push(json!({"globalRva":global,"storage":kind,"object":object,"type":rtti}));
            }
        }
    }
    result
}

const MEMBERS: &[(&str, &[&str])] = &[
    (
        "StudioTestService",
        &[
            "ExecutePlayModeAsync",
            "ExecuteMultiplayerTestAsync",
            "ExecuteRunModeAsync",
            "GetTestArgs",
        ],
    ),
    (
        "DataModel",
        &["PlaceId", "GameId", "JobId", "SetPlaceId", "SetUniverseId"],
    ),
    ("DataStoreService", &["GetDataStore", "GetOrderedDataStore"]),
    (
        "MemoryStoreService",
        &["GetQueue", "GetSortedMap", "GetHashMap"],
    ),
    ("MessagingService", &["PublishAsync", "SubscribeAsync"]),
    ("BadgeService", &["AwardBadgeAsync", "UserHasBadgeAsync"]),
    ("MarketplaceService", &["UserOwnsGamePassAsync"]),
    (
        "TeleportService",
        &["TeleportAsync", "GetLocalPlayerTeleportData"],
    ),
];

fn vtable(
    memory: &ProcessMemory,
    studio: &ModuleEntry,
    layout: &PackageLayout,
    object: usize,
) -> Result<Value> {
    let kind = read_rtti_type(memory, object, studio.base, studio.size)
        .context("Object RTTI unavailable")?;
    let table = memory.read_u64(object)? as usize;
    let locator = memory.read_u64(table - 8)? as usize;
    anyhow::ensure!(
        studio
            .base
            .checked_add(memory.read_u32(locator + 20)? as usize)
            == Some(locator),
        "RTTI locator self RVA disagrees"
    );
    let slots = properties::vtable_methods(memory, studio, layout, table)?.into_iter().enumerate()
        .map(|(slot, target)| json!({"slot":slot,"byteOffset":slot*8,"targetRva":target-studio.base})).collect::<Vec<_>>();
    Ok(
        json!({"type":kind,"object":object,"vtableRva":table-studio.base,"locatorRva":locator-studio.base,
        "offsetToTop":memory.read_u32(locator+4)?,"slots":slots}),
    )
}

pub(super) fn objects(
    memory: &ProcessMemory,
    studio: &ModuleEntry,
    addresses: &[usize],
) -> Result<Value> {
    anyhow::ensure!(
        addresses.len() <= 32,
        "At most 32 live objects may be inspected"
    );
    let layout = package_layout(&studio.path)?;
    let mut rows = Vec::new();
    for &address in addresses {
        let result = (|| -> Result<Value> {
            let mut row = vtable(memory, studio, &layout, address)?;
            let table = memory.read_u64(address)? as usize;
            let mut fields = BTreeSet::new();
            for function in properties::vtable_methods(memory, studio, &layout, table)? {
                if let Ok(code) = properties::dispatch_code(memory, studio, &layout, function) {
                    fields.extend(properties::referenced_fields(&code));
                }
            }
            anyhow::ensure!(
                fields.len() <= 1024,
                "Object referenced-field budget exceeded"
            );
            row["codeReferencedPointers"] = json!(
                fields
                    .into_iter()
                    .filter_map(|offset| {
                        let pointer = memory.read_u64(address.checked_add(offset)?).ok()? as usize;
                        let rtti = read_rtti_type(memory, pointer, studio.base, studio.size)?;
                        Some(json!({"offset":offset,"object":pointer,"type":rtti}))
                    })
                    .collect::<Vec<_>>()
            );
            row["fieldPolicy"] = json!(
                "Only RTTI-bearing pointers read by vtable code; receiver provenance still requires analysis"
            );
            if row["type"] == ".?AVDataModel@RBX@@" {
                row["dataModel"] = outcome(model_evidence(memory, studio, address));
            }
            anyhow::ensure!(
                memory.read_u64(address)? as usize == table,
                "Object vtable changed during discovery"
            );
            Ok(row)
        })();
        rows.push(json!({"address":address,"evidence":outcome(result)}));
    }
    Ok(json!(rows))
}

fn model_evidence(memory: &ProcessMemory, studio: &ModuleEntry, outer: usize) -> Result<Value> {
    let mut candidates = discover_instance_layout(memory, studio, outer);
    anyhow::ensure!(
        candidates.len() == 1,
        "Live DataModel has {} candidate instance layouts",
        candidates.len()
    );
    let (layout, _, roots) = candidates.remove(0);
    let instance = outer + layout.data_model_instance;
    let identity = ["PlaceId","GameId"].into_iter().map(|name| {
        let value = integer_property_layout(memory,studio,instance,layout,name)
            .and_then(|field| Ok(json!({"offset":field.offset,"width":field.width,"value":read_integer_property(memory,instance,field)?})));
        json!({"property":name,"evidence":outcome(value)})
    }).collect::<Vec<_>>();
    let services = roots
        .into_iter()
        .filter_map(|root| {
            let class = read_instance_class(memory, root.instance, layout)?;
            Some(
                json!({"class":class,"instance":root.instance,"owner":root.owner,
            "ownerValid":valid_owner(memory,root.owner,studio.base,studio.size)}),
            )
        })
        .collect::<Vec<_>>();
    Ok(
        json!({"instance":instance,"instanceOffset":layout.data_model_instance,"identity":identity,"services":services,
        "role":"unproven; correlate controller references and phase snapshots"}),
    )
}

fn descriptor(
    memory: &ProcessMemory,
    studio: &ModuleEntry,
    layout: &PackageLayout,
    model: &ActiveDataModel,
    instance: usize,
    name: &str,
) -> Result<Value> {
    let descriptor = find_class_member_descriptor(memory, instance, model.layout, name)?;
    let mut row = vtable(memory, studio, layout, descriptor)?;
    // Bound function resolution follows descriptor dispatch data flow. Async
    // signatures may be unsupported; retain their entire vtable for analysis.
    let table = memory.read_u64(descriptor)? as usize;
    let mut fields = BTreeSet::new();
    for function in properties::vtable_methods(memory, studio, layout, table)? {
        if let Ok(code) = properties::dispatch_code(memory, studio, layout, function) {
            fields.extend(properties::referenced_fields(&code));
        }
    }
    row["referencedDescriptorFields"] = json!(
        fields
            .into_iter()
            .map(|offset| {
                let value = memory.read_u64(descriptor + offset);
                match value {
                    Ok(value) => {
                        let target = (value as usize).checked_sub(studio.base).filter(|rva| {
                            layout.text.virtual_address <= *rva
                                && *rva < layout.text.virtual_address + layout.text.raw_size
                        });
                        json!({"offset":offset,"value":value,"targetRva":target})
                    }
                    Err(error) => json!({"offset":offset,"reason":format!("{error:#}")}),
                }
            })
            .collect::<Vec<_>>()
    );
    row["memberBinding"] = outcome(
        properties::function_binding(memory, studio, layout, descriptor, table)
            .map(|(field, target)| json!({"descriptorField":field,"targetRva":target-studio.base})),
    );
    let bindings = properties::bindings(memory, studio, descriptor).map(|bindings| {
        bindings
            .into_iter()
            .map(|(field, object)| {
                json!({"descriptorField":field,
            "binding":outcome(vtable(memory, studio, layout, object))})
            })
            .collect::<Vec<_>>()
    });
    row["propertyBindings"] = outcome(bindings.map(|value| json!(value)));
    // Only scalar place/universe values are read. Job/ticket payloads and
    // arbitrary object memory are deliberately absent from the report.
    if matches!(name, "PlaceId" | "GameId") {
        row["scalar"] = outcome(integer_property_layout(memory, studio, instance, model.layout, name)
            .and_then(|field| Ok(json!({"offset":field.offset,"width":field.width,"value":read_integer_property(memory,instance,field)?}))));
    }
    anyhow::ensure!(
        memory.read_u64(descriptor)? as usize == table,
        "Reflection descriptor changed during discovery"
    );
    Ok(row)
}

pub(super) fn reflection(memory: &ProcessMemory, studio: &ModuleEntry, pid: u32) -> Result<Value> {
    let layout = package_layout(&studio.path)?;
    verify_loaded_image(memory, studio, layout.image_stamp)?;
    let title = super::super::super::target_name(pid, "")?;
    let model = active_data_model(pid, memory, studio, layout.data, &title).map_err(|error| {
        anyhow::anyhow!(format!("{error:#}").replace(&title, "<selected-document>"))
    })?;
    let instance = model.outer + model.layout.data_model_instance;
    let mut services = Vec::new();
    for (class, members) in MEMBERS {
        let candidates = if *class == "DataModel" {
            vec![instance]
        } else {
            model
                .roots
                .iter()
                .filter(|root| {
                    read_instance_class(memory, root.instance, model.layout).as_deref()
                        == Some(class)
                })
                .map(|root| root.instance)
                .collect::<Vec<_>>()
        };
        if candidates.len() != 1 {
            services.push(
                json!({"class":class,"candidateCount":candidates.len(),"status":"unresolved",
                "reason":"Service absent or ambiguous; discovery never creates services"}),
            );
            continue;
        }
        let object = candidates[0];
        let rows = members.iter().map(|name| json!({"name":name,"evidence":outcome(descriptor(memory,studio,&layout,&model,object,name))})).collect::<Vec<_>>();
        services.push(json!({"class":class,"instance":object,"vtable":outcome(vtable(memory,studio,&layout,object)),"members":rows}));
    }
    Ok(
        json!({"dataModel":model.outer,"owner":model.owner,"instanceOffset":model.layout.data_model_instance,
        "role":"selected document; Edit/Play role requires snapshot comparison","services":services,
        "consistency":"non-atomic reads; no process suspension or engine calls"}),
    )
}

pub(super) fn command_line(line: &str) -> Value {
    // Tokenize Windows quoting only to recover switches. Never return raw
    // tokens/unknown switch names: they may contain credentials or file names.
    let mut quoted = false;
    let mut token = String::new();
    let mut tokens = Vec::new();
    let mut slashes = 0;
    for character in line.chars().chain(std::iter::once(' ')) {
        if character == '\\' {
            slashes += 1;
            continue;
        }
        if character == '"' {
            token.extend(std::iter::repeat_n('\\', slashes / 2));
            if slashes % 2 == 1 {
                token.push('"');
            } else {
                quoted = !quoted;
            }
        } else {
            token.extend(std::iter::repeat_n('\\', slashes));
            if character.is_whitespace() && !quoted {
                if !token.is_empty() {
                    tokens.push(std::mem::take(&mut token));
                }
            } else {
                token.push(character);
            }
        }
        slashes = 0;
    }
    let mut result = Vec::new();
    for (index, token) in tokens.iter().enumerate().skip(1) {
        let key = token.trim_start_matches('-').to_ascii_lowercase();
        let next = tokens
            .get(index + 1)
            .map(String::as_str)
            .unwrap_or_default();
        let value = match key.as_str() {
            "task" if matches!(next, "StartServer" | "StartClient" | "EditFile") => json!(next),
            "placeid" | "universeid" | "port" | "serverport"
                if !next.is_empty() && next.bytes().all(|b| b.is_ascii_digit()) =>
            {
                json!(next)
            }
            "localplacefile"
            | "placefile"
            | "testargs"
            | "testarguments"
            | "jointicket"
            | "authenticationticket"
            | "ticket"
            | "jobid" => json!({"present":true,"value":"omitted"}),
            "studiotestservicemode" => json!(true),
            _ => continue,
        };
        if token.starts_with('-') {
            result.push(json!({"argument":key,"value":value}));
        }
    }
    json!({"arguments":result,"rawCommandLineOmitted":true,"unknownArgumentsOmitted":true})
}
