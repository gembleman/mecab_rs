use super::*;

pub(super) fn is_key(value: &[u8], key: &[u8]) -> bool {
    !key.is_empty() && value == key && value != b"*"
}

#[derive(Clone, Copy)]
pub(super) enum TextOp {
    Key,
    Star,
    Empty,
    Copy(u8),
    Derive,
    New,
}

pub(super) fn text_op<'a>(
    value: &'a [u8],
    key: &[u8],
    derivable: bool,
    slots: &mut Vec<&'a [u8]>,
) -> TextOp {
    if is_key(value, key) {
        return TextOp::Key;
    }
    if value == b"*" {
        return TextOp::Star;
    }
    if value.is_empty() {
        return TextOp::Empty;
    }
    if let Some(slot) = slots.iter().position(|&slot| slot == value) {
        return TextOp::Copy(slot as u8);
    }
    if slots.len() < MAX_SLOTS {
        slots.push(value);
    }
    if derivable {
        TextOp::Derive
    } else {
        TextOp::New
    }
}

pub(super) type Context<'a> = (&'a [u8], &'a [u8]);

pub(super) type Split<'a> = (u8, &'a [u8]);

pub(super) type RuleSite = (u32, char);

pub(super) fn context_of<'a>(text: &'a [u8], fields: &[(usize, usize)]) -> Option<Context<'a>> {
    if fields.len() <= DERIVE_TARGET {
        return None;
    }
    let field = |number: usize| &text[fields[number].0..fields[number].1];
    Some((field(DERIVE_CONTEXT[0]), field(DERIVE_CONTEXT[1])))
}

pub(super) fn split_derivation<'v>(key: &str, value: &'v [u8]) -> Option<(u8, &'v [u8])> {
    let mut common = key
        .as_bytes()
        .iter()
        .zip(value)
        .take_while(|(left, right)| left == right)
        .count();
    while !key.is_char_boundary(common) {
        common -= 1;
    }
    let removed = key[common..].chars().count();
    let suffix = &value[common..];
    let valid = removed <= MAX_REMOVED_CHARS
        && suffix.len() <= MAX_RULE_SUFFIX
        && std::str::from_utf8(suffix).is_ok();
    valid.then_some((removed as u8, suffix))
}

pub(super) fn kept_bytes(key: &str, removed: u8) -> Option<usize> {
    let mut keep = key.len();
    for _ in 0..removed {
        keep -= key[..keep].chars().next_back()?.len_utf8();
    }
    Some(keep)
}

#[derive(Default)]
pub(super) struct Derive<'a> {
    pub(super) contexts: SmallKeyMap<Context<'a>, u32>,
    pub(super) table_of_context: Vec<u16>,
    pub(super) rules: SmallKeyMap<(u16, char), u16>,
    pub(super) rule_list: Vec<(u8, Vec<u8>)>,
    pub(super) entries: Vec<u32>,
    pub(super) group_starts: Vec<u32>,
}

impl<'a> Derive<'a> {
    pub(super) fn learn(
        entries: &[&'a [u8]],
        tokens: &[(u32, &str)],
        classes: &[Class],
        blob: Option<&Blob>,
    ) -> Self {
        let enabled = classes.len() > DERIVE_TARGET
            && DERIVE_CONTEXT
                .iter()
                .all(|&field| classes[field] == Class::Folded)
            && classes[DERIVE_TARGET] == Class::Text;
        let mut derive = Self::default();
        if !enabled {
            return derive;
        }
        let mut counts: SmallKeyMap<RuleSite, SmallKeyMap<Split<'a>, u32>> = SmallKeyMap::default();
        let mut fields = Vec::new();
        for &(feature, key) in tokens {
            let entry = feature as usize;
            let Some(&text) = entries.get(entry) else {
                continue;
            };
            let Some(last) = key.chars().next_back() else {
                continue;
            };
            split_fields(text, &mut fields);
            let Some(context) = context_of(text, &fields) else {
                continue;
            };
            let next = derive.contexts.len() as u32;
            let id = *derive.contexts.entry(context).or_insert(next);
            let (begin, end) = fields[DERIVE_TARGET];
            let value = &text[begin..end];
            if blob_slot(blob, entry, DERIVE_TARGET).is_some()
                || is_key(value, key.as_bytes())
                || value == b"*"
                || value.is_empty()
            {
                continue;
            }
            if let Some(split) = split_derivation(key, value) {
                *counts
                    .entry((id, last))
                    .or_default()
                    .entry(split)
                    .or_insert(0) += 1;
            }
        }
        let mut chosen: Vec<(RuleSite, Split)> = counts
            .into_iter()
            .filter_map(|(at, uses)| {
                let (split, count) = uses
                    .into_iter()
                    .max_by_key(|&(split, count)| (count, std::cmp::Reverse(split)))?;
                (count >= MIN_RULE_USES).then_some((at, split))
            })
            .collect();
        chosen.sort_unstable_by_key(|&(at, _)| at);
        derive.table_of_context = vec![0; derive.contexts.len()];
        let mut rule_index: SmallKeyMap<(u8, &[u8]), u16> = SmallKeyMap::default();
        let mut table = 0u16;
        let mut current: Option<u32> = None;
        for ((id, last), split) in chosen {
            let known = rule_index.get(&split).copied();
            let rule = match known {
                Some(rule) => rule,
                None if derive.rule_list.len() < MAX_RULES => {
                    let rule = derive.rule_list.len() as u16;
                    derive.rule_list.push((split.0, split.1.to_vec()));
                    rule_index.insert(split, rule);
                    rule
                }
                None => continue,
            };
            if current != Some(id) {
                current = Some(id);
                table += 1;
                derive.table_of_context[id as usize] = table;
                derive.group_starts.push(derive.entries.len() as u32);
            }
            derive.rules.insert((table, last), rule);
            derive
                .entries
                .push(last as u32 | u32::from(rule) << RULE_CHAR_BITS);
        }
        derive
    }

    pub(super) fn groups(&self) -> u32 {
        self.group_starts.len() as u32
    }

    pub(super) fn table_of(&self, text: &[u8], fields: &[(usize, usize)]) -> u16 {
        context_of(text, fields)
            .and_then(|context| self.contexts.get(&context))
            .map_or(0, |&id| self.table_of_context[id as usize])
    }

    pub(super) fn accepts(&self, table: u16, key: &[u8], value: &[u8]) -> bool {
        if table == 0 {
            return false;
        }
        let Ok(key) = std::str::from_utf8(key) else {
            return false;
        };
        let Some(last) = key.chars().next_back() else {
            return false;
        };
        let Some(&rule) = self.rules.get(&(table, last)) else {
            return false;
        };
        let (removed, suffix) = &self.rule_list[usize::from(rule)];
        let Some(keep) = kept_bytes(key, *removed) else {
            return false;
        };
        value.len() == keep + suffix.len()
            && value[..keep] == key.as_bytes()[..keep]
            && value[keep..] == suffix[..]
    }

    pub(super) fn rule_bytes(&self) -> Vec<u8> {
        let mut bytes = vec![0u8; RULE_BYTES * self.rule_list.len()];
        for (number, (removed, suffix)) in self.rule_list.iter().enumerate() {
            let at = number * RULE_BYTES;
            bytes[at] = *removed;
            bytes[at + 1] = suffix.len() as u8;
            let start = at + RULE_HEADER_BYTES;
            bytes[start..start + suffix.len()].copy_from_slice(suffix);
        }
        bytes
    }
}

pub(super) struct Planner<'p, 'a> {
    pub(super) classes: &'p [Class],
    pub(super) derive: &'p Derive<'a>,
    pub(super) blob: Option<&'p Blob<'a>>,
}

impl<'a> Planner<'_, 'a> {
    pub(super) fn plan(
        &self,
        entry: usize,
        text: &'a [u8],
        fields: &[(usize, usize)],
        key: &[u8],
        slots: &mut Vec<&'a [u8]>,
        ops: &mut Vec<Option<TextOp>>,
    ) {
        ops.clear();
        slots.clear();
        let table = self.derive.table_of(text, fields);
        for (field, &(begin, end)) in fields.iter().enumerate() {
            if self.classes[field] != Class::Text {
                ops.push(None);
                continue;
            }
            let value = &text[begin..end];
            if blob_slot(self.blob, entry, field).is_some() {
                if slots.len() < MAX_SLOTS {
                    slots.push(value);
                }
                ops.push(None);
                continue;
            }
            let derivable = field == DERIVE_TARGET && self.derive.accepts(table, key, value);
            ops.push(Some(text_op(value, key, derivable, slots)));
        }
    }

    pub(super) fn uses_key(&self, ops: &[Option<TextOp>]) -> bool {
        ops.iter()
            .any(|op| matches!(op, Some(TextOp::Key | TextOp::Derive)))
    }
}
