# Runtime：bootstrap 与 scheduler

状态：v2 设计（2026-08-29 修订）。本文定义概念 crate `runtime` 的 FFI 入口、bootstrap worker、readiness 阶梯、Unity 主线程 scheduler、Handoff、TLS 与 runtime 级失败边界。App / PluginManager / driver 见 [Runtime：App 与 driver](plugin-system.md)。

## 依赖与职责

runtime 负责：

- 导出 `scsp_start` 并保证入口非阻塞、幂等。
- 定位已加载 UnityFramework，并通过 exact handle 初始化 core IL2CPP backend。
- 读取 `DataRoot/shiny-song-tools/scsp.toml` 构造 typed `RuntimeConfig`（缺失时自动创建空的 fail-closed 配置并使用默认值，解析失败仍 fail-closed：全默认值、debug 强制关闭）。
- 构造 App、按固定顺序注册生产插件（`debug.enabled` 时 **DebugPlugin 注册在列表首位**，其后为功能插件）并驱动 worker build。
- 安装 LateUpdate SchedulerHook。
- 将 App 一次性交接到 Unity 主线程 TLS。
- 保证 scheduler callback 中 original LateUpdate 恰好调用一次。
- 把 scheduler 核心失败升级为 global failure 路径。

runtime 不定义功能插件的数据格式，不实现 wire transport 或具体功能逻辑。

## 证据边界

实验仓库已经证明当前精确游戏版本上 MethodPointer replacement、callback 进入、original 调用、恢复以及 exact UnityFramework handle 的低层 IL2CPP API 加载路径可行。这些结果只约束生产设计，不表示本 crate 已实现或验证。特别注意两条实验定案：**`il2cpp_domain_get` 在跨过 main-queue gate 与 image/export 校验后恰好调用一次**（把 domain_get 当轮询探针曾导致 `GC_init_gcj_vector: bad index` SIGABRT，该 carrier 已退役）；**AKPlugin 三个窗口安全修复必须保留**（缺失曾导致主线程 SIGTRAP）。

PlayCover 环境中的 UnityFramework 可能以 `RTLD_LOCAL` 加载。runtime 不使用 `RTLD_DEFAULT` 或假定高层 bridge 初始化可以发现 IL2CPP symbols；必须通过平台 image 枚举取得精确 UnityFramework image/handle，再交给 core 的 exact-handle loader。

## 生命周期对象

- `ObservabilityRoot`：在 App 之前创建并保活到进程退出的 `tracing::Dispatch`、Apple Unified Logging layer、compact event queue 与 drain worker retention root。
- `BootstrapContext`：worker 临时对象，负责 DataRoot/config、启动状态与启动期资源；交接或启动失败安全处理后销毁。
- `App`：唯一 `Send` 组合根；worker 完成 build 后通过 Handoff 转移给主线程 TLS。
- `Handoff`：`Mutex<Option<Box<App>>>` 的一次性跨线程所有权槽。
- `SchedulerContext`：进程期稳定的 LateUpdate callback context，持有 Handoff、SchedulerHook、RuntimeGate 控制 handle 与 global failure flag。

不设置持有插件业务状态的常驻 RuntimeKernel。入口去重只需要进程级一次性启动标记；不为入口重复、bootstrap 失败或 global failure 增加 supervisor 状态机。

## `scsp_start` 与 bootstrap

```text
AKPlugin.init()
  → DispatchQueue.main.async
  → scsp_start(documents_path)
  → 在最外层 unwind guard 内初始化或取得进程期 ObservabilityRoot
  → 启动独立于 App/RuntimeGate 的 compact event drain worker
  → 尝试领取进程期一次性启动标记
  → 若已领取：记录 start duplicate 并立即返回
  → 复制并校验路径（拒绝空指针/空路径；无效路径记录后结束本次启动，不重试）
  → 解析 scsp.toml → RuntimeConfig（缺失自动创建空配置并按默认；解析失败 fail-closed：全默认 + debug 关闭）
  → 启动唯一 bootstrap worker
  → 立即返回

bootstrap worker
  → 阶梯 1（可轮询，非 IL2CPP 操作）：在 monotonic deadline 内等待 image 列表出现
    唯一且身份匹配的 UnityFramework；取得并保活 exact handle
  → 阶梯 2（单次）：在该 exact handle 上一次性加载全部所需 IL2CPP exports；
    缺失即失败，不回退 RTLD_DEFAULT
  → 阶梯 3（可轮询，非 IL2CPP 操作）：在同一 exact handle 上调用经版本审计的
    `CRIWARE2813B966` 完成谓词；有界超时或符号缺失即 fail-closed，绝不触碰 IL2CPP
  → 阶梯 4（探测恰好一次）：调用 il2cpp_domain_get()；
    返回 null 即本次一次性 bootstrap 终止——不轮询、不重试（实验定案；
    阶梯 5 的 cache hydration 内部会重读一次 domain，实证无害，见 core-crate 分册）
  → 阶梯 5（单次）：worker attach 到该 domain（RAII detach guard，只 detach 本次建立的
    attachment）；等待/执行 scheduler 所需 assembly/image/metadata 解析与目标校验
  → 阶梯 6：校验 runtime/layout 身份属于生产实现明确支持的集合
  → 构造 core IL2CPP backend 与基础设施 handles
  → 构造初始关闭的 RuntimeGate
  → App::new（注入 RuntimeConfig 与 RuntimeGateReader）
  → 按固定顺序 App::add_plugin
      （debug.enabled 时 DebugPlugin 在首位：其 build 创建 UDS transport worker 并
        删除同名残留 socket；失败只使 Debug 不可用，不阻塞其它插件）
      → Plugin.build（直接插入 resources、boxed systems、container/route/topic、
        hook typestate 发布 site 后 CAS 安装，gate 关闭）
  → 解析并校验 scheduler LateUpdate 目标
  → 捕获 typed original，构造 SchedulerHook
  → 以空 Handoff 构造 SchedulerContext
  → SCHEDULER.set(context)
  → CAS 安装 SchedulerHook
  → Handoff.publish(App)
  → worker detach 本次建立的 IL2CPP attachment
  → worker 退出
```

`DispatchQueue.main.async` 只让 `scsp_start` 离开 `AKPlugin.init()` 当前调用栈；它不创建 Unity 主线程，也不代表 IL2CPP ready。LateUpdate replacement 才是运行时交接点。

任何一步失败的统一语义：本次一次性 bootstrap 终止，无重试。失败时在 RuntimeGate 已创建的前提下将其关闭，按 owner 逆序尝试恢复已安装功能 effects，并丢弃未交接的 App；已发布的 CallbackSite、typed original、gate reader 与 callback backend 继续由静态 site 保活。阶梯 1–5 阶段尚未发布 site 或写入 MethodPointer，因此该阶段失败不需要 hook rollback。具体总超时、backoff（仅阶梯 1 允许轮询）、image identity 格式与 assembly-ready 探测属于实现参数，但必须有界且可测试。

一次性入口标记在复制参数前领取，因此首个调用即使参数无效也不会触发第二次 bootstrap。ObservabilityRoot 使用自己的进程期 `OnceLock`，重复入口只复用它记录事件。

## readiness 阶梯要点

- 阶梯间有严格先后：前一步未满足不得调用依赖后一步的 API。
- 阶梯 1（image 出现）和阶梯 3（CRIWARE 完成谓词）允许有界轮询等待；阶梯 2、4–6 都是单次尝试、fail-closed。cache hydration 等阻塞调用在 attach 之后执行（实验实测约 6 秒量级），属于 bootstrap worker 内的正常工作。
- readiness 只证明可以安全开始 build。scheduler 目标缺失使整个 bootstrap 失败；功能插件目标缺失只退役该插件。
- worker 只对本次调用建立的 attachment 创建 RAII detach guard；不得 detach 外部已建立的 attachment。exact handle 与 API table 转移进 `Il2CppBackend` 按其生命周期保活。

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

worker 的 `publish` 加锁把 `None` 改为 `Some`，只允许一次；重复发布不得覆盖。callback 只调用 `try_take`：槽中有 App → `Ready`；槽空或 worker 暂时持锁 → `Pending`（不等待，本次 callback 只调 original，下一次重试）；mutex poisoned 或不变量损坏 → `Failed`（当前线程 TLS 进入 `Unavailable`，之后只调 original）。持有 Handoff guard 时不得执行插件逻辑、日志 I/O、original 或其它外部调用；临界区只移动一个 `Box<App>`。

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

original LateUpdate pointer 只由 SchedulerHook 持有；callback 始终通过 `SchedulerHook::call_original` 使用安装前捕获的 typed original，不重读 slot。固定激活顺序：

```text
捕获 original → 构造 SchedulerHook → 用仍关闭的 RuntimeGate 构造空 Handoff 与 SchedulerContext
  → SCHEDULER.set → CAS 安装 SchedulerHook → Handoff.publish(App)
```

包含 original 的完整 context 必须先发布，replacement 才能可达；callback 可达但 `SCHEDULER.get() == None` 是构造不变量破坏，不是正常启动分支。SchedulerHook 已安装而 App 尚未 publish 的短窗口内，callback 只调用已发布的 original，后续 callback 再尝试领取 App。

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
    static APP_SLOT: RefCell<AppSlot> = const { RefCell::new(AppSlot::AwaitingHandoff) };
}
```

- `AwaitingHandoff`：尚未领取 App。`Running(App)`：App 由当前游戏主线程 TLS 独占。`Busy`：外层 callback 已把 App 移到栈上，正在运行 App 或 original。`Exited(App)`：逻辑终态，保留 App 与仍可能被 callback 使用的资源。`Unavailable`：该线程从未取得 App，且 Handoff 或 scheduler 已永久失败。

外层 callback 以很短的 RefCell 借用把槽替换为 Busy，取得 App 后立即释放借用；Busy 覆盖 App schedule 与随后的 original 调用，original 返回后才放回 Running 或 Exited。嵌套 callback 看到 Busy 时只调 original，不争用 RefCell。普通 Hook callback 不访问此 TLS。

外层 callback 在取得静态 SchedulerContext 后，先用不分配、不借用 TLS 的字段初始化建立栈上 `SchedulerFrame`，再进入 catch scope 领取 App。frame 是单次 callback 的执行与所有权 guard，不放入 App、TLS 或任何持久生命周期 enum；TLS 中的 Busy 只表示 App 当前由这个 frame 独占。

## 固定 callback 顺序

```text
取得 SchedulerContext
  → 检查 global failure 与当前线程身份
  → 根据 TLS 领取 App 并设为 Busy，释放 RefCell 借用
  → 首次调用 App.run_startup；后续调用 App.run_update
  → 在 TLS 仍为 Busy 时调用 original LateUpdate
  → original 返回后把 App 放回 Running 或 Exited
```

首次 callback 只运行 Startup driver（RuntimeGate 保持关闭，driver 结束时最后开启 RuntimeGate），Update 从下一次外层 LateUpdate 开始；driver 的内部顺序（MessageMaintenance → CommandDrain → plugin Update）由 plugin-system 冻结，runtime 不重新排序。CommandDrain watermark 之后到达的 callback-to-main message 留到下一帧。

## 线程身份与 global failure

v1 每次最外层 scheduler callback 都以 `pthread_main_np() != 0` 验证当前线程是 process main thread；不使用 Swift main queue 捕获的线程 ID，也不把首次 callback 自动视为可信。验证成功后 runtime 才为当前 SchedulerFrame 构造不可 Send/Sync、不可 Clone/Copy 的 `MainThreadToken`，并只把短借用作为 system 输入传入；token 所有权不进入 AppWorld 或下一帧。

任何一次平台判据不匹配都属于 scheduler 核心故障：先以 Release 语义关闭 RuntimeGate，再以 Release 语义设置 `failed = true`；错误线程上的当前 callback 只调用 original，不构造 token，也不执行插件回滚。之后的 callback 不再运行任何业务 schedule，也不承担全局回滚。

所有 global failure 生产者都必须遵循同一发布顺序：

```text
RuntimeGate.close(Release)
  → failed.store(true, Release)
  → 禁止后续 Startup/Update 与 debug dispatch
  → 本次 scheduler original 恰好调用一次
  → 停止 App/plugin 业务逻辑
  → 保活全部静态 callback context 与 callback backend
```

功能 callback 先以 Acquire 语义读取 RuntimeGate，再读取所属 PluginGate。总 gate 关闭后，观察到关闭状态的 callback 只调用自己的 typed original；已越过 gate 检查的在途 callback 不被强行中断，context 保活因此是独立且必须满足的安全条件。

`failed` 的生产者限定为：replacement 发现当前线程不是主线程；App driver、plugin-system 或 scheduler 状态机发生未被插件级边界处理的 panic 或不变量失败；SchedulerContext 发布后 SchedulerHook 安装、验证或必要回滚发生无法确认的错误；SchedulerHook 已安装后 Handoff publish 永久失败。

callback 在执行 App 前以 Acquire 检查 `failed`。失败状态不再运行 Startup/Update/debug dispatch，只决定 App 的处置：不可信线程 → TLS 进入或保持 Unavailable；可信主线程且 App 在 Handoff → 不运行 driver 领取 App，调 original 后进入 Exited；可信主线程且 App 已在 TLS → 取出设为 Busy，调 original 后 Exited；App 正被外层持有 → 嵌套 callback 只调 original，外层返回后观察 failed 停止业务逻辑；App 不可安全取得 → 本次只调 original，retention root 保活。

global failure 在当前进程不可恢复：不自动重试启动，不重新开启 RuntimeGate，不调用 plugin restore ledger，不等待未来 callback 回滚，不要求卸载 SchedulerHook。App 由当前 TLS 或既有 retention root 保活，之后只允许 original passthrough。单个插件的 Startup/Update 失败不是 scheduler failure，按 plugin-system 的局部规则退役。

## Panic 边界

生产 runtime release profile 必须使用 `panic = "unwind"`；不得为缩小产物改为 abort——scheduler 与插件错误隔离依赖 `catch_unwind`。

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

replacement 使用两层边界：最外层 FFI guard 包围整个 Rust callback body；其内部先建立 frame，再由内层 execution `catch_unwind` 借用 frame 完成 TLS/Handoff 领取、driver、original 与提交逻辑。frame 建立在 execution catch 之外，因此包括领取 App 在内的 unwind 被内层捕获后，恢复路径仍可检查 phase 与已取得的 App。`AssertUnwindSafe` 只允许出现在经过审阅的边界，不宣称内部对象天然 unwind-safe。

`call_original_once` 只允许 `BeforeOriginal -> CallingOriginal`，调用安装时捕获的 typed original 返回后立即转为 `AfterOriginal`。恢复路径按 phase 处理：`BeforeOriginal` panic → 关 RuntimeGate、发布 global failure，然后补调 original 一次；`CallingOriginal` → 不猜测 original 是否已生效，不重试；`AfterOriginal` → 绝不再次调用。

original LateUpdate 不属于可恢复 Rust 逻辑。phase 只防止 Rust 恢复路径重复调用，不捕获或恢复 Objective-C/C++ exception、Swift trap、signal、进程终止或 original 内部崩溃。

SchedulerFrame 还是 App ownership guard。正常路径显式提交回 `Running`；global failure 路径提交到 `Exited` 或由 retention root 保活。guard 的兜底 Drop 必须不分配、不调用插件代码且不 panic：若 frame 未提交且仍持有 App，它关闭 RuntimeGate 并把 App 进程期泄漏/保活；TLS 保持 Busy，使之后 callback 只 passthrough original。宁可失去回收，也不得在 unwind 中意外 drop App。

panic 恢复路径只使用已审阅的非 panic 操作；插件/effect restore action 不属于 scheduler global failure 的恢复步骤。若恢复路径发生第二次 panic，最外层 FFI guard 必须吞掉它并让兜底 Drop 保活 App，不得再次进入 original 调用分支。plugin-system 在更内层为每个 boxed system 单独 `catch_unwind`（见 plugin-system 分册）；Rust panic 不得越过 replacement 的 `extern "C"` 边界。这些边界不能修复错误 ABI、无效指针或内存破坏。

## Scheduler 目标专用 ABI

第一版只接受实验验证过的精确目标：`UniRx.dll / UniRx / MainThreadDispatcher / LateUpdate / 0`。构造 SchedulerHook 前必须确认实例方法、显式参数为零、返回 `System.Void`、MethodInfo 非空，并匹配受支持 runtime/layout 身份。

```rust
#[repr(C)]
struct Il2CppObjectOpaque { _private: [u8; 0] }

#[repr(C)]
struct MethodInfoOpaque { _private: [u8; 0] }

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

raw pointer 到 `LateUpdateFn` 的转换只能发生在目标身份、runtime 身份与 layout 校验完成后的构造边界。replacement 使用相同类型，并把 `this`、`method` 原样传给 original。metadata 参数与返回类型只用于拒绝目标漂移，不能自动推导 Rust ABI；隐式 MethodInfo 参数与调用约定是当前验证目标的版本绑定事实，不外推为其它 IL2CPP 方法的通用规则（功能插件的目标走 plugin-api 的 `HookTarget`，各自声明 ABI）。

安装失败使整个 bootstrap 失败；功能 Hook 安装失败只退役所属插件。installed/CAS/readback/ownership 语义遵循 core 分册。

## 启动失败与保活

Handoff 前的失败统一属于 bootstrap failure：worker 关闭 RuntimeGate，按 owner 逆序尝试恢复已安装功能 effects，并丢弃未交接的 App。功能 Hook 的 replacement 只会在其目标专用 site 发布后安装；已发布 site 即使从未安装成功也保活到进程退出。

SchedulerContext 发布后但 Handoff 尚未成功时，SchedulerHook 安装、验证、恢复或 publish 发生无法确认的错误：worker 先关 RuntimeGate，再设置 global failed，使任何可能到达的 scheduler 或功能 callback 都只调用各自 original；worker 完成本轮恢复尝试后丢弃未交接的 App。

Handoff 成功后的基础设施故障属于 runtime global failure：只关 RuntimeGate、设置 global failed、停止 driver，并保活 SchedulerContext、静态 site/container、typed original、gate reader 与 callback backend。第一版不设计额外的全局回滚或物理卸载；ownership drift 或恢复结果无法确认时，关闭的总 gate 保证仍可达的 replacement 只调用静态 site 中的 original。

## 退出与卸载

第一版不支持运行时释放 scheduler。SchedulerContext 与 SchedulerHook 由 OnceLock 保活到进程退出；App 退出只把 TLS 变为 Exited(App)，之后 callback 只调 original。Debug transport 与 worker 的生命周期同样到进程退出为止（客户端感知为连接关闭；残留 socket 由下次启动的 unlink 前置清理，见 debug 分册）。未来真正卸载 scheduler 必须另行设计 quiescence 协议，届时 OnceLock 需替换为可清空的共享槽，当前不展开。

## 待打磨项

- exact UnityFramework image 匹配、版本身份格式与阶梯 1 的 timeout/backoff 参数。
- `pthread_main_np()` 与目标 LateUpdate 线程关系的实验确认。
- Observability 事件字段与队列容量；scsp.toml 的完整 schema（v1 仅 `[debug] enabled` 与各插件私有段）。
- App 退出后的物理 unload 与进程结束行为。
