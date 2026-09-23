//! The uptime SLI's host probe: a Postgres handshake on the entrypoint's own
//! port 5432, the way a client would open one, without logging in.
//!
//! The probe sends a StartupMessage and reads the first answer:
//! - the server asks to authenticate (or lets the session in) → ok: the
//!   connection crossed HAProxy, reached the primary, and Postgres answered;
//! - the server sends an ErrorResponse → fail, carrying its SQLSTATE so the
//!   control plane can class it (53300 "too many clients" is
//!   CONNECTION_EXHAUSTED);
//! - HAProxy accepts the TCP connection and closes it (no backend) → fail;
//! - nothing within the timeout → fail.
//!
//! No credential is sent. The server's authentication request is the answer
//! the probe waits for; the connection is dropped right after it, which
//! Postgres treats as a client that only wanted to know whether a password is
//! needed (it exits without logging).

use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::{Duration, Instant};

/// Protocol 3.0, as in every StartupMessage since Postgres 7.4.
const PROTOCOL_VERSION_3: i32 = 196_608;
/// Upper bound on a message body the probe will read. The answers it waits for
/// are a few dozen bytes; anything larger is not a Postgres server.
const MAX_BODY: usize = 64 * 1024;
/// application_name the probe presents, so a probe is recognisable in
/// `pg_stat_activity` / server logs if it is ever seen there.
pub const APPLICATION_NAME: &str = "railway-sli-probe";
/// AuthenticationRequest code 0: the session is in without a password.
const AUTH_OK: [u8; 4] = 0i32.to_be_bytes();
/// The frontend Terminate message.
const TERMINATE: [u8; 5] = [b'X', 0, 0, 0, 4];

/// Why a probe failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FailReason {
    /// The TCP connection to the entrypoint itself was refused or errored.
    Connect,
    /// No answer within the timeout.
    Timeout,
    /// The entrypoint accepted the connection and closed it without a
    /// Postgres answer (HAProxy with no backend to route to).
    Closed,
    /// Postgres answered with an ErrorResponse; the SQLSTATE is kept.
    Error { sqlstate: String },
    /// Something answered that is not the Postgres protocol.
    Protocol,
}

impl FailReason {
    /// The `reason=` token on the log line.
    pub fn token(&self) -> &'static str {
        match self {
            FailReason::Connect => "connect",
            FailReason::Timeout => "timeout",
            FailReason::Closed => "closed",
            FailReason::Error { .. } => "error",
            FailReason::Protocol => "protocol",
        }
    }
}

/// The outcome of one handshake.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Ok,
    Fail(FailReason),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeResult {
    pub outcome: Outcome,
    pub latency: Duration,
}

impl ProbeResult {
    pub fn is_ok(&self) -> bool {
        self.outcome == Outcome::Ok
    }
}

/// Build the StartupMessage: length, protocol version, then `key\0value\0`
/// pairs and a closing `\0`.
pub fn startup_message(user: &str) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&PROTOCOL_VERSION_3.to_be_bytes());
    for (k, v) in [("user", user), ("application_name", APPLICATION_NAME)] {
        body.extend_from_slice(k.as_bytes());
        body.push(0);
        body.extend_from_slice(v.as_bytes());
        body.push(0);
    }
    body.push(0);
    let mut msg = Vec::with_capacity(body.len() + 4);
    msg.extend_from_slice(&((body.len() + 4) as i32).to_be_bytes());
    msg.extend_from_slice(&body);
    msg
}

/// The SQLSTATE (`C` field) of an ErrorResponse body; `XX000` when the
/// server sent none (every real server does).
pub fn error_sqlstate(body: &[u8]) -> String {
    let mut rest = body;
    while let Some((&field, tail)) = rest.split_first() {
        if field == 0 {
            break;
        }
        let end = tail.iter().position(|b| *b == 0).unwrap_or(tail.len());
        if field == b'C' {
            return String::from_utf8_lossy(&tail[..end]).into_owned();
        }
        rest = tail.get(end + 1..).unwrap_or(&[]);
    }
    "XX000".to_string()
}

fn is_timeout(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
    )
}

/// Read one backend message header + body under the deadline.
fn read_message(stream: &mut TcpStream, deadline: Instant) -> Result<(u8, Vec<u8>), FailReason> {
    let mut header = [0u8; 5];
    read_exact_by(stream, &mut header, deadline)?;
    let len = i32::from_be_bytes([header[1], header[2], header[3], header[4]]);
    if len < 4 || (len as usize - 4) > MAX_BODY {
        return Err(FailReason::Protocol);
    }
    let mut body = vec![0u8; len as usize - 4];
    read_exact_by(stream, &mut body, deadline)?;
    Ok((header[0], body))
}

fn read_exact_by(
    stream: &mut TcpStream,
    buf: &mut [u8],
    deadline: Instant,
) -> Result<(), FailReason> {
    let mut filled = 0;
    while filled < buf.len() {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(FailReason::Timeout);
        }
        stream
            .set_read_timeout(Some(left))
            .map_err(|_| FailReason::Connect)?;
        match stream.read(&mut buf[filled..]) {
            Ok(0) => return Err(FailReason::Closed),
            Ok(n) => filled += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) if is_timeout(&e) => return Err(FailReason::Timeout),
            // A reset after accept is the same fact as a close: the
            // entrypoint took the connection and had nothing behind it.
            Err(_) => return Err(FailReason::Closed),
        }
    }
    Ok(())
}

fn classify(stream: &mut TcpStream, user: &str, deadline: Instant) -> Outcome {
    let left = deadline.saturating_duration_since(Instant::now());
    if stream
        .set_write_timeout(Some(left.max(Duration::from_millis(1))))
        .is_err()
    {
        return Outcome::Fail(FailReason::Connect);
    }
    if let Err(e) = stream.write_all(&startup_message(user)) {
        return Outcome::Fail(if is_timeout(&e) {
            FailReason::Timeout
        } else {
            FailReason::Closed
        });
    }
    loop {
        match read_message(stream, deadline) {
            // AuthenticationRequest of any kind (including AuthenticationOk
            // under `trust`): Postgres is answering on the customer's path.
            Ok((b'R', body)) => {
                // Under `trust` the session is already in (AuthenticationOk,
                // code 0): Terminate so it ends cleanly. On a password request
                // the probe only drops the connection — Postgres reads that as
                // a client checking whether a password is needed and exits
                // silently, where any other message would be logged as a
                // protocol violation on the customer's server.
                if body.get(..4) == Some(&AUTH_OK[..]) {
                    let _ = stream.write_all(&TERMINATE);
                }
                return Outcome::Ok;
            }
            Ok((b'E', body)) => {
                return Outcome::Fail(FailReason::Error {
                    sqlstate: error_sqlstate(&body),
                })
            }
            // NegotiateProtocolVersion precedes the authentication request
            // when the server does not know an option; keep reading.
            Ok((b'v', _)) => continue,
            Ok(_) => return Outcome::Fail(FailReason::Protocol),
            Err(reason) => return Outcome::Fail(reason),
        }
    }
}

/// One handshake against `addr`, bounded by `timeout` end to end.
pub fn handshake(addr: SocketAddr, user: &str, timeout: Duration) -> ProbeResult {
    let started = Instant::now();
    let deadline = started + timeout;
    let outcome = match TcpStream::connect_timeout(&addr, timeout) {
        Ok(mut stream) => {
            let _ = stream.set_nodelay(true);
            classify(&mut stream, user, deadline)
        }
        Err(e) if is_timeout(&e) => Outcome::Fail(FailReason::Timeout),
        Err(_) => Outcome::Fail(FailReason::Connect),
    };
    ProbeResult {
        outcome,
        latency: started.elapsed(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::thread;

    /// A one-shot server that reads the startup message and then does `answer`.
    fn serve(answer: impl FnOnce(&mut TcpStream) + Send + 'static) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut len = [0u8; 4];
            s.read_exact(&mut len).unwrap();
            let mut rest = vec![0u8; i32::from_be_bytes(len) as usize - 4];
            s.read_exact(&mut rest).unwrap();
            answer(&mut s);
        });
        addr
    }

    fn message(kind: u8, body: &[u8]) -> Vec<u8> {
        let mut m = vec![kind];
        m.extend_from_slice(&((body.len() + 4) as i32).to_be_bytes());
        m.extend_from_slice(body);
        m
    }

    fn error_body(sqlstate: &str, text: &str) -> Vec<u8> {
        let mut b = Vec::new();
        for (f, v) in [(b'S', "FATAL"), (b'C', sqlstate), (b'M', text)] {
            b.push(f);
            b.extend_from_slice(v.as_bytes());
            b.push(0);
        }
        b.push(0);
        b
    }

    const T: Duration = Duration::from_secs(2);

    #[test]
    fn startup_message_is_protocol_3_with_user_and_application_name() {
        let m = startup_message("railway");
        assert_eq!(
            i32::from_be_bytes([m[0], m[1], m[2], m[3]]) as usize,
            m.len()
        );
        assert_eq!(&m[4..8], &PROTOCOL_VERSION_3.to_be_bytes());
        let pairs = &m[8..];
        assert_eq!(
            pairs,
            b"user\0railway\0application_name\0railway-sli-probe\0\0".as_slice()
        );
    }

    /// What the client sent after the server's answer, until it closed.
    fn sent_after(answer: Vec<u8>) -> (Outcome, Vec<u8>) {
        let (tx, rx) = std::sync::mpsc::channel();
        let addr = serve(move |s| {
            s.write_all(&answer).unwrap();
            let mut rest = Vec::new();
            let _ = s.read_to_end(&mut rest);
            tx.send(rest).unwrap();
        });
        let outcome = handshake(addr, "railway", T).outcome;
        (outcome, rx.recv_timeout(T).unwrap())
    }

    #[test]
    fn a_password_request_is_ok_and_the_probe_just_hangs_up() {
        // AuthenticationSASL (10) with SCRAM-SHA-256, what the image's
        // pg_hba answers every non-local client. Anything sent now would be
        // logged by Postgres as a protocol violation; a bare close is silent.
        let mut body = 10i32.to_be_bytes().to_vec();
        body.extend_from_slice(b"SCRAM-SHA-256\0\0");
        let (outcome, sent) = sent_after(message(b'R', &body));
        assert_eq!(outcome, Outcome::Ok);
        assert!(sent.is_empty(), "sent {sent:?}");
    }

    #[test]
    fn an_md5_request_is_ok() {
        let mut body = 5i32.to_be_bytes().to_vec();
        body.extend_from_slice(&[1, 2, 3, 4]);
        let (outcome, sent) = sent_after(message(b'R', &body));
        assert_eq!(outcome, Outcome::Ok);
        assert!(sent.is_empty());
    }

    #[test]
    fn a_trust_login_is_ok_and_terminated() {
        let (outcome, sent) = sent_after(message(b'R', &0i32.to_be_bytes()));
        assert_eq!(outcome, Outcome::Ok);
        assert_eq!(sent, TERMINATE.to_vec());
    }

    #[test]
    fn negotiate_protocol_version_is_skipped_before_the_auth_request() {
        let addr = serve(|s| {
            let mut v = 0i32.to_be_bytes().to_vec();
            v.extend_from_slice(&0i32.to_be_bytes());
            s.write_all(&message(b'v', &v)).unwrap();
            s.write_all(&message(b'R', &5i32.to_be_bytes())).unwrap();
        });
        assert!(handshake(addr, "railway", T).is_ok());
    }

    #[test]
    fn too_many_clients_fails_with_its_sqlstate() {
        let addr = serve(|s| {
            s.write_all(&message(
                b'E',
                &error_body("53300", "sorry, too many clients already"),
            ))
            .unwrap();
        });
        assert_eq!(
            handshake(addr, "railway", T).outcome,
            Outcome::Fail(FailReason::Error {
                sqlstate: "53300".into()
            })
        );
    }

    #[test]
    fn starting_up_fails_with_its_sqlstate() {
        let addr = serve(|s| {
            s.write_all(&message(
                b'E',
                &error_body("57P03", "the database system is starting up"),
            ))
            .unwrap();
        });
        assert_eq!(
            handshake(addr, "railway", T).outcome,
            Outcome::Fail(FailReason::Error {
                sqlstate: "57P03".into()
            })
        );
    }

    #[test]
    fn an_accepted_then_closed_connection_is_closed() {
        // HAProxy with no backend: accepts, then closes.
        let addr = serve(|s| {
            let _ = s.shutdown(std::net::Shutdown::Both);
        });
        assert_eq!(
            handshake(addr, "railway", T).outcome,
            Outcome::Fail(FailReason::Closed)
        );
    }

    #[test]
    fn silence_past_the_timeout_is_timeout() {
        let addr = serve(|_s| thread::sleep(Duration::from_millis(800)));
        let r = handshake(addr, "railway", Duration::from_millis(200));
        assert_eq!(r.outcome, Outcome::Fail(FailReason::Timeout));
        assert!(r.latency >= Duration::from_millis(200));
        assert!(r.latency < Duration::from_millis(700));
    }

    #[test]
    fn a_refused_connection_is_connect() {
        let addr = {
            let l = TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap()
        }; // listener dropped: nothing listens there now
        assert_eq!(
            handshake(addr, "railway", T).outcome,
            Outcome::Fail(FailReason::Connect)
        );
    }

    #[test]
    fn a_non_postgres_answer_is_protocol() {
        let addr = serve(|s| {
            s.write_all(b"HTTP/1.1 400 Bad Request\r\n\r\n").unwrap();
        });
        assert_eq!(
            handshake(addr, "railway", T).outcome,
            Outcome::Fail(FailReason::Protocol)
        );
    }

    #[test]
    fn sqlstate_defaults_when_the_error_carries_none() {
        let mut b = vec![b'M'];
        b.extend_from_slice(b"no code\0\0");
        assert_eq!(error_sqlstate(&b), "XX000");
        assert_eq!(error_sqlstate(&error_body("28P01", "x")), "28P01");
    }
}
