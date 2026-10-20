use std::fs::File;
use std::io::{self, BufRead, BufReader, Read};
use std::path::Path;

use rustix::fs::{Mode, OFlags, open, openat};

use super::DIRECTORY_FLAGS;

const OBJECT_ID_LENGTHS: &[usize] = &[40, 64];
const MAX_HEADER_BYTES: u64 = 128;
const INVALID_BATCH: &str = "invalid git blob batch";

pub fn unpack(batch: &Path, stage: &Path, hashes: &[String]) -> io::Result<()> {
    let stage = open(stage, DIRECTORY_FLAGS, Mode::empty())?;
    let input = File::from(open(
        batch,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )?);
    let metadata = input.metadata()?;
    if !metadata.is_file() {
        return Err(io::Error::other(INVALID_BATCH));
    }
    let mut remaining = metadata.len();
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
        let header_text = std::str::from_utf8(&header).map_err(io::Error::other)?;
        let size = header_text
            .strip_prefix(&format!("{hash} blob "))
            .and_then(|value| value.strip_suffix('\n'))
            .and_then(|value| value.parse::<u64>().ok())
            .ok_or_else(|| {
                io::Error::other(format!(
                    "{INVALID_BATCH}: unexpected header {header_text:?} for {hash}"
                ))
            })?;
        remaining = remaining
            .checked_sub(header.len() as u64)
            .and_then(|remaining| remaining.checked_sub(size))
            .and_then(|remaining| remaining.checked_sub(1))
            .ok_or_else(|| io::Error::other(INVALID_BATCH))?;
        let mut output = File::from(openat(
            &stage,
            hash.as_str(),
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::RUSR | Mode::WUSR,
        )?);
        if io::copy(&mut input.by_ref().take(size), &mut output)? != size {
            return Err(io::Error::other(INVALID_BATCH));
        }
        let mut newline = [0];
        input.read_exact(&mut newline)?;
        if newline != *b"\n" {
            return Err(io::Error::other(INVALID_BATCH));
        }
    }
    if remaining != 0 || !input.fill_buf()?.is_empty() {
        return Err(io::Error::other(INVALID_BATCH));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{INVALID_BATCH, unpack};
    use std::fs;
    use tempfile::tempdir;
    use test_case::test_case;

    const HASH: &str = "0123456789012345678901234567890123456789";
    const CONTENT: &[u8] = b"binary\0bytes\xff\n";

    #[test]
    fn unpack_preserves_blob_bytes() {
        let dir = tempdir().unwrap();
        let batch = dir.path().join("batch");
        let mut bytes = format!("{HASH} blob {}\n", CONTENT.len()).into_bytes();
        bytes.extend_from_slice(CONTENT);
        bytes.push(b'\n');
        fs::write(&batch, bytes).unwrap();
        unpack(&batch, dir.path(), &[HASH.into()]).unwrap();
        assert_eq!(fs::read(dir.path().join(HASH)).unwrap(), CONTENT);
    }

    #[test_case("blob 100\nx\n" ; "truncated")]
    #[test_case("tree 1\nx\n" ; "wrong_type")]
    #[test_case("blob 1\nx!" ; "bad_separator")]
    #[test_case("blob 1\nx\nextra" ; "extra_data")]
    fn malformed_batches_are_refused(tail: &str) {
        let dir = tempdir().unwrap();
        let batch = dir.path().join("batch");
        fs::write(&batch, format!("{HASH} {tail}")).unwrap();
        assert!(
            unpack(&batch, dir.path(), &[HASH.into()])
                .unwrap_err()
                .to_string()
                .starts_with(INVALID_BATCH)
        );
    }
}
