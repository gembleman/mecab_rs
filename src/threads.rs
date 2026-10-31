use std::thread;

const MAX_HOT_THREADS: usize = 8;

pub fn default_threads() -> usize {
    thread::available_parallelism().map_or(1, |count| count.get().min(MAX_HOT_THREADS))
}
