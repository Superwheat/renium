//! Capture's dispatch association, retained from structural discovery to the
//! queued helper check. All offsets are relative to the DataModel Instance.
use super::*;

pub(super) const SERIALIZER_VERSION: u32 = 2;
pub(super) const PARAM_PROTOCOL_VERSION: usize = 5900;
pub(super) const PARAM_CONTEXT_FIELD: usize = 5904;
pub(super) const PARAM_INSTANCE_SIZE: usize = 5908;
pub(super) const PARAM_CONTEXT_MASK: usize = 5912;
pub(super) const PARAM_CONTEXT_OBSERVED: usize = 5920;
pub(super) const PARAM_CONTEXT_READ: usize = 5928;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(super) struct ContextField {
    pub offset: usize,
    pub mask: usize,
    pub pointer: usize,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct TaskContextBinding {
    pub field: ContextField,
    /// Proven readable prefix, not a guessed allocation size. The host has
    /// already read these Instance members while validating the live layout.
    pub instance_size: usize,
}

fn valid_field(field: ContextField, size: usize) -> bool {
    field.offset >= 8
        && field.offset.is_multiple_of(8)
        && field.offset.checked_add(8).is_some_and(|end| end <= size)
        && field.mask != 0
        && (!field.mask).wrapping_add(1).is_power_of_two()
        && likely_pointer(field.pointer)
        && field.pointer & field.mask == field.pointer
}

pub(super) fn bind_context_field(
    fields: &HashSet<ContextField>,
    instance_size: usize,
) -> Result<TaskContextBinding> {
    let locations = fields
        .iter()
        .map(|f| (f.offset, f.pointer))
        .collect::<HashSet<_>>();
    anyhow::ensure!(
        locations.len() == 1,
        "DataModel capture task-context binding has {} field/object pairs; expected one",
        locations.len()
    );
    // The decoder records both a load and subsequent ANDs. An untagged value
    // can therefore validate several masks at one slot. Retain the strongest
    // mask actually present in verified code, never synthesize a new mask.
    let field = fields
        .iter()
        .find(|candidate| {
            fields
                .iter()
                .all(|other| candidate.mask & other.mask == candidate.mask)
        })
        .copied()
        .context("DataModel capture task-context masks are ambiguous")?;
    anyhow::ensure!(
        valid_field(field, instance_size),
        "DataModel capture task-context field offset=0x{:X} mask=0x{:016X} is outside the validated Instance prefix (size=0x{instance_size:X}) or has an invalid tag mask",
        field.offset,
        field.mask
    );
    Ok(TaskContextBinding {
        field,
        instance_size,
    })
}

pub(super) fn data_model_task_binding(
    memory: &ProcessMemory,
    studio: &ModuleEntry,
    layout: &PackageLayout,
    model: &ActiveDataModel,
) -> Result<TaskContextBinding> {
    let fields = data_model_context_fields(memory, studio, layout, model)
        .context("DataModel capture task-context discovery failed")?;
    let instance_size = [
        (model.layout.self_pointer, 16),
        (model.layout.class_descriptor, 8),
        (model.layout.children, 8),
        (model.layout.name, 8),
    ]
    .into_iter()
    .try_fold(0, |span, (offset, size)| {
        Ok::<_, anyhow::Error>(
            span.max(
                offset
                    .checked_add(size)
                    .context("DataModel validated Instance prefix overflowed")?,
            ),
        )
    })?;
    let binding = bind_context_field(&fields, instance_size)?;
    crate::app::output::log_global(
        4,
        format_args!(
            "[renium] native capture context discovery: pid={} offset=0x{:X} mask=0x{:016X} pointer=0x{:016X} instanceSize=0x{:X}",
            memory.pid,
            binding.field.offset,
            binding.field.mask,
            binding.field.pointer,
            binding.instance_size
        ),
    );
    Ok(binding)
}

impl TaskContextBinding {
    pub(super) fn encode(self, parameters: &mut [u8]) -> Result<()> {
        anyhow::ensure!(
            parameters.len() == PARAM_SIZE,
            "Invalid capture parameter size"
        );
        anyhow::ensure!(
            read_u32(parameters, PARAM_PROTOCOL_VERSION)? == SERIALIZER_VERSION,
            "Incompatible native serializer protocol"
        );
        anyhow::ensure!(
            valid_field(self.field, self.instance_size),
            "Invalid capture context binding"
        );
        put_u64(parameters, PARAM_TASK_CONTEXT, self.field.pointer);
        put_u32(
            parameters,
            PARAM_CONTEXT_FIELD,
            u32::try_from(self.field.offset)?,
        );
        put_u32(
            parameters,
            PARAM_INSTANCE_SIZE,
            u32::try_from(self.instance_size)?,
        );
        put_u64(parameters, PARAM_CONTEXT_MASK, self.field.mask);
        put_u64(parameters, PARAM_CONTEXT_OBSERVED, 0);
        put_u32(parameters, PARAM_CONTEXT_READ, 0);
        Ok(())
    }

    pub(super) fn readings(self) -> serde_json::Value {
        serde_json::json!({
            "fieldOffset": self.field.offset,
            "pointerMask": format!("0x{:016X}", self.field.mask),
            "pointer": format!("0x{:016X}", self.field.pointer),
            "instanceSize": self.instance_size,
            "sizeContract": "validated Instance prefix",
            "protocolVersion": SERIALIZER_VERSION,
        })
    }

    pub(super) fn log_helper_reading(self, pid: u32, header: &[u8]) -> Result<()> {
        validate_capture_header(header)?;
        anyhow::ensure!(
            read_u32(header, 320)? as usize == self.field.offset
                && read_u32(header, 324)? as usize == self.instance_size
                && read_u64(header, 328)? as usize == self.field.mask,
            "Native capture helper returned a different context binding"
        );
        let read = read_u32(header, 344)?;
        anyhow::ensure!(read <= 1, "Invalid native capture context reading flag");
        let observed = read_u64(header, 336)? as usize;
        crate::app::output::log_global(
            3,
            format_args!(
                "[renium] native capture context: pid={pid} offset=0x{:X} mask=0x{:016X} host=0x{:016X} helper=0x{observed:016X} read={} matches={}",
                self.field.offset,
                self.field.mask,
                self.field.pointer,
                read == 1,
                read == 1 && observed == self.field.pointer
            ),
        );
        if read_u32(header, 8)? == 4 {
            anyhow::ensure!(
                read == 1 && observed == self.field.pointer,
                "Successful native capture omitted its matching context proof"
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn field(offset: usize, mask: usize) -> ContextField {
        ContextField {
            offset,
            mask,
            pointer: 0x12345000,
        }
    }

    #[test]
    fn relocated_context_retains_the_strongest_observed_mask() -> Result<()> {
        for offset in [0x48, 0x58, 0xa0, 0x180] {
            for tag in [0, 1, 7, 15, 255] {
                // MOV RAX,[RCX+offset]; AND RAX,mask; RET. Exercise the real
                // decoder's load-plus-mask candidates, including a relocated
                // slot and tags wider than the former fixed three bits.
                let mut code = vec![0x48, 0x8b, 0x81];
                code.extend_from_slice(&(offset as u32).to_le_bytes());
                code.extend_from_slice(&[0x48, 0x25]);
                code.extend_from_slice(&(!(tag as u32)).to_le_bytes());
                code.push(0xc3);
                let fields = properties::referenced_pointer_fields(&code)
                    .into_iter()
                    .map(|(offset, mask)| field(offset, mask))
                    .collect();
                let binding = bind_context_field(&fields, 0x188)?;
                assert_eq!(binding.field, field(offset, !tag));
                let readings = binding.readings();
                assert_eq!(readings["fieldOffset"], offset);
                assert_eq!(readings["pointerMask"], format!("0x{:016X}", !tag));
            }
        }
        Ok(())
    }

    #[test]
    fn unbound_ambiguous_or_out_of_instance_fields_degrade_capture() {
        let mut other_pointer = field(0x48, !7);
        other_pointer.pointer += 0x1000;
        for fields in [
            HashSet::new(),
            HashSet::from([field(0x48, !7), field(0x50, !7)]),
            HashSet::from([field(0x48, !7), other_pointer]),
            HashSet::from([field(0x48, !3), field(0x48, !5)]),
            HashSet::from([field(0x188, !7)]),
            HashSet::from([field(usize::MAX - 7, !7)]),
            HashSet::from([field(0x49, !7)]),
            HashSet::from([field(0x48, 0)]),
            HashSet::from([field(0x48, !5)]),
        ] {
            let result = bind_context_field(&fields, 0x188).map(TaskContextBinding::readings);
            let verdict = super::super::super::probes::ProbeVerdict::from_result(
                NativeOperation::Capture,
                &result,
            );
            let value = serde_json::to_value(verdict).unwrap();
            assert_eq!(value["operation"], "capture");
            assert_eq!(value["verdict"], "degraded");
            assert!(
                value["reason"]
                    .as_str()
                    .unwrap()
                    .contains("DataModel capture task-context")
            );
            assert!(
                value["fallback"]
                    .as_str()
                    .unwrap()
                    .contains("plugin serialization")
            );
        }
        let result = bind_context_field(&HashSet::from([field(0xa0, !15)]), 0x188)
            .map(TaskContextBinding::readings);
        let verdict = super::super::super::probes::ProbeVerdict::from_result(
            NativeOperation::Capture,
            &result,
        );
        assert_eq!(verdict.verdict, "native");
        assert_eq!(verdict.readings["fieldOffset"], 0xa0);
    }

    #[test]
    fn capture_discovery_failure_falls_back_once_without_refusal_retries() -> Result<()> {
        use crate::studio::native::capture::{export_with_fallback, retry_capture};
        let mut exports = Vec::new();
        let mut discoveries = 0;
        let (value, fallback) = export_with_fallback(
            |native| {
                exports.push(native);
                if !native {
                    return Ok(42);
                }
                retry_capture(
                    Duration::from_secs(1),
                    |_| {
                        discoveries += 1;
                        bind_context_field(&HashSet::new(), 0x188).map(|_| 0)
                    },
                    |_, _, delay| assert_eq!(delay, None),
                )
                .map(|(value, _)| value)
                .map_err(Into::into)
            },
            |_| {},
        )?;
        assert_eq!(value, 42);
        assert_eq!(exports, [true, false]);
        assert_eq!(discoveries, 1);
        assert_eq!(fallback.unwrap().attempts, 1);
        Ok(())
    }

    #[test]
    fn rust_and_cpp_capture_protocol_round_trip_and_bounds() -> Result<()> {
        let executable = concat!(env!("OUT_DIR"), "/renium-capture-context-test.exe");
        let output = std::process::Command::new(executable).output()?;
        anyhow::ensure!(
            output.status.success(),
            "C++ context regression failed: {output:?}"
        );
        let layout = String::from_utf8(output.stdout)?
            .split_whitespace()
            .map(str::parse::<usize>)
            .collect::<std::result::Result<Vec<_>, _>>()?;
        assert_eq!(
            layout,
            [
                SERIALIZER_VERSION as usize,
                PARAM_SIZE,
                PARAM_PROTOCOL_VERSION,
                PARAM_TASK_CONTEXT,
                PARAM_CONTEXT_FIELD,
                PARAM_INSTANCE_SIZE,
                PARAM_CONTEXT_MASK,
                PARAM_CONTEXT_OBSERVED,
                PARAM_CONTEXT_READ,
                CAPTURE_HEADER_SIZE,
                320,
                324,
                328,
                336,
                344
            ]
        );
        let directory = std::env::temp_dir().join(format!("renium-context-{}", std::process::id()));
        fs::create_dir(&directory)?;
        let _cleanup = crate::system::files::OnDrop::new(|| {
            let _ = fs::remove_dir_all(&directory);
        });
        let input = directory.join("input.bin");
        let output = directory.join("output.bin");
        for (offset, mask) in [(0x48, usize::MAX), (0xa0, !15), (0x180, !255)] {
            let binding = bind_context_field(&HashSet::from([field(offset, mask)]), 0x188)?;
            let mut parameters = vec![0; PARAM_SIZE];
            put_u32(&mut parameters, PARAM_PROTOCOL_VERSION, SERIALIZER_VERSION);
            put_u32(&mut parameters, PARAM_CAPTURE_MODE, 1);
            binding.encode(&mut parameters)?;
            fs::write(&input, &parameters)?;
            let status = std::process::Command::new(executable)
                .arg(&input)
                .arg(&output)
                .status()?;
            assert!(status.success());
            let result = fs::read(&output)?;
            assert_eq!(result.len(), PARAM_SIZE + CAPTURE_HEADER_SIZE);
            assert_eq!(read_u32(&result, PARAM_CONTEXT_FIELD)? as usize, offset);
            assert_eq!(read_u64(&result, PARAM_CONTEXT_MASK)? as usize, mask);
            assert_eq!(read_u32(&result, PARAM_INSTANCE_SIZE)?, 0x188);
            assert_eq!(
                read_u64(&result, PARAM_CONTEXT_OBSERVED)?,
                binding.field.pointer as u64
            );
            assert_eq!(read_u32(&result, PARAM_CONTEXT_READ)?, 1);
            binding.log_helper_reading(0, &result[PARAM_SIZE..])?;
            put_u64(
                &mut parameters,
                PARAM_CONTEXT_OBSERVED,
                binding.field.pointer,
            );
            put_u32(&mut parameters, PARAM_CONTEXT_READ, 1);
            put_u32(&mut parameters, PARAM_STATUS, 4);
            assert_eq!(&result[..PARAM_SIZE], &parameters);
        }
        Ok(())
    }

    #[test]
    fn capture_protocol_rejects_wrong_versions_sizes_and_missing_proof() -> Result<()> {
        let binding = bind_context_field(&HashSet::from([field(0xa0, !15)]), 0x188)?;
        for version in [0, 1, 3, u32::MAX] {
            let mut params = vec![0; PARAM_SIZE];
            put_u32(&mut params, PARAM_PROTOCOL_VERSION, version);
            assert!(binding.encode(&mut params).is_err());
            let mut header = vec![0; CAPTURE_HEADER_SIZE];
            put_u32(&mut header, 0, CAPTURE_HEADER_MAGIC);
            put_u32(&mut header, 4, version);
            assert!(validate_capture_header(&header).is_err());
        }
        for size in [0, 5904, PARAM_SIZE - 1, PARAM_SIZE + 8] {
            assert!(binding.encode(&mut vec![0; size]).is_err());
        }
        let mut header = vec![0; CAPTURE_HEADER_SIZE];
        put_u32(&mut header, 0, CAPTURE_HEADER_MAGIC);
        put_u32(&mut header, 4, SERIALIZER_VERSION);
        put_u32(&mut header, 8, 4);
        put_u32(&mut header, 320, 0xa0);
        put_u32(&mut header, 324, 0x188);
        put_u64(&mut header, 328, !15);
        assert!(binding.log_helper_reading(0, &header).is_err());
        put_u32(&mut header, 344, 1);
        assert!(binding.log_helper_reading(0, &header).is_err());
        put_u64(&mut header, 336, binding.field.pointer);
        binding.log_helper_reading(0, &header)?;
        put_u32(&mut header, 320, 0x58);
        assert!(binding.log_helper_reading(0, &header).is_err());
        Ok(())
    }
}
