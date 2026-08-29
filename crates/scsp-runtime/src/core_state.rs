//! Non-plugin composition state: config, route table, debug topic registry,
//! introspection inventory, and the method resolver handle for hooks.

use scsp_core::{DataRoot, MethodResolver};
use scsp_plugin_api::RuntimeConfig;
use scsp_plugin_api::debug::{DebugDecodeFn, DebugTopicChannel, DebugTopicLookup, DebugTopicView};
use std::sync::Arc;

use crate::inventory::PluginInventory;
use crate::routes::RouteTable;

/// Execution domain of a debug topic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TopicDomain {
    /// Handler runs as an Update system with world access.
    Main,
    /// Handler runs inside the owner's hook callbacks via the container's
    /// shared slots.
    Callback,
}

/// One registered debug topic: typed-channel plumbing owned by the registry.
pub struct TopicEntry {
    pub id: scsp_core::TopicId,
    pub owner: scsp_core::OwnerId,
    pub name: &'static str,
    pub domain: TopicDomain,
    pub channel: Arc<DebugTopicChannel>,
    pub decode: DebugDecodeFn,
}

#[derive(Default)]
pub struct TopicRegistry {
    entries: std::sync::RwLock<Vec<TopicEntry>>,
    next_id: std::sync::atomic::AtomicU64,
}

impl TopicRegistry {
    /// Register one topic; duplicate names fail the build.
    pub(crate) fn register(
        &self,
        owner: scsp_core::OwnerId,
        name: &'static str,
        domain: TopicDomain,
        channel: Arc<DebugTopicChannel>,
        decode: DebugDecodeFn,
    ) -> Result<scsp_core::TopicId, scsp_core::PluginError> {
        let mut entries = self.entries.write().expect("topics lock");
        if entries.iter().any(|e| e.name == name) {
            return Err(scsp_core::PluginError::Message(
                "debug topic already registered",
            ));
        }
        let id = scsp_core::TopicId(
            self.next_id
                .fetch_add(1, std::sync::atomic::Ordering::AcqRel),
        );
        entries.push(TopicEntry {
            id,
            owner,
            name,
            domain,
            channel,
            decode,
        });
        Ok(id)
    }

    /// Retire one owner's topics: every queued-but-undelivered request is
    /// answered `plugin_unavailable` so wire clients never hang on a retired
    /// plugin (docs/debug-diagnostics-logging.md 局部回滚). In-flight
    /// requests already owned by handler/relay systems are not forcibly
    /// cancelled; their systems stop running with the owner.
    pub(crate) fn fail_pending_requests(&self, owner: scsp_core::OwnerId) {
        use scsp_plugin_api::debug::{
            DebugResponse, DebugServerError, DebugWireError, DebugWireErrorCode,
        };
        let entries = self.entries.read().expect("topics lock");
        for entry in entries.iter().filter(|e| e.owner == owner) {
            let queued: Vec<_> = entry
                .channel
                .inbox
                .lock()
                .expect("inbox lock")
                .drain(..)
                .collect();
            if queued.is_empty() {
                continue;
            }
            let mut outbox = entry.channel.outbox.lock().expect("outbox lock");
            for request in queued {
                outbox.push(DebugResponse {
                    id: request.id,
                    result: Err(DebugWireError {
                        code: DebugWireErrorCode::ServerError(DebugServerError::PluginUnavailable),
                        message: "plugin retired; request not delivered".to_owned(),
                    }),
                });
                entry.channel.leave_pending();
            }
        }
    }
}

impl DebugTopicLookup for TopicRegistry {
    fn topics(&self) -> Vec<DebugTopicView> {
        self.entries
            .read()
            .expect("topics lock")
            .iter()
            .map(|e| DebugTopicView {
                name: e.name,
                owner: e.owner,
                channel: Arc::clone(&e.channel),
                decode: Arc::clone(&e.decode),
            })
            .collect()
    }
}

/// Non-plugin composition state held by [`App`](crate::App).
pub struct AppCore {
    pub(crate) config: RuntimeConfig,
    pub(crate) data_root: DataRoot,
    pub(crate) routes: RouteTable,
    pub(crate) topics: Arc<TopicRegistry>,
    pub(crate) inventory: PluginInventory,
    /// Method resolver injected by the bootstrap (fixtures install a mock).
    pub(crate) method_resolver: Option<Arc<dyn MethodResolver>>,
}

impl AppCore {
    pub(crate) fn new(config: RuntimeConfig, data_root: DataRoot) -> Self {
        Self {
            config,
            data_root,
            routes: RouteTable::default(),
            topics: Arc::new(TopicRegistry::default()),
            inventory: PluginInventory::default(),
            method_resolver: None,
        }
    }

    #[must_use]
    pub fn config(&self) -> &RuntimeConfig {
        &self.config
    }

    #[must_use]
    pub fn data_root(&self) -> &DataRoot {
        &self.data_root
    }

    #[must_use]
    pub fn routes(&self) -> &RouteTable {
        &self.routes
    }

    #[must_use]
    pub fn topics(&self) -> &TopicRegistry {
        &self.topics
    }

    #[must_use]
    pub fn inventory(&self) -> &PluginInventory {
        &self.inventory
    }
}
