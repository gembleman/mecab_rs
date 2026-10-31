use super::{
    BLOCK_CODES, BLOCK_COUNT, CATEGORY_COUNT_AT, CELLS, CELLS_PADDING, CHAR_BLOCKS, CHAR_INDEX,
    CHAR_TABLE, CHAR_TABLE_HEADER_BYTES, CHAR_TABLE_MAGIC, CODE_COUNT_AT, DICRC, Error,
    FEATURE_BITS_AT, FLAG_HOT_MATRIX, FLAG_INT_TEMPLATES, FLAGS_AT, HEADER_BYTES, HOT_ROWS_BYTES,
    HotShape, MAGIC, MAGIC_BYTES, MATRIX, MATRIX_HEADER_BYTES, MAX_RECORD_BITS, MAX_UNITS,
    PARAM_BITS_AT, PARAM_BYTES, PARAM_COUNT_AT, PARAMS, Path, ROW_START_BYTES, Range,
    SECTION_ALIGNMENT, SECTION_ENTRY_BYTES, SECTION_LENGTH_IN_ENTRY, SECTION_OFFSET_IN_ENTRY,
    SECTION_TABLE_AT, SECTIONS, Source, TEMPLATE_HEADER_PEEK_BYTES, TEMPLATES, TOKEN_COUNT_AT,
    TOKENS, TokenFormat, UNIT_BYTES, UNKNOWN_STARTS, UNKNOWN_TOKENS, VERSION, VERSION_AT,
    check_tokens, feature_check, header_sizes, is_runtime_form, section_bytes, u16_at, u16_values,
    u32_at, u32_values, u64_at,
};

const KNOWN_FLAGS: u32 = FLAG_HOT_MATRIX | FLAG_INT_TEMPLATES;
const MAX_FORMAT_BITS: u32 = 32;
pub(super) const DICRC_HEADER_BYTES: usize = 8;
const UNKNOWN_START_BYTES: usize = 4;
const BLOCK_ENTRY_BYTES: usize = 2;

const CHAR_TABLE_CATEGORIES_AT: usize = 8;

pub(super) struct Header {
    pub(super) flags: u32,
    pub(super) code_count: usize,
    pub(super) token_count: usize,
    pub(super) param_count: usize,
    pub(super) category_count: usize,
    pub(super) format: TokenFormat,
}

impl Header {
    pub(super) fn hot(&self) -> bool {
        self.flags & FLAG_HOT_MATRIX != 0
    }
}

fn check_flags(flags: u32, path: &Path) -> Result<(), Error> {
    if flags & !KNOWN_FLAGS != 0 {
        return Err(Error::format(path, "the flags are not valid"));
    }
    if (flags & FLAG_INT_TEMPLATES != 0) != (flags & FLAG_HOT_MATRIX != 0) {
        return Err(Error::format(
            path,
            "a hot matrix and integer templates come together",
        ));
    }
    Ok(())
}

fn token_format(header: &[u8], path: &Path) -> Result<TokenFormat, Error> {
    let broken = || Error::format(path, "the widths of the token fields are not valid");
    let (param_bits, feature_bits) = (
        u32_at(header, PARAM_BITS_AT),
        u32_at(header, FEATURE_BITS_AT),
    );
    let in_range = |bits: u32| bits != 0 && bits <= MAX_FORMAT_BITS;
    if !in_range(param_bits) || !in_range(feature_bits) {
        return Err(broken());
    }
    let format = TokenFormat::new(param_bits, feature_bits);
    if format.bits > MAX_RECORD_BITS as usize {
        return Err(broken());
    }
    Ok(format)
}

pub(super) fn parse_header(header: &[u8], path: &Path) -> Result<Header, Error> {
    if header[..MAGIC_BYTES] != *MAGIC {
        return Err(Error::format(path, "the magic number is not valid"));
    }
    if u32_at(header, VERSION_AT) != VERSION {
        return Err(Error::format(
            path,
            "the file has another format version; pack the dictionary again",
        ));
    }
    let flags = u32_at(header, FLAGS_AT);
    check_flags(flags, path)?;
    Ok(Header {
        flags,
        code_count: u32_at(header, CODE_COUNT_AT) as usize,
        token_count: u32_at(header, TOKEN_COUNT_AT) as usize,
        param_count: u32_at(header, PARAM_COUNT_AT) as usize,
        category_count: u32_at(header, CATEGORY_COUNT_AT) as usize,
        format: token_format(header, path)?,
    })
}

pub(super) fn parse_sections(
    header: &[u8],
    file_length: u64,
    path: &Path,
) -> Result<[Range<usize>; SECTIONS], Error> {
    let mut sections: [Range<usize>; SECTIONS] = Default::default();
    let mut end = HEADER_BYTES as u64;
    for (number, section) in sections.iter_mut().enumerate() {
        let at = SECTION_TABLE_AT + SECTION_ENTRY_BYTES * number;
        let (offset, length) = (
            u64_at(header, at + SECTION_OFFSET_IN_ENTRY),
            u64_at(header, at + SECTION_LENGTH_IN_ENTRY),
        );
        let stop = offset.checked_add(length);
        let misplaced = !offset.is_multiple_of(SECTION_ALIGNMENT as u64)
            || offset < end
            || stop.is_none_or(|stop| stop > file_length);
        if misplaced {
            return Err(Error::format(
                path,
                "a section is misplaced or outside the file",
            ));
        }
        end = offset + length;
        *section = offset as usize..(offset + length) as usize;
    }
    Ok(sections)
}

pub(super) fn check_section_sizes(source: &Source, header: &Header) -> Result<(), Error> {
    let length = |section: usize| source.section_length(section);
    let cell_bytes = length(CELLS).saturating_sub(CELLS_PADDING);
    let sized = length(CHAR_INDEX) == BLOCK_COUNT * BLOCK_ENTRY_BYTES
        && length(CHAR_BLOCKS) != 0
        && length(CHAR_BLOCKS).is_multiple_of(BLOCK_CODES * BLOCK_ENTRY_BYTES)
        && length(CELLS) >= UNIT_BYTES + CELLS_PADDING
        && cell_bytes.is_multiple_of(UNIT_BYTES)
        && cell_bytes / UNIT_BYTES <= MAX_UNITS
        && length(TOKENS) as u64 == section_bytes(header.token_count, header.format.bits)
        && length(PARAMS) == header.param_count * PARAM_BYTES
        && length(UNKNOWN_STARTS) == (header.category_count + 1) * UNKNOWN_START_BYTES
        && header.code_count != 0
        && header.code_count <= 1 << 16
        && header.token_count <= MAX_UNITS;
    if sized {
        Ok(())
    } else {
        Err(source.broken("the size of a section does not match the header"))
    }
}

fn expected_matrix_length(
    source: &Source,
    hot: bool,
    left_size: usize,
    right_size: usize,
) -> Result<usize, Error> {
    if !hot {
        return Ok(MATRIX_HEADER_BYTES + left_size * right_size * 2);
    }
    let mut hot_header = [0u8; HOT_ROWS_BYTES];
    source.read(MATRIX, MATRIX_HEADER_BYTES, &mut hot_header)?;
    let rows = u32_at(&hot_header, 0) as usize;
    if rows == 0 || rows > right_size {
        return Err(source.broken("the hot block lies outside of the matrix"));
    }
    let mut last_start = [0u8; ROW_START_BYTES];
    source.read(
        MATRIX,
        HotShape::header_bytes(rows) - ROW_START_BYTES,
        &mut last_start,
    )?;
    let cells = u32_at(&last_start, 0) as usize;
    Ok(HotShape::header_bytes(rows) + cells * 2)
}

fn check_templates(
    source: &Source,
    header: &Header,
    left_size: usize,
    right_size: usize,
) -> Result<(), Error> {
    let length = source.section_length(TEMPLATES);
    if !header.hot() {
        return if length == 0 {
            Ok(())
        } else {
            Err(source.broken("a dense matrix comes with templates"))
        };
    }
    if length < TEMPLATE_HEADER_PEEK_BYTES {
        return Err(source.broken("the integer templates are empty"));
    }
    let mut templates_header = [0u8; TEMPLATE_HEADER_PEEK_BYTES];
    source.read(TEMPLATES, 0, &mut templates_header)?;
    let sizes = Some((left_size, right_size));
    if !is_runtime_form(&templates_header) || header_sizes(&templates_header) != sizes {
        return Err(source.broken("the integer templates do not match the hot matrix"));
    }
    Ok(())
}

pub(super) fn check_matrix(source: &Source, header: &Header) -> Result<(usize, usize), Error> {
    if source.section_length(MATRIX) < MATRIX_HEADER_BYTES {
        return Err(source.broken("the matrix is shorter than its header"));
    }
    let mut matrix_header = [0u8; MATRIX_HEADER_BYTES];
    source.read(MATRIX, 0, &mut matrix_header)?;
    let left_size = usize::from(u16_at(&matrix_header, 0));
    let right_size = usize::from(u16_at(&matrix_header, 2));
    let expected = expected_matrix_length(source, header.hot(), left_size, right_size)?;
    if left_size == 0 || right_size == 0 || source.section_length(MATRIX) != expected {
        return Err(source.broken("the matrix size does not match its header"));
    }
    check_templates(source, header, left_size, right_size)?;
    Ok((left_size, right_size))
}

pub(super) fn check_char_table(source: &Source, category_count: usize) -> Result<(), Error> {
    if source.section_length(CHAR_TABLE) < CHAR_TABLE_HEADER_BYTES {
        return Err(source.broken("the character table is shorter than its header"));
    }
    let mut table_header = vec![0u8; CHAR_TABLE_HEADER_BYTES];
    source.read(CHAR_TABLE, 0, &mut table_header)?;
    if table_header[..4] != *CHAR_TABLE_MAGIC
        || u32_at(&table_header, CHAR_TABLE_CATEGORIES_AT) as usize != category_count
    {
        return Err(source.broken(
            "the character table is not valid or has other categories than the unknown words",
        ));
    }
    Ok(())
}

pub(super) fn check_char_codes(source: &Source, code_count: usize) -> Result<(), Error> {
    let index = source.read_all(CHAR_INDEX)?;
    let block_count = source.section_length(CHAR_BLOCKS) / (BLOCK_CODES * BLOCK_ENTRY_BYTES);
    if u16_values(&index).any(|block| usize::from(block) >= block_count) {
        return Err(source.broken("a character block does not exist"));
    }
    source.chunks(
        CHAR_BLOCKS,
        2,
        |_, part| u16_values(part).all(|code| usize::from(code) < code_count),
        "a character code is out of range",
    )
}

pub(super) fn check_params(
    source: &Source,
    left_size: usize,
    right_size: usize,
) -> Result<(), Error> {
    source.chunks(
        PARAMS,
        PARAM_BYTES,
        |_, part| {
            part.as_chunks::<PARAM_BYTES>().0.iter().all(|param| {
                usize::from(u16_at(param, 0)) < right_size
                    && usize::from(u16_at(param, 2)) < left_size
            })
        },
        "a parameter set has a context id outside the matrix",
    )
}

pub(super) fn check_unknown_starts(source: &Source, header: &Header) -> Result<usize, Error> {
    let unknown_starts = source.read_all(UNKNOWN_STARTS)?;
    let unknown_starts: Vec<u32> = u32_values(&unknown_starts).collect();
    let broken = || source.broken("the unknown tokens of the categories do not add up");
    if unknown_starts[0] != 0 || unknown_starts.windows(2).any(|pair| pair[0] > pair[1]) {
        return Err(broken());
    }
    let unknown_count = unknown_starts[header.category_count] as usize;
    if source.section_length(UNKNOWN_TOKENS) as u64
        != section_bytes(unknown_count, header.format.bits)
    {
        return Err(broken());
    }
    Ok(unknown_count)
}

pub(super) fn check_token_sections(
    source: &Source,
    header: &Header,
    unknown_count: usize,
    checked: &feature_check::Checked,
) -> Result<(), Error> {
    let param_count = header.param_count;
    let valid_token = |param: u64, feature: u64| {
        param < param_count as u64 && checked.is_record(feature as usize)
    };
    let reason = "a token has a parameter set or a feature that does not exist";
    for (section, count) in [
        (TOKENS, header.token_count),
        (UNKNOWN_TOKENS, unknown_count),
    ] {
        check_tokens(source, section, count, header.format, valid_token, reason)?;
    }
    Ok(())
}

pub(super) struct Validated {
    pub(super) header: Header,
    pub(super) left_size: usize,
    pub(super) right_size: usize,
    pub(super) unknown_count: usize,
    pub(super) checked: feature_check::Checked,
    pub(super) dicrc: Dicrc,
}

pub(super) struct Dicrc {
    pub(super) bos_length: usize,
    pub(super) unknown_length: usize,
}

pub(super) fn check_dicrc(source: &Source) -> Result<Dicrc, Error> {
    let dicrc = source.read_all(DICRC)?;
    if dicrc.len() < DICRC_HEADER_BYTES {
        return Err(source.broken("dicrc is shorter than its header"));
    }
    let bos_length = u32_at(&dicrc, 0) as usize;
    let unknown_length = u32_at(&dicrc, 4) as usize;
    if bos_length == 0 || DICRC_HEADER_BYTES + bos_length + unknown_length != dicrc.len() {
        return Err(source.broken("the bos-feature or unk-feature length is not valid"));
    }
    let text = &dicrc[DICRC_HEADER_BYTES..];
    if str::from_utf8(text).is_err() || str::from_utf8(&text[..bos_length]).is_err() {
        return Err(source.broken("bos-feature or unk-feature is not valid UTF-8"));
    }
    Ok(Dicrc {
        bos_length,
        unknown_length,
    })
}
