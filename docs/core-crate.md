# Core crate 设计

状态：v1 设计已收敛；具体 Rust 类型和物理模块可在实现时调整

本文定义概念 crate `scsp-core`。它提供平台基础 API、跨执行域共享设施和底层安全封装，不了解 App、PluginManager、具体功能插件或 runtime bootstrap 流程。

## 依赖边界

`scsp-core` 位于依赖图底部：

```text
scsp-core
  ↑
  ├─ scsp-plugin-api
  ├─ scsp-plugin-system
  └─ scsp-runtime
```

core 可以定义通用错误、typed handles、callback-safe primitives 和 platform-specific unsafe boundary。实现优先组合已经审阅的 crate，而不是重新实现通用容器或 facade：

- `thiserror` 派生公开的 typed error。
- `il2cpp-bridge-rs` 提供已经由实验验证的 IL2CPP API table 和 metadata 查询基础。
- `crossbeam-queue` 提供默认的 bounded queue primitive；SCSP 的跨执行域 route 只暴露 MPSC 拓扑，即多个 producer、一个实际 receiver，不暴露 MPMC、竞争消费或 broadcast 语义；只有 producer/consumer 拓扑已证明为 SPSC 时才使用 `rtrb`。
- `tracing` 作为普通执行域统一的结构化事件 facade。

这些依赖不改变 core 的责任边界。core 不得：

- 持有 App 或 PluginManager。
- 注册功能插件或决定插件顺序。
- 创建 Startup/Update system 列表或 driver。
- 实现 `scsp_start`、Handoff 或 LateUpdate TLS 调度。
- 了解 Translation、Camera、FPS 等具体功能。

## 基础类型与线程 capability

core 提供不包含业务状态的基础类型，例如 DataRoot、稳定 ID、错误分类、进程期 backend handle 和 `MainThreadToken`。

`MainThreadToken` 是不可跨线程的 capability。它只能由 runtime 在当前 callback frame 完成线程身份校验后通过受审阅的 unsafe 构造边界创建；需要 Unity 主线程的安全 API 必须显式要求该 token，不能只依赖调用者约定。

```rust
pub struct MainThreadToken {
    _not_send_sync: PhantomData<Rc<()>>,
    _private: (),
}
```

token 类型本身不携带 lifetime，以便作为 `bevy_ecs::system::InRef` 的输入目标；runtime 和 plugin-system 只向 system 传递当前栈上 token 的短借用，不把 token 所有权交给插件。token 不实现 `Clone`/`Copy`，构造函数不公开，因此安全插件代码不能把当前证明保存为 `'static` resource。输入引用的 lifetime 才是 system 本次调用的 capability 边界。

App 和普通 Resource 可以继续满足 `Send`；这只允许转移所有权，不授权在任意线程执行 Unity 操作。

v1 的平台判据固定为当前线程调用 `pthread_main_np() != 0`。runtime 在每次最外层 scheduler callback 中重新验证，成功后只为该 callback frame 构造 token；Swift main queue、bootstrap worker 身份和“曾经在正确线程运行过”都不能制造或缓存 token。该判据必须先由目标环境实验确认 LateUpdate 确实位于 process main thread；证据不成立时实现保持 NO-GO，而不是改为信任首次 callback。

## 共用设施 backend 与窄 handle

跨执行域共享的基础件可以拥有进程期 backend，并针对不同执行环境提供 API 不同的 cloneable handle：

```text
Arc<Backend>
  ├─ bootstrap/runtime handle
  ├─ App/system handle
  └─ callback-safe handle
```

不得用一个包含全部权限的 `SharedInfra` handle 代替这些角色。共享同一个 `Arc` 只解决所有权和生命周期，不表示 backend 的全部操作都可跨线程调用。

Logging 和 Diagnostics 统一为基于 `tracing` 的 Observability；Debug Control Plane 保持独立。状态与跨 crate 边界单独记录在 [Debug、Diagnostics 与 Logging](debug-diagnostics-logging.md)。

### IL2CPP

`Il2CppBackend` 负责保活 exact UnityFramework handle、通过固定版本 `il2cpp-bridge-rs` 加载的 IL2CPP API 表以及已经确认的 runtime/layout 身份。PlayCover 环境不能假定 `RTLD_DEFAULT` 可见性；runtime 必须把精确 UnityFramework handle 交给 `il2cpp-bridge-rs::api::load` 所需的 exact-handle resolver，不重新实现第二套导出表或高层初始化器。

```rust
pub struct Il2CppRuntime(Arc<Il2CppBackend>);
pub struct CallbackIl2Cpp(Arc<Il2CppBackend>);
```

`Il2CppRuntime` 是正常解析和调用入口；需要主线程的安全方法还必须接受 `&MainThreadToken`。`CallbackIl2Cpp` 只暴露对应 callback 已审阅的有限操作，不提供任意导出调用、任意地址访问或隐式线程附着。callback 只有在插件明确导入该 capability 时才能取得它。

`Il2CppBackend` 是否能够安全实现 `Send + Sync` 必须由具体字段、动态库 handle 生命周期和每个公开方法的线程约束证明，不能仅为满足 Resource bound 添加无依据的 `unsafe impl`。

bootstrap worker 使用 IL2CPP API 前必须遵守显式 attach 生命周期：等待 domain 非空，检查当前 worker 是否已附着，只在未附着时调用 thread attach，并用 RAII guard 仅 detach 本次自己建立的 attachment。callback 侧 capability 不得隐式 attach/detach 任意线程。

## Callback-safe primitives

core 提供可由 plugin 自定义 CallbackSiteContainer 组合使用的最小并发原语：

- 原子 gate 和标量。
- 由 CallbackSiteContainer 进程期保活、callback 只借用的 `Frozen<T>`。
- 只保留最新值的 mailbox。
- latest-value mailbox 与 reply outbox。
- 仅在确有需要时提供只暴露 `try_lock` 的同步包装。

callback 侧操作必须有明确容量上限并且永不阻塞。v1 业务跨域 route 使用 latest-value mailbox：每条 route 只有一个 mailbox cell，新值覆盖旧值，中间状态允许丢失，写入不因“满载”失败。core 不引入 FIFO、竞争消费或广播语义。

进程级 Observability 仍可使用 `crossbeam_queue::ArrayQueue`；业务 callback route 在 v1 使用 latest-value mailbox，多个 producer 写入同一个 mailbox，只有一个实际 receiver 读取。callback-to-main 和 main-to-callback 的跨 callback 边界 message 必须是固定大小的 `Copy + Send + Sync + 'static` 类型，因此覆盖或读取时不执行任意析构。普通 AppWorld 内的 plugin message 不受此限制，仍可使用一般的 Bevy `Message`。core 不把 callback endpoint 的约束伪装成 Bevy `Message` 的默认无失败语义。

CallbackSiteContainer 不需要 core 提供通用资源 trait。core 只提供可放入明确容器的 callback-safe wrapper：

插件可以在自己的 CallbackSiteContainer 中组合这些 wrapper；容器本身是静态编译的受信任类型，具体字段和 callback handler 仍需单独审阅。`Frozen<T>` 是插件特定不可变数据的主要扩展点；它由静态 CallbackSiteContainer 持有到进程退出，callback 查询只借用其中的 `T`，不在热路径 clone/drop owning `Arc`。原子、mailbox、inbox 和经过审阅的 try-only wrapper 分别只暴露其受限操作。

第一版不提供运行时替换 `Frozen<T>`、可更新 snapshot reader 或旧值 deferred reclamation。callback 可见的结构化数据在 container 注册前构造完成，随后保持不变；运行期变化只使用原子、mailbox 或 bounded message 表达。未来只有在出现明确热更新需求后，才单独设计 callback-safe 的替换与回收协议。

`Send + Sync` 只保证容器可以跨执行域保活，不能证明插件随后对其中数据执行的算法有界、无分配或可重入。整个 callback handler 和容器字段仍需单独审阅；callback-safe 性能约束不是由类型 marker 自动证明的。

### RuntimeGate

core 提供一个不承载失败原因的进程级总 gate。runtime 持有唯一控制 handle，功能 callback 和 debug route 只持有只读 handle：

```rust
pub struct RuntimeGate {
    state: Arc<AtomicBool>,
}

pub struct RuntimeGateReader {
    state: Arc<AtomicBool>,
}
```

RuntimeGate 初始关闭。首次 Startup driver 完整结束且 App 仍可运行时，runtime 以 Release 语义最后开启它；所有 feature callback 在执行插件逻辑前以 Acquire 语义读取。任何 scheduler global failure 都必须先以 Release 语义关闭 RuntimeGate，再发布其它失败状态。

RuntimeGate 关闭后在当前进程内不得重新开启。它只表示整个 runtime 是否允许进入功能逻辑，不替代每个插件自己的 gate，也不编码启动中、失败原因或恢复结果等其它状态。

## MethodPointer 底层封装

MethodPointer slot 的原子替换与 original 的 typed 函数调用属于两个不同安全层。core 只提供不带函数 ABI 的底层封装，不设计能够调用任意 IL2CPP 方法的通用 Hook 引擎。

`MethodRef` 表示经过 IL2CPP 查询和 layout 校验的目标方法，至少包含方法身份、参数和返回类型、`MethodInfo` 地址以及 `methodPointer` slot 地址。上层不得自行计算 offset 或直接操作裸 `MethodInfo` 指针。

`MethodPointerSlot` 只持有经过校验的 slot 地址，负责：

1. 校验 slot 可读、自然对齐、可写，并读取非空当前 pointer。
2. 以 CAS 完成 `expected_original -> replacement`，再 readback 确认。
3. 仅在 slot 仍为 replacement 时以 CAS 恢复 original，再 readback 确认。
4. 发现其它 owner 或未知值时报告 ownership drift，不盲写。

`MethodPointerSlot` 不把任意地址转换成函数指针，不提供 `call_original`，也不知道参数、返回类型、gate 或 callback context。raw pointer 到 typed function pointer 的转换属于目标专用 Hook 的 unsafe 构造边界。

上层 typed Hook 只保留 `installed: AtomicBool`，不扩展为多状态生命周期枚举：

```text
installed = false
  → CAS 安装并 readback 确认 replacement
  → installed = true

installed = true
  → CAS 恢复并 readback 确认 original
  → installed = false
```

`installed` 只用于拒绝重复 install/restore 和决定是否尝试恢复，不声称 slot 当前仍由本 Hook 所有。实际 slot 始终是 ownership 的最终事实来源。

安装 CAS 成功后先保守设为 true。readback 未确认 replacement 时，上层立即尝试一次 ownership-aware 回滚；只有确认恢复 original 才重新设为 false。ownership drift 或无法确认时保持 true，不自动重试。恢复同样只有 CAS 与 readback 都确认 original 后才设为 false。

## Debug 与 Observability 集成

core 使用 `tracing` macros/fields 发出普通执行域的结构化事件，但不安装 subscriber 或选择 sink。runtime 创建并保活 scoped `tracing::Dispatch`，v1 输出到 Apple Unified Logging。core 另外定义固定大小、`Copy`、无任意 Drop 的 `CompactEvent` 及只暴露 `try_emit` 的 `CallbackObservability` producer；callback/scheduler 热路径通过进程级有界队列把记录交给 runtime-owned drain worker，再转换为正常 tracing event。

`CallbackObservability` 是基础设施 handle，不是 plugin message route：它不读取 RuntimeGate/PluginGate，不访问 App/World，不因插件退役停止记录，也不允许 callback 携带动态字符串或任意插件 payload。queue 满只增加 dropped counter。

Debug 方面，core 只承担 transport-neutral envelope、`serde` 可序列化边界和 callback-safe handles，不持有 plugin routes，也不决定 handler 的执行域。具体设计状态见 [Debug、Diagnostics 与 Logging](debug-diagnostics-logging.md)。

## core 不保证的事项

Rust 类型系统和 core wrapper 无法静态证明：

- 当前线程确实是 Unity 主线程。
- 当前游戏版本的 IL2CPP ABI 和 MethodPointer layout 正确。
- 外部代码没有缓存 replacement pointer。
- slot ownership 未被第三方改变。
- callback-safe 锁或外部 API 在语义上不会阻塞或重入。

这些事项仍需要 runtime 校验、实验版本边界和受审阅的 unsafe 实现。
