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
    let bytes = encode_settings_bytecode_v11(&document)?;
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

#[test]
fn selected_reference_columns_are_not_decoded_again() -> Result<()> {
    let mut document = SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances: (0..3)
            .map(|i| {
                SettingsBytecodeInstance::new(
                    i.to_string(),
                    i.to_string(),
                    "Folder".into(),
                    (i > 0).then_some(0),
                )
            })
            .collect(),
    };
    for (index, row) in document.instances.iter_mut().enumerate() {
        row.properties.insert(
            "Nested".into(),
            json!({"values":[index, {"_type":"Ref", "instanceIndex":2}]}),
        );
        row.properties.insert("Scalar".into(), json!(index));
        row.attributes
            .insert("Text".into(), json!(format!("row {index}")));
    }
    let bytes = encode_settings_bytecode_v11(&document)?;
    let expected = decode_settings_bytecode_holding(&bytes)?;
    let (mut actual, mut columns) = decode_for_row_chunks(&bytes)?;
    columns.materialize_references(&mut actual, &[false, true, false])?;
    let mut completed = 0;
    for (group, cursor) in columns.groups.iter_mut().zip(&columns.cursors) {
        if cursor.value == group.count {
            completed += 1;
            // The values are already retained. A second decode must fail.
            group.bytes.fill(255);
        }
    }
    assert!(completed > 0);
    columns.materialize_selected(&mut actual, &[false, true, false])?;
    assert_eq!(actual.instances[1], expected.instances[1]);
    assert!(actual.instances[0].properties.is_empty() && actual.instances[2].properties.is_empty());
    Ok(())
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

/// The value kinds that can hold a reference, which a selection reads first.
/// Store codec tags, not an engine class or property list.
const REFERENCE_KINDS: [u8; 4] = [7, 8, 10, 17];

pub(crate) struct RetainedRowColumns {
    strings: Vec<String>,
    groups: Vec<RowGroup>,
    cursors: Vec<GroupCursor>,
    budget: DecodeBudget,
    file_bytes: usize,
    instance_count: usize,
    next_row: usize,
    reference_budget: Option<(usize, Vec<bool>)>,
    /// A v12 store keeps no columns: its rows come from its blocks.
    blocks: Option<super::super::blocks::RowBlocks>,
}

/// Read structure and retain the encoded value columns. The normal decoder's
/// budget, index checks, value validation and last-group-wins order still apply.
pub(crate) fn decode_for_row_chunks(
    bytes: &[u8],
) -> Result<(SettingsBytecode, RetainedRowColumns)> {
    let _span = crate::app::profile::span("store retained row columns");
    if super::super::blocks::is_block_store(bytes) {
        let (structure, blocks) = super::super::blocks::RowBlocks::decode(bytes)?;
        return Ok((structure, RetainedRowColumns::of_blocks(bytes, blocks)));
    }
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
            reference_budget: None,
            blocks: None,
        },
    ))
}

/// The retained value columns of a store whose structure the caller holds.
pub(crate) fn retained_row_columns(bytes: &[u8]) -> Result<RetainedRowColumns> {
    if super::super::blocks::is_block_store(bytes) {
        let _span = crate::app::profile::span("store retained row columns");
        let blocks = super::super::blocks::RowBlocks::open(bytes)?;
        return Ok(RetainedRowColumns::of_blocks(bytes, blocks));
    }
    Ok(decode_for_row_chunks(bytes)?.1)
}

impl RetainedRowColumns {
    fn of_blocks(bytes: &[u8], blocks: super::super::blocks::RowBlocks) -> Self {
        Self {
            strings: Vec::new(),
            groups: Vec::new(),
            cursors: Vec::new(),
            budget: DecodeBudget::for_file(bytes.len()),
            file_bytes: bytes.len(),
            instance_count: blocks.rows(),
            next_row: 0,
            reference_budget: None,
            blocks: Some(blocks),
        }
    }

    /// Only the requested range receives maps. The caller may consume those
    /// maps before the next range; structural row order must remain unchanged.
    pub(crate) fn materialize(
        &mut self,
        document: &mut SettingsBytecode,
        rows: Range<usize>,
    ) -> Result<()> {
        anyhow::ensure!(
            self.reference_budget.is_none(),
            "Selected reference columns require the same selected rows; restart the store read"
        );
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
        if let Some(blocks) = &mut self.blocks {
            blocks.fill(instances, rows.clone())?;
        }
        self.next_row = rows.end;
        Ok(())
    }

    /// Project small cross-row facts before consuming values, without retaining
    /// any other row's values. Projection does not advance the main read cursors.
    pub(crate) fn project_properties(
        &self,
        document: &mut SettingsBytecode,
        names: impl Fn(&str) -> bool + Sync,
        project: impl Fn(&SettingsBytecodeInstance, &str, Value) -> Option<Value>,
    ) -> Result<()> {
        anyhow::ensure!(
            self.next_row == 0,
            "Store context must be projected before reading rows; restart the import before writing to Studio"
        );
        if let Some(blocks) = &self.blocks {
            return blocks.project(document, names, project);
        }
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

    /// Decode reference-capable columns once, retaining the selected rows' full
    /// values for the following materialize_selected call. Include every group
    /// with the same property name so last-group-wins remains unchanged.
    pub(crate) fn materialize_references(
        &mut self,
        document: &mut SettingsBytecode,
        wanted: &[bool],
    ) -> Result<()> {
        anyhow::ensure!(
            wanted.len() == self.instance_count
                && document.instances.len() == self.instance_count
                && self.next_row == 0
                && self.reference_budget.is_none(),
            "Reference selection requires unread store columns; restart the store read"
        );
        self.reference_budget = Some((
            self.budget.remaining.load(Ordering::Relaxed),
            wanted.to_vec(),
        ));
        if let Some(blocks) = &mut self.blocks {
            return blocks.references(document, wanted, &REFERENCE_KINDS);
        }
        let names = self
            .groups
            .iter()
            .filter(|group| REFERENCE_KINDS.contains(&group.kind))
            .map(|group| &group.name)
            .collect::<std::collections::HashSet<_>>();
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
                if names.contains(&group.name) {
                    cursor.visit_until(group, self.instance_count, scope, |entry| {
                        if wanted[entry.instance_index()] {
                            entries.push(entry);
                        }
                        Ok(())
                    })?;
                }
                Ok(entries)
            })
            .collect::<Result<Vec<_>>>()?;
        let specs = self
            .groups
            .iter()
            .map(|g| (&g.name, g.kind, g.count, g.bytes.as_slice()))
            .collect::<Vec<_>>();
        apply_decoded_property_groups(&mut document.instances, &specs, groups);
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
                && self.next_row == 0
                && self
                    .reference_budget
                    .as_ref()
                    .is_none_or(|(_, rows)| rows == wanted),
            "Store rows are selected from a store nothing was read of; restart the store read"
        );
        if let Some(blocks) = &mut self.blocks {
            return blocks.select(document, wanted, self.reference_budget.is_some());
        }
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
