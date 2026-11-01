use crate::utf8::is_continuation;

pub(super) const ASCII_LIMIT: u8 = 0x80;
const LEAD_PAYLOAD_MASK: u8 = 0x7F;
const CONTINUATION_PAYLOAD_BITS: u32 = 6;
const CONTINUATION_PAYLOAD_MASK: u8 = (1 << CONTINUATION_PAYLOAD_BITS) - 1;

#[inline(always)]
fn lead_payload_mask(width: usize) -> u32 {
    u32::from(LEAD_PAYLOAD_MASK >> width)
}

#[inline]
pub(super) fn decode(bytes: &[u8]) -> Option<(char, usize)> {
    let &first = bytes.first()?;
    if first < ASCII_LIMIT {
        return Some((char::from(first), 1));
    }
    let (width, minimum) = match first {
        0xC0..=0xDF => (2, 0x80),
        0xE0..=0xEF => (3, 0x800),
        0xF0..=0xF7 => (4, 0x1_0000),
        _ => return None,
    };
    let tail = bytes.get(1..width)?;
    let mut scalar = u32::from(first) & lead_payload_mask(width);
    for &byte in tail {
        if !is_continuation(byte) {
            return None;
        }
        scalar = scalar << CONTINUATION_PAYLOAD_BITS | u32::from(byte & CONTINUATION_PAYLOAD_MASK);
    }
    if scalar < minimum {
        return None;
    }
    char::from_u32(scalar).map(|char| (char, width))
}
