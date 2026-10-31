use std::marker::PhantomData;
use std::mem::size_of;
use std::ops::Range;

use crate::mapped_bytes::MappedBytes;

pub(crate) trait SectionSource {
    fn bytes(&self, start: usize, end: usize) -> &[u8];
    fn all_bytes(&self) -> &[u8];
}

impl SectionSource for memmap2::Mmap {
    #[inline]
    fn bytes(&self, start: usize, end: usize) -> &[u8] {
        &self[start..end]
    }

    #[inline]
    fn all_bytes(&self) -> &[u8] {
        self
    }
}

impl SectionSource for MappedBytes {
    #[inline]
    fn bytes(&self, start: usize, end: usize) -> &[u8] {
        self.slice(start..end)
            .expect("a section is viewed in the mapped range it was made from")
    }

    #[inline]
    fn all_bytes(&self) -> &[u8] {
        self
    }
}

#[expect(
    clippy::missing_safety_doc,
    reason = "marker trait whose contract is plain old data layout"
)]
pub(crate) unsafe trait Plain: Copy {}

unsafe impl Plain for i16 {}
unsafe impl Plain for i32 {}
unsafe impl Plain for u8 {}
unsafe impl Plain for u16 {}
unsafe impl Plain for f64 {}
unsafe impl Plain for u32 {}
unsafe impl Plain for [u32; 2] {}

pub(crate) struct Section<T> {
    start: usize,
    end: usize,
    element: PhantomData<T>,
}

impl<T: Plain> Section<T> {
    pub(crate) fn new(file: &[u8], bytes: Range<usize>) -> Option<Self> {
        cast::<T>(file.get(bytes.clone())?)?;
        Some(Self {
            start: bytes.start,
            end: bytes.end,
            element: PhantomData,
        })
    }

    pub(crate) fn view<'f>(&self, file: &'f [u8]) -> &'f [T] {
        cast(&file[self.start..self.end]).expect("a section is viewed in the file it was made from")
    }

    #[inline]
    pub(crate) unsafe fn view_in<'f>(&self, file: &'f [u8]) -> &'f [T] {
        debug_assert!(self.end <= file.len());
        let pointer = unsafe { file.as_ptr().add(self.start) }.cast::<T>();
        debug_assert!(pointer.is_aligned());
        unsafe { std::slice::from_raw_parts(pointer, (self.end - self.start) / size_of::<T>()) }
    }

    #[inline]
    pub(crate) fn view_source<'f, S: SectionSource>(&self, file: &'f S) -> &'f [T] {
        cast(file.bytes(self.start, self.end)).expect("a section is viewed in its source map")
    }
}

fn cast<T: Plain>(bytes: &[u8]) -> Option<&[T]> {
    let pointer = bytes.as_ptr().cast::<T>();
    let aligned = pointer.is_aligned() && bytes.len().is_multiple_of(size_of::<T>());
    aligned.then(|| unsafe { std::slice::from_raw_parts(pointer, bytes.len() / size_of::<T>()) })
}
