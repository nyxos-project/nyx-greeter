//! greetd's IPC: JSON messages over the Unix socket in `$GREETD_SOCK`, each prefixed
//! with its length as a 32-bit integer in native byte order. The greeter creates a
//! session for a user, answers PAM's questions (password, fingerprint prompts), then
//! starts the session; greetd runs it once the greeter exits.
//!
//! [`fake`] is a stand-in greetd for tests and `--demo`.

use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;

pub mod fake;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    CreateSession {
        username: String,
    },
    PostAuthMessageResponse {
        #[serde(skip_serializing_if = "Option::is_none")]
        response: Option<String>,
    },
    StartSession {
        cmd: Vec<String>,
        env: Vec<String>,
    },
    CancelSession,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorType {
    AuthError,
    Error,
}

/// What PAM asks: `Visible` and `Secret` want an answer, `Info` and `Error` only want
/// to be shown (answer them with no response to go on).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthMessageType {
    Visible,
    Secret,
    Info,
    Error,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Response {
    Success,
    Error {
        error_type: ErrorType,
        description: String,
    },
    AuthMessage {
        auth_message_type: AuthMessageType,
        auth_message: String,
    },
}

pub fn write_message<T: Serialize>(stream: &mut impl Write, msg: &T) -> std::io::Result<()> {
    let body = serde_json::to_vec(msg).map_err(std::io::Error::other)?;
    stream.write_all(&(body.len() as u32).to_ne_bytes())?;
    stream.write_all(&body)?;
    stream.flush()
}

pub fn read_message<T: for<'de> Deserialize<'de>>(stream: &mut impl Read) -> std::io::Result<T> {
    let mut len = [0u8; 4];
    stream.read_exact(&mut len)?;
    let len = u32::from_ne_bytes(len) as usize;
    if len > 1 << 20 {
        return Err(std::io::Error::other("greetd sent a message over 1 MB"));
    }
    let mut body = vec![0u8; len];
    stream.read_exact(&mut body)?;
    serde_json::from_slice(&body).map_err(std::io::Error::other)
}

pub struct Client {
    stream: UnixStream,
}

impl Client {
    /// Connect to `$GREETD_SOCK`, or `path`.
    pub fn connect(path: Option<&Path>) -> std::io::Result<Client> {
        let path = match path {
            Some(p) => p.to_path_buf(),
            None => std::env::var_os("GREETD_SOCK")
                .map(Into::into)
                .ok_or_else(|| {
                    std::io::Error::other("GREETD_SOCK isn't set (is greetd running this greeter?)")
                })?,
        };
        Ok(Client {
            stream: UnixStream::connect(path)?,
        })
    }

    pub fn request(&mut self, req: &Request) -> std::io::Result<Response> {
        write_message(&mut self.stream, req)?;
        read_message(&mut self.stream)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_format_matches_greetd() {
        let mut buf = vec![];
        write_message(
            &mut buf,
            &Request::CreateSession {
                username: "ken".into(),
            },
        )
        .unwrap();
        let len = u32::from_ne_bytes(buf[..4].try_into().unwrap()) as usize;
        assert_eq!(
            std::str::from_utf8(&buf[4..4 + len]).unwrap(),
            r#"{"type":"create_session","username":"ken"}"#
        );
        let mut buf = vec![];
        write_message(
            &mut buf,
            &Request::PostAuthMessageResponse { response: None },
        )
        .unwrap();
        assert!(
            std::str::from_utf8(&buf[4..])
                .unwrap()
                .ends_with(r#""post_auth_message_response"}"#)
        );
        let r: Response = serde_json::from_str(
            r#"{"type":"auth_message","auth_message_type":"secret","auth_message":"Password: "}"#,
        )
        .unwrap();
        assert_eq!(
            r,
            Response::AuthMessage {
                auth_message_type: AuthMessageType::Secret,
                auth_message: "Password: ".into()
            }
        );
        let r: Response = serde_json::from_str(
            r#"{"type":"error","error_type":"auth_error","description":"nope"}"#,
        )
        .unwrap();
        assert!(matches!(
            r,
            Response::Error {
                error_type: ErrorType::AuthError,
                ..
            }
        ));
    }
}
