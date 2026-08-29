//! Cross-domain route table.

use corelib::{OwnerId, RouteId};
use plugins::host::{MainRouteDrain, RouteDirection};
use std::sync::Arc;

/// One registered cross-domain route.
pub struct RouteEntry {
    pub id: RouteId,
    pub owner: OwnerId,
    pub direction: RouteDirection,
    /// Payload type name (introspection only).
    pub payload: &'static str,
    /// Mailbox semantics label (`latest` / `bounded` / `shared_latest`).
    pub mailbox: &'static str,
    /// Present for callback→main routes: drains into the main-side receiver.
    pub drain: Option<Arc<dyn MainRouteDrain>>,
}

impl RouteEntry {
    /// Current visible depth (0 for main→callback routes without a drain).
    #[must_use]
    pub fn depth(&self) -> usize {
        self.drain.as_ref().map_or(0, |d| d.depth())
    }
}

#[derive(Default)]
pub struct RouteTable {
    entries: Vec<RouteEntry>,
    next_id: u64,
}

impl RouteTable {
    pub(crate) fn push(&mut self, mut entry: RouteEntry) -> RouteId {
        let id = RouteId(self.next_id);
        self.next_id += 1;
        entry.id = id;
        self.entries.push(entry);
        id
    }

    #[must_use]
    pub fn entries(&self) -> &[RouteEntry] {
        &self.entries
    }

    /// Drain handles of callback→main routes owned by currently active
    /// plugins, cloned out so the world can be mutably borrowed afterwards.
    #[must_use]
    pub fn active_drains(
        &self,
        is_active: impl Fn(OwnerId) -> bool,
    ) -> Vec<Arc<dyn MainRouteDrain>> {
        self.entries
            .iter()
            .filter(|e| {
                e.direction == RouteDirection::CallbackToMain
                    && is_active(e.owner)
                    && e.drain.is_some()
            })
            .filter_map(|e| e.drain.clone())
            .collect()
    }

    /// Snapshot for introspection: (owner, mailbox label, depth).
    #[must_use]
    pub fn snapshot(&self) -> Vec<(&'static str, &'static str, usize)> {
        self.entries
            .iter()
            .map(|e| (e.payload, e.mailbox, e.depth()))
            .collect()
    }
}
