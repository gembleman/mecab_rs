use super::coder::Coder;
use super::ppm::{NOVEL, Ppm, mix};

const CHAR_CODE_LIMIT: u32 = 0x11_0000;
const END: u32 = CHAR_CODE_LIMIT;
const BLOCK_SHIFT: u32 = 7;
const BLOCKS: u64 = (CHAR_CODE_LIMIT >> BLOCK_SHIFT) as u64;
const HIST_SLACK: usize = 4;
const HIRAGANA_TO_KATAKANA: u32 = 0x60;
const HIRAGANA_FIRST: char = '\u{3041}';
const HIRAGANA_LAST: char = '\u{3096}';
const KANA_LAST: char = '\u{30FF}';
const CONTEXT_SEED: u64 = 0x5151;
const MAX_CONTEXT_LEVELS: usize = 8;

#[derive(Default)]
pub struct NovelChar {
    blocks: Ppm,
}

impl NovelChar {
    fn code(&mut self, coder: &mut Coder, ch: u32) -> u32 {
        let block = ch >> BLOCK_SHIFT;
        let coded_block = match self.blocks.code(coder, &[1], block) {
            Some(known) => known,
            None => coder.uniform(block as u64, BLOCKS) as u32,
        };
        self.blocks.update(coded_block);
        let low = coder.raw_bits((ch & ((1 << BLOCK_SHIFT) - 1)) as u64, BLOCK_SHIFT) as u32;
        (coded_block << BLOCK_SHIFT) | low
    }
}

pub struct CharModel {
    order: usize,
    ppm: Ppm,
    novel: NovelChar,
}

impl CharModel {
    pub fn new(order: usize) -> Self {
        CharModel {
            order,
            ppm: Ppm::default(),
            novel: NovelChar::default(),
        }
    }

    fn code_sym(&mut self, coder: &mut Coder, tail: &[u32], sym: u32) -> u32 {
        let mut context_len = tail.len().min(self.order);
        let mut ctxs = [0u64; MAX_CONTEXT_LEVELS];
        let mut level = 0;
        loop {
            let mut h = CONTEXT_SEED + context_len as u64;
            for &x in &tail[tail.len() - context_len..] {
                h = mix(h, x as u64);
            }
            ctxs[level] = h;
            level += 1;
            if context_len == 0 {
                break;
            }
            context_len -= 1;
        }
        let got = match self.ppm.code(coder, &ctxs[..level], sym) {
            Some(s) => s,
            None => self.novel_end(coder, sym),
        };
        self.ppm.update(got);
        got
    }

    fn novel_end(&mut self, coder: &mut Coder, sym: u32) -> u32 {
        let is_end = coder.uniform((sym == END) as u64, 2) == 1;
        if is_end {
            END
        } else {
            self.novel.code(coder, sym)
        }
    }

    pub fn code_str(&mut self, coder: &mut Coder, tail: &[u32], text: &str, out: &mut String) {
        out.clear();
        let mut hist: Vec<u32> = tail.to_vec();
        let mut it = text.chars();
        loop {
            let want = if coder.is_decoder() {
                NOVEL
            } else {
                it.next().map_or(END, |ch| ch as u32)
            };
            let got = self.code_sym(coder, &hist, want);
            if got == END || coder.overrun() {
                break;
            }
            out.push(char::from_u32(got).unwrap_or('\u{FFFD}'));
            hist.push(got);
            if hist.len() > self.order + HIST_SLACK {
                hist.drain(..hist.len() - self.order);
            }
        }
    }
}

impl CharModel {
    pub fn code_bytes(&mut self, coder: &mut Coder, text: &[u8], out: &mut Vec<u8>) {
        out.clear();
        let mut hist: Vec<u32> = Vec::new();
        let mut it = text.iter();
        loop {
            let want = if coder.is_decoder() {
                NOVEL
            } else {
                it.next().map_or(END, |&b| b as u32)
            };
            let got = self.code_sym(coder, &hist, want);
            if got == END || coder.overrun() {
                break;
            }
            out.push(got as u8);
            hist.push(got);
            if hist.len() > self.order + HIST_SLACK {
                hist.drain(..hist.len() - self.order);
            }
        }
    }
}

pub fn is_kana(ch: char) -> bool {
    (HIRAGANA_FIRST..=KANA_LAST).contains(&ch)
}

pub fn kata(ch: char) -> char {
    if (HIRAGANA_FIRST..=HIRAGANA_LAST).contains(&ch) {
        char::from_u32(ch as u32 + HIRAGANA_TO_KATAKANA).unwrap_or(ch)
    } else {
        ch
    }
}
