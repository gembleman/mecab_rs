use std::path::Path;

use super::{IntContext, MAGIC, VERSION, projection_digest};
use crate::Error;
use crate::int_templates::IntTemplates;
use crate::part::IntPart;
use crate::template_universe::UniverseSide;
use crate::wire::{fnv1a64, put_varint, zigzag};

fn expand(
    part: &IntPart,
    left: &UniverseSide,
    right: &UniverseSide,
    path: &Path,
) -> Result<IntPart, Error> {
    let inconsistent = || Error::rebuild(path, "the id definitions do not match the templates");
    let key_map = |projection: &[u16], count: usize, side: &UniverseSide| {
        if projection.len() != side.projection.len() {
            return Err(inconsistent());
        }
        let mut map = vec![u16::MAX; count];
        for (&key, &universe) in projection.iter().zip(&side.projection) {
            if usize::from(key) < count {
                let slot = &mut map[usize::from(key)];
                if usize::from(universe) >= side.count || (*slot != u16::MAX && *slot != universe) {
                    return Err(inconsistent());
                }
                *slot = universe;
            }
        }
        if map.contains(&u16::MAX) || map.windows(2).any(|pair| pair[0] >= pair[1]) {
            return Err(inconsistent());
        }
        Ok(map)
    };
    let left_map = key_map(&part.left_projection, part.left_count, left)?;
    let right_map = key_map(&part.right_projection, part.right_count, right)?;
    let mut indptr = vec![0u32; right.count + 1];
    for (key, span) in part.indptr.windows(2).enumerate() {
        indptr[usize::from(right_map[key]) + 1] = span[1] - span[0];
    }
    for at in 1..indptr.len() {
        indptr[at] += indptr[at - 1];
    }
    Ok(IntPart {
        left_count: left.count,
        right_count: right.count,
        left_projection: left.projection.clone(),
        right_projection: right.projection.clone(),
        indptr,
        left_index: part
            .left_index
            .iter()
            .map(|&key| left_map[usize::from(key)])
            .collect(),
        weight: part.weight.clone(),
    })
}

fn template_text(bigrams: &[(String, String)]) -> String {
    let mut text = String::new();
    for (name, body) in bigrams {
        text.push_str(name);
        text.push('\t');
        text.push_str(body);
        text.push('\n');
    }
    text
}

#[derive(Default)]
struct Streams {
    pub header: Vec<u8>,
    pub counts: Vec<u8>,
    pub lefts: Vec<u8>,
    pub weights: Vec<u8>,
    pub exception_cells: Vec<u8>,
    pub exception_values: Vec<u8>,
}

impl Streams {
    fn payload(&self) -> Vec<u8> {
        let mut out = self.header.clone();
        for stream in [
            &self.counts,
            &self.lefts,
            &self.weights,
            &self.exception_cells,
            &self.exception_values,
        ] {
            out.extend_from_slice(stream);
        }
        out.extend_from_slice(&fnv1a64(&out).to_le_bytes());
        out
    }
}

fn encode_part(streams: &mut Streams, part: &IntPart) {
    for key in 0..part.right_count {
        let span = part.indptr[key] as usize..part.indptr[key + 1] as usize;
        put_varint(&mut streams.counts, span.len() as u64);
        let mut previous = None;
        for &left_key in &part.left_index[span] {
            let coded = previous.map_or(u64::from(left_key), |p: u16| u64::from(left_key - p - 1));
            put_varint(&mut streams.lefts, coded);
            previous = Some(left_key);
        }
    }
    for &weight in &part.weight {
        put_varint(&mut streams.weights, zigzag(i64::from(weight)));
    }
}

fn encode_exceptions(streams: &mut Streams, templates: &IntTemplates) {
    let left_size = templates.left_size();
    let mut previous = None;
    for (cell, value) in templates.exceptions() {
        let coded = previous.map_or(u64::from(cell), |p: u32| u64::from(cell - p - 1));
        put_varint(&mut streams.exception_cells, coded);
        previous = Some(cell);
        let computed = templates.computed_cost(
            (cell as usize % left_size) as u16,
            (cell as usize / left_size) as u16,
        );
        put_varint(
            &mut streams.exception_values,
            zigzag(i64::from(value) - i64::from(computed)),
        );
    }
}

fn write_header(streams: &mut Streams, templates: &IntTemplates, context: &IntContext) {
    let text = template_text(&context.bigrams);
    let lengths = [
        streams.counts.len(),
        streams.lefts.len(),
        streams.weights.len(),
        streams.exception_cells.len(),
        streams.exception_values.len(),
        text.len(),
    ];
    let header = &mut streams.header;
    header.extend_from_slice(MAGIC);
    for value in [
        VERSION,
        templates.cost_factor(),
        templates.shift(),
        templates.left_size() as u32,
        templates.right_size() as u32,
        templates.parts().len() as u32,
        templates.exception_count() as u32,
    ]
    .into_iter()
    .chain(lengths.map(|length| length as u32))
    {
        header.extend_from_slice(&value.to_le_bytes());
    }
    header.extend_from_slice(&projection_digest(&context.universe).to_le_bytes());
    header.extend_from_slice(text.as_bytes());
}

fn encode_streams(
    templates: &IntTemplates,
    context: &IntContext,
    path: &Path,
) -> Result<Streams, Error> {
    if templates.parts().len() != context.universe.len() {
        return Err(Error::rebuild(
            path,
            "the templates do not match feature.def",
        ));
    }
    let mut streams = Streams::default();
    for (part, (left, right)) in templates.parts().iter().zip(&context.universe) {
        encode_part(&mut streams, &expand(part, left, right, path)?);
    }
    encode_exceptions(&mut streams, templates);
    write_header(&mut streams, templates, context);
    Ok(streams)
}

pub fn encode_payload(
    templates: &IntTemplates,
    context: &IntContext,
    path: &Path,
) -> Result<Vec<u8>, Error> {
    Ok(encode_streams(templates, context, path)?.payload())
}
