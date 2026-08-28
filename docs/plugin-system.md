# Plugin system 设计

状态：v1 设计已收敛；具体 Rust 适配类型和物理模块可在实现时调整

本文定义概念 crate `scsp-plugin-system`。它组合 `bevy_ecs` 的共享 AppWorld、Resource、SystemParam 和 System，实现 `scsp-plugin-api` 的 App、插件注册、CallbackSiteContainer、插件生命周期和 effect 回滚，但不提供生产 FFI 入口或 LateUpdate scheduler。

## 依赖边界

```text
scsp-core
  ↑
scsp-plugin-api
  ↑
scsp-plugin-system
  ↑
scsp-runtime
```

功能插件只依赖 plugin API 和必要的 core 类型，不依赖 plugin-system 内部结构。runtime 负责构造 App、注册具体插件并驱动生命周期。

## App 是组合根

`App` 从 bootstrap worker 构造到主线程运行始终是同一个 `Send` 类型，不引入 `PreparedApp`，也不使用本身为 `!Send` 的 `bevy_app::App`：

```rust
pub struct App {
    world: bevy_ecs::world::World,
    core: AppCore,
    plugins: PluginManager,
}
```

App 持有一个所有 plugin system 共享的 `bevy_ecs::World`，以及按注册顺序排列的 boxed system 列表。`AppCore` 保存 DebugDispatch、CommandDrain、`DebugState` 和 core-owned shared backend 等非插件组合状态；普通 plugin resource 统一进入 AppWorld。`DebugState` 只属于 AppCore，不进入 AppWorld。

概念上，`DebugState` 持有已启用 Debug 的 I/O worker、DebugHub、pending request 状态和供主线程 `DebugDispatch` 使用的窄 facade。DebugState 可以先作为无 worker 的 AppCore 状态参与 plugin build；只有 App 创建和 plugin build 处理完成、App 仍可继续运行后，才按 `debug.enabled` 决定是否创建 worker、socket 和 pending 状态。某个 plugin build 局部失败不阻止其它 plugin 或 Debug 启动。worker 或 socket 启动失败时，DebugState 进入 `Unavailable`，记录 observability 事件，不重试；后续 Debug request 统一返回 `runtime_unavailable`，不影响 App 和游戏。App 退出时由 `DebugState` 停止 worker 并关闭 transport；ObservabilityRoot 不属于 App，仍按 runtime 进程期生命周期保活。

App 在 worker 阶段只执行线程无关的 build；经过 Handoff 后由游戏主线程 TLS 独占。AppWorld 只接受 `Send + Sync + 'static` resource，不启用 Bevy non-send resource，因此 App 必须由编译期和无游戏 fixture 证明为 `Send`。Unity 主线程操作仍要求 runtime 创建、driver 只按借用传入的 `MainThreadToken`。

## PluginManager 与 owner scope

`PluginManager` 从 `App::new` 开始就是 App 的内部成员。`App::add_plugin` 为每个插件建立唯一 owner scope，并在该 scope 内调用 `Plugin.build`。

插件配置对象不作为长期 runtime 保存。manager 保留的是运行期所有权记录：

```rust
struct Plugin {
    state: PluginState,
    startup: Vec<BoxedStartupSystem>,
    update: Vec<BoxedUpdateSystem>,
    effects: Vec<Effect>,
    callback_sites: Vec<CallbackSiteHandle>,
    callback_container: Option<CallbackContainerHandle>,
    debug_routes: Vec<DebugRouteId>,
}
```

PluginState 只保存 driver 和路由实际消费的最小逻辑状态，例如能否运行 Startup/Update、能否接受 debug request 以及是否已逻辑退役。失败原因属于 observability event，不为每一种错误创建持久状态枚举。

PluginBuildContext 不直接暴露 manager 或 `bevy_ecs::World`。所有 resource、system、Hook、CallbackSiteContainer 和 debug route 注册都自动带当前 owner；resource 通过受限 facade 插入共享 AppWorld，system 通过 `IntoSystem` 转换并初始化后进入对应有序列表。插件间依赖只由固定注册顺序表达，不建立依赖图；build 期间缺失所需 resource 或重复 resource type 都是当前 plugin error。每个 plugin 至多注册一个明确类型的 CallbackSiteContainer。

## Build、Startup 与 Update

```text
worker：App::add_plugin
  → 建立 owner scope
  → 建立该插件的未提交 transaction
  → 使用共享 AppWorld
  → Plugin.build
  → 插入 resources
  → IntoSystem 转换并初始化 Startup/Update systems
  → 注册 plugin CallbackSiteContainer
  → 创建 worker-safe effects、安装功能 Hook（gate 关闭）

首次外层 LateUpdate driver
  → RuntimeGate 保持关闭
  → 按插件及注册顺序逐个调用 boxed Startup systems
  → 需要时创建 main-thread effects
  → 该插件全部 Startup systems 成功
  → 提交其完整 Build+Startup transaction
  → 插件进入 Active，并提交其 plugin gate
  → 全部插件处理完成且 App 可继续运行
  → runtime 最后开启 RuntimeGate

后续外层 LateUpdate driver
  → 按插件及注册顺序逐个调用 boxed Update systems
  → 只操作已提交到 AppWorld 的 resources
  → 跳过已经逻辑退役的 owner
```

首次 callback 不在 Startup 成功后继续运行 Update；Update 从下一次外层 LateUpdate 开始。driver 执行顺序固定并可复现，第一版不并行执行 systems，也不使用 Bevy Schedule executor。RuntimeGate 在整个 Startup driver 期间保持关闭，因此即使某个 plugin gate 已经提交，普通功能 callback 也要等所有 Startup 处理结束后才能进入插件逻辑。

## 固定 driver 顺序

第一版不构建 system dependency graph，也不提供 before/after 约束。这里的 schedule 是 SCSP driver 的固定阶段，不是 `bevy_ecs::Schedule`。Startup 的顺序严格为：

```text
plugin 注册顺序
  → 当前 plugin 的 Startup system 注册顺序
  → 当前 plugin transaction 提交或回滚
  → 下一个 plugin
  → runtime 最后开启 RuntimeGate
```

某个 Startup system 失败后，该 owner 剩余的 Startup systems 全部跳过；局部回滚完成后继续下一个 owner。插件注册顺序来自 runtime 的固定生产插件列表，单个插件的 system 顺序来自其 `Plugin.build` 调用顺序。

每次正常 Update 固定为：

```text
MessageMaintenance
  → DebugDispatch
  → CommandDrain
      → callback-to-main latest-value mailbox 转入 owner 的 Bevy Messages 或 typed handler
  → plugin 注册顺序
      → 当前 plugin 的 Update system 注册顺序
  → 返回 runtime 调用 original LateUpdate
```

每条跨执行域业务 route 都是 latest-value MPSC mailbox：可以有多个 producer，但只有一个实际 receiver；新值覆盖旧值，中间状态允许丢失，不提供竞争消费、FIFO、MPMC 或 broadcast route。MessageMaintenance 对所有已注册的主线程接收 route 执行一次等价于 `Messages<M>::update` 的维护，确保旧 buffer 有界回收；没有 Bevy Messages 接收端的 route 不参与。callback-to-main route 的唯一 receiver 是 MessageBridge/CommandDrain；它读取 mailbox 中该边界前可见的最新值，并可转入 Bevy `Messages<M>` 后让多个 main systems 通过各自 cursor 观察，但不改变跨域 route 的单 receiver 语义。阶段开始后写入的新值延迟到下一帧；需要多个 callback 消费者时注册多条 route。具体 mailbox 版本与单帧预算仍由 message route 注册时声明。

plugin Update system 在 CommandDrain 之后向 callback-to-main route 提交的 message 一律到下一帧处理；main-to-callback message 在下一次对应 callback 自然进入时才可见。当前 LateUpdate 或 callback 中刚发送的跨域 message 不对同一执行过程中的重入 callback 立即可见，message 系统也不主动唤醒 callback。某个 Update system 使 owner 退役后，该 owner 本帧剩余 systems 立即跳过；其它 owner 仍按原注册顺序继续。

单个插件 build 或 Startup 失败只回滚该插件；其它插件和 App 可以继续运行。只有 scheduler 核心、主线程交接、共享 AppWorld/system adapter 或 driver 不变量失败才升级为 runtime 级故障。

每个 Startup/Update boxed system 都由 plugin-system 在调用 `bevy_ecs::system::System::run` 外建立独立 `catch_unwind` 边界。system 正常返回 error、Bevy parameter validation/run error 或发生 Rust panic 时，plugin-system 由当前列表位置准确知道所属 owner，关闭其 gate、逻辑退役并回滚该插件，然后继续后续 owner；当前 owner 剩余 systems 不再运行。panic 不得直接越过整个 App driver。某个插件包含多个 Startup system 时，前面 system 创建的 resources/effects 继续留在同一个未提交 transaction 中；全部成功才整体提交，任一个失败则连同 Build effects 一起逆序回滚。

Startup system 的输入是当前栈上 `(&MainThreadToken, &mut StartupRegistrar)`；Update system 只取得 `&MainThreadToken`。driver 在 system 返回后检查 registrar：正常成功才把 staged resources 插入共享 AppWorld 并追加 restore actions；error/panic 路径不提交 staged resources，并把已经登记且对应外部操作可能发生的 restore actions 纳入本插件回滚。staged resource 只对后续 Startup system 可见；同一 system 内重复类型、与 AppWorld 已有类型冲突或 restore action 登记失败都会使当前 plugin Startup 失败。该事务语义不扩展到 Update，Update 只能修改已提交 resource。

transaction 提交只表示这些 resources 可以被 Update 使用并允许 plugin gate 开启，不会丢弃外部 effect 的 restore action、恢复基线或逆序顺序；这些记录继续保留到插件退役。它们不被 scheduler global failure 重新收集或执行。

只有 system 边界之外的 plugin-system 基础设施 panic，例如 driver 遍历、owner 映射、共享 AppWorld/system adapter 或回滚调度不变量失败，才继续向 runtime 返回 scheduler-level failure。`AssertUnwindSafe` 只能出现在经过审阅的 `System::run` 调用适配层，不能用来宣称任意 resource、Bevy World 或 effect 天然 unwind-safe。

## AppWorld 与 System driver

所有 plugin record 共享同一个 `bevy_ecs::World`。owner 不再通过物理 World 隔离，而由 PluginManager 记录 system、gate、restore action 和 callback container 的归属：

```text
Plugin A ─┐
Plugin B ─┼→ AppWorld → Res<T>, ResMut<T>
Plugin C ─┘
```

同一具体 resource type 在 AppWorld 中只能存在一份。`PluginBuildContext` 和 `StartupRegistrar` 在调用 `World::insert_resource` 前先检查是否已存在，重复插入返回 plugin error，不使用覆盖旧值的默认行为。插件需要独立状态时使用自己的 newtype；需要共享状态时直接使用双方约定的公开 resource type。

plugin API 只 re-export `Resource`、`Res`、`ResMut`、`Message` derive/trait、必要的 system input/conversion 类型，以及 SCSP 自己的跨域 message endpoint facade。隐藏 `World`、底层 `Messages<M>`、`Commands`、Entity、component query 和 schedule API，既避免插件绕过 phase capability，也使 v1 保持 resource-oriented 而不是引入没有消费者的完整 ECS model。

每个 boxed system 在 build 时针对共享 AppWorld 完成 `System::initialize`，运行时由 SCSP driver 按 Vec 注册顺序调用 `System::run`。Startup 与 Update 使用两个独立的 phase-specific adapter，不能交叉注册；Bevy 负责多 resource SystemParam 的类型匹配、缺失校验和共享/独占借用冲突；SCSP 负责 owner、phase input、panic boundary、gate 和 rollback。第一版不启用并行 executor、deferred `Commands` 或 change-detection 驱动的隐式调度。

AppWorld resource 可以由任意已注册、且按 API 约定使用它的 system 取得 `ResMut<T>`。如果 system 在可变操作中 panic，该 resource 可能处于业务上不完整但内存安全的状态；plugin-system 随即退役当前 owner，但不自动删除共享 resource，避免其它 plugin 的 system 契约被隐式破坏。

共享数据直接放在 AppWorld 的 typed resource 中；不再为普通 plugin 间共享设计额外 capability 或跨 World bridge。涉及 Unity 或其它线程能力的 backend 仍通过 core 提供窄 handle，不因 AppWorld 共享而扩大权限。

共享可写行为若需要固定阶段或额外校验，仍由 core-owned CommandDrain 串行执行。message 处理或 core-owned shared state 的不变量失败属于 App/plugin-system 基础设施失败，而不是任意调用方插件可以继续掩盖的局部失败。

采用 `bevy_ecs` 后不再设计自有多资源 query/downcast/borrow 引擎。仍需由无游戏 fixture 验证：共享 AppWorld 只启用 `Send + Sync` resource 时 App 可跨 Handoff；boxed system panic 被外层捕获后 World 可以保活并继续驱动其它 owner；Startup/Update 两种 input type 不可误注册；未 re-export 的 API 无法从 plugin API 安全绕过。

## CallbackSiteContainer

每个需要 callback 的插件注册至多一个自己定义的 CallbackSiteContainer；同一插件的多个 callback site 可以共享它。容器是字段明确的普通 Rust struct，不使用 `bevy_ecs::World`、`anymap2` 或其它动态 type map。没有 callback 的插件不创建容器。

容器只保存 callback 经过审阅后允许使用的窄 handle 和数据，例如 `Frozen<T>`、原子、bounded message endpoint 或 `CallbackIl2Cpp`。`Frozen<T>` 在 container 注册前构造完成，第一版不支持运行时替换；需要变化的数据使用原子、mailbox 或 bounded message 表达。AppWorld resource 与 callback container 字段可以指向同一个受审阅 backend，但 callback 不借用 AppWorld 本身。

`PluginBuildContext` 接受容器后返回一个 `Arc<C>`，并拒绝当前 plugin 重复注册。注册一旦成功，container 不可替换或注销；插件退役时只关闭 gate、停止 systems 和 routes，不释放 container。各目标专用 CallbackSite 保存该 Arc；replacement 通过目标唯一静态 OnceLock 找到 CallbackSite，再直接访问具体容器字段，不做运行时类型查询。

概念 API 为：

```rust
let container = ctx.register_callback_container(TranslationCallbackSites {
    translations,
    commands,
})?;
```

返回的 `Arc<C>` 交给本插件的目标专用 Hook builder；plugin-system 不提供通用 registry、动态类型查询、替换或注销操作。

每个目标专用 wrapper 的静态 OnceLock 是 CallbackSite 和 container 的进程期 retention root。PluginManager 只保留指向静态 site/container 的 handle 和 Hook restore action，不是 callback context 的最终生命周期所有者。container、gate reader、typed original 和 callback 所需 backend 因而独立于 App 与 PluginManager 保活到进程退出。

## Effect 与局部回滚

plugin-system 不建立第二套 typed effect storage。运行期可操作状态放在共享 AppWorld resources；owner 索引负责机械管理该 plugin 注册的 systems、debug routes、plugin gate、callback container 和静态 site handle。只有确实修改外部状态、需要以后恢复的操作才向 transaction 追加内部 restore action。

新的 restore action 只能在 Build 或 Startup 登记；Startup 全部成功前均为未提交状态。Update 不提供 restore registrar，也不允许插入新的 AppWorld resource、Hook 或 route，只能修改已经提交的 controller resource。恢复基线由对应 action 在首次登记时捕获；Update 改变当前值不追加记录，因此每帧执行不会让恢复日志无界增长。Update 操作 error/panic 仍退役插件，并执行已有 restore actions。

内部表示保持最小：

```rust
enum RestoreAction {
    AnyThread(
        Box<dyn FnOnce() -> Result<(), RestoreError> + Send>,
    ),
    MainThread(
        Box<
            dyn FnOnce(&MainThreadToken)
                -> Result<(), RestoreError>
                + Send,
        >,
    ),
}
```

Build 只能登记 `AnyThread`；Startup 可以登记两种 action。需要持续控制外部状态时，action 和 AppWorld controller resource 可以各自捕获同一个 `Arc` backend，但 restore action 必须已经独立持有完成恢复所需的数据，不能在退役后反向借用 system 栈。

rollback 在调用前先从 ledger 尾部取出 action，再以独立 `catch_unwind` 执行。因此每个 action 至多执行一次；error 或 panic 都记录为本项失败并继续更早的 action，不会重试可能非幂等的恢复。`RestoreError::OwnershipLost` 表示确认当前外部状态已经不归本 action 所有且没有写入；其它无法确认的情况使用普通 failure。它们是本轮返回值，不引入持久 effect 状态枚举。

创建过程使用插件局部 transaction：

```text
action 1
  → action 2
  → action 3 对应的外部操作失败

回滚：action 2
  → action 1
```

失败范围为：

```text
build 失败
  → 执行该插件已经登记的全部 restore actions

Startup system 失败
  → 执行该插件从 Build 到 Startup 登记的全部 restore actions

Update system error/panic
  → 逻辑退役并执行该插件全部 restore actions
```

插件局部回滚固定顺序为：

1. 原子关闭插件全部功能 gate。
2. 禁用其 Update systems 和 debug routes。
3. 将尚未执行的 owned debug requests 统一结束为 `plugin_unavailable`。
4. 从 ledger 尾部逐个取出并执行 RestoreAction。
5. 保留 AppWorld resources；只停止该 owner 的 systems 和外部行为。
6. 把插件标记为逻辑退役并停止之后的业务行为。

MethodPointer Hook 的 restore action 只有在实际 slot 仍属于该插件时才 CAS 恢复；ownership drift 时不得盲写。action 无论返回 error 还是 panic 都已经消费且不重试；即使 slot 仍可到达 replacement，静态 OnceLock 仍继续保留 CallbackSiteContainer、callback site、typed original 和 callback backend，因此不会因为恢复器被销毁而产生 UAF。

“逻辑回滚完成”只表示功能停用且所有 restore actions 都已经尝试一次，不表示每个外部状态都恢复成功，也不表示静态 callback 对象已释放。其它插件和 App 继续运行。

## Runtime global failure

plugin-system 不提供跨插件的 global rollback。Handoff 成功后，scheduler、App 或 plugin-system 基础设施发生 runtime 级故障时，由 runtime 关闭 RuntimeGate 并停止后续 App/plugin 业务逻辑；不会等待未来 callback 执行全局回滚，也不会调用各插件的 restore ledger。

Handoff 成功后发生的这种失败路径仍保活 App、静态 CallbackSiteContainer、typed original 和 callback backend，直到进程退出或更高层生命周期结束。保活的目的只是避免仍可达 callback 发生 use-after-free，并为 replacement 提供安全的 original passthrough；它不表示之后还会执行全局恢复。Handoff 前的 bootstrap failure 按下方“App 级失败后的保活”处理，不进入这里的 runtime global failure 语义。插件自己的 Build/Startup/Update 失败仍按本节前述规则执行局部回滚。

## 物理卸载与资源回收

第一版不支持进程内物理卸载插件。真正释放插件对象、Hook 和 callback context 的协议仍为待设计：

```text
关闭 gate
  → ownership-aware 恢复 slot
  → 阻止新 callback 进入插件逻辑
  → 等待在途 callback 归零
  → 处理外部缓存的 replacement pointer
  → 释放 context、effect 和 resources
```

在该协议完成前，不得因为 slot 已恢复或短时间未观察到 callback 就推断 context 不可达。第一版的静态 site 不删除、不置空也不复用；retired plugin ownership record 可以退出 App 的活动集合，但不能使静态 site 引用任何由该 record 独占的短生命周期对象。

## Debug route 所有权

plugin-system 负责把 debug topic route 归属到 plugin owner，并在插件逻辑退役时先禁用 route；它不实现 wire transport。topic 唯一性、main/callback 执行域、pending request 和回复竞态的完整设计集中在 [Debug、Diagnostics 与 Logging](debug-diagnostics-logging.md)。

所有 plugin debug route 还必须读取 RuntimeGate。总 gate 关闭时，新请求不得进入插件 handler；main route 返回 `runtime_unavailable`，callback route 不再投递新的插件工作，已经 pending 但未执行的请求统一回复 `runtime_unavailable`。已进入 handler 的请求不被强行中断，仍由既定 response 或外层 callback 安全边界完成。Debug handler 返回已声明的业务错误时只回复当前 request 的 `handler_error`；发生 panic 时才记录 observability、让当前 request 返回 `plugin_unavailable`，并按 owner-local failure 规则禁用该 owner 的 debug routes，不影响其它 plugin。

## App 级失败后的保活

SchedulerHook 安装失败时 App 尚未进入 Handoff。worker 先关闭 RuntimeGate 并尽力恢复已经安装的功能 effects；无论恢复是否确认，所有已经发布的功能 CallbackSite 都继续由各目标的静态 OnceLock 保活，因此不依赖 App 留存来避免 UAF。

任一功能 Hook 存在 ownership drift 或恢复结果无法确认时，关闭的 RuntimeGate 使其 callback 只走静态 site 中的 original；CallbackSiteContainer、gate reader 和 callback backend 也随 site 保活到进程退出。Hook effect 的最终恢复结果仍需记录，但第一版不为了保存可回收性而把整个 App 留在额外全局槽中。

runtime global failure 不等同于逐个插件故障。它首先关闭 RuntimeGate，使全部 feature callback 和 plugin debug route fail-closed；随后停止 driver，并保留所有可能可达的 context。它不调用各插件的 restore ledger，也不等待未来 callback 执行跨插件恢复；关闭的总 gate 必须保证新的 callback 只走 original 路径。

## 待打磨项

- PluginState 只保留 driver/route 所需最小状态，不扩展为错误分类状态机。
- CallbackSiteContainer 的具体物理 handle 类型仍可在实现时选择；每个 plugin 至多一个、不可替换或注销，v1 不提供可更新 snapshot。
- callback endpoint 的固定大小 `Copy` message 约束与 latest-value mailbox 公开语义已确定，内部存储仍可在实现时选择。
- 未来物理卸载的 in-flight/quiescence 协议。
