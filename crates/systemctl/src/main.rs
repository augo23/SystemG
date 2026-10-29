//! `systemctl` for SystemG: the familiar commands, carried out by dinit.
//!
//! Exit codes follow systemctl(1): `is-active`/`status` return 3 when the
//! unit is not running and 4 when it does not exist.

use sg_core::{ActiveState, Error, ServiceManager};
use sg_dinit::{generate_with, map_target, Dinit, Specifiers};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use unitparse::{system_search_path, Unit};

mod preset;

const USAGE: &str = "\
systemctl (SystemG) - control services through dinit

Usage: systemctl [OPTIONS] COMMAND [UNIT...]

Commands:
  start UNIT...           Start units
  stop UNIT...            Stop units
  restart UNIT...         Restart (or start) units
  try-restart UNIT...     Restart units that are running
  reload UNIT...          Reload units (restarts them under dinit)
  status [UNIT...]        Show unit status
  is-active UNIT...       Exit 0 if all units are active
  is-failed UNIT...       Exit 0 if any unit has failed
  is-enabled UNIT...      Exit 0 if all units are enabled
  enable [--now] UNIT...  Start units at boot ([Install] WantedBy=)
  disable [--now] UNIT... Stop starting units at boot
  preset UNIT...          Enable or disable units as the preset files say
  preset-all              The same for every installed unit
  list-units              List services known to dinit
  show [-p PROP] UNIT...  Show properties (ActiveState, SubState, LoadState,
                          UnitFileState, MainPID, Id, Description)
  daemon-reload           Re-translate unit files into dinit services
  import-environment [VAR...]   Pass variables to the service manager
  set-environment VAR=VALUE...  Set variables in the service manager
  unset-environment VAR...      Remove variables from the service manager
  is-system-running       Report overall state
  poweroff | reboot | halt

Options:
      --user      Talk to your per-user service manager (systemd --user)
      --global    With enable/disable/preset/is-enabled: user units, for
                  every user (/etc/systemd/user)
  -q, --quiet     Suppress output
      --now       With enable/disable, also start/stop
  -h, --help      Show this help
  -p, --property P  With show, only these properties
      --value     With show, print values only
      --preset-mode=full|enable-only|disable-only   With preset/preset-all
Other options (--system, --no-block, --no-pager, ...) are accepted and ignored.
";

struct Ctx {
    /// `--user`: talk to the caller's per-user manager instead of PID 1.
    user: bool,
    /// `--global`: user units for all users; only unit-file links change.
    global: bool,
    spec: Specifiers,
    dinit: Dinit,
    search_path: Vec<PathBuf>,
    quiet: bool,
    now: bool,
    /// `show -p/--property`
    properties: Vec<String>,
    /// `show --value`
    value_only: bool,
    /// `preset --preset-mode=`: full, enable-only or disable-only
    preset_mode: String,
}

impl Ctx {
    fn new(user: bool) -> Result<Self, String> {
        let mut dinit = Dinit::default();
        let (spec, default_dir, default_path) = if user {
            let rt = std::env::var("XDG_RUNTIME_DIR").map_err(|_| {
                "Failed to connect to user scope bus: $XDG_RUNTIME_DIR not set".to_string()
            })?;
            let (uid, gid) = ids();
            let name = std::env::var("USER")
                .or_else(|_| std::env::var("LOGNAME"))
                .unwrap_or_else(|_| uid.to_string());
            let home = std::env::var("HOME").unwrap_or_else(|_| format!("/home/{name}"));
            dinit.user = true;
            (
                Specifiers::user(&name, uid, &name, gid, &home, &rt),
                PathBuf::from(&rt).join("systemg/dinit.d"),
                user_search_path(&home, &rt),
            )
        } else {
            (
                Specifiers::system(),
                PathBuf::from(sg_dinit::DINIT_DIR),
                system_search_path(),
            )
        };
        dinit.service_dir = std::env::var("SYSTEMG_DINIT_DIR")
            .map(PathBuf::from)
            .unwrap_or(default_dir);
        if let Ok(c) = std::env::var("SYSTEMG_DINITCTL") {
            dinit.dinitctl = c.into();
        }
        if let Ok(c) = std::env::var("SYSTEMG_SHUTDOWN") {
            dinit.shutdown = c.into();
        }
        let search_path = std::env::var("SYSTEMG_UNIT_PATH")
            .map(|p| p.split(':').map(PathBuf::from).collect())
            .unwrap_or(default_path);
        Ok(Ctx {
            user,
            spec,
            dinit,
            search_path,
            quiet: false,
            now: false,
            global: false,
            properties: Vec::new(),
            value_only: false,
            preset_mode: "full".to_string(),
        })
    }

    fn say(&self, msg: impl AsRef<str>) {
        if !self.quiet {
            println!("{}", msg.as_ref());
        }
    }
}

/// Where systemd looks for user units, highest priority first.
fn user_search_path(home: &str, runtime_dir: &str) -> Vec<PathBuf> {
    vec![
        PathBuf::from(home).join(".config/systemd/user"),
        PathBuf::from("/etc/systemd/user"),
        PathBuf::from(runtime_dir).join("systemd/user"),
        PathBuf::from("/usr/local/lib/systemd/user"),
        PathBuf::from("/usr/lib/systemd/user"),
        PathBuf::from("/lib/systemd/user"),
    ]
}

/// Real uid and gid of this process, from /proc.
fn ids() -> (u32, u32) {
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    let field = |k: &str| {
        status
            .lines()
            .find_map(|l| l.strip_prefix(k))
            .and_then(|v| v.split_whitespace().next())
            .and_then(|v| v.parse().ok())
            .unwrap_or(0)
    };
    (field("Uid:"), field("Gid:"))
}

/// `nginx`, `nginx.service` -> (`nginx.service`, dinit name `nginx`).
/// Aliases resolve to the real unit, as in systemd: `gdm3` -> `gdm.service`
/// (a symlink shipped by the package), `display-manager` -> the enabled
/// display manager, or any unit's `[Install] Alias=`.
fn resolve(ctx: &Ctx, arg: &str) -> (String, String) {
    let mut unit = if arg.contains('.') {
        arg.to_string()
    } else {
        format!("{arg}.service")
    };
    if let Some(real) = alias_target(&ctx.search_path, &unit) {
        unit = real;
    }
    let svc = map_target(&unit);
    (unit, svc)
}

/// The unit an alias name points to, if `name` is an alias.
fn alias_target(search_path: &[PathBuf], name: &str) -> Option<String> {
    for dir in search_path {
        let p = dir.join(name);
        if std::fs::symlink_metadata(&p).is_ok_and(|m| m.file_type().is_symlink()) {
            let real = std::fs::canonicalize(&p).ok()?;
            let real_name = real.file_name()?.to_string_lossy().into_owned();
            return (real_name != name && real.is_file()).then_some(real_name);
        }
        if p.is_file() {
            return None;
        }
    }
    for dir in search_path {
        let Ok(entries) = std::fs::read_dir(dir) else { continue };
        for e in entries.flatten() {
            let n = e.file_name().to_string_lossy().into_owned();
            if !n.ends_with(".service") || !e.path().is_file() {
                continue;
            }
            if let Ok(Some(u)) = Unit::load(&n, search_path) {
                if u.list("Install", "Alias").iter().any(|a| a == name) {
                    return Some(n);
                }
            }
        }
    }
    None
}

fn main() -> ExitCode {
    let flag = |f: &str| std::env::args().skip(1).take_while(|a| a != "--").any(|a| a == f);
    let (user, global) = (flag("--user"), flag("--global"));
    let mut ctx = match Ctx::new(user && !global) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(1);
        }
    };
    if global {
        // Like systemd: no manager is involved, only /etc/systemd/user links.
        ctx.global = true;
        ctx.user = true;
        ctx.search_path = global_search_path();
    }
    let mut positional = Vec::new();
    let mut args = std::env::args().skip(1);
    let mut only_positional = false;
    while let Some(a) = args.next() {
        if only_positional {
            positional.push(a);
            continue;
        }
        match a.as_str() {
            "-h" | "--help" => {
                print!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            // systemd's format: scripts parse the number on the first line.
            "--version" => {
                println!("systemd 257 (SystemG {})", env!("CARGO_PKG_VERSION"));
                println!("+PAM -SELINUX -APPARMOR -SECCOMP -BPF_FRAMEWORK backend=dinit logind=elogind");
                return ExitCode::SUCCESS;
            }
            "--" => only_positional = true,
            "-q" | "--quiet" => ctx.quiet = true,
            "--now" => ctx.now = true,
            "--value" => ctx.value_only = true,
            "-p" | "--property" => {
                if let Some(p) = args.next() {
                    ctx.properties.extend(p.split(',').map(str::to_string));
                }
            }
            s if s.starts_with("--preset-mode=") => {
                ctx.preset_mode = s["--preset-mode=".len()..].to_string()
            }
            s if s.starts_with("--property=") => ctx
                .properties
                .extend(s["--property=".len()..].split(',').map(str::to_string)),
            s if s.starts_with("-p") && s.len() > 2 => {
                ctx.properties.extend(s[2..].split(',').map(str::to_string))
            }
            // Package maintainer scripts pass many systemd-specific flags; none
            // change what SystemG does, so accept them instead of failing.
            s if s.starts_with('-') => {
                if std::env::var_os("SYSTEMG_VERBOSE").is_some() {
                    eprintln!("systemctl: ignoring option {s}");
                }
            }
            _ => positional.push(a),
        }
    }
    let Some((cmd, units)) = positional.split_first() else {
        return list_units(&ctx);
    };
    let needs_unit = !matches!(
        cmd.as_str(),
        "status"
            | "list-units"
            | "preset-all"
            | "daemon-reload"
            | "is-system-running"
            | "poweroff"
            | "reboot"
            | "halt"
    );
    if needs_unit && units.is_empty() {
        eprintln!("Too few arguments.");
        return ExitCode::from(1);
    }
    match cmd.as_str() {
        "start" => each(&ctx, units, |c, _, s| c.dinit.start(s)),
        "stop" => each(&ctx, units, |c, _, s| c.dinit.stop(s)),
        "restart" | "reload" | "reload-or-restart" | "force-reload" => {
            each(&ctx, units, |c, _, s| c.dinit.restart(s))
        }
        "try-restart" | "condrestart" | "try-reload-or-restart" => {
            each(&ctx, units, |c, _, s| match c.dinit.status(s)?.state {
                ActiveState::Active | ActiveState::Activating => c.dinit.restart(s),
                _ => Ok(()),
            })
        }
        "status" => status(&ctx, units),
        "is-active" => state_query(&ctx, units, |s| s == ActiveState::Active, true),
        "is-failed" => state_query(&ctx, units, |s| s == ActiveState::Failed, false),
        "is-enabled" => is_enabled(&ctx, units),
        "enable" => enable(&ctx, units, true),
        "disable" => enable(&ctx, units, false),
        "preset" => preset(&ctx, units),
        "preset-all" => {
            let all = installable_units(&ctx);
            preset(&ctx, &all)
        }
        "list-units" => list_units(&ctx),
        "show" => show(&ctx, units),
        "import-environment" => {
            // No names: import everything, like systemd's deprecated form.
            let vars: Vec<String> = if units.is_empty() {
                std::env::vars().map(|(k, v)| format!("{k}={v}")).collect()
            } else {
                units
                    .iter()
                    .filter_map(|k| std::env::var(k).ok().map(|v| format!("{k}={v}")))
                    .collect()
            };
            set_env(&ctx, &vars)
        }
        "set-environment" => set_env(&ctx, units),
        "unset-environment" => {
            let mut code = 0u8;
            for k in units {
                if let Err(e) = ctx.dinit.unsetenv(k) {
                    eprintln!("{e}");
                    code = 1;
                }
            }
            ExitCode::from(code)
        }
        "poweroff" | "reboot" | "halt" if ctx.user => {
            eprintln!("systemctl --user {cmd}: power actions go to the system manager; run without --user");
            ExitCode::from(1)
        }
        "poweroff" | "reboot" | "halt" => match ctx.dinit.power(cmd) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("{e}");
                ExitCode::from(1)
            }
        },
        "daemon-reload" => daemon_reload(&ctx),
        "is-system-running" => {
            ctx.say(match ctx.dinit.status("boot").map(|s| s.state) {
                Ok(ActiveState::Active) => "running",
                Ok(ActiveState::Activating) => "starting",
                _ => "degraded",
            });
            ExitCode::SUCCESS
        }
        "mask" | "unmask" | "edit" | "set-property" | "isolate" => {
            eprintln!("systemctl: '{cmd}' is not supported by SystemG yet");
            ExitCode::from(1)
        }
        other => {
            eprintln!("Unknown command verb '{other}'.");
            ExitCode::from(1)
        }
    }
}

fn each(
    ctx: &Ctx,
    units: &[String],
    f: impl Fn(&Ctx, &str, &str) -> sg_core::Result<()>,
) -> ExitCode {
    let mut code = 0u8;
    for u in units {
        let (unit, svc) = resolve(ctx, u);
        if let Err(e) = f(ctx, &unit, &svc) {
            code = match e {
                Error::NotFound(_) => {
                    eprintln!("Unit {unit} not found.");
                    5
                }
                e => {
                    eprintln!("Job for {unit} failed: {e}");
                    1
                }
            };
        }
    }
    ExitCode::from(code)
}

fn state_query(
    ctx: &Ctx,
    units: &[String],
    want: impl Fn(ActiveState) -> bool,
    all: bool,
) -> ExitCode {
    let mut hits = 0;
    for u in units {
        let (_, svc) = resolve(ctx, u);
        let state = match ctx.dinit.status(&svc) {
            Ok(s) => s.state,
            Err(Error::NotFound(_)) => ActiveState::Inactive,
            Err(e) => {
                eprintln!("{e}");
                return ExitCode::from(1);
            }
        };
        ctx.say(state.as_str());
        if want(state) {
            hits += 1;
        }
    }
    let ok = if all { hits == units.len() } else { hits > 0 };
    if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(if all { 3 } else { 1 })
    }
}

fn status(ctx: &Ctx, units: &[String]) -> ExitCode {
    if units.is_empty() {
        return list_units(ctx);
    }
    let mut code = 0u8;
    for u in units {
        let (unit, svc) = resolve(ctx, u);
        let desc = Unit::load(&unit, &ctx.search_path)
            .ok()
            .flatten()
            .and_then(|un| un.get("Unit", "Description").map(str::to_string))
            .unwrap_or_default();
        match ctx.dinit.status(&svc) {
            Ok(s) => {
                let dot = if s.state == ActiveState::Active {
                    "●"
                } else {
                    "○"
                };
                ctx.say(format!("{dot} {unit} - {desc}"));
                let enabled = ctx.dinit.is_enabled(&svc).unwrap_or(false);
                ctx.say(format!(
                    "     Loaded: loaded (dinit: {svc}; {})",
                    if enabled { "enabled" } else { "disabled" }
                ));
                ctx.say(format!("     Active: {}", s.state));
                if let Some(pid) = s.pid {
                    ctx.say(format!("   Main PID: {pid}"));
                }
                if s.state != ActiveState::Active {
                    code = code.max(3);
                }
            }
            Err(Error::NotFound(_)) => {
                eprintln!("Unit {unit} could not be found.");
                code = 4;
            }
            Err(e) => {
                eprintln!("{e}");
                code = code.max(1);
            }
        }
    }
    ExitCode::from(code)
}

fn is_enabled(ctx: &Ctx, units: &[String]) -> ExitCode {
    let mut all = true;
    for u in units {
        let (unit, svc) = resolve(ctx, u);
        let en = if ctx.global {
            // Any <target>.wants/<unit> link in /etc/systemd/user.
            ctx.search_path.first().and_then(|c| std::fs::read_dir(c).ok()).is_some_and(|d| {
                d.flatten().any(|e| std::fs::symlink_metadata(e.path().join(&unit)).is_ok())
            })
        } else {
            ctx.dinit.is_enabled(&svc).unwrap_or(false)
        };
        ctx.say(if en { "enabled" } else { "disabled" });
        all &= en;
    }
    if all {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}

fn enable(ctx: &Ctx, units: &[String], on: bool) -> ExitCode {
    let verb = if on { "enable" } else { "disable" };
    let mut code = 0u8;
    for u in units {
        let (unit, svc) = resolve(ctx, u);
        if !ctx.global && !ctx.dinit.service_dir.join(&svc).is_file() {
            // Unit installed since the last daemon-reload: translate it now.
            if let Err(e) = generate_with(&ctx.search_path, &ctx.dinit.service_dir, &ctx.spec) {
                eprintln!("daemon-reload failed: {e}");
            }
        }
        let (unit_path, mut targets, aliases) = match find_unit(ctx, &unit) {
            Ok(Some((path, un))) => {
                let t: Vec<String> = un
                    .list("Install", "WantedBy")
                    .into_iter()
                    .chain(un.list("Install", "RequiredBy"))
                    .collect();
                (Some(path), t, un.list("Install", "Alias"))
            }
            // A native dinit service with no unit file.
            Ok(None) if !ctx.global && ctx.dinit.has_description(&svc) => {
                (None, vec!["multi-user.target".to_string()], Vec::new())
            }
            Ok(None) => {
                eprintln!("Failed to {verb} unit: Unit file {unit} does not exist.");
                code = 1;
                continue;
            }
            Err(e) => {
                eprintln!("{e}");
                code = 1;
                continue;
            }
        };
        // [Install] Alias=: enabling creates the alias link. display-manager.service
        // is what graphical.target starts, so that alias means "start at boot".
        if let (Some(path), Some(config)) = (&unit_path, ctx.search_path.first()) {
            for a in &aliases {
                let link = config.join(a);
                let r = if on {
                    if std::fs::symlink_metadata(&link).is_ok() {
                        Ok(())
                    } else {
                        std::os::unix::fs::symlink(path, &link)
                    }
                } else if std::fs::canonicalize(&link).ok().as_deref() == std::fs::canonicalize(path).ok().as_deref() {
                    std::fs::remove_file(&link)
                } else {
                    Ok(())
                };
                if let Err(e) = r {
                    eprintln!("Failed to {verb} alias {a}: {e}");
                    code = 1;
                }
            }
        }
        if aliases.iter().any(|a| a == "display-manager.service") {
            targets.push("graphical.target".to_string());
        }
        if targets.is_empty() && aliases.is_empty() {
            ctx.say(format!(
                "The unit files have no installation config (WantedBy=, RequiredBy=, Alias=).\n{unit} was not {verb}d."
            ));
            continue;
        }
        if targets.is_empty() {
            continue;
        }

        // Record it the systemd way too, so daemon-reload keeps the state.
        if let (Some(path), Some(config)) = (&unit_path, ctx.search_path.first()) {
            let r = if on {
                add_wants(config, &targets, &unit, path)
            } else {
                remove_wants(config, &unit)
            };
            if let Err(e) = r {
                eprintln!("Failed to {verb} {unit}: {e}");
                code = 1;
                continue;
            }
        }
        if ctx.global {
            // Each user's manager picks the links up at its next daemon-reload.
            ctx.say(format!("{}d {unit} for all users.", capitalize(verb)));
            continue;
        }

        let mut dinit_targets: Vec<String> = targets.iter().map(|t| map_target(t)).collect();
        dinit_targets.dedup();
        for t in &dinit_targets {
            let r = if on {
                ctx.dinit.enable(&svc, t)
            } else {
                ctx.dinit.disable(&svc, t)
            };
            match r {
                Ok(()) => ctx.say(format!(
                    "{}d {unit} ({} {t}).",
                    capitalize(verb),
                    if on { "under" } else { "from" }
                )),
                Err(e) => {
                    eprintln!("{e}");
                    code = 1;
                }
            }
        }
        if ctx.now {
            let r = if on {
                ctx.dinit.start(&svc)
            } else {
                ctx.dinit.stop(&svc)
            };
            if let Err(e) = r {
                eprintln!("{e}");
                code = 1;
            }
        }
    }
    ExitCode::from(code)
}

/// Pass environment to the manager, so services started later inherit it
/// (what `dbus-update-activation-environment --systemd` and display managers
/// rely on for DISPLAY, WAYLAND_DISPLAY, XDG_SESSION_ID, ...).
/// systemd.preset(5): enable or disable each unit as the preset files say.
fn preset(ctx: &Ctx, units: &[String]) -> ExitCode {
    let rules = preset::load(&preset::search_path(ctx.user));
    let (mut on, mut off) = (Vec::new(), Vec::new());
    for u in units {
        let (unit, _) = resolve(ctx, u);
        if preset::wants_enabled(&rules, &unit) {
            on.push(unit);
        } else {
            off.push(unit);
        }
    }
    let mode = ctx.preset_mode.as_str();
    if !matches!(mode, "full" | "enable-only" | "disable-only") {
        eprintln!("Failed to parse preset mode: {mode}");
        return ExitCode::from(1);
    }
    let mut code = ExitCode::SUCCESS;
    if !on.is_empty() && mode != "disable-only" && enable(ctx, &on, true) != ExitCode::SUCCESS {
        code = ExitCode::from(1);
    }
    if !off.is_empty() && mode != "enable-only" && enable(ctx, &off, false) != ExitCode::SUCCESS {
        code = ExitCode::from(1);
    }
    code
}

/// Unit files with an [Install] section, for preset-all.
fn installable_units(ctx: &Ctx) -> Vec<String> {
    let mut names = std::collections::BTreeSet::new();
    for dir in &ctx.search_path {
        let Ok(entries) = std::fs::read_dir(dir) else { continue };
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let kind = name.rsplit('.').next().unwrap_or("");
            if !matches!(kind, "service" | "socket" | "timer" | "path" | "target") || name.contains("@.") {
                continue;
            }
            if let Ok(Some((_, unit))) = find_unit(ctx, &name) {
                if !unit.list("Install", "WantedBy").is_empty()
                    || !unit.list("Install", "RequiredBy").is_empty()
                    || !unit.list("Install", "Alias").is_empty()
                {
                    names.insert(name);
                }
            }
        }
    }
    names.into_iter().collect()
}

fn set_env(ctx: &Ctx, assignments: &[String]) -> ExitCode {
    let mut code = 0u8;
    for kv in assignments {
        if !kv.contains('=') {
            eprintln!("Not a valid environment assignment: {kv}");
            code = 1;
            continue;
        }
        if let Err(e) = ctx.dinit.setenv(kv) {
            eprintln!("{e}");
            code = 1;
        }
    }
    ExitCode::from(code)
}

fn show(ctx: &Ctx, units: &[String]) -> ExitCode {
    for (i, u) in units.iter().enumerate() {
        if i > 0 && !ctx.value_only {
            println!();
        }
        let (unit, svc) = resolve(ctx, u);
        let found = find_unit(ctx, &unit).ok().flatten();
        let status = ctx.dinit.status(&svc).ok();
        let loaded = found.is_some() || ctx.dinit.has_description(&svc);
        let state = status
            .as_ref()
            .map(|s| s.state)
            .unwrap_or(ActiveState::Inactive);
        let sub = match state {
            ActiveState::Active => "running",
            ActiveState::Activating => "start",
            ActiveState::Deactivating => "stop",
            ActiveState::Failed => "failed",
            ActiveState::Inactive => "dead",
        };
        let enabled = ctx.dinit.is_enabled(&svc).unwrap_or(false);
        let props: Vec<(&str, String)> = vec![
            ("Id", unit.clone()),
            (
                "Description",
                found
                    .as_ref()
                    .and_then(|(_, un)| un.get("Unit", "Description").map(str::to_string))
                    .unwrap_or_else(|| unit.clone()),
            ),
            (
                "LoadState",
                if loaded { "loaded" } else { "not-found" }.to_string(),
            ),
            ("ActiveState", state.as_str().to_string()),
            ("SubState", sub.to_string()),
            (
                "UnitFileState",
                if enabled { "enabled" } else { "disabled" }.to_string(),
            ),
            (
                "MainPID",
                status.and_then(|s| s.pid).unwrap_or(0).to_string(),
            ),
            ("CanStart", "yes".into()),
            ("CanStop", "yes".into()),
            ("CanReload", "no".into()),
        ];
        for (k, v) in props {
            if !ctx.properties.is_empty() && !ctx.properties.iter().any(|p| p == k) {
                continue;
            }
            if ctx.value_only {
                println!("{v}");
            } else {
                println!("{k}={v}");
            }
        }
    }
    ExitCode::SUCCESS
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    c.next()
        .map(|f| f.to_uppercase().chain(c).collect())
        .unwrap_or_default()
}

/// Locate a unit file and parse it (with drop-ins).
/// User units for all users (`--global`), highest priority first.
fn global_search_path() -> Vec<PathBuf> {
    if let Ok(p) = std::env::var("SYSTEMG_UNIT_PATH") {
        return p.split(':').map(PathBuf::from).collect();
    }
    ["/etc/systemd/user", "/usr/local/lib/systemd/user", "/usr/lib/systemd/user", "/lib/systemd/user"]
        .iter()
        .map(PathBuf::from)
        .collect()
}

fn find_unit(ctx: &Ctx, unit: &str) -> Result<Option<(PathBuf, Unit)>, unitparse::LoadError> {
    let Some(path) = ctx
        .search_path
        .iter()
        .map(|d| d.join(unit))
        .find(|p| p.is_file())
    else {
        return Ok(None);
    };
    Ok(Unit::load(unit, &ctx.search_path)?.map(|u| (path, u)))
}

/// `/etc/systemd/system/<target>.wants/<unit> -> <unit file>`, as `systemctl enable` does.
fn add_wants(
    config: &std::path::Path,
    targets: &[String],
    unit: &str,
    path: &std::path::Path,
) -> std::io::Result<()> {
    for t in targets {
        let dir = config.join(format!("{t}.wants"));
        std::fs::create_dir_all(&dir)?;
        let link = dir.join(unit);
        if std::fs::symlink_metadata(&link).is_err() {
            std::os::unix::fs::symlink(path, &link)?;
        }
    }
    Ok(())
}

fn remove_wants(config: &std::path::Path, unit: &str) -> std::io::Result<()> {
    let Ok(entries) = std::fs::read_dir(config) else {
        return Ok(());
    };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if name.ends_with(".wants") || name.ends_with(".requires") {
            let link = e.path().join(unit);
            if std::fs::symlink_metadata(&link).is_ok() {
                std::fs::remove_file(link)?;
            }
        }
    }
    Ok(())
}

fn list_units(ctx: &Ctx) -> ExitCode {
    match ctx.dinit.list() {
        Ok(list) => {
            ctx.say(format!("{:<40} {:<12} {}", "UNIT", "ACTIVE", "PID"));
            for s in &list {
                let pid = s.pid.map(|p| p.to_string()).unwrap_or_default();
                ctx.say(format!("{:<40} {:<12} {}", s.name, s.state.as_str(), pid));
            }
            ctx.say(format!("\n{} units listed.", list.len()));
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("{e}");
            ExitCode::from(1)
        }
    }
}

/// D-Bus activation goes through dinit (see `sg_dinit::dbus_activation`), so a
/// bus-activated service and the one dinit starts are the same process.
fn route_dbus_activation(ctx: &Ctx) {
    const HELPER: &str = "/usr/libexec/systemg/dbus-activate";
    if std::env::var_os("SYSTEMG_UNIT_PATH").is_some() || !Path::new(HELPER).exists() {
        return; // tests and non-SystemG systems
    }
    let (sources, dest): (Vec<PathBuf>, PathBuf) = if ctx.user {
        let data = std::env::var("XDG_DATA_HOME").unwrap_or_else(|_| format!("{}/.local/share", ctx.spec.home));
        (
            vec![
                PathBuf::from(data).join("dbus-1/services"),
                "/usr/local/share/dbus-1/services".into(),
                "/usr/share/dbus-1/services".into(),
            ],
            PathBuf::from(&ctx.spec.runtime_dir).join("dbus-1/services"),
        )
    } else {
        (
            vec!["/usr/share/dbus-1/system-services".into(), "/lib/dbus-1/system-services".into()],
            "/usr/local/share/dbus-1/system-services".into(),
        )
    };
    match sg_dinit::dbus_activation(&sources, &ctx.search_path, &ctx.dinit.service_dir, &dest, HELPER, !ctx.user) {
        Ok(_) => {
            // dbus-daemon re-reads its service directories on ReloadConfig.
            let bus = if ctx.user { "--session" } else { "--system" };
            let _ = std::process::Command::new("dbus-send")
                .args([bus, "--type=method_call", "--dest=org.freedesktop.DBus", "/org/freedesktop/DBus", "org.freedesktop.DBus.ReloadConfig"])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
        }
        Err(e) => eprintln!("D-Bus activation setup failed: {e}"),
    }
}

fn daemon_reload(ctx: &Ctx) -> ExitCode {
    let report = match generate_with(&ctx.search_path, &ctx.dinit.service_dir, &ctx.spec) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("daemon-reload failed: {e}");
            return ExitCode::from(1);
        }
    };
    for svc in &report.changed {
        // Ask dinit to re-read services that are already loaded.
        let _ = ctx.dinit.reload_definitions(svc);
    }
    route_dbus_activation(ctx);
    if std::env::var_os("SYSTEMG_VERBOSE").is_some() {
        for (unit, w) in &report.warnings {
            eprintln!("{unit}: {w}");
        }
    }
    for e in &report.errors {
        eprintln!("{e}");
    }
    ctx.say(format!(
        "Translated {} units into {} ({} hand-written services kept, {} warnings; SYSTEMG_VERBOSE=1 to show).",
        report.written.len(),
        ctx.dinit.service_dir.display(),
        report.kept.len(),
        report.warnings.len()
    ));
    if report.errors.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}
