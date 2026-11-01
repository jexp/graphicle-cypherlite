//! Compact-Thrift footer and page preflight before Parquet decoding (ADR-0086).
use std::io::{self, Read, SeekFrom};
use std::sync::{Arc, Mutex};

use super::*;
use bytes::Bytes;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use parquet::file::reader::{ChunkReader, Length};

use crate::vfs::{Vfs, VfsFile, VfsOpen};

pub(super) fn invalid(what: &str) -> Error {
    Error::storage(format!("invalid or out-of-bounds {what} (ADR-0086)"))
}

pub(super) struct Compact<'a> {
    data: &'a [u8],
    pub(super) pos: usize,
    // Footer lists are larger than page-header lists. Bound the *sum* of their
    // reservations as well as each count: parquet 54.3.1 uses Vec::with_capacity
    // on list sizes before it reads the first element.
    metadata_entries: Option<u64>,
}

impl Compact<'_> {
    pub(super) fn page(data: &[u8]) -> Compact<'_> {
        Compact {
            data,
            pos: 0,
            metadata_entries: None,
        }
    }

    fn metadata(data: &[u8]) -> Compact<'_> {
        Compact {
            data,
            pos: 0,
            // A page's column has several nested metadata lists (encodings,
            // schema path, etc.); leave room for them across row groups.
            // Individual lists remain capped at MAX_PAGES entries.
            metadata_entries: Some(MAX_PAGES * 16),
        }
    }

    pub(super) fn byte(&mut self) -> Result<u8> {
        let b = *self
            .data
            .get(self.pos)
            .ok_or_else(|| invalid("page header"))?;
        self.pos += 1;
        Ok(b)
    }

    fn varint(&mut self) -> Result<u64> {
        let mut result = 0u64;
        for shift in (0..=63).step_by(7) {
            let b = self.byte()?;
            if shift == 63 && b > 1 {
                return Err(invalid("Thrift varint"));
            }
            result |= u64::from(b & 0x7f) << shift;
            if b & 0x80 == 0 {
                return Ok(result);
            }
        }
        Err(invalid("Thrift varint"))
    }

    pub(super) fn integer(&mut self) -> Result<i64> {
        let v = self.varint()?;
        Ok(((v >> 1) as i64) ^ (-((v & 1) as i64)))
    }

    fn collection_count(&mut self, count: u64, width: u64) -> Result<()> {
        if let Some(remaining) = &mut self.metadata_entries {
            // Each compact list element consumes at least one byte (including
            // an empty struct's STOP), and each map pair at least two.
            if count > MAX_PAGES
                || count > *remaining
                || count > (self.data.len() - self.pos) as u64 / width
            {
                return Err(invalid("footer Thrift collection count"));
            }
            *remaining -= count;
        } else if count > 1024 {
            return Err(invalid("Thrift container count"));
        }
        Ok(())
    }

    pub(super) fn skip(&mut self, ty: u8, depth: usize) -> Result<()> {
        if depth > 16 {
            return Err(invalid("Thrift nesting"));
        }
        match ty {
            1 | 2 => Ok(()), // bool encoded in field type
            3 => {
                self.byte()?;
                Ok(())
            }
            4..=6 => {
                self.varint()?;
                Ok(())
            }
            7 => self.advance(8),
            8 => {
                let n = usize::try_from(self.varint()?).map_err(|_| invalid("Thrift binary"))?;
                self.advance(n)
            }
            12 => self.fields(depth + 1, |c, _, ty, depth| c.skip(ty, depth)),
            9 | 10 => {
                let head = self.byte()?;
                let count = if head >> 4 == 15 {
                    self.varint()?
                } else {
                    u64::from(head >> 4)
                };
                self.collection_count(count, 1)?;
                for _ in 0..count {
                    if head & 15 == 1 {
                        self.byte()?;
                    } else {
                        self.skip(head & 15, depth + 1)?;
                    }
                }
                Ok(())
            }
            11 => {
                let count = self.varint()?;
                self.collection_count(count, 2)?;
                if count != 0 {
                    let types = self.byte()?;
                    for _ in 0..count {
                        if types >> 4 == 1 {
                            self.byte()?;
                        } else {
                            self.skip(types >> 4, depth + 1)?;
                        }
                        if types & 15 == 1 {
                            self.byte()?;
                        } else {
                            self.skip(types & 15, depth + 1)?;
                        }
                    }
                }
                Ok(())
            }
            _ => Err(invalid("Thrift field type")),
        }
    }

    fn advance(&mut self, n: usize) -> Result<()> {
        self.pos = self
            .pos
            .checked_add(n)
            .filter(|end| *end <= self.data.len())
            .ok_or_else(|| invalid("page header length"))?;
        Ok(())
    }

    pub(super) fn fields(
        &mut self,
        depth: usize,
        mut field: impl FnMut(&mut Self, i64, u8, usize) -> Result<()>,
    ) -> Result<()> {
        if depth > 16 {
            return Err(invalid("Thrift nesting"));
        }
        let mut previous = 0i64;
        loop {
            let head = self.byte()?;
            if head == 0 {
                break;
            }
            let ty = head & 15;
            let id = if head >> 4 == 0 {
                self.integer()?
            } else {
                previous
                    .checked_add(i64::from(head >> 4))
                    .ok_or_else(|| invalid("Thrift field id"))?
            };
            if id <= 0 {
                return Err(invalid("Thrift field id"));
            }
            previous = id;
            field(self, id, ty, depth)?;
        }
        Ok(())
    }
}

pub(super) fn page_header(data: &[u8]) -> Result<(usize, u64, u64, u64)> {
    let mut c = Compact::page(data);
    let (mut kind, mut raw, mut packed) = (None, None, None);
    let mut details = None;
    c.fields(0, |c, id, ty, depth| {
        match (id, ty) {
            (1, 5) => kind = Some(c.integer()?),
            (2, 5) => raw = Some(c.integer()?),
            (3, 5) => packed = Some(c.integer()?),
            (5 | 7 | 8, 12) => {
                let mut fields = [None; 6];
                c.fields(depth + 1, |c, id, ty, depth| {
                    if (1..=6).contains(&id) && ty == 5 {
                        fields[(id - 1) as usize] = Some(c.integer()?);
                    } else {
                        c.skip(ty, depth)?;
                    }
                    Ok(())
                })?;
                details = Some((id, fields));
            }
            _ => c.skip(ty, depth)?,
        }
        Ok(())
    })?;
    let raw = bounded(
        raw.ok_or_else(|| invalid("page uncompressed size"))?,
        envelope().page,
        "page uncompressed size",
    )?;
    let packed = bounded(
        packed.ok_or_else(|| invalid("page compressed size"))?,
        envelope().page,
        "page compressed size",
    )?;
    // Parquet PageType: DATA_PAGE=0, INDEX_PAGE=1, DICTIONARY_PAGE=2, DATA_PAGE_V2=3.
    let values = match (kind, details) {
        (Some(0), Some((5, fields))) => bounded(
            fields[0].ok_or_else(|| invalid("page value count"))?,
            MAX_ROWS,
            "page value count",
        )?,
        (Some(2), Some((7, fields))) => {
            bounded(
                fields[0].ok_or_else(|| invalid("dictionary value count"))?,
                MAX_ROWS,
                "dictionary value count",
            )?;
            0
        }
        (Some(3), Some((8, fields))) => {
            let values = bounded(
                fields[0].ok_or_else(|| invalid("V2 value count"))?,
                MAX_ROWS,
                "V2 value count",
            )?;
            let nulls = bounded(
                fields[1].ok_or_else(|| invalid("V2 null count"))?,
                values,
                "V2 null count",
            )?;
            let rows = bounded(
                fields[2].ok_or_else(|| invalid("V2 row count"))?,
                MAX_ROWS,
                "V2 row count",
            )?;
            let rep = bounded(
                fields[5].ok_or_else(|| invalid("V2 repetition length"))?,
                envelope().page,
                "V2 repetition length",
            )?;
            let def = bounded(
                fields[4].ok_or_else(|| invalid("V2 definition length"))?,
                envelope().page,
                "V2 definition length",
            )?;
            if nulls > values
                || rows > values
                || rep.checked_add(def).is_none_or(|n| n > raw || n > packed)
            {
                return Err(invalid("V2 levels"));
            }
            values
        }
        (Some(1), _) => 0, // index pages are skipped by parquet's page reader
        _ => return Err(invalid("page type/header")),
    };
    Ok((c.pos, packed, raw, values))
}

/// A bounded random-access VFS object used by Parquet. Each range returns
/// a fresh cursor over the one shared, mutex-guarded handle. Every read
/// re-seeks under that lock, so cursors are position-independent but never
/// parallel, and a complete shard is not retained in heap memory.
#[derive(Clone)]
pub(crate) struct VfsChunkReader {
    file: Arc<Mutex<Box<dyn VfsFile>>>,
    len: u64,
}

impl VfsChunkReader {
    pub(crate) fn open(vfs: Arc<dyn Vfs>, path: &str, max_len: u64) -> Result<Self> {
        let metadata = vfs.metadata(path)?;
        if metadata.len > max_len {
            return Err(Error::storage(format!(
                "Parquet shard {path} exceeds {max_len} bytes"
            )));
        }
        let file = vfs.open(path, VfsOpen::read_only())?;
        let len = file.len()?;
        if len > max_len {
            return Err(Error::storage(format!(
                "Parquet shard {path} exceeds {max_len} bytes"
            )));
        }
        if len != metadata.len {
            return Err(Error::storage(format!(
                "Parquet shard {path} changed length during read"
            )));
        }
        Ok(Self {
            file: Arc::new(Mutex::new(file)),
            len,
        })
    }

    pub(crate) fn verify_len(&self) -> Result<()> {
        let len = self
            .file
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .len()?;
        if len != self.len {
            return Err(Error::storage("Parquet shard changed length during read"));
        }
        Ok(())
    }

    fn open_at(&self, offset: u64) -> io::Result<VfsRead> {
        if offset > self.len {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "Parquet range begins after EOF",
            ));
        }
        if self
            .file
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .len()
            .map_err(|error| io::Error::other(error.to_string()))?
            != self.len
        {
            return Err(io::Error::other("Parquet shard changed length during read"));
        }
        Ok(VfsRead {
            file: Arc::clone(&self.file),
            pos: offset,
            remaining: self.len - offset,
        })
    }

    fn range(&self, start: u64, len: usize) -> parquet::errors::Result<Bytes> {
        start
            .checked_add(len as u64)
            .filter(|&end| end <= self.len)
            .ok_or_else(|| {
                parquet::errors::ParquetError::EOF("Parquet range exceeds EOF".into())
            })?;
        let mut reader = self.open_at(start)?;
        let mut bytes = vec![0; len];
        reader.read_exact(&mut bytes)?;
        if self
            .file
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .len()
            .map_err(|error| io::Error::other(error.to_string()))?
            != self.len
        {
            return Err(parquet::errors::ParquetError::General(
                "Parquet shard changed length during read".into(),
            ));
        }
        Ok(bytes.into())
    }
}

impl Length for VfsChunkReader {
    fn len(&self) -> u64 {
        self.len
    }
}

impl ChunkReader for VfsChunkReader {
    type T = VfsRead;

    fn get_read(&self, start: u64) -> parquet::errors::Result<Self::T> {
        Ok(self.open_at(start)?)
    }

    fn get_bytes(&self, start: u64, length: usize) -> parquet::errors::Result<Bytes> {
        self.range(start, length)
    }
}

pub(crate) struct VfsRead {
    file: Arc<Mutex<Box<dyn VfsFile>>>,
    pos: u64,
    remaining: u64,
}

impl Read for VfsRead {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() || self.remaining == 0 {
            return Ok(0);
        }
        let len = usize::try_from(self.remaining)
            .unwrap_or(usize::MAX)
            .min(buf.len());
        let mut file = self
            .file
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        file.seek(SeekFrom::Start(self.pos))
            .map_err(|error| io::Error::other(error.to_string()))?;
        file.read_exact(&mut buf[..len])
            .map_err(|error| io::Error::other(error.to_string()))?;
        self.pos += len as u64;
        self.remaining -= len as u64;
        Ok(len)
    }
}

/// Check a shard's footer and pages against the ADR-0086 envelope, then
/// open a reader over `file`. The preflight and Arrow reader fetch bounded
/// ranges rather than retaining a complete shard.
pub(crate) fn preflight<T>(file: T) -> Result<ParquetRecordBatchReaderBuilder<T>>
where
    T: ChunkReader + Clone + 'static,
{
    let len = file.len();
    if len > MAX_FILE {
        return Err(invalid("1 GiB shard file"));
    }
    let footer = file
        .get_bytes(len.checked_sub(8).ok_or_else(|| invalid("footer"))?, 8)
        .map_err(|_| invalid("footer"))?;
    if &footer[4..] != b"PAR1" {
        return Err(invalid("footer magic"));
    }
    let meta_len =
        u32::from_le_bytes(footer[..4].try_into().map_err(|_| invalid("footer"))?) as usize;
    if meta_len > MAX_METADATA || meta_len.checked_add(8).is_none_or(|n| n > len as usize) {
        return Err(invalid("16 MiB footer metadata"));
    }
    // Walk the complete footer before Parquet's generated FileMetaData reader
    // reserves vectors from untrusted compact-Thrift list counts. Both loaders
    // and the writer pass through here before try_new.
    let metadata = file
        .get_bytes(len - 8 - meta_len as u64, meta_len)
        .map_err(|_| invalid("footer metadata"))?;
    let mut metadata = Compact::metadata(&metadata);
    metadata.fields(0, |c, _, ty, depth| c.skip(ty, depth))?;
    let builder = ParquetRecordBatchReaderBuilder::try_new(file.clone())
        .map_err(|e| Error::storage(e.to_string()))?;
    let meta = builder.metadata();
    let declared_rows = bounded(meta.file_metadata().num_rows(), MAX_ROWS, "file rows")?;
    let (mut rows, mut uncompressed, mut observed, mut pages) = (0, 0, 0, 0);
    for group in meta.row_groups() {
        let mut group_observed = 0;
        accumulate(
            &mut rows,
            bounded(group.num_rows(), MAX_ROWS, "row group rows")?,
            MAX_ROWS,
            "row count",
        )?;
        bounded(
            group.total_byte_size(),
            envelope().decoded,
            "row group bytes",
        )?;
        for col in group.columns() {
            bounded(col.num_values(), envelope().decoded, "column values")?;
            let declared = bounded(
                col.uncompressed_size(),
                envelope().decoded,
                "column uncompressed bytes",
            )?;
            accumulate(
                &mut uncompressed,
                declared,
                envelope().decoded,
                "column uncompressed total",
            )?;
            let size = bounded(col.compressed_size(), MAX_FILE, "column compressed bytes")?;
            let offset = bounded(
                col.dictionary_page_offset()
                    .unwrap_or_else(|| col.data_page_offset()),
                len,
                "column start",
            )?;
            bounded(col.data_page_offset(), len, "data page offset")?;
            if let Some(index) = col.index_page_offset() {
                bounded(index, len, "index page offset")?;
            }
            let end = offset
                .checked_add(size)
                .filter(|n| *n <= len)
                .ok_or_else(|| invalid("column range"))?;
            let data_offset = col.data_page_offset() as u64;
            if size != 0 && (data_offset < offset || data_offset >= end) {
                return Err(invalid("data page offset outside column"));
            }
            let mut pos = offset;
            let mut column_observed = 0;
            let mut column_values = 0;
            while pos < end {
                // ArrowWriter 54.3.1 may leave one encoded padding byte at the
                // end of a column. Only accept it after all declared values.
                if end - pos == 1 && column_values == col.num_values() as u64 {
                    break;
                }
                accumulate(&mut pages, 1, MAX_PAGES, "page count")?;
                let remaining = usize::try_from(end - pos).unwrap_or(usize::MAX);
                let header = file
                    .get_bytes(pos, remaining.min(MAX_HEADER))
                    .map_err(|_| invalid("page header"))?;
                let (head, packed, raw, values) = page_header(&header)?;
                if head > MAX_HEADER {
                    return Err(invalid("64 KiB page header"));
                }
                let page_end = pos
                    .checked_add(head as u64)
                    .and_then(|n| n.checked_add(packed))
                    .filter(|n| *n <= end)
                    .ok_or_else(|| invalid("page payload range"))?;
                pos = page_end;
                accumulate(
                    &mut column_observed,
                    raw,
                    envelope().decoded,
                    "column page bytes",
                )?;
                accumulate(
                    &mut observed,
                    raw,
                    envelope().decoded,
                    "page uncompressed total",
                )?;
                accumulate(
                    &mut group_observed,
                    raw,
                    envelope().decoded,
                    "row group page bytes",
                )?;
                accumulate(
                    &mut column_values,
                    values,
                    envelope().decoded,
                    "column page values",
                )?;
            }
            if column_observed > declared {
                return Err(invalid("underreported column page bytes"));
            }
            if column_values != col.num_values() as u64 {
                return Err(invalid("column page value count"));
            }
        }
        if group_observed > group.total_byte_size() as u64 {
            return Err(invalid("underreported row group page bytes"));
        }
    }
    if rows != declared_rows {
        return Err(invalid("file/row group count mismatch"));
    }
    Ok(builder.with_batch_size(1024))
}

#[cfg(test)]
pub(super) fn compact_integer(value: i64) -> Vec<u8> {
    let mut n = ((value as u64) << 1) ^ ((value >> 63) as u64);
    let mut encoded = Vec::new();
    while n >= 0x80 {
        encoded.push((n as u8) | 0x80);
        n >>= 7;
    }
    encoded.push(n as u8);
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_header_rejects_v2_levels_and_thrift_container_bombs() {
        // Compact Thrift: V2 page with five bytes of levels in four bytes
        // of compressed payload (field 8 contains the V2 header).
        let v2 = [
            0x15, 6, 0x15, 10, 0x15, 8, 0x5c, 0x15, 4, 0x15, 0, 0x15, 4, 0x15, 0, 0x15, 6, 0x15, 4,
            0, 0,
        ];
        assert!(page_header(&v2)
            .unwrap_err()
            .to_string()
            .contains("V2 levels"));

        // Unknown optional list with a forged count of 1025 is rejected
        // without allocating a collection or reading any of its elements.
        let container = [0x15, 0, 0x15, 2, 0x15, 2, 0x69, 0xf8, 0x81, 0x08, 0];
        assert!(page_header(&container)
            .unwrap_err()
            .to_string()
            .contains("Thrift container count"));
    }

    #[test]
    fn thrift_field_delta_overflow_returns_error() {
        // Unknown i32 field with explicit ID i64::MAX, then a delta-ID field.
        // The second ID used to overflow in debug builds before validation.
        let mut header = vec![0x05];
        header.extend(compact_integer(i64::MAX));
        header.push(0); // field value
        header.extend([0x15, 0]); // next field: delta 1, i32, value 0
        let err = page_header(&header).unwrap_err();
        assert!(err.to_string().contains("Thrift field id"), "{err}");
    }
}
