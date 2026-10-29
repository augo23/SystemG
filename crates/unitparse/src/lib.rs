//! Parser for systemd unit files.
//!
//! Implements the subset of `systemd.syntax(7)` that real-world unit files use:
//! sections, `Key=Value` assignments, comments (`#` and `;`), line continuations
//! with a trailing backslash, repeated keys, the empty-assignment reset rule
//! (`Key=` clears earlier values), and drop-in overrides (`foo.service.d/*.conf`).

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io;
use std::path::PathBuf;

/// The kind of unit, taken from the file name suffix.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum UnitKind {
    Service,
    Socket,
    Target,
    Timer,
    Mount,
    Path,
    Other,
}

impl UnitKind {
    pub fn from_name(name: &str) -> Self {
        match name.rsplit_once('.').map(|(_, ext)| ext) {
            Some("service") => Self::Service,
            Some("socket") => Self::Socket,
            Some("target") => Self::Target,
            Some("timer") => Self::Timer,
            Some("mount") => Self::Mount,
            Some("path") => Self::Path,
            _ => Self::Other,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub line: usize,
    pub message: String,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "line {}: {}", self.line, self.message)
    }
}

impl std::error::Error for ParseError {}

#[derive(Debug)]
pub enum LoadError {
    Io(PathBuf, io::Error),
    Parse(PathBuf, ParseError),
}

impl fmt::Display for LoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(p, e) => write!(f, "{}: {e}", p.display()),
            Self::Parse(p, e) => write!(f, "{}: {e}", p.display()),
        }
    }
}

impl std::error::Error for LoadError {}

/// A parsed unit: section name -> key -> ordered values.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Unit {
    pub name: String,
    sections: BTreeMap<String, BTreeMap<String, Vec<String>>>,
}

impl Unit {
    pub fn kind(&self) -> UnitKind {
        UnitKind::from_name(&self.name)
    }

    /// Parse unit text. `name` is the unit's file name, e.g. `nginx.service`.
    pub fn parse(name: &str, text: &str) -> Result<Self, ParseError> {
        let mut unit = Unit {
            name: name.to_string(),
            ..Default::default()
        };
        unit.merge_text(text)?;
        Ok(unit)
    }

    /// Apply another file's contents on top of this unit (drop-in semantics).
    pub fn merge_text(&mut self, text: &str) -> Result<(), ParseError> {
        let mut section: Option<String> = None;
        let mut pending = String::new();
        let mut pending_line = 0;

        for (idx, raw) in text.lines().enumerate() {
            let lineno = idx + 1;
            let line = raw.trim();

            if pending.is_empty() {
                if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
                    continue;
                }
                pending_line = lineno;
            } else if line.starts_with('#') || line.starts_with(';') {
                // Comments inside a continuation are skipped.
                continue;
            }

            if let Some(stripped) = line.strip_suffix('\\') {
                pending.push_str(stripped);
                pending.push(' ');
                continue;
            }
            pending.push_str(line);
            let logical = std::mem::take(&mut pending);
            self.apply_line(&logical, pending_line, &mut section)?;
        }

        if !pending.is_empty() {
            self.apply_line(pending.trim_end(), pending_line, &mut section)?;
        }
        Ok(())
    }

    fn apply_line(
        &mut self,
        line: &str,
        lineno: usize,
        section: &mut Option<String>,
    ) -> Result<(), ParseError> {
        if let Some(rest) = line.strip_prefix('[') {
            let name = rest.strip_suffix(']').ok_or_else(|| ParseError {
                line: lineno,
                message: format!("unterminated section header: {line}"),
            })?;
            *section = Some(name.trim().to_string());
            self.sections.entry(name.trim().to_string()).or_default();
            return Ok(());
        }

        let Some(sec) = section.as_ref() else {
            return Err(ParseError {
                line: lineno,
                message: "assignment outside of a section".into(),
            });
        };
        let Some((key, value)) = line.split_once('=') else {
            return Err(ParseError {
                line: lineno,
                message: format!("expected Key=Value, got: {line}"),
            });
        };
        let key = key.trim();
        if key.is_empty() {
            return Err(ParseError {
                line: lineno,
                message: "empty key".into(),
            });
        }
        let value = value.trim();
        let values = self
            .sections
            .entry(sec.clone())
            .or_default()
            .entry(key.to_string())
            .or_default();
        if value.is_empty() {
            values.clear(); // `Key=` resets the list
        } else {
            values.push(value.to_string());
        }
        Ok(())
    }

    pub fn has_section(&self, section: &str) -> bool {
        self.sections.contains_key(section)
    }

    /// All values for a key, in order.
    pub fn values(&self, section: &str, key: &str) -> &[String] {
        self.sections
            .get(section)
            .and_then(|s| s.get(key))
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    /// The last value for a key (later assignments win for single-valued keys).
    pub fn get(&self, section: &str, key: &str) -> Option<&str> {
        self.values(section, key).last().map(String::as_str)
    }

    /// Values of a list key (e.g. `After=a b` + `After=c`), split on whitespace.
    pub fn list(&self, section: &str, key: &str) -> Vec<String> {
        self.values(section, key)
            .iter()
            .flat_map(|v| v.split_whitespace().map(str::to_string))
            .collect()
    }

    pub fn get_bool(&self, section: &str, key: &str) -> Option<bool> {
        self.get(section, key).and_then(parse_bool)
    }

    /// Every (key, value) in a section, for reporting unsupported options.
    pub fn entries(&self, section: &str) -> impl Iterator<Item = (&str, &str)> {
        self.sections.get(section).into_iter().flat_map(|s| {
            s.iter()
                .flat_map(|(k, vs)| vs.iter().map(move |v| (k.as_str(), v.as_str())))
        })
    }

    pub fn sections(&self) -> impl Iterator<Item = &str> {
        self.sections.keys().map(String::as_str)
    }

    /// Load a unit from the first directory that has it, then apply drop-ins
    /// (`<name>.d/*.conf`) from every directory, sorted by file name.
    pub fn load(name: &str, search_path: &[PathBuf]) -> Result<Option<Self>, LoadError> {
        Self::load_with(name, search_path, &Specifiers::system())
    }

    /// Like [`Unit::load`], expanding `%` specifiers (`%t`, `%h`, `%i`, ...)
    /// for the given manager context, as systemd does when it loads a unit.
    pub fn load_with(
        name: &str,
        search_path: &[PathBuf],
        spec: &Specifiers,
    ) -> Result<Option<Self>, LoadError> {
        let Some(main) = search_path
            .iter()
            .map(|d| d.join(name))
            .find(|p| p.is_file())
        else {
            return Ok(None);
        };
        let text = fs::read_to_string(&main).map_err(|e| LoadError::Io(main.clone(), e))?;
        let text = spec.expand(name, &text);
        let mut unit = Unit::parse(name, &text).map_err(|e| LoadError::Parse(main.clone(), e))?;

        // Later files with the same name override earlier ones; earlier dirs have priority.
        let mut dropins: BTreeMap<String, PathBuf> = BTreeMap::new();
        for dir in search_path.iter().rev() {
            let d = dir.join(format!("{name}.d"));
            let Ok(entries) = fs::read_dir(&d) else {
                continue;
            };
            for e in entries.flatten() {
                let p = e.path();
                if p.extension().is_some_and(|x| x == "conf") {
                    dropins.insert(e.file_name().to_string_lossy().into_owned(), p);
                }
            }
        }
        for path in dropins.values() {
            let text = fs::read_to_string(path).map_err(|e| LoadError::Io(path.clone(), e))?;
            let text = spec.expand(name, &text);
            unit.merge_text(&text)
                .map_err(|e| LoadError::Parse(path.clone(), e))?;
        }
        Ok(Some(unit))
    }
}

/// Values for systemd's `%` specifiers (systemd.unit(5), "Specifiers").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Specifiers {
    /// `%u`, `%U`, `%g`, `%G`
    pub user: String,
    pub uid: u32,
    pub group: String,
    pub gid: u32,
    /// `%h`
    pub home: String,
    /// `%t`: /run for the system manager, $XDG_RUNTIME_DIR for a user manager.
    pub runtime_dir: String,
    /// `%S`, `%C`, `%L`, `%E`
    pub state_dir: String,
    pub cache_dir: String,
    pub logs_dir: String,
    pub config_dir: String,
    /// `%H`
    pub hostname: String,
}

impl Specifiers {
    /// The system manager (PID 1).
    pub fn system() -> Self {
        Specifiers {
            user: "root".into(),
            uid: 0,
            group: "root".into(),
            gid: 0,
            home: "/root".into(),
            runtime_dir: "/run".into(),
            state_dir: "/var/lib".into(),
            cache_dir: "/var/cache".into(),
            logs_dir: "/var/log".into(),
            config_dir: "/etc".into(),
            hostname: hostname(),
        }
    }

    /// A per-user manager (`systemd --user`).
    pub fn user(
        user: &str,
        uid: u32,
        group: &str,
        gid: u32,
        home: &str,
        runtime_dir: &str,
    ) -> Self {
        Specifiers {
            user: user.into(),
            uid,
            group: group.into(),
            gid,
            home: home.into(),
            runtime_dir: runtime_dir.into(),
            state_dir: format!("{home}/.local/state"),
            cache_dir: format!("{home}/.cache"),
            logs_dir: format!("{home}/.local/state/log"),
            config_dir: format!("{home}/.config"),
            hostname: hostname(),
        }
    }

    /// Expand every specifier in `text` for the unit `unit_name`.
    /// Unknown specifiers are left untouched.
    pub fn expand(&self, unit_name: &str, text: &str) -> String {
        if !text.contains('%') {
            return text.to_string();
        }
        let prefix_part = base_name(unit_name);
        let (prefix, instance) = match prefix_part.split_once('@') {
            Some((p, i)) => (p, i),
            None => (prefix_part, ""),
        };
        let mut out = String::with_capacity(text.len());
        let mut chars = text.chars().peekable();
        while let Some(c) = chars.next() {
            if c != '%' {
                out.push(c);
                continue;
            }
            let Some(&n) = chars.peek() else {
                out.push('%');
                break;
            };
            let rep: Option<String> = match n {
                '%' => Some("%".into()),
                'n' => Some(unit_name.into()),
                'N' => Some(prefix_part.into()),
                'p' | 'P' => Some(prefix.into()),
                'i' | 'I' => Some(instance.into()),
                'j' | 'J' => Some(prefix.rsplit('-').next().unwrap_or(prefix).into()),
                'u' => Some(self.user.clone()),
                'U' => Some(self.uid.to_string()),
                'g' => Some(self.group.clone()),
                'G' => Some(self.gid.to_string()),
                'h' => Some(self.home.clone()),
                's' => Some("/bin/sh".into()),
                't' => Some(self.runtime_dir.clone()),
                'S' => Some(self.state_dir.clone()),
                'C' => Some(self.cache_dir.clone()),
                'L' => Some(self.logs_dir.clone()),
                'E' => Some(self.config_dir.clone()),
                'T' => Some("/tmp".into()),
                'V' => Some("/var/tmp".into()),
                'H' | 'l' => Some(self.hostname.clone()),
                _ => None,
            };
            match rep {
                Some(r) => {
                    chars.next();
                    out.push_str(&r);
                }
                None => out.push('%'),
            }
        }
        out
    }
}

fn hostname() -> String {
    fs::read_to_string("/proc/sys/kernel/hostname")
        .or_else(|_| fs::read_to_string("/etc/hostname"))
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "localhost".into())
}

pub fn parse_bool(s: &str) -> Option<bool> {
    match s.to_ascii_lowercase().as_str() {
        "1" | "yes" | "y" | "true" | "t" | "on" => Some(true),
        "0" | "no" | "n" | "false" | "f" | "off" => Some(false),
        _ => None,
    }
}

/// Parse a systemd time span (`90`, `5s`, `1min 30s`, `2h`, `infinity`) into seconds.
/// Returns `Some(None)` for infinity, `None` if unparseable.
pub fn parse_timespan_secs(s: &str) -> Option<Option<f64>> {
    let s = s.trim();
    if s.eq_ignore_ascii_case("infinity") {
        return Some(None);
    }
    let mut total = 0.0;
    let mut rest = s;
    let mut any = false;
    while !rest.trim_start().is_empty() {
        rest = rest.trim_start();
        let num_end = rest
            .find(|c: char| !(c.is_ascii_digit() || c == '.'))
            .unwrap_or(rest.len());
        if num_end == 0 {
            return None;
        }
        let n: f64 = rest[..num_end].parse().ok()?;
        rest = rest[num_end..].trim_start();
        let unit_end = rest
            .find(|c: char| !c.is_ascii_alphabetic())
            .unwrap_or(rest.len());
        let mult = match &rest[..unit_end] {
            "" | "s" | "sec" | "second" | "seconds" => 1.0,
            "us" | "usec" => 1e-6,
            "ms" | "msec" => 1e-3,
            "m" | "min" | "minute" | "minutes" => 60.0,
            "h" | "hr" | "hour" | "hours" => 3600.0,
            "d" | "day" | "days" => 86400.0,
            "w" | "week" | "weeks" => 604800.0,
            _ => return None,
        };
        total += n * mult;
        rest = &rest[unit_end..];
        any = true;
    }
    any.then_some(Some(total))
}

/// The directories systemd searches for system units, highest priority first.
pub fn system_search_path() -> Vec<PathBuf> {
    [
        "/etc/systemd/system",
        "/run/systemd/system",
        "/usr/local/lib/systemd/system",
        "/usr/lib/systemd/system",
        "/lib/systemd/system",
    ]
    .iter()
    .map(PathBuf::from)
    .collect()
}

/// Split an `ExecStart=` value into its prefix flags and argv, honoring quotes.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ExecLine {
    /// `-`: a non-zero exit is ignored.
    pub ignore_failure: bool,
    /// `@`: argv[0] is given separately.
    pub custom_argv0: bool,
    /// `+`, `!`, `!!`: run with full privileges.
    pub privileged: bool,
    pub argv: Vec<String>,
}

impl ExecLine {
    pub fn parse(s: &str) -> Option<Self> {
        let mut exec = ExecLine::default();
        let mut rest = s.trim_start();
        loop {
            match rest.chars().next() {
                Some('-') => exec.ignore_failure = true,
                Some('@') => exec.custom_argv0 = true,
                Some('+') | Some('!') => exec.privileged = true,
                Some(':') | Some('|') => {}
                _ => break,
            }
            rest = &rest[1..];
        }
        exec.argv = split_quoted(rest)?;
        if exec.custom_argv0 && exec.argv.len() >= 2 {
            exec.argv.remove(1); // drop the custom argv[0]; dinit cannot express it
        }
        (!exec.argv.is_empty()).then_some(exec)
    }

    pub fn program(&self) -> &str {
        &self.argv[0]
    }
}

/// Split a command line on whitespace with single/double quotes and backslash escapes.
pub fn split_quoted(s: &str) -> Option<Vec<String>> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_word = false;
    let mut quote: Option<char> = None;
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), '\\') => cur.push(chars.next()?),
            (Some(_), c) => cur.push(c),
            (None, '"' | '\'') => {
                quote = Some(c);
                in_word = true;
            }
            (None, '\\') => {
                cur.push(chars.next()?);
                in_word = true;
            }
            (None, c) if c.is_whitespace() => {
                if in_word {
                    out.push(std::mem::take(&mut cur));
                    in_word = false;
                }
            }
            (None, c) => {
                cur.push(c);
                in_word = true;
            }
        }
    }
    if quote.is_some() {
        return None;
    }
    if in_word {
        out.push(cur);
    }
    Some(out)
}

/// Strip the unit type suffix: `nginx.service` -> `nginx`.
pub fn base_name(unit: &str) -> &str {
    unit.rsplit_once('.').map(|(b, _)| b).unwrap_or(unit)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NGINX: &str = r#"
[Unit]
Description=A high performance web server
After=network-online.target remote-fs.target
Wants=network-online.target

[Service]
Type=forking
PIDFile=/run/nginx.pid
ExecStartPre=/usr/sbin/nginx -t -q -g 'daemon on; master_process on;'
ExecStart=/usr/sbin/nginx -g 'daemon on; master_process on;'
ExecReload=/usr/sbin/nginx -g 'daemon on; master_process on;' -s reload
TimeoutStopSec=5
KillMode=mixed

[Install]
WantedBy=multi-user.target
"#;

    #[test]
    fn parses_nginx() {
        let u = Unit::parse("nginx.service", NGINX).unwrap();
        assert_eq!(u.kind(), UnitKind::Service);
        assert_eq!(u.get("Service", "Type"), Some("forking"));
        assert_eq!(
            u.list("Unit", "After"),
            vec!["network-online.target", "remote-fs.target"]
        );
        let exec = ExecLine::parse(u.get("Service", "ExecStart").unwrap()).unwrap();
        assert_eq!(
            exec.argv,
            vec!["/usr/sbin/nginx", "-g", "daemon on; master_process on;"]
        );
    }

    #[test]
    fn continuation_and_comments() {
        let u = Unit::parse(
            "a.service",
            "# top\n[Service]\nExecStart=/bin/echo \\\n  hello \\\n# skipped\n  world\n",
        )
        .unwrap();
        let exec = ExecLine::parse(u.get("Service", "ExecStart").unwrap()).unwrap();
        assert_eq!(exec.argv, vec!["/bin/echo", "hello", "world"]);
    }

    #[test]
    fn empty_assignment_resets() {
        let mut u = Unit::parse("a.service", "[Service]\nExecStart=/bin/a\n").unwrap();
        u.merge_text("[Service]\nExecStart=\nExecStart=/bin/b\n")
            .unwrap();
        assert_eq!(u.values("Service", "ExecStart"), &["/bin/b".to_string()]);
    }

    #[test]
    fn exec_prefixes() {
        let e = ExecLine::parse("-+/bin/true --x").unwrap();
        assert!(e.ignore_failure && e.privileged);
        assert_eq!(e.argv, vec!["/bin/true", "--x"]);
        let e = ExecLine::parse("@/bin/sh myname -c 'echo hi'").unwrap();
        assert_eq!(e.argv, vec!["/bin/sh", "-c", "echo hi"]);
    }

    #[test]
    fn errors() {
        assert!(Unit::parse("a.service", "Foo=bar\n").is_err());
        assert!(Unit::parse("a.service", "[Unit\n").is_err());
        assert!(Unit::parse("a.service", "[Unit]\nnoequals\n").is_err());
    }

    #[test]
    fn timespans() {
        assert_eq!(parse_timespan_secs("90"), Some(Some(90.0)));
        assert_eq!(parse_timespan_secs("1min 30s"), Some(Some(90.0)));
        assert_eq!(parse_timespan_secs("500ms"), Some(Some(0.5)));
        assert_eq!(parse_timespan_secs("infinity"), Some(None));
        assert_eq!(parse_timespan_secs("soon"), None);
    }

    #[test]
    fn dropins() {
        let dir = std::env::temp_dir().join(format!("unitparse-test-{}", std::process::id()));
        let etc = dir.join("etc");
        let lib = dir.join("lib");
        fs::create_dir_all(etc.join("x.service.d")).unwrap();
        fs::create_dir_all(&lib).unwrap();
        fs::write(
            lib.join("x.service"),
            "[Service]\nExecStart=/bin/a\nUser=nobody\n",
        )
        .unwrap();
        fs::write(
            etc.join("x.service.d/override.conf"),
            "[Service]\nUser=root\n",
        )
        .unwrap();
        let u = Unit::load("x.service", &[etc, lib]).unwrap().unwrap();
        assert_eq!(u.get("Service", "User"), Some("root"));
        assert_eq!(u.get("Service", "ExecStart"), Some("/bin/a"));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn specifiers() {
        let sp = Specifiers::user("ann", 1000, "ann", 1000, "/home/ann", "/run/user/1000");
        assert_eq!(
            sp.expand(
                "pipewire.socket",
                "ListenStream=%t/pipewire-0\nX=%h %u %U 100%%"
            ),
            "ListenStream=/run/user/1000/pipewire-0\nX=/home/ann ann 1000 100%"
        );
        assert_eq!(
            sp.expand("getty@tty1.service", "%n %N %p %i"),
            "getty@tty1.service getty@tty1 getty tty1"
        );
        assert_eq!(
            Specifiers::system().expand("x.service", "%t/x %E %z"),
            "/run/x /etc %z"
        );
    }
}
