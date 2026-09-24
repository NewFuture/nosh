//! Streaming SHA-256 helpers.

use std::fs::File;
use std::io::Read;
use std::path::Path;

use sha2::{Digest, Sha256};

pub fn to_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0xf) as usize] as char);
    }
    s
}

pub fn finalize_hex(h: Sha256) -> String {
    to_hex(&h.finalize())
}

/// Feeds `len` bytes of `reader` into `hasher`, reporting progress in bytes.
pub fn hash_reader(
    reader: &mut impl Read,
    hasher: &mut Sha256,
    len: Option<u64>,
    mut on_progress: impl FnMut(u64),
) -> std::io::Result<u64> {
    let mut buf = vec![0u8; 1 << 20];
    let mut done = 0u64;
    loop {
        let want = match len {
            Some(l) if done >= l => break,
            Some(l) => ((l - done) as usize).min(buf.len()),
            None => buf.len(),
        };
        let n = reader.read(&mut buf[..want])?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        done += n as u64;
        on_progress(done);
    }
    Ok(done)
}

pub fn sha256_file(path: &Path, on_progress: impl FnMut(u64)) -> std::io::Result<String> {
    let mut f = File::open(path)?;
    let mut h = Sha256::new();
    hash_reader(&mut f, &mut h, None, on_progress)?;
    Ok(finalize_hex(h))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_vectors() {
        let mut h = Sha256::new();
        h.update(b"abc");
        assert_eq!(
            finalize_hex(h),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            finalize_hex(Sha256::new()),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn hashes_prefix_only() {
        let data = b"hello world";
        let mut h = Sha256::new();
        let n = hash_reader(&mut &data[..], &mut h, Some(5), |_| {}).unwrap();
        assert_eq!(n, 5);
        let mut h2 = Sha256::new();
        h2.update(b"hello");
        assert_eq!(finalize_hex(h), finalize_hex(h2));
    }
}
