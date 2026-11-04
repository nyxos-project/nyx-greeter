//! Nyx Locker: the NyxOS lock screen. It locks the session with the Wayland
//! session-lock protocol (the compositor keeps it locked if the locker stops), draws the
//! lock-screen theme on every monitor, and unlocks when any way in succeeds: the
//! password, a fingerprint, a face (scanned when you come back, not all the time) or a
//! security key, each its own PAM service at once. No notifications.
//!
//! `--demo` shows it in a normal window without locking anything (the password is
//! `nyx`); `--style` picks the indicator for a preview.

mod ui;

use gtk::prelude::*;

pub const APP_ID: &str = "io.github.design_nexus.nyx.Locker";

/// Another locker has the session: don't restart.
pub const EXIT_ALREADY_LOCKED: u8 = 3;

#[derive(Debug, Clone, Default)]
pub struct Options {
    pub demo: bool,
    pub style: Option<nyx_signin_ui::Style>,
}

fn main() -> std::process::ExitCode {
    let mut o = Options::default();
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--demo" => o.demo = true,
            "--style" => match args.next().as_deref().and_then(nyx_signin_ui::Style::parse) {
                Some(s) => o.style = Some(s),
                None => {
                    eprintln!("nyx-locker: --style is circular, cyberpunk, minimal or edge");
                    return std::process::ExitCode::from(2);
                }
            },
            "--version" => {
                println!("nyx-locker {}", env!("CARGO_PKG_VERSION"));
                return std::process::ExitCode::SUCCESS;
            }
            "-h" | "--help" => {
                println!(
                    "Usage: nyx-locker [--demo] [--style circular|cyberpunk|minimal|edge]\n\n\
                     Locks the session until you sign in again. Ways in: /etc/nyx-locker/config.toml.\n\
                     The theme and indicator: appearance.toml's lock-theme and lock-style.\n\
                     --demo   a window, nothing locked; the password is \"nyx\"\n\
                     --style  the indicator, for a preview"
                );
                return std::process::ExitCode::SUCCESS;
            }
            other => {
                eprintln!("nyx-locker: unknown option {other}");
                return std::process::ExitCode::from(2);
            }
        }
    }
    if let Err(e) = gtk::init() {
        eprintln!("nyx-locker: {e}");
        return std::process::ExitCode::from(1);
    }
    if !o.demo && !gtk4_session_lock::is_supported() {
        eprintln!("nyx-locker: this compositor can't lock the session (ext-session-lock-v1)");
        return std::process::ExitCode::from(1);
    }
    let look = ui::Look::load(o.style);
    ui::theme(&look.theme);
    let id = std::env::var("NYX_LOCKER_APP_ID").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| APP_ID.into());
    let app = gtk::Application::builder().application_id(id).flags(gtk::gio::ApplicationFlags::NON_UNIQUE).build();
    app.connect_startup(|_| ui::install_css());
    let started = std::cell::Cell::new(false);
    let code = std::rc::Rc::new(std::cell::Cell::new(0u8));
    let c = code.clone();
    app.connect_activate(move |app| {
        if !started.replace(true) {
            ui::start(app, look.clone(), o.clone(), c.clone());
        }
    });
    app.run_with_args::<&str>(&[]);
    std::process::ExitCode::from(code.get())
}
