//! The runtime-side `PluginHost` implementation: one host per owner scope,
//! translating facade calls into world mutations, ledger records, and route
//! table entries.

use corelib::RuntimeConfig;
use corelib::host::{
    BoxedStartupSystem, BoxedUpdateSystem, MessageRegister, PluginHost, ResourceInsert,
    RouteRegistration,
};
use corelib::{DataRoot, GateReader, OwnerId, PluginError, RouteId};
use std::any::Any;
use std::sync::Arc;

use crate::app::App;
use crate::manager::ResourceLedgerEntry;
use crate::routes::RouteEntry;

pub(crate) struct HostImpl<'a> {
    pub(crate) app: &'a mut App,
    pub(crate) owner: OwnerId,
}

impl PluginHost for HostImpl<'_> {
    fn owner_id(&mut self) -> OwnerId {
        self.owner
    }

    fn runtime_gate_reader(&mut self) -> GateReader {
        self.app.runtime_gate_reader.clone()
    }

    fn owner_gate_reader(&mut self) -> GateReader {
        self.app.plugins.record(self.owner).gate.reader()
    }

    fn config(&mut self) -> RuntimeConfig {
        self.app.core.config.clone()
    }

    fn data_root(&mut self) -> DataRoot {
        self.app.core.data_root.clone()
    }

    fn method_resolver(&mut self) -> Option<Arc<dyn corelib::MethodResolver>> {
        self.app.core.method_resolver.clone()
    }

    fn insert_resource_dyn(&mut self, insert: ResourceInsert) -> Result<(), PluginError> {
        let order = {
            let record = self.app.plugins.record(self.owner);
            record.inserted.last().map_or(0, |e| e.order + 1)
        };
        // The closure performs the conflict check; on failure nothing was
        // inserted and nothing is recorded.
        (insert.insert)(&mut self.app.world)?;
        self.app
            .plugins
            .record_mut(self.owner)
            .inserted
            .push(ResourceLedgerEntry {
                type_id: insert.type_id,
                type_name: insert.type_name,
                order,
                remove: insert.remove,
            });
        Ok(())
    }

    fn register_message_dyn(&mut self, register: MessageRegister) -> Result<(), PluginError> {
        (register.register)(&mut self.app.world);
        Ok(())
    }

    fn add_startup_system_dyn(&mut self, system: BoxedStartupSystem) {
        self.app.plugins.record_mut(self.owner).startup.push(system);
    }

    fn add_update_system_dyn(&mut self, system: BoxedUpdateSystem) {
        self.app.plugins.record_mut(self.owner).update.push(system);
    }

    fn register_container_dyn(
        &mut self,
        container: Arc<dyn Any + Send + Sync>,
    ) -> Result<(), PluginError> {
        let record = self.app.plugins.record_mut(self.owner);
        if record.container.is_some() {
            return Err(PluginError::Message("a container is already registered"));
        }
        record.container = Some(container);
        Ok(())
    }

    fn register_route_dyn(&mut self, route: RouteRegistration) -> Result<RouteId, PluginError> {
        if let Some(receiver) = route.ensure_receiver {
            (receiver.register)(&mut self.app.world);
        }
        let entry = RouteEntry {
            id: RouteId(u64::MAX),
            owner: self.owner,
            direction: route.direction,
            payload: route.payload,
            mailbox: route.mailbox,
            drain: route.drain,
        };
        let id = self.app.core.routes.push(entry);
        self.app.plugins.record_mut(self.owner).route_ids.push(id);
        Ok(id)
    }

    fn register_restore_any_thread(&mut self, action: corelib::phase::RestoreAction) {
        self.app.plugins.record_mut(self.owner).effects.push(action);
    }

    fn topic_registry_handle(&mut self) -> Arc<dyn corelib::debug::DebugTopicLookup> {
        Arc::clone(&self.app.core.topics) as Arc<dyn corelib::debug::DebugTopicLookup>
    }

    fn introspection_handle(&mut self) -> Option<Arc<dyn corelib::debug::DebugIntrospection>> {
        Some(Arc::clone(&self.app.introspection) as _)
    }

    fn register_debug_topic_dyn(
        &mut self,
        registration: corelib::debug::DebugTopicRegistration,
    ) -> Result<(), PluginError> {
        let domain = if registration.callback_domain {
            crate::core_state::TopicDomain::Callback
        } else {
            crate::core_state::TopicDomain::Main
        };
        let topic_id = self.app.core.topics.register(
            self.owner,
            registration.name,
            domain,
            registration.channel,
            registration.decode,
        )?;
        self.app
            .plugins
            .record_mut(self.owner)
            .topic_ids
            .push(topic_id);
        Ok(())
    }
}
