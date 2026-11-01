use std::fmt;

pub(super) const SINGLE_CODES: usize = 127;
pub(super) const FIRST_LEAD: u8 = 0x80;
const LEAD_CODES: usize = 128;
const SECOND_CODES: usize = 255;
pub(super) const MAX_CHARS: usize = SINGLE_CODES + LEAD_CODES * SECOND_CODES;
pub(super) const ENTRY_BYTES: usize = 8;
pub(super) const MAX_EXPANSION: usize = 4;
const LENGTH_SHIFT: u32 = 32;
const LENGTH_BITS: u32 = 8;
const TEXT_BYTES: usize = 4;

pub(super) fn entry_of(ch: char) -> u64 {
    let mut text = [0u8; TEXT_BYTES];
    let length = ch.encode_utf8(&mut text).len();
    u64::from(u32::from_le_bytes(text)) | (length as u64) << LENGTH_SHIFT
}

pub(super) fn entry_is_char(entry: u64) -> bool {
    let length = (entry >> LENGTH_SHIFT) as usize;
    if length == 0 || length > TEXT_BYTES || entry >> (LENGTH_SHIFT + LENGTH_BITS) != 0 {
        return false;
    }
    let text = (entry as u32).to_le_bytes();
    std::str::from_utf8(&text[..length]).is_ok_and(|text| text.chars().count() == 1)
        && text[length..].iter().all(|&byte| byte == 0)
}

pub(super) fn push_code(rank: usize, out: &mut Vec<u8>) {
    debug_assert!(rank < MAX_CHARS);
    if rank < SINGLE_CODES {
        out.push(rank as u8 + 1);
    } else {
        let pair = rank - SINGLE_CODES;
        out.push(FIRST_LEAD + (pair / SECOND_CODES) as u8);
        out.push((pair % SECOND_CODES) as u8 + 1);
    }
}

#[inline(always)]
fn rank_of(first: u8, second: u8) -> usize {
    debug_assert!(first >= FIRST_LEAD && second != 0);
    SINGLE_CODES + usize::from(first - FIRST_LEAD) * SECOND_CODES + usize::from(second) - 1
}

pub(super) fn rank_at(codes: &[u8], at: usize) -> Option<(usize, usize)> {
    let first = *codes.get(at)?;
    if first == 0 {
        None
    } else if first < FIRST_LEAD {
        Some((usize::from(first) - 1, at + 1))
    } else {
        let second = *codes.get(at + 1)?;
        if second == 0 {
            return None;
        }
        Some((rank_of(first, second), at + 2))
    }
}

pub(super) fn are_codes(codes: &[u8], char_count: usize) -> bool {
    let mut at = 0;
    while at < codes.len() {
        match rank_at(codes, at) {
            Some((rank, next)) if rank < char_count => at = next,
            _ => return false,
        }
    }
    true
}

fn entry(table: &[u8], rank: usize) -> Option<u64> {
    let bytes = table.get(rank * ENTRY_BYTES..(rank + 1) * ENTRY_BYTES)?;
    Some(u64::from_le_bytes(bytes.try_into().ok()?))
}

pub(super) fn for_each_char(
    table: &[u8],
    codes: &[u8],
    mut put: impl FnMut(&str) -> fmt::Result,
) -> fmt::Result {
    let mut at = 0;
    while at < codes.len() {
        let Some((rank, next)) = rank_at(codes, at) else {
            return Ok(());
        };
        let Some(entry) = entry(table, rank) else {
            return Ok(());
        };
        let length = ((entry >> LENGTH_SHIFT) as usize).min(TEXT_BYTES);
        let text = (entry as u32).to_le_bytes();
        put(std::str::from_utf8(&text[..length]).unwrap_or(""))?;
        at = next;
    }
    Ok(())
}

#[inline(always)]
pub(super) unsafe fn decode_unchecked(
    table: &[u8],
    region: &[u8],
    start: usize,
    length: usize,
    target: *mut u8,
) -> usize {
    debug_assert!(start + length < region.len());
    let codes = unsafe { region.as_ptr().add(start) };
    let mut at = 0;
    let mut written = 0;
    while at < length {
        let first = unsafe { *codes.add(at) };
        let second = unsafe { *codes.add(at + 1) };
        let is_lead = usize::from(first >= FIRST_LEAD);
        let lead_mask = is_lead.wrapping_neg();
        let single = usize::from(first).wrapping_sub(1);
        let double = SINGLE_CODES - 1
            + usize::from(first.wrapping_sub(FIRST_LEAD)) * SECOND_CODES
            + usize::from(second);
        let rank = (double & lead_mask) | (single & !lead_mask);
        at += 1 + is_lead;
        debug_assert!(first != 0 && (is_lead == 0 || second != 0));
        debug_assert!((rank + 1) * ENTRY_BYTES <= table.len());
        let entry = u64::from_le(unsafe {
            table
                .as_ptr()
                .add(rank * ENTRY_BYTES)
                .cast::<u64>()
                .read_unaligned()
        });
        unsafe {
            target
                .add(written)
                .cast::<u64>()
                .write_unaligned(entry.to_le());
        };
        written += (entry >> LENGTH_SHIFT) as usize;
    }
    written
}
