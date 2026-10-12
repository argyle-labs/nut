//! nut as a core `ups` capability provider.
//!
//! Implements the three [`plugin_toolkit::contract::ups`] ops the loader's
//! `UpsProxy` drives over the FFI boundary — `state`, `config_get`, `config_set`
//! — reusing [`crate::client`] (live upsd reads) and [`crate::config`] (upsmon
//! rendering). This is *one* provider of the core capability; the unraid plugin
//! is another. The diagnostics checks stay; both read the same underlying UPS.

use std::env;
use std::path::Path;

use plugin_toolkit::contract::ups::{UpsConfig, UpsConfigOutcome, UpsQueryArgs, UpsState};

use crate::client::{NutClient, Ups, DEFAULT_PORT};
use crate::config::{detect_conf_dir, detect_init, InitSystem, NutConfig};

/// Host `upsd` is reached at. Loopback by default; overridable for a
/// monitor-only client that watches a remote `upsd`.
fn upsd_host() -> String {
    env::var("NUT_UPSD_HOST").unwrap_or_else(|_| "127.0.0.1".to_string())
}

/// Map a live [`Ups`] onto the core [`UpsState`].
fn to_state(u: &Ups) -> UpsState {
    let status = u.status().unwrap_or_default().to_string();
    let low_battery = status.split_whitespace().any(|f| f == "LB");
    UpsState {
        provider: crate::PROVIDER.to_string(),
        id: if u.name.is_empty() {
            "default".to_string()
        } else {
            u.name.clone()
        },
        model: (!u.description.is_empty()).then(|| u.description.clone()),
        battery_charge: u.battery_charge(),
        battery_runtime_ms: u.battery_runtime().map(|s| (s * 1000.0) as i64),
        input_voltage: u.input_voltage(),
        load_percent: u.vars.get("ups.load").and_then(|v| v.parse().ok()),
        on_battery: u.on_battery(),
        low_battery,
        status,
    }
}

/// `state` op — live UPS readings from `upsd`. Awaited directly on the shared
/// reactor (the provider future already runs under it).
pub async fn state_typed(args: UpsQueryArgs) -> Result<Vec<UpsState>, String> {
    let mut client = NutClient::connect(&upsd_host(), DEFAULT_PORT).await?;
    let upses = client.list_upses().await?;
    Ok(upses
        .iter()
        .map(to_state)
        .filter(|s| args.id.as_ref().is_none_or(|id| &s.id == id))
        .collect())
}

/// `config_get` op — the power/shutdown knobs nut manages (kill-power +
/// SHUTDOWNCMD). Threshold fields are apcupsd-shaped and left `None` here; the
/// unraid provider fills those for Unraid hosts.
pub fn config_get_typed(args: UpsQueryArgs) -> Result<Vec<UpsConfig>, String> {
    config_get_in(&detect_conf_dir(), args)
}

/// [`config_get_typed`] against `dir`; settings are absent when no upsmon.conf exists.
fn config_get_in(dir: &std::path::Path, args: UpsQueryArgs) -> Result<Vec<UpsConfig>, String> {
    let upsmon = crate::checks::read_upsmon(dir)?;
    let id = args.id.unwrap_or_else(|| "default".to_string());
    Ok(vec![UpsConfig {
        id,
        kill_power: upsmon.as_ref().map(|u| u.kill_power),
        shutdown_cmd: upsmon
            .map(|u| u.shutdown_cmd)
            .filter(|c| !c.trim().is_empty()),
        ..Default::default()
    }])
}

/// The on-disk config with `cfg`'s kill-power / SHUTDOWNCMD applied.
fn merged_config(dir: &Path, cfg: &UpsConfig, unraid: bool) -> Result<NutConfig, String> {
    let mut nut = NutConfig::read_existing(dir, unraid)?;
    if let Some(k) = cfg.kill_power {
        nut.upsmon.kill_power = k;
    }
    if let Some(s) = &cfg.shutdown_cmd {
        crate::config::check_value("SHUTDOWNCMD", s)?;
        if !s.trim().is_empty() {
            nut.upsmon.shutdown_cmd = s.clone();
        }
    }
    Ok(nut)
}

/// `config_set` op — apply kill-power / SHUTDOWNCMD onto upsmon.conf idempotently,
/// merging onto whatever is already configured so an operator's UPS/monitor
/// topology is preserved.
pub fn config_set_typed(cfg: UpsConfig) -> UpsConfigOutcome {
    let unraid = detect_init(Path::new("/")) == InitSystem::Unraid;
    config_set_in(&detect_conf_dir(), unraid, cfg, |nut, dir| nut.apply(dir))
}

/// [`config_set_typed`] against `dir` with an injectable `apply`.
fn config_set_in(
    dir: &Path,
    unraid: bool,
    cfg: UpsConfig,
    apply: impl FnOnce(&NutConfig, &Path) -> Result<crate::config::ApplyOutcome, String>,
) -> UpsConfigOutcome {
    match merged_config(dir, &cfg, unraid).and_then(|nut| apply(&nut, dir)) {
        Ok(out) if out.written.is_empty() => UpsConfigOutcome {
            id: cfg.id,
            provider: crate::PROVIDER.to_string(),
            ok: true,
            message: if out.message.is_empty() {
                format!("config already in place at {}", dir.display())
            } else {
                format!(
                    "config already in place at {}; {}",
                    dir.display(),
                    out.message
                )
            },
            restart_required: false,
        },
        Ok(out) => UpsConfigOutcome {
            id: cfg.id,
            provider: crate::PROVIDER.to_string(),
            ok: true,
            message: format!(
                "wrote {} in {}; {}",
                out.written.join(", "),
                dir.display(),
                out.message
            ),
            restart_required: out.restart_required,
        },
        Err(e) => UpsConfigOutcome {
            id: cfg.id,
            provider: crate::PROVIDER.to_string(),
            ok: false,
            message: format!(
                "failed to apply in {} ({e}){}",
                dir.display(),
                crate::config::privilege_hint(&e)
            ),
            restart_required: false,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_get_absent_without_upsmon() {
        let dir = tempfile::tempdir().unwrap();
        let cfgs = config_get_in(dir.path(), UpsQueryArgs::default()).unwrap();
        assert_eq!(cfgs[0].kill_power, None);
        assert_eq!(cfgs[0].shutdown_cmd, None);
    }

    #[test]
    fn config_get_reports_on_disk_kill_power() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("upsmon.conf"), "SHUTDOWNCMD halt\n").unwrap();
        let cfgs = config_get_in(dir.path(), UpsQueryArgs::default()).unwrap();
        assert_eq!(cfgs[0].kill_power, Some(false));
        assert_eq!(cfgs[0].shutdown_cmd.as_deref(), Some("halt"));
    }

    #[test]
    fn config_get_propagates_read_errors() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("upsmon.conf")).unwrap();
        assert!(config_get_in(dir.path(), UpsQueryArgs::default()).is_err());
    }

    #[test]
    fn config_set_refuses_newline_in_shutdown_cmd() {
        let dir = tempfile::tempdir().unwrap();
        let err = merged_config(
            dir.path(),
            &UpsConfig {
                shutdown_cmd: Some("/sbin/halt\nMONITOR x@y 1 u p master".into()),
                ..Default::default()
            },
            false,
        )
        .unwrap_err();
        assert!(err.contains("control character"), "{err}");
    }

    #[test]
    fn config_set_keeps_on_disk_topology() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("ups.conf"),
            "[serverups]\n    driver = usbhid-ups\n    port = auto\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("upsd.conf"), "LISTEN 0.0.0.0 3493\n").unwrap();
        std::fs::write(
            dir.path().join("upsmon.conf"),
            "MONITOR serverups@localhost 1 monuser secretref master\nSHUTDOWNCMD \"/sbin/halt\"\n",
        )
        .unwrap();
        let nut = merged_config(
            dir.path(),
            &UpsConfig {
                shutdown_cmd: Some("/sbin/shutdown -h +0".into()),
                ..Default::default()
            },
            false,
        )
        .unwrap();
        assert_eq!(nut.upses.len(), 1);
        assert_eq!(nut.upses[0].name, "serverups");
        assert_eq!(nut.listen.len(), 1);
        assert_eq!(nut.monitors.len(), 1);
        assert_eq!(nut.upsmon.shutdown_cmd, "/sbin/shutdown -h +0");
    }

    #[test]
    fn config_set_keeps_notifycmd_and_syslog_target() {
        let dir = tempfile::tempdir().unwrap();
        let mut nut = NutConfig {
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
        nut.notify.notify_cmd = "/usr/local/bin/mine".into();
        nut.notify.remote_syslog = "10.0.0.9:514".into();
        for f in nut.render() {
            std::fs::write(dir.path().join(f.name), f.contents).unwrap();
        }
        let before = |n: &str| std::fs::read_to_string(dir.path().join(n)).unwrap();
        let (mon, sched) = (before("upsmon.conf"), before("upssched.conf"));
        let merged = merged_config(
            dir.path(),
            &UpsConfig {
                kill_power: Some(false),
                ..Default::default()
            },
            false,
        )
        .unwrap();
        struct NoRestart;
        impl crate::config::ServiceControl for NoRestart {
            fn restart(&self, _: bool) -> crate::config::Restart {
                crate::config::Restart::Done(vec![])
            }
        }
        let out = merged
            .apply_with(dir.path(), &NoRestart, std::path::Path::new("/nonexistent"))
            .unwrap();
        assert_eq!(out.written, vec!["upsmon.conf"]);
        let mon_after = before("upsmon.conf");
        assert!(
            mon_after.contains("NOTIFYCMD /usr/local/bin/mine"),
            "{mon_after}"
        );
        assert_ne!(mon_after, mon);
        assert_eq!(before("upssched.conf"), sched);
        assert!(sched.contains("# orca-remote-syslog 10.0.0.9:514"));
    }

    #[test]
    fn config_set_writes_nothing_when_the_handler_install_fails() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("upsmon.conf"),
            "MONITOR serverups@localhost 1 u p master\nSHUTDOWNCMD \"/sbin/halt\"\n",
        )
        .unwrap();
        let before = std::fs::read_dir(dir.path()).unwrap().count();
        let out = config_set_in(
            dir.path(),
            false,
            UpsConfig {
                kill_power: Some(true),
                ..Default::default()
            },
            |nut, d| {
                nut.apply_gated(d, &|_| Err("denied".into()), |_| {
                    panic!("wrote despite a failed install")
                })
            },
        );
        assert!(!out.ok);
        assert!(
            out.message.contains("notify handler not installed"),
            "{}",
            out.message
        );
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), before);
    }

    #[test]
    fn to_state_derives_low_battery_flag() {
        let mut u = Ups {
            name: "ups".into(),
            ..Default::default()
        };
        u.vars.insert("ups.status".into(), "OB LB".into());
        let s = to_state(&u);
        assert!(s.on_battery);
        assert!(s.low_battery);
    }
}
