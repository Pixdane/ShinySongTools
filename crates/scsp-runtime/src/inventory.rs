//! Introspection snapshot (`PluginInventory`).

use scsp_core::OwnerId;

use crate::manager::{PluginManager, PluginState};

/// One plugin's snapshot row, updated on every state transition.
#[derive(Debug, Clone)]
pub struct PluginSummary {
    pub id: OwnerId,
    pub name: &'static str,
    pub state: PluginState,
    pub gate_open: bool,
    pub startup_count: usize,
    pub update_count: usize,
    pub restore_count: usize,
    pub inserted_count: usize,
    pub has_container: bool,
    pub route_count: usize,
    pub topic_count: usize,
}

/// Snapshot of all owner records, read by the runtime introspection topics.
#[derive(Debug, Clone, Default)]
pub struct PluginInventory {
    plugins: Vec<PluginSummary>,
}

impl PluginInventory {
    /// Recompute from the current manager records (N is small; simple and
    /// always consistent with the transition that triggered it).
    pub(crate) fn sync(&mut self, manager: &PluginManager) {
        self.plugins = manager
            .records()
            .iter()
            .map(|r| PluginSummary {
                id: r.id,
                name: r.name,
                state: r.state,
                gate_open: r.gate.is_open(),
                startup_count: r.startup.len(),
                update_count: r.update.len(),
                restore_count: r.effects.len(),
                inserted_count: r.inserted.len(),
                has_container: r.container.is_some(),
                route_count: r.route_ids.len(),
                topic_count: r.topic_ids.len(),
            })
            .collect();
    }

    #[must_use]
    pub fn plugins(&self) -> &[PluginSummary] {
        &self.plugins
    }
}
