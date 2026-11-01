use super::MAX_FIELDS;

pub(super) fn split_fields(text: &[u8], fields: &mut Vec<(usize, usize)>) {
    fields.clear();
    let length = text.len();
    let mut start = 0;
    loop {
        if fields.len() == MAX_FIELDS - 1 {
            fields.push((start, length));
            return;
        }
        let mut end = None;
        if text.get(start) == Some(&b'"') {
            let mut at = start + 1;
            while let Some(found) = text[at..].iter().position(|&byte| byte == b'"') {
                let quote = at + found;
                if text.get(quote + 1) == Some(&b'"') {
                    at = quote + 2;
                    continue;
                }
                if quote + 1 == length || text[quote + 1] == b',' {
                    end = Some(quote + 1);
                }
                break;
            }
        }
        let end = end.unwrap_or_else(|| {
            text[start..]
                .iter()
                .position(|&byte| byte == b',')
                .map_or(length, |found| start + found)
        });
        fields.push((start, end));
        if end == length {
            return;
        }
        start = end + 1;
    }
}

pub(super) fn parse_number(field: &[u8]) -> Option<u64> {
    if field.is_empty() || field.len() > 20 || (field[0] == b'0' && field.len() > 1) {
        return None;
    }
    let mut value = 0u64;
    for &byte in field {
        if !byte.is_ascii_digit() {
            return None;
        }
        value = value.checked_mul(10)?.checked_add(u64::from(byte - b'0'))?;
    }
    Some(value)
}
