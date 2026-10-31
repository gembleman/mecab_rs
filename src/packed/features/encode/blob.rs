use super::{
    Class, Error, HashMap, MAX_BLOB_FIELDS, MAX_ID_BYTES, SmallKeyMap, Stored, chars, split_fields,
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

pub(super) fn blob_for_number<'a>(
    entries: &[&'a [u8]],
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
    let texts: Vec<usize> = (0..classes.len())
        .filter(|&field| classes[field] == Class::Text && field != number_field)
        .collect();
    let mut maps: Vec<Option<SmallKeyMap<u64, &[u8]>>> =
        texts.iter().map(|_| Some(SmallKeyMap::default())).collect();
    let mut fields = Vec::new();
    for (entry, text) in entries.iter().enumerate() {
        if usize::from(field_counts[entry]) <= number_field {
            continue;
        }
        split_fields(text, &mut fields);
        for (slot, &field) in texts.iter().enumerate() {
            let (Some(map), Some(&(begin, end))) = (maps[slot].as_mut(), fields.get(field)) else {
                continue;
            };
            let value = &text[begin..end];
            if *map.entry(ids[entry]).or_insert(value) != value {
                maps[slot] = None;
            }
        }
    }
    let chosen: Vec<usize> = texts
        .iter()
        .zip(&maps)
        .filter(|(_, map)| map.as_ref().is_some_and(|map| !map.is_empty()))
        .map(|(&field, _)| field)
        .take(MAX_BLOB_FIELDS)
        .collect();
    drop(maps);
    let first = *chosen.first()?;
    if first >= number_field {
        return None;
    }
    let last = number_field.max(*chosen.last()?);
    let eligible: Vec<bool> = field_counts
        .iter()
        .map(|&count| usize::from(count) > last)
        .collect();
    let mut known: SmallKeyMap<u64, Vec<&[u8]>> = SmallKeyMap::default();
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
        split_fields(text, &mut fields);
        known.insert(
            id,
            chosen
                .iter()
                .map(|&field| &text[fields[field].0..fields[field].1])
                .collect(),
        );
    }
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
        let found = blob_for_number(entries, field_counts, classes, stored, &ids, number);
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
    let mut counts: HashMap<char, u64> = HashMap::new();
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
    rank_of: &HashMap<char, usize>,
    out: &mut Vec<u8>,
) -> Result<(), Error> {
    for ch in text_of(text)?.chars() {
        chars::push_code(rank_of[&ch], out);
    }
    Ok(())
}

pub(super) fn recode(
    area: &[u8],
    references: &[u32],
    inline: &[&[u8]],
    blob: Option<&Blob>,
) -> Result<Recoded, Error> {
    let ranked = char_ranks(area, inline, blob)?;
    let rank_of: HashMap<char, usize> = ranked
        .iter()
        .enumerate()
        .map(|(rank, &ch)| (ch, rank))
        .collect();
    let mut coded_area: Vec<u8> = Vec::new();
    let mut old_starts: Vec<u32> = Vec::new();
    let mut new_starts: Vec<u32> = Vec::new();
    let mut at = 0usize;
    while at < area.len() {
        let length = area[at..]
            .iter()
            .position(|&byte| byte == 0)
            .expect("every string of the area ends with NUL");
        old_starts.push(at as u32);
        new_starts.push(coded_area.len() as u32);
        code_text(&area[at..at + length], &rank_of, &mut coded_area)?;
        coded_area.push(0);
        at += length + 1;
    }
    let references = references
        .iter()
        .map(|offset| {
            let number = old_starts
                .binary_search(offset)
                .expect("a reference names the start of a string");
            new_starts[number]
        })
        .collect();
    let mut coded_inline = Vec::with_capacity(inline.len());
    for text in inline {
        let mut codes = Vec::new();
        code_text(text, &rank_of, &mut codes)?;
        coded_inline.push(codes);
    }
    let mut coded_blob: Vec<u8> = Vec::new();
    let mut blob_starts: Vec<u32> = Vec::new();
    if let Some(blob) = blob {
        for (id, texts) in &blob.entries {
            blob_starts.push(
                u32::try_from(coded_blob.len())
                    .map_err(|_| Error::Unsupported("feature strings of more than 4 GiB"))?,
            );
            coded_blob.extend_from_slice(&id.to_le_bytes()[..blob.id_bytes]);
            for text in texts {
                code_text(text, &rank_of, &mut coded_blob)?;
                coded_blob.push(0);
            }
        }
    }
    if coded_blob.len() > u32::MAX as usize {
        return Err(Error::Unsupported("feature strings of more than 4 GiB"));
    }
    Ok(Recoded {
        table: ranked.into_iter().map(chars::entry_of).collect(),
        area: coded_area,
        references,
        inline: coded_inline,
        blob: coded_blob,
        blob_starts,
    })
}
