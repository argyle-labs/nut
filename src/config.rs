//! NUT config model + idempotent renderers for the four `upsd`/`upsmon` files.
//!
//! `configure` receives a JSON [`NutConfig`] and writes `ups.conf`,
//! `upsd.conf`, `upsmon.conf`, and `upssched.conf` into the config directory.
//! Rendering is deterministic and idempotent: the same [`NutConfig`] always
//! produces byte-identical files, and a file is only rewritten when its content
//! actually differs, so re-applying is a no-op.
//!
//! The model supports the two roles NUT hosts run in:
//! - **netserver** — this host talks to the UPS hardware and serves `upsd`
//!   (has one or more [`UpsDef`]s and, usually, monitors them locally).
//! - **monitor-only client** — no local UPS; it monitors a `upsd` running on
//!   another host (an empty `upses` list, `monitors` pointing elsewhere).

use std::io::Write;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

/// The whole desired NUT configuration for one host.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct NutConfig {
    /// UPS hardware this host drives (empty on a monitor-only client).
    #[serde(default)]
    pub upses: Vec<UpsDef>,
    /// `upsd` listen directives (`LISTEN <addr> [port]`). Empty = `upsd`
    /// default (localhost only). Use `"0.0.0.0"` to serve the LAN.
    #[serde(default)]
    pub listen: Vec<ListenDef>,
    /// `upsmon` monitor lines — which `upsd`/UPS this host watches.
    #[serde(default)]
    pub monitors: Vec<MonitorDef>,
    /// `upsmon` power thresholds and behaviour.
    #[serde(default)]
    pub upsmon: UpsmonSettings,
    /// `upssched`/`upsmon` notification + event-capture wiring.
    #[serde(default)]
    pub notify: NotifySettings,
    /// UPS names on disk the caller intends to drop. Apply refuses to remove
    /// any on-disk UPS not listed here.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remove_upses: Vec<String>,
}

/// One UPS section in `ups.conf` (`[name]` + `driver`/`port`/…).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct UpsDef {
    pub name: String,
    pub driver: String,
    pub port: String,
    #[serde(default)]
    pub description: String,
    /// Extra driver options rendered verbatim as `key = value` lines.
    #[serde(default)]
    pub extra: Vec<(String, String)>,
}

/// A `upsd.conf` `LISTEN` directive.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ListenDef {
    pub address: String,
    #[serde(default)]
    pub port: Option<u16>,
}

/// A `upsmon.conf` `MONITOR` line: `MONITOR <ups>@<host> <powervalue> <user> <pass> <type>`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct MonitorDef {
    /// `ups@host` system to monitor (`serverups@localhost`).
    pub system: String,
    /// Number of power supplies this host draws from this UPS (`powervalue`).
    #[serde(default = "one")]
    pub power_value: u32,
    pub username: String,
    /// Credential reference, not the secret itself; rendered verbatim.
    pub password: String,
    /// `master` (drives shutdown) or `slave`.
    #[serde(default = "master")]
    pub role: String,
}

fn one() -> u32 {
    1
}
fn master() -> String {
    "master".to_string()
}

/// `upsmon.conf` power/shutdown settings that the `battery-thresholds`,
/// `kill-power`, and `shutdown-command` checks read and repair.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UpsmonSettings {
    /// `MINSUPPLIES` — how many power supplies must be fed for the host to run.
    pub min_supplies: u32,
    /// `SHUTDOWNCMD` — the command run to shut this host down on a critical UPS.
    pub shutdown_cmd: String,
    /// `POWERDOWNFLAG` — file `upsmon` touches so the halt scripts kill UPS power.
    pub power_down_flag: String,
    /// Whether the shutdown sequence should cut UPS power (`upsmon -K` /
    /// `upsdrvctl shutdown`). Drives the `kill-power` check.
    pub kill_power: bool,
    /// `HOSTSYNC` seconds — how long the master waits for slaves.
    pub host_sync: u32,
    /// `DEADTIME` seconds — how long a stale UPS is tolerated before dead.
    pub dead_time: u32,
}

impl Default for UpsmonSettings {
    fn default() -> Self {
        Self {
            min_supplies: 1,
            // A completing, force-unmount-aware shutdown. On Unraid, pair this
            // with the unraid `shutdown-timeout` repair (the host-side
            // force-unmount net) — the two are complementary.
            shutdown_cmd: "/sbin/shutdown -h +0".to_string(),
            power_down_flag: "/etc/killpower".to_string(),
            kill_power: true,
            host_sync: 15,
            dead_time: 15,
        }
    }
}

/// Notification / event-capture wiring for `upsmon.conf` + `upssched.conf`.
///
/// The survivable-logging path: on events `upsmon` calls `upssched`, whose
/// command handler logs to flash (`log_path`) and forwards to a remote syslog
/// target, and on `ONBATT` runs the diagnostics snapshot before shutdown.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NotifySettings {
    /// Path `upssched` invokes as its command handler (`NOTIFYCMD`/`CMDSCRIPT`).
    pub notify_cmd: String,
    /// `upssched` command pipe/lock paths.
    pub pipe: String,
    pub lock: String,
    /// Where event records are appended — on persistent flash, not tmpfs, so
    /// they survive the power-cycle recovery needs.
    pub log_path: String,
    /// Remote syslog target (`host:port`). Generic placeholder by default; the
    /// real fleet target is supplied at deploy time.
    pub remote_syslog: String,
    /// Command run on `ONBATT` to snapshot diagnostics to flash + remote before
    /// the shutdown proceeds.
    pub onbattery_capture_cmd: String,
}

impl Default for NotifySettings {
    fn default() -> Self {
        Self {
            notify_cmd: "/usr/local/bin/orca-nut-notify".to_string(),
            pipe: "/var/run/nut/upssched.pipe".to_string(),
            lock: "/var/run/nut/upssched.lock".to_string(),
            // Flash-backed by default so records survive a power-cycle.
            log_path: "/boot/config/plugins/nut/events.log".to_string(),
            // Generic placeholder — no real fleet host. Overridden at deploy.
            remote_syslog: "syslog-collector.example.invalid:514".to_string(),
            onbattery_capture_cmd:
                "/usr/local/bin/orca diagnostics diagnose >> /boot/config/plugins/nut/onbattery.json"
                    .to_string(),
        }
    }
}

/// The default config directory NUT reads on most Linux hosts.
pub const DEFAULT_CONF_DIR: &str = "/etc/nut";

/// A rendered config file: relative name + full contents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedFile {
    pub name: &'static str,
    pub contents: String,
}

impl NutConfig {
    /// The topology and settings currently on disk in `dir`; missing files
    /// read as empty. `notify` is left at its default.
    pub fn read_existing(dir: &Path) -> Self {
        let read = |name: &str| std::fs::read_to_string(dir.join(name)).unwrap_or_default();
        Self {
            upses: parse_ups_conf(&read("ups.conf")),
            listen: parse_upsd_conf(&read("upsd.conf")),
            monitors: parse_monitors(&read("upsmon.conf")),
            upsmon: crate::checks::read_upsmon(dir).unwrap_or_default(),
            ..Default::default()
        }
    }

    /// Render all four config files deterministically.
    pub fn render(&self) -> Vec<RenderedFile> {
        vec![
            RenderedFile {
                name: "ups.conf",
                contents: self.render_ups_conf(),
            },
            RenderedFile {
                name: "upsd.conf",
                contents: self.render_upsd_conf(),
            },
            RenderedFile {
                name: "upsmon.conf",
                contents: self.render_upsmon_conf(),
            },
            RenderedFile {
                name: "upssched.conf",
                contents: self.render_upssched_conf(),
            },
        ]
    }

    /// Write every rendered file into `dir`, only touching files whose content
    /// changed, then restart the NUT daemons this topology runs.
    pub fn apply(&self, dir: &Path) -> Result<ApplyOutcome, String> {
        self.apply_with(dir, &SystemServices::new(), Path::new("/etc/group"))
    }

    /// [`apply`](Self::apply) with injectable service control and group file.
    ///
    /// Every changed file is fully staged (written, chmod, chown, fsync) before
    /// any rename; the renames then run back to back and are rolled back if one
    /// fails. `Err` means the old config is still in place; once the renames
    /// land the outcome is `Ok`, with restart problems in `restart_required`.
    pub fn apply_with(
        &self,
        dir: &Path,
        services: &dyn ServiceControl,
        group_file: &Path,
    ) -> Result<ApplyOutcome, String> {
        if self.monitors.is_empty() {
            return Err(
                "refusing to apply: config has no MONITOR lines, so upsmon would \
                        protect nothing"
                    .to_string(),
            );
        }
        let on_disk =
            parse_ups_conf(&std::fs::read_to_string(dir.join("ups.conf")).unwrap_or_default());
        let dropped: Vec<&str> = on_disk
            .iter()
            .map(|u| u.name.as_str())
            .filter(|n| !self.upses.iter().any(|u| u.name == *n))
            .filter(|n| !self.remove_upses.iter().any(|r| r == n))
            .collect();
        if !dropped.is_empty() {
            return Err(format!(
                "refusing to apply: it would drop UPS {} from ups.conf; list them in \
                 remove_upses to remove them",
                dropped.join(", ")
            ));
        }
        // Restart the server stack if this host drives UPS hardware before or after.
        let server = !self.upses.is_empty() || !on_disk.is_empty();
        std::fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
        let rendered = self.render();
        sweep_stale_temps(dir, &rendered);
        let mut changed = Vec::new();
        for file in &rendered {
            let path = dir.join(file.name);
            // Symlinked targets are refused: renaming over the link would
            // silently detach it from whatever it pointed at. Only the files
            // are checked; a symlinked `dir` itself is followed.
            if std::fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink()) {
                return Err(format!(
                    "{} is a symlink; refusing to replace it",
                    path.display()
                ));
            }
            let old = std::fs::read(&path).ok();
            if old.as_deref() != Some(file.contents.as_bytes()) {
                changed.push((file, path, old));
            }
        }
        if changed.is_empty() {
            return Ok(ApplyOutcome::default());
        }

        let mut staged = Vec::new();
        for (file, path, _) in &changed {
            match stage(path, file.contents.as_bytes(), group_file) {
                Ok(s) => staged.push(s),
                Err(e) => {
                    for s in &staged {
                        let _removed = std::fs::remove_file(&s.tmp);
                    }
                    return Err(e);
                }
            }
        }

        for (i, s) in staged.iter().enumerate() {
            if let Err(e) = std::fs::rename(&s.tmp, &s.path) {
                let mut err = format!("rename {}: {e}", s.path.display());
                for s in &staged[i..] {
                    let _removed = std::fs::remove_file(&s.tmp);
                }
                for (j, s) in staged[..i].iter().enumerate().rev() {
                    if let Err(r) = rollback(&s.path, changed[j].2.as_deref(), &s.meta, group_file)
                    {
                        err.push_str(&format!("; rollback {}: {r}", s.path.display()));
                    }
                }
                return Err(err);
            }
        }

        let written: Vec<String> = changed.iter().map(|(f, _, _)| f.name.to_string()).collect();
        let mut notes: Vec<String> = staged.iter().filter_map(|s| s.note.clone()).collect();
        if let Err(e) = std::fs::File::open(dir).and_then(|d| d.sync_all()) {
            notes.push(format!("fsync {}: {e}", dir.display()));
        }
        let restart_required = match services.restart(server) {
            Restart::Done(units) => {
                notes.push(format!("restarted {}", units.join(", ")));
                false
            }
            Restart::Failed(e) => {
                notes.push(format!("NUT restart failed: {e}"));
                true
            }
            Restart::Unavailable(e) => {
                notes.push(e);
                true
            }
        };
        Ok(ApplyOutcome {
            written,
            restart_required,
            message: notes.join("; "),
        })
    }

    fn render_ups_conf(&self) -> String {
        let mut out = String::from("# Managed by orca (nut plugin). Do not edit by hand.\n");
        for ups in &self.upses {
            out.push_str(&format!("\n[{}]\n", ups.name));
            out.push_str(&format!("    driver = {}\n", ups.driver));
            out.push_str(&format!("    port = {}\n", ups.port));
            if !ups.description.is_empty() {
                out.push_str(&format!("    desc = \"{}\"\n", ups.description));
            }
            for (k, v) in &ups.extra {
                out.push_str(&format!("    {k} = {v}\n"));
            }
        }
        out
    }

    fn render_upsd_conf(&self) -> String {
        let mut out = String::from("# Managed by orca (nut plugin). Do not edit by hand.\n");
        for l in &self.listen {
            match l.port {
                Some(p) => out.push_str(&format!("LISTEN {} {p}\n", l.address)),
                None => out.push_str(&format!("LISTEN {}\n", l.address)),
            }
        }
        out
    }

    fn render_upsmon_conf(&self) -> String {
        let s = &self.upsmon;
        let n = &self.notify;
        let mut out = String::from("# Managed by orca (nut plugin). Do not edit by hand.\n");
        for m in &self.monitors {
            out.push_str(&format!(
                "MONITOR {} {} {} {} {}\n",
                m.system, m.power_value, m.username, m.password, m.role
            ));
        }
        out.push_str(&format!("MINSUPPLIES {}\n", s.min_supplies));
        out.push_str(&format!("SHUTDOWNCMD \"{}\"\n", s.shutdown_cmd));
        out.push_str(&format!("POWERDOWNFLAG {}\n", s.power_down_flag));
        out.push_str(&format!("HOSTSYNC {}\n", s.host_sync));
        out.push_str(&format!("DEADTIME {}\n", s.dead_time));
        // Route every event through upssched so logging + capture run.
        out.push_str(&format!("NOTIFYCMD {}\n", n.notify_cmd));
        for flag in [
            "ONLINE", "ONBATT", "LOWBATT", "COMMBAD", "COMMOK", "SHUTDOWN",
        ] {
            out.push_str(&format!("NOTIFYFLAG {flag} SYSLOG+WALL+EXEC\n"));
        }
        out
    }

    fn render_upssched_conf(&self) -> String {
        let n = &self.notify;
        let mut out = String::from("# Managed by orca (nut plugin). Do not edit by hand.\n");
        out.push_str(&format!("CMDSCRIPT {}\n", n.notify_cmd));
        out.push_str(&format!("PIPEFN {}\n", n.pipe));
        out.push_str(&format!("LOCKFN {}\n", n.lock));
        // Persistent-event-log: every event logs to flash + remote immediately.
        for event in [
            "ONLINE", "ONBATT", "LOWBATT", "COMMBAD", "COMMOK", "SHUTDOWN",
        ] {
            out.push_str(&format!(
                "AT {event} * EXECUTE log-{}\n",
                event.to_lowercase()
            ));
        }
        // On-battery diagnostics capture runs before shutdown proceeds.
        out.push_str("AT ONBATT * EXECUTE onbattery-capture\n");
        // Persist the log target + remote + capture command as comments the
        // command handler reads (single source of truth for the handler).
        out.push_str(&format!("# orca-log-path {}\n", n.log_path));
        out.push_str(&format!("# orca-remote-syslog {}\n", n.remote_syslog));
        out.push_str(&format!(
            "# orca-onbattery-capture {}\n",
            n.onbattery_capture_cmd
        ));
        out
    }
}

/// Resolve the active NUT config directory: the first of the standard paths
/// that exists, else [`DEFAULT_CONF_DIR`].
pub fn detect_conf_dir() -> PathBuf {
    for candidate in [DEFAULT_CONF_DIR, "/boot/config/plugins/nut", "/etc/ups"] {
        if Path::new(candidate).is_dir() {
            return PathBuf::from(candidate);
        }
    }
    PathBuf::from(DEFAULT_CONF_DIR)
}

/// Result of an [`NutConfig::apply`] that got the new files into place.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ApplyOutcome {
    /// Files actually rewritten (empty = no-op).
    pub written: Vec<String>,
    /// The new config is on disk but the daemons may still run the old one.
    pub restart_required: bool,
    /// Restart / fsync notes for the operator; empty on a no-op.
    pub message: String,
}

/// What a [`ServiceControl::restart`] achieved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Restart {
    /// Every listed unit was restarted.
    Done(Vec<String>),
    /// At least one restart failed; the message names each failure.
    Failed(String),
    /// No usable service manager (e.g. a container); nothing was attempted.
    Unavailable(String),
}

/// Restarts the NUT daemons after a config write.
pub trait ServiceControl {
    /// `server`: this host drives UPS hardware (drivers + `upsd` + `upsmon`);
    /// otherwise it is a monitor-only client and only `upsmon` is touched.
    fn restart(&self, server: bool) -> Restart;
}

/// The init system NUT runs under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InitSystem {
    /// Unraid (Slackware) with the NUT plugin's `/etc/rc.d/rc.nut`.
    Unraid,
    Systemd,
    OpenRc,
    None,
}

const RC_NUT: &str = "etc/rc.d/rc.nut";

/// Detect the init system under `root` (`/` on a real host).
pub fn detect_init(root: &Path) -> InitSystem {
    if root.join(RC_NUT).is_file() {
        InitSystem::Unraid
    } else if root.join("run/systemd/system").is_dir() {
        InitSystem::Systemd
    } else if root.join("sbin/openrc-run").exists() || root.join("sbin/rc-service").exists() {
        InitSystem::OpenRc
    } else {
        InitSystem::None
    }
}

/// Why `rc.nut restart` is unsafe here, or `None` when the Unraid NUT plugin
/// is positively in manual config mode.
///
/// `rc.nut restart` runs the plugin's `write_config`, which in GUI mode
/// (`MANUAL="disable"`/`"onlyups"`) rewrites lines of `ups.conf`/`upsmon.conf`
/// from the plugin's own settings, clobbering ours. Only `MANUAL="enable"`
/// leaves `/etc/nut` alone (it then just copies it to flash), so every plugin
/// cfg present must say so.
fn unraid_gui_mode(root: &Path) -> Option<String> {
    let mut found = false;
    for cfg in [
        "boot/config/plugins/nut-dw/nut-dw.cfg",
        "boot/config/plugins/nut/nut.cfg",
    ] {
        let Ok(text) = std::fs::read_to_string(root.join(cfg)) else {
            continue;
        };
        let manual = text.lines().find_map(|l| {
            let (k, v) = l.split_once('=')?;
            (k.trim() == "MANUAL").then(|| v.trim().trim_matches('"').to_string())
        });
        match manual {
            Some(m) if m == "enable" => found = true,
            Some(m) => return Some(format!("MANUAL={m} in /{cfg}")),
            None => {}
        }
    }
    (!found).then(|| "no MANUAL=\"enable\" in the NUT plugin config".to_string())
}

type Runner = Box<dyn Fn(&str, &[&str]) -> Result<String, String>>;

/// Real service control for the detected [`InitSystem`].
pub struct SystemServices {
    root: PathBuf,
    run: Runner,
}

impl SystemServices {
    pub fn new() -> Self {
        Self {
            root: PathBuf::from("/"),
            run: Box::new(crate::checks::run),
        }
    }

    /// The `(bin, args)` restarts for this topology, only for units that exist.
    fn plan(&self, init: InitSystem, server: bool) -> Vec<(String, Vec<String>)> {
        match init {
            InitSystem::Unraid => {
                // rc.nut picks driver/upsd/upsmon from its own MODE.
                let rc = self.root.join(RC_NUT).to_string_lossy().into_owned();
                vec![(rc, vec!["restart".to_string()])]
            }
            InitSystem::Systemd => {
                let present = |u: &&str| (self.run)("systemctl", &["cat", u]).is_ok();
                let mut units = Vec::new();
                if server {
                    // NUT >= 2.8 drives per-UPS nut-driver@ instances via the target.
                    units.extend(
                        ["nut-driver.target", "nut-driver.service"]
                            .into_iter()
                            .find(present),
                    );
                    units.extend(["nut-server.service"].into_iter().filter(present));
                }
                units.extend(["nut-monitor.service"].into_iter().filter(present));
                units
                    .into_iter()
                    .map(|u| {
                        let args = vec!["restart".to_string(), u.to_string()];
                        ("systemctl".to_string(), args)
                    })
                    .collect()
            }
            InitSystem::OpenRc => {
                // Alpine: nut-upsd (starts the drivers too) + nut-upsmon.
                // Gentoo: upsdrv + upsd + upsmon.
                let mut names = Vec::new();
                if server {
                    names.extend(["nut-upsd", "upsdrv", "upsd"]);
                }
                names.extend(["nut-upsmon", "upsmon"]);
                names
                    .into_iter()
                    .filter(|n| self.root.join("etc/init.d").join(n).exists())
                    .map(|n| {
                        let args = vec![n.to_string(), "restart".to_string()];
                        ("rc-service".to_string(), args)
                    })
                    .collect()
            }
            InitSystem::None => Vec::new(),
        }
    }
}

impl Default for SystemServices {
    fn default() -> Self {
        Self::new()
    }
}

impl ServiceControl for SystemServices {
    fn restart(&self, server: bool) -> Restart {
        let init = detect_init(&self.root);
        if init == InitSystem::Unraid {
            if let Some(mode) = unraid_gui_mode(&self.root) {
                return Restart::Unavailable(format!(
                    "Unraid NUT plugin is not in manual config mode ({mode}); `rc.nut restart` would \
                     overwrite the new config. Switch the plugin to manual config mode, then run \
                     /etc/rc.d/rc.nut restart"
                ));
            }
        }
        let plan = self.plan(init, server);
        if plan.is_empty() {
            return Restart::Unavailable(if init == InitSystem::None {
                "no NUT service manager found; restart NUT to load the new config".to_string()
            } else {
                format!("no NUT units found ({init:?}); restart NUT to load the new config")
            });
        }
        let mut done = Vec::new();
        let mut failed = Vec::new();
        for (bin, args) in &plan {
            let args: Vec<&str> = args.iter().map(String::as_str).collect();
            let label = format!("{bin} {}", args.join(" "));
            match (self.run)(bin, &args) {
                Ok(_) => done.push(label),
                Err(e) => failed.push(format!("{label}: {e}")),
            }
        }
        if failed.is_empty() {
            Restart::Done(done)
        } else if done.is_empty() {
            Restart::Failed(failed.join("; "))
        } else {
            Restart::Failed(format!(
                "{} (restarted {})",
                failed.join("; "),
                done.join(", ")
            ))
        }
    }
}

static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// A fully written temp file ready to be renamed over `path`.
struct Staged {
    tmp: PathBuf,
    path: PathBuf,
    meta: Option<std::fs::Metadata>,
    /// Non-fatal staging problem to surface to the operator.
    note: Option<String>,
}

/// Write `contents` to a same-dir temp file with the target's final mode and
/// owner, fsynced. Keeps an existing file's mode/uid/gid (upsmon.conf is clamped
/// to at most 0640 since it carries the MONITOR password); a new file gets 0640
/// root:nut.
fn stage(path: &Path, contents: &[u8], group_file: &Path) -> Result<Staged, String> {
    let meta = std::fs::metadata(path).ok();
    let (tmp, note) = stage_with(path, contents, meta.as_ref(), group_file, true)?;
    Ok(Staged {
        tmp,
        path: path.to_path_buf(),
        meta,
        note,
    })
}

fn stage_with(
    path: &Path,
    contents: &[u8],
    existing: Option<&std::fs::Metadata>,
    group_file: &Path,
    clamp: bool,
) -> Result<(PathBuf, Option<String>), String> {
    let dir = path.parent().unwrap_or(Path::new("."));
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("conf");
    let (mut mode, uid, gid) = match existing {
        Some(m) => (m.mode() & 0o7777, m.uid(), m.gid()),
        None => {
            let gid = nut_gid(group_file).ok_or_else(|| {
                format!(
                    "no `nut` group in {}; cannot create {} as root:nut",
                    group_file.display(),
                    path.display()
                )
            })?;
            (0o640, 0, gid)
        }
    };
    if clamp && name == "upsmon.conf" {
        mode &= 0o640;
    }
    let tmp = dir.join(format!(
        ".{name}.{}.{}.tmp",
        std::process::id(),
        TMP_SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let result = (|| {
        let mut note = None;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(&tmp)
            .map_err(|e| format!("create {}: {e}", tmp.display()))?;
        // `mode()` is masked by the umask; set it explicitly.
        f.set_permissions(std::fs::Permissions::from_mode(mode))
            .map_err(|e| format!("chmod {}: {e}", tmp.display()))?;
        // An unprivileged caller cannot chown to another owner; that is only
        // an error if the result differs from the owner we must keep.
        if let Err(e) = std::os::unix::fs::fchown(&f, Some(uid), Some(gid)) {
            let m = f
                .metadata()
                .map_err(|e| format!("stat {}: {e}", tmp.display()))?;
            if (m.uid(), m.gid()) != (uid, gid) {
                if existing.is_some() {
                    return Err(format!("chown {}: {e}", tmp.display()));
                }
                note = Some(format!(
                    "{} created as {}:{} (chown to {uid}:{gid} failed: {e})",
                    path.display(),
                    m.uid(),
                    m.gid()
                ));
            }
        }
        f.write_all(contents)
            .and_then(|_| f.sync_all())
            .map_err(|e| format!("write {}: {e}", tmp.display()))?;
        Ok(note)
    })();
    match result {
        Ok(note) => Ok((tmp, note)),
        Err(e) => {
            let _removed = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// Undo an already-renamed file: restore `old` with its original metadata, or
/// remove the file if it did not exist before.
fn rollback(
    path: &Path,
    old: Option<&[u8]>,
    meta: &Option<std::fs::Metadata>,
    group_file: &Path,
) -> Result<(), String> {
    match old {
        Some(old) => {
            let (tmp, _) = stage_with(path, old, meta.as_ref(), group_file, false)?;
            std::fs::rename(&tmp, path).map_err(|e| {
                let _removed = std::fs::remove_file(&tmp);
                e.to_string()
            })
        }
        None => std::fs::remove_file(path).map_err(|e| e.to_string()),
    }
}

/// Remove temp files a crashed earlier apply left behind (`.<name>.*.tmp`).
fn sweep_stale_temps(dir: &Path, files: &[RenderedFile]) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let stale = name.ends_with(".tmp")
            && files
                .iter()
                .any(|f| name.starts_with(&format!(".{}.", f.name)));
        if stale {
            let _removed = std::fs::remove_file(entry.path());
        }
    }
}

/// `[name]` sections of `ups.conf`; directives before the first section are
/// not modelled and are dropped.
fn parse_ups_conf(text: &str) -> Vec<UpsDef> {
    let mut upses: Vec<UpsDef> = Vec::new();
    for line in text.lines().map(str::trim) {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            upses.push(UpsDef {
                name: name.trim().to_string(),
                ..Default::default()
            });
            continue;
        }
        let Some(ups) = upses.last_mut() else {
            continue;
        };
        let (k, v) = line.split_once('=').unwrap_or((line, ""));
        let (k, v) = (k.trim(), v.trim());
        match k {
            "driver" => ups.driver = v.to_string(),
            "port" => ups.port = v.to_string(),
            "desc" => ups.description = v.trim_matches('"').to_string(),
            _ => ups.extra.push((k.to_string(), v.to_string())),
        }
    }
    upses
}

fn parse_upsd_conf(text: &str) -> Vec<ListenDef> {
    text.lines()
        .filter_map(|l| {
            let mut f = l.split_whitespace();
            (f.next()? == "LISTEN").then_some(())?;
            Some(ListenDef {
                address: f.next()?.to_string(),
                port: f.next().and_then(|p| p.parse().ok()),
            })
        })
        .collect()
}

fn parse_monitors(text: &str) -> Vec<MonitorDef> {
    text.lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            if f.len() != 6 || f[0] != "MONITOR" {
                return None;
            }
            Some(MonitorDef {
                system: f[1].to_string(),
                power_value: f[2].parse().unwrap_or(1),
                username: f[3].to_string(),
                password: f[4].to_string(),
                role: f[5].to_string(),
            })
        })
        .collect()
}

/// gid of the `nut` group from `group_file`, if present.
fn nut_gid(group_file: &Path) -> Option<u32> {
    let groups = std::fs::read_to_string(group_file).ok()?;
    groups.lines().find_map(|l| {
        let mut parts = l.split(':');
        (parts.next()? == "nut").then(|| parts.nth(1)?.parse().ok())?
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> NutConfig {
        NutConfig {
            upses: vec![UpsDef {
                name: "serverups".into(),
                driver: "usbhid-ups".into(),
                port: "auto".into(),
                description: "Rack UPS".into(),
                extra: vec![],
            }],
            listen: vec![ListenDef {
                address: "0.0.0.0".into(),
                port: Some(3493),
            }],
            monitors: vec![MonitorDef {
                system: "serverups@localhost".into(),
                power_value: 1,
                username: "monuser".into(),
                password: "secretref".into(),
                role: "master".into(),
            }],
            upsmon: UpsmonSettings::default(),
            notify: NotifySettings::default(),
            remove_upses: vec![],
        }
    }

    #[test]
    fn renders_all_four_files() {
        let files = sample().render();
        let names: Vec<_> = files.iter().map(|f| f.name).collect();
        assert_eq!(
            names,
            vec!["ups.conf", "upsd.conf", "upsmon.conf", "upssched.conf"]
        );
    }

    #[test]
    fn render_is_deterministic() {
        assert_eq!(sample().render(), sample().render());
    }

    #[test]
    fn apply_round_trips_idempotently() {
        let (dir, group) = setup();
        let cfg = sample();
        let svc = MockServices::default();
        let first = cfg.apply_with(dir.path(), &svc, &group).unwrap();
        assert_eq!(first.written.len(), 4, "first apply writes all four files");
        assert!(!first.restart_required);
        let second = cfg.apply_with(dir.path(), &svc, &group).unwrap();
        assert_eq!(second, ApplyOutcome::default(), "re-apply is a no-op");
        assert_eq!(*svc.calls.borrow(), vec![true]);
    }

    #[test]
    fn ups_conf_has_the_section() {
        let c = sample().render_ups_conf();
        assert!(c.contains("[serverups]"));
        assert!(c.contains("driver = usbhid-ups"));
        assert!(c.contains("port = auto"));
    }

    #[test]
    fn upsmon_conf_carries_thresholds_and_flag() {
        let c = sample().render_upsmon_conf();
        assert!(c.contains("MONITOR serverups@localhost 1 monuser secretref master"));
        assert!(c.contains("MINSUPPLIES 1"));
        assert!(c.contains("POWERDOWNFLAG /etc/killpower"));
        assert!(c.contains("SHUTDOWNCMD"));
    }

    #[test]
    fn monitor_only_client_renders_no_ups_sections() {
        let cfg = NutConfig {
            monitors: vec![MonitorDef {
                system: "serverups@upshost".into(),
                power_value: 1,
                username: "monuser".into(),
                password: "secretref".into(),
                role: "slave".into(),
            }],
            ..Default::default()
        };
        let ups = cfg.render_ups_conf();
        assert!(
            !ups.contains('['),
            "monitor-only client has no [ups] sections"
        );
        let mon = cfg.render_upsmon_conf();
        assert!(mon.contains("MONITOR serverups@upshost 1 monuser secretref slave"));
    }

    /// A config dir plus a separate group file mapping `nut` to the caller's
    /// gid, so staging's chown succeeds unprivileged.
    fn setup() -> (tempfile::TempDir, tempfile::TempPath) {
        let dir = tempfile::tempdir().unwrap();
        let gid = std::fs::metadata(dir.path()).unwrap().gid();
        let mut group = tempfile::NamedTempFile::new().unwrap();
        write!(group, "root:x:0:\nnut:x:{gid}:\n").unwrap();
        (dir, group.into_temp_path())
    }

    fn entries(dir: &Path) -> Vec<String> {
        let mut names: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    #[derive(Default)]
    struct MockServices {
        calls: std::cell::RefCell<Vec<bool>>,
    }

    impl ServiceControl for MockServices {
        fn restart(&self, server: bool) -> Restart {
            self.calls.borrow_mut().push(server);
            Restart::Done(vec!["mock".into()])
        }
    }

    #[test]
    fn atomic_write_leaves_no_temp_file() {
        let (dir, group) = setup();
        sample()
            .apply_with(dir.path(), &MockServices::default(), &group)
            .unwrap();
        assert_eq!(
            entries(dir.path()),
            vec!["ups.conf", "upsd.conf", "upsmon.conf", "upssched.conf"]
        );
    }

    #[test]
    fn new_file_is_0640_and_existing_mode_owner_preserved() {
        let (dir, group) = setup();
        let ups = dir.path().join("ups.conf");
        sample()
            .apply_with(dir.path(), &MockServices::default(), &group)
            .unwrap();
        assert_eq!(std::fs::metadata(&ups).unwrap().mode() & 0o7777, 0o640);

        std::fs::write(&ups, "stale").unwrap();
        std::fs::set_permissions(&ups, std::fs::Permissions::from_mode(0o600)).unwrap();
        let before = std::fs::metadata(&ups).unwrap();
        sample()
            .apply_with(dir.path(), &MockServices::default(), &group)
            .unwrap();
        let after = std::fs::metadata(&ups).unwrap();
        assert_eq!(after.mode() & 0o7777, 0o600);
        assert_eq!((after.uid(), after.gid()), (before.uid(), before.gid()));
        assert_ne!(after.ino(), before.ino(), "replaced via rename");
    }

    #[test]
    fn upsmon_conf_mode_is_clamped_to_0640() {
        let (dir, group) = setup();
        let mon = dir.path().join("upsmon.conf");
        std::fs::write(&mon, "stale").unwrap();
        std::fs::set_permissions(&mon, std::fs::Permissions::from_mode(0o664)).unwrap();
        sample()
            .apply_with(dir.path(), &MockServices::default(), &group)
            .unwrap();
        assert_eq!(std::fs::metadata(&mon).unwrap().mode() & 0o7777, 0o640);
    }

    #[test]
    fn unchanged_config_does_not_restart_services() {
        let (dir, group) = setup();
        sample()
            .apply_with(dir.path(), &MockServices::default(), &group)
            .unwrap();
        let svc = MockServices::default();
        sample().apply_with(dir.path(), &svc, &group).unwrap();
        assert!(svc.calls.borrow().is_empty());
    }

    #[test]
    fn monitor_only_config_restarts_client_only() {
        let (dir, group) = setup();
        let cfg = NutConfig {
            monitors: sample().monitors,
            ..Default::default()
        };
        let svc = MockServices::default();
        cfg.apply_with(dir.path(), &svc, &group).unwrap();
        assert_eq!(*svc.calls.borrow(), vec![false]);
    }

    #[test]
    fn rename_failure_rolls_back_and_skips_restart() {
        let (dir, group) = setup();
        std::fs::write(dir.path().join("ups.conf"), "old").unwrap();
        // A non-empty directory where upsd.conf must go makes its rename fail
        // after ups.conf has already been renamed.
        std::fs::create_dir(dir.path().join("upsd.conf")).unwrap();
        std::fs::write(dir.path().join("upsd.conf").join("x"), "").unwrap();
        let svc = MockServices::default();
        let err = sample().apply_with(dir.path(), &svc, &group).unwrap_err();
        assert!(err.contains("rename"), "{err}");
        assert!(svc.calls.borrow().is_empty());
        assert_eq!(
            std::fs::read_to_string(dir.path().join("ups.conf")).unwrap(),
            "old"
        );
        assert_eq!(entries(dir.path()), vec!["ups.conf", "upsd.conf"]);
    }

    #[test]
    fn missing_nut_group_stages_nothing() {
        let (dir, _) = setup();
        let group = dir.path().join("nogroup");
        std::fs::write(&group, "root:x:0:\n").unwrap();
        let svc = MockServices::default();
        let err = sample().apply_with(dir.path(), &svc, &group).unwrap_err();
        assert!(err.contains("no `nut` group"), "{err}");
        assert_eq!(entries(dir.path()), vec!["nogroup"]);
        assert!(svc.calls.borrow().is_empty());
    }

    #[test]
    fn symlinked_target_is_refused() {
        let (dir, group) = setup();
        std::fs::write(dir.path().join("real"), "x").unwrap();
        std::os::unix::fs::symlink(dir.path().join("real"), dir.path().join("upsd.conf")).unwrap();
        let err = sample()
            .apply_with(dir.path(), &MockServices::default(), &group)
            .unwrap_err();
        assert!(err.contains("symlink"), "{err}");
    }

    type Log = std::rc::Rc<std::cell::RefCell<Vec<String>>>;

    /// Services rooted at a fake `/`; `systemctl cat` succeeds for `units`.
    fn fake_services(root: &Path, units: &'static [&'static str]) -> (SystemServices, Log) {
        let log: Log = Default::default();
        let l = log.clone();
        let svc = SystemServices {
            root: root.to_path_buf(),
            run: Box::new(move |bin, args| {
                if bin == "systemctl" && args[0] == "cat" {
                    return if units.contains(&args[1]) {
                        Ok(String::new())
                    } else {
                        Err("no such unit".into())
                    };
                }
                l.borrow_mut().push(format!("{bin} {}", args.join(" ")));
                Ok(String::new())
            }),
        };
        (svc, log)
    }

    fn touch(root: &Path, rel: &str) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, "").unwrap();
    }

    #[test]
    fn unraid_restarts_via_rc_nut() {
        let root = tempfile::tempdir().unwrap();
        touch(root.path(), RC_NUT);
        std::fs::create_dir_all(root.path().join("run/systemd/system")).unwrap();
        touch(root.path(), "boot/config/plugins/nut-dw/nut-dw.cfg");
        std::fs::write(
            root.path().join("boot/config/plugins/nut-dw/nut-dw.cfg"),
            "MANUAL=\"enable\"\n",
        )
        .unwrap();
        let (svc, log) = fake_services(root.path(), &[]);
        assert_eq!(detect_init(root.path()), InitSystem::Unraid);
        assert!(matches!(svc.restart(false), Restart::Done(_)));
        let rc = root.path().join(RC_NUT);
        assert_eq!(*log.borrow(), vec![format!("{} restart", rc.display())]);
    }

    #[test]
    fn unraid_gui_mode_does_not_restart() {
        let root = tempfile::tempdir().unwrap();
        touch(root.path(), RC_NUT);
        touch(root.path(), "boot/config/plugins/nut/nut.cfg");
        std::fs::write(
            root.path().join("boot/config/plugins/nut/nut.cfg"),
            "MANUAL=\"disable\"\n",
        )
        .unwrap();
        let (svc, log) = fake_services(root.path(), &[]);
        assert!(matches!(svc.restart(true), Restart::Unavailable(_)));
        assert!(log.borrow().is_empty());
    }

    #[test]
    fn systemd_monitor_only_restarts_only_upsmon() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("run/systemd/system")).unwrap();
        let (svc, log) = fake_services(root.path(), &["nut-monitor.service"]);
        assert!(matches!(svc.restart(false), Restart::Done(_)));
        assert_eq!(*log.borrow(), vec!["systemctl restart nut-monitor.service"]);
    }

    #[test]
    fn systemd_server_restarts_existing_units_in_order() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("run/systemd/system")).unwrap();
        let (svc, log) = fake_services(
            root.path(),
            &[
                "nut-driver.target",
                "nut-server.service",
                "nut-monitor.service",
            ],
        );
        assert!(matches!(svc.restart(true), Restart::Done(_)));
        assert_eq!(
            *log.borrow(),
            vec![
                "systemctl restart nut-driver.target",
                "systemctl restart nut-server.service",
                "systemctl restart nut-monitor.service",
            ]
        );
    }

    #[test]
    fn openrc_restarts_present_scripts() {
        let root = tempfile::tempdir().unwrap();
        touch(root.path(), "sbin/openrc-run");
        touch(root.path(), "etc/init.d/nut-upsd");
        touch(root.path(), "etc/init.d/nut-upsmon");
        let (svc, log) = fake_services(root.path(), &[]);
        assert!(matches!(svc.restart(true), Restart::Done(_)));
        assert_eq!(
            *log.borrow(),
            vec![
                "rc-service nut-upsd restart",
                "rc-service nut-upsmon restart"
            ]
        );
        log.borrow_mut().clear();
        assert!(matches!(svc.restart(false), Restart::Done(_)));
        assert_eq!(*log.borrow(), vec!["rc-service nut-upsmon restart"]);
    }

    #[test]
    fn unraid_missing_manual_key_does_not_restart() {
        let root = tempfile::tempdir().unwrap();
        touch(root.path(), RC_NUT);
        touch(root.path(), "boot/config/plugins/nut/nut.cfg");
        std::fs::write(
            root.path().join("boot/config/plugins/nut/nut.cfg"),
            "MODE=\"netserver\"\n",
        )
        .unwrap();
        let (svc, log) = fake_services(root.path(), &[]);
        assert!(matches!(svc.restart(true), Restart::Unavailable(_)));
        assert!(log.borrow().is_empty());
    }

    #[test]
    fn unraid_checks_every_cfg_and_tolerates_spaces() {
        let root = tempfile::tempdir().unwrap();
        touch(root.path(), RC_NUT);
        touch(root.path(), "boot/config/plugins/nut-dw/nut-dw.cfg");
        touch(root.path(), "boot/config/plugins/nut/nut.cfg");
        std::fs::write(
            root.path().join("boot/config/plugins/nut-dw/nut-dw.cfg"),
            "MANUAL = \"enable\"\n",
        )
        .unwrap();
        std::fs::write(
            root.path().join("boot/config/plugins/nut/nut.cfg"),
            "MANUAL=\"disable\"\n",
        )
        .unwrap();
        assert!(unraid_gui_mode(root.path()).is_some());
        std::fs::write(
            root.path().join("boot/config/plugins/nut/nut.cfg"),
            "MANUAL =enable\n",
        )
        .unwrap();
        assert_eq!(unraid_gui_mode(root.path()), None);
    }

    #[test]
    fn systemd_without_units_says_no_units_found() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("run/systemd/system")).unwrap();
        let (svc, _) = fake_services(root.path(), &[]);
        match svc.restart(true) {
            Restart::Unavailable(m) => assert!(m.contains("no NUT units found"), "{m}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn config_without_monitors_is_refused() {
        let (dir, group) = setup();
        let svc = MockServices::default();
        let cfg = NutConfig {
            monitors: vec![],
            ..sample()
        };
        let err = cfg.apply_with(dir.path(), &svc, &group).unwrap_err();
        assert!(err.contains("MONITOR"), "{err}");
        assert!(entries(dir.path()).is_empty());
        assert!(svc.calls.borrow().is_empty());
    }

    #[test]
    fn dropping_an_on_disk_ups_is_refused_unless_listed() {
        let (dir, group) = setup();
        sample()
            .apply_with(dir.path(), &MockServices::default(), &group)
            .unwrap();
        let before = std::fs::read_to_string(dir.path().join("ups.conf")).unwrap();
        let svc = MockServices::default();
        let client = NutConfig {
            monitors: sample().monitors,
            ..Default::default()
        };
        let err = client.apply_with(dir.path(), &svc, &group).unwrap_err();
        assert!(err.contains("serverups"), "{err}");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("ups.conf")).unwrap(),
            before
        );
        assert!(svc.calls.borrow().is_empty());

        let client = NutConfig {
            remove_upses: vec!["serverups".into()],
            ..client
        };
        client.apply_with(dir.path(), &svc, &group).unwrap();
        // The host drove UPS hardware before this apply, so the server stack restarts.
        assert_eq!(*svc.calls.borrow(), vec![true]);
    }

    #[test]
    fn read_existing_round_trips_topology() {
        let (dir, group) = setup();
        let mut cfg = sample();
        cfg.upses[0].extra = vec![("pollinterval".into(), "5".into())];
        cfg.apply_with(dir.path(), &MockServices::default(), &group)
            .unwrap();
        let read = NutConfig::read_existing(dir.path());
        assert_eq!(read, cfg);
    }

    #[test]
    fn rollback_restores_exact_original_mode() {
        let (dir, group) = setup();
        let mon = dir.path().join("upsmon.conf");
        std::fs::write(&mon, "old").unwrap();
        std::fs::set_permissions(&mon, std::fs::Permissions::from_mode(0o664)).unwrap();
        std::fs::create_dir(dir.path().join("upssched.conf")).unwrap();
        std::fs::write(dir.path().join("upssched.conf").join("x"), "").unwrap();
        sample()
            .apply_with(dir.path(), &MockServices::default(), &group)
            .unwrap_err();
        assert_eq!(std::fs::read_to_string(&mon).unwrap(), "old");
        assert_eq!(std::fs::metadata(&mon).unwrap().mode() & 0o7777, 0o664);
    }

    #[test]
    fn no_init_system_is_unavailable() {
        let root = tempfile::tempdir().unwrap();
        let (svc, log) = fake_services(root.path(), &[]);
        assert!(matches!(svc.restart(true), Restart::Unavailable(_)));
        assert!(log.borrow().is_empty());
    }
}
