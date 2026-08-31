//! Sidecar key manifest codec (usearch-wal-persistence task 3.2, ADR 0004 §3).
//!
//! The manifest is the durable record of every key stored in a layer (the
//! usearch core exposes no key-enumeration API, so the manifest is the only
//! key listing). The engine's on-disk layout (task 3.3) writes the manifest
//! next to every index file and reads it back on `create`/`open`/`save`.

use std::path::Path;

use crate::VectorsError;

/// Magic for the sidecar key manifest: the ASCII bytes `"SKEY"` as a
/// little-endian u32 (ADR 0004 §3: `magic u32 LE = 0x534B4559`).
const KEYS_MAGIC: u32 = 0x53_4B_45_59;
/// Manifest header size in bytes: magic u32 + count u32.
const KEYS_HEADER_LEN: usize = 8;
/// Size of one key record in bytes.
const KEYS_KEY_LEN: usize = 4;

/// Reads one little-endian u32 at `offset` from `bytes`.
///
/// The caller must guarantee `offset + 4 <= bytes.len()`: [`read_keys`]
/// validates the total file length before any record is read.
fn keys_u32_at(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ])
}

/// Serializes `keys` to the sidecar key manifest format (ADR 0004 §3)
/// and writes it to `path` atomically: the payload is written to a
/// sibling `<stem>.tmp` file, fsynced, and renamed over `path` — atomic
/// on the same filesystem, so a crash mid-write leaves the previous
/// manifest intact and only a temp file, which startup garbage cleanup
/// removes (ADR 0004 §8).
///
/// Format: `magic u32 LE` ([`KEYS_MAGIC`]), `count u32 LE`, then
/// `count ×` key `u32 LE`.
pub(super) fn write_keys(path: &Path, keys: &[u32]) -> Result<(), VectorsError> {
    let count = u32::try_from(keys.len()).map_err(|_| {
        VectorsError::InvalidArgument(format!(
            "key manifest holds {} keys, more than the u32 count field can name",
            keys.len()
        ))
    })?;
    let mut bytes = Vec::with_capacity(KEYS_HEADER_LEN + keys.len() * KEYS_KEY_LEN);
    bytes.extend_from_slice(&KEYS_MAGIC.to_le_bytes());
    bytes.extend_from_slice(&count.to_le_bytes());
    for &key in keys {
        bytes.extend_from_slice(&key.to_le_bytes());
    }
    let tmp = path.with_extension("tmp");
    let mut file = std::fs::File::create(&tmp)?;
    let completed = std::io::Write::write_all(&mut file, &bytes)
        .and_then(|()| file.sync_data())
        .and_then(|()| std::fs::rename(&tmp, path).map(drop));
    if let Err(io_err) = completed {
        // A failed write leaves no half-manifest: the temp file is the
        // only artifact and it is removed here (startup cleanup would
        // pick it up regardless — ADR 0004 §8).
        let _ = std::fs::remove_file(&tmp);
        return Err(VectorsError::Io(io_err));
    }
    Ok(())
}

/// Reads and validates the sidecar key manifest at `path` (the inverse
/// of [`write_keys`]).
///
/// Returns the manifest's keys in file order. Failures are distinct and
/// never panic:
///
/// - missing file → [`VectorsError::NotFound`] (the payload is `path`);
/// - a file shorter than the 8-byte header, or fewer key bytes than the
///   declared count → [`VectorsError::KeysTruncated`];
/// - a magic other than [`KEYS_MAGIC`] → [`VectorsError::KeysBadMagic`];
/// - trailing bytes after the declared key count →
///   [`VectorsError::KeysTrailingBytes`].
pub(super) fn read_keys(path: &Path) -> Result<Vec<u32>, VectorsError> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Err(VectorsError::NotFound(path.display().to_string()));
        }
        Err(err) => return Err(VectorsError::Io(err)),
    };
    if bytes.len() < KEYS_HEADER_LEN {
        return Err(VectorsError::KeysTruncated(format!(
            "{}: header needs {} bytes, file is {} bytes",
            path.display(),
            KEYS_HEADER_LEN,
            bytes.len()
        )));
    }
    if keys_u32_at(&bytes, 0) != KEYS_MAGIC {
        return Err(VectorsError::KeysBadMagic);
    }
    let count = keys_u32_at(&bytes, 4);
    // Checked arithmetic: a corrupt count field must not overflow the
    // length computation.
    let expected_len = usize::try_from(count)
        .ok()
        .and_then(|c| {
            c.checked_mul(KEYS_KEY_LEN)
                .and_then(|n| n.checked_add(KEYS_HEADER_LEN))
        })
        .ok_or_else(|| {
            VectorsError::KeysTruncated(format!(
                "{}: declared key count {} overflows the addressable file size",
                path.display(),
                count
            ))
        })?;
    if bytes.len() < expected_len {
        return Err(VectorsError::KeysTruncated(format!(
            "{}: declares {} keys ({} payload bytes) but the file is only {} bytes",
            path.display(),
            count,
            expected_len - KEYS_HEADER_LEN,
            bytes.len()
        )));
    }
    if bytes.len() > expected_len {
        return Err(VectorsError::KeysTrailingBytes(
            bytes.len() - expected_len,
            count,
        ));
    }
    let mut keys = Vec::with_capacity(count as usize);
    for i in 0..count as usize {
        keys.push(keys_u32_at(&bytes, KEYS_HEADER_LEN + i * KEYS_KEY_LEN));
    }
    Ok(keys)
}

#[cfg(test)]
mod keys_tests {
    //! Sidecar key manifest codec tests (usearch-wal-persistence task 3.2).

    // Test code: unwrap/expect are intentional (the fixtures are deterministic).
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::super::test_util::TempDir;
    use super::*;

    /// The on-disk spelling of the `"SKEY"` magic (0x534B4559, little-endian).
    const MAGIC_BYTES: [u8; 4] = [0x59, 0x45, 0x4B, 0x53];

    #[test]
    fn round_trip_zero_keys() {
        let dir = TempDir::new("zero");
        let path = dir.0.join("segment-1.keys");
        write_keys(&path, &[]).unwrap();
        assert_eq!(read_keys(&path).unwrap(), Vec::<u32>::new());
        // Exact byte layout of the empty manifest: magic LE + zero count.
        assert_eq!(
            std::fs::read(&path).unwrap(),
            [
                MAGIC_BYTES[0],
                MAGIC_BYTES[1],
                MAGIC_BYTES[2],
                MAGIC_BYTES[3],
                0,
                0,
                0,
                0
            ]
        );
    }

    #[test]
    fn round_trip_single_key() {
        let dir = TempDir::new("single");
        let path = dir.0.join("ram.keys");
        write_keys(&path, &[42]).unwrap();
        assert_eq!(read_keys(&path).unwrap(), vec![42]);
        // Exact byte layout: magic LE + count 1 LE + key 42 LE.
        assert_eq!(
            std::fs::read(&path).unwrap(),
            [
                MAGIC_BYTES[0],
                MAGIC_BYTES[1],
                MAGIC_BYTES[2],
                MAGIC_BYTES[3],
                1,
                0,
                0,
                0,
                42,
                0,
                0,
                0
            ]
        );
    }

    #[test]
    fn round_trip_many_keys() {
        let dir = TempDir::new("many");
        let path = dir.0.join("segment-2.keys");
        let keys: Vec<u32> = (0..1000).rev().collect();
        write_keys(&path, &keys).unwrap();
        assert_eq!(read_keys(&path).unwrap(), keys);
    }

    #[test]
    fn read_missing_file_is_not_found() {
        let dir = TempDir::new("missing");
        let path = dir.0.join("nope.keys");
        match read_keys(&path) {
            Err(VectorsError::NotFound(payload)) => {
                assert!(
                    payload.contains("nope.keys"),
                    "the error must name the path: {payload}"
                );
            }
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    #[test]
    fn read_bad_magic_is_distinct_error() {
        let dir = TempDir::new("bad-magic");
        let path = dir.0.join("bad.keys");
        let mut bytes = [0u8; 8];
        bytes[0..4].copy_from_slice(&MAGIC_BYTES);
        bytes[0] = 0x00; // corrupt the magic
        std::fs::write(&path, bytes).unwrap();
        assert!(
            matches!(read_keys(&path), Err(VectorsError::KeysBadMagic)),
            "a corrupted magic must be a distinct KeysBadMagic error, not a panic"
        );
    }

    #[test]
    fn read_truncated_payload_is_distinct_error() {
        let dir = TempDir::new("truncated");
        // The header declares 3 keys but only 2 key records are present.
        let path = dir.0.join("short.keys");
        std::fs::write(
            &path,
            [
                MAGIC_BYTES[0],
                MAGIC_BYTES[1],
                MAGIC_BYTES[2],
                MAGIC_BYTES[3],
                3,
                0,
                0,
                0,
                1,
                0,
                0,
                0,
                2,
                0,
                0,
                0,
            ],
        )
        .unwrap();
        assert!(
            matches!(read_keys(&path), Err(VectorsError::KeysTruncated(_))),
            "a short payload must be a distinct KeysTruncated error, not a panic"
        );
        // A file shorter than the 8-byte header is truncated as well.
        let headerless = dir.0.join("headerless.keys");
        std::fs::write(&headerless, [MAGIC_BYTES[0], MAGIC_BYTES[1]]).unwrap();
        assert!(matches!(
            read_keys(&headerless),
            Err(VectorsError::KeysTruncated(_))
        ));
    }

    #[test]
    fn read_trailing_bytes_is_distinct_error() {
        let dir = TempDir::new("trailing");
        let path = dir.0.join("extra.keys");
        let mut bytes = vec![
            MAGIC_BYTES[0],
            MAGIC_BYTES[1],
            MAGIC_BYTES[2],
            MAGIC_BYTES[3],
            1,
            0,
            0,
            0,
            7,
            0,
            0,
            0,
        ];
        bytes.push(0xFF); // one byte beyond the declared count
        std::fs::write(&path, &bytes).unwrap();
        assert!(
            matches!(read_keys(&path), Err(VectorsError::KeysTrailingBytes(1, 1))),
            "trailing bytes must be a distinct KeysTrailingBytes error, not a panic"
        );
    }

    #[test]
    fn write_leaves_no_tmp_residue() {
        let dir = TempDir::new("no-residue");
        let path = dir.0.join("segment-3.keys");
        write_keys(&path, &[1, 2, 3]).unwrap();
        let entries: Vec<String> = std::fs::read_dir(&dir.0)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            entries,
            vec!["segment-3.keys".to_string()],
            "a successful write must leave only the manifest itself: {entries:?}"
        );
    }

    #[test]
    fn rewrite_is_atomic_replace() {
        // A second write over an existing manifest replaces it cleanly (the
        // rename target already exists) and the content converges.
        let dir = TempDir::new("rewrite");
        let path = dir.0.join("ram.keys");
        write_keys(&path, &[1, 2, 3]).unwrap();
        write_keys(&path, &[9]).unwrap();
        assert_eq!(read_keys(&path).unwrap(), vec![9]);
    }
}
