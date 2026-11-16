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
fn marker(uid: u32) -> std::path::PathBuf {
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
    card: gtk::Box,
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

fn home_of(user: &str) -> std::path::PathBuf {
    std::fs::read_to_string("/etc/passwd")
        .unwrap_or_default()
        .lines()
        .find_map(|l| {
            let f: Vec<&str> = l.split(':').collect();
            (f.first() == Some(&user)).then(|| f.get(5).unwrap_or(&"").into())
        })
        .unwrap_or_default()
}

/// Polkit's message as a title and the rest of it: without "Authentication is required
/// to" (the card says that already), split at its first colon, capitalised.
fn title(message: &str) -> (String, String) {
    let mut m = message.trim().trim_end_matches('.');
    for lead in ["Authentication is required to ", "Authentication is needed to "] {
        if m.len() > lead.len() && m[..lead.len()].eq_ignore_ascii_case(lead) {
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
    if error {
        d.status.add_css_class("error");
    } else {
        d.status.remove_css_class("error");
    }
}

fn shake(d: &Dialog) {
    d.card.add_css_class("shake");
    let c = d.card.clone();
    glib::timeout_add_local_once(std::time::Duration::from_millis(450), move || c.remove_css_class("shake"));
}

fn finish(d: &Rc<Dialog>, ok: bool) {
    d.talk.borrow_mut().take();
    if let Some(uid) = my_uid() {
        let _ = std::fs::remove_file(marker(uid));
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

/// Skip the face, finger or key this time: mark it for the gate and start the
/// conversation again, which now asks for the password straight away. Only for your own
/// account, whose runtime folder the gate reads.
fn password_first(d: &Rc<Dialog>) {
    if d.asking.get() || d.restarting.get() || d.demo {
        return;
    }
    let Some(uid) = my_uid().filter(|u| uid_of(&d.user.borrow()) == Some(*u)) else { return };
    let path = marker(uid);
    let made = path.parent().is_some_and(|dir| {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir).is_ok()
    }) && std::fs::write(&path, "").is_ok();
    if !made {
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
    if d.demo {
        d.asking.set(true);
        d.entry.grab_focus();
        return;
    }
    let cookie = d.request.borrow().as_ref().map(|r| r.cookie.clone()).unwrap_or_default();
    let (tx, rx) = async_channel::unbounded();
    *d.talk.borrow_mut() = Some(helper::start(&d.user.borrow(), &cookie, tx));
    // Typing waits for a scan otherwise; offer the password once one is awaited.
    let wd = Rc::downgrade(d);
    glib::timeout_add_local_once(std::time::Duration::from_millis(700), move || {
        if let Some(d) = wd.upgrade()
            && !d.asking.get()
            && !d.restarting.get()
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
                    d.asking.set(true);
                    d.use_password.set_visible(false);
                    d.entry.set_show_peek_icon(!echo);
                    let hint = text.trim().trim_end_matches(':');
                    d.entry.set_property("placeholder-text", if hint.is_empty() { "Password" } else { hint });
                    d.ok.set_sensitive(!d.entry.text().is_empty());
                    d.entry.grab_focus();
                }
                Line::Info(text) => set_status(&d, &text, false),
                Line::Error(text) => set_status(&d, &text, true),
                Line::Done(true) => return finish(&d, true),
                Line::Done(false) => {
                    if d.tried.get() {
                        shake(&d);
                        set_status(&d, "That password didn't work. Try again.", true);
                        converse(&d);
                    } else {
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
    set_status(d, "Checking…", false);
    if d.demo {
        if text == "nyx" {
            finish(d, true);
        } else {
            shake(d);
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
    w.set_title(Some("Administrator password"));
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
        message: "Nyx Welcome wants to install software and change system settings".into(),
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
    let card = gtk::Box::new(gtk::Orientation::Vertical, 0);
    card.add_css_class("card");
    card.set_halign(gtk::Align::Center);
    card.set_valign(gtk::Align::Center);
    card.set_vexpand(true);
    card.set_size_request(380, -1);
    backdrop.append(&card);

    let user = req.users.first().cloned().unwrap_or_default();
    let picture = nyx_signin_ui::picture_of(&user, &home_of(&user));
    let avatar = nyx_signin_ui::avatar(picture.as_deref(), &nyx_signin_ui::initials(&full_name(&user), &user), 56);
    avatar.set_halign(gtk::Align::Center);
    avatar.add_css_class("avatar");
    card.append(&avatar);

    let (head, more) = title(&req.message);
    let heading = gtk::Label::new(Some(&head));
    heading.add_css_class("title");
    heading.set_wrap(true);
    heading.set_justify(gtk::Justification::Center);
    heading.set_max_width_chars(34);
    card.append(&heading);
    if !more.is_empty() {
        let l = gtk::Label::new(Some(&more));
        l.add_css_class("subtitle");
        l.set_wrap(true);
        l.set_justify(gtk::Justification::Center);
        l.set_max_width_chars(42);
        card.append(&l);
    }

    let account: gtk::Widget = if req.users.len() > 1 {
        let who = gtk::Label::new(Some("An administrator's password"));
        who.add_css_class("who");
        card.append(&who);
        let names: Vec<String> = req.users.iter().map(|u| full_name(u)).collect();
        let strs: Vec<&str> = names.iter().map(String::as_str).collect();
        let dd = gtk::DropDown::from_strings(&strs);
        dd.add_css_class("account");
        dd.set_halign(gtk::Align::Center);
        dd.upcast()
    } else {
        let l = gtk::Label::new(Some(&format!("The password for {}", full_name(&user))));
        l.add_css_class("who");
        l.add_css_class("account");
        l.upcast()
    };
    card.append(&account);

    let entry = gtk::PasswordEntry::new();
    entry.set_show_peek_icon(true);
    entry.add_css_class("password");
    entry.set_property("placeholder-text", "Password");
    entry.update_property(&[gtk::accessible::Property::Label("Password")]);
    card.append(&entry);

    let use_password = gtk::Button::with_label("Use password instead");
    use_password.add_css_class("flat");
    use_password.add_css_class("use-password");
    use_password.set_halign(gtk::Align::Center);
    use_password.set_visible(false);
    use_password.set_tooltip_text(Some("Skip the face, fingerprint or security key this time"));
    card.append(&use_password);

    let status = gtk::Label::new(None);
    status.add_css_class("status");
    status.set_wrap(true);
    status.set_justify(gtk::Justification::Center);
    status.set_max_width_chars(40);
    card.append(&status);

    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    buttons.add_css_class("buttons");
    buttons.set_homogeneous(true);
    let cancel = gtk::Button::with_label("Cancel");
    let ok = gtk::Button::with_label("Continue");
    ok.add_css_class("suggested-action");
    ok.set_sensitive(false);
    buttons.append(&cancel);
    buttons.append(&ok);
    card.append(&buttons);

    if !req.action.is_empty() {
        let details = gtk::Expander::new(Some("Details"));
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
        l.set_xalign(0.0);
        details.set_child(Some(&l));
        card.append(&details);
    }

    w.set_child(Some(&backdrop));
    let d = Rc::new(Dialog {
        window: w.clone(),
        entry: entry.clone(),
        status,
        card,
        ok: ok.clone(),
        request: RefCell::new(Some(req)),
        done: RefCell::new(Some(done)),
        user: RefCell::new(user),
        talk: RefCell::default(),
        restarting: Cell::new(false),
        use_password: use_password.clone(),
        asking: Cell::new(false),
        tried: Cell::new(false),
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
    w.present();
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
