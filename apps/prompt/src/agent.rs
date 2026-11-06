//! The polkit side: register as this session's authentication agent and take polkit's
//! `BeginAuthentication` and `CancelAuthentication` calls, one request on screen at a time.

use crate::dialog;
use gtk::gio;
use gtk::glib;
use gtk::prelude::*;
use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};

const PATH: &str = "/io/github/design_nexus/nyx/Prompt";

const XML: &str = r#"<node>
  <interface name="org.freedesktop.PolicyKit1.AuthenticationAgent">
    <method name="BeginAuthentication">
      <arg type="s" name="action_id" direction="in"/>
      <arg type="s" name="message" direction="in"/>
      <arg type="s" name="icon_name" direction="in"/>
      <arg type="a{ss}" name="details" direction="in"/>
      <arg type="s" name="cookie" direction="in"/>
      <arg type="a(sa{sv})" name="identities" direction="in"/>
    </method>
    <method name="CancelAuthentication">
      <arg type="s" name="cookie" direction="in"/>
    </method>
  </interface>
</node>"#;

/// What polkit asks to authenticate.
pub struct Request {
    pub action: String,
    pub message: String,
    pub details: Vec<(String, String)>,
    pub cookie: String,
    /// Accounts that may answer, the caller's own first when it's one of them.
    pub users: Vec<String>,
    pub invocation: Option<gio::DBusMethodInvocation>,
}

thread_local! {
    static QUEUE: RefCell<VecDeque<Request>> = RefCell::default();
    static SHOWN: RefCell<Option<String>> = const { RefCell::new(None) };
}

fn call(
    bus: &gio::DBusConnection,
    dest: &str,
    path: &str,
    iface: &str,
    method: &str,
    args: glib::Variant,
) -> Result<glib::Variant, String> {
    bus.call_sync(Some(dest), path, iface, method, Some(&args), None, gio::DBusCallFlags::NONE, 5000, gio::Cancellable::NONE)
        .map_err(|e| e.message().to_string())
}

fn uid() -> u32 {
    std::fs::metadata("/proc/self").map(|m| std::os::unix::fs::MetadataExt::uid(&m)).unwrap_or(u32::MAX)
}

/// The session polkit should send this user's requests for: `XDG_SESSION_ID`, else the
/// user's graphical (display) session from logind.
fn session_id(bus: &gio::DBusConnection) -> Result<String, String> {
    if let Ok(id) = std::env::var("XDG_SESSION_ID")
        && !id.is_empty()
    {
        return Ok(id);
    }
    let login = "org.freedesktop.login1";
    let user = call(bus, login, "/org/freedesktop/login1", "org.freedesktop.login1.Manager", "GetUser", (uid(),).to_variant())?;
    let path = user.child_value(0).str().unwrap_or_default().to_string();
    let display = call(
        bus,
        login,
        &path,
        "org.freedesktop.DBus.Properties",
        "Get",
        ("org.freedesktop.login1.User", "Display").to_variant(),
    )?;
    let id = display.child_value(0).as_variant().and_then(|v| v.child_value(0).str().map(String::from)).unwrap_or_default();
    if id.is_empty() { Err("no graphical session to register for".into()) } else { Ok(id) }
}

/// Account names for polkit's identities: users as they are, groups as their members.
fn users_of(identities: &glib::Variant) -> Vec<String> {
    let passwd = std::fs::read_to_string("/etc/passwd").unwrap_or_default();
    let groups = std::fs::read_to_string("/etc/group").unwrap_or_default();
    let name_of = |uid: u32| {
        passwd.lines().find_map(|l| {
            let f: Vec<&str> = l.split(':').collect();
            (f.get(2)?.parse::<u32>().ok()? == uid).then(|| f[0].to_string())
        })
    };
    let mut out = Vec::new();
    for i in 0..identities.n_children() {
        let identity = identities.child_value(i);
        let kind = identity.child_value(0).str().unwrap_or_default().to_string();
        let props = glib::VariantDict::new(Some(&identity.child_value(1)));
        match kind.as_str() {
            "unix-user" => out.extend(props.lookup::<u32>("uid").ok().flatten().and_then(name_of)),
            "unix-group" => {
                if let Some(gid) = props.lookup::<u32>("gid").ok().flatten() {
                    for l in groups.lines() {
                        let f: Vec<&str> = l.split(':').collect();
                        if f.get(2).and_then(|g| g.parse::<u32>().ok()) == Some(gid) {
                            out.extend(f.get(3).unwrap_or(&"").split(',').filter(|m| !m.is_empty()).map(String::from));
                        }
                    }
                }
            }
            _ => {}
        }
    }
    out.dedup();
    let me = name_of(uid()).unwrap_or_default();
    if let Some(i) = out.iter().position(|u| *u == me) {
        let mine = out.remove(i);
        out.insert(0, mine);
    }
    out
}

pub fn register(app: &gtk::Application) -> Result<(), String> {
    let bus = gio::bus_get_sync(gio::BusType::System, gio::Cancellable::NONE).map_err(|e| e.message().to_string())?;
    let node = gio::DBusNodeInfo::for_xml(XML).map_err(|e| e.message().to_string())?;
    let iface = node.lookup_interface("org.freedesktop.PolicyKit1.AuthenticationAgent").ok_or("no interface")?;
    let a = app.clone();
    bus.register_object(PATH, &iface)
        .method_call(move |_, _, _, _, method, params, invocation| match method {
            "BeginAuthentication" => {
                let details = (0..params.child_value(3).n_children())
                    .filter_map(|i| {
                        let kv = params.child_value(3).child_value(i);
                        Some((kv.child_value(0).str()?.to_string(), kv.child_value(1).str()?.to_string()))
                    })
                    .collect();
                let req = Request {
                    action: params.child_value(0).str().unwrap_or_default().into(),
                    message: params.child_value(1).str().unwrap_or_default().into(),
                    details,
                    cookie: params.child_value(4).str().unwrap_or_default().into(),
                    users: users_of(&params.child_value(5)),
                    invocation: Some(invocation),
                };
                QUEUE.with(|q| q.borrow_mut().push_back(req));
                next(&a);
            }
            "CancelAuthentication" => {
                let cookie = params.child_value(0).str().unwrap_or_default().to_string();
                QUEUE.with(|q| {
                    let mut q = q.borrow_mut();
                    if let Some(i) = q.iter().position(|r| r.cookie == cookie) {
                        let mut r = q.remove(i).unwrap();
                        cancelled(&mut r);
                    }
                });
                if SHOWN.with(|s| s.borrow().as_deref() == Some(cookie.as_str())) {
                    dialog::cancel_shown();
                }
                invocation.return_value(None);
            }
            _ => invocation.return_dbus_error("org.freedesktop.DBus.Error.UnknownMethod", method),
        })
        .build()
        .map_err(|e| e.message().to_string())?;
    let session = session_id(&bus)?;
    let mut details: HashMap<String, glib::Variant> = HashMap::new();
    details.insert("session-id".into(), session.to_variant());
    let subject = ("unix-session", details);
    let locale = std::env::var("LANG").unwrap_or_else(|_| "en_US.UTF-8".into());
    let args = (subject, locale, PATH).to_variant();
    call(
        &bus,
        "org.freedesktop.PolicyKit1",
        "/org/freedesktop/PolicyKit1/Authority",
        "org.freedesktop.PolicyKit1.Authority",
        "RegisterAuthenticationAgent",
        args,
    )?;
    std::mem::forget(bus);
    Ok(())
}

fn cancelled(r: &mut Request) {
    if let Some(inv) = r.invocation.take() {
        inv.return_dbus_error("org.freedesktop.PolicyKit1.Error.Cancelled", "The person cancelled");
    }
}

/// Show the next request when nothing is on screen.
fn next(app: &gtk::Application) {
    if SHOWN.with(|s| s.borrow().is_some()) {
        return;
    }
    let Some(req) = QUEUE.with(|q| q.borrow_mut().pop_front()) else { return };
    SHOWN.with(|s| *s.borrow_mut() = Some(req.cookie.clone()));
    let a = app.clone();
    dialog::show(app, req, move |mut req, ok| {
        match (ok, req.invocation.take()) {
            (true, Some(inv)) => inv.return_value(None),
            (false, Some(inv)) => inv.return_dbus_error("org.freedesktop.PolicyKit1.Error.Cancelled", "Not authorized"),
            _ => {}
        }
        SHOWN.with(|s| *s.borrow_mut() = None);
        next(&a);
    });
}
