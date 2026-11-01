use super::{
    Class, Error, MAX_BLOB_FIELDS, MAX_ID_BYTES, SmallKeyMap, Spans, Stored, chars, for_each_lane,
};

pub(super) struct Blob<'a> {
    pub(super) number: usize,
    pub(super) fields: Vec<usize>,
    pub(super) id_bytes: usize,
    pub(super) entries: Vec<(u64, Vec<&'a [u8]>)>,
    pub(super) eligible: Vec<bool>,
}

impl Blob<'_> {
    pub(super) fn slot(&self, entry: usize, field: usize) -> Option<usize> {
        if self.eligible[entry] {
            self.fields.iter().position(|&candidate| candidate == field)
        } else {
            None
        }
    }

    pub(super) fn entry_of(&self, id: u64) -> usize {
        self.entries
            .binary_search_by_key(&id, |&(entry_id, _)| entry_id)
            .expect("every id is in the blob")
    }
}

fn field_follows_ids(
    entries: &[&[u8]],
    spans: &Spans,
    field_counts: &[u16],
    ids: &[u64],
    number_field: usize,
    field: usize,
) -> bool {
    let mut map: SmallKeyMap<u64, &[u8]> = SmallKeyMap::default();
    for (entry, text) in entries.iter().enumerate() {
        let count = usize::from(field_counts[entry]);
        if count <= number_field || count <= field {
            continue;
        }
        let (begin, end) = spans.span(entry, field);
        let value = &text[begin..end];
        if *map.entry(ids[entry]).or_insert(value) != value {
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
    ids: &[u64],
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

type KnownTexts<'a> = SmallKeyMap<u64, Vec<&'a [u8]>>;

fn known_texts<'a>(
    entries: &[&'a [u8]],
    spans: &Spans,
    eligible: &[bool],
    ids: &[u64],
    chosen: &[usize],
) -> (KnownTexts<'a>, usize) {
    let mut fields = Vec::new();
    let mut known: KnownTexts = SmallKeyMap::default();
    let mut eligible_count = 0usize;
    for (entry, text) in entries.iter().enumerate() {
        if !eligible[entry] {
            continue;
        }
        eligible_count += 1;
        let id = ids[entry];
        if known.contains_key(&id) {
            continue;
        }
        spans.fill(entry, &mut fields);
        known.insert(
            id,
            chosen
                .iter()
                .map(|&field| &text[fields[field].0..fields[field].1])
                .collect(),
        );
    }
    (known, eligible_count)
}

pub(super) fn blob_for_number<'a>(
    entries: &[&'a [u8]],
    spans: &Spans,
    field_counts: &[u16],
    classes: &[Class],
    stored: &[Stored],
    ids: &[u64],
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
    let (known, eligible_count) = known_texts(entries, spans, &eligible, ids, &chosen);
    if known.len() * 2 > eligible_count {
        return None;
    }
    let mut blob_entries: Vec<(u64, Vec<&[u8]>)> = known.into_iter().collect();
    blob_entries.sort_unstable_by_key(|&(id, _)| id);
    Some(Blob {
        number,
        fields: chosen,
        id_bytes: spec.hi_bits.div_ceil(8) as usize,
        entries: blob_entries,
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
        let ids: Vec<u64> = values[stored_values[number]]
            .iter()
            .map(|&value| value >> shift)
            .collect();
        let found = blob_for_number(entries, spans, field_counts, classes, stored, &ids, number);
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
    pub(super) inline: Vec<Vec<u8>>,
    pub(super) blob: Vec<u8>,
    pub(super) blob_starts: Vec<u32>,
}

pub(super) fn text_of(bytes: &[u8]) -> Result<&str, Error> {
    std::str::from_utf8(bytes).map_err(|_| Error::Unsupported("feature text that is not UTF-8"))
}

pub(super) fn char_ranks(
    area: &[u8],
    inline: &[&[u8]],
    blob: Option<&Blob>,
) -> Result<Vec<char>, Error> {
    let mut counts: SmallKeyMap<char, u64> = SmallKeyMap::default();
    let blob_texts = blob.into_iter().flat_map(|blob| {
        blob.entries
            .iter()
            .flat_map(|(_, texts)| texts.iter().copied())
    });
    for bytes in area
        .split(|&byte| byte == 0)
        .chain(inline.iter().copied())
        .chain(blob_texts)
    {
        for ch in text_of(bytes)?.chars() {
            *counts.entry(ch).or_insert(0) += 1;
        }
    }
    let mut ranked: Vec<(char, u64)> = counts.into_iter().collect();
    ranked.sort_by_key(|&(ch, count)| (std::cmp::Reverse(count), ch));
    if ranked.len() > chars::MAX_CHARS {
        return Err(Error::Unsupported(
            "more distinct feature characters than codes",
        ));
    }
    Ok(ranked.into_iter().map(|(ch, _)| ch).collect())
}

pub(super) fn code_text(
    text: &[u8],
    rank_of: &SmallKeyMap<char, usize>,
    out: &mut Vec<u8>,
) -> Result<(), Error> {
    for ch in text_of(text)?.chars() {
        chars::push_code(rank_of[&ch], out);
    }
    Ok(())
}

struct CodedArea {
    bytes: Vec<u8>,
    old_starts: Vec<u32>,
    new_starts: Vec<u32>,
}

fn code_area(area: &[u8], rank_of: &SmallKeyMap<char, usize>) -> Result<CodedArea, Error> {
    let mut coded = CodedArea {
        bytes: Vec::new(),
        old_starts: Vec::new(),
        new_starts: Vec::new(),
    };
    let mut at = 0usize;
    while at < area.len() {
        let length = area[at..]
            .iter()
            .position(|&byte| byte == 0)
            .expect("every string of the area ends with NUL");
        coded.old_starts.push(at as u32);
        coded.new_starts.push(coded.bytes.len() as u32);
        code_text(&area[at..at + length], rank_of, &mut coded.bytes)?;
        coded.bytes.push(0);
        at += length + 1;
    }
    Ok(coded)
}

fn code_blob(
    blob: Option<&Blob>,
    rank_of: &SmallKeyMap<char, usize>,
) -> Result<(Vec<u8>, Vec<u32>), Error> {
    let too_large = || Error::Unsupported("feature strings of more than 4 GiB");
    let mut coded_blob: Vec<u8> = Vec::new();
    let mut blob_starts: Vec<u32> = Vec::new();
    if let Some(blob) = blob {
        for (id, texts) in &blob.entries {
            blob_starts.push(u32::try_from(coded_blob.len()).map_err(|_| too_large())?);
            coded_blob.extend_from_slice(&id.to_le_bytes()[..blob.id_bytes]);
            for text in texts {
                code_text(text, rank_of, &mut coded_blob)?;
                coded_blob.push(0);
            }
        }
    }
    if coded_blob.len() > u32::MAX as usize {
        return Err(too_large());
    }
    Ok((coded_blob, blob_starts))
}

pub(super) fn recode(
    area: &[u8],
    references: &[u32],
    inline: &[&[u8]],
    blob: Option<&Blob>,
) -> Result<Recoded, Error> {
    let ranked = char_ranks(area, inline, blob)?;
    let rank_of: SmallKeyMap<char, usize> = ranked
        .iter()
        .enumerate()
        .map(|(rank, &ch)| (ch, rank))
        .collect();
    let coded_area = code_area(area, &rank_of)?;
    let references = references
        .iter()
        .map(|offset| {
            let number = coded_area
                .old_starts
                .binary_search(offset)
                .expect("a reference names the start of a string");
            coded_area.new_starts[number]
        })
        .collect();
    let mut coded_inline = Vec::with_capacity(inline.len());
    for text in inline {
        let mut codes = Vec::new();
        code_text(text, &rank_of, &mut codes)?;
        coded_inline.push(codes);
    }
    let (coded_blob, blob_starts) = code_blob(blob, &rank_of)?;
    Ok(Recoded {
        table: ranked.into_iter().map(chars::entry_of).collect(),
        area: coded_area.bytes,
        references,
        inline: coded_inline,
        blob: coded_blob,
        blob_starts,
    })
}
