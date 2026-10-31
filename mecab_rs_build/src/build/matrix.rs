use std::io::{self, Write};
use std::path::Path;

use super::text::{LINE_BUFFER_BYTES, Lines, atoi, cstr, tokenize2};
use mecab_rs::__private::{
    MATRIX_BIN_FILE, MATRIX_COST_BYTES, MATRIX_DEF_FILE, MATRIX_HEADER_BYTES, io_error, u16_at,
};
use mecab_rs::Error;

const MATRIX_DEF_DEFAULT: &str = "1 1\n0 0 0\n";
const FIELDS_PER_LINE: usize = 3;
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
        return Err(Error::build(lines.path(), format!("format error: {shown}")));
    }
    let size_of = |field: usize| {
        usize::try_from(atoi(cstr(line, field)))
            .ok()
            .filter(|&size| u16::try_from(size).is_ok())
    };
    let (left, right) = (size_of(found[0]), size_of(found[1]));
    let (Some(left), Some(right)) = (left, right) else {
        return Err(Error::build(
            lines.path(),
            format!("matrix size out of range: {shown}"),
        ));
    };
    Ok(Some(MatrixSize { left, right }))
}

pub(super) fn sizes(dic_dir: &Path) -> Result<MatrixSize, Error> {
    let text = dic_dir.join(MATRIX_DEF_FILE);
    if text.is_file() {
        let mut lines = Lines::open(&text, Some(LINE_BUFFER_BYTES))?;
        return Ok(size_of_first_line(&mut lines)?.unwrap_or(MatrixSize { left: 0, right: 0 }));
    }
    let binary = dic_dir.join(MATRIX_BIN_FILE);
    if let Ok(mut file) = std::fs::File::open(&binary)
        && let Ok(length) = file.metadata().map(|meta| meta.len())
    {
        let mut header = [0u8; 4];
        if io::Read::read_exact(&mut file, &mut header).is_ok() {
            let left = usize::from(u16_at(&header, 0));
            let right = usize::from(u16_at(&header, 2));
            if (MATRIX_HEADER_BYTES + left * right * MATRIX_COST_BYTES) as u64 == length {
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
        Lines::open(matrix_def, Some(LINE_BUFFER_BYTES))?
    } else {
        eprintln!(
            "{} is not found. minimum setting is used.",
            matrix_def.display()
        );
        Lines::builtin(MATRIX_DEF_DEFAULT, matrix_def, Some(LINE_BUFFER_BYTES))
    };
    let declared = size_of_first_line(&mut lines)?
        .ok_or_else(|| Error::build(matrix_def, "format error: "))?;
    let left_size = declared.left;
    let right_size = declared.right;
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
            return Err(Error::build(matrix_def, format!("format error: {shown}")));
        }
        let [left, right, cost] = numbers;
        if left < 0 || right < 0 || left as usize >= left_size || right as usize >= right_size {
            return Err(Error::build(matrix_def, "index values are out of range"));
        }
        let cost = i16::try_from(cost).map_err(|_| {
            Error::build(matrix_def, format!("cost out of the 16-bit range: {cost}"))
        })?;
        matrix[left as usize + left_size * right as usize] = cost;
    }

    let write = |output: &mut dyn Write| -> io::Result<()> {
        output.write_all(&(left_size as u16).to_le_bytes())?;
        output.write_all(&(right_size as u16).to_le_bytes())?;
        let mut bytes = Vec::with_capacity(WRITE_CHUNK_CELLS * MATRIX_COST_BYTES);
        for chunk in matrix.chunks(WRITE_CHUNK_CELLS) {
            bytes.clear();
            for cell in chunk {
                bytes.extend_from_slice(&cell.to_le_bytes());
            }
            output.write_all(&bytes)?;
        }
        Ok(())
    };
    write(output).map_err(io_error(output_path))
}
