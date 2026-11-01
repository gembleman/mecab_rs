use std::thread;

use crate::threads::default_threads;

const PARALLEL_MIN_WORK: usize = 1 << 14;

pub(super) fn lane_count(work: usize) -> usize {
    if work < PARALLEL_MIN_WORK {
        1
    } else {
        default_threads().max(1)
    }
}

pub(super) fn for_each_lane<T: Send>(
    items: &mut [T],
    work: usize,
    visit: impl Fn(usize, &mut T) + Sync,
) {
    let lanes = lane_count(work).min(items.len());
    if lanes <= 1 {
        for (index, item) in items.iter_mut().enumerate() {
            visit(index, item);
        }
        return;
    }
    let mut assigned: Vec<Vec<(usize, &mut T)>> = (0..lanes).map(|_| Vec::new()).collect();
    for (index, item) in items.iter_mut().enumerate() {
        assigned[index % lanes].push((index, item));
    }
    let visit = &visit;
    thread::scope(|scope| {
        for lane in assigned {
            scope.spawn(move || {
                for (index, item) in lane {
                    visit(index, item);
                }
            });
        }
    });
}
