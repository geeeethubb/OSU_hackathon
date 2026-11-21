//! Minimal Parquet writers, with no dependencies. Layout and Thrift compact
//! encoding follow the Parquet format specification. Every page is a data
//! page (v1) or a dictionary page.
//!
//! * [`write_file`]: one row group and one PLAIN page per column, from columns
//!   already in memory. The reference pipeline uses it.
//! * [`GroupWriter`]: the optimized engine's writer. It encodes one small row
//!   group at a time into a reused buffer and writes it with one call, so a
//!   file of any size takes a few hundred kilobytes of memory, and it uses
//!   dictionary pages where they are cheaper to produce than PLAIN ones.
//!   Chosen columns can be GZIP (DEFLATE written here, Huffman codes only).

use std::fs::File;
use std::io::{self, Write};
use std::path::Path;

/// Physical types (parquet.thrift `Type`).
const TYPE_INT32: i32 = 1;
const TYPE_INT64: i32 = 2;
const TYPE_BYTE_ARRAY: i32 = 6;
/// `ConvertedType`.
const CONVERTED_UTF8: i32 = 0;
const CONVERTED_DATE: i32 = 6;
/// `FieldRepetitionType`.
const REQUIRED: i32 = 0;
const OPTIONAL: i32 = 1;
/// `Encoding`.
const ENCODING_PLAIN: i32 = 0;
const ENCODING_RLE: i32 = 3;
const ENCODING_RLE_DICTIONARY: i32 = 8;
/// `PageType`.
const PAGE_DATA: i32 = 0;
const PAGE_DICTIONARY: i32 = 2;
/// `CompressionCodec`.
const CODEC_UNCOMPRESSED: i32 = 0;
const CODEC_GZIP: i32 = 2;

/// Thrift compact protocol wire types.
const T_I32: u8 = 5;
const T_I64: u8 = 6;
const T_BINARY: u8 = 8;
const T_LIST: u8 = 9;
const T_STRUCT: u8 = 12;

/// A column's values, already in memory.
pub enum Column {
    /// Required UTF-8 strings, PLAIN-encoded (`u32` length then bytes) as they are pushed.
    Str(StrColumn),
    /// Nullable UTF-8 strings.
    OptStr(OptStrColumn),
    /// Required 64-bit integers.
    I64(Vec<i64>),
    /// Required DATE: days since 1970-01-01.
    Date(Vec<i32>),
}

#[derive(Default)]
pub struct StrColumn {
    pub plain: Vec<u8>,
    pub count: usize,
}

impl StrColumn {
    pub fn push(&mut self, value: &[u8]) {
        self.plain.extend_from_slice(&(value.len() as u32).to_le_bytes());
        self.plain.extend_from_slice(value);
        self.count += 1;
    }
}

#[derive(Default)]
pub struct OptStrColumn {
    /// PLAIN-encoded non-null values only.
    pub plain: Vec<u8>,
    /// One entry per row: `true` when the value is present.
    pub present: Vec<bool>,
}

impl OptStrColumn {
    pub fn push(&mut self, value: Option<&[u8]>) {
        match value {
            Some(value) => {
                self.plain.extend_from_slice(&(value.len() as u32).to_le_bytes());
                self.plain.extend_from_slice(value);
                self.present.push(true);
            }
            None => self.present.push(false),
        }
    }
}

fn put_varint(out: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        out.push((value as u8) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

fn zigzag(value: i64) -> u64 {
    ((value << 1) ^ (value >> 63)) as u64
}

/// Thrift compact protocol writer for the handful of structs Parquet needs.
struct Thrift {
    out: Vec<u8>,
    last_field: i16,
    /// `last_field` of the enclosing structs (they nest four deep at most).
    stack: [i16; 8],
    depth: usize,
}

impl Thrift {
    fn new() -> Self {
        Self { out: Vec::with_capacity(32), last_field: 0, stack: [0; 8], depth: 0 }
    }

    fn field(&mut self, id: i16, wire_type: u8) {
        let delta = id - self.last_field;
        if (1..=15).contains(&delta) {
            self.out.push(((delta as u8) << 4) | wire_type);
        } else {
            self.out.push(wire_type);
            put_varint(&mut self.out, zigzag(i64::from(id)));
        }
        self.last_field = id;
    }

    fn i32(&mut self, id: i16, value: i32) {
        self.field(id, T_I32);
        put_varint(&mut self.out, zigzag(i64::from(value)));
    }

    fn i64(&mut self, id: i16, value: i64) {
        self.field(id, T_I64);
        put_varint(&mut self.out, zigzag(value));
    }

    fn string(&mut self, id: i16, value: &[u8]) {
        self.field(id, T_BINARY);
        put_varint(&mut self.out, value.len() as u64);
        self.out.extend_from_slice(value);
    }

    fn list(&mut self, id: i16, element_type: u8, length: usize) {
        self.field(id, T_LIST);
        if length < 15 {
            self.out.push(((length as u8) << 4) | element_type);
        } else {
            self.out.push(0xF0 | element_type);
            put_varint(&mut self.out, length as u64);
        }
    }

    /// A struct-valued field.
    fn begin_struct(&mut self, id: i16) {
        self.field(id, T_STRUCT);
        self.begin_element();
    }

    /// A struct that is a list element (no field header).
    fn begin_element(&mut self) {
        self.stack[self.depth] = self.last_field;
        self.depth += 1;
        self.last_field = 0;
    }

    fn end_struct(&mut self) {
        self.out.push(0);
        self.depth -= 1;
        self.last_field = self.stack[self.depth];
    }

    fn element_i32(&mut self, value: i32) {
        put_varint(&mut self.out, zigzag(i64::from(value)));
    }

    fn element_string(&mut self, value: &[u8]) {
        put_varint(&mut self.out, value.len() as u64);
        self.out.extend_from_slice(value);
    }

    /// Ends the top-level struct.
    fn finish(mut self) -> Vec<u8> {
        self.out.push(0);
        self.out
    }
}

/// Definition levels for a nullable flat column: the RLE/bit-packed hybrid at
/// bit width 1, written as bit-packed groups of eight, with the 4-byte length
/// prefix data page v1 requires.
fn definition_levels(present: &[bool]) -> Vec<u8> {
    let groups = present.len().div_ceil(8);
    let mut body = Vec::with_capacity(groups + 10);
    put_varint(&mut body, ((groups as u64) << 1) | 1);
    for chunk in present.chunks(8) {
        let mut byte = 0u8;
        for (bit, &is_present) in chunk.iter().enumerate() {
            byte |= u8::from(is_present) << bit;
        }
        body.push(byte);
    }
    let mut out = Vec::with_capacity(body.len() + 4);
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(&body);
    out
}

/// The header of a dictionary page (`encoding` of its values) or of a data
/// page v1 (`encoding` of its values; levels are RLE). `raw_size` is the size
/// of the page's body and `stored_size` what it takes in the file: the same
/// unless the chunk has a codec.
fn page_header(
    page_type: i32,
    num_values: usize,
    raw_size: usize,
    stored_size: usize,
    encoding: i32,
) -> io::Result<Vec<u8>> {
    let too_large = |_| io::Error::new(io::ErrorKind::InvalidData, "a Parquet page over 2 GiB");
    let raw_size = i32::try_from(raw_size).map_err(too_large)?;
    let stored_size = i32::try_from(stored_size).map_err(too_large)?;
    let num_values = i32::try_from(num_values).map_err(too_large)?;
    let mut header = Thrift::new();
    header.i32(1, page_type);
    header.i32(2, raw_size); // uncompressed_page_size
    header.i32(3, stored_size); // compressed_page_size
    if page_type == PAGE_DICTIONARY {
        header.begin_struct(7); // DictionaryPageHeader
        header.i32(1, num_values);
        header.i32(2, encoding);
    } else {
        header.begin_struct(5); // DataPageHeader
        header.i32(1, num_values);
        header.i32(2, encoding);
        header.i32(3, ENCODING_RLE);
        header.i32(4, ENCODING_RLE);
    }
    header.end_struct();
    Ok(header.finish())
}

struct ChunkInfo {
    physical_type: i32,
    /// The chunk's first byte: its dictionary page when it has one.
    offset: u64,
    /// Equal to `offset` when the chunk has no dictionary page.
    data_page_offset: u64,
    /// Bytes in the file, and what they would be without the codec.
    size: u64,
    raw_size: u64,
    gzip: bool,
    num_values: u64,
}

/// `(physical type, repetition, converted type, LogicalType union field)` of a leaf.
type Leaf = (i32, i32, Option<i32>, Option<i16>);

/// Starts the file metadata: version and schema (a root, then one leaf per column).
fn file_metadata<'a>(columns: impl ExactSizeIterator<Item = (&'a str, Leaf)>) -> Thrift {
    let mut meta = Thrift::new();
    meta.i32(1, 1); // version
    meta.list(2, T_STRUCT, columns.len() + 1); // schema
    meta.begin_element();
    meta.string(4, b"schema");
    meta.i32(5, columns.len() as i32);
    meta.end_struct();
    for (name, (physical_type, repetition, converted, logical_field)) in columns {
        meta.begin_element();
        meta.i32(1, physical_type);
        meta.i32(3, repetition);
        meta.string(4, name.as_bytes());
        if let Some(converted) = converted {
            meta.i32(6, converted);
        }
        if let Some(logical_field) = logical_field {
            // LogicalType union: field 1 = STRING, field 6 = DATE; both are empty structs.
            meta.begin_struct(10);
            meta.begin_struct(logical_field);
            meta.end_struct();
            meta.end_struct();
        }
        meta.end_struct();
    }
    meta
}

/// One row group: the chunks of all its columns, in schema order.
fn row_group<'a>(meta: &mut Thrift, names: impl Iterator<Item = &'a str>, chunks: &[ChunkInfo], num_rows: usize) {
    meta.begin_element();
    meta.list(1, T_STRUCT, chunks.len());
    for (name, chunk) in names.zip(chunks) {
        let dictionary = chunk.data_page_offset != chunk.offset;
        meta.begin_element(); // ColumnChunk
        meta.i64(2, chunk.offset as i64); // file_offset
        meta.begin_struct(3); // ColumnMetaData
        meta.i32(1, chunk.physical_type);
        meta.list(2, T_I32, if dictionary { 3 } else { 2 });
        meta.element_i32(ENCODING_PLAIN);
        meta.element_i32(ENCODING_RLE);
        if dictionary {
            meta.element_i32(ENCODING_RLE_DICTIONARY);
        }
        meta.list(3, T_BINARY, 1);
        meta.element_string(name.as_bytes());
        meta.i32(4, if chunk.gzip { CODEC_GZIP } else { CODEC_UNCOMPRESSED });
        meta.i64(5, chunk.num_values as i64);
        meta.i64(6, chunk.raw_size as i64); // total_uncompressed_size
        meta.i64(7, chunk.size as i64); // total_compressed_size
        meta.i64(9, chunk.data_page_offset as i64);
        if dictionary {
            meta.i64(11, chunk.offset as i64); // dictionary_page_offset
        }
        meta.end_struct();
        meta.end_struct();
    }
    let total: u64 = chunks.iter().map(|chunk| chunk.raw_size).sum();
    meta.i64(2, total as i64); // total_byte_size: uncompressed
    meta.i64(3, num_rows as i64);
    meta.end_struct();
}

const CREATED_BY: &[u8] = b"shopping-sessions pipeline";

/// Writes one Parquet file holding `num_rows` rows of the given columns.
pub fn write_file(path: &Path, columns: &[(&str, &Column)], num_rows: usize) -> io::Result<()> {
    let mut file = File::create(path)?;
    let mut offset: u64 = 4;
    file.write_all(b"PAR1")?;
    let mut chunks = Vec::with_capacity(columns.len());
    if num_rows > 0 {
        for (_, column) in columns {
            let mut levels = Vec::new();
            let fixed;
            let (physical_type, values): (i32, &[u8]) = match column {
                Column::Str(strings) => {
                    debug_assert_eq!(strings.count, num_rows);
                    (TYPE_BYTE_ARRAY, &strings.plain)
                }
                Column::OptStr(strings) => {
                    debug_assert_eq!(strings.present.len(), num_rows);
                    levels = definition_levels(&strings.present);
                    (TYPE_BYTE_ARRAY, &strings.plain)
                }
                Column::I64(numbers) => {
                    debug_assert_eq!(numbers.len(), num_rows);
                    fixed = numbers.iter().flat_map(|value| value.to_le_bytes()).collect::<Vec<u8>>();
                    (TYPE_INT64, &fixed)
                }
                Column::Date(days) => {
                    debug_assert_eq!(days.len(), num_rows);
                    fixed = days.iter().flat_map(|value| value.to_le_bytes()).collect::<Vec<u8>>();
                    (TYPE_INT32, &fixed)
                }
            };
            let page_size = levels.len() + values.len();
            let header = page_header(PAGE_DATA, num_rows, page_size, page_size, ENCODING_PLAIN)?;
            file.write_all(&header)?;
            file.write_all(&levels)?;
            file.write_all(values)?;
            let size = (header.len() + page_size) as u64;
            chunks.push(ChunkInfo {
                physical_type,
                offset,
                data_page_offset: offset,
                size,
                raw_size: size,
                gzip: false,
                num_values: num_rows as u64,
            });
            offset += size;
        }
    }

    let mut meta = file_metadata(columns.iter().map(|(name, column)| {
        let leaf = match column {
            Column::Str(_) => ColumnType::Str,
            Column::OptStr(_) => ColumnType::OptStr,
            Column::I64(_) => ColumnType::Int64,
            Column::Date(_) => ColumnType::Date,
        };
        (*name, leaf.leaf())
    }));
    meta.i64(3, num_rows as i64);
    if chunks.is_empty() {
        meta.list(4, T_STRUCT, 0);
    } else {
        meta.list(4, T_STRUCT, 1); // one row group
        row_group(&mut meta, columns.iter().map(|(name, _)| *name), &chunks, num_rows);
    }
    meta.string(6, CREATED_BY);
    let meta = meta.finish();
    file.write_all(&meta)?;
    file.write_all(&(meta.len() as u32).to_le_bytes())?;
    file.write_all(b"PAR1")?;
    Ok(())
}

// ---------------------------------------------------------------------------
// GZIP pages.
// ---------------------------------------------------------------------------

/// GZIP members (RFC 1952) whose DEFLATE stream (RFC 1951) uses Huffman codes
/// only, never a match: what a Parquet page of a GZIP column chunk holds.
/// Index bytes and ASCII IDs have no repeats worth finding, but their bytes
/// are far from uniform, so the entropy code alone gets most of what a full
/// DEFLATE would, at a fraction of the cost.
mod gzip {
    /// Symbols of the literal/length alphabet in use: the bytes and end-of-block.
    const SYMBOLS: usize = 257;
    const END_OF_BLOCK: usize = 256;
    /// Longest code. DEFLATE allows 15; 14 lets four codes and seven pending
    /// bits fit a 64-bit word.
    const LONGEST: u32 = 14;
    /// Bits of a dynamic block before its data: the block header, the lengths
    /// of the 19 code length codes (4 bits each for the lengths 0 to 15, none
    /// for the repeat codes), then one 4-bit code per literal/length code
    /// length and for each of two distance codes.
    const TREE_BITS: usize = 3 + 5 + 5 + 4 + 19 * 3 + (SYMBOLS + 2) * 4;
    /// Bytes a stored block may hold.
    const STORED: usize = 65_535;
    /// Bytes around the DEFLATE stream: the header, then CRC-32 and length.
    const FRAME: usize = 10 + 8;

    /// CRC-32 (IEEE 802.3, reflected) tables for eight bytes at a time.
    static TABLES: [[u32; 256]; 8] = tables();

    const fn tables() -> [[u32; 256]; 8] {
        let mut tables = [[0u32; 256]; 8];
        let mut byte = 0;
        while byte < 256 {
            let mut crc = byte as u32;
            let mut bit = 0;
            while bit < 8 {
                crc = if crc & 1 != 0 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
                bit += 1;
            }
            tables[0][byte] = crc;
            byte += 1;
        }
        let mut byte = 0;
        while byte < 256 {
            let mut crc = tables[0][byte];
            let mut table = 1;
            while table < 8 {
                crc = tables[0][(crc & 0xFF) as usize] ^ (crc >> 8);
                tables[table][byte] = crc;
                table += 1;
            }
            byte += 1;
        }
        tables
    }

    /// Advances a CRC-32 state (the checksum, inverted) over `data`.
    pub(super) fn crc32_tables(mut state: u32, data: &[u8]) -> u32 {
        let mut chunks = data.chunks_exact(8);
        for chunk in &mut chunks {
            let low = u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]) ^ state;
            let high = u32::from_le_bytes([chunk[4], chunk[5], chunk[6], chunk[7]]);
            state = TABLES[7][(low & 0xFF) as usize]
                ^ TABLES[6][((low >> 8) & 0xFF) as usize]
                ^ TABLES[5][((low >> 16) & 0xFF) as usize]
                ^ TABLES[4][(low >> 24) as usize]
                ^ TABLES[3][(high & 0xFF) as usize]
                ^ TABLES[2][((high >> 8) & 0xFF) as usize]
                ^ TABLES[1][((high >> 16) & 0xFF) as usize]
                ^ TABLES[0][(high >> 24) as usize];
        }
        for &byte in chunks.remainder() {
            state = TABLES[0][((state ^ u32::from(byte)) & 0xFF) as usize] ^ (state >> 8);
        }
        state
    }

    /// The same over a whole number of 16-byte blocks, at least four, by
    /// carry-less multiplication (Gopal et al., "Fast CRC Computation for
    /// Generic Polynomials Using PCLMULQDQ Instruction", with the constants
    /// of this polynomial as zlib uses them).
    ///
    /// # Safety
    /// The CPU must support PCLMULQDQ and SSE4.1.
    #[cfg(target_arch = "x86_64")]
    #[target_feature(enable = "pclmulqdq", enable = "sse4.1")]
    pub(super) unsafe fn crc32_clmul(state: u32, data: &[u8]) -> u32 {
        use std::arch::x86_64::*;
        assert!(data.len() >= 64 && data.len() % 16 == 0);
        let k1k2 = _mm_set_epi64x(0x01_c6e4_1596, 0x01_5444_2bd4);
        let k3k4 = _mm_set_epi64x(0x00_ccaa_009e, 0x01_7519_97d0);
        let k5 = _mm_set_epi64x(0, 0x01_63cd_6124);
        let poly = _mm_set_epi64x(0x01_f701_1641, 0x01_db71_0641);
        // SAFETY: every load is of 16 bytes inside `data` (see the assertion
        // and the loop conditions); unaligned loads have no other requirement.
        let load = |at: usize| unsafe { _mm_loadu_si128(data.as_ptr().add(at).cast()) };
        let fold = |x: __m128i, next: __m128i, k: __m128i| {
            _mm_xor_si128(_mm_xor_si128(next, _mm_clmulepi64_si128(x, k, 0x00)), _mm_clmulepi64_si128(x, k, 0x11))
        };
        let mut x1 = _mm_xor_si128(load(0), _mm_cvtsi32_si128(state as i32));
        let (mut x2, mut x3, mut x4) = (load(16), load(32), load(48));
        let mut at = 64;
        // Four independent 128-bit folds per 64 bytes.
        while at + 64 <= data.len() {
            x1 = fold(x1, load(at), k1k2);
            x2 = fold(x2, load(at + 16), k1k2);
            x3 = fold(x3, load(at + 32), k1k2);
            x4 = fold(x4, load(at + 48), k1k2);
            at += 64;
        }
        x1 = fold(x1, x2, k3k4);
        x1 = fold(x1, x3, k3k4);
        x1 = fold(x1, x4, k3k4);
        while at + 16 <= data.len() {
            x1 = fold(x1, load(at), k3k4);
            at += 16;
        }
        // 128 bits to 64, then Barrett reduction to 32.
        let mask = _mm_setr_epi32(!0, 0, !0, 0);
        let mut x2 = _mm_clmulepi64_si128(x1, k3k4, 0x10);
        x1 = _mm_xor_si128(_mm_srli_si128(x1, 8), x2);
        x2 = _mm_srli_si128(x1, 4);
        x1 = _mm_xor_si128(_mm_clmulepi64_si128(_mm_and_si128(x1, mask), k5, 0x00), x2);
        x2 = _mm_clmulepi64_si128(_mm_and_si128(x1, mask), poly, 0x10);
        x2 = _mm_clmulepi64_si128(_mm_and_si128(x2, mask), poly, 0x00);
        _mm_extract_epi32(_mm_xor_si128(x1, x2), 1) as u32
    }

    /// The CRC-32 of `data`.
    pub(super) fn crc32(data: &[u8]) -> u32 {
        let mut state = !0u32;
        let mut rest = data;
        #[cfg(target_arch = "x86_64")]
        if data.len() >= 64 && is_x86_feature_detected!("pclmulqdq") && is_x86_feature_detected!("sse4.1") {
            let (blocks, tail) = data.split_at(data.len() & !15);
            // SAFETY: the features were just detected.
            state = unsafe { crc32_clmul(state, blocks) };
            rest = tail;
        }
        !crc32_tables(state, rest)
    }

    /// Code lengths (0 for a symbol that does not occur, else 1 to `LONGEST`)
    /// of a Huffman code for these frequencies: the optimal code, or, when
    /// that has a longer code, the optimal code of flattened frequencies.
    fn code_lengths(frequencies: &[u32; SYMBOLS]) -> [u8; SYMBOLS] {
        let mut lengths = [0u8; SYMBOLS];
        let mut weights = *frequencies;
        loop {
            // Leaves in order of weight, then internal nodes in order of creation.
            let mut leaves = [(0u32, 0u16); SYMBOLS];
            let mut count = 0;
            for (symbol, &weight) in weights.iter().enumerate() {
                if weight > 0 {
                    leaves[count] = (weight, symbol as u16);
                    count += 1;
                }
            }
            let leaves = &mut leaves[..count];
            if count < 2 {
                // A code needs two symbols; a lone one gets one bit.
                for leaf in leaves.iter() {
                    lengths[usize::from(leaf.1)] = 1;
                }
                return lengths;
            }
            leaves.sort_unstable();
            let mut weight = [0u64; 2 * SYMBOLS];
            let mut parent = [0u16; 2 * SYMBOLS];
            for (node, leaf) in leaves.iter().enumerate() {
                weight[node] = u64::from(leaf.0);
            }
            // Both queues are sorted, so the two lightest nodes are at their fronts.
            let (mut leaf, mut internal) = (0, count);
            for next in count..2 * count - 1 {
                for _ in 0..2 {
                    let lightest = if leaf < count && (internal >= next || weight[leaf] <= weight[internal]) {
                        leaf += 1;
                        leaf - 1
                    } else {
                        internal += 1;
                        internal - 1
                    };
                    weight[next] += weight[lightest];
                    parent[lightest] = next as u16;
                }
            }
            // A node was created after its children: depths from the root down.
            let mut depth = [0u16; 2 * SYMBOLS];
            for node in (0..2 * count - 2).rev() {
                depth[node] = depth[usize::from(parent[node])] + 1;
            }
            if depth[..count].iter().all(|&depth| u32::from(depth) <= LONGEST) {
                for (node, leaf) in leaves.iter().enumerate() {
                    lengths[usize::from(leaf.1)] = depth[node] as u8;
                }
                return lengths;
            }
            for weight in weights.iter_mut() {
                *weight = (*weight + 1) / 2;
            }
        }
    }

    /// Appends bits, least significant first, to a byte buffer that has
    /// eight spare bytes behind everything written.
    struct Bits<'a> {
        out: &'a mut [u8],
        at: usize,
        pending: u64,
        count: u32,
    }

    impl Bits<'_> {
        /// Adds `count` bits (at most 32) without making room.
        #[inline]
        fn add(&mut self, bits: u32, count: u32) {
            self.pending |= u64::from(bits) << self.count;
            self.count += count;
        }

        /// Moves the whole pending bytes out; fewer than eight bits stay.
        #[inline]
        fn flush(&mut self) {
            self.out[self.at..self.at + 8].copy_from_slice(&self.pending.to_le_bytes());
            self.at += (self.count >> 3) as usize;
            self.pending >>= self.count & !7;
            self.count &= 7;
        }

        fn put(&mut self, bits: u32, count: u32) {
            self.add(bits, count);
            self.flush();
        }

        /// Pads to a whole byte and returns the position behind it.
        fn finish(mut self) -> usize {
            self.count = (self.count + 7) & !7;
            self.flush();
            self.at
        }
    }

    /// A 4-bit code length code: the length itself, most significant bit first.
    fn length_code(length: u8) -> u32 {
        u32::from(length.reverse_bits() >> 4)
    }

    /// The most bytes [`member`] writes for `raw` bytes, spare bytes included.
    pub(super) fn bound(raw: usize) -> usize {
        FRAME + raw + 5 * (raw / STORED + 1) + 8
    }

    /// Writes one GZIP member holding `raw` to the start of `out` (at least
    /// [`bound`] bytes) and returns its length.
    pub(super) fn member(raw: &[u8], out: &mut [u8]) -> usize {
        // ID, deflate, no flags, no time, no extra flags, unknown system.
        out[..10].copy_from_slice(&[0x1F, 0x8B, 8, 0, 0, 0, 0, 0, 0, 0xFF]);
        let mut at = 10;

        // Four counters, so that equal neighbours do not wait for one another.
        let mut counts = [[0u32; 256]; 4];
        let mut quads = raw.chunks_exact(4);
        for quad in &mut quads {
            for (count, &byte) in counts.iter_mut().zip(quad) {
                count[usize::from(byte)] += 1;
            }
        }
        for &byte in quads.remainder() {
            counts[0][usize::from(byte)] += 1;
        }
        let mut frequencies = [0u32; SYMBOLS];
        for (byte, frequency) in frequencies[..256].iter_mut().enumerate() {
            *frequency = counts[0][byte] + counts[1][byte] + counts[2][byte] + counts[3][byte];
        }
        frequencies[END_OF_BLOCK] = 1;
        let lengths = code_lengths(&frequencies);
        let coded: usize =
            TREE_BITS + frequencies.iter().zip(&lengths).map(|(&count, &length)| count as usize * usize::from(length)).sum::<usize>();
        let stored = raw.len() + 5 * raw.len().div_ceil(STORED).max(1);

        if coded.div_ceil(8) < stored {
            // Canonical codes (RFC 1951, 3.2.2), stored bit-reversed with the
            // length above bit 16: codes go out most significant bit first.
            let mut per_length = [0u32; 16];
            for &length in &lengths {
                per_length[usize::from(length)] += 1;
            }
            per_length[0] = 0;
            let mut next = [0u32; 16];
            for length in 1..16 {
                next[length] = (next[length - 1] + per_length[length - 1]) << 1;
            }
            let mut codes = [0u32; SYMBOLS];
            for (code, &length) in codes.iter_mut().zip(&lengths) {
                if length > 0 {
                    let straight = next[usize::from(length)];
                    next[usize::from(length)] += 1;
                    *code = (straight.reverse_bits() >> (32 - u32::from(length))) | (u32::from(length) << 16);
                }
            }
            let mut bits = Bits { out: &mut *out, at, pending: 0, count: 0 };
            // Final block, dynamic codes; 257 literal/length codes, 2 distance
            // codes, 19 code length code lengths.
            bits.put(1 | (2 << 1), 3);
            bits.put(0, 5);
            bits.put(1, 5);
            bits.put(15, 4);
            // In the order 16, 17, 18, 0, 8, 7, ...: no repeat codes, and
            // four bits for each of the lengths 0 to 15.
            for symbol in 0..19 {
                bits.put(if symbol < 3 { 0 } else { 4 }, 3);
            }
            for &length in &lengths {
                bits.put(length_code(length), 4);
            }
            // Two distance codes of one bit: none is used, but a complete
            // code is what every inflater accepts.
            bits.put(length_code(1), 4);
            bits.put(length_code(1), 4);
            let mut quads = raw.chunks_exact(4);
            for quad in &mut quads {
                for &byte in quad {
                    let code = codes[usize::from(byte)];
                    bits.add(code & 0xFFFF, code >> 16);
                }
                bits.flush();
            }
            for &byte in quads.remainder() {
                let code = codes[usize::from(byte)];
                bits.put(code & 0xFFFF, code >> 16);
            }
            let code = codes[END_OF_BLOCK];
            bits.put(code & 0xFFFF, code >> 16);
            at = bits.finish();
        } else {
            // Stored blocks: a header byte (1 on the last block), the length
            // and its complement, the bytes.
            let mut blocks = raw.chunks(STORED).peekable();
            if blocks.peek().is_none() {
                out[at..at + 5].copy_from_slice(&[1, 0, 0, 0xFF, 0xFF]);
                at += 5;
            }
            while let Some(block) = blocks.next() {
                out[at] = u8::from(blocks.peek().is_none());
                out[at + 1..at + 3].copy_from_slice(&(block.len() as u16).to_le_bytes());
                out[at + 3..at + 5].copy_from_slice(&(!(block.len() as u16)).to_le_bytes());
                out[at + 5..at + 5 + block.len()].copy_from_slice(block);
                at += 5 + block.len();
            }
        }
        out[at..at + 4].copy_from_slice(&crc32(raw).to_le_bytes());
        out[at + 4..at + 8].copy_from_slice(&(raw.len() as u32).to_le_bytes());
        at + 8
    }

    #[cfg(test)]
    pub(super) mod tests {
        use super::*;

        /// Reads bits, least significant first.
        struct Reader<'a> {
            data: &'a [u8],
            bit: usize,
        }

        impl Reader<'_> {
            fn bits(&mut self, count: usize) -> u32 {
                let mut value = 0;
                for at in 0..count {
                    value |= u32::from(self.data[self.bit / 8] >> (self.bit % 8) & 1) << at;
                    self.bit += 1;
                }
                value
            }

            /// One symbol of the canonical code with these lengths.
            fn symbol(&mut self, lengths: &[u8]) -> usize {
                let mut per_length = [0usize; 16];
                for &length in lengths {
                    per_length[usize::from(length)] += 1;
                }
                per_length[0] = 0;
                // The code must be complete, or have a single one-bit code.
                let space: usize = (1..16).map(|length| per_length[length] << (15 - length)).sum();
                assert!(space == 1 << 15 || (space == 1 << 14 && per_length[1] == 1), "incomplete code");
                let (mut code, mut first, mut index) = (0usize, 0usize, 0usize);
                for length in 1..16 {
                    code |= self.bits(1) as usize;
                    let count = per_length[length];
                    if code < first + count {
                        let rank = index + code - first;
                        return (0..lengths.len()).filter(|&symbol| usize::from(lengths[symbol]) == length).nth(rank - index).unwrap();
                    }
                    index += count;
                    first = (first + count) << 1;
                    code <<= 1;
                }
                panic!("no such code");
            }
        }

        /// A strict inflater for what [`member`] writes: stored blocks and
        /// dynamic blocks without matches. Checks the frame, CRC and length.
        pub(crate) fn inflate(member: &[u8]) -> Vec<u8> {
            assert_eq!(&member[..10], &[0x1F, 0x8B, 8, 0, 0, 0, 0, 0, 0, 0xFF]);
            let mut reader = Reader { data: &member[10..], bit: 0 };
            let mut raw = Vec::new();
            loop {
                let last = reader.bits(1) == 1;
                match reader.bits(2) {
                    0 => {
                        reader.bit = reader.bit.div_ceil(8) * 8;
                        let length = reader.bits(16) as usize;
                        assert_eq!(reader.bits(16) as usize, !length & 0xFFFF);
                        let start = reader.bit / 8;
                        raw.extend_from_slice(&reader.data[start..start + length]);
                        reader.bit += 8 * length;
                    }
                    2 => {
                        let literals = reader.bits(5) as usize + 257;
                        let distances = reader.bits(5) as usize + 1;
                        let count = reader.bits(4) as usize + 4;
                        let mut meta = [0u8; 19];
                        for &symbol in &[16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15][..count] {
                            meta[symbol] = reader.bits(3) as u8;
                        }
                        let mut lengths = Vec::new();
                        while lengths.len() < literals + distances {
                            match reader.symbol(&meta) {
                                length @ 0..=15 => lengths.push(length as u8),
                                _ => panic!("repeat codes are not written"),
                            }
                        }
                        let (literal_lengths, distance_lengths) = lengths.split_at(literals);
                        assert!(literal_lengths[256] > 0, "no end of block");
                        assert_eq!(distance_lengths, &[1, 1]);
                        loop {
                            match reader.symbol(literal_lengths) {
                                byte @ 0..=255 => raw.push(byte as u8),
                                256 => break,
                                _ => panic!("matches are not written"),
                            }
                        }
                    }
                    kind => panic!("block type {kind}"),
                }
                if last {
                    break;
                }
            }
            let end = 10 + reader.bit.div_ceil(8);
            assert_eq!(member.len(), end + 8, "bytes behind the member");
            assert_eq!(u32::from_le_bytes(member[end..end + 4].try_into().unwrap()), !crc32_tables(!0, &raw), "CRC-32");
            assert_eq!(u32::from_le_bytes(member[end + 4..].try_into().unwrap()), raw.len() as u32, "length");
            raw
        }

        fn noise(seed: u64, length: usize, shape: impl Fn(u64) -> u8) -> Vec<u8> {
            let mut state = seed | 1;
            (0..length)
                .map(|_| {
                    state ^= state << 13;
                    state ^= state >> 7;
                    state ^= state << 17;
                    shape(state)
                })
                .collect()
        }

        #[test]
        fn crc32_matches_the_standard_check_value() {
            assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
            assert_eq!(crc32(b""), 0);
            assert_eq!(!crc32_tables(!0, b"123456789"), 0xCBF4_3926);
        }

        #[test]
        fn crc32_by_multiplication_matches_the_tables() {
            for length in (0..700).chain([4_096, 65_537, 300_001]) {
                let data = noise(length as u64 + 99, length, |state| (state >> 24) as u8);
                assert_eq!(crc32(&data), !crc32_tables(!0, &data), "length {length}");
            }
            #[cfg(target_arch = "x86_64")]
            if is_x86_feature_detected!("pclmulqdq") && is_x86_feature_detected!("sse4.1") {
                for blocks in 4..40 {
                    let data = noise(blocks as u64, 16 * blocks, |state| (state >> 9) as u8);
                    for state in [!0u32, 0, 0x1234_5678] {
                        // SAFETY: the features were just detected.
                        assert_eq!(unsafe { crc32_clmul(state, &data) }, crc32_tables(state, &data), "{blocks} blocks");
                    }
                }
            }
        }

        #[test]
        fn code_lengths_are_complete_and_short() {
            let mut cases: Vec<[u32; SYMBOLS]> = Vec::new();
            let mut one = [0u32; SYMBOLS];
            one[END_OF_BLOCK] = 1;
            cases.push(one);
            let mut two = one;
            two[65] = 1_000_000;
            cases.push(two);
            cases.push([1; SYMBOLS]);
            // Fibonacci weights make the deepest tree: it must be flattened.
            let mut steep = one;
            let (mut a, mut b) = (1u32, 1u32);
            for symbol in 0..40 {
                steep[symbol] = a;
                (a, b) = (b, a + b);
            }
            cases.push(steep);
            let mut state = 88u64;
            for _ in 0..200 {
                let mut random = one;
                for frequency in random[..256].iter_mut() {
                    state = state.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
                    let draw = (state >> 33) as u32;
                    *frequency = if draw % 3 == 0 { 0 } else { 1 << (draw % 21) };
                }
                cases.push(random);
            }
            for frequencies in &cases {
                let lengths = code_lengths(frequencies);
                let used = frequencies.iter().filter(|&&frequency| frequency > 0).count();
                let mut space = 0u32;
                for (&frequency, &length) in frequencies.iter().zip(&lengths) {
                    assert_eq!(frequency > 0, length > 0);
                    assert!(u32::from(length) <= LONGEST);
                    if length > 0 {
                        space += 1 << (LONGEST - u32::from(length));
                    }
                }
                assert_eq!(space, if used == 1 { 1 << (LONGEST - 1) } else { 1 << LONGEST }, "Kraft sum");
            }
        }

        #[test]
        fn members_inflate_to_their_input() {
            let mut cases: Vec<Vec<u8>> = vec![
                Vec::new(),
                vec![7],
                b"u3fezza6p".to_vec(),
                vec![0; 100_000],
                noise(1, 70_000, |state| (state >> 24) as u8),
                noise(2, 200_000, |state| (state >> 24) as u8),
                noise(3, 131_072, |state| b"0123456789abcdefghijklmnopqrstuvwxyz"[(state >> 24) as usize % 36]),
                noise(4, 50_001, |state| if state % 100 < 77 { 1 } else { (state >> 20) as u8 % 23 }),
                noise(5, 16_384, |state| ((state >> 30) % 6 + (state >> 40) % 6) as u8),
            ];
            for length in 0..70 {
                cases.push(noise(100 + length, length as usize, |state| (state >> 24) as u8 % 5));
            }
            let mut steep = Vec::new();
            let (mut a, mut b) = (1usize, 1usize);
            for symbol in 0..24u8 {
                steep.extend(std::iter::repeat(symbol).take(a));
                (a, b) = (b, a + b);
            }
            cases.push(steep);
            for raw in &cases {
                let mut out = vec![0x55u8; bound(raw.len())];
                let length = member(raw, &mut out);
                assert!(length + 8 <= out.len());
                assert_eq!(&inflate(&out[..length]), raw, "{} bytes", raw.len());
            }
            // Skewed bytes shrink, uniform ones are stored.
            let skewed = noise(4, 50_001, |state| if state % 100 < 77 { 1 } else { (state >> 20) as u8 % 23 });
            let mut out = vec![0u8; bound(skewed.len())];
            assert!(member(&skewed, &mut out) < skewed.len() / 2);
            let uniform = noise(1, 70_000, |state| (state >> 24) as u8);
            let mut out = vec![0u8; bound(uniform.len())];
            assert_eq!(member(&uniform, &mut out), uniform.len() + 10 + FRAME);
        }
    }
}

// ---------------------------------------------------------------------------
// The streaming writer.
// ---------------------------------------------------------------------------

/// Most rows a row group of [`GroupWriter`] may hold. The writer's scratch
/// memory is a few bytes per row of this, whatever the size of the file.
pub const MAX_GROUP_ROWS: usize = 1 << 14;
/// Pending bytes at which [`GroupWriter::begin`] writes before it starts a row group.
const FLUSH_BYTES: usize = 1 << 16;
/// Bytes of the slot [`GroupWriter::runs`] hands its callback.
pub const SHORT_TEXT: usize = 16;
/// Integers below this find their dictionary index in a direct map.
const DIRECT: usize = 1 << 16;
/// Distinct integers outside the direct map a chunk may hold before it is written PLAIN.
const MOST_FAR: usize = 64;
/// Leading zeros that make [`GroupWriter::sparse_ints`] write them as one run.
const ZERO_RUN: usize = 16;
/// The per-row scratch arrays have one entry per row position. The hot loops
/// index them through `& ROW_MASK`, which changes no index below
/// [`MAX_GROUP_ROWS`] and lets the compiler leave out the bounds check.
const ROW_MASK: usize = MAX_GROUP_ROWS - 1;
/// Bytes of the RLE runs of one `runs` chunk at most (three per run, a run
/// per row and one more per 63 rows), rounded up; the arrays have 8 more for
/// the 4-byte store that writes a run.
const RUN_BYTES: usize = 1 << 16;
const RUN_MASK: usize = RUN_BYTES - 1;
const _: () = assert!(3 * (MAX_GROUP_ROWS + MAX_GROUP_ROWS / 63) < RUN_BYTES);

/// What a column of [`GroupWriter`] holds.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ColumnType {
    /// Required UTF-8 string.
    Str,
    /// Nullable UTF-8 string.
    OptStr,
    /// Required integer stored as INT32.
    Int32,
    /// Required integer stored as INT64.
    Int64,
    /// Required DATE: days since 1970-01-01.
    Date,
}

impl ColumnType {
    fn leaf(self) -> Leaf {
        match self {
            ColumnType::Str => (TYPE_BYTE_ARRAY, REQUIRED, Some(CONVERTED_UTF8), Some(1)),
            ColumnType::OptStr => (TYPE_BYTE_ARRAY, OPTIONAL, Some(CONVERTED_UTF8), Some(1)),
            ColumnType::Int32 => (TYPE_INT32, REQUIRED, None, None),
            ColumnType::Int64 => (TYPE_INT64, REQUIRED, None, None),
            ColumnType::Date => (TYPE_INT32, REQUIRED, Some(CONVERTED_DATE), Some(6)),
        }
    }
}

/// The text of one run of [`GroupWriter::runs`].
pub enum Text<'a> {
    /// The first so many bytes of the slot the callback was given.
    Short(usize),
    /// Text of any length.
    Long(&'a [u8]),
}

/// A zeroed array on the heap. Pages of it that are never written cost nothing.
fn zeroed<T: Copy + Default, const SIZE: usize>() -> Box<[T; SIZE]> {
    match vec![T::default(); SIZE].into_boxed_slice().try_into() {
        Ok(array) => array,
        Err(_) => unreachable!(),
    }
}

/// Replaces `buffer` by a zeroed one of at least `needed` bytes that starts
/// with the same `keep` bytes. A fresh zeroed allocation costs nothing for the
/// pages that are never written.
#[cold]
#[inline(never)]
fn enlarge(buffer: &mut Vec<u8>, keep: usize, needed: usize) {
    let mut larger = vec![0u8; needed.max(buffer.len() * 2).max(1 << 12)];
    larger[..keep].copy_from_slice(&buffer[..keep]);
    *buffer = larger;
}

/// Bits needed for dictionary indices below `entries` (at least one).
fn index_width(entries: usize) -> usize {
    (usize::BITS - entries.saturating_sub(1).leading_zeros()).max(1) as usize
}

/// Whether an integer can be stored as INT32.
fn fits_int32(value: i64) -> bool {
    i64::from(value as i32) == value
}

/// Bit-packs `value(0)`, `value(1)`, ... `value(count - 1)` at `width` bits
/// each (1 to 16, every value below `2^width`), least significant bits first
/// and padded with zeros to a multiple of eight values: the body of one
/// bit-packed run. `out` must hold `ceil(count / 8) * width` bytes plus 16
/// that get zeroed or scribbled on.
#[inline]
fn pack(count: usize, value: impl Fn(usize) -> u16, width: usize, out: &mut [u8]) {
    debug_assert!((1..=16).contains(&width));
    // Eight values are `width` whole bytes: four per half, both halves in one word.
    let word = |group: [u16; 8]| -> u128 {
        let (mut low, mut high) = (0u64, 0u64);
        for slot in 0..4 {
            low |= u64::from(group[slot]) << (slot * width);
            high |= u64::from(group[slot + 4]) << (slot * width);
        }
        u128::from(low) | (u128::from(high) << (4 * width))
    };
    let mut to = 0;
    let mut at = 0;
    if width <= 8 {
        // The common case: eight values fit one 64-bit word.
        while at + 8 <= count {
            let mut packed = 0u64;
            for slot in 0..8 {
                packed |= u64::from(value(at + slot)) << (slot * width);
            }
            out[to..to + 8].copy_from_slice(&packed.to_le_bytes());
            to += width;
            at += 8;
        }
    }
    while at + 8 <= count {
        let group = std::array::from_fn(|slot| value(at + slot));
        out[to..to + 16].copy_from_slice(&word(group).to_le_bytes());
        to += width;
        at += 8;
    }
    if at < count {
        let group = std::array::from_fn(|slot| if at + slot < count { value(at + slot) } else { 0 });
        out[to..to + 16].copy_from_slice(&word(group).to_le_bytes());
    }
}

/// Stores one RLE run of `rows` rows (at most 63) of dictionary entry `index`
/// at `runs[at..at + 3]`: a one-byte header (twice the length) and the 16-bit
/// index. The byte after it is scribbled on.
#[inline]
fn put_run(runs: &mut [u8; RUN_BYTES + 8], at: usize, rows: usize, index: usize) {
    let at = at & RUN_MASK;
    let record = ((rows as u32) << 1) | ((index as u32) << 8);
    runs[at..at + 4].copy_from_slice(&record.to_le_bytes());
}

/// Appends RLE runs of dictionary entry `index` for `rows` rows, at most 63
/// rows each, and returns the new fill of `runs`.
fn rle_runs(runs: &mut [u8; RUN_BYTES + 8], mut used: usize, mut rows: usize, index: usize) -> usize {
    while rows > 0 {
        let take = rows.min(63);
        put_run(runs, used, take, index);
        used += 3;
        rows -= take;
    }
    used
}

/// Appends one RLE run of a definition level: a varint header (twice the
/// length) and the level. An empty run is not written.
fn level_run(levels: &mut Vec<u8>, length: usize, level: bool) {
    if length > 0 {
        put_varint(levels, (length as u64) << 1);
        levels.push(u8::from(level));
    }
}

/// A page whose header is written and whose body is being written.
struct Page {
    /// Where its header and its body start in the pending bytes.
    start: usize,
    body: usize,
    /// What the header says.
    page_type: i32,
    num_values: usize,
    size: usize,
    encoding: i32,
}

/// The bytes of a file that are not written yet: its magic, then whole row
/// groups. The vector keeps its full length; `used` is how much is filled.
struct Pending {
    bytes: Vec<u8>,
    used: usize,
    /// File offset of `bytes[0]`.
    written: u64,
    /// The column chunk at hand is GZIP: its pages are compressed as they end.
    gzip: bool,
    /// Bytes the codec took off the pages of the chunk at hand.
    saved: i64,
    /// Scratch for a compressed page.
    packed: Vec<u8>,
}

impl Pending {
    /// The file offset of the next byte.
    fn position(&self) -> u64 {
        self.written + self.used as u64
    }

    /// The next `bytes` bytes, to be filled by the caller.
    #[inline]
    fn room(&mut self, bytes: usize) -> &mut [u8] {
        let start = self.used;
        let end = start + bytes;
        if end > self.bytes.len() {
            enlarge(&mut self.bytes, start, end);
        }
        self.used = end;
        &mut self.bytes[start..end]
    }

    #[inline]
    fn put(&mut self, bytes: &[u8]) {
        self.room(bytes.len()).copy_from_slice(bytes);
    }

    /// The body of one bit-packed run of `count` values (see [`pack`]).
    #[inline]
    fn pack(&mut self, count: usize, value: impl Fn(usize) -> u16, width: usize) {
        let bytes = count.div_ceil(8) * width;
        pack(count, value, width, self.room(bytes + 16));
        self.used -= 16;
    }

    /// Starts a page: writes its header for a body of `size` bytes, which the
    /// caller writes next and then hands to [`seal`](Self::seal).
    fn page(&mut self, page_type: i32, num_values: usize, size: usize, encoding: i32) -> io::Result<Page> {
        let start = self.used;
        let header = page_header(page_type, num_values, size, size, encoding)?;
        self.put(&header);
        Ok(Page { start, body: self.used, page_type, num_values, size, encoding })
    }

    /// Ends a page. In a GZIP chunk its body becomes one GZIP member and its
    /// header is written again with both sizes.
    fn seal(&mut self, page: Page) -> io::Result<()> {
        assert_eq!(self.used - page.body, page.size, "page size");
        if !self.gzip {
            return Ok(());
        }
        let room = gzip::bound(page.size);
        if self.packed.len() < room {
            self.packed = vec![0; room.max(2 * self.packed.len())];
        }
        let length = gzip::member(&self.bytes[page.body..self.used], &mut self.packed);
        let header = page_header(page.page_type, page.num_values, page.size, length, page.encoding)?;
        self.used = page.start;
        self.put(&header);
        let end = self.used + length;
        if end > self.bytes.len() {
            enlarge(&mut self.bytes, self.used, end);
        }
        self.bytes[self.used..end].copy_from_slice(&self.packed[..length]);
        self.used = end;
        self.saved += page.size as i64 - length as i64;
        Ok(())
    }
}

/// Writes a Parquet file one row group at a time.
///
/// Use: [`begin`](Self::begin) a row group, add one column chunk per field in
/// schema order with [`runs`](Self::runs), [`strings`](Self::strings),
/// [`ints`](Self::ints), [`sparse_ints`](Self::sparse_ints) or
/// [`dates`](Self::dates), repeat, then [`finish`](Self::finish). A file with
/// no row group is a valid empty table.
///
/// Rows of a row group are not written in the caller's order: `begin` takes a
/// predicate that sends rows to the tail of the row group. Every column
/// method takes its values in the caller's order and applies the same
/// reordering, so a caller that puts its rare, expensive rows (here: days
/// with purchases) in the tail gets long runs in the head for free.
///
/// Encodings, per column chunk, all standard:
/// * `runs`: a dictionary page with one entry per run of equal keys and an
///   RLE_DICTIONARY page of RLE runs (16-bit values).
/// * `strings`: the caller's dictionary and bit-packed indices of the values
///   that are present, or PLAIN when that is smaller; definition levels of a
///   nullable column are RLE runs.
/// * `ints`, `dates`: a dictionary of the distinct values and bit-packed
///   indices, or PLAIN when that is smaller.
/// * `sparse_ints`: leading zeros as one RLE run of a one-entry dictionary,
///   the remaining rows in a PLAIN page of the same chunk.
///
/// Pages are uncompressed unless a column is named to [`gzip`](Self::gzip).
pub struct GroupWriter<'a> {
    file: File,
    fields: &'a [(&'a str, ColumnType)],
    /// The fields whose chunks are GZIP.
    gzip: Vec<bool>,
    out: Pending,
    /// `fields.len()` chunks per finished row group, then those of the open one.
    chunks: Vec<ChunkInfo>,
    /// Rows of every row group; the last one is open until the next `begin` or `finish`.
    groups: Vec<usize>,
    /// The open row group: the caller's row written at each position (head
    /// rows, then tail rows), how many head rows there are, and for every
    /// caller's row the number of tail rows up to and including it.
    order: Box<[u16; MAX_GROUP_ROWS]>,
    head: usize,
    tails: Box<[u16; MAX_GROUP_ROWS]>,
    /// Scratch of `runs`: where each run starts, the dictionary page, and the
    /// RLE runs of the head and of the tail.
    starts: Box<[u16; MAX_GROUP_ROWS]>,
    entries: Vec<u8>,
    head_runs: Box<[u8; RUN_BYTES + 8]>,
    tail_runs: Box<[u8; RUN_BYTES + 8]>,
    /// Scratch of the dictionary encoders: an index per caller's row, the
    /// distinct integers, for every small integer its index plus one
    /// (`direct`, all zero between chunks) and the few integers beyond it
    /// (`far`).
    by_row: Box<[u16; MAX_GROUP_ROWS]>,
    distinct: Vec<i64>,
    direct: Box<[u16; DIRECT]>,
    far: Vec<(i64, u16)>,
}

impl<'a> GroupWriter<'a> {
    pub fn create(path: &Path, fields: &'a [(&'a str, ColumnType)]) -> io::Result<Self> {
        let mut writer = Self {
            file: File::create(path)?,
            fields,
            gzip: vec![false; fields.len()],
            out: Pending { bytes: Vec::new(), used: 0, written: 0, gzip: false, saved: 0, packed: Vec::new() },
            chunks: Vec::new(),
            groups: Vec::new(),
            order: zeroed(),
            head: 0,
            tails: zeroed(),
            starts: zeroed(),
            entries: Vec::new(),
            head_runs: zeroed(),
            tail_runs: zeroed(),
            by_row: zeroed(),
            distinct: Vec::new(),
            direct: zeroed(),
            far: Vec::new(),
        };
        writer.out.put(b"PAR1");
        Ok(writer)
    }

    /// Makes the chunks of the named columns GZIP: every page of theirs is a
    /// GZIP member (DEFLATE with Huffman codes only, or stored when that is
    /// not smaller). Worth it for columns whose bytes are far from uniform.
    pub fn gzip(mut self, columns: &[&str]) -> Self {
        for name in columns {
            let at = self.fields.iter().position(|field| field.0 == *name).expect("no such column");
            self.gzip[at] = true;
        }
        self
    }

    /// Starts a row group with one row per element of `marks` (1 to
    /// [`MAX_GROUP_ROWS`]). The rows whose mark makes `tail` true are written
    /// after all the others; within each of the two parts rows keep the
    /// caller's order.
    pub fn begin<T>(&mut self, marks: &[T], tail: impl Fn(&T) -> bool) -> io::Result<()> {
        let rows = marks.len();
        assert!((1..=MAX_GROUP_ROWS).contains(&rows), "row group size");
        assert_eq!(self.chunks.len(), self.groups.len() * self.fields.len(), "the open row group lacks columns");
        if self.out.used >= FLUSH_BYTES {
            self.flush()?;
        }
        // Usually enough for the whole row group, so the buffer is sized once.
        let wanted = self.out.used + 16 * rows + (1 << 12);
        if self.out.bytes.len() < wanted {
            enlarge(&mut self.out.bytes, self.out.used, wanted);
        }
        // Without a branch per row: every row is stored as the next head row
        // and as the next tail row, and only one of the two is kept.
        let (order, tails, behind) = (&mut *self.order, &mut *self.tails, &mut *self.by_row);
        let (mut head, mut tail_rows) = (0usize, 0usize);
        for (row, mark) in marks.iter().enumerate() {
            let is_tail = tail(mark);
            order[head & ROW_MASK] = row as u16;
            behind[tail_rows & ROW_MASK] = row as u16;
            head += usize::from(!is_tail);
            tail_rows += usize::from(is_tail);
            tails[row & ROW_MASK] = tail_rows as u16;
        }
        order[head..rows].copy_from_slice(&behind[..tail_rows]);
        self.head = head;
        self.groups.push(rows);
        Ok(())
    }

    /// Writes the footer and whatever is still pending.
    pub fn finish(mut self) -> io::Result<()> {
        let width = self.fields.len();
        assert_eq!(self.chunks.len(), self.groups.len() * width, "the open row group lacks columns");
        let mut meta = file_metadata(self.fields.iter().map(|(name, kind)| (*name, kind.leaf())));
        meta.i64(3, self.groups.iter().sum::<usize>() as i64);
        meta.list(4, T_STRUCT, self.groups.len());
        for (group, &rows) in self.groups.iter().enumerate() {
            let chunks = &self.chunks[group * width..(group + 1) * width];
            row_group(&mut meta, self.fields.iter().map(|(name, _)| *name), chunks, rows);
        }
        meta.string(6, CREATED_BY);
        let meta = meta.finish();
        self.out.put(&meta);
        self.out.put(&(meta.len() as u32).to_le_bytes());
        self.out.put(b"PAR1");
        self.flush()
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.write_all(&self.out.bytes[..self.out.used])?;
        self.out.written += self.out.used as u64;
        self.out.used = 0;
        Ok(())
    }

    /// Turns to the column that comes next in the open row group, which must
    /// have `values` rows, and returns its type.
    fn next(&mut self, values: usize) -> ColumnType {
        assert_eq!(values, *self.groups.last().expect("no open row group"), "column length");
        let done = self.chunks.len() - (self.groups.len() - 1) * self.fields.len();
        self.out.gzip = self.gzip[done];
        self.out.saved = 0;
        self.fields[done].1
    }

    /// Records the chunk that started at `offset` and ends at the current position.
    fn chunk(&mut self, physical_type: i32, offset: u64, data_page_offset: u64) {
        let num_values = *self.groups.last().expect("no open row group") as u64;
        let size = self.out.position() - offset;
        let raw_size = (size as i64 + self.out.saved) as u64;
        let gzip = self.out.gzip;
        self.chunks.push(ChunkInfo { physical_type, offset, data_page_offset, size, raw_size, gzip, num_values });
    }

    /// A required string column whose equal values are adjacent in the
    /// caller's order: `keys` has one key per row, equal keys mean equal
    /// strings, and `text` gives the string of a key, either by filling the
    /// slot it is handed and returning the length, or as a slice. It is
    /// called once per run of equal keys.
    pub fn runs<'t>(
        &mut self,
        keys: &[u64],
        mut text: impl FnMut(u64, &mut [u8; SHORT_TEXT]) -> Text<'t>,
    ) -> io::Result<()> {
        assert!(self.next(keys.len()) == ColumnType::Str, "column type");
        let rows = keys.len();

        // Where each run starts, without a branch per row: every row writes
        // its position into the slot of the run that would start there, and
        // the slot is kept only when the key differs from the one before.
        let starts = &mut *self.starts;
        let mut count = 0usize;
        let mut previous = !keys[0];
        for (at, &key) in keys.iter().enumerate() {
            starts[count & ROW_MASK] = at as u16;
            count += usize::from(key != previous);
            previous = key;
        }

        // One dictionary entry per run (PLAIN: length, then bytes), and its
        // rows as one RLE run among the head rows and one among the tail rows.
        if self.entries.len() < 13 * count + 32 {
            self.entries = vec![0; 13 * count + 32];
        }
        let (entries, head_runs, tail_runs, tails) =
            (&mut self.entries, &mut *self.head_runs, &mut *self.tail_runs, &*self.tails);
        let (mut entries_used, mut head_used, mut tail_used) = (0, 0, 0);
        // Tail rows before the run at hand: runs follow one another.
        let mut tails_before = 0;
        for run in 0..count {
            let start = usize::from(starts[run & ROW_MASK]);
            let end = if run + 1 < count { usize::from(starts[(run + 1) & ROW_MASK]) } else { rows };
            if entries.len() < entries_used + 4 + SHORT_TEXT {
                enlarge(entries, entries_used, entries_used + 4 + SHORT_TEXT);
            }
            let entry: &mut [u8; 4 + SHORT_TEXT] =
                (&mut entries[entries_used..entries_used + 4 + SHORT_TEXT]).try_into().unwrap();
            let (length_slot, slot) = entry.split_at_mut(4);
            let slot: &mut [u8; SHORT_TEXT] = slot.try_into().unwrap();
            let length = match text(keys[start], slot) {
                Text::Short(length) => {
                    assert!(length <= SHORT_TEXT);
                    length_slot.copy_from_slice(&(length as u32).to_le_bytes());
                    length
                }
                Text::Long(bytes) => {
                    if entries.len() < entries_used + 4 + bytes.len() {
                        enlarge(entries, entries_used, entries_used + 4 + bytes.len());
                    }
                    entries[entries_used..entries_used + 4].copy_from_slice(&(bytes.len() as u32).to_le_bytes());
                    entries[entries_used + 4..entries_used + 4 + bytes.len()].copy_from_slice(bytes);
                    bytes.len()
                }
            };
            entries_used += 4 + length;

            let tails_through = usize::from(tails[(end - 1) & ROW_MASK]);
            let behind = tails_through - tails_before;
            tails_before = tails_through;
            let ahead = end - start - behind;
            if ahead <= 63 && behind <= 63 {
                // Both runs are stored; an empty one is overwritten by the next.
                put_run(head_runs, head_used, ahead, run);
                head_used += if ahead > 0 { 3 } else { 0 };
                put_run(tail_runs, tail_used, behind, run);
                tail_used += if behind > 0 { 3 } else { 0 };
            } else {
                head_used = rle_runs(head_runs, head_used, ahead, run);
                tail_used = rle_runs(tail_runs, tail_used, behind, run);
            }
        }

        let offset = self.out.position();
        let page = self.out.page(PAGE_DICTIONARY, count, entries_used, ENCODING_PLAIN)?;
        self.out.put(&self.entries[..entries_used]);
        self.out.seal(page)?;
        let data_page_offset = self.out.position();
        let page = self.out.page(PAGE_DATA, rows, 1 + head_used + tail_used, ENCODING_RLE_DICTIONARY)?;
        self.out.put(&[16]); // bit width of the indices
        self.out.put(&self.head_runs[..head_used]);
        self.out.put(&self.tail_runs[..tail_used]);
        self.out.seal(page)?;
        self.chunk(TYPE_BYTE_ARRAY, offset, data_page_offset);
        Ok(())
    }

    /// A string column (nullable or not, as its field says): the value of a
    /// row is `dictionary[indices[row]]`, or null where the index is `null`.
    pub fn strings<S: AsRef<[u8]>>(&mut self, dictionary: &[S], indices: &[u32], null: u32) -> io::Result<()> {
        let optional = match self.next(indices.len()) {
            ColumnType::Str => false,
            ColumnType::OptStr => true,
            _ => panic!("column type"),
        };
        let rows = indices.len();
        let head = self.head;
        let order = &self.order[..rows];

        // One pass in the caller's order: how many values are present, the
        // highest index, and whether the nulls are exactly the head rows.
        let (mut present, mut highest, mut nulls_lead, mut tails_before) = (0usize, 0u32, true, 0u16);
        for (&index, &tails_through) in indices.iter().zip(&self.tails[..rows]) {
            let here = index != null;
            present += usize::from(here);
            highest = highest.max(if here { index } else { 0 });
            nulls_lead &= here == (tails_through != tails_before);
            tails_before = tails_through;
        }
        assert!(optional || present == rows, "null in a required column");
        assert!(present == 0 || (highest as usize) < dictionary.len(), "index outside the dictionary");

        // The definition levels (1 = present) of a nullable column, as RLE
        // runs in written order behind their byte length: what its data page
        // starts with. And the caller's rows of the values that are present,
        // in written order.
        let mut prefix = Vec::with_capacity(16);
        let mut scattered = Vec::new();
        let valued: &[u16] = if !optional {
            order
        } else {
            let mut levels = Vec::with_capacity(8);
            let valued = if nulls_lead {
                level_run(&mut levels, head, false);
                level_run(&mut levels, rows - head, true);
                &order[head..]
            } else {
                let (mut level, mut length) = (false, 0);
                for &row in order {
                    let here = indices[usize::from(row)] != null;
                    if here != level {
                        level_run(&mut levels, length, level);
                        (level, length) = (here, 0);
                    }
                    length += 1;
                    if here {
                        scattered.push(row);
                    }
                }
                level_run(&mut levels, length, level);
                &scattered[..]
            };
            prefix.extend_from_slice(&(levels.len() as u32).to_le_bytes());
            prefix.extend_from_slice(&levels);
            valued
        };
        debug_assert_eq!(valued.len(), present);
        let plain_size: usize =
            valued.iter().map(|&row| 4 + dictionary[indices[usize::from(row)] as usize].as_ref().len()).sum();

        let width = index_width(dictionary.len());
        let groups = present.div_ceil(8);
        let dictionary_size: usize = dictionary.iter().map(|entry| 4 + entry.as_ref().len()).sum();
        let offset = self.out.position();
        let mut data_page_offset = offset;
        if present > 0 && dictionary.len() <= DIRECT && dictionary_size + groups * width + 32 < plain_size {
            let page = self.out.page(PAGE_DICTIONARY, dictionary.len(), dictionary_size, ENCODING_PLAIN)?;
            for entry in dictionary {
                let entry = entry.as_ref();
                self.out.put(&(entry.len() as u32).to_le_bytes());
                self.out.put(entry);
            }
            self.out.seal(page)?;
            data_page_offset = self.out.position();
            let mut start = vec![width as u8];
            put_varint(&mut start, ((groups as u64) << 1) | 1);
            let size = prefix.len() + start.len() + groups * width;
            let page = self.out.page(PAGE_DATA, rows, size, ENCODING_RLE_DICTIONARY)?;
            self.out.put(&prefix);
            self.out.put(&start);
            self.out.pack(present, |at| indices[usize::from(valued[at])] as u16, width);
            self.out.seal(page)?;
        } else {
            let page = self.out.page(PAGE_DATA, rows, prefix.len() + plain_size, ENCODING_PLAIN)?;
            self.out.put(&prefix);
            for &row in valued {
                let entry = dictionary[indices[usize::from(row)] as usize].as_ref();
                self.out.put(&(entry.len() as u32).to_le_bytes());
                self.out.put(entry);
            }
            self.out.seal(page)?;
        }
        self.chunk(TYPE_BYTE_ARRAY, offset, data_page_offset);
        Ok(())
    }

    /// An integer column with few distinct values. `Ok(false)` means the
    /// field is INT32 and a value does not fit: the file is unusable and the
    /// caller starts it again with an INT64 field.
    pub fn ints(&mut self, values: &[i64]) -> io::Result<bool> {
        let physical_type = match self.next(values.len()) {
            ColumnType::Int32 => TYPE_INT32,
            ColumnType::Int64 => TYPE_INT64,
            _ => panic!("column type"),
        };
        let count = self.number(values);
        if physical_type == TYPE_INT32 {
            // Every value is in the dictionary, unless numbering gave up.
            let fits = match count {
                Some(count) => self.distinct[..count].iter().all(|&value| fits_int32(value)),
                None => values.iter().all(|&value| fits_int32(value)),
            };
            if !fits {
                return Ok(false);
            }
        }
        self.integers(physical_type, values, count)?;
        Ok(true)
    }

    /// A DATE column.
    pub fn dates(&mut self, days: &[i32]) -> io::Result<()> {
        assert!(self.next(days.len()) == ColumnType::Date, "column type");
        let count = self.number(days);
        self.integers(TYPE_INT32, days, count)
    }

    /// An integer column that is zero in most rows, when the row group's
    /// order puts the other rows in the tail. `Ok(false)` as for
    /// [`ints`](Self::ints).
    pub fn sparse_ints(&mut self, values: &[i64]) -> io::Result<bool> {
        let (physical_type, bytes) = match self.next(values.len()) {
            ColumnType::Int32 => (TYPE_INT32, 4),
            ColumnType::Int64 => (TYPE_INT64, 8),
            _ => panic!("column type"),
        };
        let rows = values.len();
        // One pass in the caller's order: the range, and whether every head row is zero.
        let (mut least, mut most, mut head_bits, mut tails_before) = (0i64, 0i64, 0i64, 0u16);
        for (&value, &tails_through) in values.iter().zip(&self.tails[..rows]) {
            least = least.min(value);
            most = most.max(value);
            head_bits |= if tails_through != tails_before { 0 } else { value };
            tails_before = tails_through;
        }
        if bytes == 4 && !(fits_int32(least) && fits_int32(most)) {
            return Ok(false);
        }
        let zeros = if head_bits == 0 {
            self.head
        } else {
            self.order[..rows].iter().take_while(|&&row| values[usize::from(row)] == 0).count()
        };

        let offset = self.out.position();
        let mut data_page_offset = offset;
        let mut done = 0;
        if zeros >= ZERO_RUN {
            // A dictionary that holds 0, and the leading zeros as one RLE run of its index.
            let page = self.out.page(PAGE_DICTIONARY, 1, bytes, ENCODING_PLAIN)?;
            self.out.put(&[0u8; 8][..bytes]);
            self.out.seal(page)?;
            data_page_offset = self.out.position();
            let mut run = vec![1u8]; // bit width of the index
            put_varint(&mut run, (zeros as u64) << 1);
            run.push(0);
            let page = self.out.page(PAGE_DATA, zeros, run.len(), ENCODING_RLE_DICTIONARY)?;
            self.out.put(&run);
            self.out.seal(page)?;
            done = zeros;
        }
        if done < rows {
            self.plain_page(bytes, done, values)?;
        }
        self.chunk(physical_type, offset, data_page_offset);
        Ok(true)
    }

    /// One PLAIN data page of the integers of the written rows from position `from` on.
    fn plain_page<T: Copy + Into<i64>>(&mut self, bytes: usize, from: usize, values: &[T]) -> io::Result<()> {
        let rows = values.len();
        let page = self.out.page(PAGE_DATA, rows - from, (rows - from) * bytes, ENCODING_PLAIN)?;
        let out = self.out.room((rows - from) * bytes);
        for (slot, &row) in out.chunks_exact_mut(bytes).zip(&self.order[from..rows]) {
            // Little-endian two's complement: the low bytes are the INT32.
            let value: i64 = values[usize::from(row)].into();
            slot.copy_from_slice(&value.to_le_bytes()[..bytes]);
        }
        self.out.seal(page)
    }

    /// Numbers the distinct values of a column as they first appear in the
    /// caller's order: `by_row` gets the number of every row's value and
    /// `distinct` the values. `None` when too many values lie outside the
    /// direct map for a dictionary to be worth it.
    fn number<T: Copy + Into<i64>>(&mut self, values: &[T]) -> Option<usize> {
        let (by_row, distinct, direct, far) =
            (&mut self.by_row[..values.len()], &mut self.distinct, &mut *self.direct, &mut self.far);
        distinct.clear();
        far.clear();
        let mut gave_up = false;
        for (index, &value) in by_row.iter_mut().zip(values) {
            let value: i64 = value.into();
            *index = if (value as u64) < DIRECT as u64 {
                // The map holds the index plus one; zero is "not seen yet".
                let slot = &mut direct[value as usize];
                if *slot == 0 {
                    distinct.push(value);
                    *slot = distinct.len() as u16;
                }
                *slot - 1
            } else if let Some(known) = far.iter().find(|known| known.0 == value) {
                known.1
            } else if far.len() < MOST_FAR {
                far.push((value, distinct.len() as u16));
                distinct.push(value);
                (distinct.len() - 1) as u16
            } else {
                gave_up = true;
                break;
            };
        }
        // Leave the direct map clean for the next chunk.
        for &value in distinct.iter() {
            if (value as u64) < DIRECT as u64 {
                direct[value as usize] = 0;
            }
        }
        (!gave_up).then_some(distinct.len())
    }

    /// One INT32 or INT64 chunk (every value fits) of a column that `number`
    /// has just numbered: the distinct values as a dictionary and bit-packed
    /// indices, or PLAIN when that is smaller.
    fn integers<T: Copy + Into<i64>>(
        &mut self,
        physical_type: i32,
        values: &[T],
        count: Option<usize>,
    ) -> io::Result<()> {
        let bytes = if physical_type == TYPE_INT64 { 8 } else { 4 };
        let rows = values.len();
        let groups = rows.div_ceil(8);
        let offset = self.out.position();
        let count = match count {
            Some(count) if count * bytes + groups * index_width(count) + 32 < rows * bytes => count,
            _ => {
                self.plain_page(bytes, 0, values)?;
                self.chunk(physical_type, offset, offset);
                return Ok(());
            }
        };
        let width = index_width(count);
        let page = self.out.page(PAGE_DICTIONARY, count, count * bytes, ENCODING_PLAIN)?;
        for (slot, value) in self.out.room(count * bytes).chunks_exact_mut(bytes).zip(&self.distinct[..count]) {
            slot.copy_from_slice(&value.to_le_bytes()[..bytes]);
        }
        self.out.seal(page)?;
        let data_page_offset = self.out.position();
        let mut start = vec![width as u8];
        put_varint(&mut start, ((groups as u64) << 1) | 1);
        let page = self.out.page(PAGE_DATA, rows, start.len() + groups * width, ENCODING_RLE_DICTIONARY)?;
        self.out.put(&start);
        let (order, by_row) = (&*self.order, &*self.by_row);
        self.out.pack(rows, |at| by_row[usize::from(order[at & ROW_MASK]) & ROW_MASK], width);
        self.out.seal(page)?;
        self.chunk(physical_type, offset, data_page_offset);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reads `count` values of `width` bits each, least significant bits first.
    fn unpack(bytes: &[u8], width: usize, count: usize) -> Vec<u16> {
        (0..count)
            .map(|at| {
                let mut value = 0u16;
                for bit in 0..width {
                    let position = at * width + bit;
                    value |= u16::from(bytes[position / 8] >> (position % 8) & 1) << bit;
                }
                value
            })
            .collect()
    }

    #[test]
    fn pack_is_least_significant_bit_first() {
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        for width in 1..=16usize {
            for count in 0..40usize {
                let values: Vec<u16> = (0..count)
                    .map(|_| {
                        state = state.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
                        ((state >> 33) & ((1 << width) - 1)) as u16
                    })
                    .collect();
                let bytes = count.div_ceil(8) * width;
                let mut out = vec![0xAAu8; bytes + 16];
                pack(count, |at| values[at], width, &mut out);
                assert_eq!(unpack(&out[..bytes], width, count), values, "width {width} count {count}");
                // The values that pad the last group are zero.
                let padded = unpack(&out[..bytes], width, count.div_ceil(8) * 8);
                assert!(padded[count..].iter().all(|&value| value == 0));
            }
        }
        // Width 4 is two values per byte, low nibble first.
        let mut out = [0u8; 24];
        pack(9, |at| at as u16 + 1, 4, &mut out);
        assert_eq!(&out[..8], &[0x21, 0x43, 0x65, 0x87, 0x09, 0, 0, 0]);
    }

    #[test]
    fn index_widths() {
        let cases = [(0, 1), (1, 1), (2, 1), (3, 2), (4, 2), (5, 3), (16, 4), (17, 5), (256, 8), (257, 9), (65536, 16)];
        for (entries, width) in cases {
            assert_eq!(index_width(entries), width, "{entries} entries");
        }
    }

    #[test]
    fn long_runs_are_split() {
        let mut runs: Box<[u8; RUN_BYTES + 8]> = zeroed();
        assert_eq!(rle_runs(&mut runs, 0, 0, 7), 0);
        let used = rle_runs(&mut runs, 0, 130, 0x1234);
        assert_eq!(&runs[..used], &[126, 0x34, 0x12, 126, 0x34, 0x12, 8, 0x34, 0x12]);
    }

    #[test]
    fn int32_range() {
        for value in [0, 1, -1, i64::from(i32::MAX), i64::from(i32::MIN)] {
            assert!(fits_int32(value));
        }
        for value in [i64::from(i32::MAX) + 1, i64::from(i32::MIN) - 1, i64::MAX, i64::MIN, 1 << 32] {
            assert!(!fits_int32(value));
        }
    }

    /// A small deterministic generator for the sample tables.
    struct Random(u64);

    impl Random {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        fn below(&mut self, bound: u64) -> u64 {
            self.next() % bound
        }
    }

    /// How one sample table is filled.
    #[derive(Clone, Copy)]
    struct Shape {
        name: &'static str,
        rows: usize,
        group_rows: usize,
        integer: ColumnType,
        /// Rows per run of equal keys: 1 to this many.
        longest_run: u64,
        /// Key texts longer than the short slot (and one empty text).
        long_keys: bool,
        /// Entries of the label dictionary; a label is null in this share of the rows, per thousand.
        labels: usize,
        null_share: u64,
        /// The tail of a row group is the rows with a label (or else a random 20%).
        tail_is_labelled: bool,
        /// Distinct values of `count`, and whether some of them are large.
        counts: u64,
        far_counts: bool,
        /// Days spread over this many dates.
        days: u64,
        /// `sum` may be nonzero in rows without a label.
        sums_anywhere: bool,
    }

    const SAMPLE_FIELDS: [&str; 7] = ["id", "key", "label", "tag", "day", "count", "sum"];

    /// Writes one sample file, with every column GZIP when `gzip` is set and
    /// else with its rows as JSON next to it; returns `false` when an INT32
    /// column could not hold a value.
    fn write_sample(directory: &Path, shape: &Shape, gzip: bool) -> bool {
        let mut random = Random(0x9E37_79B9_7F4A_7C15 ^ (shape.rows as u64) << 20 ^ shape.name.len() as u64);
        let labels: Vec<String> = (0..shape.labels).map(|at| format!("label {at} \u{e9}\u{65e5}")).collect();
        let tags: Vec<String> = (0..7).map(|at| "tag".repeat(at)).collect();
        let far = |value: u64| if shape.integer == ColumnType::Int64 { -(1i64 << 40) - value as i64 } else { 70_000 + 13 * value as i64 };
        let rows = shape.rows;
        let (mut keys, mut key_texts) = (Vec::with_capacity(rows), Vec::new());
        let (mut label, mut tag, mut day) = (Vec::new(), Vec::new(), Vec::new());
        let (mut count, mut sum, mut id) = (Vec::new(), Vec::new(), Vec::new());
        while keys.len() < rows {
            let key = key_texts.len() as u64;
            let text = match (shape.long_keys, key % 5) {
                (true, 0) => format!("a long key that does not fit the short slot, number {key} \u{fc}"),
                (true, 1) if key == 1 => String::new(),
                _ => format!("k{key}"),
            };
            key_texts.push(text);
            for _ in 0..(1 + random.below(shape.longest_run)).min((rows - keys.len()) as u64) {
                keys.push(key);
                let has_label = shape.labels > 0 && random.below(1000) >= shape.null_share;
                label.push(if has_label { random.below(shape.labels as u64) as u32 } else { u32::MAX });
                tag.push(random.below(tags.len() as u64) as u32);
                day.push(20_000 + random.below(shape.days) as i32 - if shape.days > 1000 { 30_000 } else { 0 });
                let which = random.below(shape.counts);
                count.push(if shape.far_counts && which % 3 == 0 { far(which) } else { which as i64 });
                let amount = if shape.integer == ColumnType::Int64 { (1 << 33) + random.below(1 << 40) as i64 } else { 1 + random.below(60_000) as i64 };
                sum.push(if has_label || (shape.sums_anywhere && random.below(4) == 0) { amount } else { 0 });
                id.push(id.len() as i64);
            }
        }
        let tail_draw: Vec<bool> = (0..rows).map(|_| random.below(5) == 0).collect();
        let marks: Vec<bool> =
            (0..rows).map(|row| if shape.tail_is_labelled { label[row] != u32::MAX } else { tail_draw[row] }).collect();

        let fields = [
            ("id", shape.integer),
            ("key", ColumnType::Str),
            ("label", ColumnType::OptStr),
            ("tag", ColumnType::Str),
            ("day", ColumnType::Date),
            ("count", shape.integer),
            ("sum", shape.integer),
        ];
        assert_eq!(fields.map(|field| field.0), SAMPLE_FIELDS);
        let file = format!("{}{}.parquet", shape.name, if gzip { "-gzip" } else { "" });
        let mut writer = GroupWriter::create(&directory.join(file), &fields).unwrap();
        if gzip {
            writer = writer.gzip(&SAMPLE_FIELDS);
        }
        let mut start = 0;
        while start < rows {
            let end = (start + shape.group_rows).min(rows);
            writer.begin(&marks[start..end], |&mark| mark).unwrap();
            if !writer.ints(&id[start..end]).unwrap() {
                return false;
            }
            let text = |key: u64, slot: &mut [u8; SHORT_TEXT]| {
                let text = key_texts[key as usize].as_bytes();
                if text.len() <= SHORT_TEXT && key % 2 == 0 {
                    slot[..text.len()].copy_from_slice(text);
                    Text::Short(text.len())
                } else {
                    Text::Long(text)
                }
            };
            writer.runs(&keys[start..end], text).unwrap();
            writer.strings(&labels, &label[start..end], u32::MAX).unwrap();
            writer.strings(&tags, &tag[start..end], u32::MAX).unwrap();
            writer.dates(&day[start..end]).unwrap();
            if !writer.ints(&count[start..end]).unwrap() || !writer.sparse_ints(&sum[start..end]).unwrap() {
                return false;
            }
            start = end;
        }
        writer.finish().unwrap();
        if gzip {
            return true;
        }

        // The same rows as JSON: one array per row, in the order of SAMPLE_FIELDS.
        let mut json = String::from("[\n");
        for row in 0..rows {
            let label_text =
                if label[row] == u32::MAX { "null".to_string() } else { format!("\"{}\"", labels[label[row] as usize]) };
            json.push_str(&format!(
                "[{}, \"{}\", {}, \"{}\", {}, {}, {}]{}\n",
                id[row],
                key_texts[keys[row] as usize],
                label_text,
                tags[tag[row] as usize],
                day[row],
                count[row],
                sum[row],
                if row + 1 < rows { "," } else { "" }
            ));
        }
        json.push_str("]\n");
        std::fs::write(directory.join(format!("{}.json", shape.name)), json).unwrap();
        true
    }

    /// Sample files for `checks/check_parquet_samples.py`, which reads them
    /// with polars and pyarrow and compares the rows with the JSON next to
    /// them. They reach the paths the two output tables never take: long
    /// runs, long and empty texts, nulls outside the head, INT64 columns,
    /// integers outside the direct map, every index width, PLAIN fallbacks.
    /// Every table is written twice: plain, and as `<name>-gzip.parquet` with
    /// all its columns GZIP.
    ///
    /// Run: `SS_PARQUET_SAMPLES=<empty dir> cargo test --release parquet_samples`
    /// and then `python checks/check_parquet_samples.py <dir>`. Without the
    /// variable the files go to a temporary folder and are only written.
    #[test]
    fn parquet_samples() {
        let directory = match std::env::var_os("SS_PARQUET_SAMPLES") {
            Some(directory) => std::path::PathBuf::from(directory),
            None => std::env::temp_dir().join(format!("ss-parquet-samples-{}", std::process::id())),
        };
        std::fs::create_dir_all(&directory).unwrap();
        let base = Shape {
            name: "",
            rows: 40_000,
            group_rows: MAX_GROUP_ROWS,
            integer: ColumnType::Int32,
            longest_run: 6,
            long_keys: false,
            labels: 21,
            null_share: 820,
            tail_is_labelled: true,
            counts: 180,
            far_counts: false,
            days: 14,
            sums_anywhere: false,
        };
        let mut shapes = vec![
            Shape { name: "like_user_daily", ..base },
            Shape { name: "one_row", rows: 1, ..base },
            Shape { name: "five_rows", rows: 5, null_share: 400, ..base },
            Shape { name: "small_groups", rows: 3_000, group_rows: 9, ..base },
            Shape { name: "groups_of_one", rows: 70, group_rows: 1, null_share: 500, ..base },
            Shape { name: "long_runs", rows: 30_000, longest_run: 400, long_keys: true, ..base },
            Shape { name: "scattered", tail_is_labelled: false, sums_anywhere: true, long_keys: true, ..base },
            Shape { name: "no_nulls", null_share: 0, ..base },
            Shape { name: "all_nulls", null_share: 1000, ..base },
            Shape { name: "no_labels_at_all", labels: 0, ..base },
            Shape { name: "many_labels", labels: 300, null_share: 100, ..base },
            Shape { name: "huge_dictionary", labels: 70_000, null_share: 100, rows: 20_000, ..base },
            Shape { name: "many_days", days: 5_000, ..base },
            Shape { name: "far_counts", far_counts: true, counts: 150, ..base },
            Shape { name: "too_many_far_counts", far_counts: true, counts: 600, ..base },
            Shape { name: "distinct_counts", counts: 1 << 30, ..base },
            Shape { name: "wide", integer: ColumnType::Int64, far_counts: true, sums_anywhere: true, ..base },
            Shape { name: "wide_small", integer: ColumnType::Int64, rows: 12, ..base },
        ];
        // Every index width from 1 to 14 bits.
        const WIDTH_NAMES: [&str; 14] =
            ["w01", "w02", "w03", "w04", "w05", "w06", "w07", "w08", "w09", "w10", "w11", "w12", "w13", "w14"];
        for (bits, name) in WIDTH_NAMES.iter().enumerate() {
            shapes.push(Shape { name, rows: MAX_GROUP_ROWS + 3, counts: 1 << (bits + 1), ..base });
        }
        for shape in &shapes {
            assert!(write_sample(&directory, shape, false), "{}", shape.name);
            assert!(write_sample(&directory, shape, true), "{}", shape.name);
        }
        // A table without rows.
        let fields = [("id", ColumnType::Int32), ("key", ColumnType::Str), ("label", ColumnType::OptStr)];
        GroupWriter::create(&directory.join("empty.parquet"), &fields).unwrap().finish().unwrap();
        std::fs::write(directory.join("empty.json"), "[]\n").unwrap();
        // A value that does not fit INT32 is reported, not written.
        let fields = [("count", ColumnType::Int32)];
        let path = directory.join("too_narrow.parquet");
        for (values, fits) in [(vec![1i64, 2, i64::from(i32::MAX)], true), (vec![1, i64::from(i32::MAX) + 1, 3], false)] {
            let mut writer = GroupWriter::create(&path, &fields).unwrap();
            writer.begin(&values, |_| false).unwrap();
            assert_eq!(writer.ints(&values).unwrap(), fits);
            drop(writer);
            let mut writer = GroupWriter::create(&path, &fields).unwrap();
            writer.begin(&values, |_| false).unwrap();
            assert_eq!(writer.sparse_ints(&values).unwrap(), fits);
        }
        std::fs::remove_file(&path).unwrap();
        if std::env::var_os("SS_PARQUET_SAMPLES").is_none() {
            std::fs::remove_dir_all(&directory).unwrap();
        }
    }
}
