//! The conversation with greetd, on its own thread so the UI never waits on PAM (a
//! fingerprint reader can take seconds). The UI sends [`Command`]s and gets
//! [`Event`]s back.

use nyx_greetd::{AuthMessageType, Client, ErrorType, Request, Response};
use std::path::PathBuf;
use std::sync::mpsc;

#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    /// Start over for this user (cancels any login in progress).
    Login(String),
    /// The answer to the current prompt.
    Answer(String),
    /// Start this session for the authenticated user.
    Start {
        cmd: Vec<String>,
        env: Vec<String>,
    },
    Cancel,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// PAM wants an answer; `secret` hides what's typed.
    Prompt {
        text: String,
        secret: bool,
    },
    /// PAM says something and goes on by itself (a fingerprint reader waiting).
    Info(String),
    /// PAM reports a problem and goes on.
    Notice(String),
    Authenticated,
    /// Wrong password, locked account, …; the greeter starts over.
    Failed(String),
    /// greetd will start the session as soon as the greeter exits.
    Started,
    /// greetd couldn't be reached or broke the protocol.
    Broken(String),
}

/// greetd's errors in words: it says what PAM said, as text (the fake greetd) or as
/// PAM's error names (`pam_authenticate: AUTH_ERR`).
fn friendly(desc: &str) -> String {
    let d = desc.to_lowercase();
    if d.contains("maxtries") || d.contains("locked") || d.contains("maximum") {
        "Too many tries. Wait a moment, then try again.".into()
    } else if d.contains("expired") || d.contains("new_authtok_reqd") {
        "This password has expired.".into()
    } else if d.contains("authentication failure")
        || d.contains("auth") && d.contains("fail")
        || ["auth_err", "perm_denied", "user_unknown", "cred_err", "cred_insufficient"].iter().any(|e| d.contains(e))
    {
        "That password didn't work. Try again.".into()
    } else {
        desc.trim().to_string()
    }
}

/// Start the flow. `socket` overrides `$GREETD_SOCK`.
pub fn spawn(
    socket: Option<PathBuf>,
    events: impl Fn(Event) + Send + 'static,
) -> mpsc::Sender<Command> {
    let (tx, rx) = mpsc::channel::<Command>();
    std::thread::Builder::new()
        .name("greetd".into())
        .spawn(move || {
            let mut client = match Client::connect(socket.as_deref()) {
                Ok(c) => c,
                Err(e) => {
                    events(Event::Broken(format!("Can't reach greetd: {e}")));
                    return;
                }
            };
            let mut active = false;
            // Hand greetd's reply to the UI, answering what needs no answer.
            let handle = |client: &mut Client,
                          mut reply: std::io::Result<Response>,
                          active: &mut bool| loop {
                match reply {
                    Ok(Response::AuthMessage {
                        auth_message_type,
                        auth_message,
                    }) => {
                        let text = auth_message.trim().trim_end_matches(':').trim().to_string();
                        match auth_message_type {
                            AuthMessageType::Secret | AuthMessageType::Visible => {
                                events(Event::Prompt {
                                    text,
                                    secret: auth_message_type == AuthMessageType::Secret,
                                });
                                return;
                            }
                            AuthMessageType::Info => events(Event::Info(text)),
                            AuthMessageType::Error => events(Event::Notice(text)),
                        }
                        reply =
                            client.request(&Request::PostAuthMessageResponse { response: None });
                    }
                    Ok(Response::Success) => {
                        events(Event::Authenticated);
                        return;
                    }
                    Ok(Response::Error {
                        error_type,
                        description,
                    }) => {
                        let _ = client.request(&Request::CancelSession);
                        *active = false;
                        events(match error_type {
                            ErrorType::AuthError => Event::Failed(friendly(&description)),
                            ErrorType::Error => Event::Failed(description),
                        });
                        return;
                    }
                    Err(e) => {
                        events(Event::Broken(format!("Lost greetd: {e}")));
                        return;
                    }
                }
            };
            for cmd in rx {
                match cmd {
                    Command::Login(user) => {
                        if active {
                            let _ = client.request(&Request::CancelSession);
                        }
                        active = true;
                        let r = client.request(&Request::CreateSession { username: user });
                        handle(&mut client, r, &mut active);
                    }
                    Command::Answer(text) => {
                        let r = client.request(&Request::PostAuthMessageResponse {
                            response: Some(text),
                        });
                        handle(&mut client, r, &mut active);
                    }
                    Command::Start { cmd, env } => {
                        match client.request(&Request::StartSession { cmd, env }) {
                            Ok(Response::Success) => events(Event::Started),
                            Ok(Response::Error { description, .. }) => {
                                events(Event::Failed(description))
                            }
                            Ok(_) => events(Event::Broken("greetd answered oddly".into())),
                            Err(e) => events(Event::Broken(format!("Lost greetd: {e}"))),
                        }
                    }
                    Command::Cancel => {
                        if active {
                            let _ = client.request(&Request::CancelSession);
                            active = false;
                        }
                    }
                }
            }
        })
        .expect("spawn the greetd thread");
    tx
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pam_error_names_read_as_words() {
        assert_eq!(friendly("pam_authenticate: AUTH_ERR"), "That password didn't work. Try again.");
        assert_eq!(friendly("pam_authenticate: MAXTRIES"), "Too many tries. Wait a moment, then try again.");
        assert_eq!(friendly("pam_acct_mgmt: ACCT_EXPIRED"), "This password has expired.");
        assert_eq!(friendly("Authentication failure"), "That password didn't work. Try again.");
    }
    use nyx_greetd::fake;
    use std::time::Duration;

    fn next(rx: &mpsc::Receiver<Event>) -> Event {
        rx.recv_timeout(Duration::from_secs(3)).expect("an event")
    }

    #[test]
    fn fingerprint_then_password_then_start() {
        let t = tempfile::tempdir().unwrap();
        let f = fake::serve(
            &t.path().join("g.sock"),
            fake::Rules {
                fingerprint: true,
                ..Default::default()
            },
        )
        .unwrap();
        let (etx, erx) = mpsc::channel();
        let tx = spawn(Some(f.path.clone()), move |e| {
            let _ = etx.send(e);
        });
        tx.send(Command::Login("ken".into())).unwrap();
        assert_eq!(
            next(&erx),
            Event::Info("Place your finger on the reader".into())
        );
        assert_eq!(
            next(&erx),
            Event::Prompt {
                text: "Password".into(),
                secret: true
            }
        );
        tx.send(Command::Answer("bad".into())).unwrap();
        assert_eq!(
            next(&erx),
            Event::Failed("That password didn't work. Try again.".into())
        );
        tx.send(Command::Login("ken".into())).unwrap();
        next(&erx);
        next(&erx);
        tx.send(Command::Answer("nyx".into())).unwrap();
        assert_eq!(next(&erx), Event::Authenticated);
        tx.send(Command::Start {
            cmd: vec!["Hyprland".into()],
            env: vec![],
        })
        .unwrap();
        assert_eq!(next(&erx), Event::Started);
        assert_eq!(f.log.lock().unwrap().started.len(), 1);
    }

    #[test]
    fn no_greetd() {
        let (etx, erx) = mpsc::channel();
        let _tx = spawn(Some("/nonexistent/greetd.sock".into()), move |e| {
            let _ = etx.send(e);
        });
        assert!(matches!(next(&erx), Event::Broken(_)));
    }
}
