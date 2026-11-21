//! Minimal Parquet writers, with no dependencies. Layout and Thrift compact
//! encoding follow the Parquet format specification. Every page is an
//! uncompressed data page (v1) or a dictionary page.
//!
//! * [`write_file`]: one row group and one PLAIN page per column, from columns
//!   already in memory. The reference pipeline uses it.
//! * [`GroupWriter`]: the optimized engine's writer. It encodes one small row
//!   group at a time into a reused buffer and writes it with one call, so a
//!   file of any size takes a few hundred kilobytes of memory, and it uses
//!   dictionary pages where they are cheaper to produce than PLAIN ones.

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
/// page v1 (`encoding` of its values; levels are RLE).
fn page_header(page_type: i32, num_values: usize, page_size: usize, encoding: i32) -> io::Result<Vec<u8>> {
    let too_large = |_| io::Error::new(io::ErrorKind::InvalidData, "a Parquet page over 2 GiB");
    let size = i32::try_from(page_size).map_err(too_large)?;
    let num_values = i32::try_from(num_values).map_err(too_large)?;
    let mut header = Thrift::new();
    header.i32(1, page_type);
    header.i32(2, size); // uncompressed
    header.i32(3, size); // compressed (no codec)
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
    size: u64,
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
        meta.i32(4, 0); // UNCOMPRESSED
        meta.i64(5, chunk.num_values as i64);
        meta.i64(6, chunk.size as i64);
        meta.i64(7, chunk.size as i64);
        meta.i64(9, chunk.data_page_offset as i64);
        if dictionary {
            meta.i64(11, chunk.offset as i64); // dictionary_page_offset
        }
        meta.end_struct();
        meta.end_struct();
    }
    let total: u64 = chunks.iter().map(|chunk| chunk.size).sum();
    meta.i64(2, total as i64);
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
            let header = page_header(PAGE_DATA, num_rows, page_size, ENCODING_PLAIN)?;
            file.write_all(&header)?;
            file.write_all(&levels)?;
            file.write_all(values)?;
            let size = (header.len() + page_size) as u64;
            chunks.push(ChunkInfo { physical_type, offset, data_page_offset: offset, size, num_values: num_rows as u64 });
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

/// The bytes of a file that are not written yet: its magic, then whole row
/// groups. The vector keeps its full length; `used` is how much is filled.
struct Pending {
    bytes: Vec<u8>,
    used: usize,
    /// File offset of `bytes[0]`.
    written: u64,
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
pub struct GroupWriter<'a> {
    file: File,
    fields: &'a [(&'a str, ColumnType)],
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
            out: Pending { bytes: Vec::new(), used: 0, written: 0 },
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

    /// The type of the column that comes next in the open row group, which
    /// must have `values` rows.
    fn next(&self, values: usize) -> ColumnType {
        assert_eq!(values, *self.groups.last().expect("no open row group"), "column length");
        let done = self.chunks.len() - (self.groups.len() - 1) * self.fields.len();
        self.fields[done].1
    }

    /// Records the chunk that started at `offset` and ends at the current position.
    fn chunk(&mut self, physical_type: i32, offset: u64, data_page_offset: u64) {
        let num_values = *self.groups.last().expect("no open row group") as u64;
        let size = self.out.position() - offset;
        self.chunks.push(ChunkInfo { physical_type, offset, data_page_offset, size, num_values });
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
        let header = page_header(PAGE_DICTIONARY, count, entries_used, ENCODING_PLAIN)?;
        self.out.put(&header);
        self.out.put(&self.entries[..entries_used]);
        let data_page_offset = self.out.position();
        let mut header = page_header(PAGE_DATA, rows, 1 + head_used + tail_used, ENCODING_RLE_DICTIONARY)?;
        header.push(16); // bit width of the indices
        self.out.put(&header);
        self.out.put(&self.head_runs[..head_used]);
        self.out.put(&self.tail_runs[..tail_used]);
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
            let header = page_header(PAGE_DICTIONARY, dictionary.len(), dictionary_size, ENCODING_PLAIN)?;
            self.out.put(&header);
            for entry in dictionary {
                let entry = entry.as_ref();
                self.out.put(&(entry.len() as u32).to_le_bytes());
                self.out.put(entry);
            }
            data_page_offset = self.out.position();
            let mut start = vec![width as u8];
            put_varint(&mut start, ((groups as u64) << 1) | 1);
            let size = prefix.len() + start.len() + groups * width;
            let mut header = page_header(PAGE_DATA, rows, size, ENCODING_RLE_DICTIONARY)?;
            header.extend_from_slice(&prefix);
            header.extend_from_slice(&start);
            self.out.put(&header);
            self.out.pack(present, |at| indices[usize::from(valued[at])] as u16, width);
        } else {
            let mut header = page_header(PAGE_DATA, rows, prefix.len() + plain_size, ENCODING_PLAIN)?;
            header.extend_from_slice(&prefix);
            self.out.put(&header);
            for &row in valued {
                let entry = dictionary[indices[usize::from(row)] as usize].as_ref();
                self.out.put(&(entry.len() as u32).to_le_bytes());
                self.out.put(entry);
            }
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
            let header = page_header(PAGE_DICTIONARY, 1, bytes, ENCODING_PLAIN)?;
            self.out.put(&header);
            self.out.put(&[0u8; 8][..bytes]);
            data_page_offset = self.out.position();
            let mut run = vec![1u8]; // bit width of the index
            put_varint(&mut run, (zeros as u64) << 1);
            run.push(0);
            let mut header = page_header(PAGE_DATA, zeros, run.len(), ENCODING_RLE_DICTIONARY)?;
            header.extend_from_slice(&run);
            self.out.put(&header);
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
        let header = page_header(PAGE_DATA, rows - from, (rows - from) * bytes, ENCODING_PLAIN)?;
        self.out.put(&header);
        let out = self.out.room((rows - from) * bytes);
        for (slot, &row) in out.chunks_exact_mut(bytes).zip(&self.order[from..rows]) {
            // Little-endian two's complement: the low bytes are the INT32.
            let value: i64 = values[usize::from(row)].into();
            slot.copy_from_slice(&value.to_le_bytes()[..bytes]);
        }
        Ok(())
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
        let header = page_header(PAGE_DICTIONARY, count, count * bytes, ENCODING_PLAIN)?;
        self.out.put(&header);
        for (slot, value) in self.out.room(count * bytes).chunks_exact_mut(bytes).zip(&self.distinct[..count]) {
            slot.copy_from_slice(&value.to_le_bytes()[..bytes]);
        }
        let data_page_offset = self.out.position();
        let mut start = vec![width as u8];
        put_varint(&mut start, ((groups as u64) << 1) | 1);
        let mut header = page_header(PAGE_DATA, rows, start.len() + groups * width, ENCODING_RLE_DICTIONARY)?;
        header.extend_from_slice(&start);
        self.out.put(&header);
        let (order, by_row) = (&*self.order, &*self.by_row);
        self.out.pack(rows, |at| by_row[usize::from(order[at & ROW_MASK]) & ROW_MASK], width);
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
}
