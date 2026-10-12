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

use crate::client::{NutClient, Ups, DEFAULT_PORT};
use crate::config::{
    detect_conf_dir, detect_init, marker_value, privilege_hint, read_or_empty, strip_comment,
    tokenize, InitSystem, NotifySettings, NutConfig, UpsmonSettings,
};
use crate::notify;

/// A `SHUTDOWNCMD` that can wedge on a stuck array unmount because it carries no
/// force-unmount safety net — the exact failure mode this plan exists to fix.
/// Empty, or a bare `halt`/`poweroff`, qualifies.
fn shutdown_cmd_unsafe(cmd: &str) -> bool {
    let cmd = cmd.trim();
    cmd.is_empty() || matches!(cmd, "halt" | "poweroff" | "/sbin/halt" | "/sbin/poweroff")
}

// ── diagnose ─────────────────────────────────────────────────────────────────

/// Run every check and return the typed findings. Network probes are awaited
/// directly on the shared reactor (the provider future already runs under it),
/// so nothing here re-enters `block_on`.
pub async fn diagnose_typed(_args: DiagnoseArgs) -> Vec<Finding> {
    let dir = detect_conf_dir();
    let mut findings: Vec<Finding> = Vec::new();
    let upsmon = read_upsmon(&dir).unwrap_or_else(|e| {
        findings.push(finding(
            "upsmon-conf",
            Severity::Crit,
            "upsmon.conf does not parse",
            format!("{e}; fix it by hand before orca can check or repair it"),
            None,
        ));
        None
    });
    let mut read = |id: &str, name: &str| {
        read_or_empty(&dir.join(name)).unwrap_or_else(|e| {
            findings.push(finding(
                id,
                Severity::Crit,
                &format!("{name} cannot be read"),
                format!("{e}; fix it by hand before orca can check or repair it"),
                None,
            ));
            String::new()
        })
    };
    let upssched = read("upssched-conf", "upssched.conf");
    let ups_conf = read("ups-conf", "ups.conf");
    // A `ups-comms` finding is only meaningful when this host is a NUT server
    // (has ups.conf sections) or runs apcupsd (migration candidate).
    let needs_comms = !ups_conf.trim().is_empty() || apcupsd_present();
    // Probe upsd once and share the result across the comms + battery checks.
    let probe: Option<Result<Vec<Ups>, String>> = if needs_comms || upsmon.is_some() {
        Some(probe_upses().await)
    } else {
        None
    };
    let live_runtime = probe
        .as_ref()
        .and_then(|r| r.as_ref().ok())
        .and_then(|upses| upses.iter().find_map(|u| u.battery_runtime()));

    findings.extend(check_apcupsd_active());
    if needs_comms {
        // `probe` is `Some` whenever `needs_comms` is true.
        findings.push(check_ups_comms(probe.as_ref().expect("probed when needed")));
    }
    findings.extend(check_battery_thresholds(&upsmon, live_runtime));
    findings.extend(check_kill_power(&upsmon));
    findings.extend(check_shutdown_command(&upsmon));
    findings.extend(check_persistent_event_log(
        &upsmon,
        &upssched,
        detect_init(Path::new("/")) == InitSystem::Unraid,
        on_tmpfs,
    ));
    findings.extend(check_onbattery_capture(&upssched));
    if upsmon.is_some() {
        let upsmon_text = read_or_empty(&dir.join("upsmon.conf")).unwrap_or_default();
        findings.extend(check_notify_handler(&upsmon_text, notify::is_executable));
    }
    findings
}

/// Best-effort probe of the UPSes `upsd` exposes on this host.
async fn probe_upses() -> Result<Vec<Ups>, String> {
    let host = upsd_host();
    let mut c = NutClient::connect(&host, DEFAULT_PORT).await?;
    c.list_upses().await
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

/// `ups-comms`: `upsd` unreachable or a UPS not reporting → Crit. `probe` is the
/// shared upsd read from [`diagnose_typed`]; the caller only invokes this when a
/// comms check is meaningful (NUT server or apcupsd present).
fn check_ups_comms(probe: &Result<Vec<Ups>, String>) -> Finding {
    match probe {
        Err(e) => finding(
            "ups-comms",
            Severity::Crit,
            "upsd unreachable — UPS state is invisible",
            format!("could not talk to upsd on port {DEFAULT_PORT}: {e}"),
            Some(repair_spec(
                "ups-comms",
                "Restart/redeploy the NUT server so upsd serves UPS state (privileged)",
            )),
        ),
        Ok(upses) if upses.is_empty() => finding(
            "ups-comms",
            Severity::Crit,
            "upsd reports no UPS",
            "upsd is reachable but exposes no UPS — the driver is not attached/reporting"
                .to_string(),
            Some(repair_spec(
                "ups-comms",
                "Restart the NUT driver/server so the UPS reports (privileged)",
            )),
        ),
        Ok(upses) => {
            let stale: Vec<&str> = upses
                .iter()
                .filter(|u| u.status().is_none())
                .map(|u| u.name.as_str())
                .collect();
            if stale.is_empty() {
                finding(
                    "ups-comms",
                    Severity::Ok,
                    "UPS reporting normally",
                    ups_comms_summary(upses),
                    None,
                )
            } else {
                finding(
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
                )
            }
        }
    }
}

/// `battery-thresholds`: `MINSUPPLIES` / low-battery timing unsafe vs the
/// reported runtime. Reads live `battery.runtime` when upsd is reachable.
fn check_battery_thresholds(
    upsmon: &Option<UpsmonSettings>,
    live_runtime: Option<f64>,
) -> Option<Finding> {
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
    if let Some(runtime) = live_runtime {
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
/// On Unraid only /boot (flash) survives a reboot; elsewhere any path off tmpfs does.
fn check_persistent_event_log(
    upsmon: &Option<UpsmonSettings>,
    upssched: &str,
    unraid: bool,
    tmpfs: fn(&Path) -> bool,
) -> Option<Finding> {
    // Only assess once NUT (upsmon) is set up.
    upsmon.as_ref()?;
    let has_flash = marker_value(upssched, "orca-log-path")
        .map(|v| {
            let p = Path::new(&v);
            if unraid {
                p.starts_with("/boot")
            } else {
                p.is_absolute() && !tmpfs(p)
            }
        })
        .unwrap_or(false);
    let has_remote = marker_value(upssched, "orca-remote-syslog")
        .map(|v| !v.is_empty())
        .unwrap_or(false);
    if has_flash && has_remote {
        return Some(finding(
            "persistent-event-log",
            Severity::Ok,
            "NUT events log to persistent flash and a remote target",
            "upssched writes event records to persistent storage and forwards to remote syslog — \
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
            "Install upssched NOTIFY logging to persistent storage (/boot on Unraid) and a remote syslog target (privileged)",
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

/// `notify-handler`: the `NOTIFYCMD` upsmon runs must exist and be executable,
/// or every event (and the on-battery capture) is silently lost.
fn check_notify_handler(upsmon: &str, executable: fn(&Path) -> bool) -> Option<Finding> {
    let cmd = notify_cmd(upsmon)?;
    if executable(Path::new(&cmd)) {
        return Some(finding(
            "notify-handler",
            Severity::Ok,
            "NUT event handler is installed",
            format!("NOTIFYCMD {cmd} exists and is executable."),
            None,
        ));
    }
    let ours = notify::invoked_as_handler(&cmd);
    Some(finding(
        "notify-handler",
        Severity::Crit,
        "NUT event handler is missing",
        format!(
            "NOTIFYCMD {cmd} does not exist or is not executable; upsmon cannot log events, \
             forward them, or run the on-battery capture."
        ),
        ours.then(|| {
            repair_spec(
                "notify-handler",
                "Install the orca-nut-notify handler (a link to the nut plugin binary) (privileged)",
            )
        }),
    ))
}

fn notify_cmd(upsmon: &str) -> Option<String> {
    upsmon.lines().find_map(|l| match tokenize(l).as_slice() {
        [k, v] if k == "NOTIFYCMD" => Some(v.clone()),
        _ => None,
    })
}

fn repair_notify_handler(dir: &Path) -> (bool, String) {
    let upsmon = match read_or_empty(&dir.join("upsmon.conf")) {
        Ok(t) => t,
        Err(e) => return (false, format!("cannot read upsmon.conf: {e}")),
    };
    let Some(cmd) = notify_cmd(&upsmon) else {
        return (false, "upsmon.conf sets no NOTIFYCMD".to_string());
    };
    match notify::ensure_installed(&cmd) {
        Ok(Some(true)) => (true, format!("installed {cmd}")),
        Ok(Some(false)) => (true, format!("{cmd} already in place")),
        Ok(None) => (
            false,
            format!("NOTIFYCMD {cmd} is not orca's handler; install it by hand"),
        ),
        Err(e) => (
            false,
            format!("failed to install {cmd} ({e}){}", privilege_hint(&e)),
        ),
    }
}

// ── repair ───────────────────────────────────────────────────────────────────

/// Run one repair by id and return the typed [`RepairOutcome`]. Delegated
/// repairs (`shutdown-command`) are dispatched core-side and never reach here.
pub fn repair_typed(args: RepairArgs) -> RepairOutcome {
    let (ok, message) = match args.repair_id.as_str() {
        "ups-comms" => repair_ups_comms(),
        "battery-thresholds" => repair_write_config("safe battery thresholds"),
        "kill-power" => repair_kill_power(),
        "shutdown-command" => repair_write_config("completing, force-unmount-aware SHUTDOWNCMD"),
        "persistent-event-log" => repair_upssched_config(
            &detect_conf_dir(),
            host_is_unraid(),
            "persistent + remote event logging",
        ),
        "onbattery-capture" => repair_upssched_config(
            &detect_conf_dir(),
            host_is_unraid(),
            "on-battery diagnostics capture hook",
        ),
        "apcupsd-active" => repair_apcupsd_migrate(),
        "notify-handler" => repair_notify_handler(&detect_conf_dir()),
        other => (false, format!("nut has no in-place repair '{other}'")),
    };
    RepairOutcome {
        id: args.repair_id,
        provider: crate::PROVIDER.to_string(),
        ok,
        message,
    }
}

/// Re-render the managed config with the safe defaults and write it
/// idempotently. Each config-writing repair funnels here so a single, coherent
/// config is (re)applied — `battery-thresholds`, `persistent-event-log`,
/// `kill-power`, and `onbattery-capture` are all facets of the same file set.
fn repair_write_config(what: &str) -> (bool, String) {
    let dir = detect_conf_dir();
    // Merge the desired safe settings onto whatever is already configured so we
    // don't clobber an operator's UPS/monitor topology.
    match merged_safe_config(&dir) {
        Ok(cfg) => write_config(&dir, what, cfg),
        Err(e) => (false, format!("cannot apply {what}: {e}")),
    }
}

/// A config repair whose fix lives in `upssched.conf`. A custom `upssched.conf`
/// is kept verbatim, so the repair refuses; replacing it is an explicit
/// `service.configure nut` with `replace_custom_upssched`.
fn repair_upssched_config(dir: &Path, unraid: bool, what: &str) -> (bool, String) {
    let cfg = match merged_safe_config_for(dir, unraid, on_tmpfs) {
        Ok(cfg) => cfg,
        Err(e) => return (false, format!("cannot apply {what}: {e}")),
    };
    if cfg.upssched_custom.is_some() {
        return (
            false,
            format!(
                "cannot apply {what}: a custom upssched.conf is in place in {} and was left \
                 unchanged; to replace it with the managed one, run service.configure nut \
                 with replace_custom_upssched: true",
                dir.display()
            ),
        );
    }
    write_config(dir, what, cfg)
}

fn write_config(dir: &Path, what: &str, cfg: NutConfig) -> (bool, String) {
    if cfg.monitors.is_empty() {
        return (
            false,
            format!(
                "cannot apply {what}: {} has no MONITOR lines; run service.configure nut to \
                 define monitors first",
                dir.display()
            ),
        );
    }
    match cfg.apply(dir) {
        Ok(out) if out.written.is_empty() && out.message.is_empty() => (
            true,
            format!("{what} already in place at {}", dir.display()),
        ),
        Ok(out) if out.written.is_empty() => (
            true,
            format!(
                "{what} already in place at {}; {}",
                dir.display(),
                out.message
            ),
        ),
        Ok(out) => (
            true,
            format!(
                "applied {what}: wrote {} in {}; {}",
                out.written.join(", "),
                dir.display(),
                out.message
            ),
        ),
        Err(e) => (
            false,
            format!(
                "failed to apply {what} in {} ({e}){}",
                dir.display(),
                privilege_hint(&e)
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
            "{}; now deploy NUT with service.deploy nut, then run service.configure nut to define \
             monitors first — the config repairs refuse to apply without them",
            steps.join(", ")
        ),
    )
}

// ── helpers ──────────────────────────────────────────────────────────────────

/// Build a `NutConfig` that merges orca's safe defaults onto whatever monitor
/// topology is already present, so the config-writing repairs don't discard an
/// operator's UPS/monitor setup while still enforcing the safe values.
fn merged_safe_config(dir: &Path) -> Result<NutConfig, String> {
    merged_safe_config_for(dir, host_is_unraid(), on_tmpfs)
}

fn host_is_unraid() -> bool {
    detect_init(Path::new("/")) == InitSystem::Unraid
}

/// [`merged_safe_config`] with the host's Unraid-ness and tmpfs probe given.
fn merged_safe_config_for(
    dir: &Path,
    unraid: bool,
    tmpfs: fn(&Path) -> bool,
) -> Result<NutConfig, String> {
    let mut cfg = NutConfig::read_existing(dir, unraid)?;
    // Intentional: the safe-config repairs always enforce kill-power; turning it
    // off is a deliberate ups config_set, not a repair.
    cfg.upsmon.kill_power = true;
    if cfg.upsmon.min_supplies == 0 {
        cfg.upsmon.min_supplies = 1;
    }
    // Enforce a completing, force-unmount-aware SHUTDOWNCMD — a preserved unsafe
    // one (bare halt/poweroff) is what wedges the array unmount.
    if shutdown_cmd_unsafe(&cfg.upsmon.shutdown_cmd) {
        cfg.upsmon.shutdown_cmd = UpsmonSettings::default().shutdown_cmd;
    }
    // On Unraid only /boot survives the power-cycle; elsewhere any path off tmpfs does.
    let log = Path::new(&cfg.notify.log_path);
    let lost = if unraid {
        !log.starts_with("/boot")
    } else {
        !log.is_absolute() || tmpfs(log)
    };
    if lost {
        cfg.notify.log_path = NotifySettings::for_host(unraid).log_path;
    }
    Ok(cfg)
}

const TMPFS_PREFIXES: [&str; 3] = ["/tmp", "/run", "/dev/shm"];

/// Whether `path` lives on tmpfs, judged by statfs on its nearest existing
/// ancestor; the well-known tmpfs prefixes when statfs cannot answer.
fn on_tmpfs(path: &Path) -> bool {
    tmpfs_by(path, statfs_tmpfs)
}

fn tmpfs_by(path: &Path, probe: impl Fn(&Path) -> Option<bool>) -> bool {
    path.ancestors()
        .find(|p| p.exists())
        .and_then(probe)
        .unwrap_or_else(|| prefix_tmpfs(path))
}

fn prefix_tmpfs(path: &Path) -> bool {
    TMPFS_PREFIXES.iter().any(|t| path.starts_with(t))
}

#[cfg(target_os = "linux")]
fn statfs_tmpfs(path: &Path) -> Option<bool> {
    use std::os::unix::ffi::OsStrExt;
    const TMPFS_MAGIC: i64 = 0x0102_1994;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: `c` is a valid NUL-terminated path and `st` a writable statfs.
    let mut st: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(c.as_ptr(), &mut st) } != 0 {
        return None;
    }
    // `f_type`'s integer type differs between glibc and musl.
    #[allow(clippy::unnecessary_cast)]
    let f_type = st.f_type as i64;
    Some(f_type == TMPFS_MAGIC)
}

#[cfg(not(target_os = "linux"))]
fn statfs_tmpfs(_path: &Path) -> Option<bool> {
    None
}

/// Parse `upsmon.conf` into the fields the checks read. `Ok(None)` when the
/// file is absent (NUT not set up as a monitor here); `Err` when a modelled
/// setting is present but does not parse, so callers never act on a default.
pub(crate) fn read_upsmon(dir: &Path) -> Result<Option<UpsmonSettings>, String> {
    let path = dir.join("upsmon.conf");
    let text = match fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("read {}: {e}", path.display())),
    };
    let mut s = UpsmonSettings::default();
    let mut saw_flag = false;
    for line in text.lines() {
        let f = tokenize(line);
        let Some(key) = f.first().map(String::as_str) else {
            continue;
        };
        if !matches!(
            key,
            "MINSUPPLIES" | "SHUTDOWNCMD" | "POWERDOWNFLAG" | "HOSTSYNC" | "DEADTIME"
        ) {
            continue;
        }
        let bad = || {
            format!(
                "cannot parse upsmon.conf line `{}`",
                strip_comment(line).trim()
            )
        };
        let [_, value] = f.as_slice() else {
            return Err(bad());
        };
        let num = || value.parse::<u32>().map_err(|_| bad());
        match key {
            "MINSUPPLIES" => s.min_supplies = num()?,
            "SHUTDOWNCMD" => s.shutdown_cmd = value.clone(),
            "POWERDOWNFLAG" => {
                s.power_down_flag = value.clone();
                saw_flag = true;
            }
            "HOSTSYNC" => s.host_sync = num()?,
            _ => s.dead_time = num()?,
        }
    }
    // kill_power is on only when a POWERDOWNFLAG is actually declared.
    s.kill_power = saw_flag;
    Ok(Some(s))
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
pub(crate) fn run(bin: &str, args: &[&str]) -> Result<String, String> {
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

    #[test]
    fn notify_handler_check() {
        let upsmon = "NOTIFYCMD /usr/local/bin/orca-nut-notify\n";
        let f = check_notify_handler(upsmon, |_| true).unwrap();
        assert_eq!(f.severity, Severity::Ok);
        let f = check_notify_handler(upsmon, |_| false).unwrap();
        assert_eq!(f.severity, Severity::Crit);
        assert_eq!(f.repair.unwrap().id, "notify-handler");
        let f = check_notify_handler("NOTIFYCMD /opt/mine.sh\n", |_| false).unwrap();
        assert!(f.repair.is_none());
        assert!(check_notify_handler("MINSUPPLIES 1\n", |_| false).is_none());
    }

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
        let s = read_upsmon(dir.path()).unwrap().expect("parsed");
        assert_eq!(s.min_supplies, 0);
        assert_eq!(s.shutdown_cmd, "poweroff");
        assert!(!s.kill_power, "no POWERDOWNFLAG → kill_power off");
    }

    #[test]
    fn battery_thresholds_flags_minsupplies_zero() {
        let s = read_upsmon(write_upsmon(UPSMON_UNSAFE).path()).unwrap();
        let f = check_battery_thresholds(&s, None).expect("finding");
        assert_eq!(f.severity, Severity::Warn);
        assert_eq!(f.id, "battery-thresholds");
        assert!(f.repair.is_some());
    }

    #[test]
    fn battery_thresholds_ok_when_safe() {
        let s = read_upsmon(write_upsmon(UPSMON_SAFE).path()).unwrap();
        let f = check_battery_thresholds(&s, None).expect("finding");
        // No live upsd in the test env, so it only checks MINSUPPLIES here.
        assert_eq!(f.severity, Severity::Ok);
        assert!(f.repair.is_none());
    }

    #[test]
    fn kill_power_warns_without_flag() {
        let s = read_upsmon(write_upsmon(UPSMON_UNSAFE).path()).unwrap();
        let f = check_kill_power(&s).expect("finding");
        assert_eq!(f.severity, Severity::Warn);
        assert!(f.repair.is_some());
    }

    #[test]
    fn kill_power_ok_with_flag() {
        let s = read_upsmon(write_upsmon(UPSMON_SAFE).path()).unwrap();
        let f = check_kill_power(&s).expect("finding");
        assert_eq!(f.severity, Severity::Ok);
        assert!(f.repair.is_none());
    }

    #[test]
    fn shutdown_command_warns_with_in_place_repair() {
        let s = read_upsmon(write_upsmon(UPSMON_UNSAFE).path()).unwrap();
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
        let s = read_upsmon(write_upsmon(UPSMON_UNSAFE).path()).unwrap();
        assert!(shutdown_cmd_unsafe(&s.as_ref().unwrap().shutdown_cmd));
        let dir = write_upsmon(UPSMON_UNSAFE);
        let cfg = merged_safe_config_for(dir.path(), false, |_| false).unwrap();
        assert!(!shutdown_cmd_unsafe(&cfg.upsmon.shutdown_cmd));
    }

    #[test]
    fn shutdown_command_ok_with_completing_cmd() {
        let s = read_upsmon(write_upsmon(UPSMON_SAFE).path()).unwrap();
        let f = check_shutdown_command(&s).expect("finding");
        assert_eq!(f.severity, Severity::Ok);
        assert!(f.repair.is_none());
    }

    #[test]
    fn persistent_event_log_warns_when_missing() {
        let s = read_upsmon(write_upsmon(UPSMON_SAFE).path()).unwrap();
        let f = check_persistent_event_log(&s, "", true, prefix_tmpfs).expect("finding");
        assert_eq!(f.severity, Severity::Warn);
        assert_eq!(f.id, "persistent-event-log");
    }

    #[test]
    fn persistent_event_log_ok_for_managed_upssched() {
        // A managed upssched.conf carries the flash + remote markers.
        let upssched = NutConfig {
            notify: NotifySettings::for_host(true),
            ..Default::default()
        };
        let rendered = upssched
            .render()
            .into_iter()
            .find(|f| f.name == "upssched.conf")
            .unwrap()
            .contents;
        let s = read_upsmon(write_upsmon(UPSMON_SAFE).path()).unwrap();
        let f = check_persistent_event_log(&s, &rendered, true, prefix_tmpfs).expect("finding");
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
        let o = repair_typed(RepairArgs {
            provider: "nut".to_string(),
            repair_id: "nope".to_string(),
            confirm: false,
        });
        assert!(!o.ok);
        assert!(o.message.contains("no in-place repair"));
    }

    #[test]
    fn upssched_repairs_refuse_custom_upssched() {
        let dir = tempfile::tempdir().unwrap();
        let custom = "CMDSCRIPT /usr/bin/custom\n";
        std::fs::write(dir.path().join("upssched.conf"), custom).unwrap();
        std::fs::write(
            dir.path().join("upsmon.conf"),
            "MONITOR serverups@10.0.0.2 1 monuser secret slave\n",
        )
        .unwrap();
        for what in ["persistent + remote event logging", "on-battery capture"] {
            let (ok, msg) = repair_upssched_config(dir.path(), false, what);
            assert!(!ok);
            assert!(msg.contains("replace_custom_upssched: true"), "{msg}");
        }
        assert_eq!(
            std::fs::read_to_string(dir.path().join("upssched.conf")).unwrap(),
            custom
        );
    }

    #[test]
    fn upsmon_trailing_comments_parse() {
        let dir = write_upsmon(
            "MINSUPPLIES 2 # two PSUs\nSHUTDOWNCMD \"/sbin/shutdown -h +0\" # safe\n\
             # SHUTDOWNCMD halt\nHOSTSYNC 20 # sync\n",
        );
        let s = read_upsmon(dir.path()).unwrap().unwrap();
        assert_eq!(s.min_supplies, 2);
        assert_eq!(s.shutdown_cmd, "/sbin/shutdown -h +0");
        assert_eq!(s.host_sync, 20);
    }

    #[test]
    fn unparseable_upsmon_setting_fails_closed() {
        for bad in [
            "MINSUPPLIES two\n",
            "MINSUPPLIES\n",
            "HOSTSYNC 1 2\n",
            "DEADTIME -5\n",
            "SHUTDOWNCMD /sbin/shutdown -h +0\n",
        ] {
            let dir = write_upsmon(bad);
            assert!(read_upsmon(dir.path()).is_err(), "{bad}");
            assert!(
                merged_safe_config_for(dir.path(), false, |_| false).is_err(),
                "{bad}"
            );
        }
    }

    #[test]
    fn safe_config_keeps_notify_targets() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = NutConfig {
            upses: vec![crate::config::UpsDef {
                name: "serverups".into(),
                driver: "usbhid-ups".into(),
                port: "auto".into(),
                ..Default::default()
            }],
            monitors: vec![crate::config::MonitorDef {
                system: "serverups@localhost".into(),
                power_value: 1,
                username: "u".into(),
                password: "p".into(),
                role: "master".into(),
            }],
            ..Default::default()
        };
        cfg.notify.notify_cmd = "/usr/local/bin/mine".into();
        cfg.notify.remote_syslog = "10.0.0.9:514".into();
        for f in cfg.render() {
            fs::write(dir.path().join(f.name), f.contents).unwrap();
        }
        let merged = merged_safe_config_for(dir.path(), false, |_| false).unwrap();
        assert_eq!(merged.notify, cfg.notify);
    }

    #[tokio::test]
    async fn diagnose_emits_typed_findings() {
        let findings = diagnose_typed(DiagnoseArgs::default()).await;
        for f in &findings {
            assert_eq!(f.provider, crate::PROVIDER);
        }
    }

    #[test]
    fn flash_log_path_enforced_only_on_unraid() {
        let dir = write_upsmon(UPSMON_SAFE);
        let sched = "# Managed by orca (nut plugin). Do not edit by hand.\n\
                     # orca-log-path /var/log/nut/events.log\n";
        fs::write(dir.path().join("upssched.conf"), sched).unwrap();
        let keep = merged_safe_config_for(dir.path(), false, prefix_tmpfs).unwrap();
        assert_eq!(keep.notify.log_path, "/var/log/nut/events.log");
        let unraid = merged_safe_config_for(dir.path(), true, prefix_tmpfs).unwrap();
        assert_eq!(
            unraid.notify.log_path,
            NotifySettings::for_host(true).log_path
        );
    }

    #[test]
    fn unraid_log_path_must_be_under_boot() {
        let s = read_upsmon(write_upsmon(UPSMON_SAFE).path()).unwrap();
        for (path, ok) in [
            ("/boot/config/plugins/nut/events.log", true),
            ("/tmp/boot/events.log", false),
            ("/bootleg/events.log", false),
        ] {
            let sched = format!("# orca-log-path {path}\n# orca-remote-syslog 10.0.0.9:514\n");
            let f = check_persistent_event_log(&s, &sched, true, prefix_tmpfs).expect("finding");
            assert_eq!(f.severity == Severity::Ok, ok, "{path}");
        }
    }

    #[test]
    fn non_unraid_log_path_must_be_off_tmpfs() {
        let s = read_upsmon(write_upsmon(UPSMON_SAFE).path()).unwrap();
        for (path, ok) in [
            ("/var/log/nut/events.log", true),
            ("/boot/config/plugins/nut/events.log", true),
            ("/tmp/events.log", false),
            ("/run/nut/events.log", false),
            ("/dev/shm/events.log", false),
        ] {
            let sched = format!("# orca-log-path {path}\n# orca-remote-syslog 10.0.0.9:514\n");
            let f = check_persistent_event_log(&s, &sched, false, prefix_tmpfs).expect("finding");
            assert_eq!(f.severity == Severity::Ok, ok, "{path}");
        }
    }

    #[test]
    fn unreadable_upsmon_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("upsmon.conf")).unwrap();
        assert!(read_upsmon(dir.path()).is_err());
    }

    #[test]
    fn tmpfs_probe_walks_to_existing_ancestor_and_falls_back() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("a/b/events.log");
        let seen = std::cell::RefCell::new(None);
        assert!(tmpfs_by(&missing, |p| {
            *seen.borrow_mut() = Some(p.to_path_buf());
            Some(true)
        }));
        assert_eq!(seen.into_inner().as_deref(), Some(dir.path()));
        assert!(!tmpfs_by(Path::new("/tmp/x"), |_| Some(false)));
        assert!(tmpfs_by(Path::new("/tmp/x"), |_| None));
        assert!(!tmpfs_by(Path::new("/var/log/x"), |_| None));
    }

    #[test]
    fn non_unraid_repair_resets_tmpfs_log_path() {
        let dir = write_upsmon(UPSMON_SAFE);
        let sched = "# Managed by orca (nut plugin). Do not edit by hand.\n\
                     # orca-log-path /run/nut/events.log\n";
        fs::write(dir.path().join("upssched.conf"), sched).unwrap();
        let cfg = merged_safe_config_for(dir.path(), false, prefix_tmpfs).unwrap();
        assert_eq!(cfg.notify.log_path, "/var/log/nut/events.log");
        let cfg = merged_safe_config_for(dir.path(), false, |_| true).unwrap();
        assert_eq!(cfg.notify.log_path, "/var/log/nut/events.log");
    }

    #[test]
    fn non_unraid_defaults_use_var_log() {
        let n = NotifySettings::for_host(false);
        assert_eq!(n.log_path, "/var/log/nut/events.log");
        assert!(n
            .onbattery_capture_cmd
            .ends_with(">> /var/log/nut/onbattery.json"));
        assert!(NotifySettings::for_host(true)
            .log_path
            .starts_with("/boot/"));
    }

    #[test]
    fn statfs_detects_tmpfs_where_available() {
        if Path::new("/dev/shm").is_dir() {
            if let Some(t) = statfs_tmpfs(Path::new("/dev/shm")) {
                assert!(t);
            }
        }
    }
}
