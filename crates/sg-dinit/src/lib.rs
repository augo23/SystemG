//! dinit backend for SystemG.
//!
//! - [`translate`] turns systemd units into dinit service files.
//! - [`generate`] does that for every unit on the system (`systemctl daemon-reload`).
//! - [`Dinit`] implements [`sg_core::ServiceManager`] by running `dinitctl`.

pub mod translate;

use sg_core::{ActiveState, Error, Result, ServiceManager, ServiceStatus};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
pub use unitparse::Specifiers;
use unitparse::{base_name, Unit, UnitKind};

pub use translate::{map_target, translate, DinitService, MARKER, READY_FD_VAR};

/// Directories early boot must create before dinit binds activation sockets.
pub const SOCKET_DIRS: &str = "systemg-env/socket-dirs";

/// Where dinit reads system services from.
pub const DINIT_DIR: &str = "/etc/dinit.d";

/// Result of a generation pass.
#[derive(Debug, Default)]
pub struct Report {
    pub written: Vec<String>,
    /// Hand-written dinit services that already existed and were left alone.
    pub kept: Vec<String>,
    pub warnings: Vec<(String, String)>,
    pub errors: Vec<String>,
    /// Generated services whose file content changed in this pass.
    pub changed: Vec<String>,
    /// Generated services linked into boot.d because systemd marks them enabled.
    pub enabled: Vec<String>,
}

/// Translate every service and target under `search_path` into `out_dir`.
///
/// Files in `out_dir` that do not start with [`MARKER`] are treated as
/// hand-written and never overwritten; the backend's own services win.
pub fn generate(search_path: &[PathBuf], out_dir: &Path) -> std::io::Result<Report> {
    generate_with(search_path, out_dir, &Specifiers::system())
}

/// Directories whose `.wants`/`.requires` links record what the admin, the
/// user or a package enabled (`/etc/systemd/{system,user}`, `~/.config/systemd/user`),
/// as opposed to the static vendor directories under `.../lib/...`.
fn config_dirs(search_path: &[PathBuf]) -> Vec<&PathBuf> {
    let dirs: Vec<&PathBuf> = search_path
        .iter()
        .filter(|d| !d.components().any(|c| c.as_os_str() == "lib"))
        .collect();
    if dirs.is_empty() {
        search_path.iter().take(1).collect()
    } else {
        dirs
    }
}

/// [`generate`] with an explicit specifier context: `Specifiers::user(...)`
/// for a per-user manager (`systemctl --user`).
pub fn generate_with(
    search_path: &[PathBuf],
    out_dir: &Path,
    spec: &Specifiers,
) -> std::io::Result<Report> {
    let mut report = Report::default();
    let configs = config_dirs(search_path);

    // Collect unit names; earlier directories shadow later ones.
    let mut names: BTreeSet<String> = BTreeSet::new();
    for dir in search_path {
        let Ok(entries) = fs::read_dir(dir) else {
            continue;
        };
        for e in entries.flatten() {
            let n = e.file_name().to_string_lossy().into_owned();
            // Skip templates (foo@.service), masked units (symlinks to /dev/null)
            // and aliases (sshd.service -> ssh.service): the real unit covers them.
            let target = fs::canonicalize(e.path()).ok();
            let is_alias = fs::symlink_metadata(e.path()).is_ok_and(|m| m.file_type().is_symlink())
                && target
                    .as_ref()
                    .and_then(|t| t.file_name())
                    .is_some_and(|f| f.to_string_lossy() != n);
            if n.contains("@.") || is_alias || target.as_deref() == Some(Path::new("/dev/null")) {
                continue;
            }
            if matches!(
                UnitKind::from_name(&n),
                UnitKind::Service | UnitKind::Target | UnitKind::Socket
            ) {
                names.insert(n);
            }
        }
    }

    let mut units: BTreeMap<String, Unit> = BTreeMap::new();
    for n in &names {
        match Unit::load_with(n, search_path, spec) {
            Ok(Some(u)) => {
                units.insert(n.clone(), u);
            }
            Ok(None) => {}
            Err(e) => report.errors.push(e.to_string()),
        }
    }

    let hand_written = |svc: &str| -> bool {
        let p = out_dir.join(svc);
        p.is_file() && !fs::read_to_string(&p).is_ok_and(|t| t.starts_with(MARKER))
    };

    // Everything that will exist after this pass.
    let mut known: BTreeSet<String> = units
        .keys()
        .filter(|n| !n.ends_with(".socket"))
        .map(|n| map_target(n))
        .collect();
    known.insert("boot".into());
    if let Ok(entries) = fs::read_dir(out_dir) {
        for e in entries.flatten() {
            if e.path().is_file() {
                known.insert(e.file_name().to_string_lossy().into_owned());
            }
        }
    }

    fs::create_dir_all(out_dir.join("systemg-env"))?;
    let mut generated: BTreeSet<String> = BTreeSet::new();
    let mut followers: Vec<(String, String)> = Vec::new();
    let mut socket_dirs: BTreeSet<String> = BTreeSet::new();
    for (n, unit) in &units {
        if n.ends_with(".socket") {
            continue;
        }
        let svc_name = map_target(n);
        if svc_name == "boot" {
            continue; // the backend owns `boot`
        }
        // Several systemd units can map onto one dinit service (basic.target
        // and sysinit.target -> sysinit); the first one wins.
        if !generated.insert(svc_name.clone()) {
            continue;
        }
        if hand_written(&svc_name) {
            report.kept.push(svc_name);
            continue;
        }
        // Attach a .socket only when it is enabled; otherwise the service runs
        // on its own (e.g. ssh.service vs. the optional ssh.socket).
        let socket_name = format!("{}.socket", base_name(n));
        let socket = units
            .get(&socket_name)
            .filter(|_| configs.iter().any(|c| is_wanted(c, &socket_name)));
        let mut svc = translate(unit, socket, &|d| known.contains(d));

        // Units this one pulls in through enablement links of its own:
        // `WantedBy=pipewire.service` becomes pipewire.service.wants/wireplumber.service.
        let mut extra = Vec::new();
        for c in &configs {
            for (suffix, key) in [("wants", "waits-for"), ("requires", "depends-on")] {
                let Ok(entries) = fs::read_dir(c.join(format!("{n}.{suffix}"))) else {
                    continue;
                };
                for e in entries.flatten() {
                    let entry = e.file_name().to_string_lossy().into_owned();
                    // Only services and targets become dependencies. A wanted
                    // .socket/.timer/.path is an activation mechanism; folding it
                    // into a dependency on its service would create cycles
                    // (sockets -> svc -> sysinit -> sockets), unlike in systemd
                    // where the socket/timer is a separate, early unit.
                    if !matches!(
                        UnitKind::from_name(&entry),
                        UnitKind::Service | UnitKind::Target
                    ) {
                        continue;
                    }
                    let dep = map_target(&entry);
                    // If the wanted unit itself depends on or orders after this
                    // one (wireplumber: BindsTo/After=pipewire), an edge from
                    // here would be a cycle in dinit. systemd's Wants= has no
                    // ordering, so do what it means instead: start the wanted
                    // unit whenever this one is enabled.
                    let points_back = units.get(&entry).is_some_and(|u| {
                        [
                            "Requires",
                            "BindsTo",
                            "Requisite",
                            "Wants",
                            "After",
                            "PartOf",
                        ]
                        .iter()
                        .any(|k| u.list("Unit", k).iter().any(|d| map_target(d) == svc.name))
                    });
                    if points_back {
                        followers.push((svc.name.clone(), dep));
                        continue;
                    }
                    let line = format!("{key} = {dep}");
                    if dep != svc.name
                        && known.contains(&dep)
                        && !svc.text.contains(&line)
                        && !extra.contains(&line)
                    {
                        extra.push(line);
                    }
                }
            }
        }
        extra.sort();
        for line in extra {
            svc.text.push_str(&line);
            svc.text.push('\n');
        }

        // A user manager can't write /var/log; log under its runtime dir.
        if spec.uid != 0 {
            let user_logs = format!("{}/systemg/log", spec.runtime_dir);
            fs::create_dir_all(&user_logs)?;
            svc.text = svc.text.replace(
                &format!("logfile = {}/", translate::LOG_DIR),
                &format!("logfile = {user_logs}/"),
            );
        }

        // dinit binds a socket before starting its service, so the socket's
        // directory (/run/cups, /run/avahi-daemon, ...) must already exist;
        // systemd creates it. Record it for early boot to create.
        for line in svc.text.lines() {
            if let Some(dir) = line
                .strip_prefix("socket-listen = ")
                .and_then(|p| Path::new(p).parent())
                .filter(|d| d.starts_with("/run") || d.starts_with("/var/run"))
            {
                socket_dirs.insert(dir.to_string_lossy().into_owned());
            }
        }

        let path = out_dir.join(&svc.name);
        if fs::read_to_string(&path).ok().as_deref() != Some(svc.text.as_str()) {
            fs::write(&path, &svc.text)?;
            report.changed.push(svc.name.clone());
        }
        if let Some((file, contents)) = &svc.env_file {
            fs::write(out_dir.join(file), contents)?;
        }
        for w in svc.warnings {
            report.warnings.push((n.clone(), w));
        }

        // Mirror systemd enablement (/etc/systemd/system/*.wants) into boot.d,
        // so packages enabled by deb-systemd-helper start at boot.
        if !configs.is_empty() {
            let enabled = configs
                .iter()
                .any(|c| is_wanted(c, n) || is_wanted(c, &socket_name) || is_display_manager(c, n));
            set_boot_link(out_dir, &svc.name, enabled)?;
            if enabled {
                report.enabled.push(svc.name.clone());
            }
        }
        report.written.push(svc.name);
    }

    if spec.uid == 0 {
        let list: String = socket_dirs.iter().map(|d| format!("{d}\n")).collect();
        fs::write(out_dir.join(SOCKET_DIRS), list)?;
    }

    // Final pass over everything generated: A wants B while B is ordered after
    // or depends on A is a cycle in dinit (systemd's Wants= has no ordering).
    // Drop A's edge and start B whenever A is enabled instead.
    let read = |n: &str| fs::read_to_string(out_dir.join(n)).unwrap_or_default();
    for a in &generated {
        let text = read(a);
        if !text.starts_with(MARKER) {
            continue;
        }
        let mut kept = Vec::new();
        let mut changed = false;
        for line in text.lines() {
            let target = line
                .strip_prefix("waits-for = ")
                .or_else(|| line.strip_prefix("depends-on = "));
            if let Some(b) = target.filter(|b| generated.contains(*b)) {
                let back = read(b);
                let points_back = back.lines().any(|l| {
                    [
                        format!("after = {a}"),
                        format!("depends-on = {a}"),
                        format!("waits-for = {a}"),
                    ]
                    .iter()
                    .any(|p| l == p)
                });
                if points_back {
                    followers.push((a.clone(), b.to_string()));
                    changed = true;
                    continue;
                }
            }
            kept.push(line);
        }
        if changed {
            fs::write(out_dir.join(a), kept.join("\n") + "\n")?;
            if !report.changed.contains(a) {
                report.changed.push(a.clone());
            }
        }
    }

    // Units pulled in by an enabled unit start with it (see `points_back`).
    for (leader, follower) in followers {
        if report.enabled.contains(&leader)
            && !report.enabled.contains(&follower)
            && generated.contains(&follower)
        {
            set_boot_link(out_dir, &follower, true)?;
            report.enabled.push(follower);
        }
    }
    Ok(report)
}

/// Targets whose `.wants` mean "start at boot".
fn is_boot_target(target: &str) -> bool {
    map_target(target) == "boot" || target == "sockets.target"
}

/// Is `unit` linked from a boot target's `.wants/` or `.requires/` in `config_dir`?
pub fn is_wanted(config_dir: &Path, unit: &str) -> bool {
    let Ok(entries) = fs::read_dir(config_dir) else {
        return false;
    };
    entries.flatten().any(|e| {
        let dir = e.file_name().to_string_lossy().into_owned();
        let target = dir
            .strip_suffix(".wants")
            .or_else(|| dir.strip_suffix(".requires"));
        target.is_some_and(is_boot_target) && fs::symlink_metadata(e.path().join(unit)).is_ok()
    })
}

/// Is `unit` the system's display manager? Packages like lightdm and gdm are
/// selected through the `display-manager.service` alias, which systemd's
/// `graphical.target` pulls in, rather than through a `.wants` link.
pub fn is_display_manager(config_dir: &Path, unit: &str) -> bool {
    fs::canonicalize(config_dir.join("display-manager.service"))
        .ok()
        .and_then(|p| p.file_name().map(|f| f.to_string_lossy() == unit))
        .unwrap_or(false)
}

/// Create or remove `boot.d/<svc>`.
pub fn set_boot_link(out_dir: &Path, svc: &str, on: bool) -> std::io::Result<()> {
    let dir = out_dir.join("boot.d");
    let link = dir.join(svc);
    if on {
        fs::create_dir_all(&dir)?;
        if fs::symlink_metadata(&link).is_err() {
            std::os::unix::fs::symlink(format!("../{svc}"), &link)?;
        }
    } else if fs::symlink_metadata(&link).is_ok() {
        fs::remove_file(&link)?;
    }
    Ok(())
}

/// Controls dinit through `dinitctl`.
pub struct Dinit {
    pub dinitctl: PathBuf,
    /// dinit's `shutdown` (also installed as reboot/halt/poweroff): dinitctl
    /// itself only knows `shutdown`, i.e. power off.
    pub shutdown: PathBuf,
    pub service_dir: PathBuf,
    /// Pass `--user` to dinitctl (user-mode dinit instance).
    pub user: bool,
}

impl Default for Dinit {
    fn default() -> Self {
        Self {
            dinitctl: "dinitctl".into(),
            shutdown: "/usr/sbin/shutdown".into(),
            service_dir: DINIT_DIR.into(),
            user: false,
        }
    }
}

impl Dinit {
    fn run(&self, args: &[&str]) -> Result<String> {
        let mut cmd = Command::new(&self.dinitctl);
        if self.user {
            cmd.arg("--user");
        }
        let out = cmd.args(args).output()?;
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        if out.status.success() {
            return Ok(stdout);
        }
        let msg = format!("{}{}", stdout, String::from_utf8_lossy(&out.stderr));
        let name = args.last().copied().unwrap_or_default();
        Err(classify_error(name, msg))
    }

    /// Set a variable in the manager's activation environment (`dinitctl setenv`).
    pub fn setenv(&self, assignment: &str) -> Result<()> {
        self.run(&["setenv", assignment]).map(drop)
    }

    /// Remove a variable from the manager's activation environment.
    pub fn unsetenv(&self, name: &str) -> Result<()> {
        self.run(&["unsetenv", name]).map(drop)
    }

    /// `poweroff`, `reboot` or `halt` the system through dinit.
    pub fn power(&self, action: &str) -> Result<()> {
        let flag = match action {
            "poweroff" => "-p",
            "reboot" => "-r",
            "halt" => "-h",
            other => return Err(Error::Backend(format!("unknown power action {other}"))),
        };
        let out = Command::new(&self.shutdown).arg(flag).output()?;
        if out.status.success() {
            Ok(())
        } else {
            Err(Error::Backend(format!(
                "{} {flag}: {}{}",
                self.shutdown.display(),
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            )))
        }
    }

    /// Whether a service description file exists in any dinit directory.
    pub fn has_description(&self, name: &str) -> bool {
        std::iter::once(self.service_dir.as_path())
            .chain(DINIT_SYSTEM_DIRS.iter().map(Path::new))
            .any(|d| d.join(name).is_file())
    }

    /// The `<from>.d` directory `dinitctl enable --from <from>` links into.
    fn link_dir(&self, from: &str) -> PathBuf {
        self.service_dir.join(format!("{from}.d"))
    }

    /// Create or remove the enable link directly. Used when dinit is not
    /// running (e.g. building an image in a chroot); dinit reads it at boot.
    fn set_link(&self, name: &str, from: &str, on: bool) -> Result<()> {
        let link = self.link_dir(from).join(name);
        if on {
            fs::create_dir_all(self.link_dir(from))?;
            if fs::symlink_metadata(&link).is_err() {
                std::os::unix::fs::symlink(format!("../{name}"), &link)?;
            }
        } else if fs::symlink_metadata(&link).is_ok() {
            fs::remove_file(&link)?;
        }
        Ok(())
    }
}

/// Other directories dinit searches for system services.
const DINIT_SYSTEM_DIRS: &[&str] = &["/run/dinit.d", "/usr/local/lib/dinit.d", "/lib/dinit.d"];

/// Map dinitctl's failure messages onto [`Error`].
pub fn classify_error(name: &str, msg: String) -> Error {
    if msg.contains("failed to find service description")
        || msg.contains("service not loaded")
        || msg.contains("not found")
    {
        Error::NotFound(name.to_string())
    } else if msg.contains("connecting to socket") {
        Error::NotRunning(msg)
    } else {
        Error::Backend(msg)
    }
}

/// Parse `dinitctl status <svc>` output.
pub fn parse_status(name: &str, text: &str) -> ServiceStatus {
    let field = |key: &str| {
        text.lines()
            .find_map(|l| l.trim().strip_prefix(key).map(|v| v.trim().to_string()))
    };
    let raw = field("State:").unwrap_or_default();
    let state = if raw.starts_with("STARTED") {
        ActiveState::Active
    } else if raw.starts_with("STARTING") {
        ActiveState::Activating
    } else if raw.starts_with("STOPPING") {
        ActiveState::Deactivating
    } else if raw.contains("fail")
        || raw.contains("could not")
        || raw.contains("terminated")
        || raw.contains("exited")
    {
        ActiveState::Failed
    } else {
        ActiveState::Inactive
    };
    let pid = field("Process ID:").and_then(|p| p.parse().ok());
    ServiceStatus {
        name: name.to_string(),
        state,
        pid,
        detail: text.to_string(),
    }
}

/// Parse one line of `dinitctl list`, e.g. `[[+]     ] nginx (pid: 812)`.
pub fn parse_list_line(line: &str) -> Option<ServiceStatus> {
    let line = line.trim();
    let close = line.rfind("] ")?;
    let (flags, rest) = (&line[..close], line[close + 2..].trim());
    let name_end = rest.find(" (").unwrap_or(rest.len());
    let name = rest[..name_end].to_string();
    let pid = rest
        .split("(pid: ")
        .nth(1)
        .and_then(|p| p.trim_end_matches(')').trim().parse().ok());
    let state = if flags.contains('X') || flags.contains('s') {
        ActiveState::Failed
    } else if flags.contains("<<") {
        ActiveState::Activating
    } else if flags.contains(">>") {
        ActiveState::Deactivating
    } else if flags.contains('+') {
        ActiveState::Active
    } else {
        ActiveState::Inactive
    };
    (!name.is_empty()).then(|| ServiceStatus {
        name,
        state,
        pid,
        detail: line.to_string(),
    })
}

impl ServiceManager for Dinit {
    fn start(&self, name: &str) -> Result<()> {
        self.run(&["start", name]).map(drop)
    }
    fn stop(&self, name: &str) -> Result<()> {
        self.run(&["stop", name]).map(drop)
    }
    fn restart(&self, name: &str) -> Result<()> {
        // `dinitctl restart` fails on a stopped service; systemd starts it instead.
        match self.status(name)?.state {
            ActiveState::Active | ActiveState::Activating => self.run(&["restart", name]).map(drop),
            _ => self.start(name),
        }
    }
    fn status(&self, name: &str) -> Result<ServiceStatus> {
        match self.run(&["status", name]) {
            Ok(text) => Ok(parse_status(name, &text)),
            // dinit only knows loaded services; one that exists on disk but was
            // never started is simply inactive.
            Err(Error::NotFound(_)) if self.has_description(name) => Ok(ServiceStatus {
                name: name.to_string(),
                state: ActiveState::Inactive,
                pid: None,
                detail: String::new(),
            }),
            Err(e) => Err(e),
        }
    }
    fn enable(&self, name: &str, target: &str) -> Result<()> {
        if !self.has_description(name) {
            return Err(Error::NotFound(name.to_string()));
        }
        if fs::symlink_metadata(self.link_dir(target).join(name)).is_ok() {
            return Ok(());
        }
        // dinitctl refuses to enable when the link directory is missing.
        fs::create_dir_all(self.link_dir(target))?;
        match self.run(&["enable", "--from", target, name]) {
            Ok(_) => Ok(()),
            Err(Error::NotRunning(_)) => self.set_link(name, target, true),
            Err(e) => Err(e),
        }
    }
    fn disable(&self, name: &str, target: &str) -> Result<()> {
        if fs::symlink_metadata(self.link_dir(target).join(name)).is_err() {
            return Ok(());
        }
        match self.run(&["disable", "--from", target, name]) {
            Ok(_) => Ok(()),
            Err(Error::NotRunning(_) | Error::NotFound(_)) => self.set_link(name, target, false),
            Err(e) => Err(e),
        }
    }
    fn is_enabled(&self, name: &str) -> Result<bool> {
        // `dinitctl enable` records the link in <from>.d/ (waits-for.d of `boot`).
        Ok(fs::read_dir(&self.service_dir)?
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".d"))
            .any(|e| fs::symlink_metadata(e.path().join(name)).is_ok()))
    }
    fn list(&self) -> Result<Vec<ServiceStatus>> {
        Ok(self
            .run(&["list"])?
            .lines()
            .filter_map(parse_list_line)
            .collect())
    }
    fn reload_definitions(&self, name: &str) -> Result<()> {
        match self.run(&["reload", name]) {
            // A service that was never loaded has nothing to reload.
            Ok(_) | Err(Error::NotFound(_)) => Ok(()),
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod power_tests {
    use super::*;

    #[test]
    fn power_actions_use_dinits_shutdown() {
        let dir = std::env::temp_dir().join(format!("sg-power-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("log");
        let fake = dir.join("shutdown");
        std::fs::write(&fake, format!("#!/bin/sh\necho \"$@\" >> {}\n", log.display())).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let d = Dinit { shutdown: fake, ..Dinit::default() };
        for a in ["reboot", "poweroff", "halt"] {
            d.power(a).unwrap();
        }
        assert_eq!(std::fs::read_to_string(&log).unwrap(), "-r\n-p\n-h\n");
        assert!(d.power("suspend").is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_parsing() {
        let s = parse_status("nginx", "Service: nginx\n    State: STARTED\n    Activation: explicitly started\n    Process ID: 812\n");
        assert_eq!(s.state, ActiveState::Active);
        assert_eq!(s.pid, Some(812));
        let s = parse_status("x", "Service: x\n    State: STOPPED (could not launch)\n");
        assert_eq!(s.state, ActiveState::Failed);
        let s = parse_status("x", "Service: x\n    State: STOPPED\n");
        assert_eq!(s.state, ActiveState::Inactive);
    }

    #[test]
    fn list_parsing() {
        let s = parse_list_line("[[+]     ] nginx (pid: 812)").unwrap();
        assert_eq!(
            (s.name.as_str(), s.state, s.pid),
            ("nginx", ActiveState::Active, Some(812))
        );
        let s = parse_list_line("[{ }<<   ] getty").unwrap();
        assert_eq!(s.state, ActiveState::Activating);
        let s = parse_list_line("[     {-}] cron").unwrap();
        assert_eq!(s.state, ActiveState::Inactive);
        let s = parse_list_line("[   X ] broken (exit status: 1)").unwrap();
        assert_eq!((s.name.as_str(), s.state), ("broken", ActiveState::Failed));
    }

    #[test]
    fn generate_respects_hand_written() {
        let root = std::env::temp_dir().join(format!("sg-dinit-gen-{}", std::process::id()));
        let units = root.join("units");
        let out = root.join("dinit.d");
        fs::create_dir_all(&units).unwrap();
        fs::create_dir_all(&out).unwrap();
        fs::write(
            units.join("a.service"),
            "[Unit]\nAfter=b.service ghost.service\n[Service]\nExecStart=/bin/a\n",
        )
        .unwrap();
        fs::write(units.join("b.service"), "[Service]\nExecStart=/bin/b\n").unwrap();
        fs::write(units.join("t@.service"), "[Service]\nExecStart=/bin/t %i\n").unwrap();
        fs::write(out.join("b"), "type = process\ncommand = /bin/hand\n").unwrap();
        fs::create_dir_all(units.join("multi-user.target.wants")).unwrap();
        std::os::unix::fs::symlink(
            "../a.service",
            units.join("multi-user.target.wants/a.service"),
        )
        .unwrap();

        let r = generate(std::slice::from_ref(&units), &out).unwrap();
        assert_eq!(r.enabled, vec!["a"]);
        assert!(fs::symlink_metadata(out.join("boot.d/a")).is_ok());
        // Disabling in systemd terms (removing the wants link) disables in dinit.
        fs::remove_file(units.join("multi-user.target.wants/a.service")).unwrap();
        generate(std::slice::from_ref(&units), &out).unwrap();
        assert!(fs::symlink_metadata(out.join("boot.d/a")).is_err());
        let r = generate(&[units], &out).unwrap();
        assert_eq!(r.written, vec!["a"]);
        assert_eq!(r.kept, vec!["b"]);
        let a = fs::read_to_string(out.join("a")).unwrap();
        assert!(a.contains("after = b") && !a.contains("ghost"), "{a}");
        assert_eq!(
            fs::read_to_string(out.join("b")).unwrap(),
            "type = process\ncommand = /bin/hand\n"
        );
        fs::remove_dir_all(root).unwrap();
    }

    /// Debian's real ssh.service (trixie), its sshd.service alias, and the
    /// optional ssh.socket that is not enabled by default.
    #[test]
    fn debian_ssh_layout() {
        let root = std::env::temp_dir().join(format!("sg-dinit-ssh-{}", std::process::id()));
        let etc = root.join("etc");
        let lib = root.join("lib");
        let out = root.join("dinit.d");
        fs::create_dir_all(etc.join("multi-user.target.wants")).unwrap();
        fs::create_dir_all(&lib).unwrap();
        fs::create_dir_all(&out).unwrap();
        fs::write(out.join("sysinit"), "type = internal\n").unwrap(); // hand-written
        fs::write(
            out.join("udevd"),
            "type = process\ncommand = /usr/sbin/udevd\n",
        )
        .unwrap();
        fs::write(
            lib.join("ssh.service"),
            "[Unit]\nDescription=OpenBSD Secure Shell server\nDocumentation=man:sshd(8)\n\
             After=network.target nss-user-lookup.target auditd.service\n\
             ConditionPathExists=!/etc/ssh/sshd_not_to_be_run\n\
             [Service]\nEnvironmentFile=-/etc/default/ssh\nExecStartPre=/usr/sbin/sshd -t\n\
             ExecStart=/usr/sbin/sshd -D $SSHD_OPTS\nExecReload=/usr/sbin/sshd -t\n\
             ExecReload=/bin/kill -HUP $MAINPID\nKillMode=process\nRestart=on-failure\n\
             RestartPreventExitStatus=255\nType=notify\nRuntimeDirectory=sshd\nRuntimeDirectoryMode=0755\n\
             [Install]\nWantedBy=multi-user.target\nAlias=sshd.service\n",
        )
        .unwrap();
        fs::write(
            lib.join("ssh.socket"),
            "[Socket]\nListenStream=22\nAccept=yes\n",
        )
        .unwrap();
        fs::write(
            lib.join("systemd-udevd.service"),
            "[Service]\nExecStart=/usr/lib/systemd/systemd-udevd\n",
        )
        .unwrap();
        std::os::unix::fs::symlink(lib.join("ssh.service"), etc.join("sshd.service")).unwrap();
        std::os::unix::fs::symlink(
            lib.join("ssh.service"),
            etc.join("multi-user.target.wants/ssh.service"),
        )
        .unwrap();

        let r = generate(&[etc, lib], &out).unwrap();
        assert!(r.written.contains(&"ssh".to_string()));
        assert!(
            !r.written.contains(&"sshd".to_string()),
            "alias must not be generated"
        );
        assert!(
            r.kept.contains(&"udevd".to_string()),
            "systemd-udevd maps to hand-written udevd"
        );
        assert_eq!(r.enabled, vec!["ssh"]);
        let ssh = fs::read_to_string(out.join("ssh")).unwrap();
        for want in [
            "type = process",
            "ready-notification = pipevar:SYSTEMG_READY_FD",
            "command = /usr/libexec/systemg/sg-notify-proxy /bin/sh -c",
            "mkdir -p /run/sshd; chmod 0755 /run/sshd",
            "/usr/sbin/sshd -t || exit 1; exec /usr/sbin/sshd -D $$SSHD_OPTS",
            "depends-on = sysinit",
            "logfile = /var/log/systemg/ssh.log",
        ] {
            assert!(ssh.contains(want), "missing {want:?} in:\n{ssh}");
        }
        assert!(!ssh.contains("socket-listen"), "ssh.socket is not enabled");
        assert!(!ssh.contains("MAINPID"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn display_manager_alias_starts_at_boot() {
        let root = std::env::temp_dir().join(format!("sg-dinit-dm-{}", std::process::id()));
        let etc = root.join("etc");
        let lib = root.join("lib");
        let out = root.join("dinit.d");
        for d in [&etc, &lib, &out] {
            fs::create_dir_all(d).unwrap();
        }
        // Debian's lightdm.service: no [Install] WantedBy, enabled via the alias.
        fs::write(
            lib.join("lightdm.service"),
            "[Unit]\nDescription=Light Display Manager\nAfter=systemd-user-sessions.service\n\
             [Service]\nExecStartPre=/bin/sh -c '[ \"$(cat /etc/X11/default-display-manager 2>/dev/null)\" = \"/usr/sbin/lightdm\" ]'\n\
             ExecStart=/usr/sbin/lightdm\nRestart=always\nBusName=org.freedesktop.DisplayManager\n\
             [Install]\nAlias=display-manager.service\n",
        )
        .unwrap();
        std::os::unix::fs::symlink(
            lib.join("lightdm.service"),
            etc.join("display-manager.service"),
        )
        .unwrap();

        let r = generate(&[etc, lib], &out).unwrap();
        assert_eq!(r.written, vec!["lightdm"], "alias itself is not generated");
        assert_eq!(r.enabled, vec!["lightdm"]);
        let dm = fs::read_to_string(out.join("lightdm")).unwrap();
        assert!(dm.contains("restart = true"), "{dm}");
        assert!(dm.contains("exec /usr/sbin/lightdm"), "{dm}");
        fs::remove_dir_all(root).unwrap();
    }

    /// Debian's PipeWire user units, enabled globally by deb-systemd-helper --user.
    #[test]
    fn user_manager_pipewire() {
        let root = std::env::temp_dir().join(format!("sg-dinit-user-{}", std::process::id()));
        let home_cfg = root.join("home/.config/systemd/user");
        let etc = root.join("etc/systemd/user");
        let lib = root.join("usr/lib/systemd/user");
        let out = root.join("run/systemg/dinit.d");
        for d in [&home_cfg, &etc, &lib, &out] {
            fs::create_dir_all(d).unwrap();
        }
        fs::write(lib.join("pipewire.socket"), "[Socket]\nListenStream=%t/pipewire-0\nSocketMode=0600\n[Install]\nWantedBy=sockets.target\n").unwrap();
        fs::write(lib.join("pipewire.service"), "[Unit]\nRequires=pipewire.socket\n[Service]\nType=simple\nExecStart=/usr/bin/pipewire\n[Install]\nAlso=pipewire.socket\nWantedBy=default.target\n").unwrap();
        fs::write(lib.join("wireplumber.service"), "[Unit]\nAfter=pipewire.service\nBindsTo=pipewire.service\n[Service]\nExecStart=/usr/bin/wireplumber\n[Install]\nWantedBy=pipewire.service\n").unwrap();
        for (dir, unit) in [
            ("sockets.target.wants", "pipewire.socket"),
            ("default.target.wants", "pipewire.service"),
            ("pipewire.service.wants", "wireplumber.service"),
        ] {
            fs::create_dir_all(etc.join(dir)).unwrap();
            std::os::unix::fs::symlink(lib.join(unit), etc.join(dir).join(unit)).unwrap();
        }
        let spec = Specifiers::user("ann", 1000, "ann", 1000, "/home/ann", "/run/user/1000");
        let r = generate_with(&[home_cfg, etc, lib], &out, &spec).unwrap();
        assert_eq!(r.enabled, vec!["pipewire", "wireplumber"], "{r:?}");
        let pw = fs::read_to_string(out.join("pipewire")).unwrap();
        assert!(
            pw.contains("socket-listen = /run/user/1000/pipewire-0"),
            "{pw}"
        );
        assert!(!pw.contains("wireplumber"), "no cycle: {pw}");
        let wp = fs::read_to_string(out.join("wireplumber")).unwrap();
        assert!(
            wp.contains("depends-on = pipewire") && wp.contains("after = pipewire"),
            "{wp}"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn socket_dirs_are_recorded() {
        let root = std::env::temp_dir().join(format!("sg-dinit-sockdir-{}", std::process::id()));
        let etc = root.join("etc");
        let lib = root.join("lib");
        let out = root.join("dinit.d");
        for d in [&etc, &lib, &out] {
            fs::create_dir_all(d).unwrap();
        }
        fs::write(
            lib.join("cups.socket"),
            "[Socket]\nListenStream=/run/cups/cups.sock\n[Install]\nWantedBy=sockets.target\n",
        )
        .unwrap();
        fs::write(
            lib.join("cups.service"),
            "[Service]\nExecStart=/usr/sbin/cupsd -l\n",
        )
        .unwrap();
        fs::create_dir_all(etc.join("sockets.target.wants")).unwrap();
        std::os::unix::fs::symlink(
            lib.join("cups.socket"),
            etc.join("sockets.target.wants/cups.socket"),
        )
        .unwrap();
        generate(&[etc, lib], &out).unwrap();
        assert_eq!(
            fs::read_to_string(out.join(SOCKET_DIRS)).unwrap(),
            "/run/cups\n"
        );
        fs::remove_dir_all(root).unwrap();
    }
}

/// First line of every D-Bus service file SystemG writes.
const DBUS_MARKER: &str = "# Generated by SystemG: D-Bus activation through dinit";

/// D-Bus activation through the service manager, as systemd does it.
///
/// Without systemd, dbus-daemon starts a service itself (`Exec=`) the moment
/// someone asks for its bus name, so a service dinit starts too runs twice
/// and the copy that loses the name exits (elogind, ModemManager, ...). For
/// every D-Bus service file naming a `SystemdService=` that dinit runs, this
/// writes a copy into `dest` (a directory dbus-daemon searches before the
/// vendor ones) whose `Exec=` asks dinit instead: `helper <dinit-service> <bus-name>`.
///
/// `sources` are the vendor service directories, `search_path` the unit
/// search path (to resolve `dbus-org.foo.service` aliases), `dinit_dir` the
/// generated services. Returns the bus names routed through dinit.
pub fn dbus_activation(
    sources: &[PathBuf],
    search_path: &[PathBuf],
    dinit_dir: &Path,
    dest: &Path,
    helper: &str,
    system_bus: bool,
) -> std::io::Result<Vec<String>> {
    let mut routed = Vec::new();
    let mut keep: BTreeSet<String> = BTreeSet::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    for dir in sources {
        if dir == dest {
            continue;
        }
        let Ok(entries) = fs::read_dir(dir) else { continue };
        let mut files: Vec<_> = entries.flatten().map(|e| e.path()).collect();
        files.sort();
        for path in files {
            let Some(file) = path.file_name().map(|f| f.to_string_lossy().into_owned()) else {
                continue;
            };
            if !file.ends_with(".service") || !seen.insert(file.clone()) {
                continue; // earlier directories win, as in dbus-daemon
            }
            let Ok(text) = fs::read_to_string(&path) else { continue };
            let field = |k: &str| {
                text.lines()
                    .filter_map(|l| l.trim().strip_prefix(k)?.trim_start().strip_prefix('='))
                    .map(|v| v.trim().to_string())
                    .next()
            };
            let (Some(name), Some(unit)) = (field("Name"), field("SystemdService")) else {
                continue;
            };
            let Some(svc) = dbus_unit_service(&unit, search_path) else { continue };
            if !dinit_dir.join(&svc).is_file() || svc.contains('/') || name.contains(' ') {
                continue;
            }
            let mut out = format!(
                "{DBUS_MARKER}\n# Vendor file: {}\n[D-BUS Service]\nName={name}\nExec={helper} {svc} {name}\n",
                path.display()
            );
            if system_bus {
                out.push_str("User=root\n");
            }
            out.push_str(&format!("SystemdService={unit}\n"));
            fs::create_dir_all(dest)?;
            let target = dest.join(&file);
            if fs::read_to_string(&target).ok().as_deref() != Some(out.as_str()) {
                fs::write(&target, out)?;
            }
            keep.insert(file);
            routed.push(name);
        }
    }
    // Drop our files for services that went away; never touch anything else.
    if let Ok(entries) = fs::read_dir(dest) {
        for e in entries.flatten() {
            let n = e.file_name().to_string_lossy().into_owned();
            if !keep.contains(&n)
                && fs::read_to_string(e.path()).is_ok_and(|t| t.starts_with(DBUS_MARKER))
            {
                let _ = fs::remove_file(e.path());
            }
        }
    }
    Ok(routed)
}

/// The dinit service behind a `SystemdService=` name, following aliases
/// (`dbus-org.freedesktop.login1.service` -> `systemd-logind.service` -> `elogind`).
fn dbus_unit_service(unit: &str, search_path: &[PathBuf]) -> Option<String> {
    let mut real = unit.to_string();
    for dir in search_path {
        let p = dir.join(unit);
        if fs::symlink_metadata(&p).is_ok() {
            if let Some(f) = fs::canonicalize(&p).ok().and_then(|c| c.file_name().map(|f| f.to_string_lossy().into_owned())) {
                real = f;
            }
            break;
        }
    }
    if real == "dbus-org.freedesktop.login1.service" {
        real = "systemd-logind.service".into(); // alias created by enabling systemd-logind
    }
    if real.contains("@.") {
        return None;
    }
    Some(map_target(&real))
}

#[cfg(test)]
mod dbus_tests {
    use super::*;

    #[test]
    fn dbus_activation_routes_through_dinit() {
        let t = std::env::temp_dir().join(format!("sg-dbus-{}", std::process::id()));
        let _ = fs::remove_dir_all(&t);
        let (vendor, units, dinit, dest) = (t.join("vendor"), t.join("units"), t.join("dinit"), t.join("local"));
        for d in [&vendor, &units, &dinit] {
            fs::create_dir_all(d).unwrap();
        }
        fs::write(vendor.join("org.freedesktop.login1.service"),
            "[D-BUS Service]\nName=org.freedesktop.login1\nExec=/bin/false\nUser=root\nSystemdService=dbus-org.freedesktop.login1.service\n").unwrap();
        fs::write(vendor.join("org.freedesktop.ModemManager1.service"),
            "[D-BUS Service]\nName=org.freedesktop.ModemManager1\nExec=/usr/sbin/ModemManager\nUser=root\nSystemdService=dbus-org.freedesktop.ModemManager1.service\n").unwrap();
        fs::write(vendor.join("org.example.NoUnit.service"),
            "[D-BUS Service]\nName=org.example.NoUnit\nExec=/usr/bin/x\nUser=root\n").unwrap();
        fs::write(units.join("ModemManager.service"), "[Service]\nExecStart=/usr/sbin/ModemManager\n").unwrap();
        std::os::unix::fs::symlink(units.join("ModemManager.service"), units.join("dbus-org.freedesktop.ModemManager1.service")).unwrap();
        fs::write(dinit.join("elogind"), "type = process\n").unwrap();
        fs::write(dinit.join("ModemManager"), "type = process\n").unwrap();
        fs::create_dir_all(&dest).unwrap();
        fs::write(dest.join("org.admin.Own.service"), "[D-BUS Service]\nName=org.admin.Own\n").unwrap();

        let mut r = dbus_activation(&[vendor.clone()], &[units.clone()], &dinit, &dest, "/h", true).unwrap();
        r.sort();
        assert_eq!(r, ["org.freedesktop.ModemManager1", "org.freedesktop.login1"]);
        let login = fs::read_to_string(dest.join("org.freedesktop.login1.service")).unwrap();
        assert!(login.contains("Exec=/h elogind org.freedesktop.login1\n"), "{login}");
        let mm = fs::read_to_string(dest.join("org.freedesktop.ModemManager1.service")).unwrap();
        assert!(mm.contains("Exec=/h ModemManager org.freedesktop.ModemManager1\n"), "{mm}");
        assert!(!dest.join("org.example.NoUnit.service").exists());

        // Service gone: our file goes, the admin's own file stays.
        fs::remove_file(dinit.join("ModemManager")).unwrap();
        dbus_activation(&[vendor], &[units], &dinit, &dest, "/h", true).unwrap();
        assert!(!dest.join("org.freedesktop.ModemManager1.service").exists());
        assert!(dest.join("org.admin.Own.service").exists());
        let _ = fs::remove_dir_all(&t);
    }
}
