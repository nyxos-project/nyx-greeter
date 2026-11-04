//! What a user can log in to: Wayland session `.desktop` files.

use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq)]
pub struct Session {
    /// The file name (`hyprland-uwsm.desktop`): what's remembered.
    pub id: String,
    pub name: String,
    pub cmd: Vec<String>,
    pub desktop_names: Option<String>,
}

impl Session {
    /// The environment greetd starts it with.
    pub fn env(&self) -> Vec<String> {
        let mut env = vec!["XDG_SESSION_TYPE=wayland".to_string()];
        if let Some(d) = &self.desktop_names {
            env.push(format!("XDG_CURRENT_DESKTOP={d}"));
            env.push(format!(
                "XDG_SESSION_DESKTOP={}",
                d.split(':').next().unwrap_or(d)
            ));
        }
        env
    }
}

/// Split an `Exec=` line: quotes, backslash escapes, and field codes (`%U`) dropped.
pub fn split_exec(exec: &str) -> Vec<String> {
    let mut out = vec![];
    let mut cur = String::new();
    let mut quoted = false;
    let mut any = false;
    let mut chars = exec.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                quoted = !quoted;
                any = true;
            }
            '\\' if quoted => {
                if let Some(n) = chars.next() {
                    cur.push(n);
                }
            }
            ' ' | '\t' if !quoted => {
                if any || !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                    any = false;
                }
            }
            _ => cur.push(c),
        }
    }
    if any || !cur.is_empty() {
        out.push(cur);
    }
    out.into_iter()
        .filter(|a| !(a.len() == 2 && a.starts_with('%')))
        .collect()
}

pub fn parse(id: &str, text: &str) -> Option<Session> {
    let mut in_entry = false;
    let (mut name, mut exec, mut names, mut hidden) = (None, None, None, false);
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_entry = line == "[Desktop Entry]";
            continue;
        }
        if !in_entry {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        match k.trim() {
            "Name" => name = Some(v.trim().to_string()),
            "Exec" => exec = Some(v.trim().to_string()),
            "DesktopNames" => names = Some(v.trim().trim_end_matches(';').replace(';', ":")),
            "Hidden" | "NoDisplay" if v.trim() == "true" => hidden = true,
            _ => {}
        }
    }
    let cmd = split_exec(&exec?);
    if hidden || cmd.is_empty() {
        return None;
    }
    Some(Session {
        id: id.to_string(),
        name: name.unwrap_or_else(|| id.trim_end_matches(".desktop").to_string()),
        cmd,
        desktop_names: names,
    })
}

pub fn dirs(extra: &[PathBuf]) -> Vec<PathBuf> {
    let mut d: Vec<PathBuf> = extra.to_vec();
    let data =
        std::env::var("XDG_DATA_DIRS").unwrap_or_else(|_| "/usr/local/share:/usr/share".into());
    for p in data.split(':').filter(|p| !p.is_empty()) {
        d.push(Path::new(p).join("wayland-sessions"));
    }
    d.push("/run/current-system/sw/share/wayland-sessions".into());
    d.push("/usr/share/wayland-sessions".into());
    d.dedup();
    d
}

/// Every session, the first folder winning for a file name.
pub fn all(extra: &[PathBuf]) -> Vec<Session> {
    let mut out: Vec<Session> = vec![];
    for dir in dirs(extra) {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut entries: Vec<_> = entries.flatten().collect();
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            let id = e.file_name().to_string_lossy().into_owned();
            if !id.ends_with(".desktop") || out.iter().any(|s| s.id == id) {
                continue;
            }
            if let Some(s) = std::fs::read_to_string(e.path())
                .ok()
                .and_then(|t| parse(&id, &t))
            {
                out.push(s);
            }
        }
    }
    out
}

/// The session to start for a user: theirs from last time, else the configured
/// default, else one through uwsm, else the first.
pub fn pick<'a>(
    all: &'a [Session],
    remembered: Option<&str>,
    default: Option<&str>,
) -> Option<&'a Session> {
    remembered
        .and_then(|r| all.iter().find(|s| s.id == r))
        .or_else(|| default.and_then(|d| all.iter().find(|s| s.id == d)))
        .or_else(|| all.iter().find(|s| s.id.contains("uwsm")))
        .or_else(|| all.first())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exec_lines() {
        assert_eq!(
            split_exec("uwsm start -- hyprland.desktop"),
            vec!["uwsm", "start", "--", "hyprland.desktop"]
        );
        assert_eq!(
            split_exec(r#"sh -c "echo \"hi there\"" %U"#),
            vec!["sh", "-c", "echo \"hi there\""]
        );
        assert_eq!(split_exec(r#"a "" b"#), vec!["a", "", "b"]);
    }

    #[test]
    fn desktop_files_and_picking() {
        let h = parse(
            "hyprland.desktop",
            "[Desktop Entry]\nName=Hyprland\nExec=Hyprland\nDesktopNames=Hyprland\n",
        )
        .unwrap();
        let u = parse("hyprland-uwsm.desktop", "[Desktop Entry]\nName=Hyprland (uwsm)\nExec=uwsm start -- hyprland.desktop\nDesktopNames=Hyprland;\n").unwrap();
        assert!(
            parse(
                "x.desktop",
                "[Desktop Entry]\nName=X\nExec=x\nHidden=true\n"
            )
            .is_none()
        );
        assert_eq!(
            u.env(),
            vec![
                "XDG_SESSION_TYPE=wayland",
                "XDG_CURRENT_DESKTOP=Hyprland",
                "XDG_SESSION_DESKTOP=Hyprland"
            ]
        );
        let all = vec![h.clone(), u.clone()];
        assert_eq!(pick(&all, None, None).unwrap().id, "hyprland-uwsm.desktop");
        assert_eq!(
            pick(&all, Some("hyprland.desktop"), None).unwrap().id,
            "hyprland.desktop"
        );
        assert_eq!(
            pick(&all, Some("gone.desktop"), Some("hyprland.desktop"))
                .unwrap()
                .id,
            "hyprland.desktop"
        );
    }
}
