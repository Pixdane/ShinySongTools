# App 与 driver

状态：v2 设计（2026-08-29 修订）。本文定义 `runtime` crate 的 App / PluginManager / driver 部分：组合根、owner scope、固定 driver、惰性初始化与局部回滚。bootstrap worker、scheduler、Handoff 与 `scsp_start` 见本 crate Rustdoc 的“Bootstrap 与 scheduler”。

## App 是组合根

`App` 从 bootstrap worker 构造到主线程运行始终是同一个 `Send` 类型，不引入 `PreparedApp`，也不使用本身为 `!Send` 的 `bevy_app::App`：

```rust,ignore
pub struct App {
    world: bevy_ecs::World,
    core: AppCore,
    plugins: PluginManager,
}
```

- `world`：所有 plugin system 共享的 `bevy_ecs::World`（AppWorld）。只接受 `Send + Sync + 'static` resource，不启用 non-send resource，因此 `App: Send` 可由编译期与无游戏 fixture 证明。
- `AppCore`：非插件组合状态——`RuntimeConfig`、跨域 route 表（含主线程接收端与 watermark）、debug topic registry、`PluginInventory`（自省快照数据）、observability facade 句柄。**不再有 `DebugState`**：debug 面是普通插件（DebugPlugin），其 transport 与 pending 状态归 DebugPlugin 自己。
- `plugins`：PluginManager（owner scope 与运行期记录）。

App 在 worker 阶段只执行线程无关的 build；经 Handoff 后由游戏主线程 TLS 独占。Unity 主线程操作要求 driver 按借用传入的 `MainThreadToken`（见 core 分册）。

## PluginManager 与 owner scope

`PluginManager` 从 `App::new` 起就是 App 内部成员。`App::add_plugin` 为每个插件建立唯一 owner scope，并在 scope 内调用 `Plugin::build`。每个 owner 保留的运行期记录：

```rust,ignore
struct PluginRecord {
    state: PluginState,                 // Active | Retired（最小逻辑状态）
    startup: Vec<BoxedStartupSystem>,
    update: Vec<BoxedUpdateSystem>,
    effects: Vec<RestoreAction>,        // restore ledger（逆序回滚用）
    inserted: Vec<ResourceLedgerEntry>, // 直接插入资源的 (TypeId, 顺序) 记账
    container: Option<ContainerHandle>,
    route_ids: Vec<RouteId>,
    debug_topic_ids: Vec<TopicId>,
    gate: PluginGate,                   // 控制端 + reader
}
```

`PluginState` 只保存 driver 与 debug 面消费的最小逻辑状态；失败原因属于 observability 事件，不建错误分类状态机。`PluginInventory`（自省快照：id、state、gate、各列表计数、topic 名单）随状态迁移更新，供 DebugPlugin 的自省 topic 读取。

Build context / `StartupCtx` 不暴露 manager 或 `World` 本体；resource、system、container、route、hook、debug topic 的注册都自动携带当前 owner。插件间依赖只由固定注册顺序与资源存在性表达，不建依赖图。

## Build（worker 阶段）

```text
App::add_plugin（按 runtime 的固定生产插件列表逐个执行）
  → 建立 owner scope 与空 ledger
  → Plugin.build
      → 直接插入 resources（重复类型 → ResourceConflict → build 失败）
      → add_startup_system / add_update_system（boxed，尚未 initialize）
      → 注册 container / route / debug topic
      → hook typestate：发布 site → CAS 安装（gate 关闭）→ restore 记录入 ledger
  → build 失败：关该 owner gate → ledger LIFO 移除其资源 → 逆序执行其 restore actions
    → 该插件标记 Retired → 继续下一个插件
```

单个插件 build 失败只影响自身；后续插件若依赖被移除的资源，在自己 build/首次运行时失败退役（依赖语义正确传播）。

## Startup（首个外层 LateUpdate，主线程）

```text
RuntimeGate 保持关闭
  → 按插件注册顺序逐个 owner：
      → 逐个运行其 Startup systems（每个 system 首次运行前惰性 initialize）
      → system 内直接插入的资源立即进入 AppWorld 并记入 ledger
      → 全部成功：开启该插件 gate（plugin gate）
      → 任一失败：关 gate → ledger LIFO 移除该 owner 本轮 Build+Startup 插入的资源
        → 逆序执行其 restore actions → 标记 Retired → 继续下一个 owner
  → 全部 owner 处理完成且 App 可继续运行 → runtime 最后开启 RuntimeGate
```

首个 callback 只运行 Startup，不在同一 callback 内继续运行 Update。RuntimeGate 在整个 Startup driver 期间保持关闭，因此即使某 plugin gate 已开启，功能 callback 也要等总 gate 开启后才能进入插件逻辑。

**惰性初始化规则**：每个 boxed system 在首次运行前才对当前 AppWorld 完成 `System::initialize`。这是唯一初始化时机约定；它使 Update system 引用 Startup 阶段才存在的资源、后注册插件引用前序插件 Startup 产物都天然成立。driver 不做 build 期预初始化。

## 固定 driver 顺序

v1 不构建 system dependency graph，不提供 before/after 约束。这里的 schedule 是 SCSP driver 的固定阶段，不是 `bevy_ecs::Schedule`：

```text
后续每个外层 LateUpdate：
  MessageMaintenance
    → 主线程内部消息（Bevy Messages<M> buffer、debug 主线程 inbox）执行一次等价 update 的维护
  CommandDrain
    → 以阶段入口 watermark 遍历已注册的 callback→main route，每条 route 最多取该边界前可见的
      最新值/有界批次，转入对应主线程接收端；阶段开始后写入的值留到下一帧
  → 按插件注册顺序逐个 owner 运行 Update systems（跳过 Retired）
  → 返回 runtime 调用 original LateUpdate
```

顺序、阶段与 v1 收敛结论一致；DebugDispatch 不再是内建阶段——DebugPlugin 是注册在生产插件列表首位的普通插件，其 Update system（dispatch/自省）与各插件的 debug handler/relay system 都落在 plugin Update 区。Debug 域因此从第二个外层 LateUpdate 起可用。

某 owner 的 Update system 失败或 panic：该 owner 本帧剩余 systems 跳过，执行其局部退役（见下）；其它 owner 按注册顺序继续。

## 局部回滚与 restore ledger

restore action 只登记确实修改外部状态、以后需要恢复的操作：

```rust,ignore
enum RestoreAction {
    AnyThread(Box<dyn FnOnce() -> Result<(), RestoreError> + Send>),
    MainThread(Box<dyn FnOnce(&MainThreadToken) -> Result<(), RestoreError> + Send>),
}
```

- Build 只能登记 `AnyThread`；Startup 可登记两种。恢复基线在首次登记时捕获；Update 不提供 registrar，也不追加记录——需要逐帧改变外部状态时修改 controller resource，恢复日志因此不随帧增长。
- rollback 逐项从 ledger 尾部取出，以独立 `catch_unwind` 执行；每个 action 至多执行一次，error/panic 记为本项失败并继续更早的 action，不重试可能非幂等的恢复。`RestoreError::{OwnershipLost, Failed}` 是本轮返回值，不引入持久 effect 状态枚举。
- **Build/Startup 失败**：关 gate → 移除该 owner 已插入资源（LIFO）→ 逆序执行 restore actions → Retired。移除资源可能使依赖它的其它插件在首次运行时 param validation 失败退役——这是依赖语义的正确传播。
- **Update 失败**：关 gate → 停用其 systems、routes、debug topics（pending 的 debug request 统一回复 `plugin_unavailable`）→ 逆序执行 restore actions → Retired。**不移除其 AppWorld 资源**：其它插件的 system 契约不被隐式破坏，代价只是失效插件的状态驻留到进程退出。
- MethodPointer Hook 的 restore action 只在 slot 仍属于本插件时 CAS 恢复；drift 不盲写。静态 site、container、typed original 与 callback backend 由进程期 OnceLock 保活，不因回滚释放（无 UAF）。

"逻辑回滚完成"只表示功能停用且 restore actions 都尝试过一次，不表示每个外部状态都恢复成功，也不表示静态对象已释放。

## AppWorld 共享与 panic 边界

- 同一具体 resource type 在 AppWorld 中只有一份；插件需要独立状态时用 newtype。可变操作中 panic 的 resource 可能业务上不完整但内存安全；driver 退役该 owner 且不自动删除共享资源（Build/Startup 失败路径的 ledger 移除除外）。
- 每个 boxed Startup/Update system 的 `System::run` 由 plugin-system 在独立 `catch_unwind` 中调用；system error/Bevy run error/panic 都归一化为 owner-local failure。`AssertUnwindSafe` 只出现在经过审阅的调用适配层。panic 不得直接越过整个 App driver。
- 只有 driver 遍历、owner 映射、route 表、共享 adapter 等 plugin-system 基础设施不变量失败，才升级为 scheduler-level failure（交 runtime 处理，见 runtime-crate 分册）。
- 采用 `bevy_ecs` 后不设计自有多资源 query/downcast/borrow 引擎；共享/独占借用冲突与缺失资源校验交给 `SystemParam`。

## Runtime global failure

plugin-system 不提供跨插件 global rollback。Handoff 成功后发生 runtime 级故障时，runtime 关闭 RuntimeGate 并停止后续业务逻辑；不等待未来 callback 回滚，不调用各插件的 restore ledger。已提交的资源、静态 site 与 container 保活到进程退出——保活只为避免仍可达 callback 发生 UAF 并提供 original passthrough。插件自身失败仍按上节局部规则处理。

## 物理卸载与资源回收

第一版不支持进程内物理卸载插件。真正释放插件对象、Hook 与 callback context 的 quiescence 协议仍为待设计；该协议完成前，不因 slot 已恢复或短时未见 callback 就推断 context 不可达。Retired owner 的记录可退出活动集合，但不得使静态 site 引用任何短生命周期对象。

## 待打磨项

- `PluginInventory` 自省字段集合与更新时机。
- ResourceLedgerEntry 的具体表示（TypeId + 顺序 + 移除闭包）。
- boxed system 惰性初始化的 driver 内部实现（首跑标记）。
- debug handler/relay 自动登记 system 的参数集合与命名。
