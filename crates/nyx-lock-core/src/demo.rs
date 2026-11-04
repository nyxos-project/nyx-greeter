//! The same events as PAM's, without PAM, for `--demo`: the password is `nyx`, a
//! fingerprint is asked for and not recognised once, then recognised; a face scan
//! finds no one.

use crate::{Event, Method};
use std::sync::mpsc::{Sender, channel};
use std::time::Duration;

pub fn start(method: Method, events: impl Fn(Event) + Send + 'static) -> Sender<String> {
    let (tx, rx) = channel::<String>();
    std::thread::spawn(move || match method {
        Method::Password => loop {
            events(Event::Prompt { method, text: "Password".into(), secret: true });
            let Ok(answer) = rx.recv() else { return };
            std::thread::sleep(Duration::from_millis(400));
            if answer == "nyx" {
                events(Event::Granted(method));
                return;
            }
            events(Event::Denied { method, text: "That password didn't work".into() });
        },
        Method::Fingerprint => {
            events(Event::Info { method, text: "Touch the fingerprint reader".into() });
            std::thread::sleep(Duration::from_secs(6));
            events(Event::Denied { method, text: "Fingerprint not recognised".into() });
        }
        Method::Face => {
            events(Event::Info { method, text: "Looking for you…".into() });
            std::thread::sleep(Duration::from_secs(3));
            events(Event::Denied { method, text: "Face not recognised".into() });
        }
        Method::Key => {
            events(Event::Unavailable { method, text: "No security key plugged in".into() });
        }
    });
    tx
}
