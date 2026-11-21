//! A minimal Parquet writer: one row group, one PLAIN-encoded, uncompressed
//! data page (v1) per column. Enough for the two output tables, with no
//! dependencies. Layout and Thrift compact encoding follow the Parquet format
//! specification.

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
    stack: Vec<i16>,
}

impl Thrift {
    fn new() -> Self {
        Self { out: Vec::new(), last_field: 0, stack: Vec::new() }
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
        self.stack.push(self.last_field);
        self.last_field = 0;
    }

    fn end_struct(&mut self) {
        self.out.push(0);
        self.last_field = self.stack.pop().unwrap_or(0);
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

fn page_header(num_values: usize, page_size: usize) -> Vec<u8> {
    let mut header = Thrift::new();
    header.i32(1, 0); // DATA_PAGE
    header.i32(2, page_size as i32); // uncompressed
    header.i32(3, page_size as i32); // compressed (no codec)
    header.begin_struct(5); // DataPageHeader
    header.i32(1, num_values as i32);
    header.i32(2, ENCODING_PLAIN);
    header.i32(3, ENCODING_RLE);
    header.i32(4, ENCODING_RLE);
    header.end_struct();
    header.finish()
}

struct ChunkInfo {
    physical_type: i32,
    offset: u64,
    size: u64,
    num_values: u64,
}

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
            let header = page_header(num_rows, page_size);
            file.write_all(&header)?;
            file.write_all(&levels)?;
            file.write_all(values)?;
            let size = (header.len() + page_size) as u64;
            chunks.push(ChunkInfo { physical_type, offset, size, num_values: num_rows as u64 });
            offset += size;
        }
    }

    let mut meta = Thrift::new();
    meta.i32(1, 1); // version
    meta.list(2, T_STRUCT, columns.len() + 1); // schema
    meta.begin_element();
    meta.string(4, b"schema");
    meta.i32(5, columns.len() as i32);
    meta.end_struct();
    for (name, column) in columns {
        let (physical_type, repetition, converted, logical_field) = match column {
            Column::Str(_) => (TYPE_BYTE_ARRAY, REQUIRED, Some(CONVERTED_UTF8), Some(1)),
            Column::OptStr(_) => (TYPE_BYTE_ARRAY, OPTIONAL, Some(CONVERTED_UTF8), Some(1)),
            Column::I64(_) => (TYPE_INT64, REQUIRED, None, None),
            Column::Date(_) => (TYPE_INT32, REQUIRED, Some(CONVERTED_DATE), Some(6)),
        };
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
    meta.i64(3, num_rows as i64);
    if chunks.is_empty() {
        meta.list(4, T_STRUCT, 0);
    } else {
        meta.list(4, T_STRUCT, 1); // one row group
        meta.begin_element();
        meta.list(1, T_STRUCT, chunks.len());
        for ((name, _), chunk) in columns.iter().zip(&chunks) {
            meta.begin_element(); // ColumnChunk
            meta.i64(2, chunk.offset as i64); // file_offset
            meta.begin_struct(3); // ColumnMetaData
            meta.i32(1, chunk.physical_type);
            meta.list(2, T_I32, 2);
            meta.element_i32(ENCODING_PLAIN);
            meta.element_i32(ENCODING_RLE);
            meta.list(3, T_BINARY, 1);
            meta.element_string(name.as_bytes());
            meta.i32(4, 0); // UNCOMPRESSED
            meta.i64(5, chunk.num_values as i64);
            meta.i64(6, chunk.size as i64);
            meta.i64(7, chunk.size as i64);
            meta.i64(9, chunk.offset as i64); // data_page_offset
            meta.end_struct();
            meta.end_struct();
        }
        let total: u64 = chunks.iter().map(|chunk| chunk.size).sum();
        meta.i64(2, total as i64);
        meta.i64(3, num_rows as i64);
        meta.end_struct();
    }
    meta.string(6, b"shopping-sessions pipeline");
    let meta = meta.finish();
    file.write_all(&meta)?;
    file.write_all(&(meta.len() as u32).to_le_bytes())?;
    file.write_all(b"PAR1")?;
    Ok(())
}
