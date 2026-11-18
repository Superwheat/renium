//! Retain the store's encoded columns while materializing consecutive row ranges.
//! Values use the ordinary decoder; chunk boundaries never reset group state.
use super::*;
use std::ops::Range;

struct RowGroup {
    name: String,
    kind: u8,
    count: usize,
    bytes: Range<usize>,
    first_value: usize,
    rotations: CFrameGroupDecoder,
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
        payload: &[u8],
        end: usize,
        scope: ValueScope<'_>,
        mut visit: impl FnMut(DecodedGroupEntry) -> Result<()>,
    ) -> Result<()> {
        let body = &payload[group.bytes.clone()];
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
    payload: Vec<u8>,
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
            // These slices come from this payload's checked group reader.
            let start = body.as_ptr().addr() - payload.as_ptr().addr();
            Ok(RowGroup {
                name: name.clone(),
                kind,
                count,
                bytes: start..start + body.len(),
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
            payload,
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
            .par_iter()
            .zip(self.cursors.par_iter_mut())
            .map(|(group, cursor)| {
                let mut entries = Vec::with_capacity(rows.len().min(group.count));
                cursor.visit_until(group, &self.payload, rows.end, scope, |entry| {
                    entries.push(entry);
                    Ok(())
                })?;
                Ok(entries)
            })
            .collect::<Result<Vec<_>>>()?;
        let specs = self
            .groups
            .iter()
            .map(|group| {
                (
                    &group.name,
                    group.kind,
                    group.count,
                    &self.payload[group.bytes.clone()],
                )
            })
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
        let budget = DecodeBudget::for_file(self.file_bytes).holding_payloads();
        let scope = ValueScope {
            strings: &self.strings,
            instance_count: self.instance_count,
            budget: &budget,
        };
        for group in self.groups.iter().filter(|group| names(&group.name)) {
            budget.property_group(group.count, &group.name, group.kind)?;
            let mut cursor = GroupCursor::beginning(group);
            cursor.visit_until(group, &self.payload, self.instance_count, scope, |entry| {
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
                cursor.visit_until(group, &self.payload, self.instance_count, scope, |entry| {
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
            .map(|group| {
                (
                    &group.name,
                    group.kind,
                    group.count,
                    &self.payload[group.bytes.clone()],
                )
            })
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
            cursor.visit_until(group, &self.payload, self.instance_count, scope, |_| {
                bail!("Store rows were not completely read; restart the import before writing to Studio")
            })?;
        }
        Ok(())
    }
}
