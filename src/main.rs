//! Dynamic (subprocess) entrypoint for the nut plugin.
//!
//! Hybrid arm: a `service` backend (UPS lifecycle) **and** a `diagnostics`
//! provider (UPS-side power-loss detection/repair). The stock
//! `serve_service_plugin!` macro only advertises the single service backend, so
//! this expands it by hand: it merges both backend defs into the `backends()`
//! JSON and routes both dispatch prefixes — `nut.__diag.*` to the diagnostics
//! provider, `service.__backend.nut.*` to the service backend. The plugin is a
//! `[[bin]]`, owns no runtime, and reaches orca only through the socket.

use plugin_toolkit::backend_def::service_backends_json;
use plugin_toolkit::reactor;
use plugin_toolkit::serve::{serve, PluginSpec};
use plugin_toolkit::service::dispatch_op;

use nut::registration::{diag_dispatch, merged_backends_json, DIAG_PREFIX};
use nut::NutBackend;

const SERVICE_PREFIX: &str = "service.__backend.nut";

/// Combined hybrid dispatch: diagnostics first, then the service backend.
fn dispatch(tool: &str, args_json: &str) -> Option<Result<String, String>> {
    if tool.starts_with(DIAG_PREFIX) {
        return diag_dispatch(tool, args_json);
    }
    let op = tool
        .strip_prefix(SERVICE_PREFIX)
        .and_then(|r| r.strip_prefix('.'))?;
    let backend = NutBackend::new(nut::PROVIDER);
    Some(reactor::block_on(dispatch_op(&backend, op, args_json)))
}

fn main() -> plugin_toolkit::anyhow::Result<()> {
    let backend = NutBackend::new(nut::PROVIDER);
    let service_json = service_backends_json(&backend, SERVICE_PREFIX);
    serve(PluginSpec {
        name: nut::PROVIDER.to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        prefixes: Vec::new(),
        backends_json: merged_backends_json(&service_json),
        schema_json: plugin_toolkit::backend_def::EMPTY_SCHEMAS.to_string(),
        backend_dispatch: Some(dispatch),
    })
}
