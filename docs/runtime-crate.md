# Runtime crate 设计

状态：草案

本文定义概念 crate `scsp-runtime`：生产 staticlib 的实际 FFI 入口、bootstrap worker、App 组装、Unity 主线程 scheduler、Handoff、TLS 和 runtime 级失败边界。

## 依赖与职责

```text
scsp-core
  ↑
scsp-plugin-api
  ↑
scsp-plugin-system
  ↑
scsp-runtime
```

runtime 负责：

- 导出 `scsp_start` 并保证入口非阻塞、幂等。
- 定位已加载 UnityFramework，并通过 exact handle 初始化 core IL2CPP backend。
- 构造 App、注册生产插件并驱动 worker build。
- 安装 LateUpdate SchedulerHook。
- 将 App 一次性交接到 Unity 主线程 TLS。
- 保证 scheduler callback 中 original LateUpdate 恰好调用一次。
- 把 scheduler 核心失败升级为 App 退出路径。

runtime 不定义具体功能插件的数据格式，也不实现 PluginManager、AppWorld、通用 resource API 或 Debug wire backend。

## 证据边界

实验仓库已经证明当前精确游戏版本上 MethodPointer replacement、callback 进入、original 调用、恢复以及 exact UnityFramework handle 的低层 IL2CPP API 加载路径可行。这些结果只约束生产设计，不表示本 crate 已经实现或验证。

PlayCover 环境中的 UnityFramework 可能以 `RTLD_LOCAL` 加载。runtime 不使用 `RTLD_DEFAULT` 或假定 high-level bridge 初始化可以发现 IL2CPP symbols；它必须取得精确 image/handle，再交给 core loader。UnityFramework 定位、身份和版本策略的具体 API 仍待实现设计。

## 生命周期对象

- `BootstrapContext`：worker 临时对象，负责 DataRoot、启动状态和启动期资源；交接或已安全处理的启动失败后销毁。
- `App`：由 plugin-system 提供的唯一 `Send` 组合根；worker 完成 build 后通过 Handoff 转移给主线程 TLS。
- `Handoff`：`Mutex<Option<Box<App>>>` 的一次性跨线程所有权槽。
- `SchedulerContext`：进程期稳定的 LateUpdate callback context，持有 Handoff、SchedulerHook 和 global failure flag。Diagnostics/Logging 是否需要额外字段尚未设计。

不设置持有插件业务状态的常驻 RuntimeKernel。入口去重只需要进程级一次性启动标记。

## `scsp_start` 与 bootstrap

```text
AKPlugin.init()
  → DispatchQueue.main.async
  → scsp_start(documents_path)
  → 复制并校验路径
  → 启动唯一 bootstrap worker
  → 立即返回

bootstrap worker
  → 定位 exact UnityFramework handle
  → 构造 core IL2CPP backend 和基础设施 handles
  → App::new
  → 按固定顺序 App::add_plugin
      → Plugin.build
      → 插入 resources、注册 systems
      → 准备 CallbackWorld
      → 安装 gate 关闭的功能 Hook
  → 解析并校验 scheduler LateUpdate
  → 捕获 typed original，构造 SchedulerHook
  → 以空 Handoff 构造 SchedulerContext
  → SCHEDULER.set(context)
  → 安装 SchedulerHook
  → Handoff.publish(App)
  → worker 退出
```

`DispatchQueue.main.async` 只让 `scsp_start` 离开 `AKPlugin.init()` 当前调用栈；它不创建 Unity 主线程，也不代表 IL2CPP ready。LateUpdate replacement 才是运行时交接点。

## Handoff 同步

```rust
struct Handoff {
    app: Mutex<Option<Box<App>>>,
}

enum HandoffTake {
    Ready(Box<App>),
    Pending,
    Failed,
}
```

worker 的 `publish` 正常加锁并把 App 从 `None` 改为 `Some`。只允许发布一次；重复发布不得覆盖已有 App。mutex unlock 与 callback 后续 lock 建立 App 构造结果的跨线程可见性。

callback 只调用 `try_take`：

- 成功加锁且槽中有 App：`Ready(App)`。
- 成功加锁但槽为空：`Pending`。
- worker 暂时持锁：`Pending`，不得等待。
- mutex poisoned 或其它不变量损坏：`Failed`。

`Pending` 时当前 callback 只调用 original，下一次 callback 重试。`Failed` 时当前线程 TLS 进入 `Unavailable`，之后只调用 original。持有 Handoff guard 时不得执行插件逻辑、日志 I/O、original 或其它外部调用；临界区只移动一个 `Box<App>`。

## SchedulerContext 发布顺序

```rust
static SCHEDULER: OnceLock<SchedulerContext> = OnceLock::new();

struct SchedulerContext {
    handoff: Handoff,
    hook: SchedulerHook,
    failed: AtomicBool,
}
```

Debug、Diagnostics 和 Logging 的 runtime 集成统一见 [Debug、Diagnostics 与 Logging](debug-diagnostics-logging.md)。Diagnostics 和 Logging 当前未设计，因此这里不预先放入占位 handle。

original LateUpdate pointer 只由 SchedulerHook 持有，SchedulerContext 不保存第二份。SchedulerHook 在发布前已包含 slot、original 和 replacement；callback 始终通过 `SchedulerHook::call_original` 使用安装前捕获的 typed original，不重新读取 slot 寻找 original。

固定激活顺序是：

```text
捕获 original
  → 构造 SchedulerHook
  → 构造空 Handoff 和 SchedulerContext
  → SCHEDULER.set
  → CAS 安装 SchedulerHook
  → Handoff.publish(App)
```

包含 original 的完整 context 必须先发布，replacement 才能可达。因此 callback 可达但 `SCHEDULER.get() == None` 是构造不变量破坏，不是正常启动分支。

SchedulerHook 安装完成但 App 尚未 publish 的短窗口内，callback 可以看到空 Handoff；它只调用已经发布的 original，后续 callback 再尝试领取 App。

## 主线程 TLS

```rust
enum AppSlot {
    AwaitingHandoff,
    Running(Box<App>),
    Busy,
    Exited(Box<App>),
    Unavailable,
}

thread_local! {
    static APP_SLOT: RefCell<AppSlot> =
        const { RefCell::new(AppSlot::AwaitingHandoff) };
}
```

- `AwaitingHandoff`：当前线程尚未领取 App。
- `Running(App)`：App 由当前游戏主线程 TLS 独占。
- `Busy`：外层 callback 已把 App 移到自己的栈上，正在运行 App 或 original。
- `Exited(App)`：逻辑终态，保留 App 和仍可能被 callback 使用的资源。
- `Unavailable`：该线程从未取得 App，且 Handoff 或 scheduler 已永久失败。

外层 callback 以很短的 RefCell 借用把槽替换为 Busy，取得 App 后立即释放借用。Busy 必须覆盖 App schedule 和随后 original 调用；original 返回后才把 App 放回 Running 或 Exited。

嵌套 callback 看到 Busy 时只调用 original，不再次运行 App，也不争用 RefCell。普通 Hook callback 不访问这个 TLS。

## 固定 callback 顺序

```text
取得 SchedulerContext
  → 检查 global failure 和当前线程身份
  → 根据 TLS 领取 App并设为 Busy
  → 释放 RefCell 借用
  → 首次调用 App.run_startup；后续调用 App.run_update
  → 在 TLS 仍为 Busy 时调用 original LateUpdate
  → original 返回后把 App 放回 Running 或 Exited
```

首次 callback 只运行 Startup，不在同一 callback 中继续运行 Update：

```text
首次 LateUpdate
  → Handoff.take
  → Busy
  → App.run_startup
  → original
  → Running(App)

后续 LateUpdate
  → Running(App) -> Busy
  → App.run_update
      → DebugDispatch
      → CommandDrain
      → plugin Update systems
  → original
  → Running(App)
```

Debug main request 因而在下一次外层 LateUpdate 执行，正常帧循环下近似即时。callback-domain debug request 由对应功能 Hook 自然进入时处理，不由 scheduler 或 DebugHub 主动唤起；完整边界见 [Debug、Diagnostics 与 Logging](debug-diagnostics-logging.md)。

## 线程身份与 global failure

每次 scheduler callback 都验证当前线程身份。任何一次不匹配都属于 scheduler 核心故障：当前 callback 只调用 original，并以 Release 语义设置 `failed = true`；之后所有线程只保留 original passthrough。

`failed` 的生产者限定为：

- replacement callback 发现当前线程不是主线程。
- SchedulerContext 发布后，SchedulerHook 安装、验证或必要回滚发生无法确认的错误。
- SchedulerHook 已安装后，Handoff publish 永久失败。

callback 在执行 App 前以 Acquire 语义检查 `failed`：

- App 尚在 Handoff：TLS 进入 Unavailable，不领取 App；Handoff 继续保活 App。
- App 已在 TLS：停止 Startup/Update，original 返回后进入 Exited(App)。
- App 正由外层 callback 持有：original 返回后重新检查并进入 Exited(App)。
- 其它线程和之后所有 callback：只调用 original。

global failure 在当前进程不可恢复，不自动重试启动。第一版不要求 SchedulerHook 因此已成功卸载。

单个插件 Startup/Update 失败不是 scheduler failure；plugin-system 只退役和回滚所属插件，让其它插件与 App 继续运行。

## Panic 边界

生产 runtime release profile 必须使用：

```toml
[profile.release]
panic = "unwind"
```

replacement callback 用 `catch_unwind` 包围 Rust 自己的 App schedule 和调度逻辑。必要时可以在经过审阅的 scheduler 边界使用 `AssertUnwindSafe`，把失败转换成 plugin/App 状态；这不表示内部对象天然 unwind-safe。

original LateUpdate 不属于可恢复 Rust 逻辑。callback 必须先把 Rust panic 转换成明确结果，再在 unwind 边界外调用安装时捕获的 original 恰好一次，最后恢复 TLS 状态。Rust panic 不得越过 replacement 的 `extern "C"` 边界。

该边界不捕获 Objective-C exception、C++ exception、Swift trap、`SIGABRT`、`SIGTRAP`、`SIGSEGV`、进程终止或 original 内部崩溃，也不能修复错误 ABI、无效指针或内存破坏。

## Scheduler 目标专用 ABI

第一版只接受实验验证过的精确目标：`UniRx.dll / UniRx / MainThreadDispatcher / LateUpdate / 0`。构造 SchedulerHook 前必须确认实例方法、显式参数为零、返回 `System.Void`、MethodInfo 非空，并匹配受支持 runtime/layout 身份。

```rust
#[repr(C)]
struct Il2CppObjectOpaque {
    _private: [u8; 0],
}

#[repr(C)]
struct MethodInfoOpaque {
    _private: [u8; 0],
}

type LateUpdateFn = unsafe extern "C" fn(
    this: *mut Il2CppObjectOpaque,
    method: *const MethodInfoOpaque,
);

struct SchedulerHook {
    slot: MethodPointerSlot,
    original: LateUpdateFn,
    replacement: LateUpdateFn,
    installed: AtomicBool,
}
```

raw pointer 到 LateUpdateFn 的转换只能发生在目标身份、runtime 身份和 layout 校验完成后的 SchedulerHook 构造边界。replacement 必须使用相同类型，并把 `this` 和 `method` 原样传给 original。

metadata 参数和返回类型只用于拒绝目标漂移，不能自动推导 Rust ABI。隐式 MethodInfo 参数和调用约定是当前验证目标的版本绑定事实，不外推为其它 IL2CPP 方法的通用规则。

SchedulerHook 的 installed、CAS/readback、ownership drift 和恢复语义遵循 [Core crate 设计](core-crate.md#methodpointer-底层封装)。安装失败使整个 bootstrap 失败；功能 Hook 安装失败只退役所属插件。

## 启动失败与保活

SchedulerContext 发布前失败时 replacement 不可达，worker 回滚已经安装的功能 effects。

SchedulerContext 发布后，SchedulerHook CAS/验证/回滚无法确认时，worker 先设置 global failed，使任何可能到达的 callback 只调用 original。App 不进入正常 Handoff，worker 回滚功能 Hook并结束 bootstrap。

只有全部功能 effect 都确认恢复、所有 callback context 都确认不可达时才允许丢弃 App。存在 ownership drift 或无法确认的恢复结果时，必须按 plugin-system 的待设计失败所有权协议继续保活相关资源。

## 退出与卸载

第一版不支持运行时释放 scheduler。SchedulerContext 和 SchedulerHook 由 OnceLock 保活到进程退出；App 退出只把 TLS 变为 Exited(App)，之后 callback 只调用 original。

未来真正卸载 scheduler 必须另行设计：

```text
阻止新的 App 逻辑
  → ownership-aware 恢复 LateUpdate slot
  → 等待在途 scheduler callback 完成
  → 处理缓存 replacement pointer
  → 释放 Hook、Context 和 App
```

届时 OnceLock 需要替换为可清空且具有 quiescence 协议的共享槽；当前不展开。

## Runtime 待设计项

- `scsp_start` 错误报告和重复入口的具体 API。
- exact UnityFramework image/handle 定位与版本身份表示。
- bootstrap 期间基础设施的构造和失败顺序。
- scheduler 主线程身份 token 的具体来源。
- Debug transport 的启用配置和 backend 生命周期，见独立 Debug 设计。
- Diagnostics 和 Logging 的整体设计，包括 scheduler/global failure 集成。
- App 退出后的物理 unload 与进程结束行为。
