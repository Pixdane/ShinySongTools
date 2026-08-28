# Plugin API 设计

状态：v1 设计已收敛；具体 Rust 类型别名和物理模块可在实现时调整

本文定义功能插件作者可见的公共 API，概念 crate 名为 `scsp-plugin-api`。它依赖 `scsp-core`，但不暴露 PluginManager、调度器 TLS、Handoff、effect 存储或 runtime bootstrap 内部实现。

## API 目标

插件采用 Bevy-style 的 App 配置模型，并复用 `bevy_ecs` 的 resource、SystemParam 和 System 实现；不引入 entity/component gameplay model、`bevy_app::App` 或 Bevy runner：

```text
Plugin 配置 App
typed resources 保存状态
Startup/Update systems 保存主线程行为
plugin-defined CallbackSiteContainer 保存 Hook callback 可见状态
```

插件配置对象不承担逐帧 runtime 职责，不再定义长期 `PluginRuntime` trait object，也不引入 `PreparedPlugin` 变体。

## Plugin 入口

```rust
pub trait Plugin: Send + Sync + 'static {
    fn build(
        &self,
        app: &mut PluginBuildContext<'_>,
    ) -> Result<(), PluginError>;
}
```

`PluginBuildContext` 是带当前插件 owner 的受限 facade。插件可以：

- 读取 DataRoot 和被授权的 core facilities。
- 向共享 AppWorld 插入 typed resources。
- 注册 Startup/Update systems。
- 注册一个 plugin 自己定义的 CallbackSiteContainer。
- 通过外部 Hook API 准备和安装目标专用 Hook，初始 gate 保持关闭。
- 注册 main-domain 或 callback-domain debug topic。

插件不能绕过 context 直接取得 PluginManager、修改其它插件的 owner scope、直接写 MethodPointer slot，或者制造未登记的外部 effect。

## App Resource API

`scsp-plugin-api` 选择性 re-export `bevy_ecs` 的 `Resource`、`Res`、`ResMut`、`Message` derive/trait 和必要的 system conversion/input 类型，并提供自己的跨执行域 message endpoint facade。插件不直接依赖 plugin-system 内部结构，也不取得 `World`、`Commands`、Schedule、Entity、component API 或底层 `Messages<M>` resource。

普通 App resource 使用：

```rust
#[derive(Resource)]
struct TranslationState { /* ... */ }
```

主线程 system 通过 Bevy `SystemParam` 查询共享 AppWorld：

```rust
fn update(
    InRef(main): InRef<MainThreadToken>,
    state: Res<TranslationState>,
    mut controller: ResMut<TranslationController>,
) -> Result<(), PluginError>
```

所有 plugin system 在同一个 `bevy_ecs::World` 上运行，因而可以通过同一资源类型显式共享状态。一个具体 resource type 在 AppWorld 中只能存在一份；重复插入必须由 build/startup facade 转换为错误，不允许使用 Bevy 的覆盖语义。两个插件需要同形但独立的状态时，必须定义不同 newtype。插件间依赖只由 runtime 的固定插件注册顺序表达：前序插件已经注册的 resource 可被后续插件使用；v1 不提供依赖声明、自动排序或缺失依赖的独立 graph。build 期间读不到所需 resource 直接使当前 plugin build 失败。

App 从 worker 构造并经过 Handoff，因此第一版不接受 `!Send` resource。Unity 主线程敏感操作通过 `MainThreadToken` 限权；`Resource: Send` 不表示其中的 Unity 操作可在任意线程执行。

插件私有状态仍应使用插件自己的 resource newtype，避免无意形成共享契约；需要跨插件共享时则直接约定公开 resource 类型。多资源 typed query、共享/独占借用冲突和缺失 resource 校验由 `bevy_ecs::SystemParam` 完成。v1 不额外设计 global resource capability layer 或跨 World bridge。

## Startup 与 Update systems

插件按行为注册 system：

- Startup system：首次外层 LateUpdate 在主线程执行一次；可以插入运行状态 resource 并登记恢复动作，全部成功后插件 gate 才能开启。
- Update system：后续外层 LateUpdate 按固定顺序执行；只能操作已经提交到 AppWorld 的 resource，不能登记新恢复动作。
- 事件驱动插件：可以不注册 Update，只安装 Hook 和 callback site container。

system 的第一个输入区分 Startup/Update capability，而不是在运行时用一个布尔值检查：

```rust
fn startup(
    (InRef(main), InMut(startup)):
        (InRef<MainThreadToken>, InMut<StartupRegistrar>),
    config: Res<PluginConfig>,
) -> Result<(), PluginError>;

fn update(
    InRef(main): InRef<MainThreadToken>,
    mut state: ResMut<PluginState>,
) -> Result<(), PluginError>;
```

`MainThreadToken` 只以当前 system 调用期间的 `InRef` 传入。`StartupRegistrar` 由 driver 在栈上创建并以 `InMut` 传入，只提供暂存新 AppWorld resource 和登记恢复动作的窄 API；system 返回后 driver 才把成功暂存的 resource 插入共享 AppWorld，并把 action 追加到 transaction。新 resource 对之后的 Startup system 可见，不承诺在登记它的同一个 system 内可查询。一个 Startup system 的 staged resource 类型在该 system 内不得重复；与已存在于 AppWorld 或同一插件 transaction 的类型冲突时返回 plugin error。

Update system 没有 `StartupRegistrar`，plugin API 也不 re-export Bevy `Commands` 或 `World`，因此不能通过安全公开 API 插入 resource、注册 Hook/route 或登记恢复动作。Startup/Update 输入借用均不能保存到下一帧。

插件运行状态统一使用 AppWorld 中的 typed resource，不另建 typed effect arena。Build 阶段只能登记可在 worker 执行的恢复动作；Startup 可以登记需要可信 `MainThreadToken` 的恢复动作，并把控制外部状态所需的 typed controller 插入 AppWorld 供 Update 查询。Update 不提供 restore registrar、resource insertion 或动态 Hook/debug route 注册入口；需要逐帧改变外部状态时修改 controller resource，而不是追加恢复记录。

plugin-system 使用 `bevy_ecs::system::IntoSystem` 把函数或 closure 转换为内部 boxed system，但不采用 Bevy Schedule。内部只保留两个 phase-specific adapter：`BoxedStartupSystem` 和 `BoxedUpdateSystem`；它们都把 Bevy 的 run/parameter error 归一化为 plugin-system error，再由 driver 处理 owner-local failure。Startup 与 Update 使用不同的 boxed input 类型，不能互相注册。插件不得创建自己的长期 tick 或绕过 App driver 建立独立更新线程。

第一版 system 顺序只由两级注册顺序决定：`App::add_plugin` 的插件顺序，然后是该插件调用 `add_startup_system` 或 `add_update_system` 的顺序。Plugin API 不提供 `.before()`、`.after()`、任意 schedule label 或依赖图；跨插件协作通过共享 AppWorld resource 或 message route 完成，并接受固定基础阶段边界。

## Callback Site Container API

普通游戏 Hook callback 不访问 App、共享 AppWorld、PluginManager 或主线程 TLS。需要 callback 的插件在 `Plugin.build` 中注册至多一个自己定义的 `CallbackSiteContainer`；注册成功后不可替换或注销，插件退役时只关闭 gate、不释放容器；没有 callback 的插件不注册。容器是字段明确的普通 Rust struct，不是第二个 `bevy_ecs::World`、动态 type map 或全局 service registry。

```rust
struct TranslationCallbackSites {
    translations: Frozen<TranslationTable>,
    commands: CallbackMessageWriter<TranslationCommand>,
}
```

容器必须满足 `Send + Sync + 'static`，并只保存 callback 经过审阅后允许使用的数据和窄 handle，例如不可变 `Frozen<T>`、原子、mailbox、bounded writer 或 `CallbackIl2Cpp`。这是静态编译的受信任插件边界；callback-safe 性仍需按具体字段和 handler 审阅，不用一个通用 callback resource type map 伪装成自动证明。第一版不提供运行时替换 frozen data 或可更新 snapshot API。

`PluginBuildContext::register_callback_container(container)` 概念上返回 `Arc<C>`，并拒绝同一 plugin 重复注册。该 `Arc<C>` 只用于构造本插件的目标专用 CallbackSite；callback 本身不按类型查询容器。该注册操作只允许成功一次，之后不提供替换或注销 API。

目标专用 callback context 的形状为：

```rust
pub struct CallbackContext<'a, F, C> {
    site: &'a CallbackSite<F, C>,
}

pub struct CallbackSite<F, C> {
    original: F,
    runtime_gate: RuntimeGateReader,
    plugin_gate: PluginGateReader,
    container: Arc<C>,
}
```

typed original、runtime gate 和 plugin gate 是 CallbackSite 的结构字段。callback 只有在两个 gate 都开启时才能执行插件逻辑；任一 gate 关闭时，仍通过 site 中的 original 透明回退。callback context 必须先完整发布，外部 Hook API 才能安装 MethodPointer replacement。

第一版中，每个受支持 Hook 目标都由目标专用 wrapper 声明一个进程期静态槽，概念上为 `static TARGET_SITE: OnceLock<CallbackSite<TargetFn, TargetContainer>>`。replacement ABI 不携带 userdata；replacement 通过与自身一一对应的静态槽取得 `&'static CallbackSite<TargetFn, TargetContainer>`，不查询 App、PluginManager 或动态全局 map。

外部 Hook API 必须在安装 CAS 前完成 `TARGET_SITE.set(site)`。因此只要 replacement 能由该 API 安装并被调用，site 就必然已经存在；`TARGET_SITE.get() == None` 属于构造不变量破坏，不是正常回退分支。实现不得在这个分支 panic 穿越 FFI，也不能尝试从当前 MethodPointer slot 反推出 original。

`RuntimeGateReader` 由 PluginBuildContext 自动注入，插件不能取得总 gate 的控制 handle。`PluginGateReader` 归属当前插件；只有 plugin-system 能在 Startup 提交或逻辑退役时改变它。gate 关闭只阻止之后观察到关闭状态的新插件逻辑，已经越过检查的在途 callback 仍依赖进程期 site 和 container 保活。

callback 不做阻塞 I/O、wire 解码、无界分配或等待 mutex。`Send + Sync` 本身不等于 callback-safe。callback handler panic 不得跨越 `extern "C"` 边界；具体 exactly-once original 调用由目标专用 wrapper 保证。

## 跨执行域 Message API

callback 与主线程使用同一套 typed message 注册 API，但不同执行域取得不同 endpoint。插件只声明一次 payload、方向和单帧预算；v1 业务 route 的存储语义固定为 latest-value mailbox，不再声明 FIFO 容量或满载策略。API 根据接收域选择内部实现，不要求插件自己把底层 mailbox 与 Bevy `Messages` 接起来：

```text
app.message::<M>()
  → callback_to_main(config)
  或 main_to_callback(config)
```

概念 endpoint 为：

```text
callback → main
  CallbackMessageWriter<M>::try_send
  MainMessageReader<M>（SystemParam facade）

main → callback
  MainMessageWriter<M>::try_send（SystemParam facade）
  CallbackMessageReader<M>::try_read
```

v1 固定暴露四个 endpoint facade：`CallbackMessageWriter<M>`、`MainMessageReader<M>`、`MainMessageWriter<M>` 和 `CallbackMessageReader<M>`，共享同一个 message 类型和 route registration。每条业务 route 都是 latest-value MPSC mailbox：writer 可以 clone、允许多个 producer，但只有一个实际 receiver；新值覆盖旧值，中间状态允许丢失。writer 的 `try_send` 只返回 `accepted` 或 `replaced`，不返回 `Full`，不阻塞，也不执行任意 payload 析构；reader 的 `try_read` 返回当前可见值或 `None`，每次最多取一个值。API 不提供 FIFO、竞争消费、MPMC 或 broadcast 语义。需要多个独立 callback 消费者时，插件注册多条 route，而不是让多个 reader 争抢同一条 route。普通 AppWorld 内的 plugin message 不受 callback mailbox 的类型限制，仍可使用一般的 Bevy `Message`。

callback 需要修改主线程状态时，只向该统一 API 的 callback-to-main writer 提交 owned message：

```text
Hook callback
  → CallbackMessageWriter<M>::try_send(message)
  → latest-value callback-safe mailbox
  → 返回 accepted/replaced

下一次外层 LateUpdate
  → MessageBridge / CommandDrain
  → 写入共享 AppWorld 的 bevy_ecs::Messages<M>
  → MainMessageReader<M>
```

callback-to-main route 只有一个跨域 receiver：MessageBridge/CommandDrain。它可以把收到的 message 转入共享 AppWorld 的 Bevy `Messages<M>`，之后一个或多个 main systems 使用各自的 per-system cursor 读取同一批消息；这些 systems 是主线程内部的观察者，不是跨域 route 的竞争 receiver。只有一个消费者且需要直接修改 core-owned backend 时，MessageBridge 可以把 message 交给 typed CommandDrain handler，而不建立 Bevy message buffer。这是相同公开 route 的内部优化，不改变插件 API。

main-to-callback route 也只有一个 callback receiver。若多个 callback 目标需要相同数据，分别注册 route，并由主线程向每条 route 各发送一次；不共享一个竞争消费 inbox，也不引入 broadcast reader。

消息只在下一个执行边界可见，不进行重入投递：callback-to-main message 在下一次外层 LateUpdate 的 CommandDrain 处理；main-to-callback message 在下一次对应 callback 自然进入时读取。当前 callback 或当前 LateUpdate 中刚发送的跨域 message，不对同一执行过程中的重入 callback 立即可见；message 系统也不主动唤醒 callback。

MessageBridge/CommandDrain 在自身阶段开始时为每个 mailbox 捕获当前版本，并且每条 route 最多取出一个该边界前可见的最新值；阶段开始后写入的新值留到下一帧。即使 producer 持续并发提交，单帧处理也不会变成无界循环。DebugDispatch 在 CommandDrain 之前，因此 main debug handler 在本帧提交的 main-domain message 仍可进入随后阶段；这不改变跨 callback 边界 route 的下一执行边界可见规则。

即使 Hook callback 当前碰巧位于主线程，也不直接借用 AppWorld。callback 边界 message 必须满足固定大小的 `Copy + Send + Sync + 'static`，不得携带 callback 栈借用、IL2CPP 临时参数地址或其它短生命周期指针；这保证 v1 的 queue/mailbox 操作不会执行任意 payload 析构。普通 AppWorld message 只需满足 Bevy `Message` 的类型约束。

所有 v1 业务跨域 route 都只表达最新状态，使用 latest-value mailbox；不提供需要保序的 FIFO route。统一 writer 仍使用 `try_send`，但 mailbox 写入只返回 accepted 或 replaced，不返回 `Full`；callback 不阻塞等待容量，写入只替换当前值。Bevy `Messages<M>` 只存在于主线程接收端，不能替代跨执行域 mailbox，也不能由 callback 直接取得 `MessageReader` 或 `MessageWriter`。

## Hook 注册 API

功能 Hook 必须通过 `PluginBuildContext` 暴露的外部 Hook API 完成。插件提供受支持的目标专用 wrapper；API 负责把当前 RuntimeGateReader、PluginGateReader、plugin 的 CallbackSiteContainer 和 typed original 组成完整 CallbackSite，并在发布 site 后安装 Hook：

1. 解析并校验 MethodRef。
2. 捕获 typed original，构造目标专用 CallbackSite，并在当前插件 transaction 中预留不会再分配的 Hook ownership 和 restore action 记录。
3. 把 site 发布到该目标唯一的静态 OnceLock；发布失败即拒绝本次注册并撤销未安装的记录。
4. 调用 core `MethodPointerSlot` 完成 ownership-aware 安装，并只在已经存在的记录中提交 installed 状态。
5. transaction 保留 plugin gate、静态 site handle 和 Hook restore action。

静态槽一旦发布便占用该目标直到进程退出；即使之后 CAS 安装失败、Hook 已恢复或插件已逻辑退役，也不清空或复用。因此第一版一个目标只能有一个静态 site，不支持同一目标多实例、重复注册、重新安装、共享 slot chaining 或物理卸载。插件不能取得底层 slot 的无约束写权限；第一版也不提供通用 Hook backend trait、ABI 自动推断、热更新或任意地址 inline hook。目标专用 Hook builder 对外统一返回稳定的 `HookError`，至少区分 `target_unavailable`、`signature_mismatch`、`site_already_registered`、`slot_conflict` 和 `installation_failed`；不把 CAS、readback 或具体 MethodPointer 地址暴露给插件。

## Debug topic API

Plugin API 提供 typed topic 以及 main-domain/callback-domain 的选择性注册入口。v1 topic payload 只使用标准 JSON/serde 可序列化/反序列化的类型，不提供二进制或插件自定义 codec；单个 wire frame/payload 的最大大小是固定内部上限，超限请求在 I/O worker 层返回 `payload_too_large`。main handler 由下一次外层 LateUpdate 的 DebugDispatch 阶段执行，callback handler 等对应 Hook 自然进入；两者都不由 I/O worker 直接调用。schema/协议错误返回 `decode_error` 或 `invalid_request`，不进入 route；未知 `topic` 返回 `unknown_topic`；typed handler 的业务错误只返回当前 request 的 `handler_error`，panic 才触发该 owner 的局部退役。v1 只允许一个 Debug socket 连接，已有连接时新连接返回 `queue_full` 并关闭。Debug request/response 不使用业务状态的 latest-value 覆盖语义，而使用 bounded pending-request 通道；每个已接受的 request 独立保留并通过 correlation ID 回复。同一连接内同时 active 的 request `id` 必须唯一，response 写回后可复用；客户端断开时释放尚未开始执行的 request，不取消已开始的 handler；Debug worker 停止时尚未完成的 request 统一以 `runtime_unavailable` 回复。wire 层对后端只暴露 `id/topic/version/payload` request 与 `id/ok/payload` 或 `id/ok/error` response envelope；无法关联 request ID 的协议错误使用 `id: null`。

## 功能模式示例

```text
Translation
  → 启动期构造 TranslationTable
  → TranslationCallbackSiteContainer 持有 Frozen<TranslationTable>
  → Localization Hook callback 查询 frozen table

Camera
  → CameraState AppWorld resource
  → Update system 使用 MainThreadToken
  → callback 输入通过统一 message route 交给主线程

FPS
  → 根据最终实现注册一次性 Startup system 或目标专用 Hook
```

Translation 文件缺失、解析失败或目标解析失败时只退役 Translation 插件并回退原文，不阻塞其它插件或 runtime。

## API 待打磨项

- Frozen 的具体包装类型和 mailbox 的内部存储实现仍可在实现时选择；v1 不提供可更新 callback snapshot。
- CallbackSiteContainer site handle 的具体物理类型仍可在实现时选择；每个 plugin 至多一个且注册后不可替换或注销。
