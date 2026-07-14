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

use std::path::{Path, PathBuf};

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
    /// changed. Returns the names of the files actually written (empty = no-op).
    pub fn apply(&self, dir: &Path) -> Result<Vec<String>, String> {
        std::fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
        let mut written = Vec::new();
        for file in self.render() {
            let path = dir.join(file.name);
            let current = std::fs::read_to_string(&path).ok();
            if current.as_deref() == Some(file.contents.as_str()) {
                continue; // idempotent: identical content, skip the write.
            }
            std::fs::write(&path, &file.contents)
                .map_err(|e| format!("write {}: {e}", path.display()))?;
            written.push(file.name.to_string());
        }
        Ok(written)
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
        let dir = tempfile::tempdir().unwrap();
        let cfg = sample();
        let first = cfg.apply(dir.path()).unwrap();
        assert_eq!(first.len(), 4, "first apply writes all four files");
        let second = cfg.apply(dir.path()).unwrap();
        assert!(second.is_empty(), "re-apply is a no-op: {second:?}");
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
}
