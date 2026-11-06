//! Nyx Administrator Prompt: NyxOS's polkit authentication agent. When an app asks for
//! something only an administrator may do (install software, change system settings),
//! polkit asks this agent, which shows one card over a dimmed screen in the Nyx theme
//! and talks to polkit's own helper (`/run/polkit/agent-helper.socket`) to check the
//! password. Face and fingerprint work through the `polkit-1` PAM stack, as anywhere.
//!
//! It registers for the user's graphical session and runs until the session ends.
//! `--demo` shows the card without polkit (the password is `nyx`).

mod agent;
mod dialog;
mod helper;

use gtk::prelude::*;

pub const APP_ID: &str = "io.github.design_nexus.nyx.Prompt";

fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "-h" || a == "--help") {
        println!(
            "Usage: nyx-prompt [--demo]\n\n\
             The polkit authentication agent for this graphical session.\n\
             --demo  show the card once without polkit; the password is \"nyx\""
        );
        return std::process::ExitCode::SUCCESS;
    }
    let demo = args.iter().any(|a| a == "--demo");
    let id = std::env::var("NYX_PROMPT_APP_ID").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| APP_ID.into());
    let app = gtk::Application::builder().application_id(id).flags(gtk::gio::ApplicationFlags::NON_UNIQUE).build();
    app.connect_startup(|_| dialog::install_css());
    let code = std::rc::Rc::new(std::cell::Cell::new(0u8));
    let c = code.clone();
    app.connect_activate(move |app| {
        std::mem::forget(app.hold());
        if demo {
            dialog::demo(app);
        } else if let Err(e) = agent::register(app) {
            eprintln!("nyx-prompt: {e}");
            c.set(1);
            app.quit();
        }
    });
    app.run_with_args::<&str>(&[]);
    std::process::ExitCode::from(code.get())
}
