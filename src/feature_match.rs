const CSV_BUFFER_SIZE: usize = 8192;
const MAX_COLUMNS: usize = 64;
const WILDCARD: &[u8] = b"*";

pub(super) fn partial_match(constraint: &[u8], feature: &[u8]) -> bool {
    if constraint == WILDCARD {
        return true;
    }
    let (constraint_buffer, constraint_columns) = split_columns(constraint);
    let (feature_buffer, feature_columns) = split_columns(feature);
    constraint_columns.iter().zip(&feature_columns).all(|(&left, &right)| {
        let wanted = terminated(&constraint_buffer, left);
        wanted == WILDCARD || wanted == terminated(&feature_buffer, right)
    })
}

fn terminated(buffer: &[u8], start: usize) -> &[u8] {
    let rest = &buffer[start..];
    let length = rest.iter().position(|&byte| byte == 0).unwrap_or(rest.len());
    &rest[..length]
}

fn split_columns(source: &[u8]) -> (Vec<u8>, Vec<usize>) {
    let kept = source.len().min(CSV_BUFFER_SIZE - 1);
    let mut buffer = source[..kept].to_vec();
    buffer.push(0);
    let columns = tokenize_csv(&mut buffer, MAX_COLUMNS);
    (buffer, columns)
}

fn find_comma(buffer: &[u8], from: usize, end_of_string: usize) -> usize {
    (from..end_of_string).find(|&at| buffer[at] == b',').unwrap_or(end_of_string)
}

fn tokenize_csv(buffer: &mut [u8], max: usize) -> Vec<usize> {
    let end_of_string = buffer.len() - 1;
    let mut columns = Vec::new();
    let mut remaining = max;
    let mut at = 0;
    while at < end_of_string {
        while matches!(buffer[at], b' ' | b'\t') {
            at += 1;
        }
        let start;
        let end;
        if buffer[at] == b'"' {
            at += 1;
            start = at;
            let mut write = start;
            while at < end_of_string {
                if buffer[at] == b'"' {
                    at += 1;
                    if buffer[at] != b'"' {
                        break;
                    }
                }
                buffer[write] = buffer[at];
                write += 1;
                at += 1;
            }
            at = find_comma(buffer, at, end_of_string);
            end = write;
        } else {
            start = at;
            at = find_comma(buffer, at, end_of_string);
            end = at;
        }
        let terminate = remaining > 1;
        remaining -= 1;
        if terminate {
            buffer[end] = 0;
        }
        columns.push(start);
        if remaining == 0 {
            break;
        }
        at += 1;
    }
    columns
}
