use std::fmt;

use super::{SLACK, chars};

const COPY_BLOCK: usize = 16;

#[inline(always)]
unsafe fn copy_blocks(source: *const u8, target: *mut u8, length: usize) {
    let mut done = 0;
    loop {
        unsafe {
            std::ptr::copy_nonoverlapping(source.add(done), target.add(done), COPY_BLOCK);
        }
        done += COPY_BLOCK;
        if done >= length {
            break;
        }
    }
}

pub(crate) trait Sink {
    fn put(&mut self, text: &str) -> fmt::Result;

    unsafe fn put_region(&mut self, region: &[u8], start: usize, length: usize) -> fmt::Result;

    unsafe fn put_coded(&mut self, table: &[u8], codes: &[u8]) -> fmt::Result;
}

impl Sink for String {
    #[inline(always)]
    unsafe fn put_coded(&mut self, table: &[u8], codes: &[u8]) -> fmt::Result {
        chars::for_each_char(table, codes, |text| self.put(text))
    }

    #[inline(always)]
    fn put(&mut self, text: &str) -> fmt::Result {
        self.push_str(text);
        Ok(())
    }

    #[inline(always)]
    unsafe fn put_region(&mut self, region: &[u8], start: usize, length: usize) -> fmt::Result {
        let window = &region[start..start + length + SLACK];
        unsafe {
            let vector = self.as_mut_vec();
            vector.reserve(length + SLACK);
            let end = vector.len();
            let target = vector.as_mut_ptr().add(end);
            copy_blocks(window.as_ptr(), target, length);
            vector.set_len(end + length);
        }
        Ok(())
    }
}

pub(super) struct Cursor<'a> {
    vector: &'a mut Vec<u8>,
    ptr: *mut u8,
    len: usize,
    cap: usize,
}

#[cold]
#[inline(never)]
fn grow(vector: &mut Vec<u8>, len: usize, extra: usize) -> (*mut u8, usize) {
    unsafe { vector.set_len(len) };
    vector.reserve(extra);
    (vector.as_mut_ptr(), vector.capacity())
}

impl<'a> Cursor<'a> {
    #[inline(always)]
    pub(super) fn new(string: &'a mut String) -> Self {
        let vector = unsafe { string.as_mut_vec() };
        Self {
            ptr: vector.as_mut_ptr(),
            len: vector.len(),
            cap: vector.capacity(),
            vector,
        }
    }

    #[inline(always)]
    fn reserve(&mut self, extra: usize) {
        if self.cap - self.len < extra {
            (self.ptr, self.cap) = grow(self.vector, self.len, extra);
        }
    }

    #[inline(always)]
    pub(super) fn finish(self) {
        unsafe { self.vector.set_len(self.len) };
    }
}

impl Sink for Cursor<'_> {
    #[inline(always)]
    unsafe fn put_coded(&mut self, table: &[u8], codes: &[u8]) -> fmt::Result {
        self.reserve(codes.len() * chars::MAX_EXPANSION + chars::ENTRY_BYTES);
        self.len += unsafe { chars::decode_unchecked(table, codes, self.ptr.add(self.len)) };
        Ok(())
    }

    #[inline(always)]
    fn put(&mut self, text: &str) -> fmt::Result {
        self.reserve(text.len());
        unsafe {
            std::ptr::copy_nonoverlapping(text.as_ptr(), self.ptr.add(self.len), text.len());
        }
        self.len += text.len();
        Ok(())
    }

    #[inline(always)]
    unsafe fn put_region(&mut self, region: &[u8], start: usize, length: usize) -> fmt::Result {
        debug_assert!(start + length + SLACK <= region.len());
        if length == 1 {
            self.reserve(1);
            unsafe { *self.ptr.add(self.len) = *region.get_unchecked(start) };
            self.len += 1;
            return Ok(());
        }
        let window = unsafe { region.get_unchecked(start..start + length + SLACK) };
        self.reserve(length + SLACK);
        unsafe {
            copy_blocks(window.as_ptr(), self.ptr.add(self.len), length);
        }
        self.len += length;
        Ok(())
    }
}

impl Sink for fmt::Formatter<'_> {
    unsafe fn put_coded(&mut self, table: &[u8], codes: &[u8]) -> fmt::Result {
        chars::for_each_char(table, codes, |text| self.write_str(text))
    }

    fn put(&mut self, text: &str) -> fmt::Result {
        self.write_str(text)
    }

    unsafe fn put_region(&mut self, region: &[u8], start: usize, length: usize) -> fmt::Result {
        self.write_str(unsafe { text_of(&region[start..start + length]) })
    }
}

#[inline(always)]
unsafe fn text_of(bytes: &[u8]) -> &str {
    unsafe { str::from_utf8_unchecked(bytes) }
}
