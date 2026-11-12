//! The screens: the wallpaper and clock on every monitor, the sign-in card on the largest.
//! The clock and card are one group in the middle, as on the lock screen.

use crate::Options;
use gtk::prelude::*;
use gtk::{gdk, glib};
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use nyx_greeter_core::flow::{self, Command, Event};
use nyx_greeter_core::sessions::{self, Session};
use nyx_greeter_core::users::{self, User};
use nyx_greeter_core::{Config, State};
use nyx_signin_ui::{Colours, Indicator, Kind, Phase, Style};
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::OnceLock;
use std::time::Duration;

static THEME: OnceLock<String> = OnceLock::new();

/// The indicator on the card.
const RING: i32 = 96;

/// From signing in to the session: the match shows, then the clock and card fade
/// (style.css). The session starts at once; the greeter leaves after this.
const FADE_AFTER: Duration = Duration::from_millis(250);
const LEAVE_AFTER: Duration = Duration::from_millis(600);

/// The login theme is a system setting, not anyone's desktop choice.
pub fn theme(id: &str) {
    let _ = THEME.set(id.to_string());
}

pub fn install_css() {
    nyx_theme::use_design_2();
    // Nobody's theme choice (anyone's ~/.config/nyx) changes the system's login theme.
    nyx_theme::use_fixed_theme(THEME.get().map(String::as_str).unwrap_or("nyx-dark"));
    nyx_theme::install(nyx_theme::Style {
        window_class: "nyx-greeter",
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

struct Card {
    name: gtk::Label,
    entry: gtk::Entry,
    /// The entry and its hint: shown while PAM asks for a password, hidden while it waits
    /// on a fingerprint, a face or a key.
    password: gtk::Box,
    status: gtk::Label,
    indicator: Rc<Indicator>,
    sessions: gtk::DropDown,
    users_button: gtk::MenuButton,
    card: gtk::Box,
    group: gtk::Box,
    /// Above the clock and under the card: what keeps the clock where it is as the card
    /// grows past, or shrinks below, its height with the password showing (`full`).
    counter: gtk::Box,
    foot: gtk::Box,
    full: Cell<i32>,
}

struct Greeter {
    app: gtk::Application,
    config: Config,
    opts: Options,
    users: Vec<User>,
    sessions: Vec<Session>,
    state: RefCell<State>,
    user: RefCell<Option<User>>,
    tx: std::sync::mpsc::Sender<Command>,
    card: RefCell<Option<Rc<Card>>>,
    clocks: RefCell<Vec<(gtk::Label, gtk::Label)>>,
    typed_once: Cell<bool>,
    /// Reduce motion: the indicator stays still and nothing fades.
    still: bool,
    /// When signing in succeeded (monotonic µs), so the fade has its time before leaving.
    signed_in: Cell<Option<i64>>,
    _fake: RefCell<Option<nyx_greetd::fake::Fake>>,
}

fn test_mode(o: &Options) -> bool {
    o.only.is_some()
}

pub fn start(app: &gtk::Application, config: Config, opts: Options) {
    std::mem::forget(app.hold());
    let mut fake = None;
    let socket = if opts.demo {
        let path = std::env::temp_dir().join(format!("nyx-greeter-demo-{}.sock", std::process::id()));
        let rules = nyx_greetd::fake::Rules {
            fingerprint: opts.finger,
            finger_wait: Duration::from_secs(if opts.finger { 4 } else { 0 }),
            ..Default::default()
        };
        match nyx_greetd::fake::serve(&path, rules) {
            Ok(f) => {
                let p = f.path.clone();
                fake = Some(f);
                Some(p)
            }
            Err(e) => {
                eprintln!("nyx-greeter: the demo greetd didn't start: {e}");
                None
            }
        }
    } else {
        None
    };
    let (etx, erx) = async_channel::unbounded::<Event>();
    let tx = flow::spawn(socket, move |e| {
        let _ = etx.send_blocking(e);
    });
    let mut users = users::all();
    if users.is_empty() && opts.demo {
        users = users::parse("ken:x:1000:1000:Ken Smith:/home/ken:/bin/zsh\namy:x:1001:1001:Amy Smith:/home/amy:/bin/zsh\n");
    }
    let state = State::load();
    let sessions = sessions::all(&config.session_dirs);
    let g = Rc::new(Greeter {
        app: app.clone(),
        config,
        opts,
        users,
        sessions,
        state: RefCell::new(state),
        user: RefCell::new(None),
        tx,
        card: RefCell::default(),
        clocks: RefCell::default(),
        typed_once: Cell::new(false),
        still: nyx_theme::A11y::system().reduce_motion,
        signed_in: Cell::new(None),
        _fake: RefCell::new(fake),
    });
    build_windows(&g);
    let wg = Rc::downgrade(&g);
    glib::spawn_future_local(async move {
        while let Ok(e) = erx.recv().await {
            let Some(g) = wg.upgrade() else { return };
            on_event(&g, e);
        }
    });
    tick_clocks(&g);
    let wg = Rc::downgrade(&g);
    glib::timeout_add_seconds_local(1, move || match wg.upgrade() {
        Some(g) => {
            tick_clocks(&g);
            glib::ControlFlow::Continue
        }
        None => glib::ControlFlow::Break,
    });
    let first = {
        let st = g.state.borrow();
        st.last_user.as_ref().and_then(|n| g.users.iter().find(|u| &u.name == n)).or(g.users.first()).cloned()
    };
    if let Some(u) = first {
        choose_user(&g, u);
    }
    if g.opts.show_users {
        let wg = Rc::downgrade(&g);
        glib::timeout_add_local_once(Duration::from_millis(800), move || {
            if let Some(g) = wg.upgrade()
                && let Some(c) = g.card.borrow().as_ref()
            {
                c.users_button.popup();
            }
        });
    }
    std::mem::forget(g);
}

fn monitors(only: Option<&str>) -> Vec<gdk::Monitor> {
    let Some(d) = gdk::Display::default() else {
        return vec![];
    };
    d.monitors().iter::<gdk::Monitor>().flatten().filter(|m| only.is_none_or(|o| m.connector().as_deref() == Some(o))).collect()
}

fn build_windows(g: &Rc<Greeter>) {
    let mons = monitors(g.opts.only.as_deref());
    let main = mons.iter().max_by_key(|m| m.geometry().width() * m.geometry().height()).cloned();
    for m in &mons {
        let w = gtk::Window::new();
        w.set_application(Some(&g.app));
        w.add_css_class("nyx-greeter");
        w.init_layer_shell();
        w.set_namespace(Some("nyx-greeter"));
        w.set_layer(Layer::Overlay);
        w.set_monitor(Some(m));
        for e in [Edge::Top, Edge::Bottom, Edge::Left, Edge::Right] {
            w.set_anchor(e, true);
        }
        w.set_exclusive_zone(-1);
        let is_main = main.as_ref() == Some(m);
        // A test copy never takes the keyboard from whoever is at the computer.
        w.set_keyboard_mode(if is_main && !test_mode(&g.opts) { KeyboardMode::Exclusive } else { KeyboardMode::None });
        let overlay = gtk::Overlay::new();
        let bg = gtk::Picture::new();
        bg.set_content_fit(gtk::ContentFit::Cover);
        bg.set_can_shrink(true);
        if let Some(p) = &g.config.wallpaper
            && let Ok(t) = gdk::Texture::from_filename(p)
        {
            bg.set_paintable(Some(&t));
        }
        overlay.set_child(Some(&bg));
        if g.config.dim > 0.0 {
            let dim = gtk::Box::new(gtk::Orientation::Vertical, 0);
            dim.add_css_class("dim");
            let provider = gtk::CssProvider::new();
            provider
                .load_from_string(&format!(".dim {{ background-color: alpha(black, {:.2}); }}", g.config.dim.clamp(0.0, 0.95)));
            gtk::style_context_add_provider_for_display(
                &WidgetExt::display(&w),
                &provider,
                gtk::STYLE_PROVIDER_PRIORITY_APPLICATION + 1,
            );
            overlay.add_overlay(&dim);
        }

        // The clock and the card: one group in the middle of the screen.
        let group = gtk::Box::new(gtk::Orientation::Vertical, 0);
        group.add_css_class("greeter-group");
        group.set_halign(gtk::Align::Center);
        group.set_valign(gtk::Align::Center);
        let counter = gtk::Box::new(gtk::Orientation::Vertical, 0);
        group.append(&counter);
        if g.config.clock {
            let time = gtk::Label::new(None);
            time.add_css_class("clock");
            let date = gtk::Label::new(None);
            date.add_css_class("date");
            let c = gtk::Box::new(gtk::Orientation::Vertical, 2);
            if is_main {
                c.set_margin_bottom(40);
            }
            c.append(&time);
            c.append(&date);
            group.append(&c);
            g.clocks.borrow_mut().push((time, date));
        }
        if is_main {
            let card = build_card(g, &group, &counter);
            group.append(&card.card);
            group.append(&card.foot);
            *g.card.borrow_mut() = Some(card);
        }
        overlay.add_overlay(&group);
        if is_main && g.config.power {
            overlay.add_overlay(&power_row(g));
        }
        if is_main && let Some(c) = g.card.borrow().as_ref() {
            overlay.add_overlay(c.indicator.glow());
        }
        w.set_child(Some(&overlay));
        w.present();
    }
}

fn icon_button(icon: &str, label: &str) -> gtk::Button {
    let b = gtk::Button::from_icon_name(icon);
    b.set_tooltip_text(Some(label));
    b.update_property(&[gtk::accessible::Property::Label(label)]);
    b.add_css_class("flat");
    b.add_css_class("power");
    b
}

fn power_row(g: &Rc<Greeter>) -> gtk::Box {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    row.set_halign(gtk::Align::End);
    row.set_valign(gtk::Align::End);
    row.set_margin_end(28);
    row.set_margin_bottom(24);
    for (icon, label, verb) in [
        ("weather-clear-night-symbolic", "Sleep", "suspend"),
        ("system-reboot-symbolic", "Restart", "reboot"),
        ("system-shutdown-symbolic", "Shut down", "poweroff"),
    ] {
        let b = icon_button(icon, label);
        let demo = g.opts.demo || test_mode(&g.opts);
        b.connect_clicked(move |_| {
            if demo {
                eprintln!("nyx-greeter: (demo) would run systemctl {verb}");
                return;
            }
            let _ = std::process::Command::new("systemctl").arg(verb).spawn();
        });
        row.append(&b);
    }
    row
}

/// The card: the ring, the name, what's happening, the password, and the other users
/// and sessions.
fn build_card(g: &Rc<Greeter>, group: &gtk::Box, counter: &gtk::Box) -> Rc<Card> {
    let card = gtk::Box::new(gtk::Orientation::Vertical, 10);
    card.add_css_class("card");
    card.set_halign(gtk::Align::Center);
    card.set_size_request(360, -1);
    let style = Style::parse(&g.config.style).unwrap_or_default();
    let indicator = Indicator::new(style, Colours::of_theme(&g.config.theme));
    if matches!(style, Style::Circular | Style::Cyberpunk) {
        indicator.set_size(RING);
    }
    indicator.set_still(g.still);
    indicator.set(Kind::Password, Phase::Idle, "");
    let (ring, caption) = indicator.detach();
    let name = gtk::Label::new(None);
    name.add_css_class("name");
    name.set_margin_top(6);
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

    let entry = gtk::Entry::new();
    entry.set_visibility(false);
    entry.set_input_purpose(gtk::InputPurpose::Password);
    entry.set_placeholder_text(Some("Password"));
    entry.add_css_class("password");
    entry.set_sensitive(false);
    let hint = gtk::Label::new(Some("Press Enter to sign in"));
    hint.add_css_class("hint");
    let password = gtk::Box::new(gtk::Orientation::Vertical, 8);
    password.set_margin_top(4);
    password.append(&entry);
    password.append(&hint);

    let names: Vec<&str> = g.sessions.iter().map(|s| s.name.as_str()).collect();
    let sessions = gtk::DropDown::from_strings(&names);
    sessions.set_tooltip_text(Some("Session"));
    sessions.update_property(&[gtk::accessible::Property::Label("Session")]);
    sessions.set_visible(g.sessions.len() > 1);
    let users_button = gtk::MenuButton::new();
    users_button.set_label("Switch user");
    users_button.set_tooltip_text(Some("Other users"));
    users_button.update_property(&[gtk::accessible::Property::Label("Other users")]);
    users_button.add_css_class("flat");
    users_button.set_visible(g.config.user_list && g.users.len() > 1);
    let list = gtk::Box::new(gtk::Orientation::Vertical, 2);
    for u in &g.users {
        let b = gtk::Button::new();
        b.add_css_class("flat");
        b.add_css_class("user-row");
        let h = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        let a = avatar_widget(u, 28);
        h.append(&a);
        let l = gtk::Label::new(Some(&u.display));
        l.set_xalign(0.0);
        h.append(&l);
        b.set_child(Some(&h));
        let (wg, user) = (Rc::downgrade(g), u.clone());
        b.connect_clicked(move |b| {
            if let Some(g) = wg.upgrade() {
                if let Some(p) = b.ancestor(gtk::Popover::static_type())
                    && let Ok(p) = p.downcast::<gtk::Popover>()
                {
                    p.popdown();
                }
                choose_user(&g, user.clone());
            }
        });
        list.append(&b);
    }
    let pop = gtk::Popover::new();
    if test_mode(&g.opts) {
        pop.set_autohide(false);
    }
    pop.set_child(Some(&list));
    pop.add_css_class("users");
    users_button.set_popover(Some(&pop));
    let choices = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    choices.add_css_class("greeter-choices");
    choices.set_halign(gtk::Align::Center);
    choices.append(&users_button);
    choices.append(&sessions);
    choices.set_visible(users_button.is_visible() || sessions.is_visible());

    card.append(&ring);
    card.append(&name);
    card.append(&words);
    card.append(&password);
    card.append(&choices);
    let foot = gtk::Box::new(gtk::Orientation::Vertical, 0);

    let wg = Rc::downgrade(g);
    entry.connect_activate(move |e| {
        let Some(g) = wg.upgrade() else { return };
        let text = e.text().to_string();
        e.set_sensitive(false);
        if let Some(c) = g.card.borrow().as_ref() {
            c.indicator.set(Kind::Password, Phase::Scanning, "");
        }
        set_status(&g, "", false);
        let _ = g.tx.send(Command::Answer(text));
    });
    let keys = gtk::EventControllerKey::new();
    let wg = Rc::downgrade(g);
    keys.connect_key_pressed(move |_, key, _, _| {
        if key == gdk::Key::Escape
            && let Some(g) = wg.upgrade()
            && let Some(c) = g.card.borrow().as_ref()
        {
            c.entry.set_text("");
            return glib::Propagation::Stop;
        }
        glib::Propagation::Proceed
    });
    card.add_controller(keys);
    Rc::new(Card {
        name,
        entry,
        password,
        status,
        indicator,
        sessions,
        users_button,
        card,
        group: group.clone(),
        counter: counter.clone(),
        foot,
        full: Cell::new(0),
    })
}

/// Keep the clock where it is: the card's height with the password showing is the
/// measure; what it grows past goes above the clock, what it's short of under the card.
fn balance(c: &Card) {
    let now = c.card.measure(gtk::Orientation::Vertical, -1).1;
    if c.password.is_visible() {
        c.full.set(now);
    }
    let full = if c.full.get() > 0 { c.full.get() } else { now };
    c.counter.set_size_request(-1, (now - full).max(0));
    c.foot.set_size_request(-1, (full - now).max(0));
}

fn show_password(c: &Card, on: bool) {
    if c.password.is_visible() != on {
        c.password.set_visible(on);
    }
    balance(c);
}

fn avatar_widget(u: &User, size: i32) -> gtk::Widget {
    if let Some(p) = &u.avatar
        && let Ok(t) = gdk::Texture::from_filename(p)
    {
        let pic = gtk::Picture::for_paintable(&t);
        pic.set_content_fit(gtk::ContentFit::Cover);
        pic.set_size_request(size, size);
        pic.add_css_class("avatar-image");
        pic.set_overflow(gtk::Overflow::Hidden);
        return pic.upcast();
    }
    let l = gtk::Label::new(Some(&u.initials()));
    l.add_css_class("initials");
    l.set_size_request(size, size);
    if size < 40 {
        l.add_css_class("small");
    }
    l.upcast()
}

fn choose_user(g: &Rc<Greeter>, u: User) {
    if let Some(c) = g.card.borrow().as_ref() {
        c.name.set_text(&u.display);
        c.entry.set_text("");
        c.entry.set_sensitive(false);
        c.entry.update_property(&[gtk::accessible::Property::Label(&format!("Password for {}", u.display))]);
        let remembered = g.state.borrow().sessions.get(&u.name).cloned();
        if let Some(s) = sessions::pick(&g.sessions, remembered.as_deref(), g.config.default_session.as_deref())
            && let Some(i) = g.sessions.iter().position(|x| x.id == s.id)
        {
            c.sessions.set_selected(i as u32);
        }
        c.indicator.set(Kind::Password, Phase::Idle, "");
    }
    set_status(g, "", false);
    let name = u.name.clone();
    *g.user.borrow_mut() = Some(u);
    let _ = g.tx.send(Command::Login(name));
}

fn set_status(g: &Greeter, text: &str, error: bool) {
    if let Some(c) = g.card.borrow().as_ref() {
        c.status.set_text(text);
        c.status.set_visible(!text.is_empty());
        if error {
            c.status.add_css_class("error");
        } else {
            c.status.remove_css_class("error");
        }
        balance(c);
    }
}

fn shake(c: &Card) {
    if c.indicator.style() == Style::Edge {
        return;
    }
    c.card.add_css_class("shake");
    let r = c.card.clone();
    glib::timeout_add_local_once(Duration::from_millis(450), move || r.remove_css_class("shake"));
}

fn on_event(g: &Rc<Greeter>, e: Event) {
    let card = g.card.borrow().clone();
    match e {
        Event::Prompt { text, secret } => {
            if let Some(c) = &card {
                // The reader or camera gave up and PAM went on to the password.
                if c.indicator.phase() == Phase::Scanning && c.indicator.kind() != Kind::Password {
                    c.indicator.set(c.indicator.kind(), Phase::Timeout, "Use your password");
                } else if c.indicator.phase() != Phase::Failure {
                    c.indicator.set(Kind::Password, Phase::Idle, "");
                }
                c.entry.set_visibility(!secret);
                c.entry.set_placeholder_text(Some(if text.is_empty() { "Password" } else { &text }));
                c.entry.set_sensitive(true);
                show_password(c, true);
                c.entry.grab_focus();
                if let Some(t) = g.opts.type_text.clone()
                    && !g.typed_once.replace(true)
                {
                    c.entry.set_text(&t);
                    c.entry.emit_activate();
                }
            }
        }
        Event::Info(text) if biometric(g, &text, false) => {}
        Event::Info(text) => set_status(g, &text, false),
        Event::Notice(text) => {
            if !biometric(g, &text, true) {
                set_status(g, &text, true);
            }
        }
        Event::Authenticated => {
            let Some(c) = card else { return };
            c.indicator.set(c.indicator.kind(), Phase::Success, "");
            set_status(g, "", false);
            let Some(s) = g.sessions.get(c.sessions.selected() as usize).cloned().or_else(|| g.sessions.first().cloned()) else {
                set_status(g, "No sessions are installed.", true);
                return;
            };
            if let Some(u) = g.user.borrow().as_ref() {
                g.state.borrow_mut().remember(&u.name, &s.id);
                if !g.opts.demo {
                    g.state.borrow().save();
                }
            }
            g.signed_in.set(Some(glib::monotonic_time()));
            if !g.still {
                let group = c.group.clone();
                glib::timeout_add_local_once(FADE_AFTER, move || group.add_css_class("leaving"));
            }
            let _ = g.tx.send(Command::Start { cmd: s.cmd.clone(), env: s.env() });
        }
        Event::Started => {
            if test_mode(&g.opts) {
                return;
            }
            // The fade has its time; the session is already starting.
            let since = g.signed_in.get().map_or(i64::MAX, |t| glib::monotonic_time() - t);
            let left = (LEAVE_AFTER.as_micros() as i64 - since).max(0) as u64;
            let app = g.app.clone();
            glib::timeout_add_local_once(Duration::from_micros(left), move || app.quit());
        }
        Event::Failed(msg) => {
            if let Some(c) = &card {
                c.entry.set_text("");
                shake(c);
                c.indicator.set(c.indicator.kind(), Phase::Failure, &msg);
            }
            set_status(g, "", false);
            let user = g.user.borrow().as_ref().map(|u| u.name.clone());
            if let Some(name) = user {
                let _ = g.tx.send(Command::Login(name));
            }
        }
        Event::Broken(msg) => {
            if let Some(c) = &card {
                c.entry.set_sensitive(false);
            }
            set_status(g, &msg, true);
        }
    }
}

/// A message about a fingerprint, a face or a key goes to the ring, and the password
/// waits until PAM asks for it: scanning, or a miss when `miss`. Whether it was one.
fn biometric(g: &Greeter, text: &str, miss: bool) -> bool {
    let Some(kind) = nyx_signin_ui::kind_of(text) else {
        return false;
    };
    let Some(c) = g.card.borrow().clone() else {
        return false;
    };
    let failed = miss || text.to_lowercase().contains("fail") || text.to_lowercase().contains("not match");
    c.indicator.set(kind, if failed { Phase::Failure } else { Phase::Scanning }, text);
    c.status.set_text("");
    c.status.set_visible(false);
    show_password(&c, false);
    true
}

fn tick_clocks(g: &Greeter) {
    let now = glib::DateTime::now_local().ok();
    let (t, d) = match &now {
        Some(n) => (
            n.format("%H:%M").map(|s| s.to_string()).unwrap_or_default(),
            n.format("%A, %B %-e").map(|s| s.to_string()).unwrap_or_default(),
        ),
        None => (String::new(), String::new()),
    };
    for (time, date) in g.clocks.borrow().iter() {
        if time.text() != t {
            time.set_text(&t);
        }
        if date.text() != d {
            date.set_text(&d);
        }
    }
}
