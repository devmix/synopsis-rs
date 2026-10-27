//! SYNX binary fixture format (`vectors.bin`) — the vector-dump fixture contract.
//!
//! Layout (native-seam-spikes design D4, verbatim):
//!
//! ```text
//! magic    4 B   "SYNX"
//! version  u32 LE = 1
//! dim      u32 LE
//! count    u64 LE
//! rows     [u32 LE chunk_id][f32 LE x dim] x count, ascending chunk_id
//! ```
//!
//! All integers are little-endian. [`open`] validates the header and yields a
//! streaming [`SynxReader`] that consumes one row at a time (memory bounded by
//! one row plus the read buffer, so multi-GB fixture files are never loaded
//! whole). [`write`] buffers the rows, stably sorts them by ascending
//! `chunk_id` (format requirement) and streams them to any [`std::io::Write`].
//! Bytes beyond the promised `count` rows are ignored on read.

use std::io::{self, BufReader, Read, Write};

use crate::VectorsError;

/// Magic bytes that start every SYNX file.
pub const MAGIC: [u8; 4] = *b"SYNX";
/// The only format version this crate reads and writes.
pub const VERSION: u32 = 1;
/// Header size in bytes: magic (4) + version (4) + dim (4) + count (8).
pub const HEADER_LEN: usize = 20;
/// Read buffer capacity of [`SynxReader`]: files are consumed in chunks of this size.
pub const BUF_CAPACITY: usize = 8 * 1024;

/// Writes `rows` in SYNX format to `writer`.
///
/// `dim` is the header dimensionality; every row vector must have exactly
/// `dim` elements ([`VectorsError::DimensionMismatch`] otherwise). Rows are
/// stably sorted by ascending `chunk_id` before writing (format requirement),
/// so input may arrive in any order; equal ids keep their input order.
///
/// The sort buffers all row data in memory; the ~4 GB target files are
/// produced by the fixture writer, this writer serves fixture creation and tests.
pub fn write<W: Write, R: AsRef<[f32]>>(
    writer: &mut W,
    dim: u32,
    rows: impl IntoIterator<Item = (u32, R)>,
) -> Result<(), VectorsError> {
    if dim == 0 {
        return Err(VectorsError::InvalidArgument("dim must be > 0".to_string()));
    }
    let dim = dim as usize;
    let mut rows: Vec<(u32, Vec<f32>)> = rows
        .into_iter()
        .map(|(id, vector)| {
            let vector = vector.as_ref();
            if vector.len() != dim {
                return Err(VectorsError::DimensionMismatch {
                    expected: dim,
                    actual: vector.len(),
                });
            }
            Ok((id, vector.to_vec()))
        })
        .collect::<Result<Vec<_>, VectorsError>>()?;
    rows.sort_by_key(|(id, _)| *id); // stable: equal chunk_ids keep input order

    let mut header = [0u8; HEADER_LEN];
    header[..4].copy_from_slice(&MAGIC);
    header[4..8].copy_from_slice(&VERSION.to_le_bytes());
    header[8..12].copy_from_slice(&(dim as u32).to_le_bytes());
    header[12..].copy_from_slice(&(rows.len() as u64).to_le_bytes());
    writer.write_all(&header)?;

    let mut row = vec![0u8; 4 + dim * 4];
    for (id, vector) in &rows {
        row[..4].copy_from_slice(&id.to_le_bytes());
        for (slot, value) in row[4..].chunks_mut(4).zip(vector.iter().copied()) {
            slot.copy_from_slice(&value.to_le_bytes());
        }
        writer.write_all(&row)?;
    }
    writer.flush()?;
    Ok(())
}

/// Streaming reader over an open SYNX file.
///
/// Yields `(chunk_id, vector)` rows one at a time in file order (ascending
/// `chunk_id`); memory stays bounded by one row plus the read buffer, so
/// multi-GB fixture files can be consumed without loading them whole.
#[derive(Debug)]
pub struct SynxReader<R: Read> {
    input: BufReader<R>,
    dim: u32,
    total: u64,
    remaining: u64,
    row_buf: Vec<u8>,
}

/// Parses the SYNX header from `input` and returns a streaming row reader.
///
/// A malformed header fails with a distinct [`VectorsError`] variant:
/// [`SyNxBadMagic`], [`SyNxBadVersion`], [`SyNxZeroDim`] or [`SyNxTruncated`]
/// (mid-header). Rows are validated lazily by the iterator; a row that ends
/// before its promised bytes yields [`SyNxTruncated`] (mid-row).
///
/// [`SyNxBadMagic`]: VectorsError::SyNxBadMagic
/// [`SyNxBadVersion`]: VectorsError::SyNxBadVersion
/// [`SyNxZeroDim`]: VectorsError::SyNxZeroDim
/// [`SyNxTruncated`]: VectorsError::SyNxTruncated
pub fn open<R: Read>(input: R) -> Result<SynxReader<R>, VectorsError> {
    let mut input = BufReader::with_capacity(BUF_CAPACITY, input);
    let mut header = [0u8; HEADER_LEN];
    input
        .read_exact(&mut header)
        .map_err(|e| truncate_error(e, "header"))?;

    if header[..4] != MAGIC {
        return Err(VectorsError::SyNxBadMagic);
    }
    let version = le_u32(&header[4..8]);
    if version != VERSION {
        return Err(VectorsError::SyNxBadVersion(version));
    }
    let dim = le_u32(&header[8..12]);
    if dim == 0 {
        return Err(VectorsError::SyNxZeroDim);
    }
    let total = le_u64(&header[12..]);

    Ok(SynxReader {
        input,
        dim,
        total,
        remaining: total,
        row_buf: vec![0u8; 4 + dim as usize * 4],
    })
}

impl<R: Read> SynxReader<R> {
    /// Vector dimensionality from the header.
    pub fn dim(&self) -> u32 {
        self.dim
    }

    /// Total number of rows promised by the header.
    ///
    /// Named `row_count` because a `count` method would be shadowed by
    /// [`Iterator::count`] in method resolution (by-value step wins).
    pub fn row_count(&self) -> u64 {
        self.total
    }

    fn read_row(&mut self, index: u64) -> Result<(u32, Vec<f32>), VectorsError> {
        self.input
            .read_exact(&mut self.row_buf)
            .map_err(|e| truncate_error(e, &format!("row {index} of {}", self.total)))?;
        let id = le_u32(&self.row_buf[..4]);
        let vector = self.row_buf[4..].chunks(4).map(le_f32).collect();
        Ok((id, vector))
    }
}

impl<R: Read> Iterator for SynxReader<R> {
    type Item = Result<(u32, Vec<f32>), VectorsError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            return None;
        }
        self.remaining -= 1;
        let index = self.total - self.remaining; // 1-based row number
        Some(self.read_row(index))
    }
}

/// Maps a `read_exact` failure: a short file is a format error
/// ([`VectorsError::SyNxTruncated`]), anything else is propagated as I/O.
fn truncate_error(error: io::Error, where_: &str) -> VectorsError {
    if error.kind() == io::ErrorKind::UnexpectedEof {
        VectorsError::SyNxTruncated(where_.to_string())
    } else {
        VectorsError::Io(error)
    }
}

fn le_u32(bytes: &[u8]) -> u32 {
    u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

fn le_u64(bytes: &[u8]) -> u64 {
    u64::from_le_bytes([
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
    ])
}

fn le_f32(bytes: &[u8]) -> f32 {
    f32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

#[cfg(test)]
mod tests {
    // Test code: unwrap/expect are intentional (fixtures are under our control).
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use std::io::Cursor;

    type Row = (u32, Vec<f32>);

    fn write_bytes(dim: u32, rows: &[Row]) -> Vec<u8> {
        let mut bytes = Vec::new();
        write(
            &mut bytes,
            dim,
            rows.iter().map(|(id, vector)| (*id, vector.as_slice())),
        )
        .expect("write to Vec<u8> cannot fail");
        bytes
    }

    fn read_all(bytes: Vec<u8>) -> Vec<Row> {
        open(Cursor::new(bytes))
            .expect("header parses")
            .collect::<Result<Vec<Row>, _>>()
            .expect("rows parse")
    }

    #[test]
    fn roundtrip_preserves_rows_and_sorts_by_chunk_id() {
        let rows: Vec<Row> = vec![
            (7, vec![1.0f32, -2.0]),
            (1, vec![0.5, 0.25]),
            (3, vec![-1.5, 3.0]),
        ];
        let got = read_all(write_bytes(2, &rows));
        assert_eq!(
            got,
            vec![
                (1, vec![0.5, 0.25]),
                (3, vec![-1.5, 3.0]),
                (7, vec![1.0, -2.0]),
            ]
        );
    }

    #[test]
    fn writer_sorts_stably_for_duplicate_chunk_ids() {
        let rows: Vec<Row> = vec![
            (5, vec![1.0f32]),
            (3, vec![2.0]),
            (5, vec![3.0]),
            (1, vec![4.0]),
        ];
        let got = read_all(write_bytes(1, &rows));
        // The two id=5 rows keep their input order (stable sort).
        assert_eq!(
            got,
            vec![
                (1, vec![4.0]),
                (3, vec![2.0]),
                (5, vec![1.0]),
                (5, vec![3.0]),
            ]
        );
    }

    #[test]
    fn golden_bytes_match_hand_written_reference() {
        let bytes = write_bytes(2, &[(7, vec![1.5f32, -2.0])]);
        let expected = [
            0x53, 0x59, 0x4e, 0x58, // magic "SYNX"
            0x01, 0x00, 0x00, 0x00, // version 1, LE
            0x02, 0x00, 0x00, 0x00, // dim 2, LE
            0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // count 1, LE
            0x07, 0x00, 0x00, 0x00, // chunk_id 7, LE
            0x00, 0x00, 0xc0, 0x3f, // 1.5f32, LE
            0x00, 0x00, 0x00, 0xc0, // -2.0f32, LE
        ];
        assert_eq!(bytes, expected);
    }

    #[test]
    fn truncated_header_is_rejected() {
        let full = write_bytes(2, &[(1, vec![0.0f32, 0.0])]);
        assert!(full.len() > HEADER_LEN);
        for len in 0..HEADER_LEN {
            let truncated = &full[..len];
            match open(Cursor::new(truncated)) {
                Err(VectorsError::SyNxTruncated(where_)) => assert_eq!(where_, "header"),
                other => panic!("len {len}: expected SyNxTruncated, got {other:?}"),
            }
        }
    }

    #[test]
    fn truncated_row_is_rejected() {
        let full = write_bytes(2, &[(1, vec![0.0f32, 0.0]), (2, vec![0.0, 0.0])]);
        let row_len = (full.len() - HEADER_LEN) / 2; // 12 bytes per row at dim=2
        for cut in 1..=row_len {
            let mut truncated = full.clone();
            truncated.truncate(full.len() - cut);
            let mut reader = open(Cursor::new(truncated)).expect("header parses");
            let first = reader
                .next()
                .expect("first row slot")
                .expect("first row parses");
            assert_eq!(first.0, 1);
            match reader.next().expect("second row slot") {
                Err(VectorsError::SyNxTruncated(where_)) => {
                    assert_eq!(where_, "row 2 of 2");
                }
                other => panic!("cut {cut}: expected truncated row, got {other:?}"),
            }
            assert!(reader.next().is_none());
        }
    }

    #[test]
    fn bad_magic_is_rejected() {
        let mut bytes = write_bytes(2, &[(1, vec![0.0f32, 0.0])]);
        bytes[..4].copy_from_slice(b"SNXY");
        match open(Cursor::new(bytes)) {
            Err(VectorsError::SyNxBadMagic) => {}
            other => panic!("expected SyNxBadMagic, got {other:?}"),
        }
    }

    #[test]
    fn bad_version_is_rejected() {
        let mut bytes = write_bytes(2, &[(1, vec![0.0f32, 0.0])]);
        bytes[4..8].copy_from_slice(&2u32.to_le_bytes());
        match open(Cursor::new(bytes)) {
            Err(VectorsError::SyNxBadVersion(version)) => assert_eq!(version, 2),
            other => panic!("expected SyNxBadVersion, got {other:?}"),
        }
    }

    #[test]
    fn zero_dim_is_rejected() {
        let mut header = [0u8; HEADER_LEN];
        header[..4].copy_from_slice(&MAGIC);
        header[4..8].copy_from_slice(&VERSION.to_le_bytes());
        // dim (bytes 8..12) and count (bytes 12..20) stay zero.
        match open(Cursor::new(header)) {
            Err(VectorsError::SyNxZeroDim) => {}
            other => panic!("expected SyNxZeroDim, got {other:?}"),
        }
    }

    #[test]
    fn writer_rejects_dim_mismatch() {
        let mut bytes = Vec::new();
        match write(&mut bytes, 3, [(1, vec![0.0f32, 0.0])]) {
            Err(VectorsError::DimensionMismatch { expected, actual }) => {
                assert_eq!((expected, actual), (3, 2));
            }
            other => panic!("expected DimensionMismatch, got {other:?}"),
        }
        assert!(bytes.is_empty());
    }

    #[test]
    fn writer_rejects_zero_dim() {
        let mut bytes = Vec::new();
        let rows: [(u32, Vec<f32>); 0] = [];
        match write(&mut bytes, 0, rows) {
            Err(VectorsError::InvalidArgument(message)) => assert!(message.contains("dim")),
            other => panic!("expected InvalidArgument, got {other:?}"),
        }
    }

    #[test]
    fn reader_streams_files_larger_than_the_internal_buffer() {
        let dim = 3000; // one row = 4 + 3000*4 = 12004 bytes > BUF_CAPACITY
        let count = 3;
        let rows: Vec<Row> = (0..count)
            .map(|i| {
                let vector = (0..dim)
                    .map(|j| ((i * 1000 + j) % 97) as f32 / 10.0f32)
                    .collect();
                (i as u32, vector)
            })
            .collect();
        let bytes = write_bytes(dim as u32, &rows);
        // The file is larger than the reader buffer and so is every row:
        // the reader must survive multiple buffer refills and partial rows.
        assert!(bytes.len() > BUF_CAPACITY);
        let reader = open(Cursor::new(bytes)).expect("header parses");
        assert_eq!(reader.dim() as usize, dim);
        assert_eq!(reader.row_count(), count as u64);
        let got = reader.collect::<Result<Vec<Row>, _>>().expect("rows parse");
        assert_eq!(got, rows);
    }

    #[test]
    fn empty_file_has_no_rows() {
        let mut bytes = Vec::new();
        write(&mut bytes, 4, std::iter::empty::<(u32, Vec<f32>)>()).expect("write");
        assert_eq!(bytes.len(), HEADER_LEN);
        let mut reader = open(Cursor::new(bytes)).expect("header parses");
        assert_eq!(reader.dim(), 4);
        assert_eq!(reader.row_count(), 0);
        assert!(reader.next().is_none());
    }
}
