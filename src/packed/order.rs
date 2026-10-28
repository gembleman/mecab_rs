use super::model::{Model, Param};

const UNMAPPED: u32 = u32::MAX;

pub(super) struct Layout {
    pub(super) first_tokens: Vec<u32>,
    pub(super) token_params: Vec<u32>,
    pub(super) token_features: Vec<u32>,
    pub(super) token_keys: Vec<u32>,
    pub(super) params: Vec<Param>,
    pub(super) unknown_params: Vec<u32>,
}

struct Renumbering {
    new_of: Vec<u32>,
    params: Vec<Param>,
}

impl Renumbering {
    fn new(count: usize) -> Self {
        Self {
            new_of: vec![UNMAPPED; count],
            params: Vec::with_capacity(count),
        }
    }

    fn number(&mut self, old: u32, source: &[Param]) -> u32 {
        let slot = &mut self.new_of[old as usize];
        if *slot == UNMAPPED {
            *slot = self.params.len() as u32;
            self.params.push(source[old as usize]);
        }
        *slot
    }

    fn finish(mut self, source: &[Param]) -> Vec<Param> {
        for (old, &new) in self.new_of.iter().enumerate() {
            if new == UNMAPPED {
                self.params.push(source[old]);
            }
        }
        self.params
    }
}

fn key_starts(counts: &[u8]) -> Vec<u32> {
    let mut start = 0u32;
    counts
        .iter()
        .map(|&count| {
            let first = start;
            start += u32::from(count);
            first
        })
        .collect()
}

fn cheapest_cost(model: &Model, start: u32, count: u8) -> i16 {
    let tokens = start as usize..start as usize + usize::from(count);
    model.token_params[tokens]
        .iter()
        .map(|&param| model.params[param as usize].word_cost)
        .min()
        .expect("a key has at least one token")
}

fn key_order(model: &Model, starts: &[u32]) -> Vec<u32> {
    let costs: Vec<i16> = starts
        .iter()
        .zip(&model.counts)
        .map(|(&start, &count)| cheapest_cost(model, start, count))
        .collect();
    let mut order: Vec<u32> = (0..starts.len() as u32).collect();
    order.sort_by_key(|&key| costs[key as usize]);
    order
}

pub(super) fn by_word_cost(model: &Model) -> Layout {
    let starts = key_starts(&model.counts);
    let order = key_order(model, &starts);
    let mut renumbering = Renumbering::new(model.params.len());
    let mut layout = Layout {
        first_tokens: vec![0; starts.len()],
        token_params: Vec::with_capacity(model.token_params.len()),
        token_features: Vec::with_capacity(model.token_features.len()),
        token_keys: Vec::with_capacity(model.token_params.len()),
        params: Vec::new(),
        unknown_params: Vec::with_capacity(model.unknown_params.len()),
    };
    for key in order {
        let start = starts[key as usize] as usize;
        let tokens = start..start + usize::from(model.counts[key as usize]);
        layout.first_tokens[key as usize] = layout.token_params.len() as u32;
        for token in tokens {
            let param = renumbering.number(model.token_params[token], &model.params);
            layout.token_params.push(param);
            layout.token_features.push(model.token_features[token]);
            layout.token_keys.push(key);
        }
    }
    for &param in &model.unknown_params {
        let param = renumbering.number(param, &model.params);
        layout.unknown_params.push(param);
    }
    layout.params = renumbering.finish(&model.params);
    layout
}
