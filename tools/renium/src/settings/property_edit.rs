use super::*;

#[cfg(test)]
#[path = "property_edit_sim.rs"]
pub(crate) mod sim;

/// Unedited scalar and fixed numeric columns need no per-row property maps,
/// and a binary payload many rows hold (a collision mesh) is kept once for all
/// of them. Values whose decoded form changes their wire kind use the ordinary
/// codec.
#[derive(Default)]
pub(crate) struct RetainedPropertyColumns {
    columns: Vec<PropertyColumn>,
}

struct PropertyColumn {
    name: String,
    kind: u8,
    values: Vec<(usize, NativeSettingsValue)>,
}

pub(crate) fn read_for_property_edit(
    path: &Path,
) -> Result<(SettingsBytecode, RetainedPropertyColumns)> {
    read_settings_file(path, decode_for_property_edit)
}

fn decode_for_property_edit(bytes: &[u8]) -> Result<(SettingsBytecode, RetainedPropertyColumns)> {
    let (version, payload) = decode_settings_bytecode_container(bytes)?;
    let budget = DecodeBudget::for_file(bytes.len());
    let mut reader = BytecodeReader::new(&payload);
    let SettingsPayloadHeader {
        strings,
        properties,
        mut instances,
        instance_count,
    } = decode_settings_payload_header(&mut reader, &budget)?;
    let specs = read_settings_property_groups(&mut reader, &properties)?;
    let mut occurrences = AHashMap::<&str, usize>::new();
    for (name, ..) in &specs {
        *occurrences.entry(name.as_str()).or_default() += 1;
    }
    let scope = ValueScope {
        strings: &strings,
        instance_count,
        budget: &budget,
    };
    let groups = specs
        .par_iter()
        .map(|&(name, kind, count, body)| {
            // Separate groups can overwrite the same row. Leave all groups of
            // such a property in the ordered, ordinary decoder.
            // Camera ownership and forced-property normalization inspect these
            // fields even on rows that the command did not select.
            if occurrences[name.as_str()] == 1
                && !matches!(
                    name.as_str(),
                    "CurrentCamera" | "TextScaled" | "TextWrapped"
                )
                && let Some((column, decoded)) =
                    decode_compact_column(body, name, kind, count, scope)?
            {
                return Ok((decoded, Some(column)));
            }
            Ok((
                decode_property_group_body(body, name, kind, count, scope)?,
                None,
            ))
        })
        .collect::<Result<Vec<_>>>()?;
    reader.finish()?;
    let (decoded, columns): (Vec<_>, Vec<_>) = groups.into_iter().unzip();
    apply_decoded_property_groups(&mut instances, &specs, decoded);
    Ok((
        SettingsBytecode { version, instances },
        RetainedPropertyColumns {
            columns: columns.into_iter().flatten().collect(),
        },
    ))
}

fn decode_compact_column(
    body: &[u8],
    name: &str,
    kind: u8,
    count: usize,
    scope: ValueScope<'_>,
) -> Result<Option<(PropertyColumn, Vec<DecodedGroupEntry>)>> {
    if kind == 8 && name != "Attributes" {
        return decode_payload_column(body, name, count, scope).map(Some);
    }
    if !matches!(kind, 1..=3 | 5 | 6 | 11..=20) {
        return Ok(None);
    }
    scope
        .budget
        .take(count.saturating_mul(VALUE_SLOT_BYTES + name.len() + value_kind_bytes(kind)))?;
    let mut reader = BytecodeReader::new(body);
    let rotations = decode_cframe_group_header(kind, &mut reader)?;
    let mut values = Vec::with_capacity(reader.capacity(count, 1));
    let mut decoded = Vec::new();
    let mut index = 0;
    let mut target = 0;
    for position in 0..count {
        index = next_group_instance(index, reader.read_len("property instance index")?, position)?;
        if index >= scope.instance_count {
            bail!("Invalid property instance index {index}");
        }
        let value = match kind {
            1 | 2 => NativeSettingsValue::Bool(kind == 2),
            3 => NativeSettingsValue::Int(unzigzag_i64(reader.read_var_u64()?)),
            5 => NativeSettingsValue::Float64(read_f64(&mut reader)?),
            6 => NativeSettingsValue::String(scope.string(&mut reader, "value string id")?),
            17 => NativeSettingsValue::Ref(decode_resolved_ref_target(
                &mut reader,
                scope.instance_count,
                &mut target,
            )?),
            19 => NativeSettingsValue::Enum(scope.string(&mut reader, "enum item string id")?),
            20 => NativeSettingsValue::Float32(read_f32(&mut reader)?),
            _ => decode_fixed_column_value(&mut reader, kind, &rotations)?,
        };
        let canonical = native_value_kind(&value) == kind
            && match &value {
                NativeSettingsValue::Float32(value) => value.is_finite(),
                NativeSettingsValue::Float64(value) => value.is_finite(),
                value => native_fixed_components(value)
                    .is_none_or(|values| values.iter().all(|value| value.is_finite())),
            };
        if canonical {
            values.push((index, value));
        } else {
            decoded.push(DecodedGroupEntry::Property(
                index,
                materialized_value(&value, kind)?,
            ));
        }
    }
    reader.finish()?;
    Ok(Some((
        PropertyColumn {
            name: name.to_string(),
            kind,
            values,
        },
        decoded,
    )))
}

/// From this size on the text of a binary payload is held once for all the
/// rows that have it.
const SHARED_PAYLOAD_MIN_BYTES: usize = if cfg!(test) { 4 } else { 256 };

/// A column of complex values: a binary payload (`_type` and `base64`, and
/// nothing else) of some size becomes one shared value for every row that
/// holds it, any other value goes to its row as the ordinary decoder gives it.
fn decode_payload_column(
    body: &[u8],
    name: &str,
    count: usize,
    scope: ValueScope<'_>,
) -> Result<(PropertyColumn, Vec<DecodedGroupEntry>)> {
    scope.budget.property_group(count, name, 8)?;
    let mut reader = BytecodeReader::new(body);
    decode_cframe_group_header(8, &mut reader)?;
    let mut held = AHashMap::<(usize, u64), Vec<std::sync::Arc<Value>>>::new();
    let (mut values, mut decoded) = (Vec::new(), Vec::new());
    let mut index = 0;
    for position in 0..count {
        index = next_group_instance(index, reader.read_len("property instance index")?, position)?;
        if index >= scope.instance_count {
            bail!("Invalid property instance index {index}");
        }
        let value = decode_raw_value_payload(&mut reader, 8, scope, 0)?;
        let Some(text) =
            binary_payload_text(&value).filter(|text| text.len() >= SHARED_PAYLOAD_MIN_BYTES)
        else {
            decoded.push(DecodedGroupEntry::Property(index, value));
            continue;
        };
        // Told apart by length and a sample of the text, then by the value.
        let sample = text
            .bytes()
            .step_by(text.len() / 8 + 1)
            .fold(0_u64, |sample, byte| {
                sample.wrapping_mul(0x100_0000_01b3) ^ u64::from(byte)
            });
        let alike = held.entry((text.len(), sample)).or_default();
        let shared = match alike.iter().find(|held| ***held == value) {
            Some(held) => std::sync::Arc::clone(held),
            None => {
                alike.push(std::sync::Arc::new(value));
                std::sync::Arc::clone(alike.last().context("Expected the value just held")?)
            }
        };
        values.push((index, NativeSettingsValue::Shared(shared)));
    }
    reader.finish()?;
    Ok((
        PropertyColumn {
            name: name.to_string(),
            kind: 8,
            values,
        },
        decoded,
    ))
}

/// The text of a value that is a binary payload and nothing more.
fn binary_payload_text(value: &Value) -> Option<&str> {
    let object = value.as_object()?;
    (object.len() == 2 && object.get("_type")?.is_string())
        .then(|| object.get("base64")?.as_str())
        .flatten()
}

fn decode_fixed_column_value(
    reader: &mut BytecodeReader<'_>,
    kind: u8,
    rotations: &CFrameGroupDecoder,
) -> Result<NativeSettingsValue> {
    let fixed = fixed_numeric_kind_from_tag(kind).context("Expected fixed numeric kind")?;
    let mut components = [0.0_f32; 12];
    let width = match rotations {
        CFrameGroupDecoder::Inline => fixed_numeric_len(fixed),
        CFrameGroupDecoder::RotationTable(_) => 3,
    };
    for component in &mut components[..width] {
        *component = read_f32(reader)?;
    }
    if let CFrameGroupDecoder::RotationTable(table) = rotations {
        let rotation_index = reader.read_len("CFrame rotation index")?;
        let rotation = table
            .get(rotation_index)
            .with_context(|| format!("Invalid CFrame rotation index {rotation_index}"))?;
        for (target, source) in components[3..].iter_mut().zip(rotation) {
            *target = *source as f32;
        }
    }
    Ok(match fixed {
        FixedNumericKind::Vector2 => NativeSettingsValue::Vector2([components[0], components[1]]),
        FixedNumericKind::Vector3 => NativeSettingsValue::Vector3(components[..3].try_into()?),
        FixedNumericKind::UDim => NativeSettingsValue::UDim([components[0], components[1]]),
        FixedNumericKind::UDim2 => NativeSettingsValue::UDim2(components[..4].try_into()?),
        FixedNumericKind::Color3 => NativeSettingsValue::Color3(components[..3].try_into()?),
        FixedNumericKind::CFrame => NativeSettingsValue::CFrame(components),
        FixedNumericKind::Rect => NativeSettingsValue::Rect(components[..4].try_into()?),
    })
}

impl RetainedPropertyColumns {
    /// Call before editing: all selected rows become complete, while the
    /// remaining columns keep their indices. Only value edits may follow.
    pub(crate) fn materialize(
        &mut self,
        document: &mut SettingsBytecode,
        indices: &[usize],
    ) -> Result<()> {
        let mut selected = vec![false; document.instances.len()];
        for &index in indices {
            selected[index] = true;
        }
        for column in &mut self.columns {
            let mut result = Ok(());
            column.values.retain(|(index, value)| {
                if !selected[*index] {
                    return true;
                }
                match materialized_value(value, column.kind) {
                    Ok(value) => {
                        document.instances[*index]
                            .properties
                            .insert(column.name.clone(), value);
                    }
                    Err(error) => result = Err(error),
                }
                false
            });
            result?;
        }
        Ok(())
    }

    pub(crate) fn encode(&self, document: &SettingsBytecode) -> Result<Vec<u8>> {
        validate_settings_hierarchy(&document.instances)?;
        validate_settings_record_properties(&document.instances)?;
        let canonical = canonical_attribute_document(document);
        let document = canonical.as_ref().unwrap_or(document);
        let settled = forced_properties_settled_document(document);
        let document = settled.as_ref().unwrap_or(document);
        let lookup = build_bytecode_instance_lookup(document);
        let mut collected = collect_settings_bytecode_data(document, &lookup)?;
        for column in &self.columns {
            let mut values = Vec::with_capacity(column.values.len());
            for (index, value) in &column.values {
                // Forced properties (TextScaled implies TextWrapped) can replace
                // an unedited value during normalization too.
                if document.instances[*index]
                    .properties
                    .contains_key(&column.name)
                {
                    continue;
                }
                if let NativeSettingsValue::Shared(shared) = value {
                    // Written as the JSON value it holds, as its row would be.
                    let source = SettingsBinaryValueSource::of_property(shared);
                    anyhow::ensure!(
                        binary_source_value_kind(&source, &lookup)? == column.kind,
                        "A retained payload of {} changed its wire kind",
                        column.name
                    );
                    collect_binary_source_strings(&source, &lookup, &mut collected.string_counts)?;
                    values.push(SettingsBinaryValue {
                        instance_index: *index,
                        source,
                    });
                    continue;
                }
                collect_native_value_strings(value, &mut collected.string_counts);
                values.push(SettingsBinaryValue {
                    instance_index: *index,
                    source: SettingsBinaryValueSource::Native(value),
                });
            }
            if !values.is_empty() {
                add_count_key(
                    &mut collected.string_counts,
                    Cow::Borrowed(&column.name),
                    values.len() as u64,
                );
                add_count_key(
                    &mut collected.property_counts,
                    Cow::Borrowed(&column.name),
                    values.len() as u64,
                );
                let group = collected
                    .property_groups
                    .entry((&column.name, column.kind))
                    .or_default();
                if group.is_empty() {
                    *group = values;
                } else {
                    group.append(&mut values);
                    group.sort_unstable_by_key(|value| value.instance_index);
                }
            }
        }
        let payload = encode_settings_bytecode_payload_collected(document, &lookup, collected)?;
        wrap_settings_bytecode_payload(&payload)
    }
}

fn materialized_value(value: &NativeSettingsValue, kind: u8) -> Result<Value> {
    match value {
        NativeSettingsValue::String(value) => return Ok(json!(value)),
        NativeSettingsValue::Shared(value) => return Ok(Value::clone(value)),
        NativeSettingsValue::Enum(value) => {
            return Ok(typed_object("EnumItem", [("name", json!(value))]));
        }
        NativeSettingsValue::Ref(index) => {
            return Ok(typed_object("Ref", [("instanceIndex", json!(index + 1))]));
        }
        _ => {}
    }
    let mut bytes = Vec::with_capacity(48);
    write_native_value_payload(value, kind, &SettingsStringIdMap::default(), &mut bytes)?;
    let budget = DecodeBudget::new(usize::MAX);
    let scope = ValueScope {
        strings: &[],
        instance_count: 0,
        budget: &budget,
    };
    decode_raw_value_payload(&mut BytecodeReader::new(&bytes), kind, scope, 0)
}
