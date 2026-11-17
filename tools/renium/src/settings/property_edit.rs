use super::*;

#[cfg(test)]
#[path = "property_edit_sim.rs"]
pub(crate) mod sim;

/// Unedited scalar and fixed numeric columns need no per-row property maps.
/// Values whose decoded form changes their wire kind use the ordinary codec.
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
