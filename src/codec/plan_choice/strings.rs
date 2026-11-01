use super::{PlanBuilder, SURFACE_COL, StrCost, StrSources, index_of_min};
use crate::codec::model::{Kind, Step};

const ALIGN_MIN_KANA_SHARE: f64 = 0.3;
const STR_SOURCE_LIMIT: usize = 199;
const RECENT_SETTINGS: [u8; 4] = [0, 1, 2, 4];

fn string_sources(sources: StrSources, str_cands: &[usize], col: usize) -> Vec<usize> {
    let mut v = vec![SURFACE_COL];
    if let StrSources::SurfaceAndOthers = sources {
        v.extend(
            str_cands
                .iter()
                .copied()
                .filter(|&x| x != col)
                .take(STR_SOURCE_LIMIT),
        );
    }
    v
}

impl PlanBuilder<'_> {
    pub(super) fn string_pass(&self, str_cands: &[usize], sources: StrSources) -> Vec<StrCost> {
        let mut probe = self.probe();
        let mut meta = Vec::new();
        let mut srcs = Vec::new();
        for &c in str_cands {
            for align in [false, true] {
                if align && self.info[c].kana_share <= ALIGN_MIN_KANA_SHARE {
                    continue;
                }
                srcs.push((probe.steps.len(), string_sources(sources, str_cands, c)));
                probe.steps.push(Step {
                    col: c,
                    kind: Kind::Str { align, recent: 0 },
                });
                meta.push((c, align));
            }
        }
        let active: Vec<usize> = (0..probe.steps.len()).collect();
        let bits = self.trial(&probe, &active, &srcs, false);
        meta.into_iter()
            .enumerate()
            .map(|(i, (col, align))| StrCost {
                col,
                align,
                bits: bits[i],
            })
            .collect()
    }

    pub(super) fn align_column(str_cands: &[usize], first: &[StrCost]) -> Option<usize> {
        let mut align_col = None;
        let mut best_gain = 0.0;
        for &c in str_cands {
            let cost_of = |a: bool| {
                first
                    .iter()
                    .find(|f| f.col == c && f.align == a)
                    .map(|f| f.bits)
            };
            if let (Some(t), Some(f)) = (cost_of(true), cost_of(false))
                && f - t > best_gain
            {
                best_gain = f - t;
                align_col = Some(c);
            }
        }
        align_col
    }

    pub(super) fn tune_recent(&self, str_steps: &mut [Step]) {
        let mut probe = self.probe();
        let mut srcs = Vec::new();
        let mut srcs_so_far = vec![SURFACE_COL];
        for st in str_steps.iter() {
            for &r in &RECENT_SETTINGS {
                let mut kind = st.kind.clone();
                if let Kind::Str { recent, .. } = &mut kind {
                    *recent = r;
                }
                srcs.push((probe.steps.len(), srcs_so_far.clone()));
                probe.steps.push(Step { col: st.col, kind });
            }
            srcs_so_far.push(st.col);
        }
        let active: Vec<usize> = (0..probe.steps.len()).collect();
        let bits = self.trial(&probe, &active, &srcs, false);
        for (i, st) in str_steps.iter_mut().enumerate() {
            let per_step = &bits[i * RECENT_SETTINGS.len()..(i + 1) * RECENT_SETTINGS.len()];
            let best = index_of_min(per_step);
            if let Kind::Str { recent, .. } = &mut st.kind {
                *recent = RECENT_SETTINGS[best];
            }
        }
    }
}
