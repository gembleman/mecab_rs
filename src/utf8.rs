pub(crate) const FOUR_BYTE_LEAD: u8 = 0xf0;
const TWO_BYTE_LEAD: u8 = 0xc0;
const THREE_BYTE_LEAD: u8 = 0xe0;
const CONTINUATION_MASK: u8 = 0xc0;
const CONTINUATION_TAG: u8 = 0x80;
const TWO_BYTE_MASK: u8 = 0xe0;
const THREE_BYTE_MASK: u8 = 0xf0;
const FOUR_BYTE_MASK: u8 = 0xf8;
pub(crate) const TWO_BYTE_PAYLOAD: u8 = 0x1f;
pub(crate) const THREE_BYTE_PAYLOAD: u8 = 0x0f;
pub(crate) const FOUR_BYTE_PAYLOAD: u8 = 0x07;
pub(crate) const CONTINUATION_PAYLOAD: u8 = 0x3f;

#[inline(always)]
pub(crate) fn is_continuation(byte: u8) -> bool {
    byte & CONTINUATION_MASK == CONTINUATION_TAG
}

#[inline(always)]
pub(crate) fn is_two_byte_lead(byte: u8) -> bool {
    byte & TWO_BYTE_MASK == TWO_BYTE_LEAD
}

#[inline(always)]
pub(crate) fn is_three_byte_lead(byte: u8) -> bool {
    byte & THREE_BYTE_MASK == THREE_BYTE_LEAD
}

#[inline(always)]
pub(crate) fn is_four_byte_lead(byte: u8) -> bool {
    byte & FOUR_BYTE_MASK == FOUR_BYTE_LEAD
}

#[inline(always)]
pub(crate) fn sequence_len(lead: u8) -> usize {
    match lead {
        ..TWO_BYTE_LEAD => 1,
        TWO_BYTE_LEAD..THREE_BYTE_LEAD => 2,
        THREE_BYTE_LEAD..FOUR_BYTE_LEAD => 3,
        _ => 4,
    }
}
