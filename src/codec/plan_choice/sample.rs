use super::{ColInfo, Sample};
use crate::codec::model::{canonical_i64, canonical_u64};
use crate::codec::table::{Lexicon, Parsed};
use crate::codec::text::is_kana;
use std::collections::HashSet;
use std::ops::Range;

pub(super) fn build_sample(parsed: &Parsed, groups: &[Range<usize>], target: usize) -> Sample {
    let rows = parsed.order.len();
    let stride = (rows / target.max(1)).max(1);
    let mut out = Vec::new();
    for (gi, group) in groups.iter().enumerate() {
        if gi % stride != 0 {
            continue;
        }
        for (k, &r) in parsed.order[group.clone()].iter().enumerate() {
            out.push((r, k == 0));
        }
    }
    out
}

pub(super) fn col_info(lex: &Lexicon, sample: &Sample, col: usize) -> ColInfo {
    let mut set: HashSet<&str> = HashSet::new();
    let (mut all_int, mut all_u64, mut kana) = (true, true, 0usize);
    for &(r, _) in sample {
        let v = lex.field(r as usize, col);
        set.insert(v);
        all_int &= canonical_i64(v).is_some();
        all_u64 &= canonical_u64(v).is_some();
        kana += usize::from(v.chars().any(is_kana));
    }
    ColInfo {
        distinct: set.len(),
        all_int: all_int && !sample.is_empty(),
        all_u64: all_u64 && !sample.is_empty(),
        kana_share: kana as f64 / sample.len().max(1) as f64,
    }
}
