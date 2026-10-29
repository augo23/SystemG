//! `systemctl preset`: systemd.preset(5) policy files.
//!
//! `*.preset` files in the preset directories hold `enable PATTERN` and
//! `disable PATTERN` lines. Files are read in filename order across all
//! directories (one in an earlier directory hides a same-named file in a later
//! one); the first line whose pattern matches the unit decides. With no match,
//! the unit is enabled, as systemd does.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The preset directories, highest priority first.
pub fn search_path(user: bool) -> Vec<PathBuf> {
    if let Ok(p) = std::env::var("SYSTEMG_PRESET_PATH") {
        return p.split(':').map(PathBuf::from).collect();
    }
    let kind = if user { "user-preset" } else { "system-preset" };
    ["/etc/systemd", "/run/systemd", "/usr/local/lib/systemd", "/usr/lib/systemd"]
        .iter()
        .map(|d| Path::new(d).join(kind))
        .collect()
}

/// The rules of all preset files, in the order they apply.
pub fn load(dirs: &[PathBuf]) -> Vec<(bool, String)> {
    let mut files: BTreeMap<String, PathBuf> = BTreeMap::new();
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(dir) else { continue };
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.ends_with(".preset") {
                files.entry(name).or_insert_with(|| e.path());
            }
        }
    }
    let mut rules = Vec::new();
    for path in files.values() {
        let Ok(text) = std::fs::read_to_string(path) else { continue };
        for line in text.lines() {
            let line = line.trim();
            let mut words = line.split_whitespace();
            let on = match words.next() {
                Some("enable") => true,
                Some("disable") => false,
                _ => continue, // comments, blank lines, ignore (templates' instance lists)
            };
            if let Some(pattern) = words.next() {
                rules.push((on, pattern.to_string()));
            }
        }
    }
    rules
}

/// Whether presets enable `unit`.
pub fn wants_enabled(rules: &[(bool, String)], unit: &str) -> bool {
    rules
        .iter()
        .find(|(_, p)| glob(p.as_bytes(), unit.as_bytes()))
        .map_or(true, |(on, _)| *on)
}

/// fnmatch(3) without flags: `*`, `?` and `[...]` (with `!`/`^` and ranges).
fn glob(p: &[u8], s: &[u8]) -> bool {
    match p.first() {
        None => s.is_empty(),
        Some(b'*') => (0..=s.len()).any(|i| glob(&p[1..], &s[i..])),
        Some(b'?') => !s.is_empty() && glob(&p[1..], &s[1..]),
        Some(b'[') => {
            let Some(c) = s.first() else { return false };
            let mut i = 1;
            let negate = matches!(p.get(1), Some(b'!' | b'^'));
            if negate {
                i += 1;
            }
            let mut hit = false;
            let start = i;
            while i < p.len() && (p[i] != b']' || i == start) {
                if i + 2 < p.len() && p[i + 1] == b'-' && p[i + 2] != b']' {
                    hit |= (p[i]..=p[i + 2]).contains(c);
                    i += 3;
                } else {
                    hit |= p[i] == *c;
                    i += 1;
                }
            }
            if i >= p.len() {
                return p[0] == *c && glob(&p[1..], &s[1..]); // no closing ]: literal [
            }
            hit != negate && glob(&p[i + 1..], &s[1..])
        }
        Some(&c) => s.first() == Some(&c) && glob(&p[1..], &s[1..]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn globs() {
        assert!(glob(b"*", b"ssh.service"));
        assert!(glob(b"getty@*.service", b"getty@tty1.service"));
        assert!(glob(b"ss?.service", b"ssh.service"));
        assert!(glob(b"[a-c]ups.service", b"cups.service"));
        assert!(!glob(b"[!c]ups.service", b"cups.service"));
        assert!(!glob(b"*.socket", b"ssh.service"));
    }

    #[test]
    fn first_match_wins_and_default_is_enable() {
        let rules = vec![
            (false, "foo.service".to_string()),
            (true, "foo.*".to_string()),
            (false, "*.socket".to_string()),
        ];
        assert!(!wants_enabled(&rules, "foo.service"));
        assert!(wants_enabled(&rules, "foo.timer"));
        assert!(!wants_enabled(&rules, "bar.socket"));
        assert!(wants_enabled(&rules, "bar.service"));
        assert!(wants_enabled(&[], "anything.service"));
    }

    #[test]
    fn files_sort_by_name_and_earlier_dirs_override() {
        let base = std::env::temp_dir().join(format!("sg-preset-{}", std::process::id()));
        let (etc, lib) = (base.join("etc"), base.join("lib"));
        std::fs::create_dir_all(&etc).unwrap();
        std::fs::create_dir_all(&lib).unwrap();
        std::fs::write(lib.join("90-default.preset"), "disable *\n").unwrap();
        std::fs::write(lib.join("50-vendor.preset"), "enable ssh.service\n").unwrap();
        // /etc's copy of 50-vendor.preset replaces /usr/lib's.
        std::fs::write(etc.join("50-vendor.preset"), "# local\nenable cups.service\n").unwrap();
        let rules = load(&[etc, lib]);
        assert!(wants_enabled(&rules, "cups.service"));
        assert!(!wants_enabled(&rules, "ssh.service"));
        std::fs::remove_dir_all(base).unwrap();
    }
}
