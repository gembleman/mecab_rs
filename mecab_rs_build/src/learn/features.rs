use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use super::{LineEnding, LineEnds};
use crate::build::Charset;
use crate::build::model::{Templates, bigram_keys, unigram_keys};
use crate::build::rewrite::{Rewriter, Rewritten};
use crate::build::text::{LINE_BUFFER_BYTES, Lines, atof, cstr, tokenize2};
use crate::tool_error::{Position, caused_by, learn_error};
use mecab_rs::__private::files::{FEATURE_DEF_FILE, REWRITE_DEF_FILE, write_file};
use mecab_rs::__private::hash::{FNV_OFFSET, FNV_PRIME};
use mecab_rs::Error;

pub(super) struct Fnv(u64);

impl Default for Fnv {
    fn default() -> Self {
        Self(FNV_OFFSET)
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

pub(super) const fn widen(value: u32) -> usize {
    value as usize
}

pub(super) fn weight_index(id: i32) -> usize {
    debug_assert!(id >= 0, "the id list end is never a weight index");
    usize::try_from(id).expect("feature ids are not negative")
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct FeatureId(u32);

impl FeatureId {
    fn next_after(count: usize) -> Option<Self> {
        u32::try_from(count).ok().map(Self)
    }

    fn index(self) -> usize {
        widen(self.0)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct PoolOffset(u32);

impl PoolOffset {
    pub(super) const NONE: Self = Self(u32::MAX);

    fn at_end_of(pool: &[i32]) -> Option<Self> {
        u32::try_from(pool.len()).ok().map(Self)
    }

    fn index(self) -> usize {
        widen(self.0)
    }
}

pub(super) fn ids_from(pool: &[i32], start: PoolOffset) -> impl Iterator<Item = usize> + '_ {
    pool[start.index()..]
        .iter()
        .take_while(|&&id| id != ID_LIST_END)
        .map(|&id| weight_index(id))
}

const TOO_MANY_FEATURES: &str = "there are too many distinct features";
const TOO_MANY_IDS: &str = "there are too many feature ids";
const REWRITTEN_BEFORE_USE: &str = "build_feature rewrites a feature before reading it";
const FEATURE_COUNT_FITS_I32: &str = "the feature count fits an i32";

pub(super) type Settings = Vec<(String, String)>;

pub(super) type Map<K, V> = HashMap<K, V, BuildHasherDefault<Fnv>>;

pub(super) struct Features {
    ids: Map<Vec<u8>, FeatureId>,
    list: Vec<Vec<u8>>,
    rewritten: Vec<Option<Rewritten>>,
    rewriter: Rewriter,
    rewrite_def: PathBuf,
}

impl Features {
    pub(super) fn open(dic_dir: &Path) -> Result<Self, Error> {
        let rewrite_def = dic_dir.join(REWRITE_DEF_FILE);
        Ok(Self {
            ids: Map::default(),
            list: Vec::new(),
            rewritten: Vec::new(),
            rewriter: Rewriter::open(&rewrite_def)?,
            rewrite_def,
        })
    }

    pub(super) fn intern(&mut self, feature: &[u8]) -> Result<FeatureId, Error> {
        if let Some(&id) = self.ids.get(feature) {
            return Ok(id);
        }
        let id = FeatureId::next_after(self.list.len())
            .ok_or_else(|| learn_error(&self.rewrite_def, TOO_MANY_FEATURES))?;
        self.ids.insert(feature.to_vec(), id);
        self.list.push(feature.to_vec());
        self.rewritten.push(None);
        Ok(id)
    }

    pub(super) fn get(&self, id: FeatureId) -> &[u8] {
        &self.list[id.index()]
    }

    pub(super) fn rewrite(&mut self, id: FeatureId) -> Result<(), Error> {
        let slot = &mut self.rewritten[id.index()];
        if slot.is_none() {
            let feature = &self.list[id.index()];
            let (matched, rewritten) = self.rewriter.rewrite(feature, &self.rewrite_def)?;
            if !matched {
                return Err(learn_error(
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

    pub(super) fn rewritten(&self, id: FeatureId) -> &Rewritten {
        self.rewritten[id.index()]
            .as_ref()
            .expect(REWRITTEN_BEFORE_USE)
    }

    pub(super) fn clear_rewrites(&mut self) {
        self.rewritten.fill(None);
    }
}

pub(super) const ID_LIST_END: i32 = -1;
const DROPPED: i32 = -1;

struct Cached {
    start: PoolOffset,
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

fn id_of(
    dic: &mut Map<Vec<u8>, i32>,
    maxid: &mut usize,
    key: &[u8],
    feature_def: &Path,
) -> Result<i32, Error> {
    if let Some(&id) = dic.get(key) {
        return Ok(id);
    }
    let id = i32::try_from(*maxid).map_err(|_| learn_error(feature_def, TOO_MANY_IDS))?;
    *maxid += 1;
    dic.insert(key.to_vec(), id);
    Ok(id)
}

fn cut_at_nul(key: &mut Vec<u8>) {
    if let Some(at) = key.iter().position(|&byte| byte == 0) {
        key.truncate(at);
    }
}

fn identity_renumbering(size: usize) -> Vec<i32> {
    (0..size)
        .map(|id| i32::try_from(id).expect(FEATURE_COUNT_FITS_I32))
        .collect()
}

fn renumbering(counts: &[usize], freq: usize) -> Vec<i32> {
    let mut next = 0i32;
    counts
        .iter()
        .map(|&count| {
            if count >= freq {
                next += 1;
                next - 1
            } else {
                DROPPED
            }
        })
        .collect()
}

pub(super) fn remap_weights(old: &[f64], renumbered: &[i32], size: usize) -> Vec<f64> {
    let mut remapped = vec![0.0; size];
    for (&weight, &new) in old.iter().zip(renumbered) {
        if new >= 0 {
            remapped[weight_index(new)] = weight;
        }
    }
    remapped
}

fn read_model_header(lines: &mut Lines, path: &Path) -> Result<(String, Settings), Error> {
    let mut model_charset = String::new();
    let mut settings = Vec::new();
    while let Some((number, line)) = lines.next_line()? {
        if line[0] == 0 {
            break;
        }
        let shown = String::from_utf8_lossy(cstr(line, 0)).into_owned();
        let found = tokenize2(line, b":", 2);
        if found.len() != 2 {
            return Err(Position::new(path, number).learn(format!("format error: {shown}")));
        }
        let key = String::from_utf8_lossy(cstr(line, found[0])).into_owned();
        let value =
            String::from_utf8_lossy(cstr(line, found[1]).get(1..).unwrap_or_default()).into_owned();
        if key == "charset" {
            model_charset = value;
        } else {
            settings.push((key, value));
        }
    }
    if model_charset.is_empty() {
        return Err(learn_error(path, "charset is empty"));
    }
    Ok((model_charset, settings))
}

fn require_same_charset(
    path: &Path,
    model_charset: &str,
    dictionary_charset: &str,
) -> Result<(), Error> {
    let old_charset = Charset::parse(model_charset).map_err(|reason| learn_error(path, reason))?;
    let new_charset =
        Charset::parse(dictionary_charset).map_err(|reason| learn_error(path, reason))?;
    if old_charset != new_charset {
        return Err(learn_error(
            path,
            format!("cannot create model from={model_charset} to={dictionary_charset}"),
        ));
    }
    Ok(())
}

impl FeatureIndex {
    pub(super) fn open(dic_dir: &Path) -> Result<Self, Error> {
        let feature_def = dic_dir.join(FEATURE_DEF_FILE);
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
    ) -> Result<PoolOffset, Error> {
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
    ) -> Result<PoolOffset, Error> {
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
        let mut overflow = None;
        build(
            templates,
            feature_def,
            &mut |key| match id_of(dic, maxid, key, feature_def) {
                Ok(id) => scratch.push(id),
                Err(error) => {
                    overflow.get_or_insert(error);
                }
            },
        )?;
        if let Some(error) = overflow {
            return Err(error);
        }
        let start =
            PoolOffset::at_end_of(pool).ok_or_else(|| learn_error(feature_def, TOO_MANY_IDS))?;
        pool.extend_from_slice(scratch);
        pool.push(ID_LIST_END);
        cache.insert(key, Cached { start, times: 1 });
        Ok(start)
    }

    pub(super) fn bigram(&mut self, rfeature: &[u8], lfeature: &[u8]) -> Result<PoolOffset, Error> {
        let mut key = Vec::with_capacity(rfeature.len() + lfeature.len() + 1);
        key.extend_from_slice(rfeature);
        key.push(b' ');
        key.extend_from_slice(lfeature);
        cut_at_nul(&mut key);
        self.vector(key, |templates, feature_def, add| {
            bigram_keys(&templates.bigram, rfeature, lfeature, feature_def, add)
        })
    }

    pub(super) fn shrink(&mut self, freq: usize, observed: &mut Vec<f64>) -> Vec<i32> {
        let mut counts = vec![0usize; self.maxid];
        for cached in self.cache.values() {
            for id in ids_from(&self.pool, cached.start) {
                counts[id] += cached.times;
            }
        }
        if freq <= 1 {
            return identity_renumbering(counts.len());
        }

        let renumbered = renumbering(&counts, freq);
        self.maxid = renumbered.iter().filter(|&&new| new != DROPPED).count();

        self.dic.retain(|_, id| {
            let new = renumbered[weight_index(*id)];
            *id = new;
            new >= 0
        });

        for cached in self.cache.values() {
            let mut to = cached.start.index();
            let mut at = cached.start.index();
            while self.pool[at] != ID_LIST_END {
                let new = renumbered[weight_index(self.pool[at])];
                if new >= 0 {
                    self.pool[to] = new;
                    to += 1;
                }
                at += 1;
            }
            self.pool[to] = ID_LIST_END;
        }

        *observed = remap_weights(observed, &renumbered, self.maxid);
        renumbered
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
        let mut lines =
            Lines::open(path, Some(LINE_BUFFER_BYTES)).map_err(|error| match error {
                Error::Io { source, .. } => caused_by(
                    learn_error(
                        path,
                        format!("cannot open the old model {}", path.display()),
                    ),
                    source,
                ),
                other => other,
            })?;
        let (model_charset, settings) = read_model_header(&mut lines, path)?;
        require_same_charset(path, &model_charset, dictionary_charset)?;
        let alpha = self.read_old_weights(&mut lines, path)?;
        Ok((alpha, settings))
    }

    fn read_old_weights(&mut self, lines: &mut Lines, path: &Path) -> Result<Vec<f64>, Error> {
        let mut alpha = Vec::new();
        while let Some((number, line)) = lines.next_line()? {
            let shown = String::from_utf8_lossy(cstr(line, 0)).into_owned();
            let found = tokenize2(line, b"\t", 2);
            if found.len() != 2 {
                return Err(Position::new(path, number).learn(format!("format error: {shown}")));
            }
            let id = i32::try_from(self.maxid).map_err(|_| learn_error(path, TOO_MANY_IDS))?;
            self.dic.entry(cstr(line, found[1]).to_vec()).or_insert(id);
            self.maxid += 1;
            alpha.push(atof(cstr(line, found[0])));
        }
        Ok(alpha)
    }

    pub(super) fn save(
        &self,
        path: &Path,
        header: &str,
        alpha: &[f64],
        ending: LineEnding,
    ) -> Result<(), Error> {
        let mut entries: Vec<(&Vec<u8>, i32)> =
            self.dic.iter().map(|(key, &id)| (key, id)).collect();
        entries.sort_unstable();
        write_file(path, |file| {
            let mut file = LineEnds::new(file, ending);
            file.write_all(header.as_bytes())?;
            file.write_all(b"\n")?;
            let mut line = Vec::new();
            for (key, id) in entries {
                line.clear();
                write!(line, "{:.16}\t", alpha[weight_index(id)])?;
                line.extend_from_slice(key);
                line.push(b'\n');
                file.write_all(&line)?;
            }
            io::Result::Ok(())
        })
    }
}
