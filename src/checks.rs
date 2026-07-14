//! UPS-side diagnostics + repair for the host `nut` runs on.
//!
//! Modeled on `raccoon/src/checks.rs`: each check returns an optional
//! [`Finding`]; each repair id maps to a concrete, idempotent action. Everything
//! is synchronous (local file reads + short probes); the core proxy runs it on a
//! blocking pool. UPS liveness is read over TCP via [`crate::client`] on the
//! shared reactor.
//!
//! **Active-subsystem detection first.** On Unraid the stock UPS daemon is
//! `apcupsd`; the community NUT setup replaces it. Checks target whichever is
//! active; when `apcupsd` is found, `apcupsd-active` recommends/repairs the
//! migration to NUT.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use plugin_toolkit::contract::diagnostics::{
    DiagnoseArgs, Finding, RepairArgs, RepairOutcome, RepairSpec, Severity,
};
use plugin_toolkit::reactor;
use plugin_toolkit::serde_json;

use crate::client::{NutClient, Ups, DEFAULT_PORT};
use crate::config::{detect_conf_dir, NotifySettings, NutConfig, UpsmonSettings};

/// A `SHUTDOWNCMD` that can wedge on a stuck array unmount because it carries no
/// force-unmount safety net — the exact failure mode this plan exists to fix.
/// Empty, or a bare `halt`/`poweroff`, qualifies.
fn shutdown_cmd_unsafe(cmd: &str) -> bool {
    let cmd = cmd.trim();
    cmd.is_empty() || matches!(cmd, "halt" | "poweroff" | "/sbin/halt" | "/sbin/poweroff")
}

// ── diagnose ─────────────────────────────────────────────────────────────────

/// Run every check and return the findings as JSON (`Vec<Finding>`).
pub fn diagnose(args_json: &str) -> Result<String, String> {
    let _: DiagnoseArgs = if args_json.trim().is_empty() {
        DiagnoseArgs::default()
    } else {
        serde_json::from_str(args_json).unwrap_or_default()
    };
    let dir = detect_conf_dir();
    let upsmon = read_upsmon(&dir);
    let upssched = fs::read_to_string(dir.join("upssched.conf")).unwrap_or_default();
    let findings: Vec<Finding> = [
        check_apcupsd_active(),
        check_ups_comms(&dir),
        check_battery_thresholds(&upsmon),
        check_kill_power(&upsmon),
        check_shutdown_command(&upsmon),
        check_persistent_event_log(&upsmon, &upssched),
        check_onbattery_capture(&upssched),
    ]
    .into_iter()
    .flatten()
    .collect();
    serde_json::to_string(&findings).map_err(|e| format!("encode findings: {e}"))
}

fn finding(
    id: &str,
    severity: Severity,
    title: &str,
    detail: String,
    repair: Option<RepairSpec>,
) -> Finding {
    Finding {
        id: id.to_string(),
        provider: crate::PROVIDER.to_string(),
        severity,
        title: title.to_string(),
        detail,
        repair,
    }
}

/// An in-place repair this provider applies itself. Config-writing repairs are
/// `privileged` and non-`automatic` (suggest-then-confirm), per the hard rules.
fn repair_spec(id: &str, description: &str) -> RepairSpec {
    RepairSpec {
        id: id.to_string(),
        description: description.to_string(),
        automatic: false,
        privileged: true,
        delegate: None,
    }
}

// ── checks ───────────────────────────────────────────────────────────────────

/// `apcupsd-active`: the stock apcupsd daemon is in use instead of NUT.
fn check_apcupsd_active() -> Option<Finding> {
    if !apcupsd_present() {
        return None;
    }
    Some(finding(
        "apcupsd-active",
        Severity::Warn,
        "apcupsd is managing the UPS instead of NUT",
        "apcupsd config/daemon detected; orca manages UPS lifecycle through NUT. \
         Migrating to NUT unlocks multi-UPS, networked monitoring, and the \
         survivable event-capture hooks below."
            .to_string(),
        Some(repair_spec(
            "apcupsd-active",
            "Stop/disable apcupsd and deploy NUT in its place (privileged)",
        )),
    ))
}

/// `ups-comms`: `upsd` unreachable or a UPS not reporting → Crit.
fn check_ups_comms(dir: &Path) -> Option<Finding> {
    // Only meaningful when this host is a NUT server (has ups.conf sections) or
    // is configured to monitor one. If neither apcupsd nor NUT is set up, skip.
    let ups_conf = fs::read_to_string(dir.join("ups.conf")).unwrap_or_default();
    if ups_conf.trim().is_empty() && !apcupsd_present() {
        return None;
    }
    let host = upsd_host();
    let result = reactor::block_on(async move {
        let mut c = NutClient::connect(&host, DEFAULT_PORT).await?;
        c.list_upses().await
    });
    match result {
        Err(e) => Some(finding(
            "ups-comms",
            Severity::Crit,
            "upsd unreachable — UPS state is invisible",
            format!("could not talk to upsd on port {DEFAULT_PORT}: {e}"),
            Some(repair_spec(
                "ups-comms",
                "Restart/redeploy the NUT server so upsd serves UPS state (privileged)",
            )),
        )),
        Ok(upses) if upses.is_empty() => Some(finding(
            "ups-comms",
            Severity::Crit,
            "upsd reports no UPS",
            "upsd is reachable but exposes no UPS — the driver is not attached/reporting"
                .to_string(),
            Some(repair_spec(
                "ups-comms",
                "Restart the NUT driver/server so the UPS reports (privileged)",
            )),
        )),
        Ok(upses) => {
            let stale: Vec<&str> = upses
                .iter()
                .filter(|u| u.status().is_none())
                .map(|u| u.name.as_str())
                .collect();
            if stale.is_empty() {
                Some(finding(
                    "ups-comms",
                    Severity::Ok,
                    "UPS reporting normally",
                    ups_comms_summary(&upses),
                    None,
                ))
            } else {
                Some(finding(
                    "ups-comms",
                    Severity::Crit,
                    "UPS not reporting status",
                    format!(
                        "upsd exposes {} but they report no ups.status",
                        stale.join(", ")
                    ),
                    Some(repair_spec(
                        "ups-comms",
                        "Restart the NUT driver so the UPS reports status (privileged)",
                    )),
                ))
            }
        }
    }
}

/// `battery-thresholds`: `MINSUPPLIES` / low-battery timing unsafe vs the
/// reported runtime. Reads live `battery.runtime` when upsd is reachable.
fn check_battery_thresholds(upsmon: &Option<UpsmonSettings>) -> Option<Finding> {
    let upsmon = upsmon.as_ref()?;
    // MINSUPPLIES 0 disables shutdown entirely — the host will never power off.
    if upsmon.min_supplies == 0 {
        return Some(finding(
            "battery-thresholds",
            Severity::Warn,
            "MINSUPPLIES is 0 — host will not shut down on power loss",
            "upsmon MINSUPPLIES=0 means no supply is required, so a critical UPS \
             never triggers shutdown."
                .to_string(),
            Some(repair_spec(
                "battery-thresholds",
                "Set safe upsmon MINSUPPLIES / timing thresholds (privileged)",
            )),
        ));
    }
    // If we can read live runtime, sanity-check that HOSTSYNC + shutdown headroom
    // fits inside the reported low-battery runtime.
    if let Some(runtime) = live_battery_runtime() {
        let needed = (upsmon.host_sync + upsmon.dead_time) as f64;
        if runtime < needed {
            return Some(finding(
                "battery-thresholds",
                Severity::Warn,
                "Shutdown timing does not fit the reported battery runtime",
                format!(
                    "reported battery.runtime {runtime:.0}s is below HOSTSYNC+DEADTIME {needed:.0}s; \
                     shutdown may not complete before the UPS dies"
                ),
                Some(repair_spec(
                    "battery-thresholds",
                    "Set safe upsmon HOSTSYNC/DEADTIME/MINSUPPLIES thresholds (privileged)",
                )),
            ));
        }
    }
    Some(finding(
        "battery-thresholds",
        Severity::Ok,
        "Battery thresholds look safe",
        format!(
            "MINSUPPLIES={}, HOSTSYNC={}s, DEADTIME={}s",
            upsmon.min_supplies, upsmon.host_sync, upsmon.dead_time
        ),
        None,
    ))
}

/// `kill-power`: UPS power is not cut after shutdown → the UPS keeps feeding a
/// halted box and won't auto-repower when mains return.
fn check_kill_power(upsmon: &Option<UpsmonSettings>) -> Option<Finding> {
    let upsmon = upsmon.as_ref()?;
    let flag_touched = Path::new(&upsmon.power_down_flag).exists();
    if upsmon.kill_power {
        Some(finding(
            "kill-power",
            Severity::Ok,
            "Kill-power on shutdown is configured",
            format!(
                "POWERDOWNFLAG {} set; UPS cuts power after halt and re-powers on return{}",
                upsmon.power_down_flag,
                if flag_touched { " (flag present)" } else { "" }
            ),
            None,
        ))
    } else {
        Some(finding(
            "kill-power",
            Severity::Warn,
            "UPS will not cut power after shutdown",
            "kill-power/POWERDOWNFLAG is not set: after the host halts the UPS keeps \
             feeding it and will not auto-repower when mains return, so the box stays \
             down until someone power-cycles it."
                .to_string(),
            Some(repair_spec(
                "kill-power",
                "Enable kill-power (POWERDOWNFLAG + upsdrvctl shutdown) (privileged)",
            )),
        ))
    }
}

/// `shutdown-command`: `SHUTDOWNCMD` is not wired to a completing,
/// force-unmount-aware shutdown. Repair **delegates** to the unraid
/// `shutdown-timeout` remediation (the actual reason a UPS shutdown can hang
/// forever lives on the Unraid side, not in NUT).
fn check_shutdown_command(upsmon: &Option<UpsmonSettings>) -> Option<Finding> {
    let upsmon = upsmon.as_ref()?;
    let cmd = upsmon.shutdown_cmd.trim();
    if !shutdown_cmd_unsafe(cmd) {
        return Some(finding(
            "shutdown-command",
            Severity::Ok,
            "SHUTDOWNCMD wired to a completing shutdown",
            format!("SHUTDOWNCMD = \"{cmd}\""),
            None,
        ));
    }
    Some(finding(
        "shutdown-command",
        Severity::Warn,
        "SHUTDOWNCMD may hang the box mid-shutdown",
        format!(
            "SHUTDOWNCMD = \"{cmd}\" has no force-unmount safety net; a UPS-triggered \
             shutdown can wedge on a stuck array unmount and never power off. This repair \
             rewrites SHUTDOWNCMD to a completing shutdown. On Unraid, also run the unraid \
             `shutdown-timeout` repair — the host-side force-unmount net that guarantees the \
             array actually releases (the two are complementary)."
        ),
        Some(repair_spec(
            "shutdown-command",
            "Rewrite upsmon.conf SHUTDOWNCMD to a completing shutdown; on Unraid pair with the \
             unraid shutdown-timeout repair",
        )),
    ))
}

/// `persistent-event-log`: NUT event notifications log to tmpfs only / no
/// remote → the power-cycle recovery needs erases the evidence.
fn check_persistent_event_log(upsmon: &Option<UpsmonSettings>, upssched: &str) -> Option<Finding> {
    // Only assess once NUT (upsmon) is set up.
    upsmon.as_ref()?;
    let has_flash = marker_value(upssched, "orca-log-path")
        .map(|v| v.contains("/boot"))
        .unwrap_or(false);
    let has_remote = marker_value(upssched, "orca-remote-syslog")
        .map(|v| !v.is_empty())
        .unwrap_or(false);
    if has_flash && has_remote {
        return Some(finding(
            "persistent-event-log",
            Severity::Ok,
            "NUT events log to persistent flash and a remote target",
            "upssched writes event records to /boot and forwards to remote syslog — \
             they survive a power-cycle."
                .to_string(),
            None,
        ));
    }
    Some(finding(
        "persistent-event-log",
        Severity::Warn,
        "NUT event log is not survivable",
        "NUT NOTIFYCMD/upssched logging is missing a persistent flash target and/or a \
         remote syslog target; a power-cycle (which recovery requires) erases tmpfs \
         logs, destroying the evidence of what happened."
            .to_string(),
        Some(repair_spec(
            "persistent-event-log",
            "Install upssched NOTIFY logging to flash (/boot) and a remote syslog target (privileged)",
        )),
    ))
}

/// `onbattery-capture`: no ONBATT hook snapshots diagnostics before shutdown.
fn check_onbattery_capture(upssched: &str) -> Option<Finding> {
    if upssched.trim().is_empty() {
        return None; // upssched not configured yet — covered elsewhere.
    }
    let has_hook = upssched
        .lines()
        .any(|l| l.contains("ONBATT") && l.contains("onbattery-capture"));
    if has_hook {
        Some(finding(
            "onbattery-capture",
            Severity::Ok,
            "On-battery diagnostics capture is installed",
            "an ONBATT upssched hook snapshots diagnostics to flash + remote before \
             shutdown proceeds."
                .to_string(),
            None,
        ))
    } else {
        Some(finding(
            "onbattery-capture",
            Severity::Warn,
            "No on-battery diagnostics snapshot before shutdown",
            "there is no ONBATT hook that captures diagnostics before the shutdown \
             command runs; if the next shutdown hangs, there is no record of the \
             state at the moment of failure."
                .to_string(),
            Some(repair_spec(
                "onbattery-capture",
                "Install an ONBATT upssched hook that runs `orca diagnostics diagnose` \
                 and writes the snapshot to flash + remote before shutdown (privileged)",
            )),
        ))
    }
}

// ── repair ───────────────────────────────────────────────────────────────────

/// Run one repair by id and return a [`RepairOutcome`] as JSON. Delegated
/// repairs (`shutdown-command`) are dispatched core-side and never reach here.
pub fn repair(args_json: &str) -> Result<String, String> {
    let args: RepairArgs =
        serde_json::from_str(args_json).map_err(|e| format!("invalid repair args: {e}"))?;
    let (ok, message) = match args.repair_id.as_str() {
        "ups-comms" => repair_ups_comms(),
        "battery-thresholds" => repair_write_config("safe battery thresholds"),
        "kill-power" => repair_kill_power(),
        "shutdown-command" => repair_write_config("completing, force-unmount-aware SHUTDOWNCMD"),
        "persistent-event-log" => repair_write_config("persistent + remote event logging"),
        "onbattery-capture" => repair_write_config("on-battery diagnostics capture hook"),
        "apcupsd-active" => repair_apcupsd_migrate(),
        other => (false, format!("nut has no in-place repair '{other}'")),
    };
    let outcome = RepairOutcome {
        id: args.repair_id,
        provider: crate::PROVIDER.to_string(),
        ok,
        message,
    };
    serde_json::to_string(&outcome).map_err(|e| format!("encode outcome: {e}"))
}

/// Re-render the managed config with the safe defaults and write it
/// idempotently. Each config-writing repair funnels here so a single, coherent
/// config is (re)applied — `battery-thresholds`, `persistent-event-log`,
/// `kill-power`, and `onbattery-capture` are all facets of the same file set.
fn repair_write_config(what: &str) -> (bool, String) {
    let dir = detect_conf_dir();
    // Merge the desired safe settings onto whatever is already configured so we
    // don't clobber an operator's UPS/monitor topology.
    let cfg = merged_safe_config(&dir);
    match cfg.apply(&dir) {
        Ok(written) if written.is_empty() => (
            true,
            format!("{what} already in place at {}", dir.display()),
        ),
        Ok(written) => (
            true,
            format!(
                "applied {what}: wrote {} in {}",
                written.join(", "),
                dir.display()
            ),
        ),
        Err(e) => (
            false,
            format!(
                "failed to apply {what} in {} ({e}); needs privilege",
                dir.display()
            ),
        ),
    }
}

fn repair_kill_power() -> (bool, String) {
    // Kill-power is enabled by the managed config (POWERDOWNFLAG + kill_power).
    repair_write_config("kill-power on shutdown")
}

fn repair_ups_comms() -> (bool, String) {
    // Restart the NUT server/driver stack. Best-effort across the common
    // service managers; report guidance if none succeed.
    for (bin, args) in [
        ("systemctl", &["restart", "nut-server"][..]),
        ("systemctl", &["restart", "nut-monitor"][..]),
        ("rc-service", &["nut-server", "restart"][..]),
    ] {
        if run(bin, args).is_ok() {
            return (true, format!("restarted NUT via {bin} {}", args.join(" ")));
        }
    }
    (
        false,
        "could not restart the NUT server; run with privilege: sudo systemctl restart nut-server \
         (or redeploy via service.deploy nut)"
            .to_string(),
    )
}

fn repair_apcupsd_migrate() -> (bool, String) {
    let mut steps = Vec::new();
    if run("systemctl", &["disable", "--now", "apcupsd"]).is_ok() {
        steps.push("disabled apcupsd");
    } else if run("rc-service", &["apcupsd", "stop"]).is_ok() {
        steps.push("stopped apcupsd");
    }
    if steps.is_empty() {
        return (
            false,
            "could not stop apcupsd; run with privilege: sudo systemctl disable --now apcupsd, \
             then deploy NUT via service.deploy nut"
                .to_string(),
        );
    }
    (
        true,
        format!(
            "{}; now deploy NUT with service.deploy nut and apply config via service.configure nut",
            steps.join(", ")
        ),
    )
}

// ── helpers ──────────────────────────────────────────────────────────────────

/// Build a `NutConfig` that merges orca's safe defaults onto whatever monitor
/// topology is already present, so the config-writing repairs don't discard an
/// operator's UPS/monitor setup while still enforcing the safe values.
fn merged_safe_config(dir: &Path) -> NutConfig {
    let existing = read_upsmon(dir);
    let mut cfg = NutConfig {
        upsmon: existing.unwrap_or_default(),
        ..Default::default()
    };
    // Enforce the safe kill-power + notify defaults regardless of prior state.
    cfg.upsmon.kill_power = true;
    if cfg.upsmon.min_supplies == 0 {
        cfg.upsmon.min_supplies = 1;
    }
    // Enforce a completing, force-unmount-aware SHUTDOWNCMD — a preserved unsafe
    // one (bare halt/poweroff) is what wedges the array unmount.
    if shutdown_cmd_unsafe(&cfg.upsmon.shutdown_cmd) {
        cfg.upsmon.shutdown_cmd = UpsmonSettings::default().shutdown_cmd;
    }
    cfg.notify = NotifySettings::default();
    cfg
}

/// Parse `upsmon.conf` into the fields the checks read. Returns `None` when the
/// file is absent (NUT not set up as a monitor here).
pub(crate) fn read_upsmon(dir: &Path) -> Option<UpsmonSettings> {
    let text = fs::read_to_string(dir.join("upsmon.conf")).ok()?;
    let mut s = UpsmonSettings::default();
    let mut saw_flag = false;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (key, rest) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
        let rest = rest.trim();
        match key {
            "MINSUPPLIES" => {
                if let Ok(v) = rest.parse() {
                    s.min_supplies = v;
                }
            }
            "SHUTDOWNCMD" => s.shutdown_cmd = unquote_field(rest),
            "POWERDOWNFLAG" => {
                s.power_down_flag = rest.to_string();
                saw_flag = true;
            }
            "HOSTSYNC" => {
                if let Ok(v) = rest.parse() {
                    s.host_sync = v;
                }
            }
            "DEADTIME" => {
                if let Ok(v) = rest.parse() {
                    s.dead_time = v;
                }
            }
            _ => {}
        }
    }
    // kill_power is on only when a POWERDOWNFLAG is actually declared.
    s.kill_power = saw_flag;
    Some(s)
}

/// Read the value of an `# orca-<key> <value>` marker comment the managed
/// `upssched.conf` embeds (the handler's single source of truth).
fn marker_value(upssched: &str, key: &str) -> Option<String> {
    let prefix = format!("# {key} ");
    upssched
        .lines()
        .find_map(|l| l.trim_start().strip_prefix(&prefix))
        .map(|v| v.trim().to_string())
}

fn unquote_field(s: &str) -> String {
    s.trim()
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .unwrap_or(s.trim())
        .to_string()
}

/// Read the live `battery.runtime` (seconds) from the first reporting UPS, if
/// `upsd` is reachable. Best-effort — `None` when it isn't.
fn live_battery_runtime() -> Option<f64> {
    let host = upsd_host();
    let upses: Vec<Ups> = reactor::block_on(async move {
        let mut c = NutClient::connect(&host, DEFAULT_PORT).await.ok()?;
        c.list_upses().await.ok()
    })?;
    upses.iter().find_map(|u| u.battery_runtime())
}

fn ups_comms_summary(upses: &[Ups]) -> String {
    let parts: Vec<String> = upses
        .iter()
        .map(|u| {
            let status = u.status().unwrap_or("?");
            let charge = u
                .battery_charge()
                .map(|c| format!("{c:.0}%"))
                .unwrap_or_else(|| "?".into());
            format!("{}: {status} (battery {charge})", u.name)
        })
        .collect();
    parts.join("; ")
}

/// Host `upsd` is reached at. Loopback by default; overridable for a
/// monitor-only client that watches a remote `upsd`.
fn upsd_host() -> String {
    env::var("NUT_UPSD_HOST").unwrap_or_else(|_| "127.0.0.1".to_string())
}

/// Whether apcupsd is present on this host (config or binary).
fn apcupsd_present() -> bool {
    Path::new("/etc/apcupsd/apcupsd.conf").exists()
        || Path::new("/boot/config/plugins/apcupsd").exists()
        || which("apcaccess").is_some()
}

fn which(bin: &str) -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    env::split_paths(&path)
        .map(|d| d.join(bin))
        .find(|p| p.is_file())
}

/// Run a command, returning `Ok` on success or the first stderr line on failure.
fn run(bin: &str, args: &[&str]) -> Result<String, String> {
    let out = std::process::Command::new(bin)
        .args(args)
        .output()
        .map_err(|e| format!("spawn {bin}: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(String::from_utf8_lossy(&out.stderr)
            .trim()
            .lines()
            .next()
            .unwrap_or("command failed")
            .to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const UPSMON_UNSAFE: &str = r#"
# a monitor-only client with unsafe settings
MONITOR serverups@localhost 1 monuser secretref master
MINSUPPLIES 0
SHUTDOWNCMD "poweroff"
HOSTSYNC 15
DEADTIME 15
"#;

    const UPSMON_SAFE: &str = r#"
MONITOR serverups@localhost 1 monuser secretref master
MINSUPPLIES 1
SHUTDOWNCMD "/sbin/shutdown -h +0"
POWERDOWNFLAG /etc/killpower
HOSTSYNC 15
DEADTIME 15
"#;

    fn write_upsmon(text: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("upsmon.conf"), text).unwrap();
        dir
    }

    #[test]
    fn parses_upsmon_fixture() {
        let dir = write_upsmon(UPSMON_UNSAFE);
        let s = read_upsmon(dir.path()).expect("parsed");
        assert_eq!(s.min_supplies, 0);
        assert_eq!(s.shutdown_cmd, "poweroff");
        assert!(!s.kill_power, "no POWERDOWNFLAG → kill_power off");
    }

    #[test]
    fn battery_thresholds_flags_minsupplies_zero() {
        let s = read_upsmon(write_upsmon(UPSMON_UNSAFE).path());
        let f = check_battery_thresholds(&s).expect("finding");
        assert_eq!(f.severity, Severity::Warn);
        assert_eq!(f.id, "battery-thresholds");
        assert!(f.repair.is_some());
    }

    #[test]
    fn battery_thresholds_ok_when_safe() {
        let s = read_upsmon(write_upsmon(UPSMON_SAFE).path());
        let f = check_battery_thresholds(&s).expect("finding");
        // No live upsd in the test env, so it only checks MINSUPPLIES here.
        assert_eq!(f.severity, Severity::Ok);
        assert!(f.repair.is_none());
    }

    #[test]
    fn kill_power_warns_without_flag() {
        let s = read_upsmon(write_upsmon(UPSMON_UNSAFE).path());
        let f = check_kill_power(&s).expect("finding");
        assert_eq!(f.severity, Severity::Warn);
        assert!(f.repair.is_some());
    }

    #[test]
    fn kill_power_ok_with_flag() {
        let s = read_upsmon(write_upsmon(UPSMON_SAFE).path());
        let f = check_kill_power(&s).expect("finding");
        assert_eq!(f.severity, Severity::Ok);
        assert!(f.repair.is_none());
    }

    #[test]
    fn shutdown_command_warns_with_in_place_repair() {
        let s = read_upsmon(write_upsmon(UPSMON_UNSAFE).path());
        let f = check_shutdown_command(&s).expect("finding");
        assert_eq!(f.severity, Severity::Warn);
        let repair = f.repair.expect("repair");
        // In-place SHUTDOWNCMD rewrite (no cross-plugin delegate — unraid exposes
        // shutdown-timeout as a diagnostics repair, not a routable unit action).
        assert!(repair.delegate.is_none());
        assert_eq!(repair.id, "shutdown-command");
        assert!(!repair.automatic);
        assert!(repair.privileged);
    }

    #[test]
    fn safe_config_rewrites_unsafe_shutdown_cmd() {
        let s = read_upsmon(write_upsmon(UPSMON_UNSAFE).path());
        assert!(shutdown_cmd_unsafe(&s.as_ref().unwrap().shutdown_cmd));
        let dir = write_upsmon(UPSMON_UNSAFE);
        let cfg = merged_safe_config(dir.path());
        assert!(!shutdown_cmd_unsafe(&cfg.upsmon.shutdown_cmd));
    }

    #[test]
    fn shutdown_command_ok_with_completing_cmd() {
        let s = read_upsmon(write_upsmon(UPSMON_SAFE).path());
        let f = check_shutdown_command(&s).expect("finding");
        assert_eq!(f.severity, Severity::Ok);
        assert!(f.repair.is_none());
    }

    #[test]
    fn persistent_event_log_warns_when_missing() {
        let s = read_upsmon(write_upsmon(UPSMON_SAFE).path());
        let f = check_persistent_event_log(&s, "").expect("finding");
        assert_eq!(f.severity, Severity::Warn);
        assert_eq!(f.id, "persistent-event-log");
    }

    #[test]
    fn persistent_event_log_ok_for_managed_upssched() {
        // A managed upssched.conf carries the flash + remote markers.
        let upssched = NutConfig::default();
        let rendered = upssched
            .render()
            .into_iter()
            .find(|f| f.name == "upssched.conf")
            .unwrap()
            .contents;
        let s = read_upsmon(write_upsmon(UPSMON_SAFE).path());
        let f = check_persistent_event_log(&s, &rendered).expect("finding");
        assert_eq!(f.severity, Severity::Ok);
    }

    #[test]
    fn onbattery_capture_ok_for_managed_upssched() {
        let rendered = NutConfig::default()
            .render()
            .into_iter()
            .find(|f| f.name == "upssched.conf")
            .unwrap()
            .contents;
        let f = check_onbattery_capture(&rendered).expect("finding");
        assert_eq!(f.severity, Severity::Ok);
    }

    #[test]
    fn onbattery_capture_warns_without_hook() {
        let f =
            check_onbattery_capture("CMDSCRIPT /x\nAT ONLINE * EXECUTE log\n").expect("finding");
        assert_eq!(f.severity, Severity::Warn);
        assert!(f.repair.is_some());
    }

    #[test]
    fn repair_unknown_id_reports_not_ok() {
        let out = repair(r#"{"provider":"nut","repair_id":"nope"}"#).expect("encodes");
        let o: RepairOutcome = serde_json::from_str(&out).unwrap();
        assert!(!o.ok);
        assert!(o.message.contains("no in-place repair"));
    }

    #[test]
    fn diagnose_emits_valid_json_array() {
        let out = diagnose("{}").expect("diagnose ok");
        let findings: Vec<Finding> = serde_json::from_str(&out).expect("valid findings json");
        for f in &findings {
            assert_eq!(f.provider, crate::PROVIDER);
        }
    }
}
