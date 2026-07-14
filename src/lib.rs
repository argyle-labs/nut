//! nut service backend — Network UPS Tools.
//!
//! Implements `ServiceBackend` so the generic `service.*` tools
//! (deploy/backup/restore/configure/status/connect/sync) drive nut, and a
//! `diagnostics` provider ([`checks`]) that detects and repairs the UPS-side
//! conditions that make a power-loss shutdown hang or lose its evidence. No
//! `#[orca_tool]`s — the only orca dep is `plugin-toolkit`. Modeled on the nfs
//! StorageBackend + the raccoon diagnostics provider. See orca/docs/PLUGIN-PROGRAM.md.
#![allow(clippy::disallowed_types)]

use plugin_toolkit::deploy_target::EnvVar;
use plugin_toolkit::serde_json;
use plugin_toolkit::service::{
    BoxFuture, Endpoint, Runtime, ServiceBackend, ServiceCapability, ServiceError, ServiceStatus,
    WorkloadSpec,
};

pub mod checks;
pub mod client;
pub mod config;
pub mod registration;
pub mod ups;

use crate::client::{NutClient, DEFAULT_PORT};
use crate::config::{detect_conf_dir, NutConfig};

/// Registry name this plugin uses across the service + diagnostics domains.
pub const PROVIDER: &str = "nut";

/// Upstream NUT container image. The concrete tag is host/hardware-specific
/// (USB/serial passthrough), so the operator overrides it per deployment; this
/// is the runtime-agnostic default the deploy target renders.
const NUT_IMAGE: &str = "docker.io/instantlinux/nut-upsd:latest";

/// nut backend. Holds only the provider name; per-instance endpoint/creds
/// come from the `Endpoint` the generic `service.*` tools hand each op.
#[derive(Debug, Clone)]
pub struct NutBackend {
    provider: &'static str,
}

impl NutBackend {
    pub fn new(provider: &'static str) -> Self {
        Self { provider }
    }
}

impl ServiceBackend for NutBackend {
    fn provider(&self) -> &str {
        self.provider
    }

    /// Runtimes nut can be placed on. `service.deploy` hands the
    /// `workload_spec` below to a matching deploy target — this backend never
    /// drives pct/docker itself (that mechanic lives in the deploy-target domain).
    fn runtimes(&self) -> Vec<Runtime> {
        vec![Runtime::Docker, Runtime::Lxc]
    }

    fn capabilities(&self) -> Vec<ServiceCapability> {
        vec![
            ServiceCapability::Deploy,
            ServiceCapability::Backup,
            ServiceCapability::Restore,
            ServiceCapability::Configure,
            ServiceCapability::Status,
        ]
    }

    fn default_port(&self) -> u16 {
        DEFAULT_PORT
    }

    /// In-workload paths holding config/data. This is ALL nut declares for
    /// backup — the generic pluggable backup (tar for containers/LXC, PBS for
    /// Proxmox guests when available) snapshots these. No backup/restore code
    /// here; those are inherited from ServiceBackend's defaults.
    fn data_paths(&self) -> Vec<String> {
        vec!["/etc/nut".to_string()]
    }

    /// The runtime-agnostic nut workload. NUT runs in one of two roles:
    /// - **netserver** — drives the UPS hardware and serves `upsd` on 3493; the
    ///   host must pass the UPS device through (USB/serial). This is the default.
    /// - **monitor-only client** — no local UPS; watches a remote `upsd`. The
    ///   deployer signals this with `MODE=netclient` on the endpoint, and no
    ///   port is published.
    ///
    /// The concrete UPS device and driver are host-specific and applied via
    /// [`configure`](Self::configure) after launch, so the spec stays portable.
    fn workload_spec<'a>(
        &'a self,
        _runtime: Runtime,
        ep: &'a Endpoint,
    ) -> BoxFuture<'a, Result<WorkloadSpec, ServiceError>> {
        Box::pin(async move {
            // A monitor-only client publishes no port and needs no device.
            let netclient = ep.base_url.contains("netclient");
            let ports = if netclient {
                Vec::new()
            } else {
                vec![format!("{DEFAULT_PORT}:{DEFAULT_PORT}")]
            };
            Ok(WorkloadSpec {
                name: if ep.name.is_empty() {
                    PROVIDER.to_string()
                } else {
                    ep.name.clone()
                },
                image: Some(NUT_IMAGE.to_string()),
                env: vec![EnvVar {
                    key: "MODE".to_string(),
                    value: if netclient { "netclient" } else { "netserver" }.to_string(),
                }],
                // Config lives on a persisted volume; UPS device passthrough is
                // host-specific and layered on by the deploy target/operator.
                mounts: Vec::new(),
                ports,
            })
        })
    }

    /// Apply nut config idempotently. `config` is a JSON [`NutConfig`] (multi-UPS
    /// and monitor-only clients supported); rendering the four `upsd`/`upsmon`
    /// files is deterministic and only rewrites what changed.
    fn configure<'a>(
        &'a self,
        _ep: &'a Endpoint,
        config: &'a str,
    ) -> BoxFuture<'a, Result<(), ServiceError>> {
        Box::pin(async move {
            let cfg: NutConfig = serde_json::from_str(config)
                .map_err(|e| ServiceError::Other(format!("invalid nut config JSON: {e}")))?;
            let dir = detect_conf_dir();
            cfg.apply(&dir).map_err(ServiceError::Other)?;
            Ok(())
        })
    }

    /// Real health: connect to `upsd` and summarize every UPS it reports. The
    /// rich per-UPS detail rides `ServiceStatus.detail` (the `ServiceInfo` typed
    /// enum is owned centrally by orca and has no nut variant yet).
    fn status<'a>(
        &'a self,
        ep: &'a Endpoint,
    ) -> BoxFuture<'a, Result<ServiceStatus, ServiceError>> {
        Box::pin(async move {
            let host = upsd_host(ep);
            let mut client = NutClient::connect(&host, DEFAULT_PORT)
                .await
                .map_err(ServiceError::Transport)?;
            let upses = client.list_upses().await.map_err(ServiceError::Transport)?;
            let healthy = !upses.is_empty()
                && upses
                    .iter()
                    .all(|u| u.status().is_some() && !u.on_battery());
            let detail = if upses.is_empty() {
                "upsd reachable but reports no UPS".to_string()
            } else {
                upses
                    .iter()
                    .map(|u| {
                        let status = u.status().unwrap_or("?");
                        let charge = u
                            .battery_charge()
                            .map(|c| format!("{c:.0}%"))
                            .unwrap_or_else(|| "?".into());
                        let runtime = u
                            .battery_runtime()
                            .map(|r| format!("{r:.0}s"))
                            .unwrap_or_else(|| "?".into());
                        let voltage = u
                            .input_voltage()
                            .map(|v| format!("{v:.1}V"))
                            .unwrap_or_else(|| "?".into());
                        format!(
                            "{}: {status} (battery {charge}, runtime {runtime}, input {voltage})",
                            u.name
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("; ")
            };
            Ok(ServiceStatus {
                healthy,
                detail,
                ..Default::default()
            })
        })
    }
}

/// Host `upsd` is reached at for this instance: the endpoint's host when given,
/// else loopback (the netserver monitors itself).
fn upsd_host(ep: &Endpoint) -> String {
    ep.base_url
        .trim()
        .trim_start_matches("nut://")
        .split(&['/', ':'][..])
        .find(|s| !s.is_empty() && *s != "netclient" && *s != "netserver")
        .map(str::to_string)
        .unwrap_or_else(|| "127.0.0.1".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn declares_provider() {
        let b = NutBackend::new("nut");
        assert_eq!(b.provider(), "nut");
        assert_eq!(b.default_port(), 3493);
    }

    #[tokio::test]
    async fn workload_spec_netserver_publishes_port() {
        let b = NutBackend::new("nut");
        let ep = Endpoint {
            name: "rack-ups".to_string(),
            ..Default::default()
        };
        let spec = b.workload_spec(Runtime::Docker, &ep).await.unwrap();
        assert_eq!(spec.name, "rack-ups");
        assert_eq!(spec.ports, vec!["3493:3493".to_string()]);
        assert!(spec.env.iter().any(|e| e.value == "netserver"));
    }

    #[tokio::test]
    async fn workload_spec_netclient_publishes_no_port() {
        let b = NutBackend::new("nut");
        let ep = Endpoint {
            name: "watcher".to_string(),
            base_url: "nut://netclient".to_string(),
            ..Default::default()
        };
        let spec = b.workload_spec(Runtime::Lxc, &ep).await.unwrap();
        assert!(spec.ports.is_empty());
        assert!(spec.env.iter().any(|e| e.value == "netclient"));
    }

    #[test]
    fn upsd_host_falls_back_to_loopback() {
        let ep = Endpoint::default();
        assert_eq!(upsd_host(&ep), "127.0.0.1");
    }

    #[test]
    fn upsd_host_reads_endpoint_host() {
        let ep = Endpoint {
            base_url: "nut://ups-host.example.invalid:3493".to_string(),
            ..Default::default()
        };
        assert_eq!(upsd_host(&ep), "ups-host.example.invalid");
    }
}
