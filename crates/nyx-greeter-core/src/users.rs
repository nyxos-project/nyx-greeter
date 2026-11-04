//! People who can log in: accounts with a uid from 1000 to 59999 and a real shell,
//! from `/etc/passwd` (`$NYX_GREETER_PASSWD` for tests).

use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq)]
pub struct User {
    pub name: String,
    pub display: String,
    pub uid: u32,
    pub home: PathBuf,
    pub avatar: Option<PathBuf>,
}

impl User {
    /// "Ken Smith" → "KS", "ken" → "K".
    pub fn initials(&self) -> String {
        let words: Vec<&str> = self.display.split_whitespace().collect();
        let pick = |w: &str| {
            w.chars()
                .next()
                .map(|c| c.to_uppercase().to_string())
                .unwrap_or_default()
        };
        match words.as_slice() {
            [] => pick(&self.name),
            [one] => pick(one),
            [first, .., last] => pick(first) + &pick(last),
        }
    }
}

fn avatar(name: &str, home: &Path) -> Option<PathBuf> {
    let icons = std::env::var_os("NYX_GREETER_ICONS")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/var/lib/AccountsService/icons"));
    [icons.join(name), home.join(".face")]
        .into_iter()
        .find(|p| std::fs::File::open(p).is_ok())
}

pub fn parse(passwd: &str) -> Vec<User> {
    let mut out: Vec<User> = passwd
        .lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split(':').collect();
            if f.len() < 7 {
                return None;
            }
            let uid: u32 = f[2].parse().ok()?;
            let shell = f[6];
            if !(1000..60000).contains(&uid)
                || shell.ends_with("nologin")
                || shell.ends_with("/false")
                || shell.is_empty()
            {
                return None;
            }
            let gecos = f[4].split(',').next().unwrap_or("").trim();
            let home = PathBuf::from(f[5]);
            Some(User {
                name: f[0].to_string(),
                display: if gecos.is_empty() {
                    f[0].to_string()
                } else {
                    gecos.to_string()
                },
                uid,
                avatar: avatar(f[0], &home),
                home,
            })
        })
        .collect();
    out.sort_by_key(|u| u.uid);
    out
}

pub fn all() -> Vec<User> {
    let path = std::env::var_os("NYX_GREETER_PASSWD")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/etc/passwd"));
    parse(&std::fs::read_to_string(path).unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn real_people_only() {
        let p = "root:x:0:0::/root:/bin/bash\nken:x:1000:1000:Ken Smith,,,:/home/ken:/usr/bin/zsh\n\
                 greeter:x:973:973::/var/lib/greeter:/bin/sh\nnobody:x:65534:65534::/:/usr/bin/nologin\n\
                 amy:x:1001:1001::/home/amy:/bin/bash\nsvc:x:1002:1002::/srv:/usr/sbin/nologin\n";
        let u = parse(p);
        assert_eq!(
            u.iter().map(|u| u.name.as_str()).collect::<Vec<_>>(),
            vec!["ken", "amy"]
        );
        assert_eq!(u[0].display, "Ken Smith");
        assert_eq!(u[0].initials(), "KS");
        assert_eq!(u[1].initials(), "A");
    }
}
