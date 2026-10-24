
use std::io::{self, Write};
use std::path::Path;

use super::text::{BUF_SIZE, Lines, atoi, cstr, failure, tokenize2};
use crate::error::Error;

const MATRIX_DEF_DEFAULT: &str = "1 1\n0 0 0\n";

fn size_of_first_line(lines: &mut Lines) -> Result<Option<(usize, usize)>, Error> {
    let Some(line) = lines.next_line()? else {
        return Ok(None);
    };
    let shown = String::from_utf8_lossy(cstr(line, 0)).into_owned();
    let found = tokenize2(line, b"\t ", 2);
    if found.len() != 2 {
        return Err(failure(lines.path(), format!("format error: {shown}")));
    }
    Ok(Some((
        atoi(cstr(line, found[0])) as usize,
        atoi(cstr(line, found[1])) as usize,
    )))
}

pub(super) fn sizes(dic_dir: &Path) -> Result<(usize, usize), Error> {
    let text = dic_dir.join("matrix.def");
    if text.is_file() {
        let mut lines = Lines::open(&text, Some(BUF_SIZE))?;
        return Ok(size_of_first_line(&mut lines)?.unwrap_or((0, 0)));
    }
    let binary = dic_dir.join("matrix.bin");
    if let Ok(mut file) = std::fs::File::open(&binary)
        && let Ok(length) = file.metadata().map(|meta| meta.len())
    {
        let mut header = [0u8; 4];
        if io::Read::read_exact(&mut file, &mut header).is_ok() {
            let left = usize::from(u16::from_le_bytes([header[0], header[1]]));
            let right = usize::from(u16::from_le_bytes([header[2], header[3]]));
            if (left * right + 2) as u64 * 2 == length {
                return Ok((left, right));
            }
        }
    }
    Ok((1, 1))
}

fn token<'a>(line: &'a [u8], at: &mut usize) -> Option<&'a [u8]> {
    while line[*at] == b' ' || line[*at] == b'\t' {
        *at += 1;
    }
    let start = *at;
    while line[*at] != 0 && line[*at] != b' ' && line[*at] != b'\t' {
        *at += 1;
    }
    (*at > start).then(|| &line[start..*at])
}

pub(super) fn compile(
    matrix_def: &Path,
    output_path: &Path,
    output: &mut impl Write,
) -> Result<(), Error> {
    let mut lines = if matrix_def.is_file() {
        Lines::open(matrix_def, Some(BUF_SIZE))?
    } else {
        eprintln!(
            "{} is not found. minimum setting is used.",
            matrix_def.display()
        );
        Lines::builtin(MATRIX_DEF_DEFAULT, matrix_def, Some(BUF_SIZE))
    };
    let (left_size, right_size) =
        size_of_first_line(&mut lines)?.ok_or_else(|| failure(matrix_def, "format error: "))?;
    let left_size = left_size as u16 as usize;
    let right_size = right_size as u16 as usize;
    let mut matrix = vec![0i16; left_size * right_size];

    while let Some(line) = lines.next_line()? {
        let mut at = 0;
        let mut numbers = [0i32; 3];
        let mut count = 0;
        while count < 3 {
            let Some(found) = token(line, &mut at) else {
                break;
            };
            numbers[count] = atoi(found);
            count += 1;
        }
        if count != 3 {
            let shown = String::from_utf8_lossy(cstr(line, 0)).into_owned();
            return Err(failure(matrix_def, format!("format error: {shown}")));
        }
        let [left, right, cost] = numbers;
        if left < 0 || right < 0 || left as usize >= left_size || right as usize >= right_size {
            return Err(failure(matrix_def, "index values are out of range"));
        }
        matrix[left as usize + left_size * right as usize] = cost as i16;
    }

    let write = |output: &mut dyn Write| -> io::Result<()> {
        output.write_all(&(left_size as u16).to_le_bytes())?;
        output.write_all(&(right_size as u16).to_le_bytes())?;
        let mut bytes = Vec::with_capacity(1 << 20);
        for chunk in matrix.chunks(1 << 19) {
            bytes.clear();
            for cell in chunk {
                bytes.extend_from_slice(&cell.to_le_bytes());
            }
            output.write_all(&bytes)?;
        }
        Ok(())
    };
    write(output).map_err(|source| Error::Io {
        path: output_path.to_path_buf(),
        source,
    })
}
