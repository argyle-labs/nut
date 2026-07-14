//! nut as a core `ups` capability provider.
//!
//! Implements the three [`plugin_toolkit::contract::ups`] ops the loader's
//! `UpsProxy` drives over the FFI boundary — `state`, `config_get`, `config_set`
//! — reusing [`crate::client`] (live upsd reads) and [`crate::config`] (upsmon
//! rendering). This is *one* provider of the core capability; the unraid plugin
//! is another. The diagnostics checks stay; both read the same underlying UPS.

use std::env;

use plugin_toolkit::contract::ups::{UpsConfig, UpsConfigOutcome, UpsQueryArgs, UpsState};
use plugin_toolkit::reactor;
use plugin_toolkit::serde_json;

use crate::client::{NutClient, Ups, DEFAULT_PORT};
use crate::config::{detect_conf_dir, NutConfig};

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

fn parse_query(args_json: &str) -> UpsQueryArgs {
    if args_json.trim().is_empty() {
        UpsQueryArgs::default()
    } else {
        serde_json::from_str(args_json).unwrap_or_default()
    }
}

/// `state` op — live UPS readings from `upsd`.
pub fn state(args_json: &str) -> Result<String, String> {
    let args = parse_query(args_json);
    let states: Vec<UpsState> = reactor::block_on(async {
        let mut client = NutClient::connect(&upsd_host(), DEFAULT_PORT).await?;
        let upses = client.list_upses().await?;
        Ok::<Vec<UpsState>, String>(upses.iter().map(to_state).collect())
    })?
    .into_iter()
    .filter(|s| args.id.as_ref().is_none_or(|id| &s.id == id))
    .collect();
    serde_json::to_string(&states).map_err(|e| format!("encode ups state: {e}"))
}

/// `config_get` op — the power/shutdown knobs nut manages (kill-power +
/// SHUTDOWNCMD). Threshold fields are apcupsd-shaped and left `None` here; the
/// unraid provider fills those for Unraid hosts.
pub fn config_get(args_json: &str) -> Result<String, String> {
    let args = parse_query(args_json);
    let dir = detect_conf_dir();
    let upsmon = crate::checks::read_upsmon(&dir).unwrap_or_default();
    let id = args.id.unwrap_or_else(|| "default".to_string());
    let cfg = UpsConfig {
        id,
        kill_power: Some(upsmon.kill_power),
        shutdown_cmd: (!upsmon.shutdown_cmd.trim().is_empty()).then(|| upsmon.shutdown_cmd.clone()),
        ..Default::default()
    };
    serde_json::to_string(&vec![cfg]).map_err(|e| format!("encode ups config: {e}"))
}

/// `config_set` op — apply kill-power / SHUTDOWNCMD onto upsmon.conf idempotently,
/// merging onto whatever is already configured so an operator's UPS/monitor
/// topology is preserved.
pub fn config_set(args_json: &str) -> Result<String, String> {
    let cfg: UpsConfig =
        serde_json::from_str(args_json).map_err(|e| format!("invalid ups config: {e}"))?;
    let dir = detect_conf_dir();
    let mut nut = NutConfig {
        upsmon: crate::checks::read_upsmon(&dir).unwrap_or_default(),
        ..Default::default()
    };
    if let Some(k) = cfg.kill_power {
        nut.upsmon.kill_power = k;
    }
    if let Some(s) = &cfg.shutdown_cmd {
        if !s.trim().is_empty() {
            nut.upsmon.shutdown_cmd = s.clone();
        }
    }
    let outcome = match nut.apply(&dir) {
        Ok(written) if written.is_empty() => UpsConfigOutcome {
            id: cfg.id,
            provider: crate::PROVIDER.to_string(),
            ok: true,
            message: format!("config already in place at {}", dir.display()),
            restart_required: false,
        },
        Ok(written) => UpsConfigOutcome {
            id: cfg.id,
            provider: crate::PROVIDER.to_string(),
            ok: true,
            message: format!("wrote {} in {}", written.join(", "), dir.display()),
            restart_required: true,
        },
        Err(e) => UpsConfigOutcome {
            id: cfg.id,
            provider: crate::PROVIDER.to_string(),
            ok: false,
            message: format!(
                "failed to apply in {} ({e}); needs privilege",
                dir.display()
            ),
            restart_required: false,
        },
    };
    serde_json::to_string(&outcome).map_err(|e| format!("encode ups outcome: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_get_reports_kill_power_default() {
        // No config dir → defaults; kill_power is always reported (Some).
        let out = config_get("{}").expect("ok");
        assert!(out.contains("\"kill_power\""));
    }

    #[test]
    fn config_set_rejects_bad_json() {
        assert!(config_set("not json").is_err());
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
