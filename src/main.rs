//! Subprocess entrypoint for the nut plugin.
//!
//! A backend-only plugin advertising three domain facets — `service`
//! (UPS lifecycle via [`nut::NutBackend`]), `diagnostics` (UPS power-loss
//! detect/repair via [`nut::registration::NutDiagnostics`]), and `ups` (UPS
//! state/config via [`nut::registration::NutUps`]). It owns no `nut.` tool
//! surface. The typed `Plugin` builder advertises every facet's backend def and
//! routes each domain's ops through the contract's `dispatch_op`. The plugin is
//! a `[[bin]]`, owns no runtime, and reaches orca only through the socket.
//! Started through the `orca-nut-notify` symlink it runs the NUT event handler
//! ([`nut::notify`]) instead and always exits 0.

plugin_toolkit::instrument::bootstrap!();
use plugin_toolkit::plugin::Plugin;

fn main() -> plugin_toolkit::anyhow::Result<()> {
    if nut::notify::invoked_as_handler(&std::env::args().next().unwrap_or_default()) {
        nut::notify::run();
        std::process::exit(0);
    }
    nut::notify::ensure_installed_at_startup();
    Plugin::named(nut::PROVIDER)
        .version(env!("CARGO_PKG_VERSION"))
        .service(nut::NutBackend::new(nut::PROVIDER))
        .diagnostics(nut::registration::NutDiagnostics)
        .ups(nut::registration::NutUps)
        .serve()
}
