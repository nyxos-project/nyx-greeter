//! A fake greetd: the same protocol on a socket of your choosing, with PAM played by
//! rules. Every user's password is `password` (default `nyx`); with `fingerprint`,
//! PAM first says "Place your finger on the reader" and waits, as pam_fprintd does.
//! Started sessions are recorded instead of run.

use crate::{AuthMessageType, ErrorType, Request, Response, read_message, write_message};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone)]
pub struct Rules {
    pub password: String,
    pub fingerprint: bool,
    /// How long the "reader" waits before giving up on the finger.
    pub finger_wait: std::time::Duration,
}

impl Default for Rules {
    fn default() -> Self {
        Rules {
            password: "nyx".into(),
            fingerprint: false,
            finger_wait: std::time::Duration::ZERO,
        }
    }
}

/// What the fake saw, for tests.
#[derive(Debug, Default, Clone)]
pub struct Log {
    pub started: Vec<(String, Vec<String>, Vec<String>)>,
    pub failures: u32,
}

pub struct Fake {
    pub path: PathBuf,
    pub log: Arc<Mutex<Log>>,
}

impl Drop for Fake {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[derive(PartialEq)]
enum Step {
    Idle,
    Finger(String),
    Password(String),
    Authenticated(String),
}

pub fn serve(path: &Path, rules: Rules) -> std::io::Result<Fake> {
    let _ = std::fs::remove_file(path);
    let listener = UnixListener::bind(path)?;
    let log: Arc<Mutex<Log>> = Arc::default();
    let l = log.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let (rules, log) = (rules.clone(), l.clone());
            std::thread::spawn(move || {
                let mut s = stream;
                let mut step = Step::Idle;
                while let Ok(req) = read_message::<Request>(&mut s) {
                    let reply = match (req, &step) {
                        (Request::CreateSession { username }, Step::Idle) => {
                            if rules.fingerprint {
                                step = Step::Finger(username);
                                Response::AuthMessage {
                                    auth_message_type: AuthMessageType::Info,
                                    auth_message: "Place your finger on the reader".into(),
                                }
                            } else {
                                step = Step::Password(username);
                                Response::AuthMessage {
                                    auth_message_type: AuthMessageType::Secret,
                                    auth_message: "Password: ".into(),
                                }
                            }
                        }
                        (Request::CreateSession { .. }, _) => Response::Error {
                            error_type: ErrorType::Error,
                            description: "a session is already being configured".into(),
                        },
                        (Request::PostAuthMessageResponse { .. }, Step::Finger(u)) => {
                            // The finger didn't match; fall back to the password, as PAM does.
                            std::thread::sleep(rules.finger_wait);
                            step = Step::Password(u.clone());
                            Response::AuthMessage {
                                auth_message_type: AuthMessageType::Secret,
                                auth_message: "Password: ".into(),
                            }
                        }
                        (Request::PostAuthMessageResponse { response }, Step::Password(u)) => {
                            if response.as_deref() == Some(rules.password.as_str()) {
                                step = Step::Authenticated(u.clone());
                                Response::Success
                            } else {
                                log.lock().unwrap().failures += 1;
                                step = Step::Idle;
                                Response::Error {
                                    error_type: ErrorType::AuthError,
                                    description: "Authentication failure".into(),
                                }
                            }
                        }
                        (Request::StartSession { cmd, env }, Step::Authenticated(u)) => {
                            log.lock().unwrap().started.push((u.clone(), cmd, env));
                            step = Step::Idle;
                            Response::Success
                        }
                        (Request::CancelSession, _) => {
                            step = Step::Idle;
                            Response::Success
                        }
                        _ => Response::Error {
                            error_type: ErrorType::Error,
                            description: "not now".into(),
                        },
                    };
                    if write_message(&mut s, &reply).is_err() {
                        return;
                    }
                }
            });
        }
    });
    Ok(Fake {
        path: path.to_path_buf(),
        log,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Client;

    #[test]
    fn a_whole_login() {
        let dir = std::env::temp_dir().join(format!("nyx-greetd-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let fake = serve(
            &dir.join("sock"),
            Rules {
                fingerprint: true,
                ..Default::default()
            },
        )
        .unwrap();
        let mut c = Client::connect(Some(&fake.path)).unwrap();
        let r = c
            .request(&Request::CreateSession {
                username: "ken".into(),
            })
            .unwrap();
        assert!(matches!(
            r,
            Response::AuthMessage {
                auth_message_type: AuthMessageType::Info,
                ..
            }
        ));
        let r = c
            .request(&Request::PostAuthMessageResponse { response: None })
            .unwrap();
        assert!(matches!(
            r,
            Response::AuthMessage {
                auth_message_type: AuthMessageType::Secret,
                ..
            }
        ));
        let r = c
            .request(&Request::PostAuthMessageResponse {
                response: Some("wrong".into()),
            })
            .unwrap();
        assert!(matches!(
            r,
            Response::Error {
                error_type: ErrorType::AuthError,
                ..
            }
        ));
        c.request(&Request::CreateSession {
            username: "ken".into(),
        })
        .unwrap();
        c.request(&Request::PostAuthMessageResponse { response: None })
            .unwrap();
        assert_eq!(
            c.request(&Request::PostAuthMessageResponse {
                response: Some("nyx".into())
            })
            .unwrap(),
            Response::Success
        );
        assert_eq!(
            c.request(&Request::StartSession {
                cmd: vec!["Hyprland".into()],
                env: vec![]
            })
            .unwrap(),
            Response::Success
        );
        let log = fake.log.lock().unwrap();
        assert_eq!(log.failures, 1);
        assert_eq!(log.started[0].0, "ken");
    }
}
