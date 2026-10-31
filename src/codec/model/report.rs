use super::align::AlignStats;
use super::lid::LidMode;
use super::spec::Step;
use super::state::{ModelState, StepState};

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct LidStats {
    pub mode: f64,
    pub seen: f64,
    pub range: f64,
    pub order: f64,
    pub direct: f64,
    pub low: f64,
}

impl LidStats {
    pub(super) fn add_id_bits(&mut self, mode: LidMode, bits: f64) {
        match mode {
            LidMode::Seen => self.seen += bits,
            LidMode::Range => self.range += bits,
            LidMode::OrderDelta => self.order += bits,
            LidMode::Direct | LidMode::LastNewDelta => self.direct += bits,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct LidModeCounts {
    pub seen: u64,
    pub range: u64,
    pub order_delta: u64,
    pub direct: u64,
    pub last_new_delta: u64,
}

impl LidModeCounts {
    pub(super) fn bump(&mut self, mode: LidMode) {
        match mode {
            LidMode::Seen => self.seen += 1,
            LidMode::Range => self.range += 1,
            LidMode::OrderDelta => self.order_delta += 1,
            LidMode::Direct => self.direct += 1,
            LidMode::LastNewDelta => self.last_new_delta += 1,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct LidReport {
    pub stats: LidStats,
    pub mode_counts: LidModeCounts,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct ColumnAlign {
    pub col: usize,
    pub stats: AlignStats,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct NumNovel {
    pub bits: f64,
    pub count: u64,
}

impl ModelState {
    pub(super) fn lid_stats(&self) -> Option<LidReport> {
        self.steps.iter().find_map(|s| match s {
            StepState::Lid(l) => Some(LidReport {
                stats: l.stats,
                mode_counts: l.mode_counts,
            }),
            _ => None,
        })
    }

    pub(super) fn align_stats(&self, plan_steps: &[Step]) -> Vec<ColumnAlign> {
        self.steps
            .iter()
            .zip(plan_steps)
            .filter_map(|(s, p)| match s {
                StepState::Str(st) if st.align_stats.hits + st.align_stats.misses > 0.0 => {
                    Some(ColumnAlign {
                        col: p.col,
                        stats: st.align_stats,
                    })
                }
                _ => None,
            })
            .collect()
    }

    pub(super) fn num_stats(&self) -> Vec<NumNovel> {
        self.steps
            .iter()
            .filter_map(|s| match s {
                StepState::Num(n) => Some(n.novel),
                _ => None,
            })
            .collect()
    }
}
