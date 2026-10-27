
use super::coder::Coder;
use super::ppm::{NOVEL, Ppm, mix};

const END: u32 = 0x11_0000;
const BLOCK_SHIFT: u32 = 7;
const BLOCKS: u64 = (0x11_0000 >> BLOCK_SHIFT) as u64;

#[derive(Default)]
pub struct NovelChar {
    blocks: Ppm,
}

impl NovelChar {
    fn code(&mut self, c: &mut Coder, ch: u32) -> u32 {
        let block = ch >> BLOCK_SHIFT;
        let b = match self.blocks.code(c, &[1], block) {
            Some(b) => b,
            None => c.uniform(block as u64, BLOCKS) as u32,
        };
        self.blocks.update(b);
        let low = c.raw_bits((ch & ((1 << BLOCK_SHIFT) - 1)) as u64, BLOCK_SHIFT) as u32;
        (b << BLOCK_SHIFT) | low
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

    fn code_sym(&mut self, c: &mut Coder, tail: &[u32], sym: u32) -> u32 {
        let n = tail.len().min(self.order);
        let mut ctxs = [0u64; 8];
        let mut k = n;
        let mut level = 0;
        loop {
            let mut h = 0x5151 + k as u64;
            for &x in &tail[tail.len() - k..] {
                h = mix(h, x as u64);
            }
            ctxs[level] = h;
            level += 1;
            if k == 0 {
                break;
            }
            k -= 1;
        }
        let got = match self.ppm.code(c, &ctxs[..level], sym) {
            Some(s) => s,
            None => self.novel_end(c, sym),
        };
        self.ppm.update(got);
        got
    }

    fn novel_end(&mut self, c: &mut Coder, sym: u32) -> u32 {
        let is_end = c.uniform((sym == END) as u64, 2) == 1;
        if is_end { END } else { self.novel.code(c, sym) }
    }

    pub fn code_str(&mut self, c: &mut Coder, tail: &[u32], text: &str, out: &mut String) {
        out.clear();
        let mut hist: Vec<u32> = tail.to_vec();
        let mut it = text.chars();
        loop {
            let want = if c.is_decoder() {
                NOVEL
            } else {
                it.next().map_or(END, |ch| ch as u32)
            };
            let got = self.code_sym(c, &hist, want);
            if got == END || c.overrun() {
                break;
            }
            out.push(char::from_u32(got).unwrap_or('\u{FFFD}'));
            hist.push(got);
            if hist.len() > self.order + 4 {
                hist.drain(..hist.len() - self.order);
            }
        }
    }
}

impl CharModel {
    pub fn code_bytes(&mut self, c: &mut Coder, text: &[u8], out: &mut Vec<u8>) {
        out.clear();
        let mut hist: Vec<u32> = Vec::new();
        let mut it = text.iter();
        loop {
            let want = if c.is_decoder() {
                NOVEL
            } else {
                it.next().map_or(END, |&b| b as u32)
            };
            let got = self.code_sym(c, &hist, want);
            if got == END || c.overrun() {
                break;
            }
            out.push(got as u8);
            hist.push(got);
            if hist.len() > self.order + 4 {
                hist.drain(..hist.len() - self.order);
            }
        }
    }
}

pub fn is_kana(c: char) -> bool {
    ('\u{3041}'..='\u{30FF}').contains(&c)
}

pub fn kata(c: char) -> char {
    if ('\u{3041}'..='\u{3096}').contains(&c) {
        char::from_u32(c as u32 + 0x60).unwrap_or(c)
    } else {
        c
    }
}
