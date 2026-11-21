//! Thread-local bump arenas and append-only partition lists built from
//! fixed-size blocks, so partitioning never copies or reallocates.
//!
//! A block is `[next block pointer, words used, data...]`. Records are
//! written with `reserve`, which never splits a record across blocks, so
//! every block holds whole records.

/// Words per arena chunk (16 MB).
const CHUNK_WORDS: usize = 1 << 21;
/// Words of header at the start of every block.
const HEADER_WORDS: usize = 2;

pub struct Arena {
    /// Chunks from the global allocator (where huge pages are not available).
    chunks: Vec<Vec<u64>>,
    cursor: *mut u64,
    left: usize,
}

/// Anonymous memory that asks for transparent huge pages. On the CI runners a
/// gigabyte of 4 KiB pages costs 100 to 150 ms more (page faults while
/// writing, then teardown at exit, both inside the timed interval) than the
/// same gigabyte in 2 MiB pages. Never unmapped: the process exits.
#[cfg(target_os = "linux")]
mod huge {
    const HUGE_PAGE: usize = 2 << 20;
    const PROT_READ_WRITE: i32 = 1 | 2;
    const MAP_PRIVATE_ANONYMOUS: i32 = 0x02 | 0x20;
    const MADV_HUGEPAGE: i32 = 14;

    extern "C" {
        fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, offset: i64) -> *mut u8;
        fn madvise(addr: *mut u8, len: usize, advice: i32) -> i32;
    }

    /// At least `bytes` of zeroed memory aligned to 2 MiB, or null.
    pub fn alloc(bytes: usize) -> *mut u8 {
        let size = (bytes + HUGE_PAGE - 1) & !(HUGE_PAGE - 1);
        // SAFETY: a fresh private anonymous mapping; the advice is only a hint.
        unsafe {
            let raw = mmap(std::ptr::null_mut(), size + HUGE_PAGE, PROT_READ_WRITE, MAP_PRIVATE_ANONYMOUS, -1, 0);
            if raw as isize == -1 {
                return std::ptr::null_mut();
            }
            let aligned = ((raw as usize + HUGE_PAGE - 1) & !(HUGE_PAGE - 1)) as *mut u8;
            madvise(aligned, size, MADV_HUGEPAGE);
            aligned
        }
    }
}

// SAFETY: an arena is owned and written by one thread at a time; the blocks it
// hands out are only read after that thread is done.
unsafe impl Send for Arena {}
unsafe impl Sync for Arena {}

impl Arena {
    pub fn new() -> Self {
        Self { chunks: Vec::new(), cursor: std::ptr::null_mut(), left: 0 }
    }

    #[cold]
    #[inline(never)]
    fn refill(&mut self, words: usize) {
        let words = CHUNK_WORDS.max(words);
        #[cfg(target_os = "linux")]
        {
            let chunk = huge::alloc(words * 8);
            if !chunk.is_null() {
                self.cursor = chunk as *mut u64;
                self.left = words;
                return;
            }
        }
        let mut chunk: Vec<u64> = Vec::with_capacity(words);
        self.cursor = chunk.as_mut_ptr();
        self.left = chunk.capacity();
        self.chunks.push(chunk);
    }

    /// Uninitialized space for `words` 64-bit words.
    #[inline]
    pub fn alloc(&mut self, words: usize) -> *mut u64 {
        if self.left < words {
            self.refill(words);
        }
        let block = self.cursor;
        // SAFETY: `words <= left`, so the result stays inside the chunk.
        self.cursor = unsafe { block.add(words) };
        self.left -= words;
        block
    }
}

impl Default for Arena {
    fn default() -> Self {
        Self::new()
    }
}

/// Appends records to one partition.
#[derive(Clone, Copy)]
pub struct PartWriter {
    cursor: *mut u64,
    end: *mut u64,
    /// The block being filled, null before the first record.
    block: *mut u64,
    head: *mut u64,
}

// SAFETY: a writer is used by one thread; it only holds pointers into that thread's arena.
unsafe impl Send for PartWriter {}
unsafe impl Sync for PartWriter {}

impl PartWriter {
    pub const EMPTY: PartWriter = PartWriter {
        cursor: std::ptr::null_mut(),
        end: std::ptr::null_mut(),
        block: std::ptr::null_mut(),
        head: std::ptr::null_mut(),
    };

    #[cold]
    #[inline(never)]
    fn grow(&mut self, arena: &mut Arena, block_words: usize) {
        let block = arena.alloc(block_words);
        // SAFETY: `block` has `block_words > HEADER_WORDS` writable words;
        // `self.block`, when set, is a live block this writer owns.
        unsafe {
            *block = 0;
            *block.add(1) = 0;
            if self.block.is_null() {
                self.head = block;
            } else {
                *self.block = block as u64;
                *self.block.add(1) = self.cursor.offset_from(self.block.add(HEADER_WORDS)) as u64;
            }
            self.block = block;
            self.cursor = block.add(HEADER_WORDS);
            self.end = block.add(block_words);
        }
    }

    /// Space for one record of `words` words (at most a few; far below a block).
    #[inline(always)]
    pub fn reserve(&mut self, arena: &mut Arena, block_words: usize, words: usize) -> *mut u64 {
        if (self.end as usize).wrapping_sub(self.cursor as usize) < words * 8 {
            self.grow(arena, block_words);
        }
        let at = self.cursor;
        // SAFETY: at least `words` words are free after the check above.
        self.cursor = unsafe { at.add(words) };
        at
    }

    /// Seals the list. The writer must not be used afterwards.
    pub fn finish(&mut self) -> PartList {
        if !self.block.is_null() {
            // SAFETY: `block` is the live block being filled.
            unsafe {
                *self.block.add(1) = self.cursor.offset_from(self.block.add(HEADER_WORDS)) as u64;
            }
        }
        PartList { head: self.head }
    }
}

/// A finished partition list, readable from any thread.
#[derive(Clone, Copy)]
pub struct PartList {
    head: *const u64,
}

// SAFETY: a finished list is immutable.
unsafe impl Send for PartList {}
unsafe impl Sync for PartList {}

impl PartList {
    /// The blocks of the list in write order; each holds whole records.
    #[inline]
    pub fn blocks(&self) -> Blocks<'_> {
        Blocks { block: self.head, _list: std::marker::PhantomData }
    }

    /// Total words in the list.
    pub fn words(&self) -> usize {
        self.blocks().map(<[u64]>::len).sum()
    }

    /// Appends every word of the list to `out`, in write order.
    pub fn copy_into(&self, out: &mut Vec<u64>) {
        for block in self.blocks() {
            out.extend_from_slice(block);
        }
    }
}

pub struct Blocks<'a> {
    block: *const u64,
    _list: std::marker::PhantomData<&'a PartList>,
}

impl<'a> Iterator for Blocks<'a> {
    type Item = &'a [u64];

    #[inline]
    fn next(&mut self) -> Option<&'a [u64]> {
        if self.block.is_null() {
            return None;
        }
        // SAFETY: a sealed block's header holds its link and its used length,
        // and the arena that owns it outlives every list reader.
        unsafe {
            let used = *self.block.add(1) as usize;
            let data = std::slice::from_raw_parts(self.block.add(HEADER_WORDS), used);
            self.block = *self.block as *const u64;
            Some(data)
        }
    }
}
