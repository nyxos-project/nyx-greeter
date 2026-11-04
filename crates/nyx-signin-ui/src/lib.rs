//! The sign-in pieces Nyx Greeter and Nyx Locker share: the indicator that shows a face
//! scan, a fingerprint, a security key or the password being checked, in four styles
//! (Circular Face Scan, Cyberpunk Scanner, Minimal Floating, Edge Glow), and avatars.
//!
//! The indicator only draws: callers set the method and phase from their PAM flow, so a
//! preview can show it without authenticating anything. Surfaces are solid (Nyx Design
//! 2.0), and colours come from the caller's theme.

pub mod glyphs;

use gtk::prelude::*;
use gtk::{cairo, gdk, glib};
use std::cell::RefCell;
use std::f64::consts::PI;
use std::rc::Rc;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Style {
    #[default]
    Circular,
    Cyberpunk,
    Minimal,
    Edge,
}

impl Style {
    pub const ALL: [Style; 4] = [Style::Circular, Style::Cyberpunk, Style::Minimal, Style::Edge];

    pub fn parse(s: &str) -> Option<Style> {
        Some(match s {
            "circular" => Style::Circular,
            "cyberpunk" => Style::Cyberpunk,
            "minimal" => Style::Minimal,
            "edge" => Style::Edge,
            _ => return None,
        })
    }

    pub fn id(self) -> &'static str {
        match self {
            Style::Circular => "circular",
            Style::Cyberpunk => "cyberpunk",
            Style::Minimal => "minimal",
            Style::Edge => "edge",
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Style::Circular => "Circular face scan",
            Style::Cyberpunk => "Cyberpunk scanner",
            Style::Minimal => "Minimal floating",
            Style::Edge => "Edge glow",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Kind {
    #[default]
    Face,
    Fingerprint,
    Key,
    Password,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Phase {
    #[default]
    Idle,
    Scanning,
    Success,
    Failure,
    Timeout,
}

/// The theme's colours the indicator draws with.
#[derive(Debug, Clone, Copy)]
pub struct Colours {
    pub accent: gdk::RGBA,
    pub surface: gdk::RGBA,
    pub text: gdk::RGBA,
    pub dim: gdk::RGBA,
    pub good: gdk::RGBA,
    pub bad: gdk::RGBA,
}

impl Colours {
    /// From `#rrggbb` strings; a bad one falls back to Nyx Dark's.
    pub fn from_hex(accent: &str, surface: &str, text: &str, dim: &str, good: &str, bad: &str) -> Colours {
        let c = |s: &str, d: &str| gdk::RGBA::parse(s).unwrap_or_else(|_| gdk::RGBA::parse(d).expect("a colour"));
        Colours {
            accent: c(accent, "#5b9cff"),
            surface: c(surface, "#141418"),
            text: c(text, "#e8e8ec"),
            dim: c(dim, "#8a8a94"),
            good: c(good, "#4fd18b"),
            bad: c(bad, "#ff6b6b"),
        }
    }
}

impl Colours {
    /// A Nyx theme's colours (its accent, surface, text, dim text, green and danger).
    pub fn of_theme(id: &str) -> Colours {
        match nyx_theme::all().into_iter().find(|t| t.id == nyx_theme::current_id(id)) {
            Some(t) => {
                let p = &t.palette;
                let green = t
                    .look
                    .swatches
                    .iter()
                    .find(|(n, _)| n == "green")
                    .map(|(_, c)| c.clone())
                    .unwrap_or_else(|| "#4fd18b".into());
                Colours::from_hex(&p.accent, &p.surface, &p.text, &p.dim_text, &green, &p.danger)
            }
            None => Colours::default(),
        }
    }
}

impl Default for Colours {
    fn default() -> Self {
        Colours::from_hex("#5b9cff", "#141418", "#e8e8ec", "#8a8a94", "#4fd18b", "#ff6b6b")
    }
}

/// What the indicator says for a method and phase, unless the caller has a message.
pub fn label(style: Style, kind: Kind, phase: Phase) -> String {
    if style == Style::Cyberpunk {
        let m = match kind {
            Kind::Face => "FACE",
            Kind::Fingerprint => "FINGERPRINT",
            Kind::Key => "KEY",
            Kind::Password => "PASSWORD",
        };
        return match phase {
            Phase::Success => "MATCH // ACCESS GRANTED".into(),
            Phase::Failure => format!("NO MATCH // {m}"),
            Phase::Timeout => "TIMEOUT // USE PASSWORD".into(),
            Phase::Scanning => format!("SCAN // {m}"),
            Phase::Idle => format!("READY // {m}"),
        };
    }
    match phase {
        Phase::Success => "Welcome back".into(),
        Phase::Failure if kind == Kind::Face => "Face not recognised".into(),
        Phase::Failure => "Not recognised".into(),
        Phase::Timeout => "Timed out. Use your password".into(),
        Phase::Scanning => match kind {
            Kind::Fingerprint => "Touch the fingerprint reader".into(),
            Kind::Key => "Touch your security key".into(),
            Kind::Password => "Checking…".into(),
            Kind::Face => "Looking for you…".into(),
        },
        Phase::Idle => String::new(),
    }
}

struct State {
    style: Style,
    kind: Kind,
    phase: Phase,
    colours: Colours,
    /// When the current phase began (frame-clock µs), for the sweeps and pulses.
    since: i64,
    /// When a failure's shake began.
    shake: Option<i64>,
    now: i64,
    tick: Option<gtk::TickCallbackId>,
}

/// The indicator: a drawing and its label. For [`Style::Edge`], put [`Indicator::glow`]
/// over the whole window and the root where the label goes.
pub struct Indicator {
    root: gtk::Box,
    area: gtk::DrawingArea,
    glow: gtk::DrawingArea,
    label: gtk::Label,
    state: Rc<RefCell<State>>,
}

fn now(w: &impl IsA<gtk::Widget>) -> i64 {
    w.frame_clock().map(|c| c.frame_time()).unwrap_or_else(glib::monotonic_time)
}

fn source(cr: &cairo::Context, c: &gdk::RGBA, alpha: f64) {
    cr.set_source_rgba(c.red() as f64, c.green() as f64, c.blue() as f64, c.alpha() as f64 * alpha);
}

fn rounded(cr: &cairo::Context, x: f64, y: f64, w: f64, h: f64, r: f64) {
    cr.new_sub_path();
    cr.arc(x + w - r, y + r, r, -PI / 2.0, 0.0);
    cr.arc(x + w - r, y + h - r, r, 0.0, PI / 2.0);
    cr.arc(x + r, y + h - r, r, PI / 2.0, PI);
    cr.arc(x + r, y + r, r, PI, 1.5 * PI);
    cr.close_path();
}

impl Indicator {
    pub fn new(style: Style, colours: Colours) -> Rc<Indicator> {
        let root = gtk::Box::new(gtk::Orientation::Vertical, 12);
        root.add_css_class("signin-indicator");
        // The card's width, so the scanner's words fit on a line.
        root.set_halign(gtk::Align::Fill);
        let area = gtk::DrawingArea::new();
        area.set_halign(gtk::Align::Center);
        let glow = gtk::DrawingArea::new();
        glow.set_can_target(false);
        glow.set_hexpand(true);
        glow.set_vexpand(true);
        let label = gtk::Label::new(None);
        label.add_css_class("signin-indicator-label");
        label.set_wrap(true);
        label.set_justify(gtk::Justification::Center);
        label.set_max_width_chars(40);
        root.append(&area);
        root.append(&label);
        let state = Rc::new(RefCell::new(State {
            style,
            kind: Kind::Face,
            phase: Phase::Idle,
            colours,
            since: 0,
            shake: None,
            now: 0,
            tick: None,
        }));
        let ind = Rc::new(Indicator { root, area, glow, label, state });
        let st = ind.state.clone();
        ind.area.set_draw_func(move |_, cr, w, h| draw(&st.borrow(), cr, w as f64, h as f64));
        let st = ind.state.clone();
        ind.glow.set_draw_func(move |_, cr, w, h| {
            let s = st.borrow();
            if s.style == Style::Edge {
                draw_edge(&s, cr, w as f64, h as f64);
            }
        });
        ind.restyle();
        ind
    }

    pub fn widget(&self) -> &gtk::Box {
        &self.root
    }

    /// The screen-edge glow for [`Style::Edge`]; empty for the others.
    pub fn glow(&self) -> &gtk::DrawingArea {
        &self.glow
    }

    pub fn style(&self) -> Style {
        self.state.borrow().style
    }

    pub fn kind(&self) -> Kind {
        self.state.borrow().kind
    }

    pub fn phase(&self) -> Phase {
        self.state.borrow().phase
    }

    pub fn set_style(&self, style: Style) {
        let tick = {
            let mut s = self.state.borrow_mut();
            s.style = style;
            s.tick.take()
        };
        if let Some(id) = tick {
            id.remove();
        }
        self.restyle();
        self.animate();
    }

    pub fn set_colours(&self, colours: Colours) {
        self.state.borrow_mut().colours = colours;
        self.area.queue_draw();
        self.glow.queue_draw();
    }

    fn restyle(&self) {
        let s = self.state.borrow();
        let (w, h) = match s.style {
            Style::Circular => (140, 140),
            Style::Cyberpunk => (156, 156),
            Style::Minimal => (44, 44),
            Style::Edge => (0, 0),
        };
        self.area.set_content_width(w);
        self.area.set_content_height(h);
        self.area.set_visible(s.style != Style::Edge);
        if s.style == Style::Cyberpunk {
            self.label.add_css_class("monospace");
        } else {
            self.label.remove_css_class("monospace");
        }
        drop(s);
        self.refresh_label(None);
        self.area.queue_draw();
        self.glow.queue_draw();
    }

    fn refresh_label(&self, message: Option<&str>) {
        let s = self.state.borrow();
        // The scanner speaks its own terse way; the others say what PAM says.
        let text = message
            .filter(|m| !m.is_empty() && s.style != Style::Cyberpunk)
            .map(str::to_string)
            .unwrap_or_else(|| label(s.style, s.kind, s.phase));
        self.label.set_text(&text);
        self.label.set_visible(!text.is_empty());
        self.label.remove_css_class("error");
        self.label.remove_css_class("success");
        match s.phase {
            Phase::Failure => self.label.add_css_class("error"),
            Phase::Success => self.label.add_css_class("success"),
            _ => {}
        }
        self.root.update_property(&[gtk::accessible::Property::Label(&text)]);
    }

    /// Show `kind` in `phase`, with `message` instead of the usual words.
    pub fn set(&self, kind: Kind, phase: Phase, message: &str) {
        {
            let mut s = self.state.borrow_mut();
            let t = now(&self.area);
            if s.phase != phase || s.kind != kind {
                s.since = t;
            }
            if phase == Phase::Failure && s.phase != Phase::Failure {
                s.shake = Some(t);
            }
            s.kind = kind;
            s.phase = phase;
        }
        self.refresh_label(Some(message));
        self.animate();
    }

    fn moving(s: &State) -> bool {
        s.phase == Phase::Scanning || s.shake.is_some_and(|t| s.now - t < 300_000)
    }

    /// Keep drawing while something moves; stop when nothing does.
    fn animate(&self) {
        self.area.queue_draw();
        self.glow.queue_draw();
        if self.state.borrow().tick.is_some() {
            return;
        }
        let st = self.state.clone();
        let (area, glow) = (self.area.clone(), self.glow.clone());
        // Edge glow has no drawing on the card; a hidden widget's ticks never come.
        let host = if self.state.borrow().style == Style::Edge { &self.glow } else { &self.area };
        let id = host.add_tick_callback(move |_, clock| {
            let mut s = st.borrow_mut();
            s.now = clock.frame_time();
            area.queue_draw();
            glow.queue_draw();
            if Indicator::moving(&s) {
                glib::ControlFlow::Continue
            } else {
                s.tick = None;
                glib::ControlFlow::Break
            }
        });
        self.state.borrow_mut().tick = Some(id);
    }
}

fn tone(s: &State) -> gdk::RGBA {
    match s.phase {
        Phase::Success => s.colours.good,
        Phase::Failure => s.colours.bad,
        Phase::Timeout => s.colours.dim,
        _ => s.colours.accent,
    }
}

/// Seconds since the phase began.
fn elapsed(s: &State) -> f64 {
    (s.now - s.since).max(0) as f64 / 1e6
}

/// A failure shakes once: a horizontal offset over 0.3 s.
fn shake(s: &State) -> f64 {
    let Some(t) = s.shake else { return 0.0 };
    let x = (s.now - t) as f64 / 300_000.0;
    if !(0.0..1.0).contains(&x) { 0.0 } else { (x * PI * 4.0).sin() * 8.0 * (1.0 - x) }
}

/// 0.5..1, breathing, for pulses.
fn pulse(s: &State, period: f64) -> f64 {
    if s.phase != Phase::Scanning {
        return 1.0;
    }
    0.75 + 0.25 * (elapsed(s) * 2.0 * PI / period).cos()
}

fn draw(s: &State, cr: &cairo::Context, w: f64, h: f64) {
    cr.translate(shake(s), 0.0);
    let t = tone(s);
    match s.style {
        Style::Circular => {
            let (cx, cy, r) = (w / 2.0, h / 2.0, w.min(h) / 2.0 - 4.0);
            cr.arc(cx, cy, r, 0.0, 2.0 * PI);
            source(cr, &s.colours.surface, 1.0);
            let _ = cr.fill_preserve();
            cr.set_line_width(2.0);
            source(cr, &t, 0.25);
            let _ = cr.stroke();
            let sweep = match s.phase {
                Phase::Success => 1.0,
                Phase::Scanning => 0.28,
                _ => 0.0,
            };
            if sweep > 0.0 {
                let spin = if s.phase == Phase::Scanning { elapsed(s) / 1.3 * 2.0 * PI } else { 0.0 };
                let start = -PI / 2.0 + spin;
                cr.set_line_width(5.0);
                cr.set_line_cap(cairo::LineCap::Round);
                source(cr, &t, 0.9);
                cr.arc(cx, cy, r - 6.0, start, start + sweep * 2.0 * PI);
                let _ = cr.stroke();
            }
            glyphs::draw(cr, s.kind, s.phase, cx, cy, r * 0.42, &t, pulse(s, 1.3));
        }
        Style::Cyberpunk => {
            rounded(cr, 0.0, 0.0, w, h, 2.0);
            source(cr, &s.colours.surface, 0.92);
            let _ = cr.fill();
            source(cr, &t, 1.0);
            let (l, th) = (26.0, 3.0);
            for (x, y, dx, dy) in [(0.0, 0.0, 1.0, 1.0), (w, 0.0, -1.0, 1.0), (0.0, h, 1.0, -1.0), (w, h, -1.0, -1.0)] {
                cr.rectangle(x.min(x + dx * l), y.min(y + dy * th), l, th);
                cr.rectangle(x.min(x + dx * th), y.min(y + dy * l), th, l);
            }
            let _ = cr.fill();
            glyphs::draw(cr, s.kind, s.phase, w / 2.0, h / 2.0, w * 0.2, &t, 1.0);
            if s.phase == Phase::Scanning {
                // Down and up, easing at the ends.
                let p = (elapsed(s) / 1.1) % 2.0;
                let p = if p > 1.0 { 2.0 - p } else { p };
                let p = 0.5 - 0.5 * (p * PI).cos();
                let y = 12.0 + p * (h - 26.0);
                source(cr, &t, 0.8);
                cr.rectangle(10.0, y, w - 20.0, 2.0);
                let _ = cr.fill();
                let g = cairo::LinearGradient::new(0.0, y - 18.0, 0.0, y);
                g.add_color_stop_rgba(0.0, t.red() as f64, t.green() as f64, t.blue() as f64, 0.0);
                g.add_color_stop_rgba(1.0, t.red() as f64, t.green() as f64, t.blue() as f64, 0.18);
                let _ = cr.set_source(&g);
                cr.rectangle(10.0, y - 18.0, w - 20.0, 18.0);
                let _ = cr.fill();
            }
        }
        Style::Minimal => {
            rounded(cr, 1.0, 1.0, w - 2.0, h - 2.0, 11.0);
            source(cr, &s.colours.surface, 1.0);
            let _ = cr.fill_preserve();
            cr.set_line_width(1.0);
            source(cr, &t, 0.5);
            let _ = cr.stroke();
            glyphs::draw(cr, s.kind, s.phase, w / 2.0, h / 2.0, 9.0, &t, pulse(s, 1.4));
        }
        Style::Edge => {}
    }
}

/// Edge glow: stacked hairlines along the edges, fading inwards, breathing while it
/// scans; nothing at rest.
fn draw_edge(s: &State, cr: &cairo::Context, w: f64, h: f64) {
    if s.phase == Phase::Idle {
        return;
    }
    let t = tone(s);
    let level = if s.phase == Phase::Scanning { 0.725 + 0.275 * (elapsed(s) * 2.0 * PI / 1.8).cos() } else { 1.0 };
    cr.set_line_width(2.0);
    for i in 0..16 {
        let inset = 1.0 + i as f64 * 1.5;
        let a = (1.0 - i as f64 / 16.0).powf(2.2) * 0.6 * level;
        source(cr, &t, a);
        rounded(cr, inset, inset, w - 2.0 * inset, h - 2.0 * inset, 15.0);
        let _ = cr.stroke();
    }
}

/// Which way in a PAM message is about, for a flow that only has PAM's words (greetd's
/// one conversation): pam_fprintd's reader, a face module's camera, pam_u2f's key.
pub fn kind_of(message: &str) -> Option<Kind> {
    let m = message.to_lowercase();
    let any = |words: &[&str]| words.iter().any(|w| m.contains(w));
    if any(&["finger", "reader", "swipe"]) {
        Some(Kind::Fingerprint)
    } else if any(&["face", "camera", "looking for"]) {
        Some(Kind::Face)
    } else if any(&["security key", "fido", "u2f", "token", "touch your key", "insert your key"]) {
        Some(Kind::Key)
    } else {
        None
    }
}

/// A person's picture, or their initials on the accent.
pub fn avatar(picture: Option<&std::path::Path>, initials: &str, size: i32) -> gtk::Widget {
    if let Some(p) = picture
        && let Ok(t) = gdk::Texture::from_filename(p)
    {
        let pic = gtk::Picture::for_paintable(&t);
        pic.set_content_fit(gtk::ContentFit::Cover);
        pic.set_size_request(size, size);
        pic.add_css_class("avatar-image");
        pic.set_overflow(gtk::Overflow::Hidden);
        return pic.upcast();
    }
    let l = gtk::Label::new(Some(initials));
    l.add_css_class("initials");
    l.set_size_request(size, size);
    if size < 40 {
        l.add_css_class("small");
    }
    l.upcast()
}

/// Up to two capital letters from a full name (`Ken Smith` → `KS`), or the login's first.
pub fn initials(full_name: &str, login: &str) -> String {
    let from: String = full_name.split_whitespace().filter_map(|w| w.chars().next()).take(2).collect();
    let s = if from.is_empty() { login.chars().take(1).collect() } else { from };
    s.to_uppercase()
}

/// The picture for `user`: AccountsService's, else `~/.face`.
pub fn picture_of(user: &str, home: &std::path::Path) -> Option<std::path::PathBuf> {
    [std::path::PathBuf::from(format!("/var/lib/AccountsService/icons/{user}")), home.join(".face")]
        .into_iter()
        .find(|p| p.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn words_for_each_style() {
        assert_eq!(label(Style::Circular, Kind::Fingerprint, Phase::Scanning), "Touch the fingerprint reader");
        assert_eq!(label(Style::Cyberpunk, Kind::Face, Phase::Scanning), "SCAN // FACE");
        assert_eq!(label(Style::Cyberpunk, Kind::Key, Phase::Success), "MATCH // ACCESS GRANTED");
        assert_eq!(label(Style::Minimal, Kind::Face, Phase::Idle), "");
        for s in Style::ALL {
            assert_eq!(Style::parse(s.id()), Some(s));
        }
    }

    #[test]
    fn pam_messages_name_their_way_in() {
        assert_eq!(kind_of("Place your finger on the reader"), Some(Kind::Fingerprint));
        assert_eq!(kind_of("Swipe your right index finger across the fingerprint reader"), Some(Kind::Fingerprint));
        assert_eq!(kind_of("Looking for your face…"), Some(Kind::Face));
        assert_eq!(kind_of("Please touch the FIDO authenticator"), Some(Kind::Key));
        assert_eq!(kind_of("Password"), None);
        assert_eq!(kind_of("Your password will expire in 3 days"), None);
    }

    #[test]
    fn initials_from_names() {
        assert_eq!(initials("Ken Smith", "ken"), "KS");
        assert_eq!(initials("", "ana"), "A");
        assert_eq!(initials("Bob", "bob"), "B");
    }
}
