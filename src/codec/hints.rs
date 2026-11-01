use super::ppm::{MAX_LEVELS, Ppm, PpmHint};
use crate::error::Undecodable;
use crate::wire::{get_varint, put_varint};

const MAX_HINT_CONTEXTS: u64 = 1 << 26;
const MAX_HINT_ARENA: u64 = 1 << 27;
const ARENA_PER_CONTEXT: u64 = 2;
const MIN_HINT_BYTES: usize = 2;
const MISMATCH_MESSAGE: &str = "the size hints do not match the model";

pub(super) fn write_hints(out: &mut Vec<u8>, hints: &[PpmHint]) {
    put_varint(out, hints.len() as u64);
    for hint in hints {
        put_varint(out, u64::from(hint.contexts));
        put_varint(out, u64::from(hint.arena));
    }
}

fn read_u32(data: &[u8], pos: &mut usize) -> Option<u32> {
    u32::try_from(get_varint(data, pos)?).ok()
}

pub(super) fn read_hints(data: &[u8], pos: &mut usize) -> Option<Vec<PpmHint>> {
    let count = usize::try_from(get_varint(data, pos)?).ok()?;
    if count > data.len().saturating_sub(*pos) / MIN_HINT_BYTES {
        return None;
    }
    let mut hints = Vec::with_capacity(count);
    for _ in 0..count {
        let contexts = read_u32(data, pos)?;
        let arena = read_u32(data, pos)?;
        hints.push(PpmHint { contexts, arena });
    }
    Some(hints)
}

pub(crate) struct HintReader<'a> {
    hints: &'a [PpmHint],
    taken: usize,
    trained: bool,
    max_contexts: u64,
}

impl<'a> HintReader<'a> {
    pub(super) fn untrained() -> Self {
        HintReader {
            hints: &[],
            taken: 0,
            trained: false,
            max_contexts: 0,
        }
    }

    pub(super) fn new(hints: &'a [PpmHint], symbol_budget: u64) -> Self {
        HintReader {
            hints,
            taken: 0,
            trained: true,
            max_contexts: symbol_budget
                .saturating_mul(MAX_LEVELS as u64)
                .min(MAX_HINT_CONTEXTS),
        }
    }

    fn clamp(&self, hint: PpmHint) -> PpmHint {
        let max_arena = self
            .max_contexts
            .saturating_mul(ARENA_PER_CONTEXT)
            .min(MAX_HINT_ARENA);
        PpmHint {
            contexts: u64::from(hint.contexts).min(self.max_contexts) as u32,
            arena: u64::from(hint.arena).min(max_arena) as u32,
        }
    }

    pub(super) fn ppm(&mut self) -> Ppm {
        if !self.trained {
            return Ppm::default();
        }
        let hint = self.hints.get(self.taken).copied().unwrap_or_default();
        self.taken += 1;
        Ppm::with_hint(self.clamp(hint))
    }

    pub(super) fn skip(&mut self, count: usize) {
        if self.trained {
            self.taken += count;
        }
    }

    pub(super) fn finish(&self) -> Result<(), Undecodable> {
        if self.trained && self.taken != self.hints.len() {
            return Err(Undecodable(MISMATCH_MESSAGE));
        }
        Ok(())
    }
}
