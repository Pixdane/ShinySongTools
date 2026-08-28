# Runtime crate 设计

状态：v1 设计已收敛；具体 Rust 类型和物理模块可在实现时调整

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

runtime 不定义具体功能插件的数据格式，也不实现 PluginManager、通用 resource API 或 Debug wire backend。

## 证据边界

实验仓库已经证明当前精确游戏版本上 MethodPointer replacement、callback 进入、original 调用、恢复以及 exact UnityFramework handle 的低层 IL2CPP API 加载路径可行。这些结果只约束生产设计，不表示本 crate 已经实现或验证。

PlayCover 环境中的 UnityFramework 可能以 `RTLD_LOCAL` 加载。runtime 不使用 `RTLD_DEFAULT` 或假定 high-level bridge 初始化可以发现 IL2CPP symbols；它必须通过平台 image 枚举取得精确 UnityFramework image/handle，再交给 core 的 exact-handle loader。路径匹配、唯一性和版本身份的具体表示仍待实现打磨。

## 生命周期对象

- `ObservabilityRoot`：在 App 之前创建并保活到进程退出的 `tracing::Dispatch`、Apple Unified Logging layer、compact event queue 和 drain worker retention root。
- `BootstrapContext`：worker 临时对象，负责 DataRoot、启动状态和启动期资源；交接或已安全处理的启动失败后销毁。
- `App`：由 plugin-system 提供的唯一 `Send` 组合根；worker 完成 build 后通过 Handoff 转移给主线程 TLS。
- `Handoff`：`Mutex<Option<Box<App>>>` 的一次性跨线程所有权槽。
- `SchedulerContext`：进程期稳定的 LateUpdate callback context，持有 Handoff、SchedulerHook、RuntimeGate 控制 handle 和 global failure flag。

不设置持有插件业务状态的常驻 RuntimeKernel。入口去重只需要进程级一次性启动标记；不为入口重复、bootstrap 失败或 runtime global failure 增加 supervisor 状态机。

## `scsp_start` 与 bootstrap

```text
AKPlugin.init()
  → DispatchQueue.main.async
  → scsp_start(documents_path)
  → 在最外层 unwind guard 内初始化或取得进程期 ObservabilityRoot
  → 启动独立于 App/RuntimeGate 的 compact event drain worker
  → 在 scoped tracing Dispatch 中记录 start entered
  → 尝试领取进程期一次性启动标记
  → 若已领取：记录 start duplicate 并立即返回
  → 复制并校验路径
  → 若路径无效：记录 start rejected 并结束本次启动，不重试
  → 从现有 runtime 配置读取 `debug.enabled`（默认 `false`）
  → 启动唯一 bootstrap worker
  → 立即返回

bootstrap worker
  → 在 monotonic deadline 内等待 exact UnityFramework image
  → 从该 image 取得并保活 exact handle
  → 仅通过 exact handle 加载 IL2CPP API table
  → 等待 il2cpp_domain_get() 返回非空 domain
  → worker attach 到该 domain
  → 等待 scheduler 所需 assembly/image/metadata 可解析
  → 校验 runtime/layout 身份
  → 构造 core IL2CPP backend 和基础设施 handles
  → 构造初始关闭的 RuntimeGate
  → App::new，并向 plugin-system 提供 RuntimeGateReader
  → 按固定顺序 App::add_plugin
      → Plugin.build
      → 使用共享 AppWorld 插入 resources、转换并初始化 boxed systems
      → 注册 plugin CallbackSiteContainer
      → 发布目标专用静态 CallbackSite
      → 安装 gate 关闭的功能 Hook
  → AppCore::DebugState 按 `debug.enabled` 初始化（仅在 App 与 plugin build 处理完成且 App 可继续运行后；启用时从入口 Documents 路径解析 socket、删除残留并启动 Debug I/O worker）
  → 解析并校验 scheduler LateUpdate
  → 捕获 typed original，构造 SchedulerHook
  → 以空 Handoff 构造 SchedulerContext
  → SCHEDULER.set(context)
  → 安装 SchedulerHook
  → Handoff.publish(App)
  → worker detach 本次建立的 IL2CPP attachment
  → worker 退出
```

`DispatchQueue.main.async` 只让 `scsp_start` 离开 `AKPlugin.init()` 当前调用栈；它不创建 Unity 主线程，也不代表 IL2CPP ready。LateUpdate replacement 才是运行时交接点。

runtime 不调用进程全局 `tracing::subscriber::set_global_default`。`scsp_start` body、bootstrap worker、scheduler execution、plugin system 调用、Observability drain worker 和由 AppCore::DebugState 启动的 Debug I/O worker 都由 runtime 用同一个 `tracing::Dispatch` 建立 scoped default。v1 只配置 Apple Unified Logging layer，固定 subsystem 为 `com.shinysongtools.runtime`；初始化不依赖 DataRoot，也没有 file layer。Debug 未启用时不创建 Debug I/O worker 或 Unix domain socket；启用时 socket 路径只能从入口复制得到的容器 `Documents` 路径派生，并由 AppCore::DebugState 在创建前直接清理同名残留文件。worker 或 socket 启动失败时 DebugState 进入 `Unavailable`，记录 observability，不重试，且不影响 runtime 和游戏；App 退出时停止 Debug worker，worker 停止时将未完成 pending request 回复为 `runtime_unavailable`，然后关闭 transport 并尝试删除 `debug.sock`，删除失败只记录 observability。完整边界见 [Debug、Diagnostics 与 Logging](debug-diagnostics-logging.md)。

一次性入口标记在复制参数前领取，因此首个调用即使参数无效也不会触发第二次 bootstrap。重复入口不覆盖首个调用的路径、不创建第二个 worker，也不尝试修复或重启已经失败的 bootstrap。ObservabilityRoot 使用自己的进程期 `OnceLock`，重复入口只复用它记录事件。

bootstrap 只在 `Handoff.publish(App)` 成功后才算启动完成。此前任一步失败都在 RuntimeGate 已创建时将其关闭，按 owner 逆序尝试恢复已经安装的功能 effects，停止 Debug worker（如果已启动），并丢弃未交接的 App；已发布的 CallbackSite、typed original、gate reader 和 callback backend 继续由静态 site 保活。plugin 自身的 Build/Startup 失败仍只走 owner-local 退役，不阻止其它 plugin 继续 build。

## UnityFramework 与 IL2CPP readiness

readiness 是任何 MethodPointer 写入之前的独立 bootstrap 阶段，不由 Swift main queue 或固定 sleep 代替。worker 使用单调时钟的总 deadline 和有上限的 backoff 依次等待：

1. 平台 image 列表中出现唯一且身份匹配的 UnityFramework。
2. exact handle 上所需 IL2CPP exports 全部可加载。
3. `il2cpp_domain_get()` 返回非空 domain。
4. worker 成功 attach，scheduler 目标所需 assembly、image、class 和 metadata 已可查询。
5. 当前 runtime/layout 身份属于生产实现明确支持的集合。

前一步未满足时不得提前调用依赖后一步的 API。超时、image 多义、export 缺失、attach 失败或身份不支持都结束本次一次性 bootstrap；在这个阶段尚未发布 CallbackSite 或修改 MethodPointer，因此不需要 Hook rollback。具体总超时、backoff 序列、image identity 格式和 assembly-ready probe 属于实现参数，但必须有界且可测试。

worker 先检查自身是否已附着，只对本次调用建立的 attachment 创建 RAII detach guard。所有 build/metadata 查询结束后，无论成功、普通 error 或 Rust unwind 都由该 guard detach；不得 detach 外部已经建立的 attachment。exact UnityFramework handle 和加载完成的 API table 则转移进 Il2CppBackend 并按 backend 生命周期保活。

readiness 只证明可以安全开始 build，不保证每个可选插件目标都存在。scheduler 目标缺失使整个 bootstrap 失败；某个功能插件目标缺失仍按 owner-scoped build failure 只退役该插件。

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
    runtime_gate: RuntimeGate,
    failed: AtomicBool,
}
```

ObservabilityRoot 独立于 SchedulerContext 进程期保活；SchedulerContext 不保存第二份 subscriber/backend handle。scheduler 热路径需要记录事件时只使用窄 `CallbackObservability` producer 向进程级 compact event queue 提交，之后由专用 drain worker 转成 Apple Unified Logging event。Debug 与 Observability 的完整集成见 [Debug、Diagnostics 与 Logging](debug-diagnostics-logging.md)。

original LateUpdate pointer 只由 SchedulerHook 持有，SchedulerContext 不保存第二份。SchedulerHook 在发布前已包含 slot、original 和 replacement；callback 始终通过 `SchedulerHook::call_original` 使用安装前捕获的 typed original，不重新读取 slot 寻找 original。

固定激活顺序是：

```text
捕获 original
  → 构造 SchedulerHook
  → 用仍然关闭的 RuntimeGate 构造空 Handoff 和 SchedulerContext
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

外层 callback 在取得静态 SchedulerContext 后，先用不分配、不借用 TLS 的字段初始化建立栈上 `SchedulerFrame`，再进入 catch scope 领取 App。frame 是单次 callback 的执行与所有权 guard，不放入 App、TLS 或任何持久 lifecycle enum；TLS 中的 Busy 只表示 App 当前由这个 frame 独占。

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
  → App.run_startup（RuntimeGate 保持关闭）
  → App 仍可运行时，最后开启 RuntimeGate
  → original
  → Running(App)

后续 LateUpdate
  → Running(App) -> Busy
  → App.run_update
      → MessageMaintenance
      → DebugDispatch
      → CommandDrain（阶段入口捕获有界 watermark）
      → 按 plugin 注册顺序运行 Update systems
  → original
  → Running(App)
```

runtime 不在这里重新排序 plugin systems；它只调用 plugin-system 已冻结的有序 driver。MessageMaintenance、DebugDispatch、CommandDrain 和 plugin Update 是 v1 固定阶段，不接受运行期插入其它阶段或 before/after 关系。CommandDrain watermark 之后到达的 callback-to-main message 留到下一次外层 LateUpdate。

Debug main request 因而在下一次外层 LateUpdate 执行，正常帧循环下近似即时。callback-domain debug request 由对应功能 Hook 自然进入时处理，不由 scheduler 或 DebugHub 主动唤起；完整边界见 [Debug、Diagnostics 与 Logging](debug-diagnostics-logging.md)。

## 线程身份与 global failure

v1 每次最外层 scheduler callback 都以 `pthread_main_np() != 0` 验证当前线程是 process main thread；不使用 Swift main queue 捕获的线程 ID，也不把首次 callback 自动视为可信。验证成功后 runtime 才为当前 SchedulerFrame 构造不可 Send/Sync、不可 Clone/Copy 的 `MainThreadToken`，并只把短借用作为 Bevy system input 传入；token 所有权不进入 AppWorld 或下一帧。

任何一次平台判据不匹配都属于 scheduler 核心故障：先以 Release 语义关闭 RuntimeGate，再以 Release 语义设置 `failed = true`；错误线程上的当前 callback 只调用 original，不构造 token，也不执行插件回滚。之后的 callback 不再运行任何业务 schedule，也不承担全局回滚。

所有 global failure 生产者都必须遵循同一发布顺序：

```text
RuntimeGate.close(Release)
  → failed.store(true, Release)
  → 禁止后续 Startup/Update 和 plugin debug dispatch
  → 本次 scheduler original 恰好调用一次
  → 停止 App/plugin 业务逻辑
  → 保活全部静态 callback context 与 callback backend
```

功能 callback 先以 Acquire 语义读取 RuntimeGate，再读取所属 PluginGate。总 gate 关闭后，之后观察到关闭状态的 callback 只调用自己的 typed original。已经越过 gate 检查的在途 callback 不会被强行中断，因此 context 保活仍是独立且必须满足的安全条件。

`failed` 的生产者限定为：

- replacement callback 发现当前线程不是主线程。
- App driver、plugin-system 或 scheduler 状态机发生未被插件级边界处理的 Rust panic 或不变量失败。
- SchedulerContext 发布后，SchedulerHook 安装、验证或必要回滚发生无法确认的错误。
- SchedulerHook 已安装后，Handoff publish 永久失败。

callback 在执行 App 前以 Acquire 语义检查 `failed`。失败状态不再运行 Startup、Update 或 debug dispatch；它只决定是否继续保留已有 App：

- 当前线程不可信：TLS 进入或保持 Unavailable，不领取 App；Handoff 或原 TLS owner 继续保活 App。
- 可信主线程且 App 尚在 Handoff：可以在不运行 driver 的前提下领取 App，调用 original 后进入 Exited(App)。
- 可信主线程且 App 已在 TLS：取出 App 设为 Busy，调用 original 后进入 Exited(App)。
- App 正由外层 callback 持有：嵌套 callback 只调用自己的 original；外层 original 返回后观察 failed 并停止后续业务逻辑。
- App 不可安全取得：本次只调用 original，现有 retention root 继续保活相关对象。

global failure 在当前进程不可恢复，不自动重试启动，也不得重新开启 RuntimeGate。它不调用 plugin-system 的全局 restore ledger，不等待未来 callback 执行回滚，也不要求卸载 SchedulerHook。已经进入 App 的 callback 仍由静态 context 保证安全；App 由当前 TLS 或既有 retention root 保活，之后只允许 original passthrough。

单个插件 Startup/Update 失败不是 scheduler failure；plugin-system 只退役和回滚所属插件，让其它插件与 App 继续运行。插件局部回滚只发生在该插件的 Build/Startup/Update 失败路径。

## Panic 边界

生产 runtime release profile 必须使用：

```toml
[profile.release]
panic = "unwind"
```

每次外层 replacement 构造一个只存在于当前 callback 栈上的 frame：

```rust
enum OriginalPhase {
    BeforeOriginal,
    CallingOriginal,
    AfterOriginal,
}

struct SchedulerFrame<'a> {
    context: &'a SchedulerContext,
    app: Option<Box<App>>,
    phase: OriginalPhase,
    tls_committed: bool,
}
```

replacement 使用两层边界：最外层 FFI guard 包围整个 Rust callback body；其内部先以 `app = None`、`phase = BeforeOriginal` 和 `tls_committed = false` 建立 frame，再由内层 execution `catch_unwind` 借用 frame 完成 TLS/Handoff 领取、driver、original 和提交逻辑。frame 建立在 execution catch 之外，因此包括领取 App 在内的 Rust unwind 被内层捕获后，恢复路径仍可检查 phase 和 frame 已经取得的 App。构造只复制已发布的 context 引用并初始化字段，不调用用户代码；必要时可以在经过审阅的边界使用 `AssertUnwindSafe`，但这不表示内部对象天然 unwind-safe。

`call_original_once` 只允许 `BeforeOriginal -> CallingOriginal`，在调用安装时捕获的 typed original 返回后立即转为 `AfterOriginal`。恢复路径按 phase 处理：

- `BeforeOriginal`：Rust 调度在 original 前 panic；关闭 RuntimeGate、发布 global failure，然后补调 original 一次。
- `CallingOriginal`：不得猜测 original 是否已经产生作用，也不得重试。
- `AfterOriginal`：original 已返回，恢复路径绝不再次调用。

original LateUpdate 不属于可恢复 Rust 逻辑。phase 只防止 Rust 恢复路径重复调用，并不捕获或恢复 Objective-C exception、C++ exception、Swift trap、signal、进程终止或 original 内部崩溃。

SchedulerFrame 还是 App ownership guard。正常路径显式把 App 提交回 `Running`，global failure 路径在 original 返回后停止业务逻辑并提交到 `Exited`，或由 retention root 保活。guard 的兜底 Drop 必须不分配、不调用插件代码且不 panic：若 frame 未提交且仍持有 App，它关闭 RuntimeGate 并把 App 进程期泄漏/保活；TLS 可以保持 Busy，使之后 callback 只 passthrough original。宁可失去回收，也不得在 unwind 中意外 drop App。

panic 恢复路径本身只允许使用已审阅的非 panic 操作；插件/effect restore action 不属于 scheduler global failure 的恢复步骤。如果恢复路径仍发生第二次 panic，最外层 FFI guard 必须吞掉它并让 SchedulerFrame 的兜底 Drop 保活 App，不能再次进入 original 调用分支。

plugin-system 在更内层为每个 boxed Startup/Update `System::run` 单独 `catch_unwind`。system error/panic 只退役所属插件并继续后续 owner；只有 plugin-system driver、共享 AppWorld/system adapter、TLS 或 SchedulerFrame 自身的不变量失败才越过该层，成为 scheduler global failure。Rust panic 不得越过 replacement 的 `extern "C"` 边界。

这些边界也不能修复错误 ABI、无效指针或内存破坏。

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

Handoff 前的失败统一属于 bootstrap failure：scheduler replacement 尚未可达时，worker 关闭 RuntimeGate，按 owner 逆序尝试恢复已经安装的功能 effects，停止已经启动的 Debug worker，并丢弃未交接的 App。功能 Hook 的 replacement 只会在其目标专用 CallbackSite 已发布后安装；已发布 site 即使从未安装成功也保活到进程退出。

SchedulerContext 发布后但 Handoff 尚未成功时，SchedulerHook 安装、验证、恢复或 Handoff publish 发生无法确认的错误，worker 先关闭 RuntimeGate，再设置 global failed，使任何可能到达的 scheduler 或功能 callback 都只调用各自 original。App 不进入正常运行；worker 完成本轮恢复尝试后丢弃未交接的 App。

Handoff 成功后再发生的 scheduler、App 或 plugin-system 基础设施故障属于 runtime global failure：只关闭 RuntimeGate、设置 global failed、停止 App/plugin driver，并保活 SchedulerContext、CallbackSiteContainer、typed original、gate reader 和 callback backend。第一版不设计额外的全局回滚或物理卸载；ownership drift 或恢复结果无法确认时，关闭的 RuntimeGate 保证仍可达的 replacement 只调用静态 site 中的 original。

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

## Runtime 待打磨项

- exact UnityFramework image 匹配、版本身份和 readiness timeout/backoff 的具体参数。
- `pthread_main_np()` 与目标 LateUpdate 线程关系的实验确认。
- Observability 的具体事件字段和队列容量可在实现时打磨；v1 只输出 Apple Unified Logging，不提供 file sink。
- App 退出后的物理 unload 与进程结束行为。

入口重复、bootstrap 阶段顺序、Handoff 前后失败范围、DebugState 的归属以及 Debug transport 的 v1 边界已经确定；不再作为待设计项单独扩展状态机。
