//! Selected subtrees use the service serializer and its conditional-reference
//! overlay contract. `descendants=false` is explicitly promoted to a subtree:
//! the discovered flag-64 call has no validated nonrecursive entry point.
use super::*;

const ROOT_SIZE: usize = 1680;
pub(super) const RECEIPT_SIZE: usize = 88;

pub(crate) struct NativeCaptureRoot<'a> {
    pub path: NativePropertyPath<'a>,
    pub descendants: bool,
    /// GetDebugId(32) from the caller's existing row; paths alone cannot detect
    /// a deleted instance replaced by an identically named sibling.
    pub expected_debug_id: &'a str,
}

#[derive(Debug, PartialEq)]
pub(crate) enum NativeCaptureRootResult {
    Captured {
        identity_row: usize,
        sibling_index: usize,
        /// Always true until a nonrecursive serializer ABI is discovered.
        subtree: bool,
        /// Same 72-byte identity record as capture.identities; parent=MAX.
        parent: [u8; CAPTURE_ROW_SIZE],
    },
    Missing,
    Moved,
}

// This primitive is intentionally separate from service export: its caller
// must join identities, obtain the existing conditional-reference overlay and
// fence the plugin epoch before replacing store rows. An Err permits the
// existing service capture; no partial result may be published on an Err.
#[allow(dead_code)]
pub(crate) fn capture_live_instances(
    pid: u32,
    title: &str,
    roots: &[NativeCaptureRoot<'_>],
    timeout: Duration,
) -> Result<NativeCapture> {
    anyhow::ensure!(
        !roots.is_empty() && roots.len() <= MAX_ROOTS,
        "capture-root-path detector: requires 1..=256 roots"
    );
    let memory = ProcessMemory::open(pid)?;
    let (mut captured, attempts) = crate::studio::native::capture::retry_capture(
        timeout,
        |remaining| capture_live_services_once(pid, title, &[], remaining, &memory, Some(roots)),
        |_, _, _| {},
    )?;
    captured.attempts = attempts;
    Ok(captured)
}

// Only path siblings are inspected, in fixed chunks. In particular a direct
// child of a large Workspace never allocates a Workspace-sized host vector.
fn visit_children(
    memory: &ProcessMemory,
    parent: usize,
    layout: InstanceLayout,
    deadline: Instant,
    mut visit: impl FnMut(usize, SharedEntry) -> Result<bool>,
) -> Result<()> {
    let vector = memory.read_u64(parent + layout.children)? as usize;
    if vector == 0 {
        return Ok(());
    }
    let header = memory.read_vec(vector, 24)?;
    let begin = read_u64(&header, 0)? as usize;
    let end = read_u64(&header, 8)? as usize;
    let capacity = read_u64(&header, 16)? as usize;
    anyhow::ensure!(
        end >= begin
            && capacity >= end
            && (end - begin).is_multiple_of(16)
            && (capacity - begin).is_multiple_of(16)
            && (capacity - begin) / 16 <= CAPTURE_MAX_ROWS,
        "capture-root-path detector: invalid children vector"
    );
    let mut buffer = [0; 4096];
    for offset in (0..end - begin).step_by(buffer.len()) {
        anyhow::ensure!(
            Instant::now() < deadline,
            "capture-root-path detector: deadline exceeded"
        );
        let size = buffer.len().min(end - begin - offset);
        memory.read(begin + offset, &mut buffer[..size])?;
        for (index, bytes) in buffer[..size].chunks_exact(16).enumerate() {
            let entry = SharedEntry {
                instance: read_u64(bytes, 0)? as usize,
                owner: read_u64(bytes, 8)? as usize,
            };
            anyhow::ensure!(
                likely_pointer(entry.instance) && likely_pointer(entry.owner),
                "capture-root-path detector: invalid ownership"
            );
            if !visit(offset / 16 + index, entry)? {
                return Ok(());
            }
        }
    }
    Ok(())
}

pub(super) fn encode(
    memory: &ProcessMemory,
    model: &ActiveDataModel,
    roots: &[NativeCaptureRoot<'_>],
    deadline: Instant,
) -> Result<Vec<u8>> {
    let database = rbx_reflection_database::get()?;
    let mut bytes = vec![0; 8 + roots.len() * ROOT_SIZE];
    put_u32(&mut bytes, 0, roots.len() as u32);
    for (index, root) in roots.iter().enumerate() {
        properties::validate_property_path(root.path.segments, root.path.ordinals)?;
        let id = root.expected_debug_id.as_bytes();
        anyhow::ensure!(
            !id.is_empty() && id.len() < 48 && !id.contains(&0),
            "capture-root-path detector: expected debug identity required"
        );
        let base = 8 + index * ROOT_SIZE;
        bytes[base + 8..base + 8 + id.len()].copy_from_slice(id);
        // Record requested scope; receipt always reports actual subtree scope.
        put_u32(&mut bytes, base + 4, u32::from(root.descendants));
        let mut parent = model.outer + model.layout.data_model_instance;
        let mut depth = 0;
        for (part, name) in root.path.segments.iter().enumerate() {
            let service = part == 0
                && database
                    .classes
                    .get(name.as_str())
                    .is_some_and(|class| class.tags.contains(&rbx_reflection::ClassTag::Service));
            let mut count = 0;
            let mut selected = None;
            visit_children(memory, parent, model.layout, deadline, |sibling, entry| {
                let label = if service {
                    read_instance_class(memory, entry.instance, model.layout)
                } else {
                    read_instance_name(memory, entry.instance, model.layout)
                };
                let label =
                    label.context("capture-root-path detector: unreadable sibling label")?;
                if &label == name {
                    count += 1;
                    if root.path.ordinals.get(part).copied().unwrap_or(1) == count {
                        selected = Some((sibling, entry));
                        if !root.path.ordinals.is_empty() {
                            return Ok(false);
                        }
                    }
                }
                Ok(true)
            })?;
            anyhow::ensure!(
                count <= 1 || !root.path.ordinals.is_empty(),
                "capture-root-path detector: ambiguous path requires exact ordinals"
            );
            if properties::exact_ordinal_index(root.path.ordinals.get(part).copied(), count)
                .is_none()
            {
                break;
            }
            let (sibling, entry) =
                selected.context("capture-root-path detector: inconsistent ordinal")?;
            let node = base + 56 + part * 24;
            put_u64(&mut bytes, node, entry.instance);
            put_u64(&mut bytes, node + 8, entry.owner);
            put_u64(&mut bytes, node + 16, sibling);
            parent = entry.instance;
            depth += 1;
        }
        if depth == root.path.segments.len() {
            put_u32(&mut bytes, base, depth as u32);
        }
    }
    Ok(bytes)
}

pub(super) fn decode(bytes: &[u8], rows: usize) -> Result<Vec<NativeCaptureRootResult>> {
    anyhow::ensure!(
        bytes.len().is_multiple_of(RECEIPT_SIZE),
        "Truncated selected capture receipt"
    );
    let mut next_row = 0;
    bytes
        .chunks_exact(RECEIPT_SIZE)
        .map(|receipt| {
            let status = read_u32(receipt, 0)?;
            let row = read_u32(receipt, 4)? as usize;
            anyhow::ensure!(
                read_u32(receipt, 12)? == 1,
                "Invalid selected capture scope"
            );
            match status {
                0 => {
                    anyhow::ensure!(
                        row == next_row && row < rows,
                        "Invalid selected capture root row"
                    );
                    next_row += 1;
                    let parent: [u8; CAPTURE_ROW_SIZE] = receipt[16..].try_into()?;
                    let text = &parent[16..64];
                    let end = text
                        .iter()
                        .position(|byte| *byte == 0)
                        .context("Unterminated capture parent identity")?;
                    anyhow::ensure!(
                        end > 0
                            && text[end..].iter().all(|byte| *byte == 0)
                            && read_u32(&parent, 64)? == u32::MAX
                            && read_u32(&parent, 68)? == 0,
                        "Invalid capture parent identity"
                    );
                    std::str::from_utf8(&text[..end])?;
                    Ok(NativeCaptureRootResult::Captured {
                        identity_row: row,
                        sibling_index: read_u32(receipt, 8)? as usize,
                        subtree: true,
                        parent,
                    })
                }
                1 | 2 if row == u32::MAX as usize => Ok(if status == 1 {
                    NativeCaptureRootResult::Missing
                } else {
                    NativeCaptureRootResult::Moved
                }),
                _ => bail!("Invalid selected capture status"),
            }
        })
        .collect()
}

#[test]
fn selected_capture_receipts_preserve_request_order_and_reject_invalid_rows() {
    let mut bytes = [0; RECEIPT_SIZE * 3];
    for i in 0..3 {
        put_u32(&mut bytes, i * RECEIPT_SIZE + 12, 1);
    }
    put_u32(&mut bytes, 0, 1);
    put_u32(&mut bytes, 4, u32::MAX);
    put_u32(&mut bytes, RECEIPT_SIZE + 8, 123);
    bytes[RECEIPT_SIZE + 32..RECEIPT_SIZE + 35].copy_from_slice(b"0_1");
    put_u32(&mut bytes, RECEIPT_SIZE + 80, u32::MAX);
    put_u32(&mut bytes, RECEIPT_SIZE * 2, 2);
    put_u32(&mut bytes, RECEIPT_SIZE * 2 + 4, u32::MAX);
    let result = decode(&bytes, 1).unwrap();
    assert_eq!(result[0], NativeCaptureRootResult::Missing);
    assert!(matches!(
        result[1],
        NativeCaptureRootResult::Captured {
            sibling_index: 123,
            identity_row: 0,
            subtree: true,
            ..
        }
    ));
    assert_eq!(result[2], NativeCaptureRootResult::Moved);
    assert!(decode(&bytes, 0).is_err());
    assert!(decode(&bytes[..bytes.len() - 1], 1).is_err());
    put_u32(&mut bytes, RECEIPT_SIZE + 4, 1);
    assert!(decode(&bytes, 2).is_err());
}

#[test]
fn selected_capture_path_reads_bounded_sibling_chunks() -> Result<()> {
    let memory = ProcessMemory::open_read_only(std::process::id())?;
    let entries = (0..257)
        .map(|i| [0x10000usize + i * 16, 0x80000])
        .collect::<Vec<_>>();
    let begin = entries.as_ptr() as usize;
    let vector = [
        begin,
        begin + entries.len() * 16,
        begin + entries.len() * 16,
    ];
    let parent = Box::new(vector.as_ptr() as usize);
    let layout = InstanceLayout {
        data_model_instance: 0,
        self_pointer: 0,
        class_descriptor: 0,
        children: 0,
        name: 0,
    };
    let mut seen = 0;
    visit_children(
        &memory,
        (&*parent) as *const usize as usize,
        layout,
        Instant::now() + Duration::from_secs(5),
        |index, entry| {
            assert_eq!(index, seen);
            assert_eq!(entry.instance, entries[index][0]);
            seen += 1;
            Ok(true)
        },
    )?;
    assert_eq!(seen, 257);
    let mut first = 0;
    visit_children(
        &memory,
        (&*parent) as *const usize as usize,
        layout,
        Instant::now() + Duration::from_secs(5),
        |index, _| {
            assert_eq!(index, 0);
            first += 1;
            Ok(false)
        },
    )?;
    assert_eq!(first, 1);
    let invalid = [
        begin,
        begin + entries.len() * 16 - 1,
        begin + entries.len() * 16,
    ];
    let invalid_parent = invalid.as_ptr() as usize;
    assert!(
        visit_children(
            &memory,
            &invalid_parent as *const usize as usize,
            layout,
            Instant::now() + Duration::from_secs(5),
            |_, _| panic!("invalid vector must not visit entries")
        )
        .is_err()
    );
    Ok(())
}

/// Opt-in only: never part of the offline capture test run. The owner supplies
/// the exact process/document and an existing empty artifact directory.
#[test]
#[ignore = "requires the owner's explicitly selected live Studio"]
fn selected_capture_live_canary() -> Result<()> {
    #[derive(serde::Deserialize)]
    struct Root {
        segments: Vec<String>,
        ordinals: Vec<usize>,
        id: String,
        descendants: bool,
    }
    #[derive(serde::Deserialize)]
    struct Input {
        pid: u32,
        title: String,
        roots: Vec<Root>,
        output: PathBuf,
    }
    let input: Input = serde_json::from_str(&std::env::var("RENIUM_SELECTED_CAPTURE")?)?;
    anyhow::ensure!(
        input.output.is_dir(),
        "Choose an existing empty capture artifact directory"
    );
    let roots = input
        .roots
        .iter()
        .map(|root| NativeCaptureRoot {
            path: NativePropertyPath {
                segments: &root.segments,
                ordinals: &root.ordinals,
            },
            expected_debug_id: &root.id,
            descendants: root.descendants,
        })
        .collect::<Vec<_>>();
    let captured =
        capture_live_instances(input.pid, &input.title, &roots, Duration::from_secs(15))?;
    let receipts = format!("{:#?}", captured.roots);
    for (name, bytes) in [
        ("capture.rbxl", captured.bytes.as_slice()),
        ("identities.bin", captured.identities.as_slice()),
        ("roots.txt", receipts.as_bytes()),
    ] {
        use std::io::Write;
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(input.output.join(name))?
            .write_all(bytes)?;
    }
    Ok(())
}
