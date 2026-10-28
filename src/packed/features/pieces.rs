use std::collections::HashMap;

use super::bits_for;

pub(super) const POSITION_BYTES: usize = 8;
pub(super) const MAX_ENTRY_BITS: usize = u16::MAX as usize;
const MAX_PIECE_BITS: u32 = 32;
const SHIFT_AT: u32 = 32;
const BITS_AT: u32 = 48;
const CONSTANT_AT: u32 = 63;
pub(super) const CONSTANT_FLAG: u64 = 1 << CONSTANT_AT;

#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub(super) struct Position {
    pub(super) base: u32,
    pub(super) shift: u32,
    pub(super) bits: u32,
    pub(super) constant: bool,
}

impl Position {
    pub(super) fn to_bytes(self) -> [u8; POSITION_BYTES] {
        debug_assert!(self.shift <= u32::from(u16::MAX) && self.bits <= MAX_PIECE_BITS);
        let word = u64::from(self.base)
            | u64::from(self.shift) << SHIFT_AT
            | u64::from(self.bits) << BITS_AT
            | u64::from(self.constant) << CONSTANT_AT;
        word.to_le_bytes()
    }

    pub(super) fn parse(bytes: &[u8; POSITION_BYTES]) -> Option<Self> {
        let word = u64::from_le_bytes(*bytes);
        let (base, shift, bits) = unpack_position(word);
        let constant = word & CONSTANT_FLAG != 0;
        let reserved = (word & !CONSTANT_FLAG) >> (BITS_AT + 8);
        (reserved == 0 && bits <= MAX_PIECE_BITS && (!constant || bits == 0)).then_some(Self {
            base: base as u32,
            shift: shift as u32,
            bits,
            constant,
        })
    }
}

#[inline(always)]
pub(super) fn unpack_position(word: u64) -> (usize, usize, u32) {
    (
        word as u32 as usize,
        (word >> SHIFT_AT) as u16 as usize,
        (word >> BITS_AT) as u8 as u32,
    )
}

pub(super) struct Pieces {
    pub(super) positions: Vec<Position>,
    pub(super) offsets: Vec<u32>,
    pub(super) pool: Vec<u8>,
    pub(super) entries: Vec<u8>,
    pub(super) entry_bits: u32,
}

pub(super) fn split_program(program: &[u8]) -> Option<Vec<&[u8]>> {
    let mut pieces = Vec::new();
    let mut at = 0;
    while at < program.len() {
        let (start, length) = if program[at] < 255 {
            (at + 1, usize::from(program[at]))
        } else {
            let bytes = program.get(at + 1..at + 5)?;
            (at + 5, u32::from_le_bytes(bytes.try_into().ok()?) as usize)
        };
        pieces.push(program.get(start..start.checked_add(length)?)?);
        at = start + length;
    }
    Some(pieces)
}

pub(super) fn put_bits(bytes: &mut [u8], bit: usize, width: u32, value: u64) {
    for index in 0..width as usize {
        if value >> index & 1 != 0 {
            let at = bit + index;
            bytes[at >> 3] |= 1 << (at & 7);
        }
    }
}

pub(super) fn build(programs: &[Vec<u8>]) -> Option<Pieces> {
    let lists: Vec<Vec<&[u8]>> = programs
        .iter()
        .map(|program| split_program(program))
        .collect::<Option<_>>()?;
    let count = lists.iter().map(Vec::len).max().unwrap_or(0);
    let mut positions = Vec::with_capacity(count);
    let mut locals: Vec<HashMap<&[u8], u32>> = Vec::with_capacity(count);
    let mut offsets = vec![0u32];
    let mut pool = Vec::new();
    let mut shift = 0u32;
    for position in 0..count {
        let mut uses: HashMap<&[u8], u64> = HashMap::new();
        for list in &lists {
            if let Some(&piece) = list.get(position) {
                *uses.entry(piece).or_insert(0) += 1;
            }
        }
        let mut ranked: Vec<(&[u8], u64)> = uses.into_iter().collect();
        ranked.sort_by(|left, right| right.1.cmp(&left.1).then(left.0.cmp(right.0)));
        let bits = bits_for(ranked.len() as u64);
        if bits > MAX_PIECE_BITS {
            return None;
        }
        let constant = ranked.len() == 1 && ranked[0].0.len() <= usize::from(u16::MAX);
        positions.push(if constant {
            Position {
                base: u32::try_from(pool.len()).ok()?,
                shift: ranked[0].0.len() as u32,
                bits: 0,
                constant,
            }
        } else {
            Position {
                base: u32::try_from(offsets.len() - 1).ok()?,
                shift,
                bits,
                constant,
            }
        });
        if !constant {
            shift += bits;
        }
        let mut local = HashMap::with_capacity(ranked.len());
        for (number, &(piece, _)) in ranked.iter().enumerate() {
            pool.extend_from_slice(piece);
            offsets.push(u32::try_from(pool.len()).ok()?);
            local.insert(piece, number as u32);
        }
        locals.push(local);
    }
    if shift as usize > MAX_ENTRY_BITS {
        return None;
    }
    let mut entries = vec![0u8; (programs.len() * shift as usize).div_ceil(8)];
    for (template, list) in lists.iter().enumerate() {
        for (position, piece) in list.iter().enumerate() {
            let spec = positions[position];
            if spec.constant {
                continue;
            }
            let bit = template * shift as usize + spec.shift as usize;
            put_bits(
                &mut entries,
                bit,
                spec.bits,
                u64::from(locals[position][piece]),
            );
        }
    }
    Some(Pieces {
        positions,
        offsets,
        pool,
        entries,
        entry_bits: shift,
    })
}
