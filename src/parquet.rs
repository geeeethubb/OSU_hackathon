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

/// Most rows a row group of [`GroupWriter`] may hold: a dictionary index then
/// always fits 16 bits.
pub const MAX_GROUP_ROWS: usize = 1 << 16;
/// Pending bytes at which [`GroupWriter::begin`] writes before it starts a row group.
const FLUSH_BYTES: usize = 1 << 16;
/// Bytes of the slot [`GroupWriter::runs`] hands its callback.
pub const SHORT_TEXT: usize = 16;

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

/// Bit-packs `index(value)` of every value at `width` (1 to 8) bits each,
/// least significant bits first: the body of one bit-packed run. `out` must
/// hold `ceil(values / 8) * width` bytes plus 8 that may be scribbled on.
#[inline]
fn pack<T: Copy>(values: &[T], index: impl Fn(T) -> u32, width: usize, out: &mut [u8]) {
    debug_assert!((1..=8).contains(&width));
    let mut at = 0;
    let mut groups = values.chunks_exact(8);
    for group in &mut groups {
        let mut word = 0u64;
        for (slot, &value) in group.iter().enumerate() {
            word |= u64::from(index(value)) << (slot * width);
        }
        out[at..at + 8].copy_from_slice(&word.to_le_bytes());
        at += width;
    }
    let rest = groups.remainder();
    if !rest.is_empty() {
        let mut word = 0u64;
        for (slot, &value) in rest.iter().enumerate() {
            word |= u64::from(index(value)) << (slot * width);
        }
        out[at..at + 8].copy_from_slice(&word.to_le_bytes());
    }
}

/// Writes a Parquet file one row group at a time.
///
/// Use: [`begin`](Self::begin) a row group, add one column chunk per field in
/// schema order with [`runs`](Self::runs), [`strings`](Self::strings),
/// [`ints`](Self::ints) or [`dates`](Self::dates), repeat, then
/// [`finish`](Self::finish). A file with no row group is a valid empty table.
///
/// Encodings, per column chunk:
/// * `runs`: a dictionary with one entry per run of equal keys, and the
///   indices as RLE runs (16-bit values).
/// * `strings`: the caller's dictionary (up to 256 entries) and 8-bit indices
///   in one bit-packed run; PLAIN without a dictionary when the dictionary is
///   larger or every value is null.
/// * `ints` (INT32) and `dates` whose values all lie within 255 of a base: the
///   dictionary `base..=base + n` and indices of 1 to 8 bits in one bit-packed
///   run; PLAIN otherwise. `ints` of an INT64 column are always PLAIN.
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
    /// Scratch of `runs`: where each run starts, the dictionary page and the RLE runs.
    starts: Vec<u32>,
    entries: Vec<u8>,
    records: Vec<u8>,
    /// Experiment switches (`SS_OUT`), see `flags`.
    flags: u32,
}

/// Temporary experiment switches read from `SS_OUT`: 1 = `runs` writes PLAIN,
/// 4 = no small integer dictionaries, 8 = `strings` writes PLAIN.
pub fn flags() -> u32 {
    std::env::var("SS_OUT").ok().and_then(|value| value.parse().ok()).unwrap_or(0)
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
            starts: Vec::new(),
            entries: Vec::new(),
            records: Vec::new(),
            flags: flags(),
        };
        writer.put(b"PAR1");
        Ok(writer)
    }

    /// Starts a row group of `rows` rows (1 to [`MAX_GROUP_ROWS`]).
    pub fn begin(&mut self, rows: usize) -> io::Result<()> {
        assert!((1..=MAX_GROUP_ROWS).contains(&rows), "row group size");
        assert_eq!(self.chunks.len(), self.groups.len() * self.fields.len(), "the open row group lacks columns");
        if self.used >= FLUSH_BYTES {
            self.flush()?;
        }
        // Usually enough for the whole row group, so the buffer is sized once.
        let wanted = self.used + 24 * rows + (1 << 12);
        if self.pending.len() < wanted {
            enlarge(&mut self.pending, self.used, wanted);
        }
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

    /// The kind of the column that comes next in the open row group, of which
    /// `values` must be the row count.
    fn next(&self, values: usize) -> ColumnType {
        let rows = *self.groups.last().expect("no open row group");
        assert_eq!(values, rows, "column length");
        let done = self.chunks.len() - (self.groups.len() - 1) * self.fields.len();
        self.fields[done].1
    }

    /// Records the chunk that started at `offset` and ends at the current position.
    fn chunk(&mut self, physical_type: i32, offset: u64, data_page_offset: u64) {
        let num_values = *self.groups.last().expect("no open row group") as u64;
        let size = self.position() - offset;
        self.chunks.push(ChunkInfo { physical_type, offset, data_page_offset, size, num_values });
    }

    /// A required string column whose equal values are adjacent: `keys` has
    /// one key per row, equal keys mean equal strings, and `text` gives the
    /// string of a key, either by filling the slot it is handed and returning
    /// the length, or as a slice. It is called once per run of equal keys.
    pub fn runs<'t>(
        &mut self,
        keys: &[u64],
        mut text: impl FnMut(u64, &mut [u8; SHORT_TEXT]) -> Text<'t>,
    ) -> io::Result<()> {
        assert!(self.next(keys.len()) == ColumnType::Str, "column kind");
        let rows = keys.len();

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

        // The dictionary page (PLAIN: length, then bytes) and the RLE runs.
        // A run is a one-byte header (twice its length, at most 63 rows) and
        // the 16-bit index of its entry; a longer run takes several.
        let most_records = 3 * (count + rows / 63) + 4;
        if self.records.len() < most_records {
            self.records = vec![0; most_records];
        }
        if self.entries.len() < 13 * count + 32 {
            self.entries = vec![0; 13 * count + 32];
        }
        let entries = &mut self.entries;
        let records = &mut self.records;
        let mut entries_used = 0;
        let mut records_used = 0;
        for run in 0..count {
            let start = starts[run] as usize;
            let mut left = starts[run + 1] as usize - start;
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
            loop {
                let take = left.min(63);
                let record = ((take as u32) << 1) | ((run as u32) << 8);
                records[records_used..records_used + 4].copy_from_slice(&record.to_le_bytes());
                records_used += 3;
                left -= take;
                if left == 0 {
                    break;
                }
            }
        }

        let offset = self.position();
        if self.flags & 1 != 0 {
            // Experiment: PLAIN, every run's entry repeated for each of its rows.
            let entries = std::mem::take(&mut self.entries);
            let starts = std::mem::take(&mut self.starts);
            let mut size = 0;
            let mut at = 0;
            for run in 0..count {
                let length = 4 + u32::from_le_bytes(entries[at..at + 4].try_into().unwrap()) as usize;
                size += length * (starts[run + 1] - starts[run]) as usize;
                at += length;
            }
            let header = page_header(PAGE_DATA, rows, size, ENCODING_PLAIN)?;
            self.put(&header);
            let out = self.room(size + SHORT_TEXT + 4);
            let mut at = 0;
            let mut to = 0;
            for run in 0..count {
                let length = 4 + u32::from_le_bytes(entries[at..at + 4].try_into().unwrap()) as usize;
                for _ in starts[run]..starts[run + 1] {
                    if length <= 4 + SHORT_TEXT {
                        out[to..to + 4 + SHORT_TEXT].copy_from_slice(&entries[at..at + 4 + SHORT_TEXT]);
                    } else {
                        out[to..to + length].copy_from_slice(&entries[at..at + length]);
                    }
                    to += length;
                }
                at += length;
            }
            self.used -= SHORT_TEXT + 4;
            self.entries = entries;
            self.starts = starts;
            self.chunk(TYPE_BYTE_ARRAY, offset, offset);
            return Ok(());
        }
        let header = page_header(PAGE_DICTIONARY, count, entries_used, ENCODING_PLAIN)?;
        self.put(&header);
        let entries = std::mem::take(&mut self.entries);
        self.put(&entries[..entries_used]);
        self.entries = entries;
        let data_page_offset = self.position();
        let mut header = page_header(PAGE_DATA, rows, 1 + records_used, ENCODING_RLE_DICTIONARY)?;
        header.push(16); // bit width of the indices
        self.put(&header);
        let records = std::mem::take(&mut self.records);
        self.put(&records[..records_used]);
        self.records = records;
        self.chunk(TYPE_BYTE_ARRAY, offset, data_page_offset);
        Ok(())
    }

    /// A string column (nullable or not, as its field says) whose values are
    /// `dictionary[index]`; `null` is the index that stands for null.
    pub fn strings<S: AsRef<[u8]>>(&mut self, dictionary: &[S], indices: &[u32], null: u32) -> io::Result<()> {
        let kind = self.next(indices.len());
        assert!(kind == ColumnType::Str || kind == ColumnType::OptStr, "column kind");
        let optional = kind == ColumnType::OptStr;
        let rows = indices.len();
        let (mut present, mut highest) = (0usize, 0u32);
        for &index in indices {
            present += usize::from(index != null);
            highest = highest.max(if index != null { index } else { 0 });
        }
        assert!(optional || present == rows, "null in a required column");
        assert!(present == 0 || (highest as usize) < dictionary.len(), "index outside the dictionary");

        // A nullable column starts with its definition levels: their byte
        // length, then one bit-packed run at width 1 (1 = present).
        let groups = rows.div_ceil(8);
        let level_bytes = if optional { groups } else { 0 };
        let mut levels = Vec::with_capacity(8);
        if optional {
            let mut run = Vec::with_capacity(4);
            put_varint(&mut run, ((groups as u64) << 1) | 1);
            levels.extend_from_slice(&((run.len() + groups) as u32).to_le_bytes());
            levels.extend_from_slice(&run);
        }

        let offset = self.position();
        if present > 0 && dictionary.len() <= 256 && self.flags & 8 == 0 {
            let size: usize = dictionary.iter().map(|entry| 4 + entry.as_ref().len()).sum();
            let header = page_header(PAGE_DICTIONARY, dictionary.len(), size, ENCODING_PLAIN)?;
            self.put(&header);
            for entry in dictionary {
                let entry = entry.as_ref();
                self.put(&(entry.len() as u32).to_le_bytes());
                self.put(entry);
            }
            let data_page_offset = self.position();
            // The indices of the values that are present: bit width 8, one bit-packed run.
            let value_groups = present.div_ceil(8);
            let mut values = vec![8u8];
            put_varint(&mut values, ((value_groups as u64) << 1) | 1);
            let size = levels.len() + level_bytes + values.len() + 8 * value_groups;
            let mut header = page_header(PAGE_DATA, rows, size, ENCODING_RLE_DICTIONARY)?;
            header.extend_from_slice(&levels);
            self.put(&header);
            // Eight spare bytes: a null row stores its index too, and the next
            // value overwrites it.
            let body = self.room(level_bytes + values.len() + 8 * value_groups + 8);
            let (level_slots, rest) = body.split_at_mut(level_bytes);
            let (start, packed) = rest.split_at_mut(values.len());
            start.copy_from_slice(&values);
            let mut filled = 0;
            for (group, chunk) in indices.chunks(8).enumerate() {
                let mut bits = 0u8;
                for (bit, &index) in chunk.iter().enumerate() {
                    let here = index != null;
                    bits |= u8::from(here) << bit;
                    packed[filled] = index as u8;
                    filled += usize::from(here);
                }
                if optional {
                    level_slots[group] = bits;
                }
            }
            packed[present..8 * value_groups].fill(0);
            self.used -= 8;
            self.chunk(TYPE_BYTE_ARRAY, offset, data_page_offset);
        } else {
            let mut size = levels.len() + level_bytes;
            for &index in indices {
                if index != null {
                    size += 4 + dictionary[index as usize].as_ref().len();
                }
            }
            let mut header = page_header(PAGE_DATA, rows, size, ENCODING_PLAIN)?;
            header.extend_from_slice(&levels);
            self.put(&header);
            if optional {
                for (slot, chunk) in self.room(level_bytes).iter_mut().zip(indices.chunks(8)) {
                    let mut bits = 0u8;
                    for (bit, &index) in chunk.iter().enumerate() {
                        bits |= u8::from(index != null) << bit;
                    }
                    *slot = bits;
                }
            }
            for &index in indices {
                if index != null {
                    let entry = dictionary[index as usize].as_ref();
                    self.put(&(entry.len() as u32).to_le_bytes());
                    self.put(entry);
                }
            }
            self.chunk(TYPE_BYTE_ARRAY, offset, offset);
        }
        Ok(())
    }

    /// An integer column. `Ok(false)` means the field is INT32 and a value
    /// does not fit (it is negative or over `i32::MAX`): nothing usable was
    /// written, and the caller starts the file again with an INT64 field.
    pub fn ints(&mut self, values: &[i64]) -> io::Result<bool> {
        let rows = values.len();
        match self.next(rows) {
            ColumnType::Int64 => {
                let offset = self.position();
                let header = page_header(PAGE_DATA, rows, 8 * rows, ENCODING_PLAIN)?;
                self.put(&header);
                for (slot, value) in self.room(8 * rows).chunks_exact_mut(8).zip(values) {
                    slot.copy_from_slice(&value.to_le_bytes());
                }
                self.chunk(TYPE_INT64, offset, offset);
            }
            ColumnType::Int32 => {
                let all = values.iter().fold(0u64, |all, &value| all | value as u64);
                if all > i32::MAX as u64 {
                    return Ok(false);
                }
                if all < 256 && self.flags & 4 == 0 {
                    self.small(values, |value| value as u32, 0, all as u32)?;
                } else {
                    self.plain_int32(values, |value| value as u32)?;
                }
            }
            _ => panic!("column kind"),
        }
        Ok(true)
    }

    /// A DATE column. `first` is a day at or shortly before every value (the
    /// window's first day); any other value is still written correctly.
    pub fn dates(&mut self, days: &[i32], first: i32) -> io::Result<()> {
        assert!(self.next(days.len()) == ColumnType::Date, "column kind");
        let all = days.iter().fold(0u32, |all, &day| all | day.wrapping_sub(first) as u32);
        if all < 256 && self.flags & 4 == 0 {
            self.small(days, |day| day.wrapping_sub(first) as u32, first, all)
        } else {
            self.plain_int32(days, |day| day as u32)
        }
    }

    /// An INT32 chunk of the values `base + index(value)`, where the bitwise
    /// OR of all indices is `all` (below 256): the dictionary `base..=base +
    /// all`, then the indices in one bit-packed run.
    fn small<T: Copy>(&mut self, values: &[T], index: impl Fn(T) -> u32, base: i32, all: u32) -> io::Result<()> {
        let rows = values.len();
        let entries = all as usize + 1;
        let width = ((32 - all.leading_zeros()) as usize).max(1);
        let offset = self.position();
        let header = page_header(PAGE_DICTIONARY, entries, 4 * entries, ENCODING_PLAIN)?;
        self.put(&header);
        for (slot, entry) in self.room(4 * entries).chunks_exact_mut(4).zip(0i32..) {
            slot.copy_from_slice(&base.wrapping_add(entry).to_le_bytes());
        }
        let data_page_offset = self.position();
        let groups = rows.div_ceil(8);
        let mut start = vec![width as u8];
        put_varint(&mut start, ((groups as u64) << 1) | 1);
        let mut header = page_header(PAGE_DATA, rows, start.len() + groups * width, ENCODING_RLE_DICTIONARY)?;
        header.extend_from_slice(&start);
        self.put(&header);
        pack(values, index, width, self.room(groups * width + 8));
        self.used -= 8;
        self.chunk(TYPE_INT32, offset, data_page_offset);
        Ok(())
    }

    /// A PLAIN INT32 chunk of `low(value)`.
    fn plain_int32<T: Copy>(&mut self, values: &[T], low: impl Fn(T) -> u32) -> io::Result<()> {
        let offset = self.position();
        let header = page_header(PAGE_DATA, values.len(), 4 * values.len(), ENCODING_PLAIN)?;
        self.put(&header);
        for (slot, &value) in self.room(4 * values.len()).chunks_exact_mut(4).zip(values) {
            slot.copy_from_slice(&low(value).to_le_bytes());
        }
        self.chunk(TYPE_INT32, offset, offset);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reads `count` values of `width` bits each, least significant bits first.
    fn unpack(bytes: &[u8], width: usize, count: usize) -> Vec<u32> {
        (0..count)
            .map(|at| {
                let mut value = 0u32;
                for bit in 0..width {
                    let position = at * width + bit;
                    value |= u32::from(bytes[position / 8] >> (position % 8) & 1) << bit;
                }
                value
            })
            .collect()
    }

    #[test]
    fn pack_is_least_significant_bit_first() {
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        for width in 1..=8usize {
            for count in 0..40usize {
                let values: Vec<i64> = (0..count)
                    .map(|_| {
                        state = state.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
                        ((state >> 33) & ((1 << width) - 1)) as i64
                    })
                    .collect();
                let bytes = count.div_ceil(8) * width;
                let mut out = vec![0xAAu8; bytes + 8];
                pack(&values, |value| value as u32, width, &mut out);
                let read = unpack(&out[..bytes], width, count);
                assert_eq!(read, values.iter().map(|&value| value as u32).collect::<Vec<_>>(), "width {width} count {count}");
                // Padding after the last value is zero.
                let padded = unpack(&out[..bytes], width, count.div_ceil(8) * 8);
                assert!(padded[count..].iter().all(|&value| value == 0));
            }
        }
        // Width 4 is two values per byte, low nibble first.
        let mut out = [0u8; 12];
        pack(&[1i32, 2, 3, 4, 5, 6, 7, 8, 9], |value| value as u32, 4, &mut out);
        assert_eq!(&out[..8], &[0x21, 0x43, 0x65, 0x87, 0x09, 0, 0, 0]);
    }
}
