//! The screens: the wallpaper on every monitor, the login card on the largest.

use crate::Options;
use gtk::prelude::*;
use gtk::{gdk, glib};
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use nyx_greeter_core::flow::{self, Command, Event};
use nyx_greeter_core::sessions::{self, Session};
use nyx_greeter_core::users::{self, User};
use nyx_greeter_core::{Config, State};
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::OnceLock;
use std::time::Duration;

static THEME: OnceLock<String> = OnceLock::new();

/// The login theme is a system setting, not anyone's desktop choice.
pub fn theme(id: &str) {
    let _ = THEME.set(id.to_string());
}

pub fn install_css() {
    nyx_theme::use_design_2();
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
        // Choosing its own theme keeps a suite theme file (anyone's ~/.config/nyx) from
        // overriding the system's login theme.
        set_theme: Some(|_, _| {}),
    });
}

struct Card {
    avatar: gtk::Overlay,
    name: gtk::Label,
    entry: gtk::Entry,
    status: gtk::Label,
    spinner: gtk::Spinner,
    sessions: gtk::DropDown,
    users_button: gtk::MenuButton,
    root: gtk::Box,
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
    _fake: RefCell<Option<nyx_greetd::fake::Fake>>,
}

fn test_mode(o: &Options) -> bool {
    o.only.is_some()
}

pub fn start(app: &gtk::Application, config: Config, opts: Options) {
    std::mem::forget(app.hold());
    let mut fake = None;
    let socket = if opts.demo {
        let path =
            std::env::temp_dir().join(format!("nyx-greeter-demo-{}.sock", std::process::id()));
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
        users = users::parse(
            "ken:x:1000:1000:Ken Smith:/home/ken:/bin/zsh\namy:x:1001:1001:Amy Smith:/home/amy:/bin/zsh\n",
        );
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
        st.last_user
            .as_ref()
            .and_then(|n| g.users.iter().find(|u| &u.name == n))
            .or(g.users.first())
            .cloned()
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
    d.monitors()
        .iter::<gdk::Monitor>()
        .flatten()
        .filter(|m| only.is_none_or(|o| m.connector().as_deref() == Some(o)))
        .collect()
}

fn build_windows(g: &Rc<Greeter>) {
    let mons = monitors(g.opts.only.as_deref());
    let main = mons
        .iter()
        .max_by_key(|m| m.geometry().width() * m.geometry().height())
        .cloned();
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
        w.set_keyboard_mode(if is_main && !test_mode(&g.opts) {
            KeyboardMode::Exclusive
        } else {
            KeyboardMode::None
        });
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
        let dim = gtk::Box::new(gtk::Orientation::Vertical, 0);
        dim.add_css_class("dim");
        let provider = gtk::CssProvider::new();
        provider.load_from_string(&format!(
            ".dim {{ background-color: alpha(black, {:.2}); }}",
            g.config.dim.clamp(0.0, 0.95)
        ));
        gtk::style_context_add_provider_for_display(
            &WidgetExt::display(&w),
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION + 1,
        );
        overlay.add_overlay(&dim);
        let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
        if g.config.clock {
            let time = gtk::Label::new(None);
            time.add_css_class("clock");
            let date = gtk::Label::new(None);
            date.add_css_class("date");
            let c = gtk::Box::new(gtk::Orientation::Vertical, 2);
            c.set_margin_top(72);
            c.append(&time);
            c.append(&date);
            content.append(&c);
            g.clocks.borrow_mut().push((time, date));
        }
        let spacer = gtk::Box::new(gtk::Orientation::Vertical, 0);
        spacer.set_vexpand(true);
        content.append(&spacer);
        if is_main {
            let card = build_card(g);
            content.append(&card.root);
            *g.card.borrow_mut() = Some(card);
        }
        let spacer = gtk::Box::new(gtk::Orientation::Vertical, 0);
        spacer.set_vexpand(true);
        content.append(&spacer);
        if is_main && g.config.power {
            content.append(&power_row(g));
        }
        overlay.add_overlay(&content);
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

fn mark(g: &Greeter) -> Option<gtk::Image> {
    let path = g.config.mark.clone().or_else(|| {
        [
            "/run/current-system/sw/share/nyx/brand/nyx-mark.png",
            "/usr/share/nyx/brand/nyx-mark.png",
        ]
        .iter()
        .map(std::path::PathBuf::from)
        .find(|p| p.exists())
    })?;
    let t = gdk::Texture::from_filename(path).ok()?;
    let p = gtk::Image::from_paintable(Some(&t));
    p.set_pixel_size(64);
    p.set_halign(gtk::Align::Center);
    p.set_margin_bottom(20);
    Some(p)
}

fn build_card(g: &Rc<Greeter>) -> Rc<Card> {
    let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
    root.set_halign(gtk::Align::Center);
    if let Some(m) = mark(g) {
        root.append(&m);
    }
    let card = gtk::Box::new(gtk::Orientation::Vertical, 12);
    card.add_css_class("card");
    card.set_size_request(360, -1);
    let avatar = gtk::Overlay::new();
    avatar.add_css_class("avatar");
    avatar.set_halign(gtk::Align::Center);
    avatar.set_size_request(88, 88);
    let name = gtk::Label::new(None);
    name.add_css_class("name");
    let entry = gtk::Entry::new();
    entry.set_visibility(false);
    entry.set_input_purpose(gtk::InputPurpose::Password);
    entry.set_placeholder_text(Some("Password"));
    entry.add_css_class("password");
    entry.set_sensitive(false);
    let status_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    status_row.set_halign(gtk::Align::Center);
    let spinner = gtk::Spinner::new();
    spinner.set_visible(false);
    let status = gtk::Label::new(None);
    status.add_css_class("status");
    status.set_wrap(true);
    status.set_justify(gtk::Justification::Center);
    status.set_max_width_chars(36);
    status_row.append(&spinner);
    status_row.append(&status);
    let names: Vec<&str> = g.sessions.iter().map(|s| s.name.as_str()).collect();
    let sessions = gtk::DropDown::from_strings(&names);
    sessions.set_tooltip_text(Some("Session"));
    sessions.update_property(&[gtk::accessible::Property::Label("Session")]);
    sessions.set_visible(g.sessions.len() > 1);
    let users_button = gtk::MenuButton::new();
    users_button.set_icon_name("avatar-default-symbolic");
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
    let bottom = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    bottom.set_halign(gtk::Align::Center);
    bottom.append(&sessions);
    bottom.append(&users_button);
    card.append(&avatar);
    card.append(&name);
    card.append(&entry);
    card.append(&status_row);
    card.append(&bottom);
    root.append(&card);
    let wg = Rc::downgrade(g);
    entry.connect_activate(move |e| {
        let Some(g) = wg.upgrade() else { return };
        let text = e.text().to_string();
        e.set_sensitive(false);
        set_status(&g, "Checking…", false, true);
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
    root.add_controller(keys);
    Rc::new(Card {
        avatar,
        name,
        entry,
        status,
        spinner,
        sessions,
        users_button,
        root,
    })
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
        while let Some(ch) = c.avatar.first_child() {
            c.avatar.remove_overlay(&ch);
            if ch.parent().is_some() {
                ch.unparent();
            }
        }
        c.avatar.set_child(Some(&avatar_widget(&u, 88)));
        c.name.set_text(&u.display);
        c.entry.set_text("");
        c.entry.set_sensitive(false);
        c.entry
            .update_property(&[gtk::accessible::Property::Label(&format!(
                "Password for {}",
                u.display
            ))]);
        let remembered = g.state.borrow().sessions.get(&u.name).cloned();
        if let Some(s) = sessions::pick(
            &g.sessions,
            remembered.as_deref(),
            g.config.default_session.as_deref(),
        ) && let Some(i) = g.sessions.iter().position(|x| x.id == s.id)
        {
            c.sessions.set_selected(i as u32);
        }
    }
    set_status(g, "", false, false);
    let name = u.name.clone();
    *g.user.borrow_mut() = Some(u);
    let _ = g.tx.send(Command::Login(name));
}

fn set_status(g: &Greeter, text: &str, error: bool, busy: bool) {
    if let Some(c) = g.card.borrow().as_ref() {
        c.status.set_text(text);
        if error {
            c.status.add_css_class("error");
        } else {
            c.status.remove_css_class("error");
        }
        c.spinner.set_visible(busy);
        c.spinner.set_spinning(busy);
    }
}

fn shake(c: &Card) {
    c.root.add_css_class("shake");
    let r = c.root.clone();
    glib::timeout_add_local_once(Duration::from_millis(450), move || {
        r.remove_css_class("shake")
    });
}

fn on_event(g: &Rc<Greeter>, e: Event) {
    let card = g.card.borrow().clone();
    match e {
        Event::Prompt { text, secret } => {
            if let Some(c) = &card {
                c.entry.set_visibility(!secret);
                c.entry.set_placeholder_text(Some(if text.is_empty() {
                    "Password"
                } else {
                    &text
                }));
                c.entry.set_sensitive(true);
                c.entry.grab_focus();
                c.spinner.set_visible(false);
                if let Some(t) = g.opts.type_text.clone()
                    && !g.typed_once.replace(true)
                {
                    c.entry.set_text(&t);
                    c.entry.emit_activate();
                }
            }
        }
        Event::Info(text) => set_status(g, &text, false, true),
        Event::Notice(text) => set_status(g, &text, true, false),
        Event::Authenticated => {
            set_status(g, "Starting…", false, true);
            let Some(c) = card else { return };
            let Some(s) = g
                .sessions
                .get(c.sessions.selected() as usize)
                .cloned()
                .or_else(|| g.sessions.first().cloned())
            else {
                set_status(g, "No sessions are installed.", true, false);
                return;
            };
            if let Some(u) = g.user.borrow().as_ref() {
                g.state.borrow_mut().remember(&u.name, &s.id);
                if !g.opts.demo {
                    g.state.borrow().save();
                }
            }
            let _ = g.tx.send(Command::Start {
                cmd: s.cmd.clone(),
                env: s.env(),
            });
        }
        Event::Started => {
            if test_mode(&g.opts) {
                set_status(g, "Signed in (demo)", false, false);
                return;
            }
            g.app.quit();
        }
        Event::Failed(msg) => {
            if let Some(c) = &card {
                c.entry.set_text("");
                shake(c);
            }
            set_status(g, &msg, true, false);
            let user = g.user.borrow().as_ref().map(|u| u.name.clone());
            if let Some(name) = user {
                let _ = g.tx.send(Command::Login(name));
            }
        }
        Event::Broken(msg) => {
            if let Some(c) = &card {
                c.entry.set_sensitive(false);
            }
            set_status(g, &msg, true, false);
        }
    }
}

fn tick_clocks(g: &Greeter) {
    let now = glib::DateTime::now_local().ok();
    let (t, d) = match &now {
        Some(n) => (
            n.format("%H:%M").map(|s| s.to_string()).unwrap_or_default(),
            n.format("%A, %B %-e")
                .map(|s| s.to_string())
                .unwrap_or_default(),
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
