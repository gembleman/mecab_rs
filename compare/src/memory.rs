#[derive(Clone, Copy, Default)]
pub struct Mem {
    ws: usize,
    peak_ws: usize,
    private: usize,
    peak_private: usize,
}

#[cfg(windows)]
pub fn memory() -> Mem {
    #[repr(C)]
    #[derive(Default)]
    struct Counters {
        cb: u32,
        page_faults: u32,
        peak_ws: usize,
        ws: usize,
        quota: [usize; 4],
        pagefile: usize,
        peak_pagefile: usize,
    }
    unsafe extern "system" {
        fn GetCurrentProcess() -> isize;
        fn K32GetProcessMemoryInfo(process: isize, counters: *mut Counters, cb: u32) -> i32;
    }
    let mut c = Counters {
        cb: size_of::<Counters>() as u32,
        ..Default::default()
    };
    let ok = unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut c, c.cb) };
    assert!(ok != 0, "K32GetProcessMemoryInfo failed");
    Mem {
        ws: c.ws,
        peak_ws: c.peak_ws,
        private: c.pagefile,
        peak_private: c.peak_pagefile,
    }
}

#[cfg(not(windows))]
pub fn memory() -> Mem {
    Mem::default()
}

pub fn mem_json(base: Mem, loaded: Mem, end: Mem) -> String {
    let mb = |b: usize| b as f64 / (1024.0 * 1024.0);
    format!(
        "{{\"base_ws_mb\":{:.2},\"base_private_mb\":{:.2},\"loaded_ws_mb\":{:.2},\"loaded_private_mb\":{:.2},\"peak_ws_mb\":{:.2},\"peak_private_mb\":{:.2}}}",
        mb(base.ws),
        mb(base.private),
        mb(loaded.ws),
        mb(loaded.private),
        mb(end.peak_ws),
        mb(end.peak_private)
    )
}
