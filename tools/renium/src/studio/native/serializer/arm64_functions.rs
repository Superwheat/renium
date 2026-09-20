use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Base {
    Descriptor,
    Stack,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Value {
    Address(Base, i64),
    Field(i64, usize),
    Instance,
    Adjustment(i64),
    Receiver(i64),
}

impl Value {
    fn offset(self, offset: i64) -> Option<Self> {
        match self {
            Self::Address(base, start) => Some(Self::Address(base, start.checked_add(offset)?)),
            _ => None,
        }
    }
    fn slice(self, offset: usize, size: usize) -> Option<Self> {
        match self {
            Self::Field(field, length) if offset.checked_add(size)? <= length => {
                Some(Self::Field(field + offset as i64, size))
            }
            _ if offset == 0 && size == 8 => Some(self),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct State {
    registers: [Option<Value>; 32],
    stack: BTreeMap<i64, (usize, Value)>,
}

impl State {
    fn load(&self, address: Option<Value>, size: usize) -> Option<Value> {
        let Value::Address(base, offset) = address? else {
            return None;
        };
        match base {
            Base::Descriptor if (8..=65520).contains(&offset) => Some(Value::Field(offset, size)),
            Base::Stack => {
                let (start, (length, value)) = self.stack.range(..=offset).next_back()?;
                let relative = usize::try_from(offset - start).ok()?;
                (relative.checked_add(size)? <= *length)
                    .then(|| value.slice(relative, size))
                    .flatten()
            }
            _ => None,
        }
    }
    fn store(&mut self, address: Option<Value>, size: usize, value: Option<Value>) {
        if let Some(Value::Address(Base::Stack, offset)) = address {
            self.stack.retain(|start, (length, _)| {
                *start + *length as i64 <= offset || *start >= offset + size as i64
            });
            if let Some(value) = value {
                self.stack.insert(offset, (size, value));
            }
        }
    }
    fn reg(&self, register: usize) -> Option<Value> {
        (register != 31)
            .then_some(self.registers[register])
            .flatten()
    }
    fn merge(&mut self, other: &Self) -> bool {
        let before = self.clone();
        for (left, right) in self.registers.iter_mut().zip(other.registers) {
            if *left != right {
                *left = None;
            }
        }
        self.stack
            .retain(|key, value| other.stack.get(key) == Some(value));
        *self != before
    }
    fn passed_origins(&self) -> bool {
        let mut values = Vec::new();
        for value in self.registers[..8].iter().flatten() {
            values.push(*value);
            if matches!(value, Value::Address(Base::Stack, _)) {
                values.extend(self.load(Some(*value), 8));
                values.extend(self.load(value.offset(8), 8));
            }
        }
        values.contains(&Value::Instance)
            && values.iter().any(|value| {
                matches!(
                    value,
                    Value::Field(..) | Value::Address(Base::Descriptor, _)
                )
            })
    }
    fn called_field(&self, register: usize) -> Option<usize> {
        let Value::Field(field, 8) = self.reg(register)? else {
            return None;
        };
        (field % 8 == 0 && self.registers[0] == Some(Value::Receiver(field + 8)))
            .then(|| usize::try_from(field).ok())
            .flatten()
    }
    fn single_memory(&mut self, word: u32) {
        let dst = (word & 31) as usize;
        let base = ((word >> 5) & 31) as usize;
        let vector = word & (1 << 26) != 0;
        let opc = (word >> 22) & 3;
        let size = if vector && opc & 2 != 0 {
            16
        } else {
            1 << (word >> 30)
        };
        let unsigned = word & 0x3b000000 == 0x39000000;
        let mode = (word >> 10) & 3;
        let offset = if unsigned {
            ((word >> 10) & 4095) as i64 * size as i64
        } else {
            ((word << 11) as i32 >> 23) as i64
        };
        let address = self.registers[base]
            .and_then(|v| v.offset(if !unsigned && mode == 1 { 0 } else { offset }));
        if opc & 1 != 0 || !vector && opc != 0 {
            if !vector {
                self.registers[dst] = if opc == 1 {
                    self.load(address, size)
                } else {
                    None
                };
            }
        } else {
            self.store(
                address,
                size,
                if vector {
                    None
                } else {
                    self.reg(dst).and_then(|v| v.slice(0, size))
                },
            );
        }
        if !unsigned && (mode == 1 || mode == 3) {
            self.registers[base] = self.registers[base].and_then(|v| v.offset(offset));
        }
    }

    fn pair_memory(&mut self, word: u32) {
        let dst = (word & 31) as usize;
        let base = ((word >> 5) & 31) as usize;
        let vector = word & (1 << 26) != 0;
        let size = if vector {
            4 << (word >> 30)
        } else if word >> 31 != 0 {
            8
        } else {
            4
        };
        let offset = (((word << 10) as i32 >> 25) as i64) * size as i64;
        let mode = (word >> 23) & 3;
        let address =
            self.registers[base].and_then(|v| v.offset(if mode == 1 { 0 } else { offset }));
        let second = ((word >> 10) & 31) as usize;
        if word & (1 << 22) != 0 {
            if !vector {
                let first = self.load(address, size);
                let next = self.load(address.and_then(|v| v.offset(size as i64)), size);
                self.registers[dst] = first;
                self.registers[second] = next;
            }
        } else {
            self.store(
                address,
                size,
                if vector {
                    None
                } else {
                    self.reg(dst).and_then(|v| v.slice(0, size))
                },
            );
            self.store(
                address.and_then(|v| v.offset(size as i64)),
                size,
                if vector {
                    None
                } else {
                    self.reg(second).and_then(|v| v.slice(0, size))
                },
            );
        }
        if mode == 1 || mode == 3 {
            self.registers[base] = self.registers[base].and_then(|v| v.offset(offset));
        }
    }

    fn data(&mut self, word: u32) {
        let dst = (word & 31) as usize;
        let base = ((word >> 5) & 31) as usize;
        let other = ((word >> 16) & 31) as usize;
        if word & 0xffe0ffe0 == 0xaa0003e0 {
            self.registers[dst] = self.reg(other);
        } else if word & 0x1f000000 == 0x11000000 {
            if word & (1 << 29) != 0 && dst == 31 {
                return;
            }
            let offset =
                (((word >> 10) & 4095) as i64) << if word & (1 << 22) != 0 { 12 } else { 0 };
            self.registers[dst] = if word >> 31 != 0 {
                self.registers[base].and_then(|v| {
                    v.offset(if word & (1 << 30) == 0 {
                        offset
                    } else {
                        -offset
                    })
                })
            } else {
                None
            };
        } else if word & 0xff200000 == 0x8b000000 {
            let shift = (word >> 10) & 63;
            let kind = (word >> 22) & 3;
            self.registers[dst] = match (self.reg(base), self.reg(other), kind, shift) {
                (Some(Value::Instance), Some(Value::Field(field, 8)), 2, 1) => {
                    Some(Value::Receiver(field))
                }
                (Some(Value::Instance), Some(Value::Adjustment(field)), 0, 0)
                | (Some(Value::Adjustment(field)), Some(Value::Instance), 0, 0) => {
                    Some(Value::Receiver(field))
                }
                _ => None,
            };
        } else if word & 0xfffffc00 == 0x9341fc00 {
            self.registers[dst] = match self.reg(base) {
                Some(Value::Field(field, 8)) => Some(Value::Adjustment(field)),
                _ => None,
            };
        } else if word & 0x3a000000 == 0x28000000 {
            self.pair_memory(word);
        } else if word & 0x3b000000 == 0x39000000 || word & 0x3b200000 == 0x38000000 {
            self.single_memory(word);
        } else if word & 0x3b200c00 == 0x38200000 {
            self.registers[dst] = None;
            self.store(self.registers[base], 1 << (word >> 30), None);
        } else if word & 0xffe00c00 == 0x9a800000 {
            self.registers[dst] = if self.reg(base) == self.reg(other) {
                self.reg(base)
            } else {
                None
            };
        } else if word & 0x3f00001f == 0x3100001f
            || word & 0x1f00001f == 0x0b00001f && word & (1 << 29) != 0
            || matches!(word, 0xd503201f | 0xd503233f | 0xd50323bf | 0xd503245f)
        {
        } else if word & 0x1e000000 != 0x0e000000 && dst != 31 {
            self.registers[dst] = None;
        } else if dst == 31
            && (word & 0x3f200000 == 0x0b200000
                || word & 0x1f000000 == 0x12000000 && word & 0x60000000 != 0x60000000)
        {
            self.registers[31] = None;
        }
    }
}

fn branch_target(pc: u64, word: u32, bits: u32, shift: u32) -> Option<u64> {
    let delta = ((word << (32 - bits - shift)) as i32 >> (32 - bits)) as i64 * 4;
    pc.checked_add_signed(delta)
}

struct Analysis<'a, C, D> {
    code: &'a mut C,
    descriptor: &'a mut D,
    budget: usize,
}

impl<C: FnMut(u64) -> Option<Vec<u8>>, D: FnMut(usize) -> Option<u64>> Analysis<'_, C, D> {
    fn run(&mut self, entry: u64, initial: State, depth: usize) -> Option<HashSet<usize>> {
        if depth > 8 {
            return None;
        }
        let bytes = (self.code)(entry)?;
        if bytes.len() > 65536 || bytes.len() % 4 != 0 {
            return None;
        }
        let words = bytes
            .chunks_exact(4)
            .map(|b| u32::from_le_bytes(b.try_into().unwrap()))
            .collect::<Vec<_>>();
        let index = |pc: u64| {
            usize::try_from(pc.checked_sub(entry)?)
                .ok()
                .filter(|offset| offset % 4 == 0 && *offset < bytes.len())
                .map(|offset| offset / 4)
        };
        let mut states = HashMap::from([(entry, initial)]);
        let mut pending = VecDeque::from([entry]);
        while let Some(pc) = pending.pop_front() {
            self.budget = self.budget.checked_sub(1)?;
            let word = words[index(pc)?];
            let mut state = states[&pc].clone();
            let mut next = vec![pc + 4];
            if word & 0xfffffc1f == 0xd65f0000
                || word & 0xfffffc1f == 0xd61f0000
                || word & 0xffe0001f == 0xd4200000
            {
                next.clear();
            } else if word & 0xfc000000 == 0x14000000 {
                let target = branch_target(pc, word, 26, 0)?;
                next = if index(target).is_some() {
                    vec![target]
                } else {
                    vec![]
                };
            } else if word & 0xfc000000 == 0x94000000 || word & 0xfffffc1f == 0xd63f0000 {
                state.registers[..19].fill(None);
            } else if word & 0xff000010 == 0x54000000
                || word & 0x7e000000 == 0x34000000
                || word & 0x7e000000 == 0x36000000
            {
                let test_bit = word & 0x7e000000 == 0x36000000;
                let target = branch_target(pc, word, if test_bit { 14 } else { 19 }, 5)?;
                next.push(target);
                if test_bit
                    && let Some(Value::Field(field, _)) = state.reg((word & 31) as usize)
                    && let Some(value) = (self.descriptor)(usize::try_from(field).ok()?)
                {
                    let bit = ((word >> 19) & 31) | ((word >> 26) & 32);
                    let taken = (value & (1 << bit) != 0) == (word & (1 << 24) != 0);
                    next = vec![if taken { target } else { pc + 4 }];
                }
            } else {
                state.data(word);
            }
            for target in next {
                index(target)?;
                let changed = if let Some(existing) = states.get_mut(&target) {
                    existing.merge(&state)
                } else {
                    states.insert(target, state.clone());
                    true
                };
                if changed {
                    pending.push_back(target);
                }
            }
        }
        let mut fields = HashSet::new();
        for (pc, state) in states {
            let word = words[index(pc)?];
            if word & 0xfffffc1f == 0xd63f0000 || word & 0xfffffc1f == 0xd61f0000 {
                fields.extend(state.called_field(((word >> 5) & 31) as usize));
            } else if (word & 0xfc000000 == 0x94000000
                || word & 0xfc000000 == 0x14000000
                    && index(branch_target(pc, word, 26, 0)?).is_none())
                && state.passed_origins()
            {
                let mut callee = state;
                callee.registers[19..31].fill(None);
                fields.extend(self.run(branch_target(pc, word, 26, 0)?, callee, depth + 1)?);
            }
        }
        Some(fields)
    }
}

pub(super) fn dispatch_field(
    entry: u64,
    mut code: impl FnMut(u64) -> Option<Vec<u8>>,
    mut descriptor: impl FnMut(usize) -> Option<u64>,
) -> Option<usize> {
    let mut registers = [None; 32];
    registers[0] = Some(Value::Address(Base::Descriptor, 0));
    registers[1] = Some(Value::Instance);
    registers[31] = Some(Value::Address(Base::Stack, 0));
    let fields = Analysis {
        code: &mut code,
        descriptor: &mut descriptor,
        budget: 32768,
    }
    .run(
        entry,
        State {
            registers,
            stack: BTreeMap::new(),
        },
        0,
    )?;
    let field = (fields.len() == 1).then(|| *fields.iter().next().unwrap())?;
    (descriptor(field + 8) == Some(0)).then_some(field)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bytes(words: &[u32]) -> Vec<u8> {
        words.iter().flat_map(|word| word.to_le_bytes()).collect()
    }

    fn wrapper(field: u32) -> Vec<u32> {
        vec![
            0xd10103ff,
            0xf9000be1,
            0x91000000 | (field << 10),
            0x910043e1,
            0x9400003c,
            0x910103ff,
            0xd65f03c0,
        ]
    }

    fn member_call() -> Vec<u32> {
        vec![
            0xd10103ff, 0xf9400028, 0xa9402415, 0x8b890514, 0x36000069, 0xf9400288, 0xb8754915,
            0xaa1403e0, 0xd63f02a0, 0x910103ff, 0xd65f03c0,
        ]
    }

    fn fixture(wrapper: &[u32], helper: &[u32], adjustment: u64) -> Option<usize> {
        dispatch_field(
            0,
            |address| match address {
                0 => Some(bytes(wrapper)),
                256 => Some(bytes(helper)),
                _ => None,
            },
            |_| Some(adjustment),
        )
    }

    #[test]
    fn relocated_fields_follow_the_receiver_through_stack_and_helpers() {
        for field in [8, 0x78, 0x80, 0x2f0, 0xff0] {
            assert_eq!(
                fixture(&wrapper(field), &member_call(), 0),
                Some(field as usize)
            );
            assert_eq!(fixture(&wrapper(field), &member_call(), 1), None);
            assert_eq!(fixture(&wrapper(field), &member_call(), 2), None);
            let mut helper = member_call();
            helper[1] = 0xf940002a;
            helper[3] = 0x8b890554;
            assert_eq!(fixture(&wrapper(field), &helper, 0), Some(field as usize));
        }
    }

    #[test]
    fn wrong_receiver_pair_shift_and_overwritten_stack_are_rejected() {
        for (index, replacement) in [(7, 0xaa1303e0), (3, 0x8b890914), (3, 0x8b950514)] {
            let mut helper = member_call();
            helper[index] = replacement;
            assert_eq!(fixture(&wrapper(0x78), &helper, 0), None);
        }
        let mut code = wrapper(0x78);
        code[1] = 0xb90013ff;
        assert_eq!(fixture(&code, &member_call(), 0), None);
        let mut code = wrapper(0x78);
        code[0] = 0x8b2063ff;
        assert_eq!(fixture(&code, &member_call(), 0), None);
    }

    #[test]
    fn tail_helpers_and_multiple_layers_are_traced_without_fixed_addresses() {
        let code = wrapper(0x300);
        assert_eq!(
            dispatch_field(
                0,
                |address| match address {
                    0 => Some(bytes(&code)),
                    256 => Some(bytes(&[0x14000040])),
                    512 => Some(bytes(&member_call())),
                    _ => None,
                },
                |_| Some(0)
            ),
            Some(0x300)
        );
        assert_eq!(
            dispatch_field(
                0,
                |address| (address == 0).then(|| bytes(&code)),
                |_| Some(0)
            ),
            None
        );
    }

    #[test]
    fn malformed_branches_adjacent_code_and_recursion_cannot_prove_a_binding() {
        let mut code = vec![0xd65f03c0];
        code.extend(wrapper(0x78));
        assert_eq!(fixture(&code, &member_call(), 0), None);
        assert_eq!(fixture(&[0x14000000], &member_call(), 0), None);
        assert_eq!(fixture(&[0x94000000, 0xd65f03c0], &member_call(), 0), None);
        assert_eq!(fixture(&[0x5400ffe0, 0xd65f03c0], &member_call(), 0), None);
    }

    #[test]
    #[ignore]
    fn captured_arm64_dispatch() {
        let records: serde_json::Value = serde_json::from_slice(
            &std::fs::read(std::env::var("RENIUM_FUNCTION_INSPECT_OUT").unwrap()).unwrap(),
        )
        .unwrap();
        for record in records.as_array().unwrap() {
            let methods = record["methods"]
                .as_array()
                .unwrap()
                .iter()
                .map(|method| {
                    (
                        method["rva"].as_u64().unwrap(),
                        method["code"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .map(|n| n.as_u64().unwrap() as u8)
                            .collect::<Vec<_>>(),
                    )
                })
                .collect::<Vec<_>>();
            let mut fields = HashSet::new();
            for (entry, _) in &methods {
                fields.extend(dispatch_field(
                    *entry,
                    |address| {
                        methods
                            .iter()
                            .filter(|(start, _)| *start <= address)
                            .max_by_key(|(start, _)| start)
                            .and_then(|(start, bytes)| {
                                let offset = usize::try_from(address.checked_sub(*start)?).ok()?;
                                bytes
                                    .get(offset..)
                                    .filter(|bytes| !bytes.is_empty())
                                    .map(|bytes| bytes.to_vec())
                            })
                    },
                    |field| {
                        let bytes = record["descriptor"].as_array()?;
                        Some(u64::from_le_bytes(
                            bytes
                                .get(field..field + 8)?
                                .iter()
                                .map(|n| n.as_u64().unwrap() as u8)
                                .collect::<Vec<_>>()
                                .try_into()
                                .unwrap(),
                        ))
                    },
                ));
            }
            assert_eq!(fields.len(), 1, "{}: {fields:?}", record["name"]);
        }
    }
}
