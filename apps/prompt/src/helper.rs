//! A conversation with polkit's agent helper over its socket: the user and the cookie go
//! in, PAM's prompts come out, and the helper tells polkit the result itself.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::sync::mpsc;

pub const SOCKET: &str = "/run/polkit/agent-helper.socket";

#[derive(Debug, Clone, PartialEq)]
pub enum Line {
    /// PAM asks for something; `echo` when what's typed may show.
    Request {
        text: String,
        echo: bool,
    },
    Error(String),
    Info(String),
    /// Polkit has the answer: allowed or not.
    Done(bool),
}

/// One line from the helper, after its C escapes are undone.
pub fn parse(raw: &str) -> Line {
    let line = unescape(raw.trim_end_matches('\n'));
    if let Some(s) = line.strip_prefix("PAM_PROMPT_ECHO_OFF ") {
        Line::Request { text: s.to_string(), echo: false }
    } else if let Some(s) = line.strip_prefix("PAM_PROMPT_ECHO_ON ") {
        Line::Request { text: s.to_string(), echo: true }
    } else if let Some(s) = line.strip_prefix("PAM_ERROR_MSG ") {
        Line::Error(s.to_string())
    } else if let Some(s) = line.strip_prefix("PAM_TEXT_INFO ") {
        Line::Info(s.to_string())
    } else {
        Line::Done(line.starts_with("SUCCESS"))
    }
}

/// `g_strcompress`: `\n`, `\t`, `\\`, `\"` and octal escapes.
fn unescape(s: &str) -> String {
    let mut out = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some(d @ '0'..='7') => {
                let mut n = d.to_digit(8).unwrap_or(0);
                for _ in 0..2 {
                    match chars.peek().and_then(|c| c.to_digit(8)) {
                        Some(v) => {
                            n = n * 8 + v;
                            chars.next();
                        }
                        None => break,
                    }
                }
                out.push(char::from_u32(n).unwrap_or('?'));
            }
            Some(other) => out.push(other),
            None => out.push('\\'),
        }
    }
    out
}

/// Authenticate `user` for `cookie` on its own thread. Lines go to `lines`; answers
/// to the helper's requests come from the returned sender. Dropping that sender ends
/// the conversation (a cancel).
pub fn start(user: &str, cookie: &str, lines: async_channel::Sender<Line>) -> mpsc::Sender<String> {
    let (answers, rx) = mpsc::channel::<String>();
    let (user, cookie) = (user.to_string(), cookie.to_string());
    std::thread::spawn(move || {
        let done = |ok| {
            let _ = lines.send_blocking(Line::Done(ok));
        };
        let Ok(mut stream) = UnixStream::connect(SOCKET) else {
            let _ = lines.send_blocking(Line::Error("Polkit's helper isn't running".into()));
            return done(false);
        };
        if stream.write_all(format!("{user}\n{cookie}\n").as_bytes()).is_err() {
            return done(false);
        }
        let Ok(read) = stream.try_clone() else { return done(false) };
        for raw in BufReader::new(read).lines() {
            let Ok(raw) = raw else { return done(false) };
            let line = parse(&raw);
            let ask = matches!(line, Line::Request { .. });
            let end = matches!(line, Line::Done(_));
            let _ = lines.send_blocking(line);
            if end {
                return;
            }
            if ask {
                let Ok(answer) = rx.recv() else { return };
                if stream.write_all(format!("{answer}\n").as_bytes()).is_err() {
                    return done(false);
                }
            }
        }
        done(false);
    });
    answers
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_helpers_lines() {
        assert_eq!(parse("PAM_PROMPT_ECHO_OFF Password: \n"), Line::Request { text: "Password: ".into(), echo: false });
        assert_eq!(
            parse("PAM_TEXT_INFO Place your finger\\non the reader"),
            Line::Info("Place your finger\non the reader".into())
        );
        assert_eq!(parse("PAM_ERROR_MSG nope"), Line::Error("nope".into()));
        assert_eq!(parse("SUCCESS"), Line::Done(true));
        assert_eq!(parse("FAILURE"), Line::Done(false));
        assert_eq!(unescape("a\\101\\\\b"), "aA\\b");
    }
}
