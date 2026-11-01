use std::collections::BTreeMap;

use super::{
    Class, Error, FastMap, MAX_BLOB_FIELDS, MAX_ID_BYTES, Spans, Stored, chars, for_each_lane,
};

pub(super) struct Blob<'a> {
    pub(super) number: usize,
    pub(super) fields: Vec<usize>,
    pub(super) id_bytes: usize,
    pub(super) ids: Vec<u64>,
    pub(super) texts: Vec<&'a [u8]>,
    pub(super) eligible: Vec<bool>,
}

impl<'a> Blob<'a> {
    pub(super) fn slot(&self, entry: usize, field: usize) -> Option<usize> {
        if self.eligible[entry] {
            self.fields.iter().position(|&candidate| candidate == field)
        } else {
            None
        }
    }

    pub(super) fn entry_of(&self, id: u64) -> usize {
        self.ids
            .binary_search(&id)
            .expect("every id is in the blob")
    }

    pub(super) fn release_texts(&mut self) {
        self.texts = Vec::new();
        self.eligible = Vec::new();
    }

    pub(super) fn texts_of(&self, index: usize) -> &[&'a [u8]] {
        let width = self.fields.len();
        &self.texts[index * width..(index + 1) * width]
    }
}

#[derive(Clone, Copy)]
pub(super) struct IdSource<'v> {
    values: &'v [u64],
    shift: u32,
}

impl IdSource<'_> {
    fn id(&self, entry: usize) -> u64 {
        self.values[entry] >> self.shift
    }
}

fn field_follows_ids(
    entries: &[&[u8]],
    spans: &Spans,
    field_counts: &[u16],
    ids: IdSource,
    number_field: usize,
    field: usize,
) -> bool {
    let mut map: FastMap<u64, &[u8]> = FastMap::default();
    for (entry, text) in entries.iter().enumerate() {
        let count = usize::from(field_counts[entry]);
        if count <= number_field || count <= field {
            continue;
        }
        let (begin, end) = spans.span(entry, field);
        let value = &text[begin..end];
        if *map.entry(ids.id(entry)).or_insert(value) != value {
            return false;
        }
    }
    !map.is_empty()
}

fn shared_text_fields(
    entries: &[&[u8]],
    spans: &Spans,
    field_counts: &[u16],
    classes: &[Class],
    ids: IdSource,
    number_field: usize,
) -> Vec<usize> {
    let mut verdicts: Vec<(usize, bool)> = (0..classes.len())
        .filter(|&field| classes[field] == Class::Text && field != number_field)
        .map(|field| (field, false))
        .collect();
    for_each_lane(&mut verdicts, entries.len(), |_, verdict| {
        verdict.1 = field_follows_ids(entries, spans, field_counts, ids, number_field, verdict.0);
    });
    verdicts
        .into_iter()
        .filter(|&(_, follows)| follows)
        .map(|(field, _)| field)
        .take(MAX_BLOB_FIELDS)
        .collect()
}

fn first_entries(eligible: &[bool], ids: IdSource) -> (Vec<(u64, u32)>, usize) {
    let mut first: FastMap<u64, u32> = FastMap::default();
    let mut eligible_count = 0usize;
    for (entry, &is_eligible) in eligible.iter().enumerate() {
        if is_eligible {
            eligible_count += 1;
            first.entry(ids.id(entry)).or_insert(entry as u32);
        }
    }
    let mut sorted: Vec<(u64, u32)> = first.into_iter().collect();
    sorted.sort_unstable();
    (sorted, eligible_count)
}

fn gather_texts<'a>(
    entries: &[&'a [u8]],
    spans: &Spans,
    sorted: &[(u64, u32)],
    chosen: &[usize],
) -> Vec<&'a [u8]> {
    let mut texts = Vec::with_capacity(sorted.len() * chosen.len());
    let mut fields = Vec::new();
    for &(_, entry) in sorted {
        let text: &'a [u8] = entries[entry as usize];
        spans.fill(entry as usize, &mut fields);
        texts.extend(
            chosen
                .iter()
                .map(|&field| &text[fields[field].0..fields[field].1]),
        );
    }
    texts
}

pub(super) fn blob_for_number<'a>(
    entries: &[&'a [u8]],
    spans: &Spans,
    field_counts: &[u16],
    classes: &[Class],
    stored: &[Stored],
    ids: IdSource,
    number: usize,
) -> Option<Blob<'a>> {
    let number_field = stored[number].field;
    let spec = stored[number].spec;
    let shift_ok = spec.shift > 0 && (1..=8 * MAX_ID_BYTES as u32).contains(&(spec.hi_bits));
    if !shift_ok {
        return None;
    }
    let chosen = shared_text_fields(entries, spans, field_counts, classes, ids, number_field);
    let first = *chosen.first()?;
    if first >= number_field {
        return None;
    }
    let last = number_field.max(*chosen.last()?);
    let eligible: Vec<bool> = field_counts
        .iter()
        .map(|&count| usize::from(count) > last)
        .collect();
    let (sorted, eligible_count) = first_entries(&eligible, ids);
    if sorted.len() * 2 > eligible_count {
        return None;
    }
    let texts = gather_texts(entries, spans, &sorted, &chosen);
    Some(Blob {
        number,
        fields: chosen,
        id_bytes: spec.hi_bits.div_ceil(8) as usize,
        ids: sorted.into_iter().map(|(id, _)| id).collect(),
        texts,
        eligible,
    })
}

pub(super) fn plan_blob<'a>(
    entries: &[&'a [u8]],
    spans: &Spans,
    field_counts: &[u16],
    classes: &[Class],
    stored: &[Stored],
    stored_values: &[usize],
    values: &[Vec<u64>],
) -> Option<Blob<'a>> {
    let mut best: Option<Blob<'a>> = None;
    for number in 0..stored.len() {
        let shift = stored[number].spec.shift;
        let ids = IdSource {
            values: &values[stored_values[number]],
            shift,
        };
        let found = blob_for_number(entries, spans, field_counts, classes, stored, ids, number);
        if let Some(found) = found
            && best
                .as_ref()
                .is_none_or(|best| found.fields.len() > best.fields.len())
        {
            best = Some(found);
        }
    }
    best
}

pub(super) struct Recoded {
    pub(super) table: Vec<u64>,
    pub(super) area: Vec<u8>,
    pub(super) references: Vec<u32>,
    pub(super) inline: Vec<u8>,
    pub(super) inline_ends: Vec<u32>,
    pub(super) blob: Vec<u8>,
    pub(super) blob_starts: Vec<u32>,
}

impl Recoded {
    pub(super) fn inline_text(&self, index: usize) -> &[u8] {
        let begin = if index == 0 {
            0
        } else {
            self.inline_ends[index - 1] as usize
        };
        &self.inline[begin..self.inline_ends[index] as usize]
    }
}

pub(super) fn text_of(bytes: &[u8]) -> Result<&str, Error> {
    std::str::from_utf8(bytes).map_err(|_| Error::Unsupported("feature text that is not UTF-8"))
}

pub(super) fn char_ranks(
    area: &[u8],
    inline: &[&[u8]],
    blob: Option<&Blob>,
) -> Result<Vec<char>, Error> {
    let mut counts = CharValues::new();
    let mut seen: Vec<char> = Vec::new();
    let blob_texts = blob.into_iter().flat_map(|blob| blob.texts.iter().copied());
    for bytes in area
        .split(|&byte| byte == 0)
        .chain(inline.iter().copied())
        .chain(blob_texts)
    {
        for ch in text_of(bytes)?.chars() {
            let count = counts.slot(ch);
            if *count == 0 {
                seen.push(ch);
            }
            *count += 1;
        }
    }
    let mut ranked: Vec<(char, u32)> = seen.into_iter().map(|ch| (ch, counts.get(ch))).collect();
    ranked.sort_by_key(|&(ch, count)| (std::cmp::Reverse(count), ch));
    if ranked.len() > chars::MAX_CHARS {
        return Err(Error::Unsupported(
            "more distinct feature characters than codes",
        ));
    }
    Ok(ranked.into_iter().map(|(ch, _)| ch).collect())
}

const PLANE_CHARS: usize = 0x10000;

struct CharValues {
    plane: Vec<u32>,
    astral: BTreeMap<char, u32>,
}

impl CharValues {
    fn new() -> Self {
        CharValues {
            plane: vec![0; PLANE_CHARS],
            astral: BTreeMap::new(),
        }
    }

    fn slot(&mut self, ch: char) -> &mut u32 {
        match self.plane.get_mut(ch as usize) {
            Some(value) => value,
            None => self.astral.entry(ch).or_insert(0),
        }
    }

    fn get(&self, ch: char) -> u32 {
        match self.plane.get(ch as usize) {
            Some(&value) => value,
            None => self.astral[&ch],
        }
    }
}

struct RankTable(CharValues);

impl RankTable {
    fn new(ranked: &[char]) -> Self {
        let mut values = CharValues::new();
        for (rank, &ch) in ranked.iter().enumerate() {
            *values.slot(ch) = rank as u32;
        }
        RankTable(values)
    }

    fn rank(&self, ch: char) -> usize {
        self.0.get(ch) as usize
    }
}

fn code_text(text: &[u8], rank_of: &RankTable, out: &mut Vec<u8>) -> Result<(), Error> {
    for ch in text_of(text)?.chars() {
        chars::push_code(rank_of.rank(ch), out);
    }
    Ok(())
}

struct CodedArea {
    bytes: Vec<u8>,
    new_starts: Vec<u32>,
}

#[allow(
    clippy::naive_bytecount,
    reason = "a plain count of NUL terminators; no bytecount dependency"
)]
fn code_area(area: &[u8], rank_of: &RankTable) -> Result<CodedArea, Error> {
    let strings = area.iter().filter(|&&byte| byte == 0).count();
    let mut coded = CodedArea {
        bytes: Vec::with_capacity(area.len()),
        new_starts: Vec::with_capacity(strings),
    };
    let mut at = 0usize;
    while at < area.len() {
        let length = area[at..]
            .iter()
            .position(|&byte| byte == 0)
            .expect("every string of the area ends with NUL");
        coded.new_starts.push(coded.bytes.len() as u32);
        code_text(&area[at..at + length], rank_of, &mut coded.bytes)?;
        coded.bytes.push(0);
        at += length + 1;
    }
    coded.bytes.shrink_to_fit();
    Ok(coded)
}

fn blob_bytes_bound(blob: &Blob) -> usize {
    let texts: usize = blob.texts.iter().map(|text| text.len() + 1).sum();
    texts + blob.ids.len() * blob.id_bytes
}

fn code_blob(blob: Option<&Blob>, rank_of: &RankTable) -> Result<(Vec<u8>, Vec<u32>), Error> {
    let too_large = || Error::Unsupported("feature strings of more than 4 GiB");
    let mut coded_blob: Vec<u8> = Vec::with_capacity(blob.map_or(0, blob_bytes_bound));
    let mut blob_starts: Vec<u32> = Vec::with_capacity(blob.map_or(0, |blob| blob.ids.len()));
    if let Some(blob) = blob {
        for (index, id) in blob.ids.iter().enumerate() {
            blob_starts.push(u32::try_from(coded_blob.len()).map_err(|_| too_large())?);
            coded_blob.extend_from_slice(&id.to_le_bytes()[..blob.id_bytes]);
            for text in blob.texts_of(index) {
                code_text(text, rank_of, &mut coded_blob)?;
                coded_blob.push(0);
            }
        }
    }
    if coded_blob.len() > u32::MAX as usize {
        return Err(too_large());
    }
    coded_blob.shrink_to_fit();
    Ok((coded_blob, blob_starts))
}

fn code_inline(inline: &[&[u8]], rank_of: &RankTable) -> Result<(Vec<u8>, Vec<u32>), Error> {
    let bound: usize = inline.iter().map(|text| text.len()).sum();
    let mut codes: Vec<u8> = Vec::with_capacity(bound);
    let mut ends: Vec<u32> = Vec::with_capacity(inline.len());
    for text in inline {
        code_text(text, rank_of, &mut codes)?;
        ends.push(
            u32::try_from(codes.len())
                .map_err(|_| Error::Unsupported("feature strings of more than 4 GiB"))?,
        );
    }
    codes.shrink_to_fit();
    Ok((codes, ends))
}

pub(super) fn recode(
    area: &[u8],
    references: &[u32],
    inline: &[&[u8]],
    blob: Option<&Blob>,
) -> Result<Recoded, Error> {
    let ranked = char_ranks(area, inline, blob)?;
    let rank_of = RankTable::new(&ranked);
    let coded_area = code_area(area, &rank_of)?;
    let references = references
        .iter()
        .map(|&string| coded_area.new_starts[string as usize])
        .collect();
    let (coded_inline, inline_ends) = code_inline(inline, &rank_of)?;
    let (coded_blob, blob_starts) = code_blob(blob, &rank_of)?;
    Ok(Recoded {
        table: ranked.into_iter().map(chars::entry_of).collect(),
        area: coded_area.bytes,
        references,
        inline: coded_inline,
        inline_ends,
        blob: coded_blob,
        blob_starts,
    })
}
