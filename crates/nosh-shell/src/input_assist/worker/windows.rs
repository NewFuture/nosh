use std::io::{self, Read};
use std::net::{TcpListener, TcpStream};
use std::time::{Duration, Instant};

const MAX_CONNECTIONS: usize = 8;

struct Authentication {
    stream: TcpStream,
    received: [u8; 32],
    read: usize,
}

impl Authentication {
    fn poll(&mut self, token: &[u8]) -> io::Result<Option<bool>> {
        while self.read < self.received.len() {
            match self.stream.read(&mut self.received[self.read..]) {
                Ok(0) => return Ok(Some(false)),
                Ok(count) => self.read += count,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(None),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::ConnectionReset | io::ErrorKind::ConnectionAborted
                    ) =>
                {
                    return Ok(Some(false));
                }
                Err(error) => return Err(error),
            }
        }
        Ok(Some(self.received.as_slice() == token))
    }
}

pub(crate) fn accept_worker(
    listener: &TcpListener,
    token: &str,
    deadline: Instant,
    mut exit_code: impl FnMut() -> io::Result<Option<i32>>,
) -> io::Result<TcpStream> {
    if token.len() != 32 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "worker authentication token must be 32 bytes",
        ));
    }
    let mut connections: Vec<Authentication> = Vec::new();
    loop {
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "worker startup timed out",
            ));
        }
        if let Some(code) = exit_code()? {
            return Err(io::Error::other(format!(
                "worker exited during startup ({code})"
            )));
        }
        // Finish buffered handshakes before a new connection can evict an old one.
        let mut index = 0;
        while index < connections.len() {
            match connections[index].poll(token.as_bytes())? {
                Some(true) => {
                    let stream = connections.remove(index).stream;
                    stream.set_nodelay(true)?;
                    return Ok(stream);
                }
                Some(false) => {
                    connections.remove(index);
                }
                None => index += 1,
            }
        }
        match listener.accept() {
            Ok((stream, _)) => {
                stream.set_nonblocking(true)?;
                if connections.len() == MAX_CONNECTIONS {
                    connections.remove(0);
                }
                connections.push(Authentication {
                    stream,
                    received: [0; 32],
                    read: 0,
                });
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::net::Ipv4Addr;

    #[test]
    fn incomplete_tokens_are_not_compared_before_all_bytes_arrive() {
        let token = b"0123456789abcdef0123456789abcdef";
        for candidate in [*token, *b"ffffffffffffffffffffffffffffffff"] {
            let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
            let mut client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
            let (stream, _) = listener.accept().unwrap();
            stream.set_nonblocking(true).unwrap();
            let mut authentication = Authentication {
                stream,
                received: [0; 32],
                read: 0,
            };
            let deadline = Instant::now() + Duration::from_secs(3);
            client.write_all(&candidate[..7]).unwrap();
            loop {
                assert_eq!(authentication.poll(token).unwrap(), None);
                if authentication.read == 7 {
                    break;
                }
                assert!(Instant::now() < deadline, "token prefix was not received");
                std::thread::sleep(Duration::from_millis(2));
            }
            client.write_all(&candidate[7..]).unwrap();
            let accepted = loop {
                if let Some(accepted) = authentication.poll(token).unwrap() {
                    break accepted;
                }
                assert!(Instant::now() < deadline, "complete token was not received");
                std::thread::sleep(Duration::from_millis(2));
            };
            assert_eq!(accepted, candidate == *token);
        }
    }

    #[test]
    fn worker_authentication_retains_delayed_token_fragments() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let token = "0123456789abcdef0123456789abcdef";
        let client = std::thread::spawn(move || -> io::Result<()> {
            let mut stream = TcpStream::connect(address)?;
            stream.write_all(&token.as_bytes()[..7])?;
            std::thread::sleep(Duration::from_millis(200));
            stream.write_all(&token.as_bytes()[7..])?;
            stream.write_all(b"next")?;
            Ok(())
        });
        let result = accept_worker(
            &listener,
            token,
            Instant::now() + Duration::from_secs(3),
            || Ok(None),
        );
        let sent = client.join().unwrap();
        let mut stream = result.unwrap();
        sent.unwrap();
        stream.set_nonblocking(false).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let mut next = [0; 4];
        stream.read_exact(&mut next).unwrap();
        assert_eq!(&next, b"next");
    }

    #[test]
    fn unauthenticated_connections_do_not_block_the_worker() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let token = "0123456789abcdef0123456789abcdef";
        let _idle = TcpStream::connect(address).unwrap();
        let mut wrong = TcpStream::connect(address).unwrap();
        wrong
            .write_all(b"ffffffffffffffffffffffffffffffff")
            .unwrap();
        drop(TcpStream::connect(address).unwrap());
        let mut client = TcpStream::connect(address).unwrap();
        client.write_all(token.as_bytes()).unwrap();
        client.write_all(b"next").unwrap();
        let mut stream = accept_worker(
            &listener,
            token,
            Instant::now() + Duration::from_secs(3),
            || Ok(None),
        )
        .unwrap();
        assert_eq!(stream.peer_addr().unwrap(), client.local_addr().unwrap());
        stream.set_nonblocking(false).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let mut next = [0; 4];
        stream.read_exact(&mut next).unwrap();
        assert_eq!(&next, b"next");
    }

    #[test]
    fn a_full_idle_connection_pool_does_not_exclude_the_worker() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let token = "0123456789abcdef0123456789abcdef";
        let _idle: Vec<_> = (0..MAX_CONNECTIONS)
            .map(|_| TcpStream::connect(address).unwrap())
            .collect();
        let mut client = TcpStream::connect(address).unwrap();
        client.write_all(token.as_bytes()).unwrap();
        client.write_all(b"next").unwrap();
        let mut stream = accept_worker(
            &listener,
            token,
            Instant::now() + Duration::from_secs(3),
            || Ok(None),
        )
        .unwrap();
        assert_eq!(stream.peer_addr().unwrap(), client.local_addr().unwrap());
        stream.set_nonblocking(false).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let mut next = [0; 4];
        stream.read_exact(&mut next).unwrap();
        assert_eq!(&next, b"next");
    }

    #[test]
    fn incomplete_authentication_keeps_the_startup_deadline_and_exit_checks() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let token = "0123456789abcdef0123456789abcdef";
        let mut client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        client.write_all(&token.as_bytes()[..7]).unwrap();
        let error = accept_worker(
            &listener,
            token,
            Instant::now() + Duration::from_millis(50),
            || Ok(None),
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        let error = accept_worker(
            &listener,
            token,
            Instant::now() + Duration::from_secs(3),
            || Ok(Some(17)),
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("worker exited during startup (17)")
        );
    }
}
