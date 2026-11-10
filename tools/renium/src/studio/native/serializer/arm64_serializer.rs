//! Local-save discovery follows the serializer call graph and libc++ ownership
//! contract. Stack frames, register allocation and object sizes are not locators.
use std::collections::{BTreeMap, BTreeSet};

pub(super) struct Code<'a> {
    pub bytes: &'a [u8],
    pub address: u64,
    pub starts: &'a [u64],
}

pub(super) fn branch(word: u32, address: u64) -> Option<u64> {
    (word & 0x7c00_0000 == 0x1400_0000)
        .then(|| address.wrapping_add_signed(((word << 6) as i32 >> 4) as i64))
}

impl Code<'_> {
    pub fn containing(&self, address: u64) -> Option<u64> {
        let offset = address.checked_sub(self.address)?;
        if offset >= self.bytes.len() as u64 {
            return None;
        }
        self.starts
            .get(
                self.starts
                    .partition_point(|start| *start <= address)
                    .checked_sub(1)?,
            )
            .copied()
    }

    pub fn body(&self, address: u64) -> Option<&[u8]> {
        let index = self.starts.binary_search(&address).ok()?;
        let end = self
            .starts
            .get(index + 1)
            .copied()
            .unwrap_or(self.address + self.bytes.len() as u64);
        self.bytes.get(
            usize::try_from(address.checked_sub(self.address)?).ok()?
                ..usize::try_from(end.checked_sub(self.address)?).ok()?,
        )
    }

    pub fn callers(&self, targets: &[u64]) -> BTreeMap<u64, BTreeSet<u64>> {
        let mut found = BTreeMap::<u64, BTreeSet<u64>>::new();
        for (index, bytes) in self.bytes.chunks_exact(4).enumerate() {
            let site = self.address + index as u64 * 4;
            let word = u32::from_le_bytes(bytes.try_into().unwrap());
            if let Some(target) = branch(word, site).filter(|target| targets.contains(target))
                && let Some(caller) = self.containing(site)
                && caller != target
            {
                found.entry(caller).or_default().insert(target);
            }
        }
        found
    }

    pub fn execute(&self, serializer: u64) -> Option<u64> {
        // The serialization predicate and file writer both call the logged
        // serializer; their common entry invokes both on its unchanged receiver.
        let children = self.callers(&[serializer]).into_keys().collect::<Vec<_>>();
        if children.len() != 2 {
            return None;
        }
        unique(
            self.callers(&children)
                .into_iter()
                .filter_map(|(caller, calls)| {
                    (calls.len() == 2 && receiver_calls(self.body(caller)?, caller, &children))
                        .then_some(caller)
                }),
        )
    }
}

fn unique(mut values: impl Iterator<Item = u64>) -> Option<u64> {
    let first = values.next()?;
    values.next().is_none().then_some(first)
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Value {
    Arg(usize),
    Loaded(usize, usize),
    Constant(u64),
    Heap(usize),
}

struct Flow {
    registers: [Option<Value>; 32],
}

impl Flow {
    fn new() -> Self {
        let mut registers = [None; 32];
        for (index, value) in registers[..9].iter_mut().enumerate() {
            *value = Some(Value::Arg(index));
        }
        Self { registers }
    }

    fn clobber(&mut self) {
        self.registers[..19].fill(None);
    }

    fn data(&mut self, word: u32, address: u64) {
        if word & 0xffff_fc1f == 0xd63f_0000 {
            self.clobber();
            return;
        }
        let dst = (word & 31) as usize;
        let base = ((word >> 5) & 31) as usize;
        let value = if word & 0xffe0_ffe0 == 0xaa00_03e0 {
            self.registers[((word >> 16) & 31) as usize]
        } else if word & 0x7f80_0000 == 0x5280_0000 {
            Some(Value::Constant(
                u64::from((word >> 5) & 0xffff) << (((word >> 21) & 3) * 16),
            ))
        } else if word & 0x9f00_0000 == 0x9000_0000 {
            let immediate = (((word >> 5) & 0x7ffff) << 2) | ((word >> 29) & 3);
            Some(Value::Constant((address & !4095).wrapping_add_signed(
                ((immediate << 11) as i32 >> 11) as i64 * 4096,
            )))
        } else if word & 0xffc0_0000 == 0x9100_0000 {
            let offset = ((word >> 10) & 4095) as usize;
            match self.registers[base] {
                Some(Value::Constant(value)) => {
                    value.checked_add(offset as u64).map(Value::Constant)
                }
                Some(Value::Heap(value)) => value.checked_add(offset).map(Value::Heap),
                Some(Value::Arg(0)) if offset == 24 => Some(Value::Heap(24)),
                value if offset == 0 => value,
                _ => None,
            }
        } else if word & 0xffc0_0000 == 0xf940_0000 || word & 0xffc0_0000 == 0x3940_0000 {
            let size = if word >> 30 == 3 { 8 } else { 1 };
            match self.registers[base] {
                Some(Value::Arg(arg)) if (word >> 10) & 4095 == 0 => Some(Value::Loaded(arg, size)),
                _ => None,
            }
        } else {
            // Stores, stack pairs and control flow do not produce a value.
            // Loads/restores invalidate their destination, including both pair registers.
            if word & 0x3b00_0000 == 0x2900_0000 {
                if word & (1 << 22) != 0 {
                    self.registers[dst] = None;
                    self.registers[((word >> 10) & 31) as usize] = None;
                }
                return;
            }
            if (word & 0x3b00_0000 == 0x3900_0000 || word & 0x3b20_0000 == 0x3800_0000)
                && word & (1 << 22) == 0
                || word & 0x7c00_0000 == 0x1400_0000
                || word & 0x7e00_0000 == 0x3400_0000
                || word & 0x7e00_0000 == 0x3600_0000
                || word & 0xff00_0010 == 0x5400_0000
            {
                return;
            }
            None
        };
        if dst != 31 {
            self.registers[dst] = value;
        }
    }
}

fn receiver_calls(bytes: &[u8], address: u64, targets: &[u64]) -> bool {
    let mut flow = Flow::new();
    let mut found = BTreeSet::new();
    for (index, bytes) in bytes.chunks_exact(4).enumerate() {
        let word = u32::from_le_bytes(bytes.try_into().unwrap());
        let site = address + index as u64 * 4;
        if word == 0xd65f_03c0 {
            break;
        }
        if let Some(target) = branch(word, site) {
            if targets.contains(&target) {
                if flow.registers[0] != Some(Value::Arg(0)) {
                    return false;
                }
                found.insert(target);
            }
            if word & 0xfc00_0000 == 0x9400_0000 {
                flow.clobber();
            }
        } else if word & 0xffff_fc1f == 0xd63f_0000 {
            flow.clobber();
        } else {
            flow.data(word, site);
        }
    }
    found.len() == targets.len()
}

/// The std::allocator control-block constructor initializes the two libc++
/// reference counts and vptr, then forwards QString*, *DataModel and *bool to
/// StudioLocalSaveHelper at control + 24. No engine field offsets are assumed.
pub(super) fn constructor(bytes: &[u8], address: u64, table: u64) -> bool {
    let mut flow = Flow::new();
    let mut counts = false;
    let mut vptr = false;
    for (index, bytes) in bytes.chunks_exact(4).enumerate() {
        let word = u32::from_le_bytes(bytes.try_into().unwrap());
        let site = address + index as u64 * 4;
        if word == 0xd65f_03c0 {
            break;
        }
        let base = ((word >> 5) & 31) as usize;
        counts |= word & 0xffc0_0000 == 0xa900_0000
            && word & 31 == 31
            && (word >> 10) & 31 == 31
            && ((word >> 15) & 127) == 1
            && flow.registers[base] == Some(Value::Arg(0));
        // STR Xt,[Xn],#24 writes the vptr and advances the constructor receiver.
        if word & 0xffe0_0c00 == 0xf800_0400
            && ((word >> 12) & 511) == 24
            && flow.registers[base] == Some(Value::Arg(0))
            && flow.registers[(word & 31) as usize] == Some(Value::Constant(table))
        {
            vptr = true;
            flow.registers[base] = Some(Value::Heap(24));
        } else if word & 0xfc00_0000 == 0x9400_0000 {
            if counts
                && vptr
                && flow.registers[..5]
                    == [
                        Some(Value::Heap(24)),
                        Some(Value::Arg(1)),
                        Some(Value::Loaded(2, 8)),
                        Some(Value::Loaded(3, 1)),
                        Some(Value::Constant(0)),
                    ]
            {
                return true;
            }
            flow.clobber();
        } else {
            flow.data(word, site);
        }
    }
    false
}

/// Verify the helper's Factory ABI: x8 is the indirect shared_ptr result;
/// x1..x3 are forwarded intact; owner and owner+24 are returned in that result.
pub(super) fn factory(bytes: &[u8], address: u64, constructor: u64, allocator: u64) -> bool {
    let mut flow = Flow::new();
    let mut allocated = false;
    let mut constructed = false;
    for (index, bytes) in bytes.chunks_exact(4).enumerate() {
        let word = u32::from_le_bytes(bytes.try_into().unwrap());
        let site = address + index as u64 * 4;
        if word == 0xd65f_03c0 {
            break;
        }
        if let Some(target) = branch(word, site) {
            if target == allocator
                && !allocated
                && matches!(flow.registers[0], Some(Value::Constant(size)) if (32..=1024*1024).contains(&size) && size % 8 == 0)
            {
                allocated = true;
                flow.clobber();
                flow.registers[0] = Some(Value::Heap(0));
            } else {
                if target == constructor {
                    if !allocated
                        || flow.registers[..4]
                            != [
                                Some(Value::Heap(0)),
                                Some(Value::Arg(1)),
                                Some(Value::Arg(2)),
                                Some(Value::Arg(3)),
                            ]
                    {
                        return false;
                    }
                    constructed = true;
                }
                flow.clobber();
            }
        } else if word & 0xffc0_0000 == 0xa900_0000
            && ((word >> 15) & 127) == 0
            && flow.registers[((word >> 5) & 31) as usize] == Some(Value::Arg(8))
        {
            return constructed
                && flow.registers[(word & 31) as usize] == Some(Value::Heap(24))
                && flow.registers[((word >> 10) & 31) as usize] == Some(Value::Heap(0));
        } else {
            flow.data(word, site);
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encoded(words: &[u32]) -> Vec<u8> {
        words.iter().flat_map(|word| word.to_le_bytes()).collect()
    }
    fn mov(dst: u32, src: u32) -> u32 {
        0xaa00_03e0 | src << 16 | dst
    }
    fn add(dst: u32, src: u32, offset: u32) -> u32 {
        0x9100_0000 | offset << 10 | src << 5 | dst
    }
    fn call(at: u64, target: u64) -> u32 {
        0x9400_0000 | (((target as i64 - at as i64) / 4) as u32 & 0x03ff_ffff)
    }

    #[test]
    fn save_graph_survives_relocation_and_unrelated_functions() {
        for base in [0x1000, 0x1_0080_0000] {
            let starts = [base, base + 0x200, base + 0x500, base + 0x900, base + 0xa00];
            let mut bytes = vec![0; 0xb00];
            let execute = encoded(&[
                0xd101_43ff,
                mov(22, 0),
                mov(0, 22),
                call(base + 12, base + 0x200),
                mov(0, 22),
                call(base + 20, base + 0x500),
                0xd65f_03c0,
            ]);
            bytes[..execute.len()].copy_from_slice(&execute);
            for offset in [0x200, 0x500] {
                bytes[offset..offset + 8].copy_from_slice(&encoded(&[
                    call(base + offset as u64, base + 0x900),
                    0xd65f_03c0,
                ]));
            }
            let execute = |bytes: &[u8]| {
                Code {
                    bytes,
                    address: base,
                    starts: &starts,
                }
                .execute(base + 0x900)
            };
            assert_eq!(execute(&bytes), Some(base));
            // A second matching parent must never be guessed away.
            let duplicate = encoded(&[
                mov(21, 0),
                call(base + 0xa04, base + 0x200),
                mov(0, 21),
                call(base + 0xa0c, base + 0x500),
                0xd65f_03c0,
            ]);
            bytes[0xa00..0xa00 + duplicate.len()].copy_from_slice(&duplicate);
            assert_eq!(execute(&bytes), None);
            bytes[0xa00..].fill(0);
            // Same graph, wrong receiver ABI.
            bytes[16..20].copy_from_slice(&add(0, 22, 8).to_le_bytes());
            assert_eq!(execute(&bytes), None);
        }
    }

    #[test]
    fn factory_requires_allocator_forwarding_and_shared_owner_result() {
        for (base, size, saved) in [
            (0x4000u64, 416u32, [19, 20, 21, 22, 23]),
            (0x1_0040_0000, 616, [24, 25, 26, 27, 28]),
        ] {
            let [out, heap, path, model, direct] = saved;
            let ctor = base + 0x1500;
            let alloc = base - 0x2000;
            let mut words = vec![
                mov(out, 8),
                mov(path, 1),
                mov(model, 2),
                mov(direct, 3),
                0x5280_0000 | size << 5,
                call(base + 20, alloc),
                mov(heap, 0),
                mov(1, path),
                mov(2, model),
                mov(3, direct),
                call(base + 40, ctor),
                add(1, heap, 24),
                0xa900_0000 | heap << 10 | out << 5 | 1,
                0xd65f_03c0,
            ];
            assert!(factory(&encoded(&words), base, ctor, alloc));
            assert!(!factory(&encoded(&words), base, ctor, alloc + 4));
            words[9] = mov(3, model);
            assert!(!factory(&encoded(&words), base, ctor, alloc));
            words[9] = mov(3, direct);
            words[11] = add(1, heap, 32);
            assert!(!factory(&encoded(&words), base, ctor, alloc));
            words[11] = add(1, heap, 24);
            // An indirect call can destroy the forwarded arguments even if
            // the allocated receiver is restored afterwards.
            words.splice(10..10, [0xd63f_0200, mov(0, heap)]);
            words[12] = call(base + 48, ctor);
            assert!(!factory(&encoded(&words), base, ctor, alloc));
        }
    }

    #[test]
    fn control_constructor_requires_counts_type_and_argument_widths() {
        let base = 0x1000;
        let table = 0x3900;
        let mut words = vec![
            0xa900_fc1f,
            0xd000_0009,
            add(9, 9, 0x900),
            0xf801_8409,
            0xf940_0042,
            0x3940_0063,
            0x5280_0004,
            call(base + 28, 0x2000),
            0xd65f_03c0,
        ];
        assert!(constructor(&encoded(&words), base, table));
        assert!(!constructor(&encoded(&words), base, table + 8));
        words[5] = 0xf940_0063;
        assert!(!constructor(&encoded(&words), base, table));
        words[5] = 0x3940_0063;
        words[0] = 0xa901_7c1f;
        assert!(!constructor(&encoded(&words), base, table));
    }
}
