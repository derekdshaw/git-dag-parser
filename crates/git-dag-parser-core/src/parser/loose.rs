//! Loose objects: `objects/xx/yyyy...`, each a zlib stream of
//! `<type> <size>\0<body>`.

use std::fs::File;
use std::io::Read;
use std::path::Path;

use anyhow::{Context, Result};
use flate2::read::ZlibDecoder;

use crate::error::ParseError;
use crate::models::ObjectKind;

/// Enough inflated bytes to hold any loose object header: the longest type
/// name is `commit` (6), plus a space, up to 20 digits of size, and the null.
const LOOSE_HEADER_MAX: usize = 32;

/// Read a loose object's type and size without inflating its body.
///
/// Only the first `LOOSE_HEADER_MAX` bytes of the zlib stream are inflated,
/// so the cost is constant no matter how large the object is.
pub fn read_loose_header(path: &Path) -> Result<(ObjectKind, u64)> {
    let file = File::open(path).context("Failed to open loose object")?;
    let mut decoder = ZlibDecoder::new(file);

    let mut prefix = [0u8; LOOSE_HEADER_MAX];
    let mut len = 0;
    let mut ended = false;
    while len < prefix.len() {
        // A short object inflates to fewer bytes than the buffer holds and
        // then ends. A stream that is corrupt within these bytes is an error;
        // damage further into a large body is not seen, since the body is
        // never inflated.
        let n = decoder
            .read(&mut prefix[len..])
            .map_err(|e| ParseError::DecompressionError(e.to_string()))?;
        if n == 0 {
            ended = true;
            break;
        }
        len += n;
    }

    let (kind, size, body_start) = parse_loose_header(&prefix[..len])?;
    // When the stream ended inside the prefix, the whole body is here. Some
    // zlib backends end a cut-short stream without an error, so its length
    // is the check that the object is complete.
    let body_len = (len - body_start) as u64;
    if ended && body_len != size {
        return Err(ParseError::InvalidObjectFormat(format!(
            "loose object body is {body_len} bytes, header says {size}"
        ))
        .into());
    }
    Ok((kind, size))
}

/// Read and inflate a whole loose object, returning its kind and body
pub fn read_loose_object(path: &Path) -> Result<(ObjectKind, Vec<u8>)> {
    let file = File::open(path).context("Failed to open loose object")?;
    let mut data = Vec::new();
    ZlibDecoder::new(file)
        .read_to_end(&mut data)
        .map_err(|e| ParseError::DecompressionError(e.to_string()))?;

    let (kind, size, body_start) = parse_loose_header(&data)?;
    let body = data.split_off(body_start);
    if body.len() as u64 != size {
        return Err(ParseError::InvalidObjectFormat(format!(
            "loose object body is {} bytes, header says {}",
            body.len(),
            size
        ))
        .into());
    }
    Ok((kind, body))
}

/// Split an inflated loose object header: `<type> <size>\0`, returning the
/// type, the size, and the offset where the body starts
fn parse_loose_header(data: &[u8]) -> Result<(ObjectKind, u64, usize)> {
    let null_pos = data
        .iter()
        .position(|&b| b == 0)
        .ok_or_else(|| ParseError::InvalidObjectFormat("missing null in header".into()))?;

    let header = std::str::from_utf8(&data[..null_pos])?;
    let (kind, size) = header
        .split_once(' ')
        .ok_or_else(|| ParseError::InvalidObjectFormat("missing space in header".into()))?;

    let kind = ObjectKind::from_name(kind)
        .ok_or_else(|| ParseError::InvalidObjectFormat(format!("unknown object type {kind}")))?;

    let size = size
        .parse()
        .map_err(|_| ParseError::InvalidObjectFormat("invalid size".into()))?;

    Ok((kind, size, null_pos + 1))
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::path::PathBuf;

    use flate2::write::ZlibEncoder;
    use flate2::Compression;

    use super::*;

    fn write_object(dir: &Path, name: &str, body: &[u8], truncate_to: Option<usize>) -> PathBuf {
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(body).unwrap();
        let mut bytes = encoder.finish().unwrap();
        if let Some(n) = truncate_to {
            bytes.truncate(n);
        }
        let path = dir.join(name);
        std::fs::write(&path, bytes).unwrap();
        path
    }

    #[test]
    fn parse_loose_header_reads_type_size_and_body_start() {
        let (kind, size, body_start) = parse_loose_header(b"blob 12\0hello world\n").unwrap();
        assert_eq!(kind, ObjectKind::Blob);
        assert_eq!(size, 12);
        assert_eq!(body_start, 8);

        let (kind, size, body_start) = parse_loose_header(b"commit 1234\0tree ...").unwrap();
        assert_eq!(kind, ObjectKind::Commit);
        assert_eq!(size, 1234);
        assert_eq!(body_start, 12);
    }

    #[test]
    fn parse_loose_header_rejects_malformed_headers() {
        // No null terminator within the prefix
        assert!(parse_loose_header(b"blob 12").is_err());
        // No space between type and size
        assert!(parse_loose_header(b"blob12\0body").is_err());
        // Size is not a number
        assert!(parse_loose_header(b"blob xy\0body").is_err());
        // Not a git object type
        assert!(parse_loose_header(b"blobby 1\0x").is_err());
    }

    #[test]
    fn read_loose_header_does_not_inflate_the_body() {
        let dir = tempfile::tempdir().unwrap();
        let mut body = b"blob 1048576\0".to_vec();
        body.extend(std::iter::repeat_n(b'x', 1024 * 1024));
        // Cut the stream short: inflating the whole object would fail, so this
        // only passes if just the header is read.
        let path = write_object(dir.path(), "truncated", &body, Some(64));

        let (kind, size) = read_loose_header(&path).unwrap();

        assert_eq!(kind, ObjectKind::Blob);
        assert_eq!(size, 1024 * 1024);
    }

    #[test]
    fn read_loose_header_reports_a_stream_that_breaks_after_the_header() {
        // A small object cut short: the header inflates on the first read and
        // the missing end of the stream is only seen on the next one.
        let dir = tempfile::tempdir().unwrap();
        let path = write_object(dir.path(), "cut", b"blob 5\0hello", None);
        let mut bytes = std::fs::read(&path).unwrap();
        bytes.truncate(bytes.len() - 6);
        std::fs::write(&path, bytes).unwrap();

        assert!(read_loose_header(&path).is_err());
    }

    #[test]
    fn read_loose_header_handles_objects_shorter_than_the_buffer() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_object(dir.path(), "tiny", b"blob 1\0x", None);

        let (kind, size) = read_loose_header(&path).unwrap();

        assert_eq!(kind, ObjectKind::Blob);
        assert_eq!(size, 1);
    }

    #[test]
    fn read_loose_object_returns_kind_and_body() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_object(dir.path(), "whole", b"tree 9\x00100644 xy", None);

        let (kind, body) = read_loose_object(&path).unwrap();

        assert_eq!(kind, ObjectKind::Tree);
        assert_eq!(body, b"100644 xy");
    }

    #[test]
    fn read_loose_object_rejects_body_size_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_object(dir.path(), "short", b"blob 5\0abc", None);

        assert!(read_loose_object(&path).is_err());
    }

    #[test]
    fn read_loose_header_reads_every_object_type() {
        let dir = tempfile::tempdir().unwrap();
        for (kind, body) in [
            (ObjectKind::Commit, &b"commit 5\0tree "[..]),
            (ObjectKind::Tree, &b"tree 7\x00100644 "[..]),
            (ObjectKind::Blob, &b"blob 3\0abc"[..]),
            (ObjectKind::Tag, &b"tag 9\0object ab"[..]),
        ] {
            let path = write_object(dir.path(), kind.name(), body, None);
            assert_eq!(read_loose_header(&path).unwrap().0, kind);
        }
    }
}
