use super::coder::Coder;
use super::hints::HintReader;
use super::ppm::{NOVEL, Ppm, PpmHint, mix};

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

pub(crate) const CHAR_MODEL_PPMS: usize = 2;

pub(crate) struct NovelChar {
    blocks: Ppm,
}

impl NovelChar {
    fn new(hints: &mut HintReader) -> Self {
        NovelChar {
            blocks: hints.ppm(),
        }
    }

    fn code(&mut self, coder: &mut Coder, ch: u32) -> u32 {
        let block = ch >> BLOCK_SHIFT;
        let coded_block = match self.blocks.code(coder, &[1], block) {
            Some(known) => known,
            None => coder.uniform(u64::from(block), BLOCKS) as u32,
        };
        self.blocks.update(coded_block);
        let low = coder.raw_bits(u64::from(ch & ((1 << BLOCK_SHIFT) - 1)), BLOCK_SHIFT) as u32;
        (coded_block << BLOCK_SHIFT) | low
    }
}

pub(crate) struct CharModel {
    order: usize,
    ppm: Ppm,
    novel: NovelChar,
}

impl CharModel {
    pub(crate) fn new(order: usize, hints: &mut HintReader) -> Self {
        CharModel {
            order,
            ppm: hints.ppm(),
            novel: NovelChar::new(hints),
        }
    }

    pub(crate) fn hints(&self, out: &mut Vec<PpmHint>) {
        out.push(self.ppm.hint());
        out.push(self.novel.blocks.hint());
    }

    fn code_sym(&mut self, coder: &mut Coder, tail: &[u32], sym: u32) -> u32 {
        let mut context_len = tail.len().min(self.order);
        let mut ctxs = [0u64; MAX_CONTEXT_LEVELS];
        let mut level = 0;
        loop {
            let mut h = CONTEXT_SEED + context_len as u64;
            for &x in &tail[tail.len() - context_len..] {
                h = mix(h, u64::from(x));
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
        let is_end = coder.uniform(u64::from(sym == END), 2) == 1;
        if is_end {
            END
        } else {
            self.novel.code(coder, sym)
        }
    }

    #[inline]
    fn code_symbols(
        &mut self,
        coder: &mut Coder,
        tail: &[u32],
        mut wanted: impl Iterator<Item = u32>,
        mut emit: impl FnMut(u32),
    ) {
        let mut hist: Vec<u32> = tail.to_vec();
        loop {
            let want = if coder.is_decoder() {
                NOVEL
            } else {
                wanted.next().unwrap_or(END)
            };
            let got = self.code_sym(coder, &hist, want);
            if got == END || coder.overrun() || !coder.spend_symbol() {
                break;
            }
            emit(got);
            hist.push(got);
            if hist.len() > self.order + HIST_SLACK {
                hist.drain(..hist.len() - self.order);
            }
        }
    }

    pub(crate) fn code_str(
        &mut self,
        coder: &mut Coder,
        tail: &[u32],
        text: &str,
        out: &mut String,
    ) {
        out.clear();
        self.code_symbols(coder, tail, text.chars().map(|ch| ch as u32), |got| {
            out.push(char::from_u32(got).unwrap_or(char::REPLACEMENT_CHARACTER));
        });
    }

    pub(crate) fn code_bytes(&mut self, coder: &mut Coder, text: &[u8], out: &mut Vec<u8>) {
        out.clear();
        self.code_symbols(coder, &[], text.iter().map(|&b| u32::from(b)), |got| {
            out.push(got as u8);
        });
    }
}

pub(crate) fn is_kana(ch: char) -> bool {
    (HIRAGANA_FIRST..=KANA_LAST).contains(&ch)
}

pub(crate) fn kata(ch: char) -> char {
    if (HIRAGANA_FIRST..=HIRAGANA_LAST).contains(&ch) {
        char::from_u32(ch as u32 + HIRAGANA_TO_KATAKANA).unwrap_or(ch)
    } else {
        ch
    }
}
