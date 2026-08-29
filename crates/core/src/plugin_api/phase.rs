//! Phase systems: compile-time distinct Startup / Update phases.
//!
//! The first parameter of every system is its phase context; the phase is
//! carried by the type, so registering a system under the wrong phase is a
//! compile error (no `before/after` scheduling exists in v1). Each boxed
//! system creates its `SystemState` on first run — the driver's
//! lazy-initialize rule — which is what makes cross-phase resource
//! dependencies (an Update system referencing a resource inserted by a
//! Startup system, or by an earlier plugin's Startup) resolve naturally.

use bevy_ecs::system::SystemParam;
use bevy_ecs::world::World;
use corelib::{MainThreadToken, PluginError, RestoreError};
use std::marker::PhantomData;

use crate::plugin_api::host::{
    ResourceInsert, StartupRun, StartupSystemRunner, UpdateSystemRunner,
};

/// Return type of every phase system.
pub type SystemResult = Result<(), PluginError>;

/// Closure executed during rollback from any thread.
pub type AnyThreadRestoreFn = Box<dyn FnOnce() -> Result<(), RestoreError> + Send>;
/// Closure executed during rollback on the Unity main thread.
pub type MainThreadRestoreFn = Box<dyn FnOnce(&MainThreadToken) -> Result<(), RestoreError> + Send>;

/// Restore action registered by a plugin. Each action runs at most once
/// during rollback, in reverse registration order, each inside its own
/// `catch_unwind`.
pub enum RestoreAction {
    /// Safe to run from the bootstrap worker or any thread.
    AnyThread(AnyThreadRestoreFn),
    /// Requires the Unity main thread; gets the current frame's
    /// `MainThreadToken` when the driver executes it.
    MainThread(MainThreadRestoreFn),
}

impl core::fmt::Debug for RestoreAction {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::AnyThread(_) => f.write_str("RestoreAction::AnyThread"),
            Self::MainThread(_) => f.write_str("RestoreAction::MainThread"),
        }
    }
}

/// Narrow registration surface given to Startup systems: restore actions
/// only. Resource insertion goes through `StartupCtx::insert_resource`.
pub struct StartupRegistrar<'sink> {
    pub(crate) sink: &'sink mut Vec<RestoreAction>,
}

impl StartupRegistrar<'_> {
    /// Register an `AnyThread` restore action.
    pub fn register_restore_any_thread(
        &mut self,
        action: impl FnOnce() -> Result<(), RestoreError> + Send + 'static,
    ) {
        self.sink.push(RestoreAction::AnyThread(Box::new(action)));
    }

    /// Register a `MainThread` restore action.
    pub fn register_restore_main_thread(
        &mut self,
        action: impl FnOnce(&MainThreadToken) -> Result<(), RestoreError> + Send + 'static,
    ) {
        self.sink.push(RestoreAction::MainThread(Box::new(action)));
    }
}

/// Phase context of a Startup system (runs once, first outer LateUpdate).
pub struct StartupCtx<'a> {
    pub main: &'a MainThreadToken,
    pub reg: &'a mut StartupRegistrar<'a>,
    pub(crate) pending: &'a mut Vec<ResourceInsert>,
}

impl StartupCtx<'_> {
    /// Direct resource insertion. The insert (with its conflict check) is
    /// applied immediately after this system function returns, and the
    /// resource is recorded in the owner ledger. A conflicting insert fails
    /// the owner at that point.
    pub fn insert_resource<T: bevy_ecs::prelude::Resource>(
        &mut self,
        value: T,
    ) -> Result<(), PluginError> {
        let type_name = core::any::type_name::<T>();
        self.pending.push(ResourceInsert {
            type_id: core::any::TypeId::of::<T>(),
            type_name,
            insert: Box::new(move |world: &mut World| {
                if world.contains_resource::<T>() {
                    return Err(PluginError::ResourceConflict(type_name));
                }
                world.insert_resource(value);
                Ok(())
            }),
            remove: Box::new(|world: &mut World| {
                world.remove_resource::<T>();
            }),
        });
        Ok(())
    }
}

/// Phase context of an Update system (runs every outer LateUpdate).
#[derive(Clone, Copy)]
pub struct UpdateCtx<'a> {
    pub main: &'a MainThreadToken,
}

/// A function usable as a Startup system: first parameter
/// `StartupCtx<'_>`, remaining parameters are `SystemParam`s.
///
/// The `Marker` parameter (a `fn(...)` type) is inferred from the function
/// and distinguishes arities so the macro-generated impls never conflict.
pub trait StartupFunction<Marker>: Send + Sync + 'static {
    type Param: SystemParam + 'static;
    fn run(
        &mut self,
        ctx: StartupCtx<'_>,
        param: bevy_ecs::system::SystemParamItem<Self::Param>,
    ) -> SystemResult;
}

/// A function usable as an Update system: first parameter
/// `UpdateCtx<'_>`, remaining parameters are `SystemParam`s.
pub trait UpdateFunction<Marker>: Send + Sync + 'static {
    type Param: SystemParam + 'static;
    fn run(
        &mut self,
        ctx: UpdateCtx<'_>,
        param: bevy_ecs::system::SystemParamItem<Self::Param>,
    ) -> SystemResult;
}

macro_rules! impl_phase_function {
    ($trait_name:ident, $ctx:ident, $run:ident; $($param: ident),*) => {
        impl<Func, $($param: SystemParam + 'static),*> $trait_name<fn($($param,)*) -> SystemResult> for Func
        where
            Func: Send + Sync + 'static,
            // First bound guides inference from the function signature;
            // second bound matches the fetched `SystemParamItem` shapes.
            // Both are higher-ranked so fn items and polymorphic closures
            // qualify.
            Func: for<'a, 'w, 's> FnMut($ctx<'a>, $($param,)*) -> SystemResult,
            Func: for<'a, 'w, 's> FnMut(
                $ctx<'a>,
                $(bevy_ecs::system::SystemParamItem<'w, 's, $param>),*
            ) -> SystemResult,
        {
            type Param = ($($param,)*);

            #[allow(non_snake_case)]
            fn $run(
                &mut self,
                ctx: $ctx<'_>,
                param: bevy_ecs::system::SystemParamItem<Self::Param>,
            ) -> SystemResult {
                let ($($param,)*) = param;
                self(ctx, $($param,)*)
            }
        }
    };
}

impl_phase_function!(StartupFunction, StartupCtx, run;);
impl_phase_function!(StartupFunction, StartupCtx, run; P0);
impl_phase_function!(StartupFunction, StartupCtx, run; P0, P1);
impl_phase_function!(StartupFunction, StartupCtx, run; P0, P1, P2);
impl_phase_function!(StartupFunction, StartupCtx, run; P0, P1, P2, P3);
impl_phase_function!(StartupFunction, StartupCtx, run; P0, P1, P2, P3, P4);
impl_phase_function!(StartupFunction, StartupCtx, run; P0, P1, P2, P3, P4, P5);
impl_phase_function!(StartupFunction, StartupCtx, run; P0, P1, P2, P3, P4, P5, P6);
impl_phase_function!(StartupFunction, StartupCtx, run; P0, P1, P2, P3, P4, P5, P6, P7);

impl_phase_function!(UpdateFunction, UpdateCtx, run;);
impl_phase_function!(UpdateFunction, UpdateCtx, run; P0);
impl_phase_function!(UpdateFunction, UpdateCtx, run; P0, P1);
impl_phase_function!(UpdateFunction, UpdateCtx, run; P0, P1, P2);
impl_phase_function!(UpdateFunction, UpdateCtx, run; P0, P1, P2, P3);
impl_phase_function!(UpdateFunction, UpdateCtx, run; P0, P1, P2, P3, P4);
impl_phase_function!(UpdateFunction, UpdateCtx, run; P0, P1, P2, P3, P4, P5);
impl_phase_function!(UpdateFunction, UpdateCtx, run; P0, P1, P2, P3, P4, P5, P6);
impl_phase_function!(UpdateFunction, UpdateCtx, run; P0, P1, P2, P3, P4, P5, P6, P7);

/// Map a Bevy param validation failure to the plugin error chain. The
/// concrete reason travels through observability; the error type only
/// carries the closed vocabulary.
pub(crate) fn map_validation_error(
    err: bevy_ecs::system::SystemParamValidationError,
) -> PluginError {
    let _ = err;
    PluginError::MissingDependency("param_validation")
}

/// Lazy boxed Startup system: `SystemState` created on first run.
pub(crate) struct LazyStartupSystem<Marker: 'static, F: StartupFunction<Marker>> {
    pub(crate) f: F,
    pub(crate) state: Option<bevy_ecs::system::SystemState<F::Param>>,
    pub(crate) marker: PhantomData<fn() -> Marker>,
}

impl<Marker: 'static, F: StartupFunction<Marker>> StartupSystemRunner
    for LazyStartupSystem<Marker, F>
{
    fn run(&mut self, run: StartupRun<'_>) -> SystemResult {
        let StartupRun {
            world,
            main,
            pending,
            restore_sink,
        } = run;
        // Lazy initialize: the only init timing; cross-phase dependencies
        // therefore resolve without build-time presence.
        if self.state.is_none() {
            self.state = Some(bevy_ecs::system::SystemState::new(world));
        }
        let state = self.state.as_mut().expect("state initialized above");
        let mut registrar = StartupRegistrar { sink: restore_sink };
        let ctx = StartupCtx {
            main,
            reg: &mut registrar,
            pending,
        };
        let param = state.get_mut(world).map_err(map_validation_error)?;
        let result = self.f.run(ctx, param);
        state.apply(world);
        result
    }
}

/// Lazy boxed Update system.
pub(crate) struct LazyUpdateSystem<Marker: 'static, F: UpdateFunction<Marker>> {
    pub(crate) f: F,
    pub(crate) state: Option<bevy_ecs::system::SystemState<F::Param>>,
    pub(crate) marker: PhantomData<fn() -> Marker>,
}

impl<Marker: 'static, F: UpdateFunction<Marker>> UpdateSystemRunner
    for LazyUpdateSystem<Marker, F>
{
    fn run(&mut self, world: &mut World, main: &MainThreadToken) -> SystemResult {
        if self.state.is_none() {
            self.state = Some(bevy_ecs::system::SystemState::new(world));
        }
        let state = self.state.as_mut().expect("state initialized above");
        let ctx = UpdateCtx { main };
        let param = state.get_mut(world).map_err(map_validation_error)?;
        let result = self.f.run(ctx, param);
        state.apply(world);
        result
    }
}
