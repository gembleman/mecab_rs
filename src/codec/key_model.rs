use super::coder::{Coder, UintModel};
use super::ppm::{NOVEL, Ppm, mix};
use super::text::CharModel;

const KEY_CHAR_ORDER: usize = 2;
const TAIL_CONTEXT_CHARS: usize = 2;

pub(super) struct KeyModel {
    prefix_len: Ppm,
    prefix_len_escape: UintModel,
    first_char_delta: Ppm,
    first_char_delta_escape: UintModel,
    chars: CharModel,
    group_size: Ppm,
    group_size_escape: UintModel,
    prev: Vec<char>,
    cur: Vec<char>,
    last_prefix_len: u32,
}

fn saturate_u32(v: u64) -> u32 {
    u32::try_from(v).unwrap_or(u32::MAX)
}

impl KeyModel {
    pub(super) fn new() -> Self {
        KeyModel {
            prefix_len: Ppm::default(),
            prefix_len_escape: UintModel::default(),
            first_char_delta: Ppm::default(),
            first_char_delta_escape: UintModel::default(),
            chars: CharModel::new(KEY_CHAR_ORDER),
            group_size: Ppm::default(),
            group_size_escape: UintModel::default(),
            prev: Vec::new(),
            cur: Vec::new(),
            last_prefix_len: 0,
        }
    }

    fn encoder_shared_len(&self) -> usize {
        self.prev
            .iter()
            .zip(&self.cur)
            .take_while(|(a, b)| a == b)
            .count()
    }

    fn code_shared_len(&mut self, coder: &mut Coder, wanted: usize) -> usize {
        let prev_len = self.prev.len();
        let ctxs = [
            mix(mix(1, prev_len as u64), u64::from(self.last_prefix_len)),
            mix(1, prev_len as u64),
            2,
        ];
        let want = if coder.is_decoder() {
            NOVEL
        } else {
            wanted as u32
        };
        let shared_len = match self.prefix_len.code(coder, &ctxs, want) {
            Some(shared_len) => shared_len,
            None => saturate_u32(self.prefix_len_escape.code(coder, wanted as u64)),
        };
        self.prefix_len.update(shared_len);
        let shared_len = (shared_len as usize).min(prev_len);
        self.last_prefix_len = shared_len as u32;
        shared_len
    }

    fn code_first_char(&mut self, coder: &mut Coder, shared_len: usize) -> char {
        let prev_char = self.prev[shared_len] as u32;
        let char_before = if shared_len > 0 {
            self.prev[shared_len - 1] as u64
        } else {
            0
        };
        let delta = if coder.is_decoder() {
            0
        } else {
            (self.cur[shared_len] as u32).wrapping_sub(prev_char + 1)
        };
        let ctxs = [
            mix(mix(5, char_before), u64::from(prev_char)),
            mix(6, u64::from(prev_char)),
            7,
        ];
        let delta = match self.first_char_delta.code(coder, &ctxs, delta) {
            Some(delta) => delta,
            None => saturate_u32(self.first_char_delta_escape.code(coder, u64::from(delta))),
        };
        self.first_char_delta.update(delta);
        char::from_u32(prev_char.wrapping_add(1).wrapping_add(delta))
            .unwrap_or(char::REPLACEMENT_CHARACTER)
    }

    fn code_rest(&mut self, coder: &mut Coder, out: &mut String, rest_from: usize) {
        let mut tail: Vec<u32> = out
            .chars()
            .rev()
            .take(TAIL_CONTEXT_CHARS)
            .map(|ch| ch as u32)
            .collect();
        tail.reverse();
        let rest: String = if coder.is_decoder() {
            String::new()
        } else {
            self.cur[rest_from..].iter().collect()
        };
        let mut suffix = String::new();
        self.chars.code_str(coder, &tail, &rest, &mut suffix);
        out.push_str(&suffix);
    }

    pub(super) fn code_key(&mut self, coder: &mut Coder, key: &str, out: &mut String) {
        self.cur.clear();
        let mut wanted = 0;
        if !coder.is_decoder() {
            self.cur.extend(key.chars());
            wanted = self.encoder_shared_len();
        }
        let shared_len = self.code_shared_len(coder, wanted);
        out.clear();
        out.extend(&self.prev[..shared_len]);
        let mut rest_from = shared_len;
        if shared_len < self.prev.len() {
            let ch = self.code_first_char(coder, shared_len);
            out.push(ch);
            rest_from = shared_len + 1;
        }
        self.code_rest(coder, out, rest_from);
        self.prev.clear();
        self.prev.extend(out.chars());
    }

    pub(super) fn code_count(&mut self, coder: &mut Coder, key: &str, count: usize) -> usize {
        let last = key.chars().next_back().map_or(0, |ch| ch as u64);
        let ctxs = [mix(3, last), 4];
        let size = match self.group_size.code(coder, &ctxs, count as u32) {
            Some(size) => size,
            None => saturate_u32(
                self.group_size_escape
                    .code(coder, (count as u64).wrapping_sub(1)),
            )
            .saturating_add(1),
        };
        self.group_size.update(size);
        size as usize
    }
}
