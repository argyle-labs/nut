//! Typed capability providers for the backend-only nut export.
//!
//! Besides its `service` backend ([`crate::NutBackend`]), nut contributes two
//! more domain facets:
//! - a `diagnostics` provider ([`NutDiagnostics`]) — UPS-side power-loss
//!   detection/repair, delegating to [`crate::checks`];
//! - a `ups` provider ([`NutUps`]) — live UPS state + power/shutdown config,
//!   delegating to [`crate::ups`].
//!
//! Each implements the typed contract trait; the toolkit's `Plugin` builder
//! wires them into `backends()` and routes their ops via the contract's
//! `dispatch_op`. No hand-rolled dispatch or merged-backends JSON here anymore.

use plugin_toolkit::anyhow::Result;
use plugin_toolkit::contract::diagnostics::{
    DiagnoseArgs, DiagnosticsProvider, Finding, RepairArgs, RepairOutcome,
};
use plugin_toolkit::contract::ups::{
    UpsConfig, UpsConfigOutcome, UpsProvider, UpsQueryArgs, UpsState,
};
use plugin_toolkit::contract::BoxFuture;

/// The `diagnostics` provider nut advertises (`diagnose` + `repair`).
pub struct NutDiagnostics;

impl DiagnosticsProvider for NutDiagnostics {
    fn name(&self) -> &str {
        crate::PROVIDER
    }

    fn diagnose(&self, args: DiagnoseArgs) -> BoxFuture<'_, Result<Vec<Finding>>> {
        Box::pin(async move { Ok(crate::checks::diagnose_typed(args).await) })
    }

    fn repair(&self, args: RepairArgs) -> BoxFuture<'_, Result<RepairOutcome>> {
        Box::pin(async move { Ok(crate::checks::repair_typed(args)) })
    }
}

/// The `ups` provider nut advertises (`state` + `config_get` + `config_set`).
pub struct NutUps;

impl UpsProvider for NutUps {
    fn name(&self) -> &str {
        crate::PROVIDER
    }

    fn state(&self, args: UpsQueryArgs) -> BoxFuture<'_, Result<Vec<UpsState>>> {
        Box::pin(async move {
            crate::ups::state_typed(args)
                .await
                .map_err(|e| plugin_toolkit::anyhow::anyhow!(e))
        })
    }

    fn config_get(&self, args: UpsQueryArgs) -> BoxFuture<'_, Result<Vec<UpsConfig>>> {
        Box::pin(async move { Ok(crate::ups::config_get_typed(args)) })
    }

    fn config_set(&self, config: UpsConfig) -> BoxFuture<'_, Result<UpsConfigOutcome>> {
        Box::pin(async move { Ok(crate::ups::config_set_typed(config)) })
    }
}
