use std::io::{self, Write};
use std::path::Path;

use super::text::{BUF_SIZE, Lines, atoi, cstr, failure, tokenize2};
use crate::error::Error;

const MATRIX_DEF_DEFAULT: &str = "1 1\n0 0 0\n";
const FIELDS_PER_LINE: usize = 3;
const HEADER_CELLS: usize = 2;
const CELL_BYTES: usize = 2;
const WRITE_CHUNK_CELLS: usize = 1 << 19;

#[derive(Clone, Copy)]
pub(super) struct MatrixSize {
    pub(super) left: usize,
    pub(super) right: usize,
}

fn size_of_first_line(lines: &mut Lines) -> Result<Option<MatrixSize>, Error> {
    let Some(line) = lines.next_line()? else {
        return Ok(None);
    };
    let shown = String::from_utf8_lossy(cstr(line, 0)).into_owned();
    let found = tokenize2(line, b"\t ", 2);
    if found.len() != 2 {
        return Err(failure(lines.path(), format!("format error: {shown}")));
    }
    Ok(Some(MatrixSize {
        left: atoi(cstr(line, found[0])) as usize,
        right: atoi(cstr(line, found[1])) as usize,
    }))
}

pub(super) fn sizes(dic_dir: &Path) -> Result<MatrixSize, Error> {
    let text = dic_dir.join("matrix.def");
    if text.is_file() {
        let mut lines = Lines::open(&text, Some(BUF_SIZE))?;
        return Ok(size_of_first_line(&mut lines)?.unwrap_or(MatrixSize { left: 0, right: 0 }));
    }
    let binary = dic_dir.join("matrix.bin");
    if let Ok(mut file) = std::fs::File::open(&binary)
        && let Ok(length) = file.metadata().map(|meta| meta.len())
    {
        let mut header = [0u8; 4];
        if io::Read::read_exact(&mut file, &mut header).is_ok() {
            let left = usize::from(u16::from_le_bytes([header[0], header[1]]));
            let right = usize::from(u16::from_le_bytes([header[2], header[3]]));
            if ((left * right + HEADER_CELLS) * CELL_BYTES) as u64 == length {
                return Ok(MatrixSize { left, right });
            }
        }
    }
    Ok(MatrixSize { left: 1, right: 1 })
}

fn next_field<'a>(line: &'a [u8], cursor: &mut usize) -> Option<&'a [u8]> {
    while line[*cursor] == b' ' || line[*cursor] == b'\t' {
        *cursor += 1;
    }
    let start = *cursor;
    while line[*cursor] != 0 && line[*cursor] != b' ' && line[*cursor] != b'\t' {
        *cursor += 1;
    }
    (*cursor > start).then(|| &line[start..*cursor])
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
    let declared =
        size_of_first_line(&mut lines)?.ok_or_else(|| failure(matrix_def, "format error: "))?;
    let left_size = declared.left as u16 as usize;
    let right_size = declared.right as u16 as usize;
    let mut matrix = vec![0i16; left_size * right_size];

    while let Some(line) = lines.next_line()? {
        let mut cursor = 0;
        let mut numbers = [0i32; FIELDS_PER_LINE];
        let mut count = 0;
        while count < FIELDS_PER_LINE {
            let Some(found) = next_field(line, &mut cursor) else {
                break;
            };
            numbers[count] = atoi(found);
            count += 1;
        }
        if count != FIELDS_PER_LINE {
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
        let mut bytes = Vec::with_capacity(WRITE_CHUNK_CELLS * CELL_BYTES);
        for chunk in matrix.chunks(WRITE_CHUNK_CELLS) {
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
