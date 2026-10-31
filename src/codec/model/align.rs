use super::row::RowState;
use super::strings::StrState;
use crate::codec::coder::Coder;
use crate::codec::ppm::{NOVEL, hash_bytes, mix};
use crate::codec::text::{is_kana, kata};

const KNOWN_SEGMENT_COST: f64 = 2.0;
const NEW_SEGMENT_BASE_COST: f64 = 5.0;
const NEW_SEGMENT_COST_PER_CHAR: f64 = 6.0;
const MAX_SEGMENT_LEN: usize = 8;
const UNREACHABLE_COST: f64 = f64::MAX / 4.0;
const PAIR_HASH_SEED: u64 = 0x5E6;

#[derive(Clone, Copy)]
pub(super) struct Run {
    pub kana: bool,
    pub start: usize,
    pub end: usize,
}

#[derive(Clone, Copy)]
pub(super) struct Span {
    pub start: usize,
    pub end: usize,
}

pub(super) fn split_script_runs(s: &str, out: &mut Vec<Run>) {
    out.clear();
    for (i, ch) in s.char_indices() {
        let kana = is_kana(ch);
        match out.last_mut() {
            Some(last) if last.kana == kana => last.end = i + ch.len_utf8(),
            _ => out.push(Run {
                kana,
                start: i,
                end: i + ch.len_utf8(),
            }),
        }
    }
}

fn match_kana_run(surface_run: &str, target: &str, pos: usize) -> Option<usize> {
    let mut p = pos;
    let mut t = target[pos..].chars();
    for ch in surface_run.chars() {
        match t.next() {
            Some(x) if x == kata(ch) => p += x.len_utf8(),
            _ => return None,
        }
    }
    Some(p)
}

fn match_from(
    surface: &str,
    script_runs: &[Run],
    target: &str,
    pos: usize,
    reading_spans: &mut Vec<Span>,
) -> bool {
    let Some(&Run {
        kana: is_kana_run,
        start: run_start,
        end: run_end,
    }) = script_runs.first()
    else {
        return pos == target.len();
    };
    if is_kana_run {
        return match match_kana_run(&surface[run_start..run_end], target, pos) {
            Some(p) => match_from(surface, &script_runs[1..], target, p, reading_spans),
            None => false,
        };
    }
    let mut end = pos;
    for ch in target[pos..].chars() {
        end += ch.len_utf8();
        reading_spans.push(Span { start: pos, end });
        if match_from(surface, &script_runs[1..], target, end, reading_spans) {
            return true;
        }
        reading_spans.pop();
    }
    false
}

pub(super) fn match_reading(
    surface: &str,
    script_runs: &[Run],
    target: &str,
    reading_spans: &mut Vec<Span>,
) -> bool {
    reading_spans.clear();
    match_from(surface, script_runs, target, 0, reading_spans)
}

fn pair_hash(ch: char, seg: &[char]) -> u64 {
    let mut h = mix(PAIR_HASH_SEED, ch as u64);
    for &x in seg {
        h = mix(h, x as u64);
    }
    h
}

fn segment_cost(known: bool, len: usize) -> f64 {
    if known {
        KNOWN_SEGMENT_COST
    } else {
        NEW_SEGMENT_BASE_COST + NEW_SEGMENT_COST_PER_CHAR * len as f64
    }
}

fn fill_segment_lengths(st: &StrState, chars: &[char], reading: &[char]) -> Vec<usize> {
    let (char_count, reading_len) = (chars.len(), reading.len());
    let stride = reading_len + 1;
    let mut cost = vec![UNREACHABLE_COST; (char_count + 1) * stride];
    let mut segment_len_at = vec![0usize; (char_count + 1) * stride];
    cost[0] = 0.0;
    for i in 0..char_count {
        for reading_pos in 0..=reading_len {
            let base = cost[i * stride + reading_pos];
            if base >= UNREACHABLE_COST {
                continue;
            }
            let longest = if i + 1 == char_count {
                reading_len - reading_pos
            } else {
                MAX_SEGMENT_LEN.min(reading_len - reading_pos)
            };
            for len in 0..=longest {
                let seg = &reading[reading_pos..reading_pos + len];
                let known = st.known.contains(&pair_hash(chars[i], seg));
                let idx = (i + 1) * stride + reading_pos + len;
                let total = base + segment_cost(known, len);
                if total < cost[idx] {
                    cost[idx] = total;
                    segment_len_at[idx] = len;
                }
            }
        }
    }
    segment_len_at
}

fn segment_reading(st: &StrState, chars: &[char], reading: &[char]) -> Vec<Span> {
    let char_count = chars.len();
    let reading_len = reading.len();
    let stride = reading_len + 1;
    let segment_len_at = fill_segment_lengths(st, chars, reading);
    let mut out = vec![Span { start: 0, end: 0 }; char_count];
    let mut reading_pos = reading_len;
    for i in (0..char_count).rev() {
        let len = segment_len_at[(i + 1) * stride + reading_pos];
        out[i] = Span {
            start: reading_pos - len,
            end: reading_pos,
        };
        reading_pos -= len;
    }
    out
}

fn code_segments(st: &mut StrState, coder: &mut Coder, run: &str, reading: &str, out: &mut String) {
    let dec = coder.is_decoder();
    let chars: Vec<char> = run.chars().collect();
    let rchars: Vec<char> = reading.chars().collect();
    let ranges = if dec {
        Vec::new()
    } else {
        segment_reading(st, &chars, &rchars)
    };
    out.clear();
    let mut piece = String::new();
    for (i, &ch) in chars.iter().enumerate() {
        let sym = if dec {
            NOVEL
        } else {
            let seg = &rchars[ranges[i].start..ranges[i].end];
            piece.clear();
            piece.extend(seg.iter());
            st.segs.find(piece.as_bytes()).unwrap_or(NOVEL)
        };
        let ctxs = [mix(80, ch as u64), 81];
        let id = if let Some(id) = st.seg_ppm.code(coder, &ctxs, sym) {
            id
        } else {
            let want = piece.clone();
            st.read_chars.code_str(coder, &[], &want, &mut piece);
            st.segs.find_or_add(piece.as_bytes())
        };
        st.seg_ppm.update(id);
        let text = std::str::from_utf8(st.segs.get(id)).unwrap_or("");
        out.push_str(text);
        let seg_chars: Vec<char> = text.chars().collect();
        st.known.insert(pair_hash(ch, &seg_chars));
    }
}

fn code_run_reading(
    st: &mut StrState,
    coder: &mut Coder,
    run: &str,
    reading: &str,
    coded_text: &mut String,
) -> u32 {
    let dec = coder.is_decoder();
    let sym = if dec {
        NOVEL
    } else {
        st.run_readings.find(reading.as_bytes()).unwrap_or(NOVEL)
    };
    let hit = st
        .run_ppm
        .code(coder, &[mix(12, hash_bytes(run.as_bytes()))], sym);
    let id = if let Some(id) = hit {
        id
    } else {
        code_segments(st, coder, run, reading, coded_text);
        st.run_readings.find_or_add(coded_text.as_bytes())
    };
    st.run_ppm.update(id);
    id
}

pub(super) fn code_aligned(st: &mut StrState, row: &mut RowState, coder: &mut Coder, col: usize) {
    let dec = coder.is_decoder();
    split_script_runs(&row.fields[0], &mut row.script_runs);
    if !dec {
        match_reading(
            &row.fields[0],
            &row.script_runs,
            &row.fields[col],
            &mut row.reading_spans,
        );
    }
    row.rebuilt.clear();
    let mut gi = 0;
    for &Run {
        kana: is_kana_run,
        start: run_start,
        end: run_end,
    } in &row.script_runs
    {
        if is_kana_run {
            row.rebuilt
                .extend(row.fields[0][run_start..run_end].chars().map(kata));
            continue;
        }
        let run = &row.fields[0][run_start..run_end];
        let reading = if dec {
            ""
        } else {
            &row.fields[col][row.reading_spans[gi].start..row.reading_spans[gi].end]
        };
        gi += 1;
        let id = code_run_reading(st, coder, run, reading, &mut row.coded_text);
        row.rebuilt
            .push_str(std::str::from_utf8(st.run_readings.get(id)).unwrap_or(""));
    }
    if dec {
        std::mem::swap(&mut row.fields[col], &mut row.rebuilt);
    }
}
