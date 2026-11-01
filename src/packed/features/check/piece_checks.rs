use super::super::pieces::{POSITION_BYTES, Position};
use super::super::{Layout, read_bits};
use super::{offsets_of, read_range};
use crate::Error;
use crate::packed::reader::Source;

pub(super) fn check_pieces(source: &Source, section: usize, layout: &Layout) -> Result<(), Error> {
    let positions = read_positions(source, section, layout)?;
    if !positions_fit_entries(&positions, layout.counts.entry_bits as usize) {
        return Err(source.broken("a piece position of the features leaves its entry"));
    }
    check_pool(source, section, layout, &positions)?;
    check_template_pieces(source, section, layout, &positions)
}

fn read_positions(
    source: &Source,
    section: usize,
    layout: &Layout,
) -> Result<Vec<Position>, Error> {
    let position_bytes = read_range(source, section, &layout.positions)?;
    position_bytes
        .as_chunks::<POSITION_BYTES>()
        .0
        .iter()
        .map(Position::parse)
        .collect::<Option<_>>()
        .ok_or_else(|| source.broken("a piece position of the features is not valid"))
}

fn positions_fit_entries(positions: &[Position], entry_bits: usize) -> bool {
    positions.iter().all(|position| {
        position.constant || position.shift as usize + position.bits as usize <= entry_bits
    })
}

fn check_pool(
    source: &Source,
    section: usize,
    layout: &Layout,
    positions: &[Position],
) -> Result<(), Error> {
    let mut pool = read_range(source, section, &layout.pool)?;
    pool.truncate(layout.counts.pool_bytes as usize);
    let offsets = offsets_of(
        &read_range(source, section, &layout.piece_offsets)?,
        pool.len(),
    )
    .ok_or_else(|| source.broken("the feature pieces are not delimited correctly"))?;
    if !pieces_are_utf8(&pool, &offsets) {
        return Err(source.broken("a feature piece is not valid UTF-8"));
    }
    if !constant_pieces_are_utf8(&pool, positions) {
        return Err(source.broken("a constant feature piece is not valid"));
    }
    Ok(())
}

fn pieces_are_utf8(pool: &[u8], offsets: &[u32]) -> bool {
    offsets
        .windows(2)
        .all(|pair| str::from_utf8(&pool[pair[0] as usize..pair[1] as usize]).is_ok())
}

fn constant_pieces_are_utf8(pool: &[u8], positions: &[Position]) -> bool {
    positions
        .iter()
        .filter(|position| position.constant)
        .all(|position| {
            let start = position.base as usize;
            pool.get(start..start + position.shift as usize)
                .is_some_and(|piece| str::from_utf8(piece).is_ok())
        })
}

fn check_template_pieces(
    source: &Source,
    section: usize,
    layout: &Layout,
    positions: &[Position],
) -> Result<(), Error> {
    let counts = layout.counts;
    let entry_bits = counts.entry_bits as usize;
    let entries = read_range(source, section, &layout.entries)?;
    let pieces = u64::from(counts.pieces);
    let names_missing_piece = (0..counts.templates as usize).any(|template| {
        positions
            .iter()
            .filter(|position| !position.constant)
            .any(|position| {
                let local = read_bits(
                    &entries,
                    template * entry_bits + position.shift as usize,
                    position.bits,
                );
                u64::from(position.base) + local >= pieces
            })
    });
    if names_missing_piece {
        return Err(source.broken("a feature template names a piece that does not exist"));
    }
    Ok(())
}
