//! Resumable, verified, multi-source file download.
//!
//! Bytes go to `<name>.partial` with a streaming SHA-256; the file is renamed
//! into place only after size and hash match the registry. Requests are made in
//! ranged chunks so a stalled or failing source can be swapped for the next one
//! while keeping the downloaded offset.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

use crate::HubError;
use crate::hash;
use crate::net;
use crate::progress::Progress;
use crate::registry::FileEntry;
use crate::sources::{Candidate, Hub};

#[derive(Debug, Clone)]
pub struct DownloadOptions {
    /// Bytes per ranged request.
    pub chunk_size: u64,
    /// How many times each source may fail without progress before giving up.
    pub max_rounds: usize,
    pub per_call_timeout: Duration,
    pub backoff: Duration,
}

impl Default for DownloadOptions {
    fn default() -> Self {
        Self {
            chunk_size: 64 * 1024 * 1024,
            max_rounds: 3,
            per_call_timeout: Duration::from_secs(240),
            backoff: Duration::from_millis(500),
        }
    }
}

#[derive(Debug, Clone)]
pub struct DownloadOutcome {
    pub path: PathBuf,
    pub hub: Option<Hub>,
    pub resumed_from: u64,
    pub downloaded: u64,
}

pub fn partial_path(dir: &Path, file: &FileEntry) -> PathBuf {
    dir.join(format!("{}.partial", file.name))
}

/// Bytes needed on disk to finish downloading `file` into `dir`.
pub fn remaining_bytes(dir: &Path, file: &FileEntry) -> u64 {
    let have = fs::metadata(partial_path(dir, file))
        .map(|m| m.len())
        .unwrap_or(0);
    file.size.saturating_sub(have.min(file.size))
}

struct Lock {
    _file: File,
}

/// How often a download waiting for another process's lock re-checks it.
const LOCK_POLL: Duration = Duration::from_millis(200);

/// Takes `<name>.lock`, waiting while another process holds it; `cancelled`
/// is checked between attempts so Ctrl-C ends the wait.
fn acquire_lock(
    dir: &Path,
    file: &FileEntry,
    progress: &dyn Progress,
    cancelled: &dyn Fn() -> bool,
) -> Result<Lock, HubError> {
    let lock_path = dir.join(format!("{}.lock", file.name));
    let f = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)?;
    let mut waiting = false;
    loop {
        match f.try_lock() {
            Ok(()) => return Ok(Lock { _file: f }),
            Err(std::fs::TryLockError::WouldBlock) => {
                if !waiting {
                    waiting = true;
                    progress.note(&crate::tr!(
                        format!(
                            "另一个 nosh 进程正在下载 {}，等待其完成…（Ctrl-C 取消）",
                            file.name
                        ),
                        format!(
                            "another nosh process is downloading {}; waiting for it… (Ctrl-C to cancel)",
                            file.name
                        )
                    ));
                }
                if cancelled() {
                    return Err(HubError::Cancelled);
                }
                std::thread::sleep(LOCK_POLL);
            }
            Err(std::fs::TryLockError::Error(e)) => return Err(e.into()),
        }
    }
}

/// Downloads `file` into `dir` trying `cands` in order, failing over on error.
pub fn download_file(
    file: &FileEntry,
    dir: &Path,
    cands: &[Candidate],
    opts: &DownloadOptions,
    progress: &dyn Progress,
) -> Result<DownloadOutcome, HubError> {
    download_file_with(file, dir, cands, opts, progress, &net::is_cancelled)
}

/// Sleeps for `d` in short steps; false as soon as `cancelled` is true.
fn sleep_unless_cancelled(d: Duration, cancelled: &dyn Fn() -> bool) -> bool {
    let end = Instant::now() + d;
    loop {
        if cancelled() {
            return false;
        }
        let left = end.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return true;
        }
        std::thread::sleep(left.min(LOCK_POLL));
    }
}

/// Hashes `len` bytes of `r` (all of it with `None`) into `h` in 64 MiB
/// steps, returning `Cancelled` between steps once `cancelled` is true.
fn hash_unless_cancelled(
    r: &mut impl Read,
    h: &mut Sha256,
    len: Option<u64>,
    cancelled: &dyn Fn() -> bool,
) -> Result<(), HubError> {
    let mut left = len.unwrap_or(u64::MAX);
    while left > 0 {
        if cancelled() {
            return Err(HubError::Cancelled);
        }
        let n = hash::hash_reader(r, h, Some(left.min(64 << 20)), |_| {})?;
        if n == 0 {
            break;
        }
        left -= n;
    }
    Ok(())
}

/// [`download_file`] with the cancellation check used while waiting.
fn download_file_with(
    file: &FileEntry,
    dir: &Path,
    cands: &[Candidate],
    opts: &DownloadOptions,
    progress: &dyn Progress,
    cancelled: &dyn Fn() -> bool,
) -> Result<DownloadOutcome, HubError> {
    if cands.is_empty() {
        return Err(HubError::Network(format!(
            "no download source for {}",
            file.name
        )));
    }
    fs::create_dir_all(dir)?;
    let _lock = acquire_lock(dir, file, progress, cancelled)?;
    let final_path = dir.join(&file.name);

    // Another process may have finished while we waited for the lock.
    if let Ok(m) = fs::metadata(&final_path)
        && m.len() == file.size
    {
        let mut h = Sha256::new();
        hash_unless_cancelled(&mut File::open(&final_path)?, &mut h, None, cancelled)?;
        if hash::finalize_hex(h).eq_ignore_ascii_case(&file.sha256) {
            return Ok(DownloadOutcome {
                path: final_path,
                hub: None,
                resumed_from: file.size,
                downloaded: 0,
            });
        }
    }

    let part_path = partial_path(dir, file);
    let mut part = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&part_path)?;
    let mut offset = part.metadata()?.len();
    if offset > file.size {
        part.set_len(0)?;
        offset = 0;
    }

    let mut hasher = Sha256::new();
    if offset > 0 {
        let mb = offset as f64 / 1e6;
        progress.note(&crate::tr!(
            format!("从 {mb:.1} MB 处续传 {}（校验已下载部分）", file.name),
            format!(
                "resuming {} from {mb:.1} MB (verifying existing part)",
                file.name
            )
        ));
        part.seek(SeekFrom::Start(0))?;
        // Cancelling keeps the partial file for the next attempt.
        hash_unless_cancelled(&mut part, &mut hasher, Some(offset), cancelled)?;
    }

    let need = file.size - offset;
    if let Ok(avail) = fs4::available_space(dir) {
        let margin = 64 * 1024 * 1024;
        if avail < need + margin {
            return Err(HubError::InsufficientSpace {
                needed: need + margin,
                available: avail,
            });
        }
    }

    let resumed_from = offset;
    part.seek(SeekFrom::Start(offset))?;
    progress.start(&file.name, file.size, offset);

    let mut idx = 0usize;
    let mut failures = 0usize;
    let max_failures = cands.len() * opts.max_rounds.max(1);
    let mut last_err: Option<HubError> = None;
    let mut used_hub = None;
    while offset < file.size {
        if failures >= max_failures {
            progress.finish(false);
            return Err(HubError::AllSourcesFailed(
                last_err.map(|e| e.to_string()).unwrap_or_default(),
            ));
        }
        let c = &cands[idx % cands.len()];
        let before = offset;
        let end = (offset + opts.chunk_size).min(file.size) - 1;
        match fetch_chunk(
            c,
            end,
            file.size,
            opts,
            &mut part,
            &mut hasher,
            &mut offset,
            progress,
            cancelled,
        ) {
            Ok(()) => used_hub = Some(c.hub),
            Err(e @ (HubError::Offline | HubError::Cancelled)) => {
                progress.finish(false);
                return Err(e);
            }
            Err(e) => {
                if offset == before {
                    failures += 1;
                }
                let next = &cands[(idx + 1) % cands.len()];
                progress.note(&crate::tr!(
                    format!("{} 失败：{e}；切换到 {}", c.hub, next.hub),
                    format!("{} failed: {e}; switching to {}", c.hub, next.hub)
                ));
                last_err = Some(e);
                idx += 1;
                let exp = failures.min(4) as u32;
                // Ctrl-C only sets the flag: wake up for it.
                if !sleep_unless_cancelled(opts.backoff * 2u32.pow(exp), cancelled) {
                    progress.finish(false);
                    return Err(HubError::Cancelled);
                }
            }
        }
    }

    part.flush()?;
    part.sync_all()?;
    drop(part);
    let actual = hash::finalize_hex(hasher);
    if !actual.eq_ignore_ascii_case(&file.sha256) {
        progress.finish(false);
        let _ = fs::remove_file(&part_path);
        return Err(HubError::ChecksumMismatch {
            file: file.name.clone(),
            expected: file.sha256.clone(),
            actual,
        });
    }
    fs::rename(&part_path, &final_path)?;
    progress.finish(true);
    Ok(DownloadOutcome {
        path: final_path,
        hub: used_hub,
        resumed_from,
        downloaded: file.size - resumed_from,
    })
}

#[allow(clippy::too_many_arguments)]
fn fetch_chunk(
    c: &Candidate,
    end: u64,
    size: u64,
    opts: &DownloadOptions,
    part: &mut File,
    hasher: &mut Sha256,
    offset: &mut u64,
    progress: &dyn Progress,
    cancelled: &dyn Fn() -> bool,
) -> Result<(), HubError> {
    let resp = net::get_range(&c.url, *offset, Some(end), opts.per_call_timeout)?;
    let limit = if resp.status == 200 {
        if *offset > 0 {
            // Server ignored the Range header: restart from zero.
            part.set_len(0)?;
            part.seek(SeekFrom::Start(0))?;
            *hasher = Sha256::new();
            *offset = 0;
            progress.note(&crate::tr!(
                format!("{} 不支持断点续传，从头下载", c.hub),
                format!("{} ignores Range; restarting from zero", c.hub)
            ));
        }
        size
    } else {
        if resp.start != *offset {
            return Err(HubError::Network(format!(
                "{}: unexpected range start {} (wanted {})",
                c.hub, resp.start, *offset
            )));
        }
        end + 1
    };
    let mut reader = resp.reader;
    let mut buf = vec![0u8; 256 * 1024];
    while *offset < limit {
        if cancelled() {
            return Err(HubError::Cancelled);
        }
        let want = ((limit - *offset) as usize).min(buf.len());
        let n = reader.read(&mut buf[..want]).map_err(|e| {
            HubError::Network(format!("{}: read failed at {}: {e}", c.hub, *offset))
        })?;
        if n == 0 {
            return Err(HubError::Network(format!(
                "{}: connection closed at {}",
                c.hub, *offset
            )));
        }
        part.write_all(&buf[..n])?;
        hasher.update(&buf[..n]);
        *offset += n as u64;
        progress.advance(*offset);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::progress::NoProgress;
    use crate::registry::{FileRole, SourceRef};
    use crate::testserver::{Behavior, TestServer, tempdir};

    fn entry(data: &[u8], name: &str) -> FileEntry {
        let mut h = Sha256::new();
        h.update(data);
        FileEntry {
            role: FileRole::Weights,
            name: name.to_string(),
            size: data.len() as u64,
            sha256: hash::finalize_hex(h),
            sources: vec![SourceRef {
                hub: "hf".into(),
                repo: "x/y".into(),
                revision: "main".into(),
            }],
        }
    }

    fn cand(hub: Hub, url: String) -> Candidate {
        Candidate {
            hub,
            url,
            revision: "main".into(),
        }
    }

    fn opts() -> DownloadOptions {
        DownloadOptions {
            chunk_size: 1000,
            max_rounds: 2,
            per_call_timeout: Duration::from_secs(5),
            backoff: Duration::from_millis(1),
        }
    }

    fn data(n: usize) -> Vec<u8> {
        (0..n).map(|i| (i * 7 % 251) as u8).collect()
    }

    #[test]
    fn downloads_and_verifies_in_chunks() {
        let body = data(4500);
        let srv = TestServer::start(body.clone(), Behavior::Normal);
        let dir = tempdir("dl-ok");
        let f = entry(&body, "model.bin");
        let out = download_file(
            &f,
            &dir,
            &[cand(Hub::HuggingFace, srv.url("model.bin"))],
            &opts(),
            &NoProgress,
        )
        .unwrap();
        assert_eq!(fs::read(&out.path).unwrap(), body);
        assert!(!partial_path(&dir, &f).exists());
        assert_eq!(out.resumed_from, 0);
        assert!(srv.requests() >= 5, "chunked: {} requests", srv.requests());
    }

    #[test]
    fn resumes_from_partial_offset() {
        let body = data(3000);
        let srv = TestServer::start(body.clone(), Behavior::Normal);
        let dir = tempdir("dl-resume");
        let f = entry(&body, "m.gguf");
        fs::write(partial_path(&dir, &f), &body[..1234]).unwrap();
        let out = download_file(
            &f,
            &dir,
            &[cand(Hub::ModelScope, srv.url("m.gguf"))],
            &opts(),
            &NoProgress,
        )
        .unwrap();
        assert_eq!(out.resumed_from, 1234);
        assert_eq!(out.downloaded, 3000 - 1234);
        assert_eq!(srv.first_range_start(), Some(1234));
        assert_eq!(fs::read(&out.path).unwrap(), body);
    }

    #[test]
    fn fails_over_to_next_source_keeping_offset() {
        let body = data(5000);
        let bad = TestServer::start(body.clone(), Behavior::DropAfter(1500));
        let good = TestServer::start(body.clone(), Behavior::Normal);
        let dir = tempdir("dl-failover");
        let f = entry(&body, "w.gguf");
        let mut o = opts();
        o.chunk_size = 10_000;
        let out = download_file(
            &f,
            &dir,
            &[
                cand(Hub::HuggingFace, bad.url("w.gguf")),
                cand(Hub::ModelScope, good.url("w.gguf")),
            ],
            &o,
            &NoProgress,
        )
        .unwrap();
        assert_eq!(fs::read(&out.path).unwrap(), body);
        assert_eq!(out.hub, Some(Hub::ModelScope));
        assert_eq!(good.first_range_start(), Some(1500));
    }

    #[test]
    fn rejects_checksum_mismatch() {
        let body = data(2000);
        let mut tampered = body.clone();
        tampered[100] ^= 0xff;
        let srv = TestServer::start(tampered, Behavior::Normal);
        let dir = tempdir("dl-bad-sha");
        let f = entry(&body, "t.bin");
        let err = download_file(
            &f,
            &dir,
            &[cand(Hub::HuggingFace, srv.url("t.bin"))],
            &opts(),
            &NoProgress,
        )
        .unwrap_err();
        assert!(matches!(err, HubError::ChecksumMismatch { .. }), "{err}");
        assert!(!dir.join("t.bin").exists());
        assert!(!partial_path(&dir, &f).exists());
    }

    #[test]
    fn restarts_when_range_is_ignored() {
        let body = data(2500);
        let srv = TestServer::start(body.clone(), Behavior::IgnoreRange);
        let dir = tempdir("dl-norange");
        let f = entry(&body, "n.bin");
        fs::write(partial_path(&dir, &f), &body[..700]).unwrap();
        let out = download_file(
            &f,
            &dir,
            &[cand(Hub::HfMirror, srv.url("n.bin"))],
            &opts(),
            &NoProgress,
        )
        .unwrap();
        assert_eq!(fs::read(&out.path).unwrap(), body);
    }

    #[test]
    fn gives_up_after_all_sources_fail() {
        let body = data(1000);
        let srv = TestServer::start(body.clone(), Behavior::Status(500));
        let dir = tempdir("dl-500");
        let f = entry(&body, "e.bin");
        let err = download_file(
            &f,
            &dir,
            &[cand(Hub::HuggingFace, srv.url("e.bin"))],
            &opts(),
            &NoProgress,
        )
        .unwrap_err();
        assert!(matches!(err, HubError::AllSourcesFailed(_)), "{err}");
    }

    #[test]
    fn ctrl_c_ends_the_retry_backoff() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        let body = data(1000);
        let srv = TestServer::start(body.clone(), Behavior::Status(500));
        let dir = tempdir("dl-backoff");
        let f = entry(&body, "r.bin");
        let mut o = opts();
        // The first retry would wait 10 s.
        o.backoff = Duration::from_secs(5);
        let cancel = Arc::new(AtomicBool::new(false));
        let t0 = Instant::now();
        std::thread::scope(|s| {
            let c = cancel.clone();
            s.spawn(move || {
                std::thread::sleep(Duration::from_millis(300));
                c.store(true, Ordering::SeqCst);
            });
            let r = download_file_with(
                &f,
                &dir,
                &[cand(Hub::HuggingFace, srv.url("r.bin"))],
                &o,
                &NoProgress,
                &|| cancel.load(Ordering::SeqCst),
            );
            assert!(matches!(r, Err(HubError::Cancelled)), "{r:?}");
        });
        let waited = t0.elapsed();
        assert!(waited < Duration::from_secs(2), "{waited:?}");
        assert_eq!(srv.requests(), 1, "no retry after the cancel");
    }

    #[test]
    fn ctrl_c_stops_hashing_the_bytes_already_there() {
        let body = data(3000);
        let srv = TestServer::start(body.clone(), Behavior::Normal);
        let dir = tempdir("dl-hash-cancel");
        let f = entry(&body, "h.bin");
        let c = [cand(Hub::HuggingFace, srv.url("h.bin"))];
        // Resuming: the partial file is kept and nothing is downloaded.
        fs::write(partial_path(&dir, &f), &body[..1000]).unwrap();
        let r = download_file_with(&f, &dir, &c, &opts(), &NoProgress, &|| true);
        assert!(matches!(r, Err(HubError::Cancelled)), "{r:?}");
        assert_eq!(fs::read(partial_path(&dir, &f)).unwrap(), &body[..1000]);
        // A complete file left by another process is not hashed to the end.
        fs::write(dir.join("h.bin"), &body).unwrap();
        let r = download_file_with(&f, &dir, &c, &opts(), &NoProgress, &|| true);
        assert!(matches!(r, Err(HubError::Cancelled)), "{r:?}");
        assert_eq!(srv.requests(), 0);
    }

    #[derive(Default)]
    struct Notes(std::sync::Mutex<Vec<String>>);

    impl Progress for Notes {
        fn start(&self, _: &str, _: u64, _: u64) {}
        fn advance(&self, _: u64) {}
        fn note(&self, msg: &str) {
            self.0.lock().unwrap().push(msg.to_string());
        }
        fn finish(&self, _: bool) {}
    }

    #[test]
    fn waiting_for_another_process_can_be_cancelled() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        let dir = tempdir("dl-lock");
        let f = entry(b"x", "locked.bin");
        // Another handle holds the lock, as another nosh process would.
        let other = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(dir.join("locked.bin.lock"))
            .unwrap();
        other.try_lock().unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        let notes = Notes::default();
        let t0 = std::time::Instant::now();
        std::thread::scope(|s| {
            let c = cancel.clone();
            s.spawn(move || {
                std::thread::sleep(Duration::from_millis(300));
                c.store(true, Ordering::SeqCst);
            });
            let r = acquire_lock(&dir, &f, &notes, &|| cancel.load(Ordering::SeqCst));
            assert!(matches!(r, Err(HubError::Cancelled)));
        });
        let waited = t0.elapsed();
        assert!(waited >= Duration::from_millis(300), "{waited:?}");
        // A blocking lock() would never return here.
        assert!(waited < Duration::from_secs(2), "{waited:?}");
        let notes = notes.0.lock().unwrap();
        assert_eq!(notes.len(), 1, "one note while waiting: {notes:?}");
        assert!(
            notes[0].contains("nosh") && notes[0].contains("locked.bin"),
            "{notes:?}"
        );
        // Once the other process is done the lock is taken.
        drop(other);
        assert!(acquire_lock(&dir, &f, &NoProgress, &|| false).is_ok());
    }
}
