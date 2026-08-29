//! Owner ledger and per-plugin runtime records.

use bevy_ecs::world::World;
use corelib::OwnerId;
use plugins::host::{BoxedStartupSystem, BoxedUpdateSystem};
use plugins::phase::RestoreAction;
use std::any::{Any, TypeId};
use std::sync::Arc;

use crate::gate::PluginGate;

/// Minimal logical state consumed by the driver, the inventory, and the
/// debug introspection. Failure reasons live in observability events only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginState {
    Active,
    Retired,
}

/// One resource inserted by an owner, recorded for LIFO rollback. The
/// removal closure is the documented ledger representation (TypeId + order +
/// remove closure); it runs at most once.
pub struct ResourceLedgerEntry {
    pub type_id: TypeId,
    pub type_name: &'static str,
    pub order: u64,
    pub remove: Box<dyn FnOnce(&mut World) + Send>,
}

impl core::fmt::Debug for ResourceLedgerEntry {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ResourceLedgerEntry")
            .field("type_name", &self.type_name)
            .field("order", &self.order)
            .finish()
    }
}

/// Runtime record of one plugin owner scope.
pub struct PluginRecord {
    pub id: OwnerId,
    pub name: &'static str,
    pub state: PluginState,
    pub startup: Vec<BoxedStartupSystem>,
    pub update: Vec<BoxedUpdateSystem>,
    /// Restore ledger (reverse-order rollback).
    pub effects: Vec<RestoreAction>,
    /// Direct-inserted resources of this owner (Build + Startup), in order.
    pub inserted: Vec<ResourceLedgerEntry>,
    /// The plugin's single callback container, retained for the process
    /// lifetime (retention root).
    pub container: Option<Arc<dyn Any + Send + Sync>>,
    pub route_ids: Vec<corelib::RouteId>,
    pub topic_ids: Vec<corelib::TopicId>,
    pub gate: PluginGate,
}

impl PluginRecord {
    pub(crate) fn new(id: OwnerId, name: &'static str) -> Self {
        Self {
            id,
            name,
            state: PluginState::Active,
            startup: Vec::new(),
            update: Vec::new(),
            effects: Vec::new(),
            inserted: Vec::new(),
            container: None,
            route_ids: Vec::new(),
            topic_ids: Vec::new(),
            gate: PluginGate::new(),
        }
    }
}

/// Owner-scoped records in registration order.
#[derive(Default)]
pub struct PluginManager {
    records: Vec<PluginRecord>,
    next_owner: u64,
}

impl PluginManager {
    pub(crate) fn begin_owner(&mut self, name: &'static str) -> OwnerId {
        let id = OwnerId(self.next_owner);
        self.next_owner += 1;
        self.records.push(PluginRecord::new(id, name));
        id
    }

    #[must_use]
    pub fn record(&self, id: OwnerId) -> &PluginRecord {
        self.records
            .iter()
            .find(|r| r.id == id)
            .expect("owner id from this manager")
    }

    pub(crate) fn record_mut(&mut self, id: OwnerId) -> &mut PluginRecord {
        self.records
            .iter_mut()
            .find(|r| r.id == id)
            .expect("owner id from this manager")
    }

    /// Ids of owners that are not retired, in registration order.
    #[must_use]
    pub fn active_owner_ids(&self) -> Vec<OwnerId> {
        self.records
            .iter()
            .filter(|r| r.state == PluginState::Active)
            .map(|r| r.id)
            .collect()
    }

    #[must_use]
    pub fn records(&self) -> &[PluginRecord] {
        &self.records
    }

    #[must_use]
    pub fn is_active(&self, id: OwnerId) -> bool {
        self.record(id).state == PluginState::Active
    }
}
