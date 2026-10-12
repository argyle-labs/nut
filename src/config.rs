//! NUT has no config API: its conf files are the official configuration interface, so file
//! edits here are the documented exception to the official-API rule.
//!
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
    /// On-disk `MONITOR` systems (`ups@host`) the caller intends to drop. Apply
    /// refuses to drop any on-disk system not listed here.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remove_monitors: Vec<String>,
    /// Unmodelled `ups.conf` global lines (before the first section), kept verbatim.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ups_globals: Vec<String>,
    /// Unmodelled `upsd.conf` lines (everything but `LISTEN`), kept verbatim.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub upsd_extra: Vec<String>,
    /// Unmodelled `upsmon.conf` lines, kept verbatim.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub upsmon_extra: Vec<String>,
    /// Operator `NOTIFYFLAG` flags for the managed events, merged onto ours as
    /// `(event, "FLAG+FLAG")`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notify_flags: Vec<(String, String)>,
    /// An operator-written `upssched.conf`, kept verbatim instead of the managed one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upssched_custom: Option<String>,
    /// Render the managed `upssched.conf` even when `upssched_custom` is set.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub replace_custom_upssched: bool,
}

/// One UPS section in `ups.conf` (`[name]` + `driver`/`port`/…).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct UpsDef {
    pub name: String,
    pub driver: String,
    pub port: String,
    #[serde(default)]
    pub description: String,
    /// Extra driver options rendered as `key = value` lines; an empty value
    /// renders the key alone (a bare flag like `nolock`, or a `#` comment line).
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
        Self::for_host(false)
    }
}

impl NotifySettings {
    /// Defaults for an Unraid host (flash under `/boot`) or any other (`/var/log/nut`).
    pub fn for_host(unraid: bool) -> Self {
        let dir = if unraid {
            "/boot/config/plugins/nut"
        } else {
            "/var/log/nut"
        };
        Self {
            notify_cmd: "/usr/local/bin/orca-nut-notify".to_string(),
            pipe: "/var/run/nut/upssched.pipe".to_string(),
            lock: "/var/run/nut/upssched.lock".to_string(),
            log_path: format!("{dir}/events.log"),
            // Generic placeholder — no real fleet host. Overridden at deploy.
            remote_syslog: "syslog-collector.example.invalid:514".to_string(),
            onbattery_capture_cmd: format!(
                "/usr/local/bin/orca diagnostics diagnose >> {dir}/onbattery.json"
            ),
        }
    }
}

const MANAGED_HEADER: &str = "# Managed by orca (nut plugin). Do not edit by hand.\n";

const MANAGED_EVENTS: [&str; 6] = [
    "ONLINE", "ONBATT", "LOWBATT", "COMMBAD", "COMMOK", "SHUTDOWN",
];

const MANAGED_FLAGS: [&str; 3] = ["SYSLOG", "WALL", "EXEC"];

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
    /// read as empty. Unmodelled lines and comment lines are carried as
    /// passthrough; only trailing comments on modelled lines (`MONITOR`,
    /// `LISTEN`, `[section]`, `driver = …`) are dropped. A modelled value that
    /// does not parse is an error rather than a silent default.
    /// `unraid` picks the notify defaults used where the files don't set them.
    pub fn read_existing(dir: &Path, unraid: bool) -> Result<Self, String> {
        let read = |name: &str| read_or_empty(&dir.join(name));
        let ups = read("ups.conf")?;
        let upsd = read("upsd.conf")?;
        let upsmon = read("upsmon.conf")?;
        let upssched = read("upssched.conf")?;
        let managed_sched = upssched.starts_with(MANAGED_HEADER);
        Ok(Self {
            upses: parse_ups_conf(&ups)?,
            listen: parse_upsd_conf(&upsd)?,
            monitors: parse_monitors(&upsmon)?,
            upsmon: crate::checks::read_upsmon(dir)?.unwrap_or_default(),
            notify: parse_notify(&upsmon, managed_sched.then_some(upssched.as_str()), unraid)?,
            ups_globals: parse_ups_globals(&ups),
            upsd_extra: parse_upsd_extra(&upsd),
            upsmon_extra: parse_upsmon_extra(&upsmon),
            notify_flags: parse_notify_flags(&upsmon),
            upssched_custom: (!upssched.trim().is_empty() && !managed_sched).then_some(upssched),
            ..Default::default()
        })
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
        self.check_values()
            .map_err(|e| format!("refusing to apply: {e}"))?;
        let ignored: Vec<&str> = self
            .notify_flags
            .iter()
            .filter(|(e, f)| {
                MANAGED_EVENTS.contains(&e.as_str()) && f.split('+').any(|f| f == "IGNORE")
            })
            .map(|(e, _)| e.as_str())
            .collect();
        if !ignored.is_empty() {
            let mut cfg = self.clone();
            for (event, flags) in &mut cfg.notify_flags {
                if !MANAGED_EVENTS.contains(&event.as_str()) {
                    continue;
                }
                *flags = flags
                    .split('+')
                    .filter(|f| *f != "IGNORE")
                    .collect::<Vec<_>>()
                    .join("+");
            }
            cfg.notify_flags.retain(|(_, f)| !f.is_empty());
            let mut out = cfg.apply_with(dir, services, group_file)?;
            let note = format!(
                "dropped IGNORE from NOTIFYFLAG {}: orca relies on these events for logging and \
                 shutdown",
                ignored.join(", ")
            );
            out.message = if out.message.is_empty() {
                note
            } else {
                format!("{note}; {}", out.message)
            };
            return Ok(out);
        }
        if self.monitors.is_empty() {
            return Err(
                "refusing to apply: config has no MONITOR lines, so upsmon would \
                        protect nothing"
                    .to_string(),
            );
        }
        let incomplete: Vec<&str> = self
            .upses
            .iter()
            .filter(|u| u.driver.trim().is_empty() || u.port.trim().is_empty())
            .map(|u| u.name.as_str())
            .collect();
        if !incomplete.is_empty() {
            return Err(format!(
                "refusing to apply: UPS {} has no driver or port",
                incomplete.join(", ")
            ));
        }
        let ups_text = read_or_empty(&dir.join("ups.conf"))?;
        let on_disk = parse_ups_conf(&ups_text).map_err(|e| format!("refusing to apply: {e}"))?;
        let raw_sections = ups_text
            .lines()
            .filter(|l| strip_comment(l).trim().starts_with('['))
            .count();
        if raw_sections != on_disk.len() {
            return Err(format!(
                "refusing to apply: ups.conf has {raw_sections} [section] headers but only {} \
                 parsed; fix it by hand first",
                on_disk.len()
            ));
        }
        let upsmon_text = read_or_empty(&dir.join("upsmon.conf"))?;
        let disk_monitors: Vec<String> = upsmon_text
            .lines()
            .map(tokenize)
            .filter(|t| t.first().is_some_and(|t| t == "MONITOR"))
            .map(|t| t.get(1).cloned().unwrap_or_default())
            .collect();
        let both: Vec<&str> = self
            .monitors
            .iter()
            .map(|m| m.system.as_str())
            .filter(|s| self.remove_monitors.iter().any(|r| r == s))
            .collect();
        if !both.is_empty() {
            return Err(format!(
                "refusing to apply: {} is in both monitors and remove_monitors",
                both.join(", ")
            ));
        }
        let unknown_monitors: Vec<&str> = self
            .remove_monitors
            .iter()
            .map(String::as_str)
            .filter(|r| !disk_monitors.iter().any(|m| m == r))
            .collect();
        if !unknown_monitors.is_empty() {
            return Err(format!(
                "refusing to apply: remove_monitors lists {} which is not in upsmon.conf",
                unknown_monitors.join(", ")
            ));
        }
        let dropped_monitors: Vec<&str> = disk_monitors
            .iter()
            .map(String::as_str)
            .filter(|d| !self.monitors.iter().any(|m| m.system == *d))
            .filter(|d| !self.remove_monitors.iter().any(|r| r == d))
            .collect();
        if !dropped_monitors.is_empty() {
            return Err(format!(
                "refusing to apply: it would drop MONITOR {} from upsmon.conf; list them in \
                 remove_monitors to remove them",
                dropped_monitors.join(", ")
            ));
        }
        let missing_local: Vec<&str> = self
            .monitors
            .iter()
            .filter_map(|m| {
                let (ups, host) = m.system.split_once('@')?;
                let host = host.split(':').next().unwrap_or(host);
                matches!(host, "localhost" | "127.0.0.1").then_some(ups)
            })
            .filter(|ups| !self.upses.iter().any(|u| u.name == *ups))
            .collect();
        if !missing_local.is_empty() {
            return Err(format!(
                "refusing to apply: local MONITOR refers to UPS {} not defined in upses",
                missing_local.join(", ")
            ));
        }
        let unknown: Vec<&str> = self
            .remove_upses
            .iter()
            .map(String::as_str)
            .filter(|r| !on_disk.iter().any(|u| u.name == *r))
            .collect();
        if !unknown.is_empty() {
            return Err(format!(
                "refusing to apply: remove_upses lists {} which is not in ups.conf",
                unknown.join(", ")
            ));
        }
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

    /// Every value apply would render must be free of control characters (a
    /// newline would inject a directive); values rendered unquoted must be a
    /// single bare NUT token.
    pub(crate) fn check_values(&self) -> Result<(), String> {
        let mut values: Vec<(&str, &str)> = Vec::new();
        for u in &self.upses {
            values.extend([
                ("ups name", u.name.as_str()),
                ("ups driver", &u.driver),
                ("ups port", &u.port),
                ("ups description", &u.description),
            ]);
            for (k, v) in &u.extra {
                values.extend([("ups option", k.as_str()), ("ups option", v)]);
            }
        }
        values.extend(self.listen.iter().map(|l| ("LISTEN", l.address.as_str())));
        for m in &self.monitors {
            values.extend([
                ("MONITOR", m.system.as_str()),
                ("MONITOR", &m.username),
                ("MONITOR", &m.password),
                ("MONITOR", &m.role),
            ]);
        }
        let (s, n) = (&self.upsmon, &self.notify);
        values.extend([
            ("SHUTDOWNCMD", s.shutdown_cmd.as_str()),
            ("POWERDOWNFLAG", &s.power_down_flag),
            ("NOTIFYCMD", &n.notify_cmd),
            ("PIPEFN", &n.pipe),
            ("LOCKFN", &n.lock),
            ("log path", &n.log_path),
            ("remote syslog", &n.remote_syslog),
            ("onbattery capture", &n.onbattery_capture_cmd),
        ]);
        if n.onbattery_capture_cmd.trim().is_empty() {
            return Err("onbattery capture command must not be empty".to_string());
        }
        values.extend(
            self.ups_globals
                .iter()
                .map(|l| ("ups.conf line", l.as_str())),
        );
        values.extend(
            self.upsd_extra
                .iter()
                .map(|l| ("upsd.conf line", l.as_str())),
        );
        values.extend(
            self.upsmon_extra
                .iter()
                .map(|l| ("upsmon.conf line", l.as_str())),
        );
        for (e, f) in &self.notify_flags {
            values.extend([("NOTIFYFLAG", e.as_str()), ("NOTIFYFLAG", f)]);
        }
        for (what, v) in values {
            check_value(what, v)?;
        }
        let mut words: Vec<(&str, &str)> = Vec::new();
        for u in &self.upses {
            words.push(("ups name", &u.name));
            // A bare key starting with `#` is a carried-over comment line.
            words.extend(
                u.extra
                    .iter()
                    .filter(|(k, v)| !(v.is_empty() && k.starts_with('#')))
                    .map(|(k, _)| ("ups option", k.as_str())),
            );
        }
        words.extend(self.listen.iter().map(|l| ("LISTEN", l.address.as_str())));
        for m in &self.monitors {
            words.extend([
                ("MONITOR system", m.system.as_str()),
                ("MONITOR role", &m.role),
            ]);
        }
        for (what, v) in words {
            if v.is_empty()
                || v.contains(|c: char| c.is_whitespace() || matches!(c, '"' | '#' | '\\'))
            {
                return Err(format!(
                    "{what} value {v:?} must be non-empty and free of whitespace, '\"', '#' and '\\'"
                ));
            }
        }
        let flag_list = |s: &str| {
            s.split('+')
                .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_uppercase()))
        };
        for (event, flags) in &self.notify_flags {
            if !flag_list(event) || !flag_list(flags) {
                return Err(format!(
                    "NOTIFYFLAG {event:?} {flags:?} must be upper-case words joined by '+'"
                ));
            }
        }
        Ok(())
    }

    fn render_ups_conf(&self) -> String {
        let mut out = String::from(MANAGED_HEADER);
        for line in &self.ups_globals {
            out.push_str(&format!("{line}\n"));
        }
        for ups in &self.upses {
            out.push_str(&format!("\n[{}]\n", ups.name));
            out.push_str(&format!("    driver = {}\n", quote_token(&ups.driver)));
            out.push_str(&format!("    port = {}\n", quote_token(&ups.port)));
            if !ups.description.is_empty() {
                out.push_str(&format!("    desc = {}\n", quote_token(&ups.description)));
            }
            for (k, v) in &ups.extra {
                if v.is_empty() {
                    out.push_str(&format!("    {k}\n"));
                } else {
                    out.push_str(&format!("    {k} = {}\n", quote_token(v)));
                }
            }
        }
        out
    }

    fn render_upsd_conf(&self) -> String {
        let mut out = String::from(MANAGED_HEADER);
        for l in &self.listen {
            match l.port {
                Some(p) => out.push_str(&format!("LISTEN {} {p}\n", l.address)),
                None => out.push_str(&format!("LISTEN {}\n", l.address)),
            }
        }
        for line in &self.upsd_extra {
            out.push_str(&format!("{line}\n"));
        }
        out
    }

    fn render_upsmon_conf(&self) -> String {
        let s = &self.upsmon;
        let n = &self.notify;
        let mut out = String::from(MANAGED_HEADER);
        for m in &self.monitors {
            out.push_str(&format!(
                "MONITOR {} {} {} {} {}\n",
                m.system,
                m.power_value,
                quote_token(&m.username),
                quote_token(&m.password),
                m.role
            ));
        }
        out.push_str(&format!("MINSUPPLIES {}\n", s.min_supplies));
        out.push_str(&format!("SHUTDOWNCMD {}\n", quote_token(&s.shutdown_cmd)));
        // read_upsmon derives kill_power from POWERDOWNFLAG's presence.
        if s.kill_power {
            out.push_str(&format!(
                "POWERDOWNFLAG {}\n",
                quote_token(&s.power_down_flag)
            ));
        }
        out.push_str(&format!("HOSTSYNC {}\n", s.host_sync));
        out.push_str(&format!("DEADTIME {}\n", s.dead_time));
        // Route every event through upssched so logging + capture run.
        out.push_str(&format!("NOTIFYCMD {}\n", quote_token(&n.notify_cmd)));
        for event in MANAGED_EVENTS {
            let mut flags = MANAGED_FLAGS.to_vec();
            for (_, extra) in self.notify_flags.iter().filter(|(e, _)| e == event) {
                for f in extra.split('+') {
                    if !f.is_empty() && !flags.contains(&f) {
                        flags.push(f);
                    }
                }
            }
            out.push_str(&format!("NOTIFYFLAG {event} {}\n", flags.join("+")));
        }
        for line in &self.upsmon_extra {
            out.push_str(&format!("{line}\n"));
        }
        out
    }

    fn render_upssched_conf(&self) -> String {
        if let Some(custom) = self
            .upssched_custom
            .as_ref()
            .filter(|_| !self.replace_custom_upssched)
        {
            return custom.clone();
        }
        let n = &self.notify;
        let mut out = String::from(MANAGED_HEADER);
        out.push_str(&format!("CMDSCRIPT {}\n", quote_token(&n.notify_cmd)));
        out.push_str(&format!("PIPEFN {}\n", quote_token(&n.pipe)));
        out.push_str(&format!("LOCKFN {}\n", quote_token(&n.lock)));
        // Persistent-event-log: every event logs to flash + remote immediately.
        for event in MANAGED_EVENTS {
            out.push_str(&format!(
                "AT {event} * EXECUTE log-{}\n",
                event.to_lowercase()
            ));
        }
        // On-battery diagnostics capture runs before shutdown proceeds.
        out.push_str("AT ONBATT * EXECUTE onbattery-capture\n");
        // Persist the log target + remote + capture command as comments the
        // command handler reads (single source of truth for the handler).
        out.push_str(&format!("# orca-log-path {}\n", quote_token(&n.log_path)));
        out.push_str(&format!(
            "# orca-remote-syslog {}\n",
            quote_token(&n.remote_syslog)
        ));
        // Raw text: the handler reads this comment payload verbatim, not as a NUT token.
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
        // The cfg is sourced by bash, so the last assignment wins.
        let manual = text
            .lines()
            .filter_map(|l| {
                let (k, v) = l.split_once('=')?;
                (k.trim() == "MANUAL").then(|| v.trim().trim_matches('"').to_string())
            })
            .next_back();
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

/// `line` with any `#` comment outside double quotes removed.
pub(crate) fn strip_comment(line: &str) -> &str {
    let (mut quoted, mut escaped) = (false, false);
    for (i, c) in line.char_indices() {
        match c {
            _ if escaped => escaped = false,
            '\\' => escaped = true,
            '"' => quoted = !quoted,
            '#' if !quoted => return &line[..i],
            _ => {}
        }
    }
    line
}

/// NUT-style tokens: whitespace-separated, `"..."` groups, `\` escapes, `#` comments.
pub(crate) fn tokenize(line: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut cur: Option<String> = None;
    let (mut quoted, mut chars) = (false, line.chars());
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                if let Some(n) = chars.next() {
                    cur.get_or_insert_with(String::new).push(n);
                }
            }
            '"' => {
                quoted = !quoted;
                cur.get_or_insert_with(String::new);
            }
            '#' if !quoted => break,
            c if c.is_whitespace() && !quoted => tokens.extend(cur.take()),
            c => cur.get_or_insert_with(String::new).push(c),
        }
    }
    tokens.extend(cur);
    tokens
}

/// `Err` naming `what` when `v` holds a control character other than tab.
pub(crate) fn check_value(what: &str, v: &str) -> Result<(), String> {
    if v.contains(|c: char| c.is_control() && c != '\t') {
        return Err(format!("{what} value {v:?} contains a control character"));
    }
    Ok(())
}

/// Contents of `path`; empty when it does not exist, any other read error propagates.
pub(crate) fn read_or_empty(path: &Path) -> Result<String, String> {
    match std::fs::read_to_string(path) {
        Ok(t) => Ok(t),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(format!("read {}: {e}", path.display())),
    }
}

/// `s` quoted and escaped if [`tokenize`] would otherwise split or mangle it.
fn quote_token(s: &str) -> String {
    if !s.is_empty() && !s.contains(|c: char| c.is_whitespace() || matches!(c, '"' | '#' | '\\')) {
        return s.to_string();
    }
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Non-blank lines of `text`, trimmed, comments kept, minus the managed header.
fn raw_lines(text: &str) -> impl Iterator<Item = &str> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && *l != MANAGED_HEADER.trim_end())
}

/// The single NUT token in `v`, or an error naming `what`.
fn one_token(v: &str, what: &str) -> Result<String, String> {
    match tokenize(v).as_slice() {
        [t] => Ok(t.clone()),
        _ => Err(format!("cannot parse {what} value `{}`", v.trim())),
    }
}

/// `[name]` sections of `ups.conf`; global directives are in [`parse_ups_globals`].
/// Comment lines inside a section are kept as bare `extra` keys.
fn parse_ups_conf(text: &str) -> Result<Vec<UpsDef>, String> {
    let mut upses: Vec<UpsDef> = Vec::new();
    for raw in raw_lines(text) {
        let line = strip_comment(raw).trim();
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
        if line.is_empty() {
            ups.extra.push((raw.to_string(), String::new()));
            continue;
        }
        let what = format!("ups.conf [{}]", ups.name);
        let (k, v) = match line.split_once('=') {
            Some((k, v)) => (k.trim(), one_token(v, &format!("{what} {}", k.trim()))?),
            None => (line, String::new()),
        };
        if k.is_empty() || k.contains(char::is_whitespace) {
            return Err(format!("cannot parse {what} line `{line}`"));
        }
        match k {
            "driver" => ups.driver = v,
            "port" => ups.port = v,
            "desc" => ups.description = v,
            _ => ups.extra.push((k.to_string(), v)),
        }
    }
    if let Some(u) = upses
        .iter()
        .find(|u| u.driver.is_empty() || u.port.is_empty())
    {
        return Err(format!("ups.conf [{}] has no driver or port", u.name));
    }
    Ok(upses)
}

fn parse_ups_globals(text: &str) -> Vec<String> {
    raw_lines(text)
        .take_while(|l| !strip_comment(l).trim().starts_with('['))
        .map(str::to_string)
        .collect()
}

fn parse_upsd_conf(text: &str) -> Result<Vec<ListenDef>, String> {
    let mut listen = Vec::new();
    for l in text.lines() {
        let f = tokenize(l);
        if f.first().map(String::as_str) != Some("LISTEN") {
            continue;
        }
        let (address, port) = match f.as_slice() {
            [_, a] => (a.clone(), None),
            [_, a, p] => {
                let port = p
                    .parse()
                    .map_err(|_| format!("upsd.conf LISTEN {a} has a non-numeric port `{p}`"))?;
                (a.clone(), Some(port))
            }
            _ => return Err(format!("cannot parse upsd.conf line `{}`", l.trim())),
        };
        listen.push(ListenDef { address, port });
    }
    Ok(listen)
}

fn parse_upsd_extra(text: &str) -> Vec<String> {
    raw_lines(text)
        .filter(|l| tokenize(l).first().map(String::as_str) != Some("LISTEN"))
        .map(str::to_string)
        .collect()
}

fn parse_monitors(text: &str) -> Result<Vec<MonitorDef>, String> {
    let mut monitors = Vec::new();
    for l in text.lines() {
        let f = tokenize(l);
        if f.len() != 6 || f[0] != "MONITOR" {
            continue;
        }
        let power_value = f[2].parse().map_err(|_| {
            format!(
                "upsmon.conf MONITOR {} has a non-numeric power value `{}`",
                f[1], f[2]
            )
        })?;
        monitors.push(MonitorDef {
            system: f[1].clone(),
            power_value,
            username: f[3].clone(),
            password: f[4].clone(),
            role: f[5].clone(),
        });
    }
    Ok(monitors)
}

/// `upsmon.conf` lines [`NutConfig::render_upsmon_conf`] does not itself emit.
fn parse_upsmon_extra(text: &str) -> Vec<String> {
    raw_lines(text)
        .filter(|l| {
            let f = tokenize(l);
            match f.first().map(String::as_str) {
                Some(
                    "MONITOR" | "MINSUPPLIES" | "SHUTDOWNCMD" | "POWERDOWNFLAG" | "HOSTSYNC"
                    | "DEADTIME" | "NOTIFYCMD",
                ) => false,
                Some("NOTIFYFLAG") => !f
                    .get(1)
                    .is_some_and(|e| MANAGED_EVENTS.contains(&e.as_str())),
                _ => true,
            }
        })
        .map(str::to_string)
        .collect()
}

/// Operator flags on managed-event `NOTIFYFLAG` lines beyond [`MANAGED_FLAGS`].
fn parse_notify_flags(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|l| {
            let f = tokenize(l);
            let [kw, event, flags] = f.as_slice() else {
                return None;
            };
            if kw != "NOTIFYFLAG" || !MANAGED_EVENTS.contains(&event.as_str()) {
                return None;
            }
            let mut extra: Vec<&str> = Vec::new();
            for f in flags.split('+') {
                if !f.is_empty() && !MANAGED_FLAGS.contains(&f) && !extra.contains(&f) {
                    extra.push(f);
                }
            }
            (!extra.is_empty()).then(|| (event.clone(), extra.join("+")))
        })
        .collect()
}

/// [`NotifySettings`] from `upsmon.conf`'s `NOTIFYCMD` and, when orca manages
/// it, `upssched.conf`'s `PIPEFN`/`LOCKFN` and `# orca-*` markers.
fn parse_notify(
    upsmon: &str,
    managed_upssched: Option<&str>,
    unraid: bool,
) -> Result<NotifySettings, String> {
    let mut n = NotifySettings::for_host(unraid);
    for l in upsmon.lines() {
        let f = tokenize(l);
        if f.first().map(String::as_str) == Some("NOTIFYCMD") {
            let [_, cmd] = f.as_slice() else {
                return Err(format!("cannot parse upsmon.conf line `{}`", l.trim()));
            };
            n.notify_cmd = cmd.clone();
        }
    }
    let Some(sched) = managed_upssched else {
        return Ok(n);
    };
    for l in sched.lines() {
        match tokenize(l).as_slice() {
            [k, v] if k == "PIPEFN" => n.pipe = v.clone(),
            [k, v] if k == "LOCKFN" => n.lock = v.clone(),
            _ => {}
        }
    }
    for (key, field) in [
        ("orca-log-path", &mut n.log_path),
        ("orca-remote-syslog", &mut n.remote_syslog),
    ] {
        if let Some(v) = marker_value(sched, key).filter(|v| !v.is_empty()) {
            *field = v;
        }
    }
    if let Some(v) = marker_raw(sched, "orca-onbattery-capture").filter(|v| !v.is_empty()) {
        n.onbattery_capture_cmd = v.to_string();
    }
    Ok(n)
}

/// Read the value of an `# orca-<key> <value>` marker comment the managed
/// `upssched.conf` embeds (the handler's single source of truth).
/// A value we quoted is unquoted; a legacy value with a `\` or more than one
/// token is taken as raw text.
pub(crate) fn marker_value(upssched: &str, key: &str) -> Option<String> {
    marker_raw(upssched, key).map(|v| match tokenize(v).as_slice() {
        [t] if v.starts_with('"') || !v.contains('\\') => t.clone(),
        _ => v.to_string(),
    })
}

/// The trimmed text after an `# orca-<key> ` marker prefix.
fn marker_raw<'a>(upssched: &'a str, key: &str) -> Option<&'a str> {
    let prefix = format!("# {key} ");
    upssched
        .lines()
        .find_map(|l| l.trim_start().strip_prefix(&prefix))
        .map(str::trim)
}

/// `"; needs privilege"` when `e` is a permission error, else empty.
pub(crate) fn privilege_hint(e: &str) -> &'static str {
    if e.contains("(os error 13)") || e.contains("(os error 1)") {
        "; needs privilege"
    } else {
        ""
    }
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
            ..Default::default()
        }
    }

    fn remote_monitors() -> Vec<MonitorDef> {
        let mut m = sample().monitors;
        m[0].system = "serverups@10.0.0.2".into();
        m
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
            monitors: remote_monitors(),
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
            monitors: remote_monitors(),
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
            remove_monitors: vec!["serverups@localhost".into()],
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
        let read = NutConfig::read_existing(dir.path(), false).unwrap();
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

    const FULL_UPS: &str = "maxretry = 3\npollinterval = 2 # fast\nuser = nut\n\n\
[serverups] # rack\n  driver = usbhid-ups\n  port = auto\n  desc = \"Rack UPS\"\n  \
ignorelb\n  nolock\n  override.battery.charge.low = 30\n\n[other]\n  driver = snmp-ups\n  \
port = 10.0.0.5\n  pollonly\n";
    const FULL_UPSD: &str = "LISTEN 0.0.0.0 3493\nMAXAGE 25\nCERTFILE /etc/nut/cert.pem\n";
    const FULL_UPSMON: &str = "RUN_AS_USER root\n\
MONITOR serverups@localhost 1 monuser \"pass with spaces\" master # local\n\
MONITOR other@127.0.0.1 1 monuser secret master\nMINSUPPLIES 1\n\
SHUTDOWNCMD \"/sbin/shutdown -h +0\"\nPOWERDOWNFLAG /etc/killpower\nPOLLFREQ 5\n\
FINALDELAY 5\nNOTIFYMSG ONBATT \"on battery\"\nNOTIFYFLAG LOWBATT SYSLOG\n\
NOTIFYFLAG REPLBATT SYSLOG+WALL\n";
    const FULL_UPSSCHED: &str = "CMDSCRIPT /usr/bin/custom\nAT ONBATT * START-TIMER x 30\n";

    fn write_full(dir: &Path) {
        for (n, t) in [
            ("ups.conf", FULL_UPS),
            ("upsd.conf", FULL_UPSD),
            ("upsmon.conf", FULL_UPSMON),
            ("upssched.conf", FULL_UPSSCHED),
        ] {
            std::fs::write(dir.join(n), t).unwrap();
        }
    }

    #[test]
    fn full_config_round_trips_without_loss() {
        let (dir, group) = setup();
        write_full(dir.path());
        let before = NutConfig::read_existing(dir.path(), false).unwrap();
        assert_eq!(before.upses.len(), 2);
        assert_eq!(before.monitors.len(), 2);
        assert_eq!(before.monitors[0].password, "pass with spaces");
        assert!(before.upses[0]
            .extra
            .contains(&("ignorelb".into(), String::new())));
        let svc = MockServices::default();
        before.apply_with(dir.path(), &svc, &group).unwrap();
        let after = NutConfig::read_existing(dir.path(), false).unwrap();
        assert_eq!(after, before);
        for line in [
            "maxretry = 3",
            "pollinterval = 2",
            "user = nut",
            "    ignorelb\n",
            "    pollonly\n",
        ] {
            let ups = std::fs::read_to_string(dir.path().join("ups.conf")).unwrap();
            assert!(ups.contains(line), "{line}\n{ups}");
        }
        let upsd = std::fs::read_to_string(dir.path().join("upsd.conf")).unwrap();
        assert!(upsd.contains("MAXAGE 25") && upsd.contains("CERTFILE /etc/nut/cert.pem"));
        let upsmon = std::fs::read_to_string(dir.path().join("upsmon.conf")).unwrap();
        for line in [
            "RUN_AS_USER root",
            "POLLFREQ 5",
            "FINALDELAY 5",
            "NOTIFYMSG ONBATT \"on battery\"",
            "NOTIFYFLAG REPLBATT SYSLOG+WALL",
            "\"pass with spaces\"",
        ] {
            assert!(upsmon.contains(line), "{line}\n{upsmon}");
        }
        assert_eq!(
            std::fs::read_to_string(dir.path().join("upssched.conf")).unwrap(),
            FULL_UPSSCHED
        );
        let again = after.apply_with(dir.path(), &svc, &group).unwrap();
        assert_eq!(again, ApplyOutcome::default(), "second apply is a no-op");
        assert_eq!(*svc.calls.borrow(), vec![true]);
    }

    #[test]
    fn unparsed_monitor_or_section_is_refused() {
        let (dir, group) = setup();
        write_full(dir.path());
        let mut cfg = NutConfig::read_existing(dir.path(), false).unwrap();
        cfg.monitors.pop();
        let err = cfg
            .apply_with(dir.path(), &MockServices::default(), &group)
            .unwrap_err();
        assert!(err.contains("MONITOR"), "{err}");

        std::fs::write(dir.path().join("ups.conf"), format!("{FULL_UPS}[broken\n")).unwrap();
        let err = NutConfig::read_existing(dir.path(), false)
            .unwrap()
            .apply_with(dir.path(), &MockServices::default(), &group)
            .unwrap_err();
        assert!(err.contains("[section]"), "{err}");
    }

    #[test]
    fn dropping_monitors_requires_remove_monitors() {
        let (dir, group) = setup();
        write_full(dir.path());
        let mut cfg = NutConfig::read_existing(dir.path(), false).unwrap();
        let dropped = cfg.monitors.pop().unwrap().system;
        let err = cfg
            .apply_with(dir.path(), &MockServices::default(), &group)
            .unwrap_err();
        assert!(err.contains("remove_monitors"), "{err}");

        cfg.remove_monitors = vec!["ghost@localhost".into()];
        let err = cfg
            .apply_with(dir.path(), &MockServices::default(), &group)
            .unwrap_err();
        assert!(err.contains("ghost@localhost"), "{err}");

        cfg.remove_monitors = vec![dropped.clone()];
        cfg.apply_with(dir.path(), &MockServices::default(), &group)
            .unwrap();
        let mon = std::fs::read_to_string(dir.path().join("upsmon.conf")).unwrap();
        assert!(!mon.contains(&dropped), "{mon}");
    }

    #[test]
    fn remove_monitors_cannot_empty_the_config() {
        let (dir, group) = setup();
        write_full(dir.path());
        let mut cfg = NutConfig::read_existing(dir.path(), false).unwrap();
        cfg.remove_monitors = cfg.monitors.drain(..).map(|m| m.system).collect();
        let err = cfg
            .apply_with(dir.path(), &MockServices::default(), &group)
            .unwrap_err();
        assert!(err.contains("no MONITOR lines"), "{err}");
    }

    #[test]
    fn replace_custom_upssched_renders_managed() {
        let mut cfg = NutConfig {
            upssched_custom: Some("CMDSCRIPT /custom\n".into()),
            ..sample()
        };
        let find = |c: &NutConfig| {
            c.render()
                .into_iter()
                .find(|f| f.name == "upssched.conf")
                .unwrap()
                .contents
        };
        assert_eq!(find(&cfg), "CMDSCRIPT /custom\n");
        cfg.replace_custom_upssched = true;
        assert!(find(&cfg).starts_with(MANAGED_HEADER));
    }

    #[test]
    fn local_monitor_without_ups_is_refused() {
        let (dir, group) = setup();
        let cfg = NutConfig {
            upses: vec![],
            ..sample()
        };
        let err = cfg
            .apply_with(dir.path(), &MockServices::default(), &group)
            .unwrap_err();
        assert!(err.contains("serverups"), "{err}");
    }

    #[test]
    fn unknown_remove_ups_is_refused() {
        let (dir, group) = setup();
        let cfg = NutConfig {
            remove_upses: vec!["ghost".into()],
            ..sample()
        };
        let err = cfg
            .apply_with(dir.path(), &MockServices::default(), &group)
            .unwrap_err();
        assert!(err.contains("ghost"), "{err}");
    }

    #[test]
    fn kill_power_false_omits_powerdownflag() {
        let mut cfg = sample();
        cfg.upsmon.kill_power = false;
        assert!(!cfg.render_upsmon_conf().contains("POWERDOWNFLAG"));
    }

    #[test]
    fn unraid_uses_last_manual_assignment() {
        let root = tempfile::tempdir().unwrap();
        let cfg = root.path().join("boot/config/plugins/nut/nut.cfg");
        std::fs::create_dir_all(cfg.parent().unwrap()).unwrap();
        std::fs::write(&cfg, "MANUAL=\"enable\"\nMANUAL=\"disable\"\n").unwrap();
        assert!(unraid_gui_mode(root.path()).is_some());
        std::fs::write(&cfg, "MANUAL=\"disable\"\nMANUAL=\"enable\"\n").unwrap();
        assert!(unraid_gui_mode(root.path()).is_none());
    }

    #[test]
    fn monitor_in_both_lists_is_refused() {
        let (dir, group) = setup();
        write_full(dir.path());
        let mut cfg = NutConfig::read_existing(dir.path(), false).unwrap();
        cfg.remove_monitors = vec![cfg.monitors[0].system.clone()];
        let err = cfg
            .apply_with(dir.path(), &MockServices::default(), &group)
            .unwrap_err();
        assert!(err.contains("both"), "{err}");
    }

    #[test]
    fn swapped_monitor_name_is_refused() {
        let (dir, group) = setup();
        write_full(dir.path());
        let mut cfg = NutConfig::read_existing(dir.path(), false).unwrap();
        cfg.monitors[1].system = "serverups@127.0.0.1".into();
        let err = cfg
            .apply_with(dir.path(), &MockServices::default(), &group)
            .unwrap_err();
        assert!(err.contains("other@127.0.0.1"), "{err}");
    }

    #[test]
    fn ignore_on_managed_event_is_dropped() {
        let (dir, group) = setup();
        write_full(dir.path());
        let mon = dir.path().join("upsmon.conf");
        let text = std::fs::read_to_string(&mon).unwrap() + "NOTIFYFLAG ONBATT SYSLOG+IGNORE\n";
        std::fs::write(&mon, text).unwrap();
        let cfg = NutConfig::read_existing(dir.path(), false).unwrap();
        let out = cfg
            .apply_with(dir.path(), &MockServices::default(), &group)
            .unwrap();
        assert!(
            out.message
                .contains("dropped IGNORE from NOTIFYFLAG ONBATT"),
            "{out:?}"
        );
        let mon = std::fs::read_to_string(&mon).unwrap();
        assert!(!mon.contains("IGNORE"), "{mon}");
        assert!(
            mon.contains("NOTIFYFLAG ONBATT SYSLOG+WALL+EXEC\n"),
            "{mon}"
        );
        let again = cfg
            .apply_with(dir.path(), &MockServices::default(), &group)
            .unwrap();
        assert!(again.written.is_empty());
        assert!(again.message.contains("dropped IGNORE"), "{again:?}");
    }

    #[test]
    fn unquoted_fields_must_be_bare_tokens() {
        let bad: [fn(&mut NutConfig); 9] = [
            |c| c.monitors[0].system = "serverups@local\thost".into(),
            |c| c.monitors[0].system = String::new(),
            |c| c.monitors[0].role = "master #x".into(),
            |c| c.listen[0].address = "0.0.0.0\"".into(),
            |c| c.upses[0].name = "server ups".into(),
            |c| c.upses[0].extra = vec![("a\\b".into(), "1".into())],
            |c| c.notify_flags = vec![("ONBATT".into(), "wall".into())],
            |c| c.notify_flags = vec![("ONBATT".into(), "WALL+".into())],
            |c| c.notify_flags = vec![("ON BATT".into(), "WALL".into())],
        ];
        for (i, f) in bad.iter().enumerate() {
            let mut cfg = sample();
            f(&mut cfg);
            assert!(cfg.check_values().is_err(), "case {i}");
        }
        let mut cfg = sample();
        cfg.upses[0].extra = vec![("# a note".into(), String::new())];
        cfg.notify_flags = vec![("ONBATT".into(), "WALL+EXEC".into())];
        assert!(cfg.check_values().is_ok());
    }

    #[test]
    fn empty_onbattery_capture_is_rejected() {
        for cmd in ["", "  "] {
            let mut cfg = sample();
            cfg.notify.onbattery_capture_cmd = cmd.into();
            let err = cfg.check_values().unwrap_err();
            assert!(err.contains("onbattery capture"), "{err}");
        }
    }

    #[test]
    fn onbattery_capture_marker_is_raw_text() {
        let mut cfg = sample();
        cfg.notify.onbattery_capture_cmd = r#"sh -c "orca diagnose" >> /x\y.json"#.into();
        let sched = cfg.render_upssched_conf();
        assert!(
            sched.contains("# orca-onbattery-capture sh -c \"orca diagnose\" >> /x\\y.json\n"),
            "{sched}"
        );
        let n = parse_notify("", Some(&sched), false).unwrap();
        assert_eq!(n.onbattery_capture_cmd, cfg.notify.onbattery_capture_cmd);
    }

    #[test]
    fn legacy_marker_falls_back_to_raw_text() {
        let get = |v: &str| marker_value(&format!("# orca-log-path {v}\n"), "orca-log-path");
        assert_eq!(get(r"/a\ b").as_deref(), Some(r"/a\ b"));
        assert_eq!(get("/a b ").as_deref(), Some("/a b"));
        assert_eq!(get(r#""/a \"b\"""#).as_deref(), Some(r#"/a "b""#));
        assert_eq!(get("/var/log/x").as_deref(), Some("/var/log/x"));
    }

    #[test]
    fn operator_notifyflags_are_deduplicated() {
        let mut cfg = sample();
        cfg.notify_flags = vec![
            ("ONBATT".into(), "X+WALL+X".into()),
            ("ONBATT".into(), "X".into()),
        ];
        let mon = cfg.render_upsmon_conf();
        assert!(
            mon.contains("NOTIFYFLAG ONBATT SYSLOG+WALL+EXEC+X\n"),
            "{mon}"
        );
        assert_eq!(
            parse_notify_flags("NOTIFYFLAG ONBATT SYSLOG+X+WALL+X\n"),
            vec![("ONBATT".to_string(), "X".to_string())]
        );
    }

    #[test]
    fn operator_comments_survive_apply() {
        let (dir, group) = setup();
        write_full(dir.path());
        let add = |name: &str, line: &str| {
            let p = dir.path().join(name);
            let t = std::fs::read_to_string(&p).unwrap();
            std::fs::write(&p, format!("{line}\n{t}")).unwrap();
        };
        add("upsmon.conf", "# upsmon note");
        add("upsd.conf", "# upsd note");
        add("ups.conf", "# ups note");
        let ups = dir.path().join("ups.conf");
        let t = std::fs::read_to_string(&ups).unwrap();
        std::fs::write(
            &ups,
            t.replace("  nolock\n", "  nolock\n  # section note\n"),
        )
        .unwrap();
        let cfg = NutConfig::read_existing(dir.path(), false).unwrap();
        cfg.apply_with(dir.path(), &MockServices::default(), &group)
            .unwrap();
        for (name, line) in [
            ("upsmon.conf", "# upsmon note"),
            ("upsd.conf", "# upsd note"),
            ("ups.conf", "# ups note"),
            ("ups.conf", "# section note"),
            ("ups.conf", "pollinterval = 2 # fast"),
        ] {
            let t = std::fs::read_to_string(dir.path().join(name)).unwrap();
            assert!(t.contains(line), "{line}\n{t}");
        }
        assert_eq!(NutConfig::read_existing(dir.path(), false).unwrap(), cfg);
    }

    #[test]
    fn ups_section_values_are_unquoted_and_validated() {
        let ups =
            parse_ups_conf("[u]\n driver = \"usbhid-ups\"\n port=auto\n desc = \"A \\\"b\\\"\"\n")
                .unwrap();
        assert_eq!(ups[0].driver, "usbhid-ups");
        assert_eq!(ups[0].port, "auto");
        assert_eq!(ups[0].description, "A \"b\"");
        assert!(parse_ups_conf("[u]\n driver = usbhid-ups\n").is_err());
        assert!(parse_ups_conf("[u]\n driver = a b\n port = auto\n").is_err());
        let cfg = NutConfig {
            upses: vec![UpsDef {
                port: String::new(),
                ..sample().upses[0].clone()
            }],
            ..sample()
        };
        let (dir, group) = setup();
        let err = cfg
            .apply_with(dir.path(), &MockServices::default(), &group)
            .unwrap_err();
        assert!(err.contains("no driver or port"), "{err}");
    }

    #[test]
    fn non_numeric_listen_port_is_refused() {
        assert!(parse_upsd_conf("LISTEN 0.0.0.0 nut\n").is_err());
        let (dir, _) = setup();
        std::fs::write(dir.path().join("upsd.conf"), "LISTEN 0.0.0.0 nut\n").unwrap();
        assert!(NutConfig::read_existing(dir.path(), false).is_err());
    }

    #[test]
    fn quoted_values_round_trip_and_reapply_is_noop() {
        let (dir, group) = setup();
        let mut cfg = sample();
        cfg.upsmon.shutdown_cmd = r#"sh -c "sync; /sbin/shutdown -h +0""#.into();
        cfg.upsmon.power_down_flag = "/etc/kill power".into();
        cfg.notify.pipe = "/var/run/nut/my pipe".into();
        cfg.notify.log_path = "/boot/nut logs/events.log".into();
        cfg.apply_with(dir.path(), &MockServices::default(), &group)
            .unwrap();
        let read = NutConfig::read_existing(dir.path(), false).unwrap();
        assert_eq!(read.upsmon.shutdown_cmd, cfg.upsmon.shutdown_cmd);
        assert_eq!(read.upsmon.power_down_flag, cfg.upsmon.power_down_flag);
        assert_eq!(read.notify, cfg.notify);
        let again = read
            .apply_with(dir.path(), &MockServices::default(), &group)
            .unwrap();
        assert!(again.written.is_empty(), "{again:?}");
    }

    #[test]
    fn newline_injection_is_refused() {
        let (dir, group) = setup();
        let mut cfg = sample();
        cfg.upsmon.shutdown_cmd = "/sbin/halt\nMONITOR evil@host 1 u p master".into();
        let err = cfg
            .apply_with(dir.path(), &MockServices::default(), &group)
            .unwrap_err();
        assert!(err.contains("control character"), "{err}");
        let mut cfg = sample();
        cfg.notify.log_path = "/boot/x\r".into();
        assert!(cfg.check_values().is_err());
        assert!(entries(dir.path()).is_empty());
    }

    #[test]
    fn unreadable_files_fail_closed() {
        let (dir, group) = setup();
        std::fs::create_dir(dir.path().join("upsmon.conf")).unwrap();
        assert!(NutConfig::read_existing(dir.path(), false).is_err());
        assert!(sample()
            .apply_with(dir.path(), &MockServices::default(), &group)
            .is_err());
        let (dir, group) = setup();
        std::fs::create_dir(dir.path().join("ups.conf")).unwrap();
        assert!(NutConfig::read_existing(dir.path(), false).is_err());
        assert!(sample()
            .apply_with(dir.path(), &MockServices::default(), &group)
            .is_err());
    }

    #[test]
    fn non_numeric_monitor_power_value_is_refused() {
        assert!(parse_monitors("MONITOR u@localhost two user pass master\n").is_err());
        let (dir, _) = setup();
        std::fs::write(
            dir.path().join("upsmon.conf"),
            "MONITOR u@localhost x user pass master\n",
        )
        .unwrap();
        assert!(NutConfig::read_existing(dir.path(), false).is_err());
    }
}
