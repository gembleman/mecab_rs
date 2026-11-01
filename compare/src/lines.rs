pub struct Lines<'a> {
    text: &'a str,
    ends: Vec<u32>,
    pub bytes: usize,
}

impl<'a> Lines<'a> {
    pub fn new(text: &'a str) -> Self {
        assert!(u32::try_from(text.len()).is_ok());
        let count = text.bytes().filter(|&byte| byte == b'\n').count() + 1;
        let mut ends = Vec::with_capacity(count);
        let base = text.as_ptr() as usize;
        ends.extend(
            text.lines()
                .map(|line| (line.as_ptr() as usize - base + line.len()) as u32),
        );
        let mut lines = Self {
            text,
            ends,
            bytes: 0,
        };
        lines.bytes = lines.iter().map(str::len).sum();
        lines
    }

    pub fn len(&self) -> usize {
        self.ends.len()
    }

    pub fn truncate(&mut self, count: usize) {
        self.ends.truncate(count);
        self.bytes = self.iter().map(str::len).sum();
    }

    pub fn iter(&self) -> impl Iterator<Item = &'a str> + '_ {
        let text = self.text;
        let mut start = 0usize;
        self.ends.iter().map(move |&end| {
            let end = end as usize;
            debug_assert!(text.is_char_boundary(start) && text.is_char_boundary(end));
            let line = unsafe { text.get_unchecked(start..end) };
            let terminator = if text.as_bytes().get(end) == Some(&b'\r') {
                2
            } else {
                1
            };
            start = end + terminator;
            line
        })
    }
}
