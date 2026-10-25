//! Follow ARM64 argument provenance for the native local-test bootstrap.
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Value {
    Address(u64),
    Stack(i64),
    Input(i64),
    Payload(i64, i64),
    Field(i64, i64),
}
impl Value {
    fn offset(self, by: i64) -> Option<Self> {
        Some(match self {
            Self::Address(at) => Self::Address(at.checked_add_signed(by)?),
            Self::Stack(at) => Self::Stack(at.checked_add(by)?),
            Self::Input(at) => Self::Input(at.checked_add(by)?),
            Self::Payload(at, field) => Self::Payload(at, field.checked_add(by)?),
            _ => return None,
        })
    }
}

pub(super) fn branch(pc: u64, word: u32) -> Option<u64> {
    (word & 0x7c000000 == 0x14000000)
        .then(|| pc.wrapping_add_signed(((word << 6) as i32 >> 4) as i64))
}

/// Small conservative register/stack walk. Unknown writes discard provenance.
pub(super) fn walk(
    entry: u64,
    bytes: &[u8],
    mut visit: impl FnMut(u64, u32, &[Option<Value>; 32], &BTreeMap<i64, Value>),
) {
    let mut regs = [None; 32];
    regs[0] = Some(Value::Input(0));
    regs[31] = Some(Value::Stack(0));
    let mut stack = BTreeMap::new();
    for (index, bytes) in bytes.chunks_exact(4).enumerate() {
        let word = u32::from_le_bytes(bytes.try_into().unwrap());
        let pc = entry + index as u64 * 4;
        visit(pc, word, &regs, &stack);
        let dst = (word & 31) as usize;
        let base = ((word >> 5) & 31) as usize;
        if word & 0xfc000000 == 0x94000000 || word & 0xfffffc1f == 0xd63f0000 {
            regs[..19].fill(None);
        } else if word & 0x9f000000 == 0x90000000 {
            let imm = (((word >> 5) & 0x7ffff) << 2) | ((word >> 29) & 3);
            regs[dst] = Some(Value::Address(
                (pc & !4095).wrapping_add_signed((((imm << 11) as i32 >> 11) as i64) << 12),
            ));
        } else if word & 0x1f000000 == 0x11000000 {
            let imm =
                (((word >> 10) & 4095) as i64) << (if word & (1 << 22) != 0 { 12 } else { 0 });
            let value =
                regs[base].and_then(|v| v.offset(if word & (1 << 30) != 0 { -imm } else { imm }));
            if dst != 31 || word & (1 << 29) == 0 {
                regs[dst] = value;
            }
        } else if word & 0x7fe0ffe0 == 0x2a0003e0 {
            let source = ((word >> 16) & 31) as usize;
            if dst != 31 {
                regs[dst] = if source == 31 {
                    Some(Value::Address(0))
                } else {
                    regs[source]
                };
            }
        } else if word & 0x7f800000 == 0x52800000 {
            if dst != 31 {
                regs[dst] = Some(Value::Address(
                    (((word >> 5) & 65535) as u64) << (((word >> 21) & 3) * 16),
                ));
            }
        } else if word & 0x3b000000 == 0x39000000 || word & 0x3b200000 == 0x38000000 {
            single_access(word, &mut regs, &mut stack);
        } else if word & 0x3a000000 == 0x28000000 {
            pair_access(word, &mut regs, &mut stack);
        } else if word & 0x7c000000 == 0x14000000
            || word & 0xff000010 == 0x54000000
            || word & 0x7e000000 == 0x34000000
            || word & 0x7e000000 == 0x36000000
            || word & 0xfffffc1f == 0xd65f0000
            || word == 0xd503201f
        {
            // These instructions do not write a general-purpose register.
        } else if dst != 31 {
            regs[dst] = None;
        }
    }
}

fn load(address: Option<Value>, width: i64, stack: &BTreeMap<i64, Value>) -> Option<Value> {
    match (address, width) {
        (Some(Value::Stack(at)), 8) => stack.get(&at).copied(),
        (Some(Value::Input(at)), 8) => Some(Value::Payload(at, 0)),
        (Some(Value::Payload(at, field)), 8) => Some(Value::Field(at, field)),
        _ => None,
    }
}

// A register moving through a stack slot keeps its provenance; a vector or
// an unknown address only clears what it overlaps.
fn access(
    register: usize,
    vector: bool,
    load_access: bool,
    width: i64,
    address: Option<Value>,
    regs: &mut [Option<Value>; 32],
    stack: &mut BTreeMap<i64, Value>,
) {
    if load_access {
        if !vector && register != 31 {
            regs[register] = load(address, width, stack);
        }
    } else if let Some(Value::Stack(at)) = address {
        stack.retain(|key, _| *key + 8 <= at || *key >= at + width);
        if !vector
            && width == 8
            && let Some(value) = if register == 31 {
                Some(Value::Address(0))
            } else {
                regs[register]
            }
        {
            stack.insert(at, value);
        }
    }
}

fn single_access(word: u32, regs: &mut [Option<Value>; 32], stack: &mut BTreeMap<i64, Value>) {
    let dst = (word & 31) as usize;
    let base = ((word >> 5) & 31) as usize;
    let vector = word & (1 << 26) != 0;
    let opc = (word >> 22) & 3;
    let width = if vector && opc & 2 != 0 {
        16
    } else {
        1 << (word >> 30)
    };
    let unsigned = word & 0x3b000000 == 0x39000000;
    let mode = (word >> 10) & 3;
    let offset = if unsigned {
        ((word >> 10) & 4095) as i64 * width
    } else {
        ((word << 11) as i32 >> 23) as i64
    };
    let address =
        regs[base].and_then(|v| v.offset(if !unsigned && mode == 1 { 0 } else { offset }));
    let load_access = opc & 1 != 0 || !vector && opc != 0;
    access(dst, vector, load_access, width, address, regs, stack);
    if !unsigned && (mode == 1 || mode == 3) {
        regs[base] = regs[base].and_then(|v| v.offset(offset));
    }
}

fn pair_access(word: u32, regs: &mut [Option<Value>; 32], stack: &mut BTreeMap<i64, Value>) {
    let dst = (word & 31) as usize;
    let base = ((word >> 5) & 31) as usize;
    let vector = word & (1 << 26) != 0;
    let width = if vector {
        4 << (word >> 30)
    } else if word >> 31 != 0 {
        8
    } else {
        4
    };
    let offset = ((word << 10) as i32 >> 25) as i64 * width;
    let mode = (word >> 23) & 3;
    let address = regs[base].and_then(|v| v.offset(if mode == 1 { 0 } else { offset }));
    let second = ((word >> 10) & 31) as usize;
    let load_access = word & (1 << 22) != 0;
    for (lane, register) in [dst, second].into_iter().enumerate() {
        let address = address.and_then(|v| v.offset(lane as i64 * width));
        access(register, vector, load_access, width, address, regs, stack);
    }
    if mode == 1 || mode == 3 {
        regs[base] = regs[base].and_then(|v| v.offset(offset));
    }
}

pub(super) fn references(entry: u64, code: &[u8], wanted: &BTreeSet<u64>) -> BTreeSet<u64> {
    let mut found = BTreeSet::new();
    walk(entry, code, |_, word, regs, _| {
        if word & 0xff000000 == 0x91000000
            && let Some(Value::Address(base)) = regs[((word >> 5) & 31) as usize]
        {
            let value = base
                + ((((word >> 10) & 4095) as u64) << (if word & (1 << 22) != 0 { 12 } else { 0 }));
            if wanted.contains(&value) {
                found.insert(value);
            }
        }
    });
    found
}

pub(super) fn writers(
    entry: u64,
    code: &[u8],
    keys: &[BTreeSet<u64>; 3],
) -> Option<[(u64, u64); 3]> {
    let mut found: [BTreeSet<_>; 3] = Default::default();
    walk(entry, code, |pc, word, regs, _| {
        if word & 0xfc000000 == 0x94000000
            && let (Some(Value::Address(key)), Some(Value::Address(length))) = (regs[1], regs[2])
        {
            for i in 0..3 {
                if keys[i].contains(&key) && length == [6, 8, 11][i] {
                    found[i].insert((pc, branch(pc, word).unwrap()));
                }
            }
        }
    });
    let result = found.map(|v| (v.len() == 1).then(|| *v.first().unwrap()));
    let result = [result[0]?, result[1]?, result[2]?];
    (result[0].1 != result[1].1 && result[1].1 == result[2].1).then_some(result)
}

pub(super) fn server_input(entry: u64, code: &[u8], to_utf8: u64) -> Option<(u64, u64)> {
    let mut found = BTreeSet::new();
    walk(entry, code, |pc, word, regs, stack| {
        if word & 0xfc000000 == 0x94000000
            && branch(pc, word) == Some(to_utf8)
            && let Some(Value::Stack(at)) = regs[0]
            && let Some(Value::Field(capture, path)) = stack.get(&at)
            && [*capture, *path]
                .iter()
                .all(|at| (0..65536).contains(at) && at % 8 == 0)
        {
            found.insert((*capture as u64, *path as u64));
        }
    });
    (found.len() == 1).then(|| *found.first().unwrap())
}

pub(super) fn configuration_destructor(entry: u64, code: &[u8]) -> Option<u64> {
    let mut destination = None;
    let mut found = BTreeSet::new();
    walk(entry, code, |pc, word, regs, _| {
        if word & 0xfc000000 != 0x94000000 {
            return;
        }
        let (Some(Value::Stack(at)), Some(Value::Stack(sp))) = (regs[0], regs[31]) else {
            return;
        };
        // Exception cleanups are emitted after the normal epilogue, but enter
        // with the original frame. Track the buffer relative to that frame,
        // not the linear walk's already-restored absolute stack position.
        let offset = at - sp;
        if matches!(regs[1], Some(Value::Input(_))) {
            destination = Some(offset);
        } else if destination == Some(offset) {
            found.insert(branch(pc, word).unwrap());
        }
    });
    (found.len() == 1).then(|| *found.first().unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> serde_json::Value {
        serde_json::from_str(include_str!("../../../../native/tests/arm64-routing.json")).unwrap()
    }
    fn bytes(value: &serde_json::Value) -> Vec<u8> {
        let hex = value.as_str().unwrap();
        (0..hex.len())
            .step_by(2)
            .map(|at| u8::from_str_radix(&hex[at..at + 2], 16).unwrap())
            .collect()
    }
    #[test]
    fn arm64_zero_register_does_not_replace_the_stack_pointer() {
        let code = [
            0xd10083ffu32,
            0xf90003ff,
            0xf94003ff,
            0x910003e0,
            0x94000001,
        ]
        .into_iter()
        .flat_map(u32::to_le_bytes)
        .collect::<Vec<_>>();
        walk(0x1000, &code, |_, word, regs, stack| {
            if word == 0x94000001 {
                assert_eq!(regs[0], Some(Value::Stack(-32)));
                assert_eq!(stack.get(&-32), Some(&Value::Address(0)));
            }
        });
    }
    #[test]
    fn arm64_routes_follow_relocated_image_and_loader_fields() {
        let fixture = fixture();
        let writer = &fixture["writer"];
        for relocation in [0, 0x9000] {
            let keys = std::array::from_fn(|i| {
                writer["keys"][i]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|value| value.as_u64().unwrap() + relocation)
                    .collect()
            });
            for key in &keys {
                assert!(
                    !references(
                        writer["entry"].as_u64().unwrap() + relocation,
                        &bytes(&writer["code"]),
                        key
                    )
                    .is_empty()
                );
            }
            let result = writers(
                writer["entry"].as_u64().unwrap() + relocation,
                &bytes(&writer["code"]),
                &keys,
            )
            .unwrap();
            assert_ne!(result[0].1, result[1].1);
            assert_eq!(result[1].1, result[2].1);
        }
        let server = &fixture["server"];
        let entry = server["entry"].as_u64().unwrap();
        let utf8 = server["toUtf8"].as_u64().unwrap();
        let mut code = bytes(&server["code"]);
        assert_eq!(server_input(entry, &code, utf8), Some((8, 16)));
        for bytes in code.chunks_exact_mut(4) {
            let word = u32::from_le_bytes(bytes.try_into().unwrap());
            let next = match word {
                0xf9400668 => 0xf9401668, // capture +8 -> +40
                0xf9400908 => 0xf9401508, // file +16 -> +40
                _ => word,
            };
            bytes.copy_from_slice(&next.to_le_bytes());
        }
        assert_eq!(server_input(entry, &code, utf8), Some((40, 40)));
        assert_eq!(server_input(entry, &code, utf8 + 4), None);
    }
    #[test]
    fn arm64_configuration_cleanup_follows_exception_frame() {
        let fixture = fixture();
        let item = &fixture["configuration"];
        let entry = item["entry"].as_u64().unwrap();
        let destructor = item["destroy"].as_u64().unwrap();
        let mut code = bytes(&item["code"]);
        assert_eq!(configuration_destructor(entry, &code), Some(destructor));
        // Grow the compiler's local frame without moving the configuration
        // relative to SP, then relocate the whole image.
        for bytes in code.chunks_exact_mut(4) {
            let word = u32::from_le_bytes(bytes.try_into().unwrap());
            let next = match word {
                0xd10903ff => 0xd10b03ff,
                0x910903ff => 0x910b03ff,
                _ => word,
            };
            bytes.copy_from_slice(&next.to_le_bytes());
        }
        assert_eq!(
            configuration_destructor(entry + 0x9000, &code),
            Some(destructor + 0x9000)
        );
    }
}
