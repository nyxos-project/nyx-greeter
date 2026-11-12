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

/// The indicator on the card.
const RING: i32 = 96;

/// From a way in succeeding to the session unlocking: the match shows, then the clock
/// and card fade (style.css). The unlock goes by this clock, not the animation's.
const FADE_AFTER: Duration = Duration::from_millis(250);
const UNLOCK_AFTER: Duration = Duration::from_millis(600);

/// What the lock screen looks like: the lock-screen theme, its wallpaper, the style and
/// whether motion is reduced.
#[derive(Debug, Clone)]
pub struct Look {
    pub theme: String,
    pub style: Style,
    pub wallpaper: Option<PathBuf>,
    pub still: bool,
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
                .unwrap_or_else(|| nyx_theme::DEFAULT_THEME.into()),
            style: style.or_else(|| appearance("lock-style").as_deref().and_then(Style::parse)).unwrap_or_default(),
            wallpaper: themed_wallpaper(),
            still: nyx_theme::A11y::system().reduce_motion,
        }
    }
}

pub fn theme(id: &str) {
    let _ = THEME.set(id.to_string());
}

pub fn install_css() {
    nyx_theme::use_design_2();
    // The lock-screen theme is its own; a system theme change doesn't restyle it.
    nyx_theme::use_fixed_theme(THEME.get().map(String::as_str).unwrap_or(nyx_theme::DEFAULT_THEME));
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
        set_theme: None,
    });
}

struct Screen {
    entry: gtk::PasswordEntry,
    status: gtk::Label,
    indicator: Rc<Indicator>,
    card: gtk::Box,
    time: gtk::Label,
    date: gtk::Label,
    /// The clock and card, centred together; they fade as the session unlocks.
    group: gtk::Box,
    /// Above the clock: as tall as the password makes the card grow, so the clock
    /// stays where it was.
    counter: gtk::Box,
    password: gtk::Box,
    use_password: gtk::Button,
    retry: gtk::Button,
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
    /// The way in "Try again" starts again.
    retry: Cell<Option<Method>>,
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
        retry: Cell::new(None),
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
        set_locked_hint(true);
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
        set_locked_hint(false);
        if let Some(l) = wl.upgrade() {
            l.app.quit();
        }
    });
    inst.lock();
    *l.lock.borrow_mut() = Some(inst);
}

/// logind's `LockedHint` on this user's session, which services that follow the lock
/// through logind read (Nyx Shell Core). The session lock is what keeps the session
/// locked; this only reports it, so a failure is logged and nothing else. Locking is
/// told without waiting; unlocking waits (briefly) so it's told before the locker exits.
fn set_locked_hint(locked: bool) {
    use gtk::gio;
    const SESSION: &str = "/org/freedesktop/login1/session/auto";
    let Ok(bus) = gio::bus_get_sync(gio::BusType::System, gio::Cancellable::NONE) else { return };
    let args = (locked,).to_variant();
    let report = move |e: glib::Error| eprintln!("nyx-locker: couldn't tell logind the session is {}: {e}", if locked { "locked" } else { "unlocked" });
    if locked {
        bus.call(
            Some("org.freedesktop.login1"),
            SESSION,
            "org.freedesktop.login1.Session",
            "SetLockedHint",
            Some(&args),
            None,
            gio::DBusCallFlags::NONE,
            2000,
            gio::Cancellable::NONE,
            move |r| {
                if let Err(e) = r {
                    report(e);
                }
            },
        );
    } else if let Err(e) = bus.call_sync(
        Some("org.freedesktop.login1"),
        SESSION,
        "org.freedesktop.login1.Session",
        "SetLockedHint",
        Some(&args),
        None,
        gio::DBusCallFlags::NONE,
        1000,
        gio::Cancellable::NONE,
    ) {
        report(e);
    }
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

    // The clock and the card: one group in the middle of the screen.
    let group = gtk::Box::new(gtk::Orientation::Vertical, 0);
    group.add_css_class("lock-group");
    group.set_halign(gtk::Align::Center);
    group.set_valign(gtk::Align::Center);
    let counter = gtk::Box::new(gtk::Orientation::Vertical, 0);
    let time = gtk::Label::new(None);
    time.add_css_class("clock");
    let date = gtk::Label::new(None);
    date.add_css_class("date");
    let clock = gtk::Box::new(gtk::Orientation::Vertical, 2);
    clock.set_margin_bottom(40);
    clock.append(&time);
    clock.append(&date);
    group.append(&counter);
    group.append(&clock);

    // The card: the ring, the name, what's happening, and the password or the choice of it.
    let card = gtk::Box::new(gtk::Orientation::Vertical, 10);
    card.add_css_class("card");
    card.set_halign(gtk::Align::Center);
    card.set_size_request(360, -1);
    let name = full_name(&l.user);
    let who = gtk::Label::new(Some(&name));
    who.add_css_class("name");
    who.set_margin_top(6);
    let indicator = Indicator::new(l.look.style, Colours::of_theme(&l.look.theme));
    if matches!(l.look.style, Style::Circular | Style::Cyberpunk) {
        indicator.set_size(RING);
    }
    indicator.set_still(l.look.still);
    let (ring, caption) = indicator.detach();
    let status = gtk::Label::new(None);
    status.add_css_class("status");
    status.set_wrap(true);
    status.set_justify(gtk::Justification::Center);
    status.set_max_width_chars(36);
    status.set_visible(false);
    // One line is always kept for the words, so the card doesn't jump as they come and go.
    let words = gtk::Box::new(gtk::Orientation::Vertical, 2);
    words.set_size_request(-1, 20);
    words.append(&caption);
    words.append(&status);

    let entry = gtk::PasswordEntry::new();
    entry.set_show_peek_icon(true);
    entry.add_css_class("password");
    entry.set_property("placeholder-text", "Password");
    entry.update_property(&[gtk::accessible::Property::Label(&format!("Password for {name}"))]);
    let hint = gtk::Label::new(Some("Press Enter to unlock"));
    hint.add_css_class("hint");
    let password = gtk::Box::new(gtk::Orientation::Vertical, 8);
    password.set_margin_top(4);
    password.append(&entry);
    password.append(&hint);

    let retry = gtk::Button::with_label("Try again");
    let use_password = gtk::Button::with_label("Use password");
    for b in [&retry, &use_password] {
        b.add_css_class("choice");
    }
    retry.set_visible(false);
    let choices = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    choices.set_halign(gtk::Align::Center);
    choices.append(&retry);
    choices.append(&use_password);

    // With a face, fingerprint or key the password waits behind "Use password" (or the
    // first key typed); with only the password, it's there.
    let others = l.methods.iter().any(|m| *m != Method::Password);
    password.set_visible(!others);
    choices.set_visible(others);
    if !others {
        status.set_text("Enter your password");
        status.set_visible(true);
    }

    card.append(&ring);
    card.append(&who);
    card.append(&words);
    card.append(&password);
    card.append(&choices);
    group.append(&card);
    overlay.add_overlay(&group);
    overlay.add_overlay(indicator.glow());

    let wl = Rc::downgrade(l);
    use_password.connect_clicked(move |_| {
        if let Some(l) = wl.upgrade() {
            show_password(&l);
        }
    });
    let wl = Rc::downgrade(l);
    retry.connect_clicked(move |_| {
        if let Some(l) = wl.upgrade() {
            try_again(&l);
        }
    });

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
    // Typing while the password waits behind "Use password" opens it with what's typed.
    let keys = gtk::EventControllerKey::new();
    keys.set_propagation_phase(gtk::PropagationPhase::Capture);
    let wl = Rc::downgrade(l);
    keys.connect_key_pressed(move |_, key, _, _| {
        let Some(l) = wl.upgrade() else { return glib::Propagation::Proceed };
        wake(&l);
        let typed = key.to_unicode().filter(|c| !c.is_control() && !c.is_whitespace());
        if let Some(c) = typed
            && !l.unlocking.get()
            && l.screens.borrow().first().is_some_and(|s| !s.password.is_visible())
        {
            show_password(&l);
            if let Some(s) = l.screens.borrow().first() {
                s.entry.set_text(&c.to_string());
                s.entry.set_position(-1);
            }
            return glib::Propagation::Stop;
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
    l.screens.borrow_mut().push(Screen {
        entry,
        status,
        indicator,
        card,
        time,
        date,
        group,
        counter,
        password,
        use_password,
        retry,
    });
    w
}

/// Open the password on every screen. The card grows below; the counterweight above the
/// clock grows as much, so the clock stays put.
fn show_password(l: &Rc<Locker>) {
    for s in l.screens.borrow().iter() {
        if s.password.is_visible() {
            continue;
        }
        let before = s.card.measure(gtk::Orientation::Vertical, -1).1;
        s.password.set_visible(true);
        s.use_password.set_visible(false);
        s.retry.set_visible(false);
        let after = s.card.measure(gtk::Orientation::Vertical, -1).1;
        s.counter.set_size_request(-1, (after - before).max(0));
    }
    l.retry.set(None);
    indicate(l, Kind::Password, Phase::Idle, "");
    set_status(l, "Enter your password", false);
    if let Some(s) = l.screens.borrow().first() {
        s.entry.grab_focus();
    }
}

/// Offer "Try again" for `m` after a miss.
fn offer_retry(l: &Locker, m: Method) {
    l.retry.set(Some(m));
    for s in l.screens.borrow().iter() {
        s.retry.set_visible(!s.password.is_visible());
    }
}

fn try_again(l: &Rc<Locker>) {
    let Some(m) = l.retry.take() else { return };
    for s in l.screens.borrow().iter() {
        s.retry.set_visible(false);
    }
    match m {
        Method::Face => l.face_after.set(0),
        Method::Fingerprint => l.finger_tries.set(0),
        _ => {}
    }
    run(l, m);
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
        if s.password.is_visible() {
            s.entry.grab_focus();
        } else {
            s.use_password.grab_focus();
        }
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
        s.status.set_visible(!text.is_empty());
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
            show_password(l);
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
            for s in l.screens.borrow().iter() {
                s.use_password.set_sensitive(false);
                s.retry.set_sensitive(false);
            }
            if !l.look.still {
                let wl = Rc::downgrade(l);
                glib::timeout_add_local_once(FADE_AFTER, move || {
                    if let Some(l) = wl.upgrade() {
                        for s in l.screens.borrow().iter() {
                            s.group.add_css_class("unlocking");
                        }
                    }
                });
            }
            let wl = Rc::downgrade(l);
            glib::timeout_add_local_once(UNLOCK_AFTER, move || {
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
                        offer_retry(l, Method::Fingerprint);
                    }
                }
                Method::Face => {
                    l.face_after.set(glib::monotonic_time() + FACE_REST.as_micros() as i64);
                    indicate(l, Kind::Face, Phase::Failure, &text);
                    offer_retry(l, Method::Face);
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
