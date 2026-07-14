//! Diagnostics-domain registration for the hybrid nut export.
//!
//! Besides its `service` backend, nut contributes a `diagnostics` provider
//! (`nut.__diag.<op>`) exposing `diagnose` and `repair`. orca's loader installs
//! a `DiagnosticsProxy` that routes those two ops back through the FFI `invoke`;
//! [`diag_dispatch`] answers them. The `main.rs` entrypoint merges this backend
//! def with the service backend def and routes both dispatch prefixes.

use plugin_toolkit::abi::BackendDef;
use plugin_toolkit::serde_json;

/// Invoke prefix the diagnostics proxy calls back through.
pub const DIAG_PREFIX: &str = "nut.__diag";

/// The `diagnostics` backend descriptor this plugin advertises.
pub fn diagnostics_backend_def() -> BackendDef {
    BackendDef {
        domain: "diagnostics".to_string(),
        name: crate::PROVIDER.to_string(),
        invoke_prefix: DIAG_PREFIX.to_string(),
        ..Default::default()
    }
}

/// Answer `nut.__diag.{diagnose,repair}` calls the loader's proxy makes. Returns
/// `None` for any name outside the diagnostics prefix (so the caller can fall
/// through to the service backend dispatch).
pub fn diag_dispatch(name: &str, args_json: &str) -> Option<Result<String, String>> {
    let op = name.strip_prefix(DIAG_PREFIX)?.strip_prefix('.')?;
    Some(match op {
        "diagnose" => crate::checks::diagnose(args_json),
        "repair" => crate::checks::repair(args_json),
        other => Err(format!("nut: unknown diagnostics op '{other}'")),
    })
}

/// Merge the service backend def with the diagnostics backend def into the
/// single `backends()` JSON array the plugin advertises. `service_json` is the
/// service backend's own one-element array (from `service_backends_json`).
pub fn merged_backends_json(service_json: &str) -> String {
    let mut defs: Vec<BackendDef> = serde_json::from_str(service_json).unwrap_or_default();
    defs.push(diagnostics_backend_def());
    serde_json::to_string(&defs).unwrap_or_else(|_| service_json.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diag_dispatch_ignores_foreign_names() {
        assert!(diag_dispatch("service.__backend.nut.status", "{}").is_none());
    }

    #[test]
    fn diag_dispatch_answers_diagnose() {
        let out = diag_dispatch("nut.__diag.diagnose", "{}").expect("owned");
        assert!(out.is_ok());
    }

    #[test]
    fn merged_backends_includes_both_domains() {
        let service =
            r#"[{"domain":"service","name":"nut","invoke_prefix":"service.__backend.nut"}]"#;
        let merged = merged_backends_json(service);
        assert!(merged.contains("\"domain\":\"service\""));
        assert!(merged.contains("\"domain\":\"diagnostics\""));
    }
}
