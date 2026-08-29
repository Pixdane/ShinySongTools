//! The `Plugin` entry point.

use crate::PluginError;
use crate::context::AppCtx;

/// A functional plugin. `build` runs once on the bootstrap worker; it
/// registers resources, systems, containers, routes, and hooks through the
/// owner-scoped [`AppCtx`].
///
/// A plugin instance is dropped after `build` returns; per-frame behavior
/// lives in the registered systems.
pub trait Plugin: Send + Sync + 'static {
    /// Display name used by the inventory and observability events.
    fn name(&self) -> &'static str {
        "plugin"
    }

    /// Configure this plugin inside its owner scope. A returned error retires
    /// this plugin only: the runtime rolls the owner scope back (LIFO
    /// resource removal + reverse restore actions) and continues with the
    /// next plugin.
    fn build(&self, ctx: &mut AppCtx<'_>) -> Result<(), PluginError>;
}
