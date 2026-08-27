# Core crate 设计

状态：草案；共享设施的具体 API 待打磨

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

core 可以定义通用错误、typed handles、callback-safe primitives 和 platform-specific unsafe boundary，但不得：

- 持有 App 或 PluginManager。
- 注册功能插件或决定插件顺序。
- 创建 Startup/Update schedule。
- 实现 `scsp_start`、Handoff 或 LateUpdate TLS 调度。
- 了解 Translation、Camera、FPS 等具体功能。

## 基础类型与线程 capability

core 提供不包含业务状态的基础类型，例如 DataRoot、稳定 ID、错误分类、进程期 backend handle 和 `MainThreadToken`。

`MainThreadToken` 是短生命周期、不可跨线程的 capability。它只能由 runtime 在完成当前线程身份校验后通过受审阅的 unsafe 构造边界创建；需要 Unity 主线程的安全 API 必须显式要求该 token，不能只依赖调用者约定。

```rust
pub struct MainThreadToken<'frame> {
    _frame: PhantomData<&'frame mut ()>,
    _not_send_sync: PhantomData<Rc<()>>,
}
```

App 和普通 Resource 可以继续满足 `Send`；这只允许转移所有权，不授权在任意线程执行 Unity 操作。

## 共用设施 backend 与窄 handle

跨执行域共享的基础件可以拥有进程期 backend，并针对不同执行环境提供 API 不同的 cloneable handle：

```text
Arc<Backend>
  ├─ bootstrap/runtime handle
  ├─ App/system handle
  └─ callback-safe handle
```

不得用一个包含全部权限的 `SharedInfra` handle 代替这些角色。共享同一个 `Arc` 只解决所有权和生命周期，不表示 backend 的全部操作都可跨线程调用。

Debug、Diagnostics 和 Logging 的状态与跨 crate 边界单独记录在 [Debug、Diagnostics 与 Logging](debug-diagnostics-logging.md)。Diagnostics 和 Logging 当前未设计，core 不预先定义具体 handle 或 backend。

### IL2CPP

`Il2CppBackend` 负责保活 exact UnityFramework handle、已加载的 IL2CPP API 表以及已经确认的 runtime/layout 身份。PlayCover 环境不能假定 `RTLD_DEFAULT` 可见性；runtime 必须把精确 UnityFramework handle 交给 core 的低层 loader。

```rust
pub struct Il2CppRuntime(Arc<Il2CppBackend>);
pub struct CallbackIl2Cpp(Arc<Il2CppBackend>);
```

`Il2CppRuntime` 是正常解析和调用入口；需要主线程的安全方法还必须接受 `&MainThreadToken`。`CallbackIl2Cpp` 只暴露对应 callback 已审阅的有限操作，不提供任意导出调用、任意地址访问或隐式线程附着。callback 只有在插件明确导入该 capability 时才能取得它。

`Il2CppBackend` 是否能够安全实现 `Send + Sync` 必须由具体字段、动态库 handle 生命周期和每个公开方法的线程约束证明，不能仅为满足 Resource bound 添加无依据的 `unsafe impl`。

## Callback-safe primitives

core 提供可由 CallbackWorld resource 组合使用的最小并发原语：

- 原子 gate 和标量。
- 不可变 `Arc<T>` snapshot。
- runtime 发布、callback 读取的 snapshot cell。
- 只保留最新值的 mailbox。
- 有界 command/event inbox 与 reply outbox。
- 仅在确有需要时提供只暴露 `try_lock` 的同步包装。

callback 侧操作必须有明确容量上限并且永不阻塞。队列满载策略由上层语义选择丢弃、覆盖最新值或合并；core 不把一种策略暗中应用到所有消息。

`Send + Sync` 只能证明 Rust 数据竞争安全，不能证明无阻塞、可重入或适合 Hook 热路径。哪些类型可以实现 `CallbackResource`、是否使用 sealed trait，以及 callback-safe 审阅契约均待打磨。

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

## Debug、Diagnostics 与 Logging 集成

core 未来只承担独立设计分配给它的 transport-neutral primitives 和 callback-safe handles，不持有 plugin routes，也不决定 handler 的执行域。具体设计状态见 [Debug、Diagnostics 与 Logging](debug-diagnostics-logging.md)。

## core 不保证的事项

Rust 类型系统和 core wrapper 无法静态证明：

- 当前线程确实是 Unity 主线程。
- 当前游戏版本的 IL2CPP ABI 和 MethodPointer layout 正确。
- 外部代码没有缓存 replacement pointer。
- slot ownership 未被第三方改变。
- callback-safe 锁或外部 API 在语义上不会阻塞或重入。

这些事项仍需要 runtime 校验、实验版本边界和受审阅的 unsafe 实现。
