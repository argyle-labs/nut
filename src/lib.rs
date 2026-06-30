//! nut service backend — Network UPS Tools.
//!
//! Implements `ServiceBackend` so the generic `service.*` tools
//! (deploy/backup/restore/configure/status/connect/sync) drive nut. No
//! `#[orca_tool]`s — the only orca dep is `plugin-toolkit`. Modeled on the
//! nfs StorageBackend. See orca/docs/PLUGIN-PROGRAM.md.
#![allow(clippy::disallowed_types)]

use plugin_toolkit::service::{
    BackupArtifact, BoxFuture, Endpoint, Runtime, ServiceBackend, ServiceCapability, ServiceError,
    ServiceStatus, WorkloadSpec,
};

mod abi_export;

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
    fn provider(&self) -> &str { self.provider }

    /// Runtimes nut can be placed on. `service.deploy` hands the
    /// `workload_spec` below to a matching deploy target — this backend never
    /// drives pct/docker itself (that mechanic lives in the deploy-target domain).
    fn runtimes(&self) -> Vec<Runtime> { vec![Runtime::Docker, Runtime::Lxc] }

    fn capabilities(&self) -> Vec<ServiceCapability> { vec![ServiceCapability::Deploy, ServiceCapability::Backup, ServiceCapability::Restore, ServiceCapability::Configure, ServiceCapability::Status] }

    fn default_port(&self) -> u16 { 3493 }

    fn workload_spec<'a>(&'a self, _runtime: Runtime, _ep: &'a Endpoint)
        -> BoxFuture<'a, Result<WorkloadSpec, ServiceError>>
    {
        // TODO: describe the nut workload (image/template, ports, mounts,
        // env) for the chosen runtime. The deploy target turns this into a
        // compose service / LXC config / VM. See deploy-target::WorkloadSpec.
        Box::pin(async move { Err(ServiceError::unimplemented("nut.workload_spec")) })
    }

    fn backup<'a>(&'a self, _ep: &'a Endpoint)
        -> BoxFuture<'a, Result<BackupArtifact, ServiceError>>
    {
        // TODO: snapshot config/data (exclude regenerable cache).
        Box::pin(async move { Err(ServiceError::unimplemented("nut.backup")) })
    }

    fn restore<'a>(&'a self, _ep: &'a Endpoint, _from: &'a BackupArtifact)
        -> BoxFuture<'a, Result<(), ServiceError>>
    {
        Box::pin(async move { Err(ServiceError::unimplemented("nut.restore")) })
    }

    fn configure<'a>(&'a self, _ep: &'a Endpoint, _config: &'a str)
        -> BoxFuture<'a, Result<(), ServiceError>>
    {
        // TODO: apply nut-specific config idempotently.
        Box::pin(async move { Err(ServiceError::unimplemented("nut.configure")) })
    }

    fn status<'a>(&'a self, _ep: &'a Endpoint)
        -> BoxFuture<'a, Result<ServiceStatus, ServiceError>>
    {
        // TODO: real health/diagnostics.
        Box::pin(async move { Err(ServiceError::unimplemented("nut.status")) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn declares_provider() {
        let b = NutBackend::new("nut");
        assert_eq!(b.provider(), "nut");
    }
}
