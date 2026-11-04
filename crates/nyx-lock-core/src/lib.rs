//! Nyx Locker's authentication, without GTK.
//!
//! Each way of unlocking is its own PAM conversation on its own thread, with its own
//! PAM service (`nyx-lock-password`, `nyx-lock-fingerprint`, `nyx-lock-face`,
//! `nyx-lock-key`), all at once: whichever succeeds first unlocks. A method's prompts
//! (the password, a security key's PIN) are answered from the screen; its messages and
//! result come back as [`Event`]s. [`demo`] plays the same events without PAM.

pub mod demo;

use pam_client2::{ConversationHandler, ErrorCode, Flag};
use serde::Deserialize;
use std::ffi::{CStr, CString};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, channel};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Method {
    Password,
    Fingerprint,
    Face,
    #[serde(rename = "security-key")]
    Key,
}

impl Method {
    /// The PAM service NyxOS sets up for it.
    pub fn service(self) -> &'static str {
        match self {
            Method::Password => "nyx-lock-password",
            Method::Fingerprint => "nyx-lock-fingerprint",
            Method::Face => "nyx-lock-face",
            Method::Key => "nyx-lock-key",
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Method::Password => "Password",
            Method::Fingerprint => "Fingerprint",
            Method::Face => "Face",
            Method::Key => "Security key",
        }
    }
}

/// `/etc/nyx-locker/config.toml`, which NyxOS writes: the methods it set up PAM
/// services for. The password is always there.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Config {
    pub methods: Vec<Method>,
}

impl Config {
    pub fn path() -> PathBuf {
        std::env::var_os("NYX_LOCKER_CONFIG")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| "/etc/nyx-locker/config.toml".into())
    }

    pub fn load() -> Config {
        std::fs::read_to_string(Config::path()).ok().and_then(|t| toml::from_str(&t).ok()).unwrap_or_default()
    }

    /// The password first, then the others, each once.
    pub fn methods(&self) -> Vec<Method> {
        let mut out = vec![Method::Password];
        for m in &self.methods {
            if !out.contains(m) {
                out.push(*m);
            }
        }
        out
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// The method asks for something to type (`secret`: don't show it).
    Prompt { method: Method, text: String, secret: bool },
    Info { method: Method, text: String },
    Error { method: Method, text: String },
    /// It worked: unlock.
    Granted(Method),
    /// It didn't match; the method can be tried again.
    Denied { method: Method, text: String },
    /// It can't be used now (no reader, no key, the service is missing).
    Unavailable { method: Method, text: String },
}

/// What PAM's result means to the person.
pub fn denial(method: Method, code: ErrorCode) -> Event {
    let text = |t: &str| t.to_string();
    match code {
        ErrorCode::AUTH_ERR | ErrorCode::USER_UNKNOWN | ErrorCode::PERM_DENIED | ErrorCode::CRED_ERR => Event::Denied {
            method,
            text: match method {
                Method::Password => text("That password didn't work"),
                Method::Fingerprint => text("Fingerprint not recognised"),
                Method::Face => text("Face not recognised"),
                Method::Key => text("Security key not accepted"),
            },
        },
        ErrorCode::MAXTRIES => Event::Denied { method, text: text("Too many tries. Use your password") },
        _ => Event::Unavailable { method, text: format!("{} isn't available", method.name()) },
    }
}

struct Conversation<F: Fn(Event)> {
    method: Method,
    events: F,
    answers: Receiver<String>,
}

impl<F: Fn(Event)> Conversation<F> {
    fn ask(&mut self, prompt: &CStr, secret: bool) -> Result<CString, ErrorCode> {
        let text = prompt.to_string_lossy().trim().trim_end_matches(':').to_string();
        (self.events)(Event::Prompt { method: self.method, text, secret });
        let answer = self.answers.recv().map_err(|_| ErrorCode::CONV_ERR)?;
        CString::new(answer).map_err(|_| ErrorCode::CONV_ERR)
    }
}

impl<F: Fn(Event)> ConversationHandler for Conversation<F> {
    fn prompt_echo_on(&mut self, prompt: &CStr) -> Result<CString, ErrorCode> {
        self.ask(prompt, false)
    }

    fn prompt_echo_off(&mut self, prompt: &CStr) -> Result<CString, ErrorCode> {
        self.ask(prompt, true)
    }

    fn text_info(&mut self, msg: &CStr) {
        (self.events)(Event::Info { method: self.method, text: msg.to_string_lossy().trim().to_string() });
    }

    fn error_msg(&mut self, msg: &CStr) {
        (self.events)(Event::Error { method: self.method, text: msg.to_string_lossy().trim().to_string() });
    }
}

/// Run `method` once for `user` on its own thread. Answers to its prompts go to the
/// returned sender.
pub fn start(method: Method, user: &str, events: impl Fn(Event) + Send + Clone + 'static) -> Sender<String> {
    let (tx, rx) = channel();
    let user = user.to_string();
    std::thread::spawn(move || {
        let conv = Conversation { method, events: events.clone(), answers: rx };
        let result = pam_client2::Context::new(method.service(), Some(&user), conv).and_then(|mut ctx| {
            ctx.authenticate(Flag::NONE)?;
            ctx.acct_mgmt(Flag::NONE)
        });
        events(match result {
            Ok(()) => Event::Granted(method),
            Err(e) => denial(method, e.code()),
        });
    });
    tx
}

/// Whose session this is.
pub fn user() -> String {
    std::env::var("USER").ok().filter(|u| !u.is_empty()).unwrap_or_else(|| {
        let uid = std::fs::read_to_string("/proc/self/status")
            .ok()
            .and_then(|s| s.lines().find_map(|l| l.strip_prefix("Uid:")).and_then(|v| v.split_whitespace().next().map(String::from)))
            .unwrap_or_default();
        std::fs::read_to_string("/etc/passwd")
            .unwrap_or_default()
            .lines()
            .find_map(|l| {
                let f: Vec<&str> = l.split(':').collect();
                (f.get(2) == Some(&uid.as_str())).then(|| f[0].to_string())
            })
            .unwrap_or_default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_password_is_always_first_and_methods_are_unique() {
        let c: Config = toml::from_str("methods = [\"fingerprint\", \"face\", \"fingerprint\", \"security-key\"]").unwrap();
        assert_eq!(c.methods(), vec![Method::Password, Method::Fingerprint, Method::Face, Method::Key]);
        assert_eq!(Config::default().methods(), vec![Method::Password]);
    }

    #[test]
    fn results_read_as_words() {
        assert!(matches!(denial(Method::Password, ErrorCode::AUTH_ERR), Event::Denied { text, .. } if text == "That password didn't work"));
        assert!(matches!(denial(Method::Face, ErrorCode::AUTHINFO_UNAVAIL), Event::Unavailable { .. }));
    }
}
