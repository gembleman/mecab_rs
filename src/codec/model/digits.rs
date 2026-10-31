const CANONICAL_I64_LIMIT: u64 = 1 << 61;
const CANONICAL_U64_LIMIT: u64 = 1 << 62;
const DIGIT_PAIRS: &[u8; 200] = b"00010203040506070809101112131415161718192021222324252627282930313233343536373839404142434445464748495051525354555657585960616263646566676869707172737475767778798081828384858687888990919293949596979899";
const EIGHT_DIGITS: u64 = 100_000_000;
const PAIRS_PER_EIGHT_DIGITS: usize = 4;
const U64_DECIMAL_DIGITS: usize = 20;

pub(in crate::codec) fn canonical_i64(s: &str) -> Option<i64> {
    let v: i64 = s.parse().ok()?;
    if v.unsigned_abs() < CANONICAL_I64_LIMIT && v.to_string() == s {
        Some(v)
    } else {
        None
    }
}

pub(in crate::codec) fn canonical_u64(s: &str) -> Option<u64> {
    let v: u64 = s.parse().ok()?;
    if v < CANONICAL_U64_LIMIT && v.to_string() == s {
        Some(v)
    } else {
        None
    }
}

#[inline]
fn put_pair(buf: &mut [u8], at: &mut usize, pair: usize) {
    *at -= 2;
    buf[*at..*at + 2].copy_from_slice(&DIGIT_PAIRS[pair * 2..pair * 2 + 2]);
}

#[inline]
fn put_eight_digits(buf: &mut [u8], at: &mut usize, mut low: u32) {
    for _ in 0..PAIRS_PER_EIGHT_DIGITS {
        put_pair(buf, at, (low % 100) as usize);
        low /= 100;
    }
}

#[inline]
pub(super) fn set_u64(s: &mut String, v: u64) {
    let mut buf = [0u8; U64_DECIMAL_DIGITS];
    let mut i = buf.len();
    let mut v = v;
    while v >= EIGHT_DIGITS {
        put_eight_digits(&mut buf, &mut i, (v % EIGHT_DIGITS) as u32);
        v /= EIGHT_DIGITS;
    }
    let mut v = v as u32;
    while v >= 100 {
        put_pair(&mut buf, &mut i, (v % 100) as usize);
        v /= 100;
    }
    if v >= 10 {
        put_pair(&mut buf, &mut i, v as usize);
    } else {
        i -= 1;
        buf[i] = b'0' + v as u8;
    }
    s.clear();
    s.push_str(std::str::from_utf8(&buf[i..]).unwrap_or(""));
}

#[inline]
pub(super) fn set_i64(s: &mut String, v: i64) {
    if v < 0 {
        set_u64(s, v.unsigned_abs());
        s.insert(0, '-');
    } else {
        set_u64(s, v as u64);
    }
}
