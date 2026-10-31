use super::align::{match_reading, split_script_runs};
use super::row::RowState;
use super::strings::{EX_ALIGNED, RefChoice, SourceRef, StrState};
use crate::codec::text::kata;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
struct RefCandidate {
    append_len: usize,
    trim: usize,
    index: usize,
    to_katakana: bool,
    common_prefix: usize,
}

fn common_prefix_len(a: impl Iterator<Item = char>, b: impl Iterator<Item = char>) -> usize {
    a.zip(b).take_while(|(x, y)| x == y).count()
}

fn source_candidates<'a>(
    srcs: &'a [usize],
    fields: &'a [String],
    target: &'a str,
    target_len: usize,
) -> impl Iterator<Item = RefCandidate> + 'a {
    srcs.iter().enumerate().flat_map(move |(index, &sc)| {
        let src = &fields[sc];
        let has_hira = src.chars().any(|ch| kata(ch) != ch);
        let slen = src.chars().count();
        (0..=usize::from(has_hira)).map(move |t| {
            let common_prefix = if t == 0 {
                common_prefix_len(src.chars(), target.chars())
            } else {
                common_prefix_len(src.chars().map(kata), target.chars())
            };
            RefCandidate {
                append_len: target_len - common_prefix,
                trim: slen - common_prefix,
                index,
                to_katakana: t == 1,
                common_prefix,
            }
        })
    })
}

fn recent_candidates<'a>(
    recent: &'a [String],
    first_index: usize,
    target: &'a str,
    target_len: usize,
) -> impl Iterator<Item = RefCandidate> + 'a {
    recent.iter().enumerate().map(move |(r, src)| {
        let common_prefix = common_prefix_len(src.chars(), target.chars());
        RefCandidate {
            append_len: target_len - common_prefix,
            trim: src.chars().count() - common_prefix,
            index: first_index + r,
            to_katakana: false,
            common_prefix,
        }
    })
}

fn choice_of(best: RefCandidate, target: &str) -> (u32, RefChoice) {
    let append_from = target
        .char_indices()
        .nth(best.common_prefix)
        .map_or(target.len(), |(i, _)| i);
    let source = SourceRef {
        index: best.index,
        to_katakana: best.to_katakana,
    };
    let ex = if best.append_len == 0 && best.trim == 0 {
        source.to_ex()
    } else {
        0
    };
    (
        ex,
        RefChoice {
            source,
            trim: best.trim,
            append_from,
        },
    )
}

fn aligned_reading_matches(row: &mut RowState, col: usize) -> bool {
    split_script_runs(&row.fields[0], &mut row.script_runs);
    match_reading(
        &row.fields[0],
        &row.script_runs,
        &row.fields[col],
        &mut row.reading_spans,
    )
}

pub(super) fn plan_str(
    st: &StrState,
    row: &mut RowState,
    col: usize,
    align: bool,
) -> (u32, RefChoice) {
    if align && aligned_reading_matches(row, col) {
        return (EX_ALIGNED, RefChoice::default());
    }
    let target = &row.fields[col];
    let target_len = target.chars().count();
    let from_sources = source_candidates(&st.srcs, &row.fields, target, target_len);
    let from_recent = recent_candidates(&st.recent, st.srcs.len(), target, target_len);
    let best = from_sources.chain(from_recent).min().unwrap_or_default();
    choice_of(best, target)
}
