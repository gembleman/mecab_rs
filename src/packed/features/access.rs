use std::ops::Range;

#[inline(always)]
pub(super) fn range<const TRUSTED: bool>(bytes: &[u8], range: Range<usize>) -> &[u8] {
    if TRUSTED {
        unsafe { bytes.get_unchecked(range) }
    } else {
        &bytes[range]
    }
}

#[inline(always)]
pub(super) fn from<const TRUSTED: bool>(bytes: &[u8], start: usize) -> &[u8] {
    if TRUSTED {
        unsafe { bytes.get_unchecked(start..) }
    } else {
        &bytes[start..]
    }
}

#[inline(always)]
pub(super) fn word<const TRUSTED: bool>(bytes: &[u8], at: usize) -> u64 {
    if TRUSTED {
        u64::from_le(unsafe { bytes.as_ptr().add(at).cast::<u64>().read_unaligned() })
    } else {
        u64::from_le_bytes(bytes[at..at + 8].try_into().expect("eight bytes"))
    }
}

#[inline(always)]
pub(super) fn u16_at<const TRUSTED: bool>(bytes: &[u8], at: usize) -> usize {
    if TRUSTED {
        u16::from_le(unsafe { bytes.as_ptr().add(at).cast::<u16>().read_unaligned() }) as usize
    } else {
        u16::from_le_bytes(bytes[at..at + 2].try_into().expect("two bytes")) as usize
    }
}

#[inline(always)]
pub(super) fn u32_at<const TRUSTED: bool>(bytes: &[u8], at: usize) -> usize {
    if TRUSTED {
        u32::from_le(unsafe { bytes.as_ptr().add(at).cast::<u32>().read_unaligned() }) as usize
    } else {
        u32::from_le_bytes(bytes[at..at + 4].try_into().expect("four bytes")) as usize
    }
}
