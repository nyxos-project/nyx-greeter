//! Nyx Greeter: the NyxOS login screen. greetd runs it in a small Hyprland session
//! (`data/hyprland.lua`); it shows the theme's wallpaper on every monitor and the
//! login card on the largest, talks to greetd for PAM (passwords, fingerprints, keys),
//! then starts the chosen session and exits.
//!
//! `--demo` runs against a fake greetd (every password is `nyx`). `--only <output>`
//! shows it on one output only, without taking the keyboard, for screenshots.

mod ui;

use gtk::prelude::*;

pub const APP_ID: &str = "io.github.design_nexus.nyx.Greeter";

#[derive(Debug, Clone, Default)]
pub struct Options {
    pub only: Option<String>,
    pub demo: bool,
    /// Demo: the fake asks for a finger first.
    pub finger: bool,
    /// Tests: type this into the first prompt.
    pub type_text: Option<String>,
    /// Tests: open the user list.
    pub show_users: bool,
}

fn main() -> std::process::ExitCode {
    let mut o = Options::default();
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--only" => o.only = args.next(),
            "--demo" => o.demo = true,
            "--demo-finger" => {
                o.demo = true;
                o.finger = true;
            }
            "--type" => o.type_text = args.next(),
            "--show-users" => o.show_users = true,
            "--version" => {
                println!("nyx-greeter {}", env!("CARGO_PKG_VERSION"));
                return std::process::ExitCode::SUCCESS;
            }
            "-h" | "--help" => {
                println!(
                    "Usage: nyx-greeter [--demo | --demo-finger] [--only <output>]\n\n\
                     Run by greetd (it reads $GREETD_SOCK). Settings: /etc/nyx-greeter/config.toml.\n\
                     --demo         a fake greetd; every password is \"nyx\"; power buttons do nothing\n\
                     --demo-finger  the same, asking for a fingerprint first\n\
                     --only         show on this output only, without taking the keyboard"
                );
                return std::process::ExitCode::SUCCESS;
            }
            other => {
                eprintln!("nyx-greeter: unknown option {other}");
                return std::process::ExitCode::from(2);
            }
        }
    }
    let config = nyx_greeter_core::Config::load();
    ui::theme(&config.theme);
    let id = std::env::var("NYX_GREETER_APP_ID")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| APP_ID.into());
    let app = gtk::Application::builder()
        .application_id(id)
        .flags(gtk::gio::ApplicationFlags::NON_UNIQUE)
        .build();
    app.connect_startup(|_| ui::install_css());
    let started = std::cell::Cell::new(false);
    app.connect_activate(move |app| {
        if !started.replace(true) {
            ui::start(app, config.clone(), o.clone());
        }
    });
    let code = app.run_with_args::<&str>(&[]);
    std::process::ExitCode::from(code.get() as u8)
}
