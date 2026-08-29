# Core crate 设计

状态：v2 设计（2026-08-29 修订）。本文定义概念 crate `scsp-core`：平台基础 API、线程/域能力、callback-safe 原语和底层安全封装。它不了解 App、PluginManager、具体功能插件或 runtime bootstrap 流程。

## 依赖边界

`scsp-core` 位于依赖图底部，被 `scsp-plugin-api` 与 `scsp-runtime` 依赖。实现优先组合已审阅的 crate，而不是重新实现通用容器或 facade：

- `thiserror` 派生公开 typed error。
- `il2cpp-bridge-rs`（0.1.4 固定）提供实验验证过的 IL2CPP API table 与 metadata 查询基础。
- `crossbeam-queue` 提供 bounded queue primitive（Bounded mailbox 与 Observability 队列共用此基础件）。
- `tracing` 作为普通执行域统一的结构化事件 facade。

core 不得：

- 持有 App 或 PluginManager。
- 注册功能插件或决定插件顺序。
- 创建 Startup/Update system 列表或 driver。
- 实现 `scsp_start`、Handoff 或 LateUpdate TLS 调度。
- 了解 FPS、翻译、相机等具体功能。

## 基础类型与线程 capability

core 提供不包含业务状态的基础类型：DataRoot、稳定 ID、错误分类、进程期 backend handle 和 `MainThreadToken`。

`MainThreadToken` 是不可跨线程的 capability。它只能由 runtime 在当前 callback frame 完成线程身份校验后通过受审阅的 unsafe 构造边界创建；需要 Unity 主线程的安全 API 必须显式要求该 token，不能只依赖调用者约定。

```rust
pub struct MainThreadToken {
    _not_send_sync: PhantomData<Rc<()>>,
    _private: (),
}
```

token 类型本身不携带 lifetime，以便作为 phase context（`StartupCtx<'_>` / `UpdateCtx<'_>`）的字段进入 system 输入；runtime 与 driver 只向 system 传递当前栈上 token 的短借用，不把所有权交给插件。token 不实现 `Clone`/`Copy`，构造函数不公开，因此安全插件代码不能把当前证明保存为 `'static` resource。输入引用的 lifetime 就是 system 本次调用的 capability 边界。

App 和普通 Resource 可以继续满足 `Send`；这只允许转移所有权，不授权在任意线程执行 Unity 操作。

v1 的平台判据固定为当前线程调用 `pthread_main_np() != 0`。runtime 在每次最外层 scheduler callback 中重新验证，成功后只为该 callback frame 构造 token；Swift main queue、bootstrap worker 身份和"曾经在正确线程运行过"都不能制造或缓存 token。该判据必须先由目标环境实验确认 LateUpdate 确实位于 process main thread；证据不成立时实现保持 NO-GO，而不是改为信任首次 callback。

## 进程级 gate

core 提供一个不承载失败原因的进程级总 gate。runtime 持有唯一控制 handle，功能 callback、debug route 与 I/O worker 只持有只读 handle：

```rust
#[derive(Clone)]
pub struct GateReader(Arc<AtomicBool>);

pub struct RuntimeGate(GateReader);
```

- `GateReader::is_open` 以 Acquire 语义读取；runtime 关闭以 Release 语义写入。
- RuntimeGate 初始关闭。首次 Startup driver 完整结束且 App 仍可运行时，runtime 最后开启它。
- RuntimeGate 关闭后在当前进程内不得重新开启。它只表示整个 runtime 是否允许进入功能逻辑，不替代每个插件自己的 gate，也不编码启动中、失败原因或恢复结果等其它状态。
- per-plugin `PluginGate` 复用同一 `GateReader` 类型；控制端归 plugin-system，语义见 plugin-system 分册。
- 门与内存序的权威定义只有本节；其它分册引用，不重复展开。

## MethodPointer 底层封装

MethodPointer slot 的原子替换与 original 的 typed 函数调用属于两个不同安全层。core 只提供不带函数 ABI 的底层封装，不设计能够调用任意 IL2CPP 方法的通用 Hook 引擎。

`MethodRef` 表示经过 IL2CPP 查询和 layout 校验的目标方法，至少包含方法身份（assembly/namespace/class/name/param count/返回类型）、`MethodInfo` 地址以及 `methodPointer` slot 地址。上层不得自行计算 offset 或直接操作裸 `MethodInfo` 指针。

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

## IL2CPP backend 与 attach 生命周期

`Il2CppBackend` 负责保活 exact UnityFramework handle、通过固定版本 `il2cpp-bridge-rs` 加载的 IL2CPP API 表以及已经确认的 runtime/layout 身份。PlayCover 环境不能假定 `RTLD_DEFAULT` 可见性；runtime 必须把精确 UnityFramework handle 交给 `il2cpp-bridge-rs::api::load` 所需的 exact-handle resolver，不重新实现第二套导出表或高层初始化器。

```rust
pub struct Il2CppRuntime(Arc<Il2CppBackend>);
pub struct CallbackIl2Cpp(Arc<Il2CppBackend>);
```

`Il2CppRuntime` 是正常解析和调用入口；需要主线程的安全方法还必须接受 `&MainThreadToken`。`CallbackIl2Cpp` 只暴露对应 callback 已审阅的有限操作，不提供任意导出调用、任意地址访问或隐式线程附着。callback 只有在插件明确导入该 capability 时才能取得它。

`Il2CppBackend` 是否能够安全实现 `Send + Sync` 必须由具体字段、动态库 handle 生命周期和每个公开方法的线程约束证明，不能仅为满足 Resource bound 添加无依据的 `unsafe impl`。

bootstrap worker 使用 IL2CPP API 前必须遵守显式 attach 生命周期：等待 domain 非空（单次调用，见 runtime-crate 分册 readiness 阶梯），检查当前 worker 是否已附着，只在未附着时调用 thread attach，并用 RAII guard 仅 detach 本次自己建立的 attachment。callback 侧 capability 不得隐式 attach/detach 任意线程。

## Callback-safe 原语

core 提供可由插件 CallbackSiteContainer 与跨域 route 组合使用的最小并发原语。callback 侧操作必须有界且永不阻塞；callback 热路径不得分配、不得调用插件任意代码。

```rust
// 语义在注册时按类型选择，三种 mailbox：
pub struct LatestCell<T>(/* 单格，新值覆盖旧值 */);            // T: CallbackPayload
pub struct BoundedQueue<T, const N: usize>(/* ArrayQueue */); // T: CallbackPayload
pub struct SharedSlot<T>(/* 单槽 Arc<T>，新值替换旧值 */);      // T: Send + Sync + 'static

pub trait CallbackPayload: Copy + Send + Sync + 'static {}
```

- `LatestCell`：latest-value 语义。写入返回 `Sent::{Accepted, Replaced}`，不返回 Full、不阻塞；中间状态允许丢失。适合"当前状态"类数据（FPS 目标值、相机参数）。
- `BoundedQueue`：保序 FIFO。`try_send` 满载返回 `Full` 并由调用侧累计计数，不阻塞。适合"事件流"类数据（每次纹理加载、每次回调命中）。
- `SharedSlot`：承载有主结构化数据（`Arc<T>`），为 debug 域 request/response 等非 `Copy` payload 设计。`try_send(Arc<T>)` 替换槽中旧值——旧 `Arc` 引用计数归零才析构 `T`；callback 侧 `try_read` 克隆 `Arc`（只增引用计数，不分配）。约束：`T` 必须是无副作用 `Drop` 的普通数据（serde 派生结构），替换操作只允许发生有界的析构工作。
- 仅在确有需要时提供只暴露 `try_lock` 的同步包装。

覆盖或读取 `LatestCell`/`BoundedQueue` 不执行任意 payload 析构（`Copy`）；`SharedSlot` 的替换可能析构旧值，因此以"普通数据类型"约束代替 `Copy`。core 不把 mailbox 约束伪装成 Bevy `Message` 的默认语义；普通 AppWorld 内的 plugin message 仍可使用一般 Bevy `Message`（`Send + Sync + 'static`，无 Copy 限制）。

`Send + Sync` 只保证容器可以跨执行域保活，不能证明插件随后对其中数据执行的算法有界、无分配或可重入。整个 callback handler 和容器字段仍需单独审阅；callback-safe 性能约束不是由类型 marker 自动证明的。

## Observability 集成

core 使用 `tracing` macros/fields 发出普通执行域的结构化事件，但不安装 subscriber 或选择 sink。runtime 创建并保活 scoped `tracing::Dispatch`，v1 输出到 Apple Unified Logging。core 另外定义固定大小、`Copy`、无任意 Drop 的 `CompactEvent` 及只暴露 `try_emit` 的 `CallbackObservability` producer；callback/scheduler 热路径通过进程级有界队列把记录交给 runtime-owned drain worker，再转换为正常 tracing event。

```rust
#[derive(Clone, Copy)]
pub struct CompactEvent {
    code: CompactEventCode,
    level: CompactLevel,
    owner: CompactOwnerId,
    site: CompactSiteId,
    arg0: u64,
    arg1: u64,
}
```

`CallbackObservability` 是基础设施 handle，不是 plugin message route：它不读取 RuntimeGate/PluginGate，不访问 App/World，不因插件退役停止记录，也不允许 callback 携带动态字符串或任意插件 payload。queue 满只增加 dropped counter。事件代码、字段与 drain worker 的完整边界见 [Debug、Diagnostics 与 Logging](debug-diagnostics-logging.md)。

## core 不保证的事项

Rust 类型系统和 core wrapper 无法静态证明：

- 当前线程确实是 Unity 主线程。
- 当前游戏版本的 IL2CPP ABI 和 MethodPointer layout 正确。
- 外部代码没有缓存 replacement pointer。
- slot ownership 未被第三方改变。
- callback-safe 锁或外部 API 在语义上不会阻塞或重入。

这些事项仍需要 runtime 校验、实验版本边界和受审阅的 unsafe 实现。
