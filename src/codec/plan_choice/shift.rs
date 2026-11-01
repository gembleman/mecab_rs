use super::{Sample, ShiftPair};
use crate::codec::model::canonical_u64;
use crate::codec::table::Lexicon;

const LID_SHIFT_SEARCH_END: u8 = 62;
const MIN_ORDER_PAIRS: usize = 20;

fn is_shift_pair(lex: &Lexicon, sample: &Sample, a: usize, b: usize, shift: u8) -> bool {
    let mut nonzero = false;
    let mut varies = false;
    let mut first = None;
    for &(r, _) in sample {
        let (Some(x), Some(y)) = (
            canonical_u64(lex.field(r as usize, a)),
            canonical_u64(lex.field(r as usize, b)),
        ) else {
            return false;
        };
        if x >> shift != y {
            return false;
        }
        nonzero |= y != 0;
        varies |= *first.get_or_insert(y) != y;
    }
    nonzero && varies
}

pub(super) fn find_shift_pair(lex: &Lexicon, sample: &Sample, cols: &[usize]) -> Option<ShiftPair> {
    for &a in cols {
        for &b in cols {
            if a == b {
                continue;
            }
            if let Some(shift) =
                (1..=LID_SHIFT_SEARCH_END).find(|&s| is_shift_pair(lex, sample, a, b, s))
            {
                return Some(ShiftPair {
                    full_col: a,
                    partner_col: b,
                    shift,
                });
            }
        }
    }
    None
}

pub(super) fn ascending_rate(
    lex: &Lexicon,
    sample: &Sample,
    order_col: usize,
    id_col: usize,
    shift: u8,
) -> f64 {
    let mut pairs: Vec<(&[u8], u64)> = sample
        .iter()
        .map(|&(r, _)| {
            let id = canonical_u64(lex.field(r as usize, id_col)).unwrap_or(0) >> shift;
            (lex.field_bytes(r as usize, order_col), id)
        })
        .collect();
    pairs.sort();
    pairs.dedup_by(|a, b| a.0 == b.0);
    if pairs.len() < MIN_ORDER_PAIRS {
        return 0.0;
    }
    let good = pairs.windows(2).filter(|w| w[0].1 <= w[1].1).count();
    good as f64 / (pairs.len() - 1) as f64
}
