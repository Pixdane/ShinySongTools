//! Runtime introspection shared state (feeds the `runtime.*` debug topics).
//!
//! The App updates this snapshot on every driver transition; the DebugPlugin
//! reads it through the [`DebugIntrospection`](scsp_plugin_api::debug::DebugIntrospection)
//! seam. Read-only for consumers: failures stay in observability events, the
//! snapshot carries state only.

use scsp_plugin_api::debug::DebugIntrospection;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use crate::routes::RouteTable;

/// One per-plugin row for `runtime.plugins`.
#[derive(Debug, Clone)]
pub struct IntrospectionPlugin {
    pub id: u64,
    pub name: &'static str,
    pub state: &'static str,
    pub gate_open: bool,
    pub startup_count: usize,
    pub update_count: usize,
    pub restore_count: usize,
    pub has_container: bool,
    pub topic_names: Vec<&'static str>,
}

#[derive(Debug, Clone)]
pub struct IntrospectionData {
    pub plugins: Vec<IntrospectionPlugin>,
    /// Route snapshots: (payload, mailbox label, visible depth).
    pub routes: Vec<(String, &'static str, usize)>,
    pub runtime_gate_open: bool,
}

/// Shared, App-updated introspection state.
pub struct IntrospectionShared {
    data: Mutex<IntrospectionData>,
    started_at: Instant,
    frames: AtomicU64,
    debug_enabled: bool,
}

impl IntrospectionShared {
    pub(crate) fn new(debug_enabled: bool) -> Self {
        Self {
            data: Mutex::new(IntrospectionData {
                plugins: Vec::new(),
                routes: Vec::new(),
                runtime_gate_open: false,
            }),
            started_at: Instant::now(),
            frames: AtomicU64::new(0),
            debug_enabled,
        }
    }

    pub(crate) fn bump_frames(&self) {
        self.frames.fetch_add(1, Ordering::AcqRel);
    }

    pub(crate) fn update(
        &self,
        manager: &crate::manager::PluginManager,
        routes: &RouteTable,
        runtime_gate_open: bool,
        topic_names: &dyn Fn(scsp_core::OwnerId) -> Vec<&'static str>,
    ) {
        let mut data = self.data.lock().expect("introspection lock");
        data.runtime_gate_open = runtime_gate_open;
        data.plugins = manager
            .records()
            .iter()
            .map(|r| IntrospectionPlugin {
                id: r.id.0,
                name: r.name,
                state: match r.state {
                    crate::manager::PluginState::Active => "active",
                    crate::manager::PluginState::Retired => "retired",
                },
                gate_open: r.gate.is_open(),
                startup_count: r.startup.len(),
                update_count: r.update.len(),
                restore_count: r.effects.len(),
                has_container: r.container.is_some(),
                topic_names: topic_names(r.id),
            })
            .collect();
        data.routes = routes
            .snapshot()
            .into_iter()
            .map(|(payload, mailbox, depth)| (payload.to_owned(), mailbox, depth))
            .collect();
    }
}

impl DebugIntrospection for IntrospectionShared {
    fn introspect(&self, method: &str) -> Option<serde_json::Value> {
        let data = self.data.lock().expect("introspection lock");
        match method {
            "runtime.plugins" => Some(serde_json::json!({
                "plugins": data.plugins.iter().map(|p| serde_json::json!({
                    "id": p.id,
                    "name": p.name,
                    "state": p.state,
                    "gate_open": p.gate_open,
                    "startup_systems": p.startup_count,
                    "update_systems": p.update_count,
                    "restore_actions": p.restore_count,
                    "container": p.has_container,
                    "topics": p.topic_names,
                })).collect::<Vec<_>>(),
            })),
            "runtime.gates" => Some(serde_json::json!({
                "runtime_gate_open": data.runtime_gate_open,
                "plugins": data.plugins.iter().map(|p| serde_json::json!({
                    "id": p.id,
                    "name": p.name,
                    "gate_open": p.gate_open,
                })).collect::<Vec<_>>(),
            })),
            "runtime.info" => Some(serde_json::json!({
                "version": env!("CARGO_PKG_VERSION"),
                "frames": self.frames.load(Ordering::Acquire),
                "uptime_seconds": self.started_at.elapsed().as_secs(),
                "debug_enabled": self.debug_enabled,
                "observability_dropped": scsp_core::process_event_queue().dropped(),
                "routes": data.routes.iter().map(|(payload, mailbox, depth)| serde_json::json!({
                    "payload": payload,
                    "mailbox": mailbox,
                    "depth": depth,
                })).collect::<Vec<_>>(),
            })),
            _ => None,
        }
    }
}
