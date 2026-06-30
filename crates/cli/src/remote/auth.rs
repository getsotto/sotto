//! Server login: the loopback OAuth flow and session-token storage.
//!
//! `authorize` starts a one-shot `127.0.0.1` listener, sends the browser to the server's GitHub
//! login with that loopback as the redirect target, and captures the session the server hands back.
//! The pure pieces ([`authorize_url`], [`parse_callback`]) are unit-tested; the socket loop is thin.
//! The session token is a bearer credential, so it lives in the OS keychain.

use std::io::{Read, Write};
use std::net::TcpListener;

use sotto_core::random;

use crate::error::{Error, Result};
use crate::keychain::Keychain;

/// Keychain entry holding the server session bearer token.
const KC_SERVER_SESSION: &str = "server-session";

pub fn store_session(keychain: &dyn Keychain, token: &str) -> Result<()> {
    keychain.set(KC_SERVER_SESSION, token.as_bytes())
}

pub fn current_session(keychain: &dyn Keychain) -> Result<Option<String>> {
    Ok(keychain
        .get(KC_SERVER_SESSION)?
        .map(|b| String::from_utf8_lossy(&b).into_owned()))
}

pub fn clear_session(keychain: &dyn Keychain) -> Result<()> {
    keychain.delete(KC_SERVER_SESSION)
}

/// Build the server's GitHub-login URL with our loopback redirect + CSRF state.
pub fn authorize_url(server: &str, port: u16, state: &str) -> Result<String> {
    let mut url = reqwest::Url::parse(&format!("{server}/auth/github/login"))
        .map_err(|e| Error::Input(format!("invalid server URL: {e}")))?;
    url.query_pairs_mut()
        .append_pair("redirect_uri", &format!("http://127.0.0.1:{port}/"))
        .append_pair("state", state);
    Ok(url.to_string())
}

/// Extract the session token from the loopback callback target (`/?session=…&state=…`), verifying
/// the CSRF state matches.
pub fn parse_callback(target: &str, expected_state: &str) -> Result<String> {
    let url = reqwest::Url::parse(&format!("http://127.0.0.1{target}"))
        .map_err(|e| Error::Server(format!("invalid callback request: {e}")))?;
    let mut session = None;
    let mut state = None;
    for (key, value) in url.query_pairs() {
        match key.as_ref() {
            "session" => session = Some(value.into_owned()),
            "state" => state = Some(value.into_owned()),
            _ => {}
        }
    }
    match state {
        Some(s) if s == expected_state => {}
        Some(_) => {
            return Err(Error::Server(
                "callback state mismatch (possible CSRF)".into(),
            ))
        }
        None => return Err(Error::Server("callback missing state".into())),
    }
    session.ok_or_else(|| Error::Server("callback missing session token".into()))
}

/// Run the loopback OAuth flow and return the captured session token (not yet stored).
pub fn authorize(server: &str) -> Result<String> {
    let listener = TcpListener::bind("127.0.0.1:0").map_err(|e| Error::Io(e.to_string()))?;
    let port = listener
        .local_addr()
        .map_err(|e| Error::Io(e.to_string()))?
        .port();
    let state = random_state();
    let url = authorize_url(server, port, &state)?;

    eprintln!("Opening your browser to authorize Sotto…");
    eprintln!("If it doesn't open, visit:\n  {url}\n");
    open_browser(&url);

    accept_callback(&listener, &state)
}

/// Accept exactly one loopback connection, capture the callback, and reply to the browser.
fn accept_callback(listener: &TcpListener, expected_state: &str) -> Result<String> {
    let (mut stream, _) = listener.accept().map_err(|e| Error::Io(e.to_string()))?;
    let mut buf = [0u8; 4096];
    let n = stream
        .read(&mut buf)
        .map_err(|e| Error::Io(e.to_string()))?;
    let request = String::from_utf8_lossy(&buf[..n]);

    // Request line: `GET /?session=…&state=… HTTP/1.1`.
    let target = request
        .split_whitespace()
        .nth(1)
        .ok_or_else(|| Error::Server("malformed callback request".into()))?;
    let result = parse_callback(target, expected_state);

    let (status, body) = match result {
        Ok(_) => (
            "200 OK",
            "<html><body>Sotto: login complete — you can close this tab.</body></html>",
        ),
        Err(_) => (
            "400 Bad Request",
            "<html><body>Sotto: login failed.</body></html>",
        ),
    };
    let _ = write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    result
}

/// Best-effort browser open; failure is fine (the URL is printed too).
fn open_browser(url: &str) {
    #[cfg(target_os = "macos")]
    let mut command = {
        let mut c = std::process::Command::new("open");
        c.arg(url);
        c
    };
    #[cfg(target_os = "windows")]
    let mut command = {
        let mut c = std::process::Command::new("cmd");
        c.args(["/C", "start", "", url]);
        c
    };
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let mut command = {
        let mut c = std::process::Command::new("xdg-open");
        c.arg(url);
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
    }

    #[test]
    fn parse_callback_extracts_session() {
        let token = parse_callback("/?session=st_xyz&state=abc", "abc").unwrap();
        assert_eq!(token, "st_xyz");
    }

    #[test]
    fn parse_callback_rejects_state_mismatch_and_missing_fields() {
        assert!(parse_callback("/?session=st_xyz&state=evil", "abc").is_err());
        assert!(parse_callback("/?state=abc", "abc").is_err());
        assert!(parse_callback("/?session=st_xyz", "abc").is_err());
    }
}
