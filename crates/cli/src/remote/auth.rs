//! Server login: the loopback OAuth flow and session-token storage.
//!
//! `authorize` starts a `127.0.0.1` listener, sends the browser to the server's GitHub
//! login with that loopback as the redirect target, and captures the single-use code the server
//! hands back, swapped for the session in a POST body. A legacy `?session=…` callback is
//! rejected outright: the CLI asked for a code, so a session value is by definition one it did
//! not request (S-10). The pure pieces ([`authorize_url`], [`parse_callback`], [`request_target`])
//! are unit-tested; the socket loop is thin.
//! The session token lives in the OS keychain.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::{Duration, Instant};

use sotto_core::random;
use subtle::ConstantTimeEq;

use crate::error::{Error, Result};
use crate::keychain::Keychain;

#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

/// Keychain entry holding the server session token.
const KC_SERVER_SESSION: &str = "server-session";

/// How long `authorize` waits for the browser callback before giving up.
const CALLBACK_TIMEOUT: Duration = Duration::from_secs(5 * 60);
/// Per-connection read deadline: a loopback peer that connects and sends nothing (or dribbles
/// bytes with no newline) is dropped and the listener goes back to waiting, so one junk
/// connection can neither hang the login nor consume it.
const READ_TIMEOUT: Duration = Duration::from_secs(10);
/// Maximum callback request-line bytes. The callback is a short GET; anything larger is junk.
const MAX_REQUEST_LINE: usize = 8192;

/// Deadlines for the loopback accept loop. Named fields so the two `Duration`s cannot be
/// passed in the wrong order: a swap would compile and no test would catch it.
#[derive(Debug, Clone, Copy)]
struct Deadlines {
    overall: Duration,
    per_read: Duration,
}

pub fn store_session(keychain: &dyn Keychain, token: &str) -> Result<()> {
    keychain.set(KC_SERVER_SESSION, token.as_bytes())
}

pub fn current_session(keychain: &dyn Keychain) -> Result<Option<String>> {
    match keychain.get(KC_SERVER_SESSION)? {
        // Fail loudly on corruption rather than `from_utf8_lossy`, which would silently swap in
        // replacement chars and hand back a wrong-but-plausible session token.
        Some(bytes) => Ok(Some(String::from_utf8(bytes).map_err(|_| {
            Error::Keychain("stored session token is not valid UTF-8".into())
        })?)),
        None => Ok(None),
    }
}

pub fn clear_session(keychain: &dyn Keychain) -> Result<()> {
    keychain.delete(KC_SERVER_SESSION)
}

/// Build the server's GitHub-login URL with our loopback redirect + CSRF state, asking for the
/// code branch of the callback.
pub fn authorize_url(server: &str, port: u16, state: &str) -> Result<String> {
    let mut url = reqwest::Url::parse(&format!("{server}/auth/github/login"))
        .map_err(|e| Error::Input(format!("invalid server URL: {e}")))?;
    url.query_pairs_mut()
        .append_pair("redirect_uri", &format!("http://127.0.0.1:{port}/"))
        .append_pair("state", state)
        .append_pair("mode", "code");
    Ok(url.to_string())
}

/// Extract the single-use code from the loopback callback target (`/?code=…&state=…`), verifying
/// the CSRF state matches in constant time. A legacy `?session=…` callback is rejected: the CLI
/// requested a code, so a session value is one it did not request, and accepting it would let
/// any local process plant its own account (S-10).
pub fn parse_callback(target: &str, expected_state: &str) -> Result<String> {
    let url = reqwest::Url::parse(&format!("http://127.0.0.1{target}"))
        .map_err(|e| Error::Server(format!("invalid callback request: {e}")))?;
    let mut code = None;
    let mut session = false;
    let mut state = None;
    for (key, value) in url.query_pairs() {
        match key.as_ref() {
            "code" => code = Some(value.into_owned()),
            "session" => session = true,
            "state" => state = Some(value.into_owned()),
            _ => {}
        }
    }
    match state {
        Some(s) if bool::from(s.as_bytes().ct_eq(expected_state.as_bytes())) => {}
        Some(_) => {
            return Err(Error::Server(
                "callback state mismatch (possible CSRF)".into(),
            ))
        }
        None => return Err(Error::Server("callback missing state".into())),
    }
    if let Some(code) = code {
        return Ok(code);
    }
    // Reachable only with valid state (mismatches returned above), which is what lets the
    // accept loop fail fast on this: a correct-state legacy session means an outdated server.
    if session {
        return Err(Error::LegacyServer);
    }
    Err(Error::Server("callback missing code".into()))
}

/// Swap a single-use login code for the session token, in a POST body and never as a URL
/// component. Transport encryption follows the configured server URL's scheme (S-15's
/// territory); this function's guarantee is only that the code never appears in a URL.
pub fn exchange_code(server: &str, code: &str, state: &str) -> Result<String> {
    #[derive(serde::Deserialize)]
    struct ExchangeResponse {
        token: String,
    }
    // Unauthenticated endpoint, so a bare client rather than `HttpClient` (which always bears a
    // token); same timeouts.
    let http = super::http::client_builder()
        .timeout(Duration::from_secs(30))
        .connect_timeout(Duration::from_secs(10))
        .build()
        .expect("reqwest client with static config builds");
    let resp = http
        .post(format!("{server}/auth/github/exchange"))
        .json(&serde_json::json!({"code": code, "state": state}))
        .send()
        .map_err(|e| Error::Network(e.to_string()))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().unwrap_or_default();
        return Err(Error::Server(format!(
            "code exchange failed: {status}: {body}"
        )));
    }
    resp.json::<ExchangeResponse>()
        .map(|r| r.token)
        .map_err(|e| Error::Server(e.to_string()))
}

/// Run the loopback OAuth flow and return the session token (not yet stored).
pub fn authorize(server: &str) -> Result<String> {
    let listener = TcpListener::bind("127.0.0.1:0").map_err(|e| Error::Io(e.to_string()))?;
    let port = listener
        .local_addr()
        .map_err(|e| Error::Io(e.to_string()))?
        .port();
    let state = random_state();
    let url = authorize_url(server, port, &state)?;

    eprintln!("Opening your browser to authorise Sotto…");
    eprintln!("If it doesn't open, visit:\n  {url}\n");
    let redirect = open_browser(&url);

    let result =
        accept_callback(&listener, &state).and_then(|code| exchange_code(server, &code, &state));
    // The browser has navigated away by the time the callback arrives (or never will, on the
    // failure paths), so the redirect file is safe to remove on every path.
    if let Some(path) = redirect {
        std::fs::remove_file(path).ok();
    }
    result
}

/// Capture the callback code, answering the browser. Malformed, oversized and wrong-state
/// requests get a 400 and do NOT consume the flow: the listener keeps waiting until a valid
/// callback arrives or the overall deadline elapses.
fn accept_callback(listener: &TcpListener, expected_state: &str) -> Result<String> {
    accept_with_deadlines(
        listener,
        expected_state,
        Deadlines {
            overall: CALLBACK_TIMEOUT,
            per_read: READ_TIMEOUT,
        },
    )
}

fn accept_with_deadlines(
    listener: &TcpListener,
    expected_state: &str,
    deadlines: Deadlines,
) -> Result<String> {
    let Deadlines { overall, per_read } = deadlines;
    debug_assert!(
        overall > per_read,
        "overall deadline must exceed the per-connection read deadline"
    );
    listener
        .set_nonblocking(true)
        .map_err(|e| Error::Io(e.to_string()))?;
    let deadline = Instant::now() + overall;
    loop {
        if Instant::now() >= deadline {
            return Err(Error::Network(format!(
                "login timed out waiting for the browser callback after {}s",
                overall.as_secs()
            )));
        }
        let mut stream = match listener.accept() {
            Ok((stream, _)) => stream,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(20));
                continue;
            }
            Err(e) => return Err(Error::Io(e.to_string())),
        };
        // Accepted sockets inherit nonblocking mode on some platforms (macOS/BSD), which
        // would fail the read below instantly instead of waiting for the peer; force blocking
        // mode back so the read deadline governs on every platform. A peer that then sends
        // nothing is dropped at the deadline and the listener goes back to waiting.
        if stream.set_nonblocking(false).is_err()
            || stream.set_read_timeout(Some(per_read)).is_err()
        {
            continue;
        }
        match read_callback_target(&stream).and_then(|t| parse_callback(&t, expected_state)) {
            Ok(code) => {
                reply(&mut stream, true);
                return Ok(code);
            }
            // A correct-state legacy session means an outdated server, not a probe: fail
            // fast with the actionable error instead of hanging until the deadline. This hands
            // a state-knowing local attacker a login-cancel primitive, but that is nuisance
            // only (retrying mints fresh state), and dribbling junk already delays the loop
            // without knowing anything.
            Err(Error::LegacyServer) => {
                reply(&mut stream, false);
                return Err(Error::LegacyServer);
            }
            // Wrong state, malformed or oversized: 400, keep waiting.
            Err(_) => reply(&mut stream, false),
        }
    }
}

/// Read one request line from a loopback peer, bounded by [`MAX_REQUEST_LINE`], and return the
/// request target. Anything that is not a short `GET /… HTTP/…` line is an error.
fn read_callback_target(stream: &TcpStream) -> Result<String> {
    let mut reader = BufReader::new(stream);
    let mut request_line = String::new();
    reader
        .by_ref()
        .take((MAX_REQUEST_LINE + 1) as u64)
        .read_line(&mut request_line)
        .map_err(|e| Error::Io(e.to_string()))?;
    if request_line.len() > MAX_REQUEST_LINE {
        return Err(Error::Server("callback request too large".into()));
    }
    request_target(&request_line).map(str::to_string)
}

/// Parse a callback request line (`GET /?code=…&state=… HTTP/1.1`), returning the target.
/// Anything else - wrong method, missing target or version, extra tokens - is malformed.
fn request_target(request_line: &str) -> Result<&str> {
    let mut parts = request_line.split_whitespace();
    match (parts.next(), parts.next(), parts.next(), parts.next()) {
        (Some("GET"), Some(target), Some(version), None)
            if target.starts_with('/') && version.starts_with("HTTP/") =>
        {
            Ok(target)
        }
        _ => Err(Error::Server("malformed callback request".into())),
    }
}

/// Answer one callback connection; failure is irrelevant (the peer may be gone).
fn reply(stream: &mut TcpStream, ok: bool) {
    let (status, body) = if ok {
        (
            "200 OK",
            "<html><body>Sotto: login complete - you can close this tab.</body></html>",
        )
    } else {
        (
            "400 Bad Request",
            "<html><body>Sotto: login failed.</body></html>",
        )
    };
    let _ = write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
}

/// Best-effort browser open via an owner-only redirect file, so the state-bearing authorise URL
/// never appears in any process's argv (world-readable through /proc/<pid>/cmdline on Linux).
/// Returns the file path for the caller to delete, or `None` when setup failed - the printed
/// fallback URL still lets the user log in by hand.
fn open_browser(url: &str) -> Option<std::path::PathBuf> {
    let path = write_redirect_file(url).ok()?;
    spawn_opener(&path);
    Some(path)
}

/// Write a meta-refresh redirect page for `url` with owner-only permissions.
fn write_redirect_file(url: &str) -> std::io::Result<std::path::PathBuf> {
    let nonce: String = random::bytes::<8>()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let path =
        std::env::temp_dir().join(format!("sotto-login-{}-{nonce}.html", std::process::id()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options.open(&path)?;
    // Escape for the double-quoted attribute; the query separators must survive verbatim.
    let escaped = url.replace('&', "&amp;").replace('"', "&quot;");
    write!(
        file,
        "<!doctype html><html><head><meta http-equiv=\"refresh\" content=\"0;url={escaped}\">\
         </head><body><p>Sotto: authorise this login by opening \
         <a href=\"{escaped}\">this link</a>.</p></body></html>"
    )?;
    file.sync_all()?;
    Ok(path)
}

/// Best-effort opener spawn; failure is fine (the URL is printed too).
fn spawn_opener(path: &std::path::Path) {
    #[cfg(target_os = "macos")]
    let mut command = {
        let mut c = std::process::Command::new("open");
        c.arg(path);
        c
    };
    #[cfg(target_os = "windows")]
    let mut command = {
        let mut c = std::process::Command::new("cmd");
        c.args(["/C", "start", ""]).arg(path);
        c
    };
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let mut command = {
        let mut c = std::process::Command::new("xdg-open");
        c.arg(path);
        c
    };
    let _ = command
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

fn random_state() -> String {
    random::bytes::<16>()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keychain::MemoryKeychain;

    #[test]
    fn session_round_trips_in_keychain() {
        let kc = MemoryKeychain::default();
        assert!(current_session(&kc).unwrap().is_none());
        store_session(&kc, "st_abc").unwrap();
        assert_eq!(current_session(&kc).unwrap().as_deref(), Some("st_abc"));
        clear_session(&kc).unwrap();
        assert!(current_session(&kc).unwrap().is_none());
    }

    #[test]
    fn authorize_url_encodes_redirect_and_state() {
        let url = authorize_url("https://api.sotto.dev", 51999, "abc123").unwrap();
        assert!(url.starts_with("https://api.sotto.dev/auth/github/login?"));
        assert!(url.contains("redirect_uri=http%3A%2F%2F127.0.0.1%3A51999%2F"));
        assert!(url.contains("state=abc123"));
        assert!(url.contains("mode=code"));
    }

    #[test]
    fn parse_callback_extracts_the_code() {
        assert_eq!(
            parse_callback("/?code=sc_xyz&state=abc", "abc").unwrap(),
            "sc_xyz"
        );
        // Both present takes the code path; the exchange re-verifies the state server-side.
        assert_eq!(
            parse_callback("/?code=sc_xyz&session=st_xyz&state=abc", "abc").unwrap(),
            "sc_xyz"
        );
    }

    #[test]
    fn parse_callback_rejects_a_legacy_session() {
        // The CLI asked for a code, so a session value is one it did not request. The
        // accept loop matches on the variant to fail fast, so pin it, not just the text.
        let err = parse_callback("/?session=st_xyz&state=abc", "abc").unwrap_err();
        assert!(
            matches!(err, Error::LegacyServer),
            "unexpected error: {err}"
        );
        assert!(
            err.to_string().contains("instead of a login code"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn parse_callback_rejects_state_mismatch_and_missing_fields() {
        assert!(parse_callback("/?code=sc_xyz&state=evil", "abc").is_err());
        assert!(parse_callback("/?state=abc", "abc").is_err());
        assert!(parse_callback("/?code=sc_xyz", "abc").is_err());
        assert!(parse_callback("/?session=st_xyz", "abc").is_err());
        assert!(parse_callback("not a target", "abc").is_err());
    }

    #[test]
    fn request_target_accepts_only_a_short_get_line() {
        assert_eq!(
            request_target("GET /?code=sc_xyz&state=abc HTTP/1.1\r\n").unwrap(),
            "/?code=sc_xyz&state=abc"
        );
        for bad in [
            "",
            "GARBAGE",
            "POST /?code=sc_xyz&state=abc HTTP/1.1",
            "GET /?code=sc_xyz&state=abc",
            "GET /?code=sc_xyz&state=abc HTTP/1.1 EXTRA",
            "GET https://evil.example/ HTTP/1.1",
            "GET /?code=sc_xyz&state=abc FROBNICATE/9.9",
        ] {
            assert!(request_target(bad).is_err(), "accepted: {bad:?}");
        }
    }

    /// Drive `accept_with_deadlines` in a background thread, returning the listener port and a
    /// receiver for its verdict.
    fn serve_callback(
        expected_state: &'static str,
        deadlines: Deadlines,
    ) -> (u16, std::sync::mpsc::Receiver<Result<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("local addr").port();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            tx.send(accept_with_deadlines(&listener, expected_state, deadlines))
                .expect("send verdict");
        });
        (port, rx)
    }

    /// Send one raw request, returning the full response. The client-side read timeout keeps a
    /// regressed server from hanging the suite.
    fn get(port: u16, raw: &str) -> String {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
        stream.write_all(raw.as_bytes()).expect("write");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("client read timeout");
        let mut resp = String::new();
        std::io::Read::read_to_string(&mut stream, &mut resp).expect("read reply");
        resp
    }

    #[test]
    fn accept_loop_ignores_junk_until_a_valid_callback() {
        let (port, rx) = serve_callback(
            "abc",
            Deadlines {
                overall: Duration::from_secs(10),
                per_read: Duration::from_secs(2),
            },
        );
        let malformed = get(port, "GARBAGE\r\n");
        assert!(malformed.starts_with("HTTP/1.1 400"), "{malformed}");
        let wrong_state = get(port, "GET /?code=sc_evil&state=evil HTTP/1.1\r\n\r\n");
        assert!(wrong_state.starts_with("HTTP/1.1 400"), "{wrong_state}");
        // None of the above consumed the flow: the real callback still lands.
        // (A correct-state legacy session is not junk: it fails the flow fast, covered below.)
        let valid = get(port, "GET /?code=sc_ok&state=abc HTTP/1.1\r\n\r\n");
        assert!(valid.starts_with("HTTP/1.1 200 OK"), "{valid}");
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(5))
                .expect("verdict")
                .unwrap(),
            "sc_ok"
        );
    }

    #[test]
    fn accept_loop_rejects_an_oversized_request() {
        let (port, rx) = serve_callback(
            "abc",
            Deadlines {
                overall: Duration::from_secs(10),
                per_read: Duration::from_secs(2),
            },
        );
        let big = get(
            port,
            &format!("GET /?{} HTTP/1.1\r\n\r\n", "a".repeat(9000)),
        );
        assert!(big.starts_with("HTTP/1.1 400"), "{big}");
        let valid = get(port, "GET /?code=sc_ok&state=abc HTTP/1.1\r\n\r\n");
        assert!(valid.starts_with("HTTP/1.1 200 OK"), "{valid}");
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(5))
                .expect("verdict")
                .unwrap(),
            "sc_ok"
        );
    }

    #[test]
    fn accept_loop_drops_a_peer_that_never_finishes_its_request() {
        let (port, rx) = serve_callback(
            "abc",
            Deadlines {
                overall: Duration::from_secs(10),
                per_read: Duration::from_millis(200),
            },
        );
        // A peer that connects and never finishes its request line is dropped with a 400 once
        // the per-connection read deadline expires (no newline, no hang).
        let mut idle = TcpStream::connect(("127.0.0.1", port)).expect("connect");
        idle.write_all(b"GET /?code=").expect("partial write");
        idle.set_read_timeout(Some(Duration::from_secs(5)))
            .expect("client read timeout");
        let mut resp = String::new();
        std::io::Read::read_to_string(&mut idle, &mut resp).expect("read reply");
        assert!(resp.starts_with("HTTP/1.1 400"), "{resp}");
        drop(idle);
        // ... and the flow survives it.
        let valid = get(port, "GET /?code=sc_ok&state=abc HTTP/1.1\r\n\r\n");
        assert!(valid.starts_with("HTTP/1.1 200 OK"), "{valid}");
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(5))
                .expect("verdict")
                .unwrap(),
            "sc_ok"
        );
    }

    #[test]
    fn accept_loop_fails_fast_on_a_legacy_session() {
        let (port, rx) = serve_callback(
            "abc",
            Deadlines {
                overall: Duration::from_secs(30),
                per_read: Duration::from_secs(2),
            },
        );
        let start = Instant::now();
        let legacy = get(port, "GET /?session=st_old&state=abc HTTP/1.1\r\n\r\n");
        assert!(legacy.starts_with("HTTP/1.1 400"), "{legacy}");
        let err = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("verdict")
            .unwrap_err();
        assert!(
            err.to_string().contains("instead of a login code"),
            "unexpected error: {err}"
        );
        // Fail fast, not at the 30s overall deadline.
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "waited for the deadline"
        );
    }

    #[test]
    fn accept_loop_times_out_instead_of_hanging() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let start = Instant::now();
        let err = accept_with_deadlines(
            &listener,
            "abc",
            Deadlines {
                overall: Duration::from_millis(100),
                per_read: Duration::from_millis(50),
            },
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("timed out"),
            "unexpected error: {err}"
        );
        assert!(start.elapsed() < Duration::from_secs(10), "hung");
    }

    #[test]
    fn write_redirect_file_is_owner_only_and_carries_the_url() {
        let path = write_redirect_file("https://srv/auth?state=abc&x=1\"q").unwrap();
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(
            body.contains("https://srv/auth?state=abc&amp;x=1&quot;q"),
            "{body}"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o077, 0, "group/other-readable: {mode:o}");
        }
        std::fs::remove_file(&path).unwrap();
    }

    /// Answer one request with `response_body` verbatim, capturing the request line and body.
    fn serve_once(
        response_body: &'static str,
    ) -> (String, std::sync::mpsc::Receiver<(String, String)>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let base = format!("http://{}", listener.local_addr().expect("local addr"));
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept");
            let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
            let mut request_line = String::new();
            reader.read_line(&mut request_line).expect("read line");
            let mut content_length = 0usize;
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).expect("read head") <= 2 {
                    break;
                }
                // Header names are case-insensitive, and reqwest sends them in lowercase.
                let lower = line.to_ascii_lowercase();
                if let Some(n) = lower.strip_prefix("content-length:") {
                    content_length = n.trim().parse().expect("content length");
                }
            }
            let mut body = vec![0u8; content_length];
            std::io::Read::read_exact(&mut reader, &mut body).expect("read body");
            tx.send((request_line, String::from_utf8(body).expect("utf8")))
                .expect("send");
            let mut stream = reader.into_inner();
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response_body}",
                response_body.len()
            );
        });
        (base, rx)
    }

    #[test]
    fn exchange_code_posts_code_and_state_and_returns_the_token() {
        let (base, rx) = serve_once(r#"{"token":"st_exchanged"}"#);
        let token = exchange_code(&base, "sc_abc", "cli-state").expect("exchange");
        assert_eq!(token, "st_exchanged");
        let (request_line, body) = rx.recv().expect("captured request");
        assert!(request_line.starts_with("POST /auth/github/exchange "));
        assert!(body.contains("\"code\":\"sc_abc\""), "code in body: {body}");
        assert!(
            body.contains("\"state\":\"cli-state\""),
            "state in body: {body}"
        );
    }
}
