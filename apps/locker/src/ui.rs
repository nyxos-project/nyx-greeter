//! The lock screen on every monitor, and the ways in.

use crate::Options;
use gtk::prelude::*;
use gtk::{gdk, glib};
use nyx_lock_core::{Config, Event, Method};
use nyx_signin_ui::{Colours, Indicator, Kind, Phase, Style};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::OnceLock;
use std::sync::mpsc::Sender;
use std::time::Duration;

static THEME: OnceLock<String> = OnceLock::new();

/// A fingerprint that doesn't match is tried again this many times, then the password.
const FINGER_TRIES: u32 = 5;

/// After a face scan, the next waits this long, so moving the pointer doesn't keep the
/// camera on.
const FACE_REST: Duration = Duration::from_secs(10);

/// What the lock screen looks like: the lock-screen theme, its wallpaper and the style.
#[derive(Debug, Clone)]
pub struct Look {
    pub theme: String,
    pub style: Style,
    pub wallpaper: Option<PathBuf>,
}

fn home() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
}

fn config_home() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).filter(|p| p.is_absolute()).unwrap_or_else(|| home().join(".config"))
}

/// `appearance.toml`'s `key`: the user's, else the system's default.
fn appearance(key: &str) -> Option<String> {
    [
        config_home().join("nyx/appearance.toml"),
        PathBuf::from("/etc/nyxos/overrides/appearance.toml"),
        PathBuf::from("/etc/nyxos/defaults/appearance.toml"),
    ]
    .iter()
    .filter_map(|p| std::fs::read_to_string(p).ok()?.parse::<toml::Table>().ok())
    .find_map(|t| t.get(key)?.as_str().map(String::from))
}

/// The wallpaper nyx-themed chose for the lock screen (`$nyx_wallpaper`).
fn themed_wallpaper() -> Option<PathBuf> {
    let text = std::fs::read_to_string(config_home().join("hypr/nyx-hyprlock.conf")).ok()?;
    let path = text.lines().find_map(|l| l.strip_prefix("$nyx_wallpaper = "))?.trim();
    (!path.is_empty()).then(|| PathBuf::from(path)).filter(|p| p.is_file())
}

impl Look {
    pub fn load(style: Option<Style>) -> Look {
        Look {
            theme: appearance("lock-theme")
                .or_else(|| nyx_theme::read_suite().map(|s| s.theme))
                .unwrap_or_else(|| "nyx-dark".into()),
            style: style.or_else(|| appearance("lock-style").as_deref().and_then(Style::parse)).unwrap_or_default(),
            wallpaper: themed_wallpaper(),
        }
    }
}

pub fn theme(id: &str) {
    let _ = THEME.set(id.to_string());
}

pub fn install_css() {
    nyx_theme::use_design_2();
    nyx_theme::install(nyx_theme::Style {
        window_class: "nyx-locker",
        base_skip: &[],
        css: include_str!("style.css"),
        no_glow_css: "",
        extra_colors: None,
        prefs: || nyx_theme::ThemePrefs {
            mode: nyx_theme::ThemeMode::Theme,
            theme: THEME.get().cloned().unwrap_or_else(|| "nyx-dark".into()),
            glow: false,
            reduce_motion: false,
        },
        // The lock-screen theme is its own; a suite theme change doesn't restyle it.
        set_theme: Some(|_, _| {}),
    });
}

struct Screen {
    entry: gtk::PasswordEntry,
    status: gtk::Label,
    indicator: Rc<Indicator>,
    card: gtk::Box,
    time: gtk::Label,
    date: gtk::Label,
}

struct Locker {
    app: gtk::Application,
    opts: Options,
    look: Look,
    user: String,
    methods: Vec<Method>,
    screens: RefCell<Vec<Screen>>,
    /// Where each running method's answers go.
    running: RefCell<HashMap<Method, Sender<String>>>,
    events: async_channel::Sender<Event>,
    finger_tries: Cell<u32>,
    /// When the next face scan may start (monotonic µs).
    face_after: Cell<i64>,
    unlocking: Cell<bool>,
    lock: RefCell<Option<gtk4_session_lock::Instance>>,
    code: Rc<Cell<u8>>,
}

fn kind(m: Method) -> Kind {
    match m {
        Method::Password => Kind::Password,
        Method::Fingerprint => Kind::Fingerprint,
        Method::Face => Kind::Face,
        Method::Key => Kind::Key,
    }
}

pub fn start(app: &gtk::Application, look: Look, opts: Options, code: Rc<Cell<u8>>) {
    std::mem::forget(app.hold());
    let (tx, rx) = async_channel::unbounded::<Event>();
    let config = Config::load();
    let methods = if opts.demo { vec![Method::Password, Method::Fingerprint, Method::Face] } else { config.methods() };
    let l = Rc::new(Locker {
        app: app.clone(),
        opts,
        look,
        user: nyx_lock_core::user(),
        methods,
        screens: RefCell::default(),
        running: RefCell::default(),
        events: tx,
        finger_tries: Cell::new(0),
        face_after: Cell::new(0),
        unlocking: Cell::new(false),
        lock: RefCell::default(),
        code,
    });
    let wl = Rc::downgrade(&l);
    glib::spawn_future_local(async move {
        while let Ok(e) = rx.recv().await {
            let Some(l) = wl.upgrade() else { return };
            on_event(&l, e);
        }
    });
    if l.opts.demo {
        let w = window(&l, true);
        w.set_default_size(1280, 800);
        w.present();
        begin(&l);
    } else {
        lock(&l);
        watch_logind(&l);
    }
    tick(&l);
    let wl = Rc::downgrade(&l);
    glib::timeout_add_seconds_local(1, move || match wl.upgrade() {
        Some(l) => {
            tick(&l);
            glib::ControlFlow::Continue
        }
        None => glib::ControlFlow::Break,
    });
    std::mem::forget(l);
}

/// `loginctl unlock-session` (logind's `Unlock` on one of this user's sessions) unlocks,
/// as with other lock screens. Only root or the session's owner can send it.
fn watch_logind(l: &Rc<Locker>) {
    use gtk::gio;
    let Ok(bus) = gio::bus_get_sync(gio::BusType::System, gio::Cancellable::NONE) else { return };
    let Ok(uid) = std::fs::metadata("/proc/self").map(|m| std::os::unix::fs::MetadataExt::uid(&m)) else { return };
    let wl = Rc::downgrade(l);
    let sub = bus.subscribe_to_signal(
        Some("org.freedesktop.login1"),
        Some("org.freedesktop.login1.Session"),
        Some("Unlock"),
        None,
        None,
        gio::DBusSignalFlags::NONE,
        move |signal| {
            let owner = signal
                .connection
                .call_sync(
                    Some("org.freedesktop.login1"),
                    signal.object_path,
                    "org.freedesktop.DBus.Properties",
                    "Get",
                    Some(&("org.freedesktop.login1.Session", "User").to_variant()),
                    None,
                    gio::DBusCallFlags::NONE,
                    2000,
                    gio::Cancellable::NONE,
                )
                .ok()
                .and_then(|reply| reply.child_value(0).as_variant())
                .and_then(|user| user.child_value(0).get::<u32>());
            if owner == Some(uid)
                && let Some(l) = wl.upgrade()
            {
                on_event(&l, Event::Granted(Method::Password));
            }
        },
    );
    std::mem::forget(sub);
}

fn lock(l: &Rc<Locker>) {
    let inst = gtk4_session_lock::Instance::new();
    let wl = Rc::downgrade(l);
    inst.connect_monitor(move |inst, monitor| {
        let Some(l) = wl.upgrade() else { return };
        // Assigned before it's shown: the lock surface is made when it's realised.
        let w = window(&l, false);
        inst.assign_window_to_monitor(&w, monitor);
        w.present();
    });
    let wl = Rc::downgrade(l);
    inst.connect_locked(move |_| {
        if let Some(l) = wl.upgrade() {
            begin(&l);
        }
    });
    let wl = Rc::downgrade(l);
    inst.connect_failed(move |_| {
        eprintln!("nyx-locker: the session couldn't be locked (another lock screen has it)");
        if let Some(l) = wl.upgrade() {
            l.code.set(crate::EXIT_ALREADY_LOCKED);
            l.app.quit();
        }
    });
    let wl = Rc::downgrade(l);
    inst.connect_unlocked(move |_| {
        if let Some(l) = wl.upgrade() {
            l.app.quit();
        }
    });
    inst.lock();
    *l.lock.borrow_mut() = Some(inst);
}

fn window(l: &Rc<Locker>, demo: bool) -> gtk::Window {
    let w = gtk::Window::new();
    w.set_application(Some(&l.app));
    w.add_css_class("nyx-locker");
    if demo {
        w.set_title(Some("Lock screen (demo)"));
    }
    let overlay = gtk::Overlay::new();
    let bg = gtk::Picture::new();
    bg.set_content_fit(gtk::ContentFit::Cover);
    bg.set_can_shrink(true);
    if let Some(p) = &l.look.wallpaper
        && let Ok(t) = gdk::Texture::from_filename(p)
    {
        bg.set_paintable(Some(&t));
    }
    overlay.set_child(Some(&bg));
    let dim = gtk::Box::new(gtk::Orientation::Vertical, 0);
    let provider = gtk::CssProvider::new();
    provider.load_from_string("box.lock-dim { background-color: alpha(black, 0.55); }");
    gtk::style_context_add_provider_for_display(&WidgetExt::display(&w), &provider, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION + 1);
    dim.add_css_class("lock-dim");
    overlay.add_overlay(&dim);

    let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
    let time = gtk::Label::new(None);
    time.add_css_class("clock");
    let date = gtk::Label::new(None);
    date.add_css_class("date");
    let clock = gtk::Box::new(gtk::Orientation::Vertical, 2);
    clock.set_margin_top(96);
    clock.append(&time);
    clock.append(&date);
    content.append(&clock);
    let spacer = gtk::Box::new(gtk::Orientation::Vertical, 0);
    spacer.set_vexpand(true);
    content.append(&spacer);

    let card = gtk::Box::new(gtk::Orientation::Vertical, 14);
    card.add_css_class("card");
    card.set_halign(gtk::Align::Center);
    card.set_size_request(360, -1);
    let name = full_name(&l.user);
    let pic = nyx_signin_ui::picture_of(&l.user, &home());
    let avatar = nyx_signin_ui::avatar(pic.as_deref(), &nyx_signin_ui::initials(&name, &l.user), 80);
    avatar.set_halign(gtk::Align::Center);
    let who = gtk::Label::new(Some(&name));
    who.add_css_class("name");
    let indicator = Indicator::new(l.look.style, Colours::of_theme(&l.look.theme));
    let entry = gtk::PasswordEntry::new();
    entry.set_show_peek_icon(true);
    entry.add_css_class("password");
    entry.set_property("placeholder-text", "Password");
    entry.update_property(&[gtk::accessible::Property::Label(&format!("Password for {name}"))]);
    let status = gtk::Label::new(None);
    status.add_css_class("status");
    status.set_wrap(true);
    status.set_justify(gtk::Justification::Center);
    status.set_max_width_chars(36);
    card.append(&avatar);
    card.append(&who);
    if l.look.style != Style::Edge {
        card.append(indicator.widget());
    }
    card.append(&entry);
    card.append(&status);
    if l.look.style == Style::Edge {
        card.append(indicator.widget());
    }
    content.append(&card);
    let spacer = gtk::Box::new(gtk::Orientation::Vertical, 0);
    spacer.set_vexpand(true);
    content.append(&spacer);
    overlay.add_overlay(&content);
    overlay.add_overlay(indicator.glow());

    let wl = Rc::downgrade(l);
    entry.connect_activate(move |e| {
        let Some(l) = wl.upgrade() else { return };
        let text = e.text().to_string();
        if text.is_empty() {
            return;
        }
        e.set_text("");
        let tx = l.running.borrow().get(&Method::Password).cloned();
        match tx {
            Some(tx) => {
                set_status(&l, "Checking…", false);
                let _ = tx.send(text);
            }
            None => set_status(&l, "One moment…", false),
        }
    });
    // Coming back (a key or the pointer) starts a face scan; the camera isn't kept on.
    let keys = gtk::EventControllerKey::new();
    let wl = Rc::downgrade(l);
    keys.connect_key_pressed(move |_, _, _, _| {
        if let Some(l) = wl.upgrade() {
            wake(&l);
        }
        glib::Propagation::Proceed
    });
    w.add_controller(keys);
    let motion = gtk::EventControllerMotion::new();
    let wl = Rc::downgrade(l);
    motion.connect_motion(move |_, _, _| {
        if let Some(l) = wl.upgrade() {
            wake(&l);
        }
    });
    w.add_controller(motion);
    w.set_child(Some(&overlay));
    l.screens.borrow_mut().push(Screen { entry, status, indicator, card, time, date });
    w
}

pub fn full_name(user: &str) -> String {
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

/// The session is locked: start the ways in (the face waits for you to come back).
fn begin(l: &Rc<Locker>) {
    for m in l.methods.clone() {
        if m != Method::Face {
            run(l, m);
        }
    }
    if let Some(s) = l.screens.borrow().first() {
        s.entry.grab_focus();
    }
}

fn run(l: &Rc<Locker>, m: Method) {
    if l.unlocking.get() || l.running.borrow().contains_key(&m) {
        return;
    }
    let tx = l.events.clone();
    let send = move |e: Event| {
        let _ = tx.send_blocking(e);
    };
    let answers = if l.opts.demo { nyx_lock_core::demo::start(m, send) } else { nyx_lock_core::start(m, &l.user, send) };
    l.running.borrow_mut().insert(m, answers);
}

fn wake(l: &Rc<Locker>) {
    if l.methods.contains(&Method::Face)
        && !l.running.borrow().contains_key(&Method::Face)
        && glib::monotonic_time() >= l.face_after.get()
    {
        run(l, Method::Face);
    }
}

fn indicate(l: &Locker, k: Kind, phase: Phase, message: &str) {
    for s in l.screens.borrow().iter() {
        s.indicator.set(k, phase, message);
    }
}

fn set_status(l: &Locker, text: &str, error: bool) {
    for s in l.screens.borrow().iter() {
        s.status.set_text(text);
        if error {
            s.status.add_css_class("error");
        } else {
            s.status.remove_css_class("error");
        }
    }
}

fn shake(l: &Locker) {
    for s in l.screens.borrow().iter() {
        s.card.add_css_class("shake");
        let c = s.card.clone();
        glib::timeout_add_local_once(Duration::from_millis(450), move || c.remove_css_class("shake"));
    }
}

fn on_event(l: &Rc<Locker>, e: Event) {
    if l.unlocking.get() {
        return;
    }
    match e {
        Event::Prompt { method: Method::Password, .. } => {}
        Event::Prompt { method, text, .. } => {
            // A security key's PIN: the entry answers it.
            set_status(l, &text, false);
            if let Some(tx) = l.running.borrow_mut().remove(&method) {
                l.running.borrow_mut().insert(Method::Password, tx);
            }
        }
        Event::Info { method, text } | Event::Error { method, text } if method != Method::Password => {
            indicate(l, kind(method), Phase::Scanning, &text);
        }
        Event::Info { text, .. } | Event::Error { text, .. } => set_status(l, &text, false),
        Event::Granted(method) => {
            l.unlocking.set(true);
            indicate(l, kind(method), Phase::Success, "");
            set_status(l, "", false);
            let wl = Rc::downgrade(l);
            glib::timeout_add_local_once(Duration::from_millis(420), move || {
                let Some(l) = wl.upgrade() else { return };
                match l.lock.borrow().as_ref() {
                    Some(inst) => inst.unlock(),
                    None => l.app.quit(),
                }
            });
        }
        Event::Denied { method, text } => {
            l.running.borrow_mut().remove(&method);
            match method {
                Method::Password => {
                    shake(l);
                    set_status(l, &text, true);
                    run(l, Method::Password);
                }
                Method::Fingerprint => {
                    indicate(l, Kind::Fingerprint, Phase::Failure, &text);
                    let n = l.finger_tries.get() + 1;
                    l.finger_tries.set(n);
                    if n < FINGER_TRIES {
                        again(l, Method::Fingerprint, 1500);
                    } else {
                        indicate(l, Kind::Fingerprint, Phase::Timeout, "Use your password");
                    }
                }
                Method::Face => {
                    l.face_after.set(glib::monotonic_time() + FACE_REST.as_micros() as i64);
                    indicate(l, Kind::Face, Phase::Failure, &text);
                }
                Method::Key => again(l, Method::Key, 2000),
            }
        }
        Event::Unavailable { method, text } => {
            l.running.borrow_mut().remove(&method);
            match method {
                // Waits for a key to be plugged in, quietly.
                Method::Key => again(l, Method::Key, 3000),
                Method::Password => {
                    set_status(l, &text, true);
                    again(l, Method::Password, 2000);
                }
                // No camera or reader now: not tried again until the next lock.
                Method::Face => l.face_after.set(i64::MAX),
                Method::Fingerprint => {}
            }
        }
    }
}

fn again(l: &Rc<Locker>, m: Method, ms: u64) {
    let wl = Rc::downgrade(l);
    glib::timeout_add_local_once(Duration::from_millis(ms), move || {
        if let Some(l) = wl.upgrade() {
            run(&l, m);
        }
    });
}

fn tick(l: &Locker) {
    let now = glib::DateTime::now_local().ok();
    let (t, d) = match &now {
        Some(n) => (
            n.format("%H:%M").map(|s| s.to_string()).unwrap_or_default(),
            n.format("%A, %B %-e").map(|s| s.to_string()).unwrap_or_default(),
        ),
        None => (String::new(), String::new()),
    };
    for s in l.screens.borrow().iter() {
        if s.time.text() != t {
            s.time.set_text(&t);
        }
        if s.date.text() != d {
            s.date.set_text(&d);
        }
    }
}
