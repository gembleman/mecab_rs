use std::fmt;

use super::SLACK;
use super::sink::Sink;

const MAX_DIGITS: usize = 20;

const DIGIT_PAIRS: &[u8; 200] = b"00010203040506070809101112131415161718192021222324252627282930313233343536373839404142434445464748495051525354555657585960616263646566676869707172737475767778798081828384858687888990919293949596979899";

pub(super) fn put_number<S: Sink + ?Sized>(sink: &mut S, mut value: u64) -> fmt::Result {
    let mut digits = [0u8; MAX_DIGITS + SLACK];
    let mut at = MAX_DIGITS;
    while value >= 10_000 {
        let rest = (value % 10_000) as usize;
        value /= 10_000;
        at -= 4;
        let (high, low) = (rest / 100 * 2, rest % 100 * 2);
        digits[at..at + 2].copy_from_slice(&DIGIT_PAIRS[high..high + 2]);
        digits[at + 2..at + 4].copy_from_slice(&DIGIT_PAIRS[low..low + 2]);
    }
    let mut rest = value as usize;
    if rest >= 100 {
        at -= 2;
        let low = rest % 100 * 2;
        digits[at..at + 2].copy_from_slice(&DIGIT_PAIRS[low..low + 2]);
        rest /= 100;
    }
    if rest >= 10 {
        at -= 2;
        digits[at..at + 2].copy_from_slice(&DIGIT_PAIRS[rest * 2..rest * 2 + 2]);
    } else {
        at -= 1;
        digits[at] = b'0' + rest as u8;
    }
    unsafe { sink.put_region(&digits, at, MAX_DIGITS - at) }
}
