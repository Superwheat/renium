//! Retain the store's encoded columns while materializing consecutive row ranges.
//! Values use the ordinary decoder; chunk boundaries never reset group state.
use super::*;
use std::ops::Range;

struct RowGroup {
    name: String,
    kind: u8,
    count: usize,
    bytes: Vec<u8>,
    first_value: usize,
    rotations: CFrameGroupDecoder,
}

#[test]
fn consumed_columns_release_their_bytes_without_resetting_deltas() -> Result<()> {
    let document = SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances: (0..2000)
            .map(|index| {
                let mut row = SettingsBytecodeInstance::new(
                    format!("id-{index}"),
                    format!("row-{index}"),
                    "Folder".into(),
                    (index > 0).then_some(0),
                );
                row.properties.insert("Value".into(), json!(index));
                row.properties.insert(
                    "Target".into(),
                    json!({"_type":"Ref", "instanceIndex":2000-index}),
                );
                row
            })
            .collect(),
    };
    let bytes = encode_settings_bytecode(&document)?;
    let expected = decode_settings_bytecode_holding(&bytes)?;
    let (mut actual, mut columns) = decode_for_row_chunks(&bytes)?;
    let initial = columns
        .groups
        .iter()
        .map(|group| group.bytes.len())
        .sum::<usize>();
    for range in [0..100, 100..1600, 1600..2000] {
        columns.materialize(&mut actual, range.clone())?;
        assert_eq!(
            actual.instances[range.clone()],
            expected.instances[range.clone()]
        );
        if range.end == 1600 {
            let remaining = columns
                .groups
                .iter()
                .map(|group| group.bytes.len())
                .sum::<usize>();
            assert!(
                remaining < initial / 2,
                "consumed column bytes stayed alive"
            );
        }
    }
    assert!(columns.groups.iter().all(|group| group.bytes.is_empty()));
    assert!(
        columns
            .project_properties(&mut actual, |_| true, |_, _, value| Some(value))
            .is_err()
    );
    columns.finish()
}

#[derive(Default)]
struct GroupCursor {
    byte: usize,
    value: usize,
    instance: usize,
    reference: usize,
}

impl GroupCursor {
    fn beginning(group: &RowGroup) -> Self {
        Self {
            byte: group.first_value,
            ..Default::default()
        }
    }

    fn visit_until(
        &mut self,
        group: &RowGroup,
        end: usize,
        scope: ValueScope<'_>,
        mut visit: impl FnMut(DecodedGroupEntry) -> Result<()>,
    ) -> Result<()> {
        let body = &group.bytes;
        let mut reader = BytecodeReader::new(&body[self.byte..]);
        while self.value < group.count {
            let before = reader.position();
            let index = next_group_instance(
                self.instance,
                reader.read_len("property instance index")?,
                self.value,
            )?;
            anyhow::ensure!(
                index < scope.instance_count,
                "Invalid property instance index {index}"
            );
            if index >= end {
                // Leave this row's delta with its value for the next chunk.
                self.byte += before;
                return Ok(());
            }
            let entry = decode_property_group_entry(
                &mut reader,
                &group.name,
                group.kind,
                index,
                scope,
                &group.rotations,
                &mut self.reference,
            )?;
            self.instance = index;
            self.value += 1;
            visit(entry)?;
        }
        self.byte += reader.position();
        reader.finish()
    }
}

pub(crate) struct RetainedRowColumns {
    strings: Vec<String>,
    groups: Vec<RowGroup>,
    cursors: Vec<GroupCursor>,
    budget: DecodeBudget,
    file_bytes: usize,
    instance_count: usize,
    next_row: usize,
}

/// Read structure and retain the encoded value columns. The normal decoder's
/// budget, index checks, value validation and last-group-wins order still apply.
pub(crate) fn decode_for_row_chunks(
    bytes: &[u8],
) -> Result<(SettingsBytecode, RetainedRowColumns)> {
    let _span = crate::app::profile::span("store retained row columns");
    let (version, payload) = decode_settings_bytecode_container(bytes)?;
    let budget = DecodeBudget::for_file(bytes.len()).holding_payloads();
    let mut reader = BytecodeReader::new(&payload);
    let SettingsPayloadHeader {
        strings,
        properties,
        instances,
        instance_count,
    } = decode_settings_payload_header(&mut reader, &budget)?;
    let specs = read_settings_property_groups(&mut reader, &properties)?;
    let groups = specs
        .into_iter()
        .map(|(name, kind, count, body)| {
            budget.property_group(count, name, kind)?;
            let mut values = BytecodeReader::new(body);
            let rotations = decode_cframe_group_header(kind, &mut values)?;
            Ok(RowGroup {
                name: name.clone(),
                kind,
                count,
                // The header's strings and structural rows are already owned.
                // Keeping the container would retain a second copy of them.
                bytes: body.to_vec(),
                first_value: values.position(),
                rotations,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    reader.finish()?;
    let cursors = groups.iter().map(GroupCursor::beginning).collect();
    Ok((
        SettingsBytecode { version, instances },
        RetainedRowColumns {
            strings,
            groups,
            cursors,
            budget,
            file_bytes: bytes.len(),
            instance_count,
            next_row: 0,
        },
    ))
}

impl RetainedRowColumns {
    /// Only the requested range receives maps. The caller may consume those
    /// maps before the next range; structural row order must remain unchanged.
    pub(crate) fn materialize(
        &mut self,
        document: &mut SettingsBytecode,
        rows: Range<usize>,
    ) -> Result<()> {
        anyhow::ensure!(
            document.instances.len() == self.instance_count,
            "Store structure changed during row decoding; restart the store read"
        );
        self.check_range(&rows)?;
        self.materialize_into(&mut document.instances[rows.clone()], rows)
    }

    /// The same decoder without borrowing structural rows. A consumer can
    /// convert the preceding range while this materializes the next one.
    pub(crate) fn materialize_values(
        &mut self,
        rows: Range<usize>,
    ) -> Result<Vec<DecodedRowValues>> {
        self.check_range(&rows)?;
        let mut values = (0..rows.len())
            .map(|_| DecodedRowValues::default())
            .collect::<Vec<_>>();
        self.materialize_into(&mut values, rows)?;
        Ok(values)
    }

    fn check_range(&self, rows: &Range<usize>) -> Result<()> {
        anyhow::ensure!(
            rows.start == self.next_row && rows.end <= self.instance_count && rows.end > rows.start,
            "Store row chunks must be consecutive and within its structure; restart the store read"
        );
        Ok(())
    }

    fn materialize_into<T: DecodedPropertyTarget>(
        &mut self,
        instances: &mut [T],
        rows: Range<usize>,
    ) -> Result<()> {
        let _span = crate::app::profile::span("store row chunk materialize");
        let scope = ValueScope {
            strings: &self.strings,
            instance_count: self.instance_count,
            budget: &self.budget,
        };
        let groups = self
            .groups
            .par_iter_mut()
            .zip(self.cursors.par_iter_mut())
            .map(|(group, cursor)| {
                let mut entries = Vec::with_capacity(rows.len().min(group.count));
                cursor.visit_until(group, rows.end, scope, |entry| {
                    entries.push(entry);
                    Ok(())
                })?;
                // Release consumed prefixes geometrically. The total bytes
                // moved are bounded by twice the original column size,
                // independent of the row chunk size.
                // Cursor deltas and CFrame state survive the buffer movement.
                if cursor.byte >= group.bytes.len().div_ceil(2) {
                    group.bytes.drain(..cursor.byte);
                    group.bytes.shrink_to_fit();
                    cursor.byte = 0;
                }
                Ok(entries)
            })
            .collect::<Result<Vec<_>>>()?;
        let specs = self
            .groups
            .iter()
            .map(|group| (&group.name, group.kind, group.count, group.bytes.as_slice()))
            .collect::<Vec<_>>();
        // Schedule only this range, with enough partitions for a bounded range
        // to use the worker pool. Group order still decides repeated values.
        apply_decoded_property_groups_in_range(instances, rows.start, &specs, groups, 512);
        self.next_row = rows.end;
        Ok(())
    }

    /// Project small cross-row facts before consuming values, without retaining
    /// any other row's values. Projection does not advance the main read cursors.
    pub(crate) fn project_properties(
        &self,
        document: &mut SettingsBytecode,
        names: impl Fn(&str) -> bool,
        project: impl Fn(&SettingsBytecodeInstance, &str, Value) -> Option<Value>,
    ) -> Result<()> {
        anyhow::ensure!(
            self.next_row == 0,
            "Store context must be projected before reading rows; restart the import before writing to Studio"
        );
        let budget = DecodeBudget::for_file(self.file_bytes).holding_payloads();
        let scope = ValueScope {
            strings: &self.strings,
            instance_count: self.instance_count,
            budget: &budget,
        };
        for group in self.groups.iter().filter(|group| names(&group.name)) {
            budget.property_group(group.count, &group.name, group.kind)?;
            let mut cursor = GroupCursor::beginning(group);
            cursor.visit_until(group, self.instance_count, scope, |entry| {
                let DecodedGroupEntry::Property(index, value) = entry else {
                    return Ok(());
                };
                let value = project(&document.instances[index], &group.name, value);
                let properties = &mut document.instances[index].properties;
                if let Some(value) = value {
                    properties.insert(group.name.clone(), value);
                } else {
                    properties.remove(&group.name);
                }
                Ok(())
            })?;
        }
        Ok(())
    }

    /// Read only columns that can carry references. Include their other kinds
    /// too, so a later scalar group still replaces an earlier reference.
    /// Main cursors remain untouched for the chosen full or selected decode.
    pub(crate) fn project_references(
        &self,
        document: &mut SettingsBytecode,
        wanted: &[bool],
    ) -> Result<()> {
        fn references(value: Value, found: &mut Vec<Value>) {
            if ref_payload_object(&value).is_some() {
                found.push(value);
                return;
            }
            match value {
                Value::Array(values) => {
                    for value in values {
                        references(value, found);
                    }
                }
                Value::Object(values) => {
                    for value in values.into_values() {
                        references(value, found);
                    }
                }
                _ => {}
            }
        }
        anyhow::ensure!(
            wanted.len() == self.instance_count
                && document.instances.len() == self.instance_count
                && self.next_row == 0,
            "Reference projection requires the original store structure; restart the store read"
        );
        // These are the store codec's array, object and reference tags, not engine types.
        let names = self
            .groups
            .iter()
            .filter(|group| matches!(group.kind, 7 | 8 | 10 | 17))
            .map(|group| &group.name)
            .collect::<std::collections::HashSet<_>>();
        let budget = DecodeBudget::for_file(self.file_bytes).holding_payloads();
        let scope = ValueScope {
            strings: &self.strings,
            instance_count: self.instance_count,
            budget: &budget,
        };
        let groups = self
            .groups
            .iter()
            .filter(|group| names.contains(&group.name))
            .collect::<Vec<_>>();
        let values = groups
            .iter()
            .map(|group| {
                budget.property_group(group.count, &group.name, group.kind)?;
                let mut entries = Vec::new();
                GroupCursor::beginning(group).visit_until(
                    group,
                    self.instance_count,
                    scope,
                    |entry| {
                        if !wanted[entry.instance_index()] {
                            return Ok(());
                        }
                        entries.push(match entry {
                            DecodedGroupEntry::Property(index, value) => {
                                let mut found = Vec::new();
                                references(value, &mut found);
                                DecodedGroupEntry::Property(index, Value::Array(found))
                            }
                            DecodedGroupEntry::Attributes(index, values) => {
                                let mut projected = Map::new();
                                for (name, value) in values {
                                    let mut found = Vec::new();
                                    references(value, &mut found);
                                    projected.insert(name, Value::Array(found));
                                }
                                DecodedGroupEntry::Attributes(index, projected)
                            }
                        });
                        Ok(())
                    },
                )?;
                Ok(entries)
            })
            .collect::<Result<Vec<_>>>()?;
        let specs = groups
            .iter()
            .map(|group| (&group.name, group.kind, group.count, group.bytes.as_slice()))
            .collect::<Vec<_>>();
        apply_decoded_property_groups(&mut document.instances, &specs, values);
        Ok(())
    }

    /// Gives the rows marked in `wanted` their maps, all in one pass over the
    /// columns, with the values of every other row dropped as they are
    /// decoded. No range may have been read before.
    pub(crate) fn materialize_selected(
        mut self,
        document: &mut SettingsBytecode,
        wanted: &[bool],
    ) -> Result<()> {
        let _span = crate::app::profile::span("store selected rows materialize");
        anyhow::ensure!(
            document.instances.len() == self.instance_count
                && wanted.len() == self.instance_count
                && self.next_row == 0,
            "Store rows are selected from a store nothing was read of; restart the store read"
        );
        let scope = ValueScope {
            strings: &self.strings,
            instance_count: self.instance_count,
            budget: &self.budget,
        };
        let groups = self
            .groups
            .par_iter()
            .zip(self.cursors.par_iter_mut())
            .map(|(group, cursor)| {
                let mut entries = Vec::new();
                cursor.visit_until(group, self.instance_count, scope, |entry| {
                    if wanted[entry.instance_index()] {
                        entries.push(entry);
                    }
                    Ok(())
                })?;
                Ok(entries)
            })
            .collect::<Result<Vec<_>>>()?;
        let specs = self
            .groups
            .iter()
            .map(|group| (&group.name, group.kind, group.count, group.bytes.as_slice()))
            .collect::<Vec<_>>();
        apply_decoded_property_groups(&mut document.instances, &specs, groups);
        Ok(())
    }

    pub(crate) fn finish(mut self) -> Result<()> {
        anyhow::ensure!(
            self.next_row == self.instance_count
                && self
                    .cursors
                    .iter()
                    .zip(&self.groups)
                    .all(|(cursor, group)| cursor.value == group.count),
            "Store rows were not completely read; restart the import before writing to Studio"
        );
        let scope = ValueScope {
            strings: &self.strings,
            instance_count: self.instance_count,
            budget: &self.budget,
        };
        // Also validate empty groups in an empty document, where no row range
        // caused materialize to visit their bodies.
        for (cursor, group) in self.cursors.iter_mut().zip(&self.groups) {
            cursor.visit_until(group, self.instance_count, scope, |_| {
                bail!("Store rows were not completely read; restart the import before writing to Studio")
            })?;
        }
        Ok(())
    }
}
