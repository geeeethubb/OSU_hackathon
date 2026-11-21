//! Thread-local bump arenas and append-only partition lists built from
//! fixed-size blocks, so partitioning never copies or reallocates.

/// Words per arena chunk (16 MB).
const CHUNK_WORDS: usize = 1 << 21;

pub struct Arena {
    chunks: Vec<Vec<u64>>,
    cursor: *mut u64,
    left: usize,
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
        let mut chunk: Vec<u64> = Vec::with_capacity(CHUNK_WORDS.max(words));
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

/// Appends words to one partition. Each block's first word links to the next block.
#[derive(Clone, Copy)]
pub struct PartWriter {
    cursor: *mut u64,
    end: *mut u64,
    head: *mut u64,
    tail: *mut u64,
    words: usize,
}

// SAFETY: a writer is used by one thread; it only holds pointers into that thread's arena.
unsafe impl Send for PartWriter {}
unsafe impl Sync for PartWriter {}

impl PartWriter {
    pub const EMPTY: PartWriter = PartWriter {
        cursor: std::ptr::null_mut(),
        end: std::ptr::null_mut(),
        head: std::ptr::null_mut(),
        tail: std::ptr::null_mut(),
        words: 0,
    };

    #[cold]
    #[inline(never)]
    fn grow(&mut self, arena: &mut Arena, block_words: usize) {
        let block = arena.alloc(block_words);
        // SAFETY: `block` has `block_words >= 2` writable words; `tail` is a live block.
        unsafe {
            *block = 0;
            if self.head.is_null() {
                self.head = block;
            } else {
                *self.tail = block as u64;
            }
            self.tail = block;
            self.cursor = block.add(1);
            self.end = block.add(block_words);
        }
    }

    #[inline(always)]
    pub fn push(&mut self, arena: &mut Arena, block_words: usize, word: u64) {
        if self.cursor == self.end {
            self.grow(arena, block_words);
        }
        // SAFETY: `cursor < end` after the check above.
        unsafe {
            *self.cursor = word;
            self.cursor = self.cursor.add(1);
        }
        self.words += 1;
    }

    pub fn list(&self, block_words: usize) -> PartList {
        PartList { head: self.head, words: self.words, block_words }
    }
}

/// A finished partition list, readable from any thread.
#[derive(Clone, Copy)]
pub struct PartList {
    head: *const u64,
    words: usize,
    block_words: usize,
}

// SAFETY: a finished list is immutable.
unsafe impl Send for PartList {}
unsafe impl Sync for PartList {}

impl PartList {
    pub fn words(&self) -> usize {
        self.words
    }

    /// Appends every word of the list to `out`, in write order.
    pub fn copy_into(&self, out: &mut Vec<u64>) {
        let per_block = self.block_words - 1;
        let mut block = self.head;
        let mut left = self.words;
        while left > 0 {
            let count = left.min(per_block);
            // SAFETY: every block holds `per_block` data words after its link
            // word, and the last one at least `left` initialized words.
            unsafe {
                out.extend_from_slice(std::slice::from_raw_parts(block.add(1), count));
                block = *block as *const u64;
            }
            left -= count;
        }
    }
}
