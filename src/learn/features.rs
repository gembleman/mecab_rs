use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use super::{LineEnds, failure};
use crate::build::model::{Templates, bigram_keys, unigram_keys};
use crate::build::rewrite::{Rewriter, Rewritten};
use crate::build::text::{BUF_SIZE, Lines, atof, cstr, tokenize2};
use crate::build::{Charset, write_file};
use crate::error::Error;

const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

pub(super) struct Fnv(u64);

impl Default for Fnv {
    fn default() -> Self {
        Self(FNV_OFFSET_BASIS)
    }
}

impl Hasher for Fnv {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.0 = (self.0 ^ u64::from(byte)).wrapping_mul(FNV_PRIME);
        }
    }
}

pub(super) type Settings = Vec<(String, String)>;

pub(super) type Map<K, V> = HashMap<K, V, BuildHasherDefault<Fnv>>;

pub(super) struct Features {
    ids: Map<Vec<u8>, u32>,
    list: Vec<Vec<u8>>,
    rewritten: Vec<Option<Rewritten>>,
    rewriter: Rewriter,
    rewrite_def: PathBuf,
}

impl Features {
    pub(super) fn open(dic_dir: &Path) -> Result<Self, Error> {
        let rewrite_def = dic_dir.join("rewrite.def");
        Ok(Self {
            ids: Map::default(),
            list: Vec::new(),
            rewritten: Vec::new(),
            rewriter: Rewriter::open(&rewrite_def)?,
            rewrite_def,
        })
    }

    pub(super) fn intern(&mut self, feature: &[u8]) -> u32 {
        if let Some(&id) = self.ids.get(feature) {
            return id;
        }
        let id = u32::try_from(self.list.len()).expect("fewer than 2^32 distinct features");
        self.ids.insert(feature.to_vec(), id);
        self.list.push(feature.to_vec());
        self.rewritten.push(None);
        id
    }

    pub(super) fn get(&self, id: u32) -> &[u8] {
        &self.list[id as usize]
    }

    pub(super) fn rewrite(&mut self, id: u32) -> Result<(), Error> {
        let slot = &mut self.rewritten[id as usize];
        if slot.is_none() {
            let feature = &self.list[id as usize];
            let (matched, rewritten) = self.rewriter.rewrite(feature, &self.rewrite_def)?;
            if !matched {
                return Err(failure(
                    &self.rewrite_def,
                    format!(
                        " cannot rewrite pattern: {}",
                        String::from_utf8_lossy(feature)
                    ),
                ));
            }
            *slot = Some(rewritten);
        }
        Ok(())
    }

    pub(super) fn rewritten(&self, id: u32) -> &Rewritten {
        self.rewritten[id as usize]
            .as_ref()
            .expect("the feature is rewritten")
    }

    pub(super) fn clear_rewrites(&mut self) {
        self.rewritten.fill(None);
    }
}

pub(super) const ID_LIST_END: i32 = -1;
const DROPPED: i32 = -1;

struct Cached {
    start: u32,
    times: usize,
}

pub(super) struct FeatureIndex {
    templates: Templates,
    feature_def: PathBuf,
    dic: Map<Vec<u8>, i32>,
    cache: Map<Vec<u8>, Cached>,
    pool: Vec<i32>,
    maxid: usize,
    scratch: Vec<i32>,
}

fn id_of(dic: &mut Map<Vec<u8>, i32>, maxid: &mut usize, key: &[u8]) -> i32 {
    if let Some(&id) = dic.get(key) {
        return id;
    }
    let id = i32::try_from(*maxid).expect("fewer than 2^31 features");
    *maxid += 1;
    dic.insert(key.to_vec(), id);
    id
}

fn cut_at_nul(key: &mut Vec<u8>) {
    if let Some(at) = key.iter().position(|&byte| byte == 0) {
        key.truncate(at);
    }
}

impl FeatureIndex {
    pub(super) fn open(dic_dir: &Path) -> Result<Self, Error> {
        let feature_def = dic_dir.join("feature.def");
        Ok(Self {
            templates: Templates::open(&feature_def)?,
            feature_def,
            dic: Map::default(),
            cache: Map::default(),
            pool: Vec::new(),
            maxid: 0,
            scratch: Vec::new(),
        })
    }

    pub(super) fn size(&self) -> usize {
        self.maxid
    }

    pub(super) fn pool(&self) -> &[i32] {
        &self.pool
    }

    pub(super) fn unigram(
        &mut self,
        ufeature: &[u8],
        char_type: u8,
        surface: &[u8],
    ) -> Result<u32, Error> {
        let mut key = Vec::with_capacity(ufeature.len() + 2);
        key.extend_from_slice(ufeature);
        key.push(b' ');
        key.push(char_type);
        cut_at_nul(&mut key);
        self.vector(key, |templates, feature_def, add| {
            unigram_keys(
                &templates.unigram,
                ufeature,
                char_type,
                surface,
                feature_def,
                add,
            )
        })
    }

    fn vector(
        &mut self,
        key: Vec<u8>,
        build: impl FnOnce(&Templates, &Path, &mut dyn FnMut(&[u8])) -> Result<(), Error>,
    ) -> Result<u32, Error> {
        if let Some(entry) = self.cache.get_mut(&key) {
            entry.times += 1;
            return Ok(entry.start);
        }
        let Self {
            templates,
            feature_def,
            dic,
            pool,
            maxid,
            scratch,
            cache,
        } = self;
        scratch.clear();
        build(templates, feature_def, &mut |key| {
            scratch.push(id_of(dic, maxid, key));
        })?;
        let start = u32::try_from(pool.len()).expect("fewer than 2^32 ids");
        pool.extend_from_slice(scratch);
        pool.push(ID_LIST_END);
        cache.insert(key, Cached { start, times: 1 });
        Ok(start)
    }

    pub(super) fn bigram(&mut self, rfeature: &[u8], lfeature: &[u8]) -> Result<u32, Error> {
        let mut key = Vec::with_capacity(rfeature.len() + lfeature.len() + 1);
        key.extend_from_slice(rfeature);
        key.push(b' ');
        key.extend_from_slice(lfeature);
        cut_at_nul(&mut key);
        self.vector(key, |templates, feature_def, add| {
            bigram_keys(&templates.bigram, rfeature, lfeature, feature_def, add)
        })
    }

    pub(super) fn shrink(&mut self, freq: usize, observed: &mut Vec<f64>) {
        let mut counts = vec![0usize; self.maxid];
        for cached in self.cache.values() {
            let mut at = cached.start as usize;
            while self.pool[at] != ID_LIST_END {
                counts[self.pool[at] as usize] += cached.times;
                at += 1;
            }
        }
        if freq <= 1 {
            return;
        }

        self.maxid = 0;
        let mut renumbered = vec![DROPPED; counts.len()];
        for (old, &count) in counts.iter().enumerate() {
            if count >= freq {
                renumbered[old] = i32::try_from(self.maxid).expect("fewer than 2^31 features");
                self.maxid += 1;
            }
        }

        self.dic.retain(|_, id| {
            let new = renumbered[*id as usize];
            *id = new;
            new >= 0
        });

        for cached in self.cache.values() {
            let mut to = cached.start as usize;
            let mut at = cached.start as usize;
            while self.pool[at] != ID_LIST_END {
                let new = renumbered[self.pool[at] as usize];
                if new >= 0 {
                    self.pool[to] = new;
                    to += 1;
                }
                at += 1;
            }
            self.pool[to] = ID_LIST_END;
        }

        let mut shrunk = vec![0.0; self.maxid];
        for (old, &value) in observed.iter().enumerate() {
            let new = renumbered[old];
            if new >= 0 {
                shrunk[new as usize] = value;
            }
        }
        *observed = shrunk;
    }

    pub(super) fn clear_cache(&mut self) {
        self.cache.clear();
        self.cache.shrink_to_fit();
    }

    pub(super) fn reopen(
        &mut self,
        path: &Path,
        dictionary_charset: &str,
    ) -> Result<(Vec<f64>, Settings), Error> {
        self.dic.clear();
        self.cache.clear();
        self.maxid = 0;
        let mut lines = Lines::open(path, Some(BUF_SIZE)).map_err(|_| {
            failure(
                path,
                format!("cannot open the old model {}", path.display()),
            )
        })?;
        let mut model_charset = String::new();
        let mut settings = Vec::new();
        while let Some(line) = lines.next_line()? {
            if line[0] == 0 {
                break;
            }
            let shown = String::from_utf8_lossy(cstr(line, 0)).into_owned();
            let found = tokenize2(line, b":", 2);
            if found.len() != 2 {
                return Err(failure(path, format!("format error: {shown}")));
            }
            let key = String::from_utf8_lossy(cstr(line, found[0])).into_owned();
            let value = String::from_utf8_lossy(cstr(line, found[1]).get(1..).unwrap_or_default())
                .into_owned();
            if key == "charset" {
                model_charset = value;
            } else {
                settings.push((key, value));
            }
        }
        if model_charset.is_empty() {
            return Err(failure(path, "charset is empty"));
        }
        if Charset::decode(&model_charset) != Charset::decode(dictionary_charset) {
            return Err(failure(
                path,
                format!("cannot create model from={model_charset} to={dictionary_charset}"),
            ));
        }
        let mut alpha = Vec::new();
        while let Some(line) = lines.next_line()? {
            let shown = String::from_utf8_lossy(cstr(line, 0)).into_owned();
            let found = tokenize2(line, b"\t", 2);
            if found.len() != 2 {
                return Err(failure(path, format!("format error: {shown}")));
            }
            let id = i32::try_from(self.maxid).expect("fewer than 2^31 features");
            self.dic.entry(cstr(line, found[1]).to_vec()).or_insert(id);
            self.maxid += 1;
            alpha.push(atof(cstr(line, found[0])));
        }
        Ok((alpha, settings))
    }

    pub(super) fn save(
        &self,
        path: &Path,
        header: &str,
        alpha: &[f64],
        crlf: bool,
    ) -> Result<(), Error> {
        let mut entries: Vec<(&Vec<u8>, i32)> =
            self.dic.iter().map(|(key, &id)| (key, id)).collect();
        entries.sort_unstable();
        write_file(path, |file| {
            let mut file = LineEnds::new(file, crlf);
            file.write_all(header.as_bytes())?;
            file.write_all(b"\n")?;
            let mut line = Vec::new();
            for (key, id) in entries {
                line.clear();
                write!(line, "{:.16}\t", alpha[id as usize])?;
                line.extend_from_slice(key);
                line.push(b'\n');
                file.write_all(&line)?;
            }
            io::Result::Ok(())
        })
    }
}
