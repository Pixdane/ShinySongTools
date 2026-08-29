//! `App`: the Send composition root, the fixed driver, and owner-local
//! rollback.
//!
//! Driver order is frozen by the design (MessageMaintenance → CommandDrain →
//! plugin systems); the Startup driver runs once on the first outer
//! LateUpdate and leaves the RuntimeGate opening to the runtime layer above.

use bevy_ecs::message::MessageRegistry;
use bevy_ecs::world::World;
use scsp_core::{DataRoot, GateReader, MainThreadToken, OwnerId};
use scsp_plugin_api::phase::RestoreAction;
use scsp_plugin_api::{AppCtx, Plugin, RuntimeConfig};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;

use crate::core_state::AppCore;
use crate::host::HostImpl;
use crate::manager::{PluginManager, PluginState};
use crate::plugin_api::debug::DebugTopicLookup;
use crate::plugin_api::host::MainRouteDrain;

/// Summary of one Startup driver pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartupReport {
    pub started: Vec<OwnerId>,
    pub retired: Vec<OwnerId>,
}

/// Summary of one Update driver pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateReport {
    pub updated: Vec<OwnerId>,
    pub retired: Vec<OwnerId>,
}

/// The unique composition root. `App` is `Send` by construction: the world
/// only ever holds `Send + Sync + 'static` resources (no non-send resources
/// are enabled), and every member is `Send`. The Handoff fixture locks this
/// in.
pub struct App {
    pub(crate) world: World,
    pub(crate) core: AppCore,
    pub(crate) plugins: PluginManager,
    pub(crate) runtime_gate_reader: GateReader,
    /// Set once the Startup driver completed for the first frame; the
    /// scheduler uses it to choose run_startup vs run_update.
    pub(crate) startup_completed: bool,
    pub(crate) introspection: Arc<crate::introspection::IntrospectionShared>,
}

impl App {
    /// Create an App. The runtime gate control stays with the bootstrap /
    /// scheduler layer; the App (and hook sites) only hold the reader.
    #[must_use]
    pub fn new(
        config: RuntimeConfig,
        data_root: DataRoot,
        runtime_gate_reader: GateReader,
    ) -> Self {
        let introspection = Arc::new(crate::introspection::IntrospectionShared::new(
            config.debug.enabled,
        ));
        Self {
            world: World::new(),
            core: AppCore::new(config, data_root),
            plugins: PluginManager::default(),
            runtime_gate_reader,
            startup_completed: false,
            introspection,
        }
    }

    /// Inject the method resolver used by hook installation (the production
    /// backend in the live path, a mock in fixtures).
    pub fn set_method_resolver(&mut self, resolver: Arc<dyn scsp_core::MethodResolver>) {
        self.core.method_resolver = Some(resolver);
    }

    /// Register one already-boxed plugin (the production plugin list is
    /// heterogeneous).
    pub fn add_boxed_plugin(&mut self, plugin: Box<dyn Plugin>) {
        self.add_plugin_dyn(plugin);
    }

    fn add_plugin_dyn(&mut self, plugin: Box<dyn Plugin>) {
        let owner = self.plugins.begin_owner(plugin.name());
        let result = {
            let mut host = HostImpl { app: self, owner };
            let mut ctx = AppCtx::new(&mut host);
            plugin.build(&mut ctx)
        };
        match result {
            Ok(()) => {
                tracing::debug!(owner = owner.0, name = plugin.name(), "plugin build ok");
                self.sync_inventory();
            }
            Err(err) => {
                self.build_rollback(owner, &err);
            }
        }
    }

    /// Bootstrap-failure rollback: every owner in reverse registration order
    /// gets the full local rollback (gate closed, LIFO resource removal,
    /// reverse restore actions, Retired). Runs on the bootstrap worker:
    /// MainThread restore actions cannot execute there and are reported as
    /// failed (they must not be silently skipped).
    pub fn teardown_all(&mut self) {
        let mut owners: Vec<OwnerId> = self.plugins.records().iter().map(|r| r.id).collect();
        owners.reverse();
        for owner in owners {
            if self.plugins.record(owner).state != PluginState::Active {
                continue;
            }
            self.plugins.record_mut(owner).gate.close();
            self.core.topics.fail_pending_requests(owner);
            loop {
                let entry = self.plugins.record_mut(owner).inserted.pop();
                let Some(entry) = entry else { break };
                (entry.remove)(&mut self.world);
            }
            loop {
                let action = self.plugins.record_mut(owner).effects.pop();
                let Some(action) = action else { break };
                execute_restore(action, None);
            }
            self.plugins.record_mut(owner).state = PluginState::Retired;
        }
        self.sync_inventory();
    }

    #[must_use]
    pub fn startup_completed(&self) -> bool {
        self.startup_completed
    }

    /// Register one plugin in its own owner scope. Build failures roll the
    /// owner scope back (close gate → LIFO resource removal → reverse
    /// restore actions → Retired) and never affect other plugins.
    pub fn add_plugin<P: Plugin>(&mut self, plugin: P) {
        self.add_plugin_dyn(Box::new(plugin));
    }

    /// Build-phase rollback: gate closed → LIFO resource removal → reverse
    /// restore actions (AnyThread only exist at this point) → Retired.
    fn build_rollback(&mut self, owner: OwnerId, err: &scsp_core::PluginError) {
        let name = self.plugins.record(owner).name;
        tracing::warn!(owner = owner.0, name, error = %err, "plugin build failed; retiring owner");
        {
            let record = self.plugins.record_mut(owner);
            record.gate.close();
        }
        self.core.topics.fail_pending_requests(owner);
        // LIFO resource removal.
        loop {
            let entry = self.plugins.record_mut(owner).inserted.pop();
            let Some(entry) = entry else { break };
            (entry.remove)(&mut self.world);
        }
        // Reverse restore actions; each runs at most once, each inside its
        // own catch_unwind. Build-time actions are AnyThread by construction;
        // a MainThread action here would be a registration bug and fails.
        loop {
            let action = self.plugins.record_mut(owner).effects.pop();
            let Some(action) = action else { break };
            execute_restore(action, None);
        }
        self.plugins.record_mut(owner).state = PluginState::Retired;
        self.sync_inventory();
    }

    /// Startup driver: first outer LateUpdate. Runs each active owner's
    /// Startup systems in registration order; every boxed system is lazily
    /// initialized on first run and executed inside its own `catch_unwind`.
    /// The RuntimeGate is NOT opened here — the runtime layer opens it after
    /// this returns and the App is still runnable.
    pub fn run_startup(&mut self, main: &MainThreadToken) -> StartupReport {
        let owners = self.plugins.active_owner_ids();
        let mut report = StartupReport {
            started: Vec::new(),
            retired: Vec::new(),
        };
        for owner in owners {
            let mut failed: Option<String> = None;
            {
                let record = self.plugins.record_mut(owner);
                let crate::manager::PluginRecord {
                    startup,
                    effects,
                    inserted,
                    ..
                } = record;
                let mut order = inserted.last().map_or(0, |e| e.order + 1);
                for system in startup.iter_mut() {
                    let mut pending: Vec<scsp_plugin_api::host::ResourceInsert> = Vec::new();
                    let result = catch_unwind(AssertUnwindSafe(|| {
                        system.run(&mut self.world, main, &mut pending, effects)
                    }));
                    match result {
                        Ok(Ok(())) => {}
                        Ok(Err(err)) => {
                            failed = Some(err.to_string());
                            break;
                        }
                        Err(payload) => {
                            failed = Some(panic_message(&payload));
                            break;
                        }
                    }
                    // Apply queued inserts at the system boundary so later
                    // systems in this Startup pass see them.
                    for ins in pending.drain(..) {
                        match (ins.insert)(&mut self.world) {
                            Ok(()) => {
                                inserted.push(crate::manager::ResourceLedgerEntry {
                                    type_id: ins.type_id,
                                    type_name: ins.type_name,
                                    order,
                                    remove: ins.remove,
                                });
                                order += 1;
                            }
                            Err(err) => {
                                failed = Some(err.to_string());
                                break;
                            }
                        }
                    }
                    if failed.is_some() {
                        break;
                    }
                }
            }
            match failed {
                None => {
                    self.plugins.record_mut(owner).gate.open();
                    report.started.push(owner);
                }
                Some(reason) => {
                    self.startup_rollback(owner, &reason, Some(main));
                    report.retired.push(owner);
                }
            }
        }
        self.startup_completed = true;
        self.sync_inventory();
        report
    }

    /// Startup-phase rollback: gate closed → LIFO removal of this owner's
    /// Build+Startup resources → reverse restore actions → Retired.
    fn startup_rollback(&mut self, owner: OwnerId, reason: &str, main: Option<&MainThreadToken>) {
        let name = self.plugins.record(owner).name;
        tracing::warn!(
            owner = owner.0,
            name,
            reason,
            "plugin startup failed; retiring owner"
        );
        {
            let record = self.plugins.record_mut(owner);
            record.gate.close();
        }
        self.core.topics.fail_pending_requests(owner);
        loop {
            let entry = self.plugins.record_mut(owner).inserted.pop();
            let Some(entry) = entry else { break };
            (entry.remove)(&mut self.world);
        }
        loop {
            let action = self.plugins.record_mut(owner).effects.pop();
            let Some(action) = action else { break };
            execute_restore(action, main);
        }
        self.plugins.record_mut(owner).state = PluginState::Retired;
    }

    /// Update driver: MessageMaintenance → CommandDrain → plugin Update
    /// systems, in registration order, skipping retired owners.
    pub fn run_update(&mut self, main: &MainThreadToken) -> UpdateReport {
        let owners = self.plugins.active_owner_ids();
        let mut report = UpdateReport {
            updated: Vec::new(),
            retired: Vec::new(),
        };

        // 1. MessageMaintenance: one equivalent-update pass over all
        //    registered message types.
        self.message_maintenance();

        // 2. CommandDrain: deliver callback→main mailboxes into their
        //    main-side receivers with the phase-entry watermark. Values
        //    written after this point stay queued until the next frame.
        let drains: Vec<Arc<dyn MainRouteDrain>> = {
            let active = |o| self.plugins.is_active(o);
            self.core.routes.active_drains(active)
        };
        for drain in drains {
            let watermark = drain.depth();
            if watermark > 0 {
                drain.drain(&mut self.world, watermark);
            }
        }

        // Frame counter for the debug introspection.
        self.introspection.bump_frames();

        // 3. Plugin Update systems.
        for owner in owners {
            let mut failed: Option<String> = None;
            {
                let record = self.plugins.record_mut(owner);
                let update = &mut record.update;
                for system in update.iter_mut() {
                    let result =
                        catch_unwind(AssertUnwindSafe(|| system.run(&mut self.world, main)));
                    match result {
                        Ok(Ok(())) => {}
                        Ok(Err(err)) => {
                            failed = Some(err.to_string());
                            break;
                        }
                        Err(payload) => {
                            failed = Some(panic_message(&payload));
                            break;
                        }
                    }
                }
            }
            match failed {
                None => report.updated.push(owner),
                Some(reason) => {
                    self.update_retire(owner, &reason, Some(main));
                    report.retired.push(owner);
                }
            }
        }
        self.sync_inventory();
        report
    }

    /// Update-phase retirement: gate closed, systems/routes/topics disabled,
    /// restore actions executed — resources are NOT removed (other plugins'
    /// system contracts are not implicitly broken).
    fn update_retire(&mut self, owner: OwnerId, reason: &str, main: Option<&MainThreadToken>) {
        let name = self.plugins.record(owner).name;
        tracing::warn!(
            owner = owner.0,
            name,
            reason,
            "plugin update failed; retiring owner"
        );
        {
            let record = self.plugins.record_mut(owner);
            record.gate.close();
            record.state = PluginState::Retired;
        }
        self.core.topics.fail_pending_requests(owner);
        loop {
            let action = self.plugins.record_mut(owner).effects.pop();
            let Some(action) = action else { break };
            execute_restore(action, main);
        }
    }

    fn message_maintenance(&mut self) {
        self.world.try_resource_scope(
            |world: &mut World, mut registry: bevy_ecs::change_detection::Mut<MessageRegistry>| {
                let tick = world.change_tick();
                registry.run_updates(world, tick);
            },
        );
    }

    fn sync_inventory(&mut self) {
        // Split borrows: inventory lives in core, records in plugins.
        let App {
            core,
            plugins,
            runtime_gate_reader,
            introspection,
            ..
        } = self;
        core.inventory.sync(plugins);
        introspection.update(
            plugins,
            &core.routes,
            runtime_gate_reader.is_open(),
            &|owner| {
                core.topics
                    .topics()
                    .iter()
                    .filter(|e| e.owner == owner)
                    .map(|e| e.name)
                    .collect()
            },
        );
    }

    /// Shared world access for fixtures and introspection.
    #[must_use]
    pub fn world(&self) -> &World {
        &self.world
    }

    /// Mutable world access for fixtures (equivalent to an exclusive system).
    pub fn world_mut(&mut self) -> &mut World {
        &mut self.world
    }

    #[must_use]
    pub fn core(&self) -> &AppCore {
        &self.core
    }

    #[must_use]
    pub fn plugins(&self) -> &PluginManager {
        &self.plugins
    }
}

/// Execute one restore action inside its own `catch_unwind`; error or panic
/// is recorded for this item and rollback continues with earlier actions.
fn execute_restore(action: RestoreAction, main: Option<&MainThreadToken>) {
    let result = match action {
        RestoreAction::AnyThread(f) => match catch_unwind(AssertUnwindSafe(f)) {
            Ok(r) => r,
            Err(_) => Err(scsp_core::RestoreError::Failed),
        },
        RestoreAction::MainThread(f) => match main {
            Some(token) => match catch_unwind(AssertUnwindSafe(|| f(token))) {
                Ok(r) => r,
                Err(_) => Err(scsp_core::RestoreError::Failed),
            },
            None => {
                tracing::warn!("MainThread restore action reached build rollback; marking failed");
                Err(scsp_core::RestoreError::Failed)
            }
        },
    };
    if let Err(err) = result {
        tracing::warn!(error = %err, "restore action reported failure");
    }
}

fn panic_message(payload: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_owned()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "panic with non-string payload".to_owned()
    }
}
