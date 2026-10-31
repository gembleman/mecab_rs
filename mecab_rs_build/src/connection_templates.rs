use mecab_rs::__private::matrix::Part;
use mecab_rs::Error;

const MIN_COST: f64 = -32767.0;
const MAX_COST: f64 = 32767.0;
const MAX_MATRIX_SIZE: usize = 0xFFFF;

pub type TemplatePart = Part<f64>;

pub struct ConnectionTemplates {
    cost_factor: f64,
    left_size: usize,
    right_size: usize,
    parts: Vec<TemplatePart>,
}

pub fn to_cost(sum: f64, cost_factor: f64) -> i16 {
    (-cost_factor * sum).clamp(MIN_COST, MAX_COST) as i16
}

fn has_sorted_spans(part: &TemplatePart) -> bool {
    part.indptr.windows(2).all(|span| {
        part.left_index[span[0] as usize..span[1] as usize]
            .windows(2)
            .all(|pair| pair[0] < pair[1])
    })
}

fn has_sizes(part: &TemplatePart, left_size: usize, right_size: usize) -> bool {
    part.left_projection.len() == left_size
        && part.right_projection.len() == right_size
        && part.weight.len() == part.left_index.len()
        && part.indptr.len() == part.right_count + 1
}

fn is_consistent(part: &TemplatePart) -> bool {
    let nnz = part.left_index.len();
    part.indptr.first() == Some(&0)
        && part.indptr.last().is_some_and(|&end| end as usize == nnz)
        && part.indptr.windows(2).all(|span| span[0] <= span[1])
        && part
            .left_projection
            .iter()
            .all(|&key| usize::from(key) <= part.left_count)
        && part
            .right_projection
            .iter()
            .all(|&key| usize::from(key) <= part.right_count)
        && part
            .left_index
            .iter()
            .all(|&key| usize::from(key) < part.left_count)
}

fn sort_spans(part: &mut TemplatePart) {
    let mut left_index = Vec::with_capacity(part.left_index.len());
    let mut weight = Vec::with_capacity(part.weight.len());
    let mut indptr = Vec::with_capacity(part.indptr.len());
    indptr.push(0);
    for span in part.indptr.windows(2) {
        let range = span[0] as usize..span[1] as usize;
        let mut cells: Vec<(u16, f64)> = part.left_index[range.clone()]
            .iter()
            .copied()
            .zip(part.weight[range].iter().copied())
            .collect();
        cells.reverse();
        cells.sort_by_key(|&(index, _)| index);
        cells.dedup_by_key(|&mut (index, _)| index);
        for (index, value) in cells {
            left_index.push(index);
            weight.push(value);
        }
        indptr.push(left_index.len() as u32);
    }
    part.left_index = left_index;
    part.weight = weight;
    part.indptr = indptr;
}

fn validate(part: &TemplatePart, left_size: usize, right_size: usize) -> Result<(), Error> {
    if has_sizes(part, left_size, right_size) && is_consistent(part) {
        Ok(())
    } else {
        Err(Error::Corrupt("a connection template is inconsistent"))
    }
}

fn weight_of(part: &TemplatePart, left_node_rc: u16, right_node_lc: u16) -> Option<f64> {
    let right_key = usize::from(part.right_projection[usize::from(right_node_lc)]);
    if right_key == part.right_count {
        return None;
    }
    let left_key = part.left_projection[usize::from(left_node_rc)];
    let span = part.indptr[right_key] as usize..part.indptr[right_key + 1] as usize;
    let found = part.left_index[span.clone()]
        .binary_search(&left_key)
        .ok()?;
    Some(part.weight[span.start + found])
}

impl ConnectionTemplates {
    pub fn new(
        cost_factor: f64,
        left_size: usize,
        right_size: usize,
        mut parts: Vec<TemplatePart>,
    ) -> Result<Self, Error> {
        if left_size == 0
            || right_size == 0
            || left_size > MAX_MATRIX_SIZE
            || right_size > MAX_MATRIX_SIZE
        {
            return Err(Error::Corrupt("the template matrix size is out of range"));
        }
        for part in &mut parts {
            validate(part, left_size, right_size)?;
            if !has_sorted_spans(part) {
                sort_spans(part);
            }
        }
        Ok(Self {
            cost_factor,
            left_size,
            right_size,
            parts,
        })
    }

    pub fn left_size(&self) -> usize {
        self.left_size
    }

    pub fn right_size(&self) -> usize {
        self.right_size
    }

    pub fn cost_factor(&self) -> f64 {
        self.cost_factor
    }

    pub fn parts(&self) -> &[TemplatePart] {
        &self.parts
    }

    pub fn cost(&self, left_node_rc: u16, right_node_lc: u16) -> i16 {
        let total = self
            .parts
            .iter()
            .filter_map(|part| weight_of(part, left_node_rc, right_node_lc))
            .fold(0.0, |mut sum, weight| {
                sum += weight;
                sum
            });
        to_cost(total, self.cost_factor)
    }
}

pub struct ExactRows<'a> {
    templates: &'a ConnectionTemplates,
    sums: Vec<f64>,
    lookups: Vec<Vec<f64>>,
}

impl<'a> ExactRows<'a> {
    pub fn new(templates: &'a ConnectionTemplates) -> Self {
        Self {
            templates,
            sums: vec![0.0; templates.left_size],
            lookups: templates
                .parts
                .iter()
                .map(|part| vec![0.0; part.left_count + 1])
                .collect(),
        }
    }

    fn fill_row_sums(&mut self, right_left_id: usize) {
        self.sums.fill(0.0);
        for (part, lookup) in self.templates.parts.iter().zip(&mut self.lookups) {
            let right_key = usize::from(part.right_projection[right_left_id]);
            if right_key == part.right_count {
                continue;
            }
            let span = part.indptr[right_key] as usize..part.indptr[right_key + 1] as usize;
            if span.is_empty() {
                continue;
            }
            let left_index = &part.left_index[span.clone()];
            for (index, weight) in left_index.iter().zip(&part.weight[span]) {
                lookup[usize::from(*index)] = *weight;
            }
            for (sum, projected) in self.sums.iter_mut().zip(&part.left_projection) {
                *sum += lookup[usize::from(*projected)];
            }
            for index in left_index {
                lookup[usize::from(*index)] = 0.0;
            }
        }
    }

    pub fn render(&mut self, right_left_id: usize, out: &mut [i16]) {
        self.fill_row_sums(right_left_id);
        for (cost, sum) in out.iter_mut().zip(&self.sums) {
            *cost = to_cost(*sum, self.templates.cost_factor);
        }
    }
}
