use super::*;

#[path = "row_chunks.rs"]
mod row_chunks;
pub(crate) use row_chunks::{RetainedRowColumns, decode_for_row_chunks, retained_row_columns};

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
    /// A v12 store keeps no columns: its edit reads and writes the blocks of
    /// the instances it makes complete or changes.
    blocks: Option<super::blocks::EditedStore>,
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

/// A store read for an edit that adds, removes or moves instances: a v12
/// store by its instances, each given its values as the edit asks for them,
/// and a store of an older version whole.
pub(crate) fn read_for_structural_edit(
    path: &Path,
) -> Result<(SettingsBytecode, RetainedPropertyColumns)> {
    read_settings_file(path, |bytes| {
        if super::blocks::is_block_store(bytes) {
            return decode_for_property_edit(bytes);
        }
        let columns = RetainedPropertyColumns::default();
        Ok((decode_settings_bytecode(bytes)?, columns))
    })
}

/// `read_for_structural_edit` of a store that holds every string of one of
/// the sets in `required`, or of any store where there are none.
pub(crate) fn read_for_structural_edit_if_contains(
    path: &Path,
    required: &[Vec<String>],
) -> Result<Option<(SettingsBytecode, RetainedPropertyColumns)>> {
    read_settings_file(path, |bytes| {
        if !super::blocks::is_block_store(bytes) {
            let document = decode_settings_bytecode_if_contains_any_string_set(bytes, required)?;
            return Ok(document.map(|document| (document, RetainedPropertyColumns::default())));
        }
        let held =
            required.is_empty() || super::blocks::BlockStore::open(bytes)?.holds_any(required)?;
        held.then(|| decode_for_property_edit(bytes)).transpose()
    })
}

/// Keep the ordinary store retry around both column parsing and the selected decode.
pub(crate) fn read_row_chunks_with<T>(
    path: &Path,
    mut decode: impl FnMut(SettingsBytecode, RetainedRowColumns, &[u8]) -> Result<T>,
) -> Result<T> {
    read_settings_file(path, |bytes| {
        let (document, columns) = decode_for_row_chunks(bytes)?;
        decode(document, columns, bytes)
    })
}

pub(crate) fn decode_for_property_edit(
    bytes: &[u8],
) -> Result<(SettingsBytecode, RetainedPropertyColumns)> {
    if super::blocks::is_block_store(bytes) {
        let (document, store) = super::blocks::EditedStore::decode(bytes)?;
        let columns = RetainedPropertyColumns {
            columns: Vec::new(),
            blocks: Some(store),
        };
        return Ok((document, columns));
    }
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
            blocks: None,
        },
    ))
}

#[cfg(test)]
thread_local! {
    pub(crate) static ROW_DECODE_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// `decode_rows` of one store file a batch of rows at a time. A v12 file's
/// instances are read once for all the batches; an older file is read anew
/// for each.
pub(crate) enum RowBatches<'a> {
    Blocks(super::blocks::RowBatches<'a>),
    Columns(&'a [u8]),
}

impl<'a> RowBatches<'a> {
    pub(crate) fn open(bytes: &'a [u8]) -> Result<Self> {
        if super::blocks::is_block_store(bytes) {
            let _span = crate::app::profile::span("store rows decode");
            return Ok(Self::Blocks(super::blocks::RowBatches::open(bytes)?));
        }
        Ok(Self::Columns(bytes))
    }

    /// The first batch, with the id of every instance of the file.
    pub(crate) fn first(
        &self,
        rows: &[usize],
    ) -> Result<(Vec<SettingsBytecodeInstance>, Vec<String>)> {
        match self {
            Self::Blocks(batches) => Ok((self.read(rows)?, batches.ids())),
            Self::Columns(bytes) => decode_rows(bytes, rows),
        }
    }

    pub(crate) fn read(&self, rows: &[usize]) -> Result<Vec<SettingsBytecodeInstance>> {
        match self {
            Self::Blocks(batches) => {
                #[cfg(test)]
                ROW_DECODE_CALLS.set(ROW_DECODE_CALLS.get() + 1);
                let _span = crate::app::profile::span("store rows decode");
                batches.read(rows)
            }
            Self::Columns(bytes) => Ok(decode_rows(bytes, rows)?.0),
        }
    }
}

/// The instances at `rows` of a store file, each as decoding the whole file
/// gives it, and the id of every instance of the file. The values of every
/// other row are dropped column by column, so the document is never whole in
/// memory.
pub(crate) fn decode_rows(
    bytes: &[u8],
    rows: &[usize],
) -> Result<(Vec<SettingsBytecodeInstance>, Vec<String>)> {
    #[cfg(test)]
    ROW_DECODE_CALLS.set(ROW_DECODE_CALLS.get() + 1);
    let _span = crate::app::profile::span("store rows decode");
    if super::blocks::is_block_store(bytes) {
        return super::blocks::decode_rows(bytes, rows);
    }
    let (_, payload) = decode_settings_bytecode_container(bytes)?;
    let budget = DecodeBudget::for_file(bytes.len());
    let mut reader = BytecodeReader::new(&payload);
    let SettingsPayloadHeader {
        strings,
        properties,
        mut instances,
        instance_count,
    } = decode_settings_payload_header(&mut reader, &budget)?;
    let specs = read_settings_property_groups(&mut reader, &properties)?;
    let mut wanted = vec![false; instance_count];
    for &row in rows {
        *wanted
            .get_mut(row)
            .with_context(|| format!("The store has no instance {row}"))? = true;
    }
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
            // Groups of one property can overwrite a row in their order, which
            // the ordinary decoder keeps.
            if occurrences[name.as_str()] == 1
                && let Some((column, mut decoded)) =
                    decode_compact_column(body, name, kind, count, scope)?
            {
                decoded.retain(|entry| wanted[entry.instance_index()]);
                for (index, value) in &column.values {
                    if wanted[*index] {
                        decoded.push(DecodedGroupEntry::Property(
                            *index,
                            materialized_value(value, kind)?,
                        ));
                    }
                }
                // The values of a group go to their rows in the order of
                // the instances, as the ordinary decoder gives them.
                decoded.sort_unstable_by_key(DecodedGroupEntry::instance_index);
                return Ok(decoded);
            }
            let mut decoded = decode_property_group_body(body, name, kind, count, scope)?;
            decoded.retain(|entry| wanted[entry.instance_index()]);
            Ok(decoded)
        })
        .collect::<Result<Vec<_>>>()?;
    reader.finish()?;
    apply_decoded_property_groups(&mut instances, &specs, groups);
    let read = rows.iter().map(|&row| instances[row].clone()).collect();
    Ok((read, ids_of(instances)))
}

fn ids_of(instances: Vec<SettingsBytecodeInstance>) -> Vec<String> {
    instances
        .into_iter()
        .map(|instance| instance.settings_id)
        .collect()
}

pub(crate) use super::captured::Moved;

/// What `row_digests` reads of a store file.
pub(crate) struct StoreRows {
    /// The digest of each instance.
    pub(crate) digests: Vec<[u8; 16]>,
    /// The digest of the UniqueId of each instance, all zero without one.
    pub(crate) unique_ids: Vec<[u8; 16]>,
    /// The instance asked for whole, where the store has one there.
    pub(crate) kept: Option<SettingsBytecodeInstance>,
    /// The id of each instance, from `row_digests`.
    pub(crate) ids: Vec<String>,
    /// How the instances stand in the state they were digested for.
    pub(crate) moved: Option<super::captured::Moved>,
    /// From `whole_row_digests`: every instance without its values but its
    /// UniqueId.
    pub(crate) instances: Vec<SettingsBytecodeInstance>,
    /// From `whole_row_digests`: the instances that name another instance by
    /// an id. Which instance that is depends on the ids of the copy that
    /// holds it, which no digest shows.
    pub(crate) named: Vec<usize>,
}

/// One value of a store, by the instance that holds it, its property's
/// place among the names, and its group's place in the file.
struct Cell {
    row: u32,
    name: u32,
    group: u32,
    digest: [u8; 16],
}

/// The cell of an instance's attributes, after every property.
const ATTRIBUTES_CELL: u32 = u32::MAX;

/// The columns whose values are decoded as JSON values, of which few are
/// held at one time; a column of typed values takes a fraction of that.
const WHOLE_VALUE_COLUMNS_AT_ONCE: usize = 2;

/// The digest `captured::document_row_digest` gives each instance of a store
/// file's document, taken column by column: the document is never whole in
/// memory. `plan` sees the instances without their values and says which of
/// them comes whole with the digests, and how they stand in the state they
/// are digested for: a parent and a reference by position are digested as
/// the position they have there. `None` where it says nothing.
pub(crate) fn row_digests(
    bytes: &[u8],
    plan: impl FnOnce(
        &[SettingsBytecodeInstance],
    ) -> Option<(Option<usize>, Option<super::captured::Moved>)>,
) -> Result<Option<StoreRows>> {
    row_digests_of(bytes, false, None, plan)
}

/// `row_digests` over every value an instance holds, what the engine alone
/// sets included: two instances with one digest and one UniqueId digest hold
/// the same values, whatever a comparison looks at. `structure` is the
/// instances of a v12 store without their values, where they were read
/// before.
pub(crate) fn whole_row_digests(
    bytes: &[u8],
    structure: Option<Vec<SettingsBytecodeInstance>>,
    plan: impl FnOnce(
        &[SettingsBytecodeInstance],
    ) -> Option<(Option<usize>, Option<super::captured::Moved>)>,
) -> Result<Option<StoreRows>> {
    row_digests_of(bytes, true, structure, plan)
}

/// `whole_row_digests` of two copies of one store, for telling the rows of
/// one from the rows of the other, with their `structures` where they were
/// read before. A block both hold byte for byte is not digested row by row
/// (`BlockStore::row_digests`).
pub(crate) fn whole_row_digests_of_both(
    first: &[u8],
    second: &[u8],
    structures: Option<(Vec<SettingsBytecodeInstance>, Vec<SettingsBytecodeInstance>)>,
) -> (Result<Option<StoreRows>>, Result<Option<StoreRows>>) {
    use super::blocks::BlockStore;
    let open = |bytes| BlockStore::open(bytes).map(BlockStore::holding);
    let shared = match (open(first), open(second)) {
        (Ok(first), Ok(second)) => first.blocks_held_by(&second),
        _ => Vec::new(),
    };
    let digests = |bytes, structure| {
        let _span = crate::app::profile::span("store instance digests");
        if shared.contains(&true) {
            open(bytes)?.row_digests(true, &shared, structure, |_| Some((None, None)))
        } else {
            whole_row_digests(bytes, structure, |_| Some((None, None)))
        }
    };
    let (first_structure, second_structure) = structures.unzip();
    rayon::join(
        || digests(first, first_structure),
        || digests(second, second_structure),
    )
}

fn row_digests_of(
    bytes: &[u8],
    whole: bool,
    structure: Option<Vec<SettingsBytecodeInstance>>,
    plan: impl FnOnce(
        &[SettingsBytecodeInstance],
    ) -> Option<(Option<usize>, Option<super::captured::Moved>)>,
) -> Result<Option<StoreRows>> {
    use super::captured::{UNIQUE_ID, attributes_digest, is_digested, row_digest, value_digest};

    let _span = crate::app::profile::span("store instance digests");
    if super::blocks::is_block_store(bytes) {
        return super::blocks::BlockStore::open(bytes)?
            .holding()
            .row_digests(whole, &[], structure, plan);
    }
    let (_, payload) = decode_settings_bytecode_container(bytes)?;
    let budget = DecodeBudget::for_file(bytes.len());
    let mut reader = BytecodeReader::new(&payload);
    let SettingsPayloadHeader {
        strings,
        properties,
        mut instances,
        instance_count,
    } = decode_settings_payload_header(&mut reader, &budget)?;
    let specs = read_settings_property_groups(&mut reader, &properties)?;
    let Some((kept, moved)) = plan(&instances) else {
        return Ok(None);
    };
    // A store too short to hold that instance gives none.
    let kept = kept.filter(|kept| *kept < instance_count);
    let mut occurrences = AHashMap::<&str, usize>::new();
    for (name, ..) in &specs {
        *occurrences.entry(name.as_str()).or_default() += 1;
    }
    // A document holds an instance's properties by name.
    let mut names = occurrences.keys().copied().collect::<Vec<_>>();
    names.sort_unstable();
    let scope = ValueScope {
        strings: &strings,
        instance_count,
        budget: &budget,
    };
    let placed = moved.as_ref();
    let value_digest = |value: &Value| match placed.and_then(|moved| moved.value_in_state(value)) {
        Some(value) => value_digest(&value),
        None => value_digest(value),
    };
    let attributes_digest = |attributes: &Map<String, Value>| match placed
        .and_then(|moved| moved.values_in_state(attributes))
    {
        Some(attributes) => attributes_digest(&attributes),
        None => attributes_digest(attributes),
    };
    let named = std::sync::Mutex::new(Vec::new());
    // The cells of one group, and what the kept instance holds of it.
    let digested = |group: usize| -> Result<(Vec<Cell>, Vec<DecodedGroupEntry>)> {
        let (name, kind, count, body) = specs[group];
        let place = names.binary_search(&name.as_str()).unwrap_or_default() as u32;
        // Every instance keeps its UniqueId where the instances come back.
        let kept_of_all = whole && name.as_str() == UNIQUE_ID;
        let cell = |row: usize, name: u32, digest: [u8; 16]| Cell {
            row: row as u32,
            name,
            group: group as u32,
            digest,
        };
        let mut held = Vec::new();
        let mut entry = |entry: DecodedGroupEntry| {
            let cell = match &entry {
                DecodedGroupEntry::Property(row, value) => {
                    if whole && names_an_instance_by_id(value) {
                        named
                            .lock()
                            .unwrap_or_else(|held| held.into_inner())
                            .push(*row);
                    }
                    cell(*row, place, value_digest(value))
                }
                DecodedGroupEntry::Attributes(row, attributes) => {
                    if whole && attributes.values().any(names_an_instance_by_id) {
                        named
                            .lock()
                            .unwrap_or_else(|held| held.into_inner())
                            .push(*row);
                    }
                    cell(*row, ATTRIBUTES_CELL, attributes_digest(attributes))
                }
            };
            if kept_of_all || kept == Some(entry.instance_index()) {
                held.push(entry);
            }
            cell
        };
        // Groups of one property can overwrite a row in their order, which
        // the ordinary decoder's entries keep.
        if occurrences[name.as_str()] == 1
            && let Some((column, decoded)) = decode_compact_column(body, name, kind, count, scope)?
        {
            let mut cells = decoded.into_iter().map(&mut entry).collect::<Vec<_>>();
            cells.reserve(column.values.len());
            // A payload many rows hold is digested once, and so is a typed
            // value: the instances of one mesh hold the same sizes, and a
            // column of enum items holds a few names.
            let mut shared = AHashMap::<usize, [u8; 16]>::new();
            let mut typed = AHashMap::<TypedKey<'_>, [u8; 16]>::new();
            for (row, value) in &column.values {
                let digest = match value {
                    NativeSettingsValue::Shared(held) if kept != Some(*row) => *shared
                        .entry(std::sync::Arc::as_ptr(held) as usize)
                        .or_insert_with(|| value_digest(held)),
                    value => {
                        let keep = kept_of_all || kept == Some(*row);
                        let key = if keep { None } else { typed_key(value) };
                        match key.as_ref().and_then(|key| typed.get(key)) {
                            Some(digest) => *digest,
                            None => {
                                let value = materialized_value(value, kind)?;
                                let digest = value_digest(&value);
                                if keep {
                                    held.push(DecodedGroupEntry::Property(*row, value));
                                }
                                if let Some(key) = key {
                                    typed.insert(key, digest);
                                }
                                digest
                            }
                        }
                    }
                };
                cells.push(cell(*row, place, digest));
            }
            return Ok((cells, held));
        }
        let cells = decode_property_group_body(body, name, kind, count, scope)?
            .into_iter()
            .map(&mut entry)
            .collect();
        Ok((cells, held))
    };
    let typed = |group: &usize| {
        let (name, kind, ..) = specs[*group];
        occurrences[name.as_str()] == 1 && matches!(kind, 1..=3 | 5 | 6 | 11..=20)
    };
    let (typed_groups, whole_groups): (Vec<_>, Vec<_>) = (0..specs.len()).partition(typed);
    let mut groups = typed_groups
        .par_iter()
        .map(|group| Ok((*group, digested(*group)?)))
        .collect::<Result<Vec<_>>>()?;
    for few in whole_groups.chunks(WHOLE_VALUE_COLUMNS_AT_ONCE) {
        groups.extend(
            few.par_iter()
                .map(|group| Ok((*group, digested(*group)?)))
                .collect::<Result<Vec<_>>>()?,
        );
    }
    reader.finish()?;
    // The kept instance takes its values in the groups' order.
    groups.sort_unstable_by_key(|(group, _)| *group);
    let mut cells = Vec::with_capacity(groups.iter().map(|(_, (cells, _))| cells.len()).sum());
    let mut held = Vec::with_capacity(specs.len());
    for (_, (group_cells, group_held)) in groups {
        cells.extend(group_cells);
        held.push(group_held);
    }
    apply_decoded_property_groups(&mut instances, &specs, held);
    cells.par_sort_unstable_by_key(|cell| (cell.row, cell.name, cell.group));
    if let Some(moved) = &moved {
        for instance in &mut instances {
            moved.parent_in_state(instance);
        }
    }
    let empty = attributes_digest(&Map::new());
    let unique_id = names
        .binary_search(&UNIQUE_ID)
        .ok()
        .map(|place| place as u32);
    let mut rows = Vec::with_capacity(instance_count);
    let mut unique_ids = Vec::with_capacity(instance_count);
    let mut at = 0;
    for (row, instance) in instances.iter().enumerate() {
        let start = at;
        while cells.get(at).is_some_and(|cell| cell.row as usize == row) {
            at += 1;
        }
        // The last group to write a value is the one the document holds.
        let held = cells[start..at]
            .iter()
            .enumerate()
            .filter(|(index, cell)| {
                cells[start..at]
                    .get(index + 1)
                    .is_none_or(|next| next.name != cell.name)
            })
            .map(|(_, cell)| cell);
        let attributes = held
            .clone()
            .find(|cell| cell.name == ATTRIBUTES_CELL)
            .map_or(empty, |cell| cell.digest);
        unique_ids.push(
            held.clone()
                .find(|cell| Some(cell.name) == unique_id)
                .map_or([0; 16], |cell| cell.digest),
        );
        rows.push(row_digest(
            instance,
            held.filter(|cell| {
                cell.name != ATTRIBUTES_CELL
                    && (whole && names[cell.name as usize] != UNIQUE_ID
                        || is_digested(&instance.class_name, names[cell.name as usize]))
            })
            .map(|cell| (names[cell.name as usize], cell.digest)),
            attributes,
        ));
    }
    let kept = kept.map(|kept| instances[kept].clone());
    // The instances carry their ids where they come back themselves.
    let (ids, instances) = if whole {
        (Vec::new(), instances)
    } else {
        (ids_of(instances), Vec::new())
    };
    let mut named = named.into_inner().unwrap_or_else(|held| held.into_inner());
    named.sort_unstable();
    named.dedup();
    Ok(Some(StoreRows {
        digests: rows,
        unique_ids,
        kept,
        ids,
        moved,
        instances,
        named,
    }))
}

/// Whether a value names an instance by an id, at any depth: whatever an
/// alignment that renames ids would rewrite.
pub(crate) fn names_an_instance_by_id(value: &Value) -> bool {
    match value {
        Value::Array(items) => items.iter().any(names_an_instance_by_id),
        Value::Object(object) => {
            ["settingsId", "instanceId", "debugId"]
                .iter()
                .any(|name| object.contains_key(*name))
                || object.values().any(names_an_instance_by_id)
        }
        _ => false,
    }
}

/// What tells one typed value of a column from another: its kind and its
/// bits or its text. Two values with one key give one JSON value.
#[derive(PartialEq, Eq, Hash)]
enum TypedKey<'a> {
    Number(u8, u64),
    Floats(u8, [u32; 12]),
    Text(u8, &'a str),
}

fn typed_key(value: &NativeSettingsValue) -> Option<TypedKey<'_>> {
    fn floats<const N: usize>(tag: u8, values: &[f32; N]) -> TypedKey<'static> {
        let mut bits = [0; 12];
        for (bit, value) in bits.iter_mut().zip(values) {
            *bit = value.to_bits();
        }
        TypedKey::Floats(tag, bits)
    }
    Some(match value {
        NativeSettingsValue::Bool(value) => TypedKey::Number(0, u64::from(*value)),
        NativeSettingsValue::Int(value) => TypedKey::Number(1, *value as u64),
        NativeSettingsValue::Float32(value) => TypedKey::Number(2, u64::from(value.to_bits())),
        NativeSettingsValue::Float64(value) => TypedKey::Number(3, value.to_bits()),
        NativeSettingsValue::Ref(index) => TypedKey::Number(4, *index as u64),
        NativeSettingsValue::String(text) => TypedKey::Text(5, text),
        NativeSettingsValue::Enum(text) => TypedKey::Text(6, text),
        NativeSettingsValue::UniqueId(text) => TypedKey::Text(14, text),
        NativeSettingsValue::Vector2(values) => floats(7, values),
        NativeSettingsValue::Vector3(values) => floats(8, values),
        NativeSettingsValue::UDim(values) => floats(9, values),
        NativeSettingsValue::UDim2(values) => floats(10, values),
        NativeSettingsValue::Color3(values) => floats(11, values),
        NativeSettingsValue::CFrame(values) => floats(12, values),
        NativeSettingsValue::Rect(values) => floats(13, values),
        NativeSettingsValue::Shared(_) => return None,
    })
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
        FixedNumericKind::CFrame => NativeSettingsValue::CFrame(Box::new(components)),
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
        if let Some(store) = &mut self.blocks {
            return store.materialize(document, indices);
        }
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

    /// Gives the rows at `rows` of a store read by its instances their
    /// stored value of the property `name`, for an edit that only reads it.
    pub(crate) fn give(
        &mut self,
        document: &mut SettingsBytecode,
        rows: &[usize],
        name: &str,
    ) -> Result<()> {
        match &mut self.blocks {
            Some(store) => store.give(document, rows, name),
            None => Ok(()),
        }
    }

    /// `materialize` of each instance of a store read by its instances that
    /// holds a value `wanted` takes: of the blocks that hold one of
    /// `strings`, or of every block where there are none.
    pub(crate) fn materialize_where(
        &mut self,
        document: &mut SettingsBytecode,
        strings: Option<&AHashSet<&str>>,
        wanted: impl Fn(&Value) -> bool + Sync,
    ) -> Result<()> {
        match &mut self.blocks {
            Some(store) => store.materialize_where(document, strings, wanted),
            None => Ok(()),
        }
    }

    /// `encode` after an edit of `read_for_structural_edit` that added,
    /// removed or moved instances, each one known by its id.
    pub(crate) fn encode_restructured(&self, document: &SettingsBytecode) -> Result<Vec<u8>> {
        match &self.blocks {
            Some(store) => store.encode_restructured(document),
            None => encode_settings_bytecode(document),
        }
    }

    pub(crate) fn encode(&self, document: &SettingsBytecode) -> Result<Vec<u8>> {
        if let Some(store) = &self.blocks {
            return store.encode(document);
        }
        // A v11 store is written whole as v12: each retained value goes back
        // to its row where the document holds none of that name, a payload
        // many rows hold by its handle.
        let mut whole = document.clone();
        for column in &self.columns {
            for (index, value) in &column.values {
                let row = &mut whole
                    .instances
                    .get_mut(*index)
                    .context("An edit of values changed how many instances the store holds")?
                    .properties;
                if row.contains_key(&column.name) {
                    continue;
                }
                let value = match value {
                    NativeSettingsValue::Shared(shared) => crate::settings::held::held_copy(shared),
                    value => materialized_value(value, column.kind)?,
                };
                row.insert(column.name.clone(), value);
            }
        }
        encode_settings_bytecode(&whole)
    }
}

fn materialized_value(value: &NativeSettingsValue, kind: u8) -> Result<Value> {
    match value {
        NativeSettingsValue::String(value) => return Ok(json!(value)),
        NativeSettingsValue::UniqueId(value) => {
            return Ok(typed_object("UniqueId", [("value", json!(value))]));
        }
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
