//! Follow the loader's captured configuration and reflected DataModel identities.
//! No instruction addresses, capture offsets, or register allocations are fixed.
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum Value {
    Capture(i64),
    ModelReference,
    Model(i64),
    Configuration(i64, i64),
    Identity(usize),
    Stack(i64),
    Zero,
    Lua,
    Output(i64),
    Field(i64),
    CopyFunction(i64),
}

impl Value {
    fn offset(self, by: i64) -> Option<Self> {
        Some(match self {
            Self::Capture(at) => Self::Capture(at.checked_add(by)?),
            Self::Model(at) => Self::Model(at.checked_add(by)?),
            Self::Configuration(field, at) => Self::Configuration(field, at.checked_add(by)?),
            Self::Stack(at) => Self::Stack(at.checked_add(by)?),
            Self::Output(at) => Self::Output(at.checked_add(by)?),
            other if by == 0 => other,
            _ => return None,
        })
    }
}

#[derive(Clone, Copy)]
struct Transfer {
    vector: bool,
    load: bool,
    size: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct State {
    registers: [Option<Value>; 32],
    vectors: [[Option<Value>; 2]; 32],
    stack: BTreeMap<i64, (usize, Value)>,
    output: BTreeMap<i64, (usize, Value)>,
    null_comparison: Option<Value>,
}

impl State {
    fn reg(&self, register: usize) -> Option<Value> {
        if register == 31 {
            Some(Value::Zero)
        } else {
            self.registers[register]
        }
    }

    fn load(&self, address: Option<Value>, size: usize, identities: [usize; 2]) -> Option<Value> {
        match address? {
            Value::ModelReference if size == 8 => Some(Value::Model(0)),
            Value::Capture(field) if size == 8 => Some(Value::Configuration(field, 0)),
            Value::Model(field) if size == 8 && identities == [usize::MAX; 2] => {
                Some(Value::Field(field))
            }
            Value::Field(field) if size == 8 => Some(Value::CopyFunction(field)),
            Value::Model(field) if size == 8 => identities
                .iter()
                .position(|offset| *offset as i64 == field)
                .map(Value::Identity),
            Value::Stack(at) => self
                .stack
                .get(&at)
                .filter(|(width, _)| *width == size)
                .map(|(_, value)| *value),
            _ => None,
        }
    }

    fn store(
        &mut self,
        address: Option<Value>,
        size: usize,
        value: Option<Value>,
        stores: &mut [HashSet<(i64, i64)>; 2],
    ) {
        match address {
            Some(Value::Stack(at)) => {
                self.stack.retain(|start, (width, _)| {
                    *start + *width as i64 <= at || *start >= at + size as i64
                });
                if let Some(value) = value {
                    self.stack.insert(at, (size, value));
                }
            }
            Some(Value::Configuration(capture, field)) if size == 8 => {
                if let Some(Value::Identity(index)) = value {
                    stores[index].insert((capture, field));
                }
            }
            Some(Value::Output(at)) => {
                self.output.retain(|start, (width, _)| {
                    *start + *width as i64 <= at || *start >= at + size as i64
                });
                if let Some(value) = value {
                    self.output.insert(at, (size, value));
                }
            }
            _ => {}
        }
    }

    fn transfer(
        &mut self,
        address: Option<Value>,
        register: usize,
        Transfer { vector, load, size }: Transfer,
        identities: [usize; 2],
        stores: &mut [HashSet<(i64, i64)>; 2],
    ) {
        if vector {
            if load {
                self.vectors[register] = if size == 16 {
                    [
                        self.load(address, 8, identities),
                        self.load(address.and_then(|a| a.offset(8)), 8, identities),
                    ]
                } else {
                    [None; 2]
                };
            } else if size == 16 {
                for lane in 0..2 {
                    self.store(
                        address.and_then(|a| a.offset(lane as i64 * 8)),
                        8,
                        self.vectors[register][lane],
                        stores,
                    );
                }
            } else {
                self.store(address, size, None, stores);
            }
        } else if load {
            if register != 31 {
                self.registers[register] = self.load(address, size, identities);
            }
        } else {
            self.store(address, size, self.reg(register), stores);
        }
    }

    // Load/store pair: both registers move through one address with a stride.
    fn pair(&mut self, word: u32, identities: [usize; 2], stores: &mut [HashSet<(i64, i64)>; 2]) {
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
        let transfer = Transfer {
            vector,
            load: word & (1 << 22) != 0,
            size,
        };
        self.transfer(address, dst, transfer, identities, stores);
        self.transfer(
            address.and_then(|a| a.offset(size as i64)),
            second,
            transfer,
            identities,
            stores,
        );
        if mode == 1 || mode == 3 {
            self.registers[base] = self.registers[base].and_then(|v| v.offset(offset));
        }
    }

    // Single register load/store with an unsigned or pre/post-indexed offset.
    fn single(&mut self, word: u32, identities: [usize; 2], stores: &mut [HashSet<(i64, i64)>; 2]) {
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
        let load = opc & 1 != 0 || !vector && opc != 0;
        if load && !vector && opc != 1 {
            if dst != 31 {
                self.registers[dst] = None;
            }
        } else {
            self.transfer(
                address,
                dst,
                Transfer { vector, load, size },
                identities,
                stores,
            );
        }
        if !unsigned && (mode == 1 || mode == 3) {
            self.registers[base] = self.registers[base].and_then(|v| v.offset(offset));
        }
    }

    fn instruction(
        &mut self,
        word: u32,
        identities: [usize; 2],
        stores: &mut [HashSet<(i64, i64)>; 2],
    ) {
        let dst = (word & 31) as usize;
        let base = ((word >> 5) & 31) as usize;
        let other = ((word >> 16) & 31) as usize;
        if word & 0xffe0ffe0 == 0xaa0003e0 {
            if dst != 31 {
                self.registers[dst] = self.reg(other);
            }
        } else if word & 0x1f000000 == 0x11000000 {
            let immediate =
                (((word >> 10) & 4095) as i64) << if word & (1 << 22) != 0 { 12 } else { 0 };
            if word & (1 << 29) != 0 {
                self.null_comparison = (dst == 31 && immediate == 0 && word & (1 << 30) != 0)
                    .then(|| self.reg(base))
                    .flatten();
                if dst == 31 {
                    return;
                }
            }
            self.registers[dst] = if word >> 31 != 0 {
                self.registers[base].and_then(|v| {
                    v.offset(if word & (1 << 30) == 0 {
                        immediate
                    } else {
                        -immediate
                    })
                })
            } else {
                None
            };
        } else if word & 0x3a000000 == 0x28000000 {
            self.pair(word, identities, stores);
        } else if word & 0x3b000000 == 0x39000000 || word & 0x3b200000 == 0x38000000 {
            self.single(word, identities, stores);
        } else if word & 0xbfe08400 == 0x2e000000 {
            // EXT's byte shift can rotate the two identity lanes together.
            let shift = ((word >> 11) & 15) as usize;
            self.vectors[dst] = if word & (1 << 30) != 0 && shift.is_multiple_of(8) {
                let lanes = [
                    self.vectors[base][0],
                    self.vectors[base][1],
                    self.vectors[other][0],
                    self.vectors[other][1],
                ];
                [lanes[shift / 8], lanes[shift / 8 + 1]]
            } else {
                [None; 2]
            };
        } else if word & 0xffe00c00 == 0x9a800000 {
            let left = self.reg(base);
            let right = self.reg(other);
            let condition = (word >> 12) & 15;
            self.registers[dst] = if left == right {
                left
            } else if condition == 0
                && left == Some(Value::Zero)
                && self.null_comparison == Some(Value::Model(0))
                && matches!(right, Some(Value::Model(_)))
            {
                right
            } else {
                None
            };
        } else if word & 0x3f00001f == 0x3100001f
            || word & 0x1f00001f == 0x0b00001f && word & (1 << 29) != 0
        {
            self.null_comparison = None;
        } else if matches!(word, 0xd503201f | 0xd503233f | 0xd50323bf | 0xd503245f) {
        } else if word & 0x1e000000 == 0x0e000000 {
            self.vectors[dst] = [None; 2];
        } else if dst != 31 {
            self.registers[dst] = None;
        } else if word & 0x3f200000 == 0x0b200000
            || word & 0x1f000000 == 0x12000000 && word & 0x60000000 != 0x60000000
        {
            self.registers[31] = None;
        }
    }

    fn merge(&mut self, other: &Self) -> bool {
        let previous = self.clone();
        for (a, b) in self.registers.iter_mut().zip(other.registers) {
            if *a != b {
                *a = None;
            }
        }
        for (a, b) in self
            .vectors
            .iter_mut()
            .flatten()
            .zip(other.vectors.iter().flatten())
        {
            if a != b {
                *a = None;
            }
        }
        self.stack
            .retain(|at, value| other.stack.get(at) == Some(value));
        self.output
            .retain(|at, value| other.output.get(at) == Some(value));
        if self.null_comparison != other.null_comparison {
            self.null_comparison = None;
        }
        *self != previous
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, serde::Serialize)]
pub(super) struct Fields {
    pub capture: usize,
    pub string: usize,
    pub place: usize,
    pub universe: usize,
}

/// The getter must return one complete 64-bit receiver field, without a call,
/// bit transform, or alternate return. This qualifies the identity width too.
pub(super) fn integer_field(code: &[u8]) -> Option<usize> {
    #[derive(Clone, Copy)]
    enum Scalar {
        Receiver(i64),
        Field(i64),
    }
    let mut registers = [None; 32];
    registers[0] = Some(Scalar::Receiver(0));
    if !code.len().is_multiple_of(4) {
        return None;
    }
    for bytes in code.chunks_exact(4) {
        let word = u32::from_le_bytes(bytes.try_into().ok()?);
        let dst = (word & 31) as usize;
        let base = ((word >> 5) & 31) as usize;
        if word == 0xd65f03c0 {
            return match registers[0]? {
                Scalar::Field(at) if at >= 0 && at % 8 == 0 => usize::try_from(at).ok(),
                _ => None,
            };
        } else if word & 0xffc00000 == 0xf9400000 {
            registers[dst] = match registers[base] {
                Some(Scalar::Receiver(at)) => {
                    Some(Scalar::Field(at + ((word >> 10) & 4095) as i64 * 8))
                }
                _ => None,
            };
        } else if word & 0xffe0ffe0 == 0xaa0003e0 {
            registers[dst] = registers[((word >> 16) & 31) as usize];
        } else if word & 0xbf000000 == 0x91000000 {
            let by = (((word >> 10) & 4095) as i64) << if word & (1 << 22) != 0 { 12 } else { 0 };
            registers[dst] = match registers[base] {
                Some(Scalar::Receiver(at)) => Some(Scalar::Receiver(
                    at + if word & (1 << 30) == 0 { by } else { -by },
                )),
                _ => None,
            };
        } else if word != 0xd503201f {
            return None;
        }
    }
    None
}

fn branch(pc: u64, word: u32, bits: u32, shift: u32) -> Option<u64> {
    pc.checked_add_signed(((word << (32 - bits - shift)) as i32 >> (32 - bits)) as i64 * 4)
}

struct Analysis<'a, C> {
    code: &'a mut C,
    identity: [usize; 2],
    instance: usize,
    budget: usize,
    stores: [HashSet<(i64, i64)>; 2],
    strings: HashSet<(i64, i64)>,
}

impl<C: FnMut(u64) -> Option<Vec<u8>>> Analysis<'_, C> {
    fn run(&mut self, entry: u64, initial: State, depth: usize) -> Option<()> {
        if depth > 6 {
            return None;
        }
        let code = (self.code)(entry)?;
        if code.len() > 65536 || !code.len().is_multiple_of(4) {
            return None;
        }
        let index = |pc: u64| {
            usize::try_from(pc.checked_sub(entry)?)
                .ok()
                .filter(|at| *at < code.len() && at.is_multiple_of(4))
        };
        let mut states = HashMap::from([(entry, initial)]);
        let mut queue = VecDeque::from([entry]);
        while let Some(pc) = queue.pop_front() {
            self.budget = self.budget.checked_sub(1)?;
            let at = index(pc)?;
            let word = u32::from_le_bytes(code[at..at + 4].try_into().ok()?);
            let mut state = states[&pc].clone();
            let mut next = vec![pc + 4];
            if word & 0xfffffc1f == 0xd65f0000
                || word & 0xfffffc1f == 0xd61f0000
                || word & 0xffe0001f == 0xd4200000
            {
                next.clear();
            } else if word & 0xfc000000 == 0x14000000 {
                let target = branch(pc, word, 26, 0)?;
                next = if index(target).is_some() {
                    vec![target]
                } else {
                    vec![]
                };
            } else if word & 0xfc000000 == 0x94000000 || word & 0xfffffc1f == 0xd63f0000 {
                state.registers[..19].fill(None);
                state.vectors.fill([None; 2]);
                state.null_comparison = None;
                if pc + 4 == entry + code.len() as u64 {
                    next.clear();
                }
            } else if word & 0xff000010 == 0x54000000
                || word & 0x7e000000 == 0x34000000
                || word & 0x7e000000 == 0x36000000
            {
                next.push(branch(
                    pc,
                    word,
                    if word & 0x7e000000 == 0x36000000 {
                        14
                    } else {
                        19
                    },
                    5,
                )?);
            } else {
                state.instruction(word, self.identity, &mut Default::default());
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
                    queue.push_back(target);
                }
            }
        }
        // Record only the converged states; transient loop facts cannot qualify a patch.
        for (pc, mut state) in states {
            let at = index(pc)?;
            let word = u32::from_le_bytes(code[at..at + 4].try_into().ok()?);
            let call = word & 0xfc000000 == 0x94000000
                || word & 0xfc000000 == 0x14000000 && index(branch(pc, word, 26, 0)?).is_none();
            if call {
                if let (Some(Value::Configuration(capture, string)), Some(Value::Model(instance))) =
                    (state.reg(0), state.reg(1))
                    && instance == self.instance as i64
                {
                    self.strings.insert((capture, string));
                } else if state.registers[..8]
                    .iter()
                    .any(|v| matches!(v, Some(Value::ModelReference | Value::Model(_))))
                    && state.registers[..8]
                        .iter()
                        .any(|v| matches!(v, Some(Value::Configuration(_, _) | Value::Capture(_))))
                {
                    // Helpers keep the same argument provenance and caller stack.
                    state.registers[19..31].fill(None);
                    self.run(branch(pc, word, 26, 0)?, state, depth + 1)?;
                }
            } else {
                state.instruction(word, self.identity, &mut self.stores);
            }
        }
        Some(())
    }
}

pub(super) fn producer_fields(
    entry: u64,
    identity: [usize; 2],
    instance: usize,
    mut code: impl FnMut(u64) -> Option<Vec<u8>>,
) -> Option<Fields> {
    let mut registers = [None; 32];
    registers[0] = Some(Value::Capture(0));
    registers[1] = Some(Value::ModelReference);
    registers[31] = Some(Value::Stack(0));
    let mut analysis = Analysis {
        code: &mut code,
        identity,
        instance,
        budget: 32768,
        stores: Default::default(),
        strings: Default::default(),
    };
    analysis.run(
        entry,
        State {
            registers,
            vectors: [[None; 2]; 32],
            stack: Default::default(),
            output: Default::default(),
            null_comparison: None,
        },
        0,
    )?;
    let mut found = HashSet::new();
    for (capture, string) in analysis.strings {
        for (place_capture, place) in &analysis.stores[0] {
            for (universe_capture, universe) in &analysis.stores[1] {
                if capture != *place_capture || capture != *universe_capture || place == universe {
                    continue;
                }
                let fields = [capture, string, *place, *universe];
                if !fields.iter().all(|v| (0..=65536).contains(v) && v % 8 == 0)
                    || *place < string + 24
                    || *universe < string + 24
                {
                    continue;
                }
                found.insert(Fields {
                    capture: capture as usize,
                    string: string as usize,
                    place: *place as usize,
                    universe: *universe as usize,
                });
            }
        }
    }
    (found.len() == 1).then(|| *found.iter().next().unwrap())
}

/// Prove a native shared-table push or Variant copy from its receiver and
/// output argument flow. Field positions and scratch registers may move.
pub(super) fn arrival_field(code: &[u8], variant: bool) -> Option<usize> {
    if code.is_empty() || !code.len().is_multiple_of(4) || code.len() > 65536 {
        return None;
    }
    let mut registers = [None; 32];
    registers[0] = Some(Value::Model(0));
    registers[1] = Some(Value::Lua);
    registers[8] = Some(Value::Output(0));
    registers[31] = Some(Value::Stack(0));
    let initial = State {
        registers,
        vectors: [[None; 2]; 32],
        stack: Default::default(),
        output: Default::default(),
        null_comparison: None,
    };
    let mut states = HashMap::from([(0u64, initial)]);
    let mut queue = VecDeque::from([0u64]);
    let mut budget = 32768usize;
    while let Some(pc) = queue.pop_front() {
        budget = budget.checked_sub(1)?;
        let word = u32::from_le_bytes(code.get(pc as usize..pc as usize + 4)?.try_into().ok()?);
        let mut state = states[&pc].clone();
        let mut next = vec![pc + 4];
        if word & 0xfffffc1f == 0xd65f0000 || word & 0xfffffc1f == 0xd61f0000 {
            next.clear();
        } else if word & 0xfc000000 == 0x14000000 {
            let target = branch(pc, word, 26, 0)?;
            next = if target < code.len() as u64 {
                vec![target]
            } else {
                vec![]
            };
        } else if word & 0xfc000000 == 0x94000000 || word & 0xfffffc1f == 0xd63f0000 {
            state.registers[..19].fill(None);
            state.vectors.fill([None; 2]);
            state.null_comparison = None;
            if pc + 4 == code.len() as u64 {
                next.clear();
            }
        } else if word & 0xff000010 == 0x54000000
            || word & 0x7e000000 == 0x34000000
            || word & 0x7e000000 == 0x36000000
        {
            next.push(branch(
                pc,
                word,
                if word & 0x7e000000 == 0x36000000 {
                    14
                } else {
                    19
                },
                5,
            )?);
        } else {
            state.instruction(word, [usize::MAX; 2], &mut Default::default());
        }
        for target in next {
            if !target.is_multiple_of(4) || target >= code.len() as u64 {
                return None;
            }
            let changed = if let Some(existing) = states.get_mut(&target) {
                existing.merge(&state)
            } else {
                states.insert(target, state.clone());
                true
            };
            if changed {
                queue.push_back(target);
            }
        }
    }
    let mut fields = HashSet::new();
    let mut copied = HashSet::new();
    for (pc, state) in states {
        let word = u32::from_le_bytes(code[pc as usize..pc as usize + 4].try_into().ok()?);
        let call = word & 0xfc000000 == 0x94000000 || word & 0xfffffc1f == 0xd63f0000;
        if variant {
            if word & 0xfffffc1f == 0xd65f0000 {
                let (8, Value::Field(field)) = *state.output.get(&0)? else {
                    return None;
                };
                if state.output.get(&8) != Some(&(8, Value::Field(field + 8))) {
                    return None;
                }
                fields.insert(field);
            } else if call {
                let Value::CopyFunction(manager) = state.reg(((word >> 5) & 31) as usize)? else {
                    return None;
                };
                if word & 0xfffffc1f != 0xd63f0000
                    || state.reg(0) != Some(Value::Model(manager + 8))
                    || state.reg(1) != Some(Value::Output(16))
                {
                    return None;
                }
                copied.insert(manager - 8);
            }
        } else if call
            && state.reg(0) == Some(Value::Lua)
            && let Some(Value::Model(field)) = state.reg(1)
        {
            fields.insert(field);
        }
    }
    let field = (fields.len() == 1).then(|| *fields.iter().next().unwrap())?;
    if !(16..65536).contains(&field) || field % 8 != 0 || variant && copied != fields {
        return None;
    }
    Some(field as usize)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arrival_fields_follow_receiver_and_complete_variant_copies() {
        let records: HashMap<String, Vec<u8>> =
            serde_json::from_str(include_str!("../../../../native/tests/arm64-arrival.json"))
                .unwrap();
        let join = &records["GetJoinData"];
        let local = &records["GetLocalPlayerTeleportData"];
        assert_eq!(arrival_field(join, false), Some(984));
        assert_eq!(arrival_field(local, true), Some(616));
        assert_eq!(arrival_field(join, true), None);
        assert_eq!(arrival_field(local, false), None);
        let mut relocated = local.clone();
        for instruction in relocated.chunks_exact_mut(4) {
            let word = u32::from_le_bytes(instruction.try_into().unwrap());
            // Relocate every access to the receiver's complete Variant by 0x80.
            let base = (word >> 5) & 31;
            let offset = (word >> 10) & 4095;
            let word = if word & 0xffc00000 == 0xf9400000
                && matches!(base, 0 | 20)
                && (77..=78).contains(&offset)
            {
                word + (16 << 10)
            } else if word & 0xffc003e0 == 0x91000000 {
                word + (128 << 10)
            } else {
                word
            };
            instruction.copy_from_slice(&word.to_le_bytes());
        }
        assert_eq!(arrival_field(&relocated, true), Some(744));
        let mut wrong_receiver = local.clone();
        wrong_receiver[32..36].copy_from_slice(&0xaa0103f4u32.to_le_bytes());
        assert_eq!(arrival_field(&wrong_receiver, true), None);
    }

    // Complete function from the installed ARM64 image. Its address is deliberately
    // absent: discovery must follow code/data relationships after relocation.
    fn producer() -> Vec<u8> {
        let hex = "ffc301d1f44f05a9fd7b06a9fd830191f30300aa280040f9a8831ef869bc02f029612d91290140393f0500716101005469a202d029613291290140393f050071c1000054082141f900f17d924e6d2b972100805231082e97a86300d1e97602b029411f9129017fb2e80700f9e91300f9e0a30091e1230091af340696e023009111200696a8bc02b008c13e91080140391f050071a1010054c8a1029008e1049108014039a9835ef82ac107913f0100f1e1038a9a090880520a4880521f0100714211891a0600001402008052a8835ef809c107911f0100f1e103899a600a40f9030080d2040080d2050080d2060080d277a12e97a8835ef8690a40f900e1c03d0040006e2081813c0abd41f92a1500f90a9943b92a3100b9692240393f0500710102005400010c911c2a2d97080040f908cd41f900013fd6680a40f909b14139018047ad0238c03d02c1833c01c1843c00c1853c690000372900805209b10139e0a3009194340696fd7b46a9f44f45a9ffc30191c0035fd6f30300aae0a300918d340696e00313aade231294";
        (0..hex.len())
            .step_by(2)
            .map(|at| u8::from_str_radix(&hex[at..at + 2], 16).unwrap())
            .collect()
    }

    fn resolve(bytes: &[u8], identity: [usize; 2]) -> Option<Fields> {
        producer_fields(0x100000000, identity, 0x1f0, |at| {
            (at == 0x100000000).then(|| bytes.to_vec())
        })
    }

    #[test]
    fn integer_identity_requires_an_unmodified_complete_field() {
        let bytes = |words: &[u32]| {
            words
                .iter()
                .flat_map(|v| v.to_le_bytes())
                .collect::<Vec<_>>()
        };
        assert_eq!(integer_field(&bytes(&[0xf9404400, 0xd65f03c0])), Some(0x88));
        assert_eq!(
            integer_field(&bytes(&[0x91008009, 0xf9404920, 0xd65f03c0])),
            Some(0xb0)
        );
        assert_eq!(integer_field(&bytes(&[0xb9408800, 0xd65f03c0])), None);
        assert_eq!(
            integer_field(&bytes(&[0xf9404400, 0x92400400, 0xd65f03c0])),
            None
        );
    }

    #[test]
    fn installed_arm64_loader_tracks_rotated_identity_lanes() {
        assert_eq!(
            resolve(&producer(), [0x388, 0x380]),
            Some(Fields {
                capture: 0x10,
                string: 0,
                place: 0x18,
                universe: 0x20
            })
        );
        assert_eq!(resolve(&producer(), [0x390, 0x380]), None);
    }

    #[test]
    fn arm64_loader_tracks_relocated_capture_and_identity() {
        let mut bytes = producer();
        for instruction in bytes.chunks_exact_mut(4) {
            let word = u32::from_le_bytes(instruction.try_into().unwrap());
            let relocated = match word {
                0xf9400a60 => 0xf9401e60, // captured configuration +0x10 -> +0x38
                0xf9400a69 => 0xf9401e69,
                0xf9400a68 => 0xf9401e68,
                0x3dc0e100 => 0x3dc10100, // identity lanes +0x380 -> +0x400
                _ => word,
            };
            instruction.copy_from_slice(&relocated.to_le_bytes());
        }
        assert_eq!(
            resolve(&bytes, [0x408, 0x400]),
            Some(Fields {
                capture: 0x38,
                string: 0,
                place: 0x18,
                universe: 0x20
            })
        );
        assert_eq!(resolve(&bytes, [0x388, 0x380]), None);
    }

    #[test]
    fn arm64_loader_rejects_wrong_receiver_and_incomplete_identity_copy() {
        let bytes = producer();
        assert_eq!(
            producer_fields(0x100000000, [0x388, 0x380], 0x200, |at| (at == 0x100000000)
                .then(|| bytes.clone())),
            None
        );
        let mut broken = bytes;
        let at = broken
            .chunks_exact(4)
            .position(|word| word == 0x6e004000u32.to_le_bytes())
            .unwrap()
            * 4;
        broken[at..at + 4].copy_from_slice(&0x4f00e400u32.to_le_bytes()); // overwrite q0 with zero
        assert_eq!(resolve(&broken, [0x388, 0x380]), None);
    }
}
