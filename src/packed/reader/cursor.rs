use crate::packed::{UNIT_BYTES, Unit, VALUE_BITS};

pub(super) enum Step {
    Stop,
    Inner,
    Key(u32, u32),
}

const NO_CHILDREN: usize = 1 << (VALUE_BITS + 1);

#[inline(always)]
fn unit_at(cells: &[u8], index: usize) -> Option<Unit> {
    let bytes = cells.get(index * UNIT_BYTES..index * UNIT_BYTES + 8)?;
    Some(Unit::from_bits(u64::from_le_bytes(*bytes.first_chunk()?)))
}

pub(super) struct Cursor<'a> {
    cells: &'a [u8],
    base: usize,
}

impl<'a> Cursor<'a> {
    #[inline(always)]
    pub(super) fn new(cells: &'a [u8]) -> Self {
        Self {
            cells,
            base: unit_at(cells, 0).map_or(NO_CHILDREN, |root| root.value() as usize),
        }
    }

    #[inline(always)]
    pub(super) fn step(&mut self, code: u16) -> Step {
        if code == 0 {
            return Step::Stop;
        }
        let Some(unit) = unit_at(self.cells, self.base + usize::from(code)) else {
            return Step::Stop;
        };
        if unit.label() != code {
            return Step::Stop;
        }
        let value = unit.value();
        self.base = value as usize;
        if !unit.is_terminal() {
            Step::Inner
        } else if unit.is_inline() {
            self.base = NO_CHILDREN;
            Step::Key(value, u32::from(unit.inline_count()))
        } else if let Some(data) = unit_at(self.cells, value as usize) {
            Step::Key(data.value(), u32::from(data.flags()))
        } else {
            Step::Stop
        }
    }
}
