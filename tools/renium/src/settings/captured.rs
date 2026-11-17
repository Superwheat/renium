use super::*;

#[derive(Clone, Copy)]
struct ConversionScope<'a, 'b> {
    lookup: &'a SettingsBinaryInstanceLookup<'b>,
    instance_count: usize,
}

pub(super) fn convert(state: &ServiceState) -> Result<SettingsBytecode> {
    let _span = crate::app::profile::span("captured store document");
    let records = collect_service_settings_binary_instances(state);
    if records.len() > MAX_SETTINGS_COLLECTION_ITEMS {
        bail!("instance count exceeds safe collection limit");
    }
    let lookup = build_settings_binary_instance_lookup(state, &records);
    let scope = ConversionScope {
        lookup: &lookup,
        instance_count: records.len(),
    };
    let instances = if records.len() < SETTINGS_BINARY_PARALLEL_MIN_INSTANCES
        || rayon::current_num_threads() <= 1
    {
        convert_chunk(state, &records, scope)?
    } else {
        records
            .par_chunks(SETTINGS_BINARY_PARALLEL_CHUNK_SIZE)
            .map(|records| convert_chunk(state, records, scope))
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .flatten()
            .collect()
    };
    let document = SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances,
    };
    validate_settings_hierarchy(&document.instances)?;
    Ok(document)
}

fn convert_chunk<'a>(
    state: &'a ServiceState,
    records: &[SettingsBinaryInstance<'a>],
    scope: ConversionScope<'_, '_>,
) -> Result<Vec<SettingsBytecodeInstance>> {
    let mut sources = Vec::new();
    let mut scratch = Vec::new();
    let mut instances = Vec::with_capacity(records.len());
    for record in records {
        let id = match &record.settings_id {
            SettingsBinaryId::Text(text) => checked_string(text)?,
            SettingsBinaryId::NumericDebug(id) => format!("debug:0_{id}"),
        };
        let mut instance = SettingsBytecodeInstance::new(
            id,
            checked_string(record.name)?,
            checked_string(record.class_name)?,
            record.parent_index,
        );
        visit_settings_binary_properties(state, record, scope.lookup, |name, kind, source| {
            // A decoder discards these columns, including native capture fields.
            if !SETTINGS_RECORD_FIELDS.contains(&name) {
                sources.push((name, kind, source));
            }
            Ok(())
        })?;
        // Different wire kinds of one name overwrite in ascending kind order.
        // Repeating a row within the same group is rejected by the decoder.
        sources.sort_unstable_by(|left, right| (left.0, left.1).cmp(&(right.0, right.1)));
        if sources
            .windows(2)
            .any(|pair| (pair[0].0, pair[0].1) == (pair[1].0, pair[1].1))
        {
            bail!("Settings bytecode gives one instance two values of a property");
        }
        for (name, kind, source) in sources.drain(..) {
            if name == "Attributes" && kind == 8 {
                let Value::Object(attributes) =
                    normalize_complex(&source, kind, true, scope, &mut scratch)?
                else {
                    bail!("Expected decoded attributes");
                };
                instance.attributes = attributes;
            } else {
                let value = normalize_source(&source, kind, scope, &mut scratch)?;
                instance.properties.insert(checked_string(name)?, value);
            }
        }
        instances.push(instance);
    }
    Ok(instances)
}

fn checked_string(text: &str) -> Result<String> {
    if text.len() > MAX_SETTINGS_STRING_BYTES {
        bail!("Settings string exceeds safe size limit of {MAX_SETTINGS_STRING_BYTES} bytes");
    }
    Ok(text.to_string())
}

fn normalize_source(
    source: &SettingsBinaryValueSource<'_>,
    kind: u8,
    scope: ConversionScope<'_, '_>,
    scratch: &mut Vec<u8>,
) -> Result<Value> {
    match source {
        SettingsBinaryValueSource::Native(value) => native_value(value, scope.instance_count),
        SettingsBinaryValueSource::Property(value) => {
            property_value(value, source, kind, scope, scratch)
        }
        SettingsBinaryValueSource::Settled(value) => {
            property_value(value, source, kind, scope, scratch)
        }
        SettingsBinaryValueSource::Attributes(_) => {
            normalize_complex(source, kind, true, scope, scratch)
        }
    }
}

fn property_value(
    value: &Value,
    source: &SettingsBinaryValueSource<'_>,
    kind: u8,
    scope: ConversionScope<'_, '_>,
    scratch: &mut Vec<u8>,
) -> Result<Value> {
    match kind {
        0..=5 | 20 => Ok(value.clone()),
        6 => Ok(Value::String(checked_string(
            raw_string_payload(value).context("Expected string binary settings value")?,
        )?)),
        19 => Ok(typed_object(
            "EnumItem",
            [(
                "name",
                Value::String(checked_string(
                    raw_string_payload(value).context("Expected enum binary settings value")?,
                )?),
            )],
        )),
        9 | 21 => {
            let values = value
                .as_array()
                .context("Expected numeric array binary settings value")?;
            if values.len() > MAX_SETTINGS_COLLECTION_ITEMS {
                bail!("numeric array length exceeds safe collection limit");
            }
            Ok(Value::Array(
                values
                    .iter()
                    .map(|value| {
                        settings_number(value)
                            .map(json_number_f64)
                            .context("Expected numeric array item")
                    })
                    .collect::<Result<_>>()?,
            ))
        }
        11..=16 | 18 => {
            let fixed = fixed_numeric_kind_from_tag(kind).context("Expected fixed numeric kind")?;
            if kind == 16 {
                return Ok(fixed_value(
                    fixed,
                    &cframe_component_bits(value)?.map(f32::from_bits),
                ));
            }
            let fields = fixed_fields(fixed).1;
            let object = value
                .as_object()
                .context("Expected fixed numeric binary settings value")?;
            let mut components = [0_f32; 4];
            for (index, field) in fields.iter().enumerate() {
                components[index] = fixed_numeric_component_f32(
                    settings_number(object.get(*field).unwrap_or(&Value::Null))
                        .context("Expected numeric fixed component")?,
                );
            }
            Ok(fixed_value(fixed, &components[..fields.len()]))
        }
        17 => {
            let reference =
                ref_payload_object(value).context("Expected Ref binary settings value")?;
            let index = resolve_ref_index(reference, scope.lookup)?
                .context("Expected resolved Ref binary settings value")?;
            reference_value(index, scope.instance_count)
        }
        _ => normalize_complex(source, kind, false, scope, scratch),
    }
}

fn native_value(value: &NativeSettingsValue, instance_count: usize) -> Result<Value> {
    match value {
        NativeSettingsValue::Bool(value) => Ok(json!(value)),
        NativeSettingsValue::Int(value) => Ok(json!(value)),
        NativeSettingsValue::Float32(value) => Ok(json_number_f64(f64::from(*value))),
        NativeSettingsValue::Float64(value) => Ok(json_number_f64(*value)),
        NativeSettingsValue::String(value) => Ok(Value::String(checked_string(value)?)),
        NativeSettingsValue::Enum(value) => Ok(typed_object(
            "EnumItem",
            [("name", Value::String(checked_string(value)?))],
        )),
        NativeSettingsValue::Ref(index) => reference_value(*index, instance_count),
        NativeSettingsValue::Vector2(_)
        | NativeSettingsValue::Vector3(_)
        | NativeSettingsValue::UDim(_)
        | NativeSettingsValue::UDim2(_)
        | NativeSettingsValue::Color3(_)
        | NativeSettingsValue::Rect(_)
        | NativeSettingsValue::CFrame(_) => {
            let kind = fixed_numeric_kind_from_tag(native_value_kind(value))
                .context("Expected fixed native value")?;
            Ok(fixed_value(
                kind,
                native_fixed_components(value).context("Expected native components")?,
            ))
        }
    }
}

fn reference_value(index: usize, instance_count: usize) -> Result<Value> {
    if index >= instance_count {
        bail!("Invalid resolved ref index {index}");
    }
    Ok(typed_object("Ref", [("instanceIndex", json!(index + 1))]))
}

fn fixed_fields(kind: FixedNumericKind) -> (&'static str, &'static [&'static str]) {
    match kind {
        FixedNumericKind::Vector2 => ("Vector2", &["x", "y"]),
        FixedNumericKind::Vector3 => ("Vector3", &["x", "y", "z"]),
        FixedNumericKind::UDim => ("UDim", &["scale", "offset"]),
        FixedNumericKind::UDim2 => ("UDim2", &["xScale", "xOffset", "yScale", "yOffset"]),
        FixedNumericKind::Color3 => ("Color3", &["r", "g", "b"]),
        FixedNumericKind::CFrame => ("CFrame", &[]),
        FixedNumericKind::Rect => ("Rect", &["minX", "minY", "maxX", "maxY"]),
    }
}

fn fixed_value(kind: FixedNumericKind, components: &[f32]) -> Value {
    let (name, fields) = fixed_fields(kind);
    if matches!(kind, FixedNumericKind::CFrame) {
        return typed_object(
            name,
            [(
                "components",
                Value::Array(
                    components
                        .iter()
                        .map(|value| json_number_f64(f64::from(*value)))
                        .collect(),
                ),
            )],
        );
    }
    let mut object = Map::new();
    object.insert("_type".into(), json!(name));
    for (field, value) in fields.iter().zip(components) {
        object.insert((*field).into(), json_number_f64(f64::from(*value)));
    }
    Value::Object(object)
}

fn normalize_complex(
    source: &SettingsBinaryValueSource<'_>,
    kind: u8,
    attributes: bool,
    scope: ConversionScope<'_, '_>,
    scratch: &mut Vec<u8>,
) -> Result<Value> {
    // Complex and extensible forms keep the codec's normalization rules in one
    // place. Only this value uses scratch wire data; no store is built, sorted
    // or compressed, and each worker reuses its buffer.
    let mut counts = SettingsStringCounts::default();
    collect_binary_source_strings(source, scope.lookup, &mut counts)?;
    let strings = counts
        .into_keys()
        .map(|value| checked_string(&value))
        .collect::<Result<Vec<_>>>()?;
    let ids = strings
        .iter()
        .enumerate()
        .map(|(index, text)| (text.as_str(), index as u64))
        .collect();
    scratch.clear();
    write_binary_source_payload(source, kind, &ids, scope.lookup, scratch)?;
    let budget = DecodeBudget::new(usize::MAX);
    let scope = ValueScope {
        strings: &strings,
        instance_count: scope.instance_count,
        budget: &budget,
    };
    let mut reader = BytecodeReader::new(scratch);
    let value = if attributes {
        Value::Object(decode_attributes_payload(&mut reader, scope)?)
    } else {
        decode_raw_value_payload(&mut reader, kind, scope, 0)?
    };
    reader.finish()?;
    Ok(value)
}
