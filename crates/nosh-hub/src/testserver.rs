//! Minimal in-process HTTP/1.1 server for download tests (HEAD + ranged GET).

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, Copy)]
pub enum Behavior {
    Normal,
    /// Close the connection once the absolute offset reaches this byte.
    DropAfter(u64),
    /// Always answer 200 with the full body.
    IgnoreRange,
    /// Always answer with this HTTP status.
    Status(u16),
}

pub struct TestServer {
    port: u16,
    requests: Arc<AtomicUsize>,
    ranges: Arc<Mutex<Vec<u64>>>,
}

impl TestServer {
    pub fn start(body: Vec<u8>, behavior: Behavior) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let port = listener.local_addr().unwrap().port();
        let requests = Arc::new(AtomicUsize::new(0));
        let ranges = Arc::new(Mutex::new(Vec::new()));
        let body = Arc::new(body);
        {
            let requests = requests.clone();
            let ranges = ranges.clone();
            std::thread::spawn(move || {
                for stream in listener.incoming().flatten() {
                    let body = body.clone();
                    let requests = requests.clone();
                    let ranges = ranges.clone();
                    std::thread::spawn(move || {
                        let _ = handle(stream, &body, behavior, &requests, &ranges);
                    });
                }
            });
        }
        Self {
            port,
            requests,
            ranges,
        }
    }

    pub fn url(&self, path: &str) -> String {
        format!(
            "http://127.0.0.1:{}/{}",
            self.port,
            path.trim_start_matches('/')
        )
    }

    pub fn base(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    pub fn requests(&self) -> usize {
        self.requests.load(Ordering::SeqCst)
    }

    pub fn first_range_start(&self) -> Option<u64> {
        self.ranges.lock().unwrap().first().copied()
    }
}

/// A fresh, unique temporary directory.
pub fn tempdir(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!(
        "nosh-test-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn handle(
    stream: TcpStream,
    body: &[u8],
    behavior: Behavior,
    requests: &AtomicUsize,
    ranges: &Mutex<Vec<u64>>,
) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let mut range: Option<(u64, Option<u64>)> = None;
    loop {
        let mut h = String::new();
        if reader.read_line(&mut h)? == 0 || h == "\r\n" || h == "\n" {
            break;
        }
        let lower = h.to_ascii_lowercase();
        if let Some(v) = lower.strip_prefix("range:") {
            let v = v.trim().trim_start_matches("bytes=");
            let mut it = v.splitn(2, '-');
            let a = it.next().and_then(|s| s.trim().parse().ok());
            let b = it.next().and_then(|s| s.trim().parse().ok());
            if let Some(a) = a {
                range = Some((a, b));
            }
        }
    }
    requests.fetch_add(1, Ordering::SeqCst);
    let mut out = stream;
    let total = body.len() as u64;
    if let Behavior::Status(code) = behavior {
        write!(
            out,
            "HTTP/1.1 {code} Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        )?;
        return Ok(());
    }
    if method == "HEAD" {
        write!(
            out,
            "HTTP/1.1 200 OK\r\nContent-Length: {total}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n"
        )?;
        return Ok(());
    }
    let (status, start, end) = match (behavior, range) {
        (Behavior::IgnoreRange, _) | (_, None) => (200, 0, total.saturating_sub(1)),
        (_, Some((a, b))) => {
            ranges.lock().unwrap().push(a);
            let end = b.unwrap_or(total - 1).min(total - 1);
            (206, a, end)
        }
    };
    let len = end + 1 - start;
    let mut head = format!(
        "HTTP/1.1 {status} {}\r\nContent-Length: {len}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n",
        if status == 206 {
            "Partial Content"
        } else {
            "OK"
        }
    );
    if status == 206 {
        head.push_str(&format!("Content-Range: bytes {start}-{end}/{total}\r\n"));
    }
    head.push_str("\r\n");
    out.write_all(head.as_bytes())?;
    let stop = match behavior {
        Behavior::DropAfter(n) => n.min(end + 1),
        _ => end + 1,
    };
    if stop > start {
        out.write_all(&body[start as usize..stop as usize])?;
    }
    out.flush()?;
    Ok(())
}
