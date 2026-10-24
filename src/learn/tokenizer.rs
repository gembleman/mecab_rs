
use std::path::Path;

use super::{Config, failure};
use crate::build::char_def::CharProperty;
use crate::error::Error;
use crate::system_dictionary::{DictionaryKind, PrefixHit, SystemDictionary};

const DEFAULT_MAX_GROUPING_SIZE: usize = 24;
const MAX_LOOKUP_BYTES: usize = 65535;

fn default_type(info: u32) -> usize {
    ((info >> 18) & 0xff) as usize
}

fn length(info: u32) -> usize {
    ((info >> 26) & 0xf) as usize
}

fn group(info: u32) -> bool {
    (info >> 30) & 1 == 1
}

fn invoke(info: u32) -> bool {
    (info >> 31) & 1 == 1
}

fn is_kind_of(info: u32, other: u32) -> bool {
    info & other & ((1 << 18) - 1) != 0
}

pub(super) const NORMAL: u8 = 0;
pub(super) const UNKNOWN: u8 = 1;
pub(super) const BOS: u8 = 2;
pub(super) const EOS: u8 = 3;

#[derive(Clone, Copy)]
pub(super) enum FeatureSource {
    System(u32),
    Unknown(u32),
    Replaced,
}

#[derive(Clone, Copy)]
pub(super) struct Found {
    pub(super) position: usize,
    pub(super) length: usize,
    pub(super) rlength: usize,
    pub(super) kind: u8,
    pub(super) char_type: u8,
    pub(super) feature: FeatureSource,
}

struct Seek {
    at: usize,
    fail: u32,
    mblen: usize,
    clen: usize,
}

pub(super) struct Tokenizer {
    system: SystemDictionary,
    unknown: SystemDictionary,
    chars: CharProperty,
    unknown_tokens: Vec<std::ops::Range<usize>>,
    space: u32,
    unknown_feature: Option<Vec<u8>>,
    max_grouping_size: usize,
    pub(super) bos_feature: Vec<u8>,
    pub(super) charset: String,
    hits: Vec<PrefixHit>,
}

impl Tokenizer {
    pub(super) fn open(dic_dir: &Path, config: &Config) -> Result<Self, Error> {
        let unknown_path = dic_dir.join("unk.dic");
        let unknown = SystemDictionary::open(&unknown_path, DictionaryKind::Unknown)?;
        let chars = CharProperty::open(&dic_dir.join("char.bin"))?;
        let system = SystemDictionary::open(&dic_dir.join("sys.dic"), DictionaryKind::System)?;
        let mut unknown_tokens = Vec::with_capacity(chars.category_count());
        for category in 0..chars.category_count() {
            let name = chars.name(category);
            let run = unknown.exact_match(name).ok_or_else(|| {
                failure(
                    &unknown_path,
                    format!(
                        "cannot find UNK category: {}",
                        String::from_utf8_lossy(name)
                    ),
                )
            })?;
            unknown_tokens.push(run);
        }
        let bos_feature = config.text("bos-feature").as_bytes().to_vec();
        if bos_feature.is_empty() {
            return Err(failure(
                &dic_dir.join("dicrc"),
                "bos-feature is undefined in dicrc",
            ));
        }
        let unknown_feature = config.text("unk-feature");
        let mut max_grouping_size: usize = config.number("max-grouping-size");
        if max_grouping_size == 0 {
            max_grouping_size = DEFAULT_MAX_GROUPING_SIZE;
        }
        let charset = String::from_utf8_lossy(system.charset()).into_owned();
        let space = chars.char_info(b" ", 0, 1).0;
        Ok(Self {
            system,
            unknown,
            chars,
            unknown_tokens,
            space,
            unknown_feature: (!unknown_feature.is_empty())
                .then(|| unknown_feature.as_bytes().to_vec()),
            max_grouping_size,
            bos_feature,
            charset,
            hits: Vec::new(),
        })
    }

    pub(super) fn feature(&self, source: FeatureSource) -> &[u8] {
        match source {
            FeatureSource::System(offset) => self.system.feature(offset).as_bytes(),
            FeatureSource::Unknown(offset) => self.unknown.feature(offset).as_bytes(),
            FeatureSource::Replaced => self.unknown_feature.as_deref().unwrap_or_default(),
        }
    }

    fn seek_to_other_type(&self, text: &[u8], begin: usize, end: usize, start: u32) -> Seek {
        let mut seek = Seek {
            at: begin,
            fail: 0,
            mblen: 0,
            clen: 0,
        };
        let mut current = start;
        while seek.at < end {
            let (info, width) = self.chars.char_info(text, seek.at, end);
            seek.fail = info;
            seek.mblen = width;
            if !is_kind_of(current, info) {
                break;
            }
            seek.at += width;
            seek.clen += 1;
            current = info;
        }
        seek
    }

    pub(super) fn lookup(&mut self, text: &[u8], begin: usize, end: usize, found: &mut Vec<Found>) {
        found.clear();
        let end = if end - begin >= MAX_LOOKUP_BYTES {
            begin + MAX_LOOKUP_BYTES
        } else {
            end
        };
        let Seek {
            at: begin2,
            fail: cinfo,
            mblen,
            ..
        } = self.seek_to_other_type(text, begin, end, self.space);

        self.system
            .common_prefix_search(&text[begin2..end], &mut self.hits);
        for hit in &self.hits {
            for index in hit.tokens() {
                let token = self.system.token(index);
                found.push(Found {
                    position: begin2,
                    length: hit.length,
                    rlength: begin2 - begin + hit.length,
                    kind: NORMAL,
                    char_type: default_type(cinfo) as u8,
                    feature: FeatureSource::System(token.feature),
                });
            }
        }

        if !found.is_empty() && !invoke(cinfo) {
            return;
        }

        let mut begin3 = begin2 + mblen;
        let mut group_begin3 = None;

        if begin3 > end {
            self.add_unknown(found, cinfo, begin, begin2, begin3);
            if !found.is_empty() {
                return;
            }
        }

        if group(cinfo) {
            let tmp = begin3;
            let grouped = self.seek_to_other_type(text, begin3, end, cinfo);
            begin3 = grouped.at;
            if grouped.clen <= self.max_grouping_size {
                self.add_unknown(found, cinfo, begin, begin2, begin3);
            }
            group_begin3 = Some(begin3);
            begin3 = tmp;
        }

        for _ in 1..=length(cinfo) {
            if begin3 > end {
                break;
            }
            if Some(begin3) == group_begin3 {
                continue;
            }
            self.add_unknown(found, cinfo, begin, begin2, begin3);
            let (info, width) = self.chars.char_info(text, begin3, end);
            if !is_kind_of(cinfo, info) {
                break;
            }
            begin3 += width;
        }

        if found.is_empty() {
            self.add_unknown(found, cinfo, begin, begin2, begin3);
        }
    }

    fn add_unknown(
        &self,
        found: &mut Vec<Found>,
        cinfo: u32,
        begin: usize,
        begin2: usize,
        begin3: usize,
    ) {
        let category = default_type(cinfo);
        let tokens = self.unknown_tokens.get(category).cloned().unwrap_or(0..0);
        for index in tokens {
            let token = self.unknown.token(index);
            found.push(Found {
                position: begin2,
                length: (begin3 - begin2) & 0xffff,
                rlength: (begin3 - begin) & 0xffff,
                kind: UNKNOWN,
                char_type: category as u8,
                feature: if self.unknown_feature.is_some() {
                    FeatureSource::Replaced
                } else {
                    FeatureSource::Unknown(token.feature)
                },
            });
        }
    }
}
