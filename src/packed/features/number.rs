use super::SLACK;

pub(super) const MAX_DIGITS: usize = 20;
const GROUP_LIMIT: u64 = 10_000;
const PAIR_BYTES: usize = 2;

const DIGIT_PAIRS: &[u8; 200] = b"00010203040506070809101112131415161718192021222324252627282930313233343536373839404142434445464748495051525354555657585960616263646566676869707172737475767778798081828384858687888990919293949596979899";

#[inline(always)]
pub(super) fn digit_count(value: u64) -> usize {
    value.checked_ilog10().map_or(1, |log| log as usize + 1)
}

#[inline(always)]
unsafe fn put_pair(pair: usize, target: *mut u8) {
    debug_assert!(pair < 100);
    unsafe {
        let source = DIGIT_PAIRS.as_ptr().add(pair * PAIR_BYTES);
        std::ptr::copy_nonoverlapping(source, target, PAIR_BYTES);
    }
}

#[inline(always)]
pub(super) unsafe fn write_digits(mut value: u64, count: usize, target: *mut u8) {
    debug_assert!((1..=MAX_DIGITS).contains(&count) && count == digit_count(value));
    let mut at = count;
    while value >= GROUP_LIMIT {
        let rest = (value % GROUP_LIMIT) as usize;
        value /= GROUP_LIMIT;
        at -= 4;
        unsafe {
            put_pair(rest / 100, target.add(at));
            put_pair(rest % 100, target.add(at + 2));
        }
    }
    let mut rest = value as usize;
    if rest >= 100 {
        at -= 2;
        unsafe { put_pair(rest % 100, target.add(at)) };
        rest /= 100;
    }
    if rest >= 10 {
        at -= 2;
        unsafe { put_pair(rest, target.add(at)) };
    } else {
        at -= 1;
        unsafe { *target.add(at) = b'0' + rest as u8 };
    }
    debug_assert_eq!(at, 0);
}

#[inline(always)]
pub(super) fn digits_of(value: u64) -> ([u8; MAX_DIGITS + SLACK], usize) {
    let mut digits = [0u8; MAX_DIGITS + SLACK];
    let count = digit_count(value);
    unsafe { write_digits(value, count, digits.as_mut_ptr()) };
    (digits, count)
}
