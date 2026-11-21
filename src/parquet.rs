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

/// Most rows a row group of [`GroupWriter`] may hold: row positions and
/// dictionary indices of a row group then fit 16 bits with a value to spare.
pub const MAX_GROUP_ROWS: usize = 1 << 15;
/// Pending bytes at which [`GroupWriter::begin`] writes before it starts a row group.
const FLUSH_BYTES: usize = 1 << 16;
/// Bytes of the slot [`GroupWriter::runs`] hands its callback.
pub const SHORT_TEXT: usize = 16;
/// Integers below this find their dictionary index in a direct map.
const DIRECT: usize = 1 << 16;
/// "Not in the dictionary yet" in the direct map.
const UNSEEN: u16 = u16::MAX;
/// Distinct integers outside the direct map a chunk may hold before it is written PLAIN.
const MOST_FAR: usize = 64;
/// Leading zeros that make [`GroupWriter::sparse_ints`] write them as one run.
const ZERO_RUN: usize = 16;

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

/// Bit-packs `values` at `width` bits each (1 to 16, every value below
/// `2^width`), least significant bits first and padded with zeros to a
/// multiple of eight values: the body of one bit-packed run. `out` must hold
/// `ceil(values / 8) * width` bytes plus 16 that get zeroed or scribbled on.
fn pack(values: &[u16], width: usize, out: &mut [u8]) {
    debug_assert!((1..=16).contains(&width));
    // Eight values are `width` whole bytes: four per half, both halves in one word.
    let word = |group: &[u16; 8]| -> u128 {
        let (mut low, mut high) = (0u64, 0u64);
        for slot in 0..4 {
            low |= u64::from(group[slot]) << (slot * width);
            high |= u64::from(group[slot + 4]) << (slot * width);
        }
        u128::from(low) | (u128::from(high) << (4 * width))
    };
    let mut at = 0;
    let mut groups = values.chunks_exact(8);
    for group in &mut groups {
        let group: &[u16; 8] = group.try_into().unwrap();
        out[at..at + 16].copy_from_slice(&word(group).to_le_bytes());
        at += width;
    }
    let rest = groups.remainder();
    if !rest.is_empty() {
        let mut last = [0u16; 8];
        last[..rest.len()].copy_from_slice(rest);
        out[at..at + 16].copy_from_slice(&word(&last).to_le_bytes());
    }
}

/// Appends RLE runs of dictionary entry `index` for `rows` rows, at most 63
/// rows each: a one-byte header (twice the length) and the 16-bit index.
/// Returns the new fill of `records`, which must have a spare byte.
fn rle_runs(records: &mut [u8], mut used: usize, mut rows: usize, index: usize) -> usize {
    while rows > 0 {
        let take = rows.min(63);
        let record = ((take as u32) << 1) | ((index as u32) << 8);
        records[used..used + 4].copy_from_slice(&record.to_le_bytes());
        used += 3;
        rows -= take;
    }
    used
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
/// method takes values by the caller's row number and applies the same order,
/// so a caller that puts its rare, expensive rows (here: days with purchases)
/// in the tail gets long runs in the head for free.
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
    /// Bytes not written yet: the file's magic, then whole row groups. The
    /// vector keeps its full length; `used` is how much of it is filled.
    pending: Vec<u8>,
    used: usize,
    /// File offset of `pending[0]`.
    written: u64,
    /// `fields.len()` chunks per finished row group, then those of the open one.
    chunks: Vec<ChunkInfo>,
    /// Rows of every row group; the last one is open until the next `begin` or `finish`.
    groups: Vec<usize>,
    /// The open row group: the caller's row written at each position (head
    /// rows, then tail rows), how many head rows there are, and for every
    /// caller's row the number of tail rows before it (one extra entry: all).
    order: Vec<u16>,
    head: usize,
    tails: Vec<u16>,
    /// Scratch of `runs`: where each run starts, the dictionary page, and the
    /// RLE runs of the head and of the tail.
    starts: Vec<u32>,
    entries: Vec<u8>,
    head_runs: Vec<u8>,
    tail_runs: Vec<u8>,
    /// Scratch of the dictionary encoders: an index per written row, the
    /// distinct integers, their indices by value (`direct`, all `UNSEEN`
    /// between chunks) and the few values beyond it (`far`).
    indices: Vec<u16>,
    distinct: Vec<i64>,
    direct: Vec<u16>,
    far: Vec<(i64, u16)>,
}

impl<'a> GroupWriter<'a> {
    pub fn create(path: &Path, fields: &'a [(&'a str, ColumnType)]) -> io::Result<Self> {
        let mut writer = Self {
            file: File::create(path)?,
            fields,
            pending: Vec::new(),
            used: 0,
            written: 0,
            chunks: Vec::new(),
            groups: Vec::new(),
            order: Vec::new(),
            head: 0,
            tails: Vec::new(),
            starts: Vec::new(),
            entries: Vec::new(),
            head_runs: Vec::new(),
            tail_runs: Vec::new(),
            indices: Vec::new(),
            distinct: Vec::new(),
            direct: Vec::new(),
            far: Vec::new(),
        };
        writer.put(b"PAR1");
        Ok(writer)
    }

    /// Starts a row group of `rows` rows (1 to [`MAX_GROUP_ROWS`]). The rows
    /// for which `tail` is true are written after all the others; within
    /// each of the two parts rows keep the caller's order.
    pub fn begin(&mut self, rows: usize, tail: impl Fn(usize) -> bool) -> io::Result<()> {
        assert!((1..=MAX_GROUP_ROWS).contains(&rows), "row group size");
        assert_eq!(self.chunks.len(), self.groups.len() * self.fields.len(), "the open row group lacks columns");
        if self.used >= FLUSH_BYTES {
            self.flush()?;
        }
        // Usually enough for the whole row group, so the buffer is sized once.
        let wanted = self.used + 16 * rows + (1 << 12);
        if self.pending.len() < wanted {
            enlarge(&mut self.pending, self.used, wanted);
        }
        if self.order.len() < rows {
            self.order.resize(rows, 0);
            self.tails.resize(rows + 1, 0);
            self.indices.resize(rows, 0);
            self.distinct.resize(rows, 0);
        }
        // Without a branch per row: every row is stored as the next head row
        // and as the next tail row, and only one of the two is kept.
        let (order, tails, behind) = (&mut self.order, &mut self.tails, &mut self.indices);
        let (mut head, mut tail_rows) = (0, 0);
        for row in 0..rows {
            let is_tail = tail(row);
            tails[row] = tail_rows as u16;
            order[head] = row as u16;
            behind[tail_rows] = row as u16;
            head += usize::from(!is_tail);
            tail_rows += usize::from(is_tail);
        }
        tails[rows] = tail_rows as u16;
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
        self.put(&meta);
        self.put(&(meta.len() as u32).to_le_bytes());
        self.put(b"PAR1");
        self.flush()
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.write_all(&self.pending[..self.used])?;
        self.written += self.used as u64;
        self.used = 0;
        Ok(())
    }

    /// The file offset of the next pending byte.
    fn position(&self) -> u64 {
        self.written + self.used as u64
    }

    /// The next `bytes` pending bytes, to be filled by the caller.
    #[inline]
    fn room(&mut self, bytes: usize) -> &mut [u8] {
        let start = self.used;
        let end = start + bytes;
        if end > self.pending.len() {
            enlarge(&mut self.pending, start, end);
        }
        self.used = end;
        &mut self.pending[start..end]
    }

    #[inline]
    fn put(&mut self, bytes: &[u8]) {
        self.room(bytes.len()).copy_from_slice(bytes);
    }

    /// Rows of the open row group.
    fn rows(&self) -> usize {
        *self.groups.last().expect("no open row group")
    }

    /// The type of the column that comes next in the open row group.
    fn next(&self) -> ColumnType {
        let done = self.chunks.len() - (self.groups.len() - 1) * self.fields.len();
        self.fields[done].1
    }

    /// Records the chunk that started at `offset` and ends at the current position.
    fn chunk(&mut self, physical_type: i32, offset: u64, data_page_offset: u64) {
        let num_values = self.rows() as u64;
        let size = self.position() - offset;
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
        assert!(self.next() == ColumnType::Str, "column type");
        let rows = self.rows();
        assert_eq!(keys.len(), rows, "column length");

        // Where each run starts, without a branch per row: every row writes
        // its position into the slot of the run that would start there, and
        // the slot is kept only when the key differs from the one before.
        if self.starts.len() <= rows {
            self.starts.resize(rows + 1, 0);
        }
        let starts = &mut self.starts[..=rows];
        let mut count = 0;
        let mut previous = !keys[0];
        for (at, &key) in keys.iter().enumerate() {
            starts[count] = at as u32;
            count += usize::from(key != previous);
            previous = key;
        }
        starts[count] = rows as u32;

        // One dictionary entry per run (PLAIN: length, then bytes), and its
        // rows as one RLE run among the head rows and one among the tail rows.
        let most_runs = 3 * (count + rows / 63) + 4;
        if self.head_runs.len() < most_runs {
            self.head_runs = vec![0; most_runs];
            self.tail_runs = vec![0; most_runs];
        }
        if self.entries.len() < 13 * count + 32 {
            self.entries = vec![0; 13 * count + 32];
        }
        let (entries, head_runs, tail_runs, tails) =
            (&mut self.entries, &mut self.head_runs, &mut self.tail_runs, &self.tails);
        let (mut entries_used, mut head_used, mut tail_used) = (0, 0, 0);
        for run in 0..count {
            let (start, end) = (starts[run] as usize, starts[run + 1] as usize);
            if entries.len() < entries_used + 4 + SHORT_TEXT {
                enlarge(entries, entries_used, entries_used + 4 + SHORT_TEXT);
            }
            let slot: &mut [u8; SHORT_TEXT] =
                (&mut entries[entries_used + 4..entries_used + 4 + SHORT_TEXT]).try_into().unwrap();
            let length = match text(keys[start], slot) {
                Text::Short(length) => {
                    assert!(length <= SHORT_TEXT);
                    length
                }
                Text::Long(bytes) => {
                    if entries.len() < entries_used + 4 + bytes.len() {
                        enlarge(entries, entries_used, entries_used + 4 + bytes.len());
                    }
                    entries[entries_used + 4..entries_used + 4 + bytes.len()].copy_from_slice(bytes);
                    bytes.len()
                }
            };
            entries[entries_used..entries_used + 4].copy_from_slice(&(length as u32).to_le_bytes());
            entries_used += 4 + length;

            let behind = usize::from(tails[end] - tails[start]);
            let ahead = end - start - behind;
            if ahead <= 63 && behind <= 63 {
                // Both runs are stored; an empty one is overwritten by the next.
                let index = (run as u32) << 8;
                head_runs[head_used..head_used + 4].copy_from_slice(&(((ahead as u32) << 1) | index).to_le_bytes());
                head_used += if ahead > 0 { 3 } else { 0 };
                tail_runs[tail_used..tail_used + 4].copy_from_slice(&(((behind as u32) << 1) | index).to_le_bytes());
                tail_used += if behind > 0 { 3 } else { 0 };
            } else {
                head_used = rle_runs(head_runs, head_used, ahead, run);
                tail_used = rle_runs(tail_runs, tail_used, behind, run);
            }
        }

        let offset = self.position();
        let header = page_header(PAGE_DICTIONARY, count, entries_used, ENCODING_PLAIN)?;
        self.put(&header);
        let entries = std::mem::take(&mut self.entries);
        self.put(&entries[..entries_used]);
        self.entries = entries;
        let data_page_offset = self.position();
        let mut header = page_header(PAGE_DATA, rows, 1 + head_used + tail_used, ENCODING_RLE_DICTIONARY)?;
        header.push(16); // bit width of the indices
        self.put(&header);
        let head_runs = std::mem::take(&mut self.head_runs);
        self.put(&head_runs[..head_used]);
        self.head_runs = head_runs;
        let tail_runs = std::mem::take(&mut self.tail_runs);
        self.put(&tail_runs[..tail_used]);
        self.tail_runs = tail_runs;
        self.chunk(TYPE_BYTE_ARRAY, offset, data_page_offset);
        Ok(())
    }

    /// A string column (nullable or not, as its field says): the value of a
    /// row is `dictionary[index(row)]`, or null where the index is `null`.
    pub fn strings<S: AsRef<[u8]>>(
        &mut self,
        dictionary: &[S],
        index: impl Fn(usize) -> u32,
        null: u32,
    ) -> io::Result<()> {
        let optional = match self.next() {
            ColumnType::Str => false,
            ColumnType::OptStr => true,
            _ => panic!("column type"),
        };
        let rows = self.rows();

        // In written order: the definition levels (1 = present) as RLE runs of
        // a varint header (twice the length) and the level, and the indices
        // of the values that are present.
        let mut levels: Vec<u8> = Vec::new();
        let mut run: Option<(bool, u64)> = None;
        let (mut present, mut highest, mut plain_size) = (0usize, 0u32, 0usize);
        for at in 0..rows {
            let value = index(usize::from(self.order[at]));
            let here = value != null;
            match &mut run {
                Some((level, length)) if *level == here => *length += 1,
                _ => {
                    if let Some((level, length)) = run {
                        put_varint(&mut levels, length << 1);
                        levels.push(u8::from(level));
                    }
                    run = Some((here, 1));
                }
            }
            if here {
                assert!((value as usize) < dictionary.len(), "index outside the dictionary");
                self.indices[present] = value as u16;
                present += 1;
                highest = highest.max(value);
                plain_size += 4 + dictionary[value as usize].as_ref().len();
            }
        }
        if let Some((level, length)) = run {
            put_varint(&mut levels, length << 1);
            levels.push(u8::from(level));
        }
        assert!(optional || present == rows, "null in a required column");
        // What a data page starts with: the levels behind their byte length.
        let mut prefix = Vec::with_capacity(4 + levels.len());
        if optional {
            prefix.extend_from_slice(&(levels.len() as u32).to_le_bytes());
            prefix.extend_from_slice(&levels);
        }

        let width = index_width(dictionary.len());
        let groups = present.div_ceil(8);
        let dictionary_size: usize = dictionary.iter().map(|entry| 4 + entry.as_ref().len()).sum();
        let offset = self.position();
        if present > 0 && dictionary.len() <= DIRECT && dictionary_size + groups * width + 32 < plain_size {
            let header = page_header(PAGE_DICTIONARY, dictionary.len(), dictionary_size, ENCODING_PLAIN)?;
            self.put(&header);
            for entry in dictionary {
                let entry = entry.as_ref();
                self.put(&(entry.len() as u32).to_le_bytes());
                self.put(entry);
            }
            let data_page_offset = self.position();
            let mut start = vec![width as u8];
            put_varint(&mut start, ((groups as u64) << 1) | 1);
            let size = prefix.len() + start.len() + groups * width;
            let mut header = page_header(PAGE_DATA, rows, size, ENCODING_RLE_DICTIONARY)?;
            header.extend_from_slice(&prefix);
            header.extend_from_slice(&start);
            self.put(&header);
            let indices = std::mem::take(&mut self.indices);
            pack(&indices[..present], width, self.room(groups * width + 16));
            self.indices = indices;
            self.used -= 16;
            self.chunk(TYPE_BYTE_ARRAY, offset, data_page_offset);
        } else {
            let mut header = page_header(PAGE_DATA, rows, prefix.len() + plain_size, ENCODING_PLAIN)?;
            header.extend_from_slice(&prefix);
            self.put(&header);
            for at in 0..rows {
                let value = index(usize::from(self.order[at]));
                if value != null {
                    let entry = dictionary[value as usize].as_ref();
                    self.put(&(entry.len() as u32).to_le_bytes());
                    self.put(entry);
                }
            }
            self.chunk(TYPE_BYTE_ARRAY, offset, offset);
        }
        Ok(())
    }

    /// An integer column with few distinct values. `Ok(false)` means the
    /// field is INT32 and a value does not fit (it is negative or over
    /// `i32::MAX`): the file is unusable and the caller starts it again with
    /// an INT64 field.
    pub fn ints(&mut self, value: impl Fn(usize) -> i64) -> io::Result<bool> {
        let Some(physical_type) = self.integer_type(&value) else {
            return Ok(false);
        };
        self.integers(physical_type, value)?;
        Ok(true)
    }

    /// An integer column that is zero in most rows, when the row group's
    /// order puts those rows first. `Ok(false)` as for [`ints`](Self::ints).
    pub fn sparse_ints(&mut self, value: impl Fn(usize) -> i64) -> io::Result<bool> {
        let Some(physical_type) = self.integer_type(&value) else {
            return Ok(false);
        };
        let bytes = if physical_type == TYPE_INT64 { 8 } else { 4 };
        let rows = self.rows();
        let zeros = self.order[..rows].iter().take_while(|&&row| value(usize::from(row)) == 0).count();
        let offset = self.position();
        let mut data_page_offset = offset;
        let mut done = 0;
        if zeros >= ZERO_RUN {
            // A dictionary that holds 0, and the leading zeros as one RLE run of its index.
            let header = page_header(PAGE_DICTIONARY, 1, bytes, ENCODING_PLAIN)?;
            self.put(&header);
            self.put(&[0u8; 8][..bytes]);
            data_page_offset = self.position();
            let mut run = vec![1u8]; // bit width of the index
            put_varint(&mut run, (zeros as u64) << 1);
            run.push(0);
            let mut header = page_header(PAGE_DATA, zeros, run.len(), ENCODING_RLE_DICTIONARY)?;
            header.extend_from_slice(&run);
            self.put(&header);
            done = zeros;
        }
        if done < rows {
            self.plain_page(bytes, done, &value)?;
        }
        self.chunk(physical_type, offset, data_page_offset);
        Ok(true)
    }

    /// A DATE column.
    pub fn dates(&mut self, day: impl Fn(usize) -> i32) -> io::Result<()> {
        assert!(self.next() == ColumnType::Date, "column type");
        self.integers(TYPE_INT32, |row| i64::from(day(row)))
    }

    /// The physical type of the integer column that comes next, or `None`
    /// when it is INT32 and one of its values does not fit.
    fn integer_type(&self, value: &impl Fn(usize) -> i64) -> Option<i32> {
        match self.next() {
            ColumnType::Int64 => Some(TYPE_INT64),
            ColumnType::Int32 => {
                // A negative value has the top bit set, so one OR tells both.
                let all = (0..self.rows()).fold(0u64, |all, row| all | value(row) as u64);
                (all <= i32::MAX as u64).then_some(TYPE_INT32)
            }
            _ => panic!("column type"),
        }
    }

    /// One PLAIN data page of `value` for the written rows from `from` on.
    fn plain_page(&mut self, bytes: usize, from: usize, value: &impl Fn(usize) -> i64) -> io::Result<()> {
        let rows = self.rows();
        let header = page_header(PAGE_DATA, rows - from, (rows - from) * bytes, ENCODING_PLAIN)?;
        self.put(&header);
        let order = std::mem::take(&mut self.order);
        let out = self.room((rows - from) * bytes);
        for (slot, &row) in out.chunks_exact_mut(bytes).zip(&order[from..rows]) {
            // Little-endian two's complement: the low bytes are the INT32.
            slot.copy_from_slice(&value(usize::from(row)).to_le_bytes()[..bytes]);
        }
        self.order = order;
        Ok(())
    }

    /// One INT32 or INT64 chunk (every value fits): the distinct values as a
    /// dictionary and bit-packed indices, or PLAIN when that is smaller.
    fn integers(&mut self, physical_type: i32, value: impl Fn(usize) -> i64) -> io::Result<()> {
        let bytes = if physical_type == TYPE_INT64 { 8 } else { 4 };
        let rows = self.rows();
        if self.direct.is_empty() {
            self.direct = vec![UNSEEN; DIRECT];
        }
        // The index of every written row, numbering values as they first appear.
        let (order, indices, distinct, direct, far) =
            (&self.order, &mut self.indices, &mut self.distinct, &mut self.direct, &mut self.far);
        far.clear();
        let mut count = 0;
        let mut too_many = false;
        for at in 0..rows {
            let value = value(usize::from(order[at]));
            indices[at] = if (value as u64) < DIRECT as u64 {
                let slot = &mut direct[value as usize];
                if *slot == UNSEEN {
                    *slot = count as u16;
                    distinct[count] = value;
                    count += 1;
                }
                *slot
            } else if let Some(known) = far.iter().find(|known| known.0 == value) {
                known.1
            } else if far.len() < MOST_FAR {
                far.push((value, count as u16));
                distinct[count] = value;
                count += 1;
                (count - 1) as u16
            } else {
                too_many = true;
                break;
            };
        }
        for &value in &distinct[..count] {
            if (value as u64) < DIRECT as u64 {
                direct[value as usize] = UNSEEN;
            }
        }

        let width = index_width(count);
        let groups = rows.div_ceil(8);
        let offset = self.position();
        if too_many || count * bytes + groups * width + 32 >= rows * bytes {
            self.plain_page(bytes, 0, &value)?;
            self.chunk(physical_type, offset, offset);
            return Ok(());
        }
        let header = page_header(PAGE_DICTIONARY, count, count * bytes, ENCODING_PLAIN)?;
        self.put(&header);
        let distinct = std::mem::take(&mut self.distinct);
        for (slot, value) in self.room(count * bytes).chunks_exact_mut(bytes).zip(&distinct) {
            slot.copy_from_slice(&value.to_le_bytes()[..bytes]);
        }
        self.distinct = distinct;
        let data_page_offset = self.position();
        let mut start = vec![width as u8];
        put_varint(&mut start, ((groups as u64) << 1) | 1);
        let mut header = page_header(PAGE_DATA, rows, start.len() + groups * width, ENCODING_RLE_DICTIONARY)?;
        header.extend_from_slice(&start);
        self.put(&header);
        let indices = std::mem::take(&mut self.indices);
        pack(&indices[..rows], width, self.room(groups * width + 16));
        self.indices = indices;
        self.used -= 16;
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
                pack(&values, width, &mut out);
                assert_eq!(unpack(&out[..bytes], width, count), values, "width {width} count {count}");
                // The values that pad the last group are zero.
                let padded = unpack(&out[..bytes], width, count.div_ceil(8) * 8);
                assert!(padded[count..].iter().all(|&value| value == 0));
            }
        }
        // Width 4 is two values per byte, low nibble first.
        let mut out = [0u8; 24];
        pack(&[1, 2, 3, 4, 5, 6, 7, 8, 9], 4, &mut out);
        assert_eq!(&out[..8], &[0x21, 0x43, 0x65, 0x87, 0x09, 0, 0, 0]);
    }

    #[test]
    fn index_widths() {
        for (entries, width) in [(0, 1), (1, 1), (2, 1), (3, 2), (4, 2), (5, 3), (16, 4), (17, 5), (256, 8), (257, 9), (65536, 16)] {
            assert_eq!(index_width(entries), width, "{entries} entries");
        }
    }

    #[test]
    fn long_runs_are_split() {
        let mut records = vec![0u8; 64];
        assert_eq!(rle_runs(&mut records, 0, 0, 7), 0);
        let used = rle_runs(&mut records, 0, 130, 0x1234);
        assert_eq!(&records[..used], &[126, 0x34, 0x12, 126, 0x34, 0x12, 8, 0x34, 0x12]);
    }
}
