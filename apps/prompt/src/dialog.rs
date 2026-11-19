//! The card: who's asked, what for, the password, and Cancel or Continue. One at a time,
//! on an overlay layer over a dimmed screen, holding the keyboard until it's answered.

use crate::agent::Request;
use crate::helper::{self, Line};
use gtk::prelude::*;
use gtk::{gdk, glib};
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use std::cell::{Cell, RefCell};
use std::rc::Rc;

/// Nyx Biometrics' gate skips face, fingerprint and keys in administrator prompts while
/// this is fresh in the person's runtime folder, so the password comes first.
fn marker(user: &str, uid: u32) -> std::path::PathBuf {
    // NixOS gives each configured local account a private directory that is
    // visible even in the socket-activated helper's ProtectHome sandbox.
    let managed = std::path::Path::new("/run/nyx-auth/password-first").join(user);
    if uid_of(user) == Some(uid) && managed.is_dir() {
        return managed.join("request");
    }
    std::path::PathBuf::from(format!("/run/user/{uid}/nyx-auth/password-first"))
}

/// This process's user ID.
fn my_uid() -> Option<u32> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata("/proc/self").ok().map(|m| m.uid())
}

fn uid_of(user: &str) -> Option<u32> {
    std::fs::read_to_string("/etc/passwd").ok()?.lines().find_map(|l| {
        let f: Vec<&str> = l.split(':').collect();
        (f.first() == Some(&user)).then(|| f.get(2)?.parse().ok()).flatten()
    })
}

pub fn install_css() {
    nyx_theme::use_design_2();
    // Part of the desktop: always the system theme.
    nyx_theme::use_shell_scope();
    nyx_theme::install(nyx_theme::Style {
        window_class: "nyx-prompt",
        base_skip: &[],
        css: include_str!("style.css"),
        no_glow_css: "",
        extra_colors: None,
        prefs: || {
            let suite = nyx_theme::read_suite().unwrap_or_default();
            nyx_theme::ThemePrefs { mode: suite.mode, theme: suite.theme, glow: false, reduce_motion: false }
        },
        set_theme: None,
    });
}

type Done = Box<dyn FnOnce(Request, bool)>;

struct Dialog {
    window: gtk::Window,
    entry: gtk::PasswordEntry,
    status: gtk::Label,
    ok: gtk::Button,
    request: RefCell<Option<Request>>,
    done: RefCell<Option<Done>>,
    user: RefCell<String>,
    /// The helper's current conversation; dropping it ends the conversation.
    talk: RefCell<Option<helper::Talk>>,
    /// The conversation was ended to start again password first.
    restarting: Cell<bool>,
    /// Shown while a face, finger or key is awaited.
    use_password: gtk::Button,
    /// The helper is waiting for an answer.
    asking: Cell<bool>,
    /// A password was sent in this conversation, so a failure means a wrong one.
    tried: Cell<bool>,
    halo: Cell<bool>,
    password_phase: Cell<bool>,
    demo: bool,
}

thread_local! {
    static CURRENT: RefCell<Option<Rc<Dialog>>> = const { RefCell::new(None) };
}

fn full_name(user: &str) -> String {
    std::fs::read_to_string("/etc/passwd")
        .unwrap_or_default()
        .lines()
        .find_map(|l| {
            let f: Vec<&str> = l.split(':').collect();
            (f.first() == Some(&user)).then(|| f.get(4).and_then(|g| g.split(',').next()).unwrap_or("").to_string())
        })
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| user.to_string())
}

/// Polkit's message as a title and the rest of it: without "Authentication is required
/// to" (the card says that already), split at its first colon, capitalised.
fn title(message: &str) -> (String, String) {
    let mut m = message.trim().trim_end_matches('.');
    for lead in ["Authentication is required to ", "Authentication is needed to "] {
        if m.get(..lead.len()).is_some_and(|prefix| prefix.eq_ignore_ascii_case(lead)) {
            m = &m[lead.len()..];
        }
    }
    let (head, rest) = m.split_once(": ").unwrap_or((m, ""));
    let cap = |s: &str| {
        let mut c = s.chars();
        c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
    };
    match cap(head) {
        h if h.is_empty() => ("An administrator's password is needed".into(), String::new()),
        h => (h, cap(rest)),
    }
}

fn set_status(d: &Dialog, text: &str, error: bool) {
    d.status.set_text(text);
    d.status.set_visible(!text.is_empty());
    if error {
        d.entry.add_css_class("error");
        d.status.add_css_class("error");
    } else {
        d.entry.remove_css_class("error");
        d.status.remove_css_class("error");
    }
}

fn notify_halo(event: &str) -> bool {
    let Ok(configuration) = std::fs::canonicalize("/etc/nyx/auth-overlay/shell.qml") else {
        return false;
    };
    std::process::Command::new("/run/current-system/sw/bin/timeout")
        .args(["0.5s", "/etc/nyx/auth-overlay/quickshell", "ipc", "-p"])
        .arg(configuration)
        .args(["call", "nyx-auth", "notify", event, "polkit", "face"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn password_was_requested(user: &str) -> bool {
    uid_of(user).is_some_and(|uid| {
        marker(user, uid).symlink_metadata().and_then(|meta| meta.modified()).ok()
            .and_then(|modified| modified.elapsed().ok())
            .is_some_and(|age| age < std::time::Duration::from_secs(60))
    })
}

fn show_fallback(d: &Dialog) {
    if d.halo.replace(false) {
        notify_halo("clear");
    }
    d.window.present();
}

fn finish(d: &Rc<Dialog>, ok: bool) {
    if d.halo.replace(false) {
        notify_halo(if ok { "done" } else { "clear" });
    }
    d.talk.borrow_mut().take();
    if let Some(uid) = my_uid() {
        let _ = std::fs::remove_file(marker(&d.user.borrow(), uid));
    }
    d.window.close();
    CURRENT.with(|c| c.borrow_mut().take());
    if let (Some(req), Some(done)) = (d.request.borrow_mut().take(), d.done.borrow_mut().take()) {
        done(req, ok);
    }
}

/// Polkit took the request back (it timed out, or the app went away).
pub fn cancel_shown() {
    if let Some(d) = CURRENT.with(|c| c.borrow().clone()) {
        finish(&d, false);
    }
}

fn mark_password_first(user: &str, uid: u32) -> bool {
    let path = marker(user, uid);
    path.parent().is_some_and(|dir| {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir).is_ok()
    }) && std::fs::write(&path, "").is_ok()
}

/// Restart the conversation with the password for the session's own account.
fn password_first(d: &Rc<Dialog>) {
    if d.asking.get() || d.restarting.get() || d.demo {
        return;
    }
    let Some(uid) = my_uid().filter(|u| uid_of(&d.user.borrow()) == Some(*u)) else { return };
    if !mark_password_first(&d.user.borrow(), uid) {
        return;
    }
    d.use_password.set_visible(false);
    d.restarting.set(true);
    set_status(d, "", false);
    if let Some(t) = d.talk.borrow().as_ref() {
        t.cancel();
    }
}

/// A conversation with polkit's helper for the chosen account.
fn converse(d: &Rc<Dialog>) {
    d.tried.set(false);
    d.asking.set(false);
    d.restarting.set(false);
    d.use_password.set_visible(false);
    d.entry.set_visible(false);
    d.ok.set_visible(false);
    if d.demo {
        d.entry.set_visible(true);
        d.ok.set_visible(true);
        d.window.present();
        d.asking.set(true);
        d.entry.grab_focus();
        return;
    }
    let own_account = my_uid().is_some_and(|uid| uid_of(&d.user.borrow()) == Some(uid));
    let face_enabled = std::path::Path::new("/etc/nyx/auth-overlay/polkit-face").is_file();
    let face_first = face_enabled && own_account && !d.password_phase.get()
        && !password_was_requested(&d.user.borrow());
    d.halo.set(face_first && notify_halo("start"));
    if !face_first {
        d.window.present();
    }
    let cookie = d.request.borrow().as_ref().map(|r| r.cookie.clone()).unwrap_or_default();
    let (tx, rx) = async_channel::unbounded();
    *d.talk.borrow_mut() = Some(helper::start(&d.user.borrow(), &cookie, tx));
    if !d.status.has_css_class("error") {
        set_status(d, "Checking authentication", false);
    }
    // Typing waits for a scan otherwise; offer the password once one is awaited.
    let wd = Rc::downgrade(d);
    glib::timeout_add_local_once(std::time::Duration::from_millis(700), move || {
        if let Some(d) = wd.upgrade()
            && !d.asking.get()
            && !d.restarting.get()
            && !d.halo.get()
            && d.talk.borrow().is_some()
            && my_uid().is_some_and(|u| uid_of(&d.user.borrow()) == Some(u))
        {
            d.use_password.set_visible(true);
        }
    });
    let wd = Rc::downgrade(d);
    glib::spawn_future_local(async move {
        while let Ok(line) = rx.recv().await {
            let Some(d) = wd.upgrade() else { return };
            // Ended to start again password first: the old conversation's end is no news.
            if d.restarting.get() {
                if matches!(line, Line::Done(_)) {
                    converse(&d);
                    return;
                }
                continue;
            }
            match line {
                Line::Request { text, echo } => {
                    d.password_phase.set(true);
                    if !d.status.has_css_class("error") {
                        set_status(&d, "", false);
                    }
                    d.asking.set(true);
                    d.use_password.set_visible(false);
                    d.entry.set_visible(true);
                    d.ok.set_visible(true);
                    d.entry.set_show_peek_icon(!echo);
                    let hint = text.trim().trim_end_matches(':');
                    d.entry.set_property("placeholder-text", if hint.is_empty() { "Password" } else { hint });
                    d.ok.set_sensitive(!d.entry.text().is_empty());
                    show_fallback(&d);
                    d.entry.grab_focus();
                }
                Line::Info(text) => set_status(&d, &text, false),
                Line::Error(text) => set_status(&d, &text, true),
                Line::Done(true) => return finish(&d, true),
                Line::Done(false) => {
                    if d.tried.get() {
                        set_status(&d, "That password didn't work. Try again.", true);
                        if let Some(uid) = my_uid().filter(|uid| uid_of(&d.user.borrow()) == Some(*uid))
                            && !mark_password_first(&d.user.borrow(), uid)
                        {
                            set_status(&d, "This can't be checked right now.", true);
                            return;
                        }
                        converse(&d);
                    } else {
                        show_fallback(&d);
                        set_status(&d, "This can't be checked right now.", true);
                    }
                    return;
                }
            }
        }
    });
}

fn submit(d: &Rc<Dialog>) {
    let text = d.entry.text().to_string();
    if text.is_empty() || !d.asking.get() {
        return;
    }
    d.entry.set_text("");
    d.asking.set(false);
    d.tried.set(true);
    set_status(d, "Checking password", false);
    if d.demo {
        if text == "nyx" {
            finish(d, true);
        } else {
            set_status(d, "That password didn't work. Try again.", true);
            converse(d);
        }
        return;
    }
    d.ok.set_sensitive(false);
    if let Some(t) = d.talk.borrow().as_ref() {
        let _ = t.answers.send(text);
    }
}

/// The prompt's window: over everything on the focused screen, or for `--demo` a normal
/// window.
fn window(app: &gtk::Application, demo: bool) -> gtk::Window {
    let w = gtk::Window::new();
    w.set_application(Some(app));
    w.add_css_class("nyx-prompt");
    w.set_title(Some("Administrator approval"));
    if demo {
        w.set_decorated(false);
        w.add_css_class("demo");
    }
    if !demo && gtk4_layer_shell::is_supported() {
        w.init_layer_shell();
        w.set_namespace(Some("nyx-prompt"));
        w.set_layer(Layer::Overlay);
        for e in [Edge::Top, Edge::Bottom, Edge::Left, Edge::Right] {
            w.set_anchor(e, true);
        }
        w.set_exclusive_zone(-1);
        w.set_keyboard_mode(KeyboardMode::Exclusive);
    }
    w
}

pub fn show(app: &gtk::Application, req: Request, done: impl FnOnce(Request, bool) + 'static) {
    build(app, req, Box::new(done), false);
}

/// The card once, without polkit; the password is `nyx`.
pub fn demo(app: &gtk::Application) {
    let me = std::env::var("USER").unwrap_or_default();
    let req = Request {
        action: "io.github.nyxos_project.system.apply".into(),
        message: "Authentication is required to install system updates.".into(),
        details: vec![],
        cookie: String::new(),
        users: vec![me],
        invocation: None,
    };
    let a = app.clone();
    build(app, req, Box::new(move |_, _| a.quit()), true);
}

fn build(app: &gtk::Application, req: Request, done: Done, demo: bool) {
    let w = window(app, demo);
    let backdrop = gtk::Box::new(gtk::Orientation::Vertical, 0);
    backdrop.add_css_class("backdrop");
    let card = gtk::Box::new(gtk::Orientation::Vertical, 16);
    card.add_css_class("card");
    card.set_halign(gtk::Align::Center);
    card.set_valign(gtk::Align::Center);
    card.set_vexpand(true);
    card.set_size_request(360, -1);
    backdrop.append(&card);

    let header = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    let badge = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    badge.add_css_class("badge");
    badge.set_valign(gtk::Align::Start);
    let icon = gtk::Image::from_icon_name("system-lock-screen-symbolic");
    icon.set_pixel_size(18);
    badge.append(&icon);
    header.append(&badge);

    let headings = gtk::Box::new(gtk::Orientation::Vertical, 3);
    headings.set_hexpand(true);
    let heading = gtk::Label::new(Some("Administrator approval"));
    heading.add_css_class("title");
    heading.set_xalign(0.0);
    headings.append(&heading);

    let (head, more) = title(&req.message);
    for text in [head, more] {
        if !text.is_empty() {
            let line = gtk::Label::new(Some(&text));
            line.add_css_class("subtitle");
            line.set_wrap(true);
            line.set_max_width_chars(32);
            line.set_xalign(0.0);
            headings.append(&line);
        }
    }
    header.append(&headings);
    card.append(&header);

    let user = req.users.first().cloned().unwrap_or_default();
    let form = gtk::Box::new(gtk::Orientation::Vertical, 6);
    let account: gtk::Widget = if req.users.len() > 1 {
        let names: Vec<String> = req.users.iter().map(|u| full_name(u)).collect();
        let strs: Vec<&str> = names.iter().map(String::as_str).collect();
        let dd = gtk::DropDown::from_strings(&strs);
        dd.update_property(&[gtk::accessible::Property::Label("Administrator account")]);
        dd.upcast()
    } else {
        let l = gtk::Label::new(Some(&full_name(&user)));
        l.set_xalign(0.0);
        l.add_css_class("account");
        l.upcast()
    };
    form.append(&account);

    let entry = gtk::PasswordEntry::new();
    entry.set_show_peek_icon(true);
    entry.add_css_class("password");
    entry.set_property("placeholder-text", "Password");
    entry.update_property(&[gtk::accessible::Property::Label("Password")]);
    form.append(&entry);

    let use_password = gtk::Button::with_label("Use password instead");
    use_password.add_css_class("flat");
    use_password.add_css_class("use-password");
    use_password.set_halign(gtk::Align::Start);
    use_password.set_visible(false);
    use_password.set_tooltip_text(Some("Skip the face, fingerprint or security key this time"));
    form.append(&use_password);

    let status = gtk::Label::new(None);
    status.add_css_class("status");
    status.set_wrap(true);
    status.set_xalign(0.0);
    status.set_max_width_chars(38);
    status.set_visible(false);
    form.append(&status);
    card.append(&form);

    let footer = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    footer.add_css_class("footer");
    footer.set_halign(gtk::Align::End);
    let cancel = gtk::Button::with_label("Cancel");
    cancel.add_css_class("flat");
    let ok = gtk::Button::with_label("Approve");
    ok.add_css_class("suggested-action");
    ok.set_sensitive(false);
    footer.append(&cancel);
    footer.append(&ok);
    card.append(&footer);

    if !req.action.is_empty() {
        let details = gtk::Expander::new(Some("Request details"));
        details.add_css_class("details");
        let mut text = req.action.clone();
        for (k, v) in &req.details {
            if !k.starts_with("polkit.") {
                text.push_str(&format!("\n{k}: {v}"));
            }
        }
        let l = gtk::Label::new(Some(&text));
        l.add_css_class("detail-text");
        l.set_selectable(true);
        l.set_wrap(true);
        l.set_wrap_mode(gtk::pango::WrapMode::WordChar);
        l.set_max_width_chars(38);
        l.set_xalign(0.0);
        details.set_child(Some(&l));
        card.append(&details);
    }

    w.set_child(Some(&backdrop));
    let d = Rc::new(Dialog {
        window: w.clone(),
        entry: entry.clone(),
        status,
        ok: ok.clone(),
        request: RefCell::new(Some(req)),
        done: RefCell::new(Some(done)),
        user: RefCell::new(user),
        talk: RefCell::default(),
        restarting: Cell::new(false),
        use_password: use_password.clone(),
        asking: Cell::new(false),
        tried: Cell::new(false),
        halo: Cell::new(false),
        password_phase: Cell::new(false),
        demo,
    });
    CURRENT.with(|c| *c.borrow_mut() = Some(d.clone()));

    if let Some(dd) = account.downcast_ref::<gtk::DropDown>() {
        let wd = Rc::downgrade(&d);
        dd.connect_selected_notify(move |dd| {
            let Some(d) = wd.upgrade() else { return };
            let users = d.request.borrow().as_ref().map(|r| r.users.clone()).unwrap_or_default();
            if let Some(u) = users.get(dd.selected() as usize) {
                *d.user.borrow_mut() = u.clone();
                d.talk.borrow_mut().take();
                set_status(&d, "", false);
                converse(&d);
            }
        });
    }
    let wd = Rc::downgrade(&d);
    entry.connect_activate(move |_| {
        if let Some(d) = wd.upgrade() {
            submit(&d);
        }
    });
    let wd = Rc::downgrade(&d);
    ok.connect_clicked(move |_| {
        if let Some(d) = wd.upgrade() {
            submit(&d);
        }
    });
    let wd = Rc::downgrade(&d);
    cancel.connect_clicked(move |_| {
        if let Some(d) = wd.upgrade() {
            finish(&d, false);
        }
    });
    let keys = gtk::EventControllerKey::new();
    let wd = Rc::downgrade(&d);
    keys.connect_key_pressed(move |_, key, _, _| {
        if key == gdk::Key::Escape
            && let Some(d) = wd.upgrade()
        {
            finish(&d, false);
            return glib::Propagation::Stop;
        }
        glib::Propagation::Proceed
    });
    w.add_controller(keys);
    let wd = Rc::downgrade(&d);
    entry.connect_changed(move |e| {
        if let Some(d) = wd.upgrade() {
            d.ok.set_sensitive(d.asking.get() && !e.text().is_empty());
            // Typing while a scan is awaited means the password, now.
            if !e.text().is_empty() && !d.asking.get() && !d.tried.get() {
                password_first(&d);
            }
        }
    });
    let wd = Rc::downgrade(&d);
    use_password.connect_clicked(move |_| {
        if let Some(d) = wd.upgrade() {
            password_first(&d);
            d.entry.grab_focus();
        }
    });
    converse(&d);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn polkits_message_reads_as_a_title() {
        assert_eq!(
            title("Authentication is required to apply system changes: settings, software or services."),
            ("Apply system changes".to_string(), "Settings, software or services".to_string())
        );
        assert_eq!(title("Install Steam"), ("Install Steam".to_string(), String::new()));
        assert_eq!(title("").0, "An administrator's password is needed");
    }
}
