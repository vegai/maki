use std::io::{self, BufRead, BufReader, Read};
use std::path::Path;
use std::str::from_utf8;

use rustix::fs::CWD;

use super::OBJECT_ID_LENGTHS;
use super::fd::{create_private, directory, open_regular};

const MAX_HEADER_BYTES: u64 = 128;
const INVALID_BATCH: &str = "invalid git blob batch";

pub fn unpack(batch: &Path, stage: &Path, hashes: &[String]) -> io::Result<()> {
    let stage = directory(&CWD, stage)?;
    let input = open_regular(&CWD, batch)?;
    let mut input = BufReader::new(input);
    for hash in hashes {
        if !OBJECT_ID_LENGTHS.contains(&hash.len())
            || !hash.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(io::Error::other(INVALID_BATCH));
        }
        let mut header = Vec::new();
        input
            .by_ref()
            .take(MAX_HEADER_BYTES)
            .read_until(b'\n', &mut header)?;
        let header_text = from_utf8(&header).map_err(io::Error::other)?;
        let size = header_text
            .strip_prefix(&format!("{hash} blob "))
            .and_then(|value| value.strip_suffix('\n'))
            .and_then(|value| value.parse::<u64>().ok())
            .ok_or_else(|| {
                io::Error::other(format!(
                    "{INVALID_BATCH}: unexpected header {header_text:?} for {hash}"
                ))
            })?;
        let mut output = create_private(&stage, hash.as_str())?;
        if io::copy(&mut input.by_ref().take(size), &mut output)? != size {
            return Err(io::Error::other(INVALID_BATCH));
        }
        let mut newline = [0];
        input.read_exact(&mut newline)?;
        if newline != *b"\n" {
            return Err(io::Error::other(INVALID_BATCH));
        }
    }
    if !input.fill_buf()?.is_empty() {
        return Err(io::Error::other(INVALID_BATCH));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{INVALID_BATCH, unpack};
    use std::fs;
    use std::io::ErrorKind;
    use tempfile::tempdir;
    use test_case::test_case;

    const HASH: &str = "0123456789012345678901234567890123456789";
    const SHA256_HASH: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const CONTENT: &[u8] = b"binary\0bytes\xff\n";
    const ENTRY: &str = "blob 1\nx\n";

    #[test_case(HASH ; "sha1")]
    #[test_case(SHA256_HASH ; "sha256")]
    fn unpack_preserves_blob_bytes(hash: &str) {
        let dir = tempdir().unwrap();
        let batch = dir.path().join("batch");
        let mut bytes = format!("{hash} blob {}\n", CONTENT.len()).into_bytes();
        bytes.extend_from_slice(CONTENT);
        bytes.push(b'\n');
        fs::write(&batch, bytes).unwrap();
        unpack(&batch, dir.path(), &[hash.into()]).unwrap();
        assert_eq!(fs::read(dir.path().join(hash)).unwrap(), CONTENT);
    }

    #[test_case(HASH, "blob 100\nx\n" ; "truncated")]
    #[test_case(HASH, "tree 1\nx\n" ; "wrong_type")]
    #[test_case(HASH, "blob 1\nx!" ; "bad_separator")]
    #[test_case(HASH, "blob 1\nx\nextra" ; "extra_data")]
    #[test_case(&HASH[1..], ENTRY ; "wrong_length_hash")]
    #[test_case(&HASH.replace('0', "g"), ENTRY ; "non_hex_hash")]
    fn malformed_batches_are_refused(hash: &str, tail: &str) {
        let dir = tempdir().unwrap();
        let batch = dir.path().join("batch");
        fs::write(&batch, format!("{hash} {tail}")).unwrap();
        assert!(
            unpack(&batch, dir.path(), &[hash.into()])
                .unwrap_err()
                .to_string()
                .starts_with(INVALID_BATCH)
        );
    }

    #[test]
    fn a_repeated_object_id_cannot_overwrite_its_stage_file() {
        let dir = tempdir().unwrap();
        let batch = dir.path().join("batch");
        fs::write(&batch, format!("{HASH} {ENTRY}").repeat(2)).unwrap();
        let error = unpack(&batch, dir.path(), &[HASH.into(), HASH.into()]).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::AlreadyExists);
    }
}
