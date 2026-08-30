# 架构审查与类型驱动重设计记录

日期：2026-08-29
性质：对当时 `docs/` 中 v1 设计（runtime-architecture / core-crate / plugin-api / plugin-system / runtime-crate / debug-diagnostics-logging 各分册）的审查结论 + 一套替代架构提案；分册现已归入各 crate Rustdoc。
证据输入：`~/Documents/scsp-playcover-hook` 的 research/01–03、lateupdate-methodpointer-poc A/B 实测记录、il2cpp-bridge-rs 适配实验、openspec specs。

---

## 第一部分：现行设计审查结论

总评：**安全骨架质量很高**。bootstrap 阶梯、MethodPointer CAS/readback、双 gate、SchedulerFrame 三阶段、Handoff、TLS 状态机、panic 边界这些与实验证据对齐的部分应当原样继承。问题集中在四处：对实验结论的一处静默偏离（高危）、对 bevy_ecs 机制的两处想当然、Debug 子系统的规模与位置错配、以及大量靠散文不变量维护的约束——后者恰恰是 Rust 类型系统能编译期吃掉的东西。

### 1.1 高危 / 致命级

**F1 · `il2cpp_domain_get` 轮询与实验定案的单次调用规则冲突（文档冲突 + 潜在致命）**
- 位置：`runtime-crate.md:102-110`（readiness 阶梯"有上限的 backoff 依次等待 … 3. `il2cpp_domain_get()` 返回非空 domain"）、`runtime-crate.md:68`（"等待 il2cpp_domain_get() 返回非空 domain"）。
- 证据：实验仓库 `research/03-hook-runtime-design.md:101` 明确"只调用一次 il2cpp_domain_get；null 或重复 GC-init 画像直接停止"；首轮 A 实验（carrier SHA `9a7f0c…`）的 SIGABRT（`GC_init_gcj_vector: bad index`）根因诊断就是把 domain_get 当 pre-init 轮询探针，该 SHA 已退役。
- 后果：实现者按字面"backoff 轮询 domain_get"实现，会精确复现已定因的启动崩溃，并消耗一次宝贵的实机窗口。
- 修正：阶梯 1–2（image/exports）可以轮询；跨过该 gate 后 `il2cpp_domain_get` **恰好调用一次**，null 即本次一次性 bootstrap 终止。

**F2 · Startup staged-resources 语义在两篇文档间自相矛盾，且与 bevy_ecs 提交模型脱节**
- 位置：`plugin-system.md:124`（"前面 system 创建的 resources/effects 继续留在同一个未提交 transaction 中；**全部成功才整体提交**"——per-plugin 提交）vs `plugin-api.md:92`（"system 返回后 driver 才把成功暂存的 resource 插入共享 AppWorld…**新 resource 对之后的 Startup system 可见**"——per-system 插入）。
- "对后续 Startup system 可见"只有在 **per-system 插入**下才成立；per-plugin 整体提交下，本插件后一个 Startup system 根本查不到前一个暂存的 resource（World 里没有），声明 `Res<T>` 会在运行时 param validation 失败。两篇文档承诺的语义不可同时成立。
- 修正（见提案 §2.6）：放弃 staged/延迟提交模型，改为**直接插入 + owner ledger 记账 + 失败 LIFO 移除**；所有 boxed system 改为**首次运行前惰性 initialize**，一并解决"Update system 引用 Startup 阶段才存在的 resource"的初始化时机问题。

**F3 · 悬空引用：Debug 启用条件依赖一个不存在的"现有 runtime 配置"**
- 位置：`runtime-crate.md:60`、`debug-diagnostics-logging.md`（"复用现有 runtime 配置"）。
- 全部 8 篇文档没有定义任何配置文件格式、加载时机、解析失败语义。实验仓库 research/03 §9 有 `config.toml` 预留与 DataRoot 布局，生产文档没有承接。
- 修正（见提案 §2.8）：定义 `DataRoot/scsp.toml` + typed `RuntimeConfig`，解析失败 fail-closed（记录事件、按全默认值、debug 强制关闭）。

### 1.2 文档冲突 / 结构问题

- **C1** = F1。
- **C2 · 命名摇摆**：`MessageMaintenance` / `MessageBridge` / `CommandDrain` 三个 M-词在 plugin-api.md 与 plugin-system.md 里指代关系需要读者自行拼装（阶段？组件？同一物？）。
- **C3 · 清理残留**：`core-crate.md:89-90` callback-safe primitives 列表有两条几乎重复的 mailbox 条目，是多轮合并后未清理的痕迹。
- **C4 · 与实验仓库设计的演进关系未标注**：research/03（RuntimeKernel + ScspApp thread_local + PreUpdate/PostUpdate + 全局失败 worker 兜底回滚）已被生产 docs 显著修订（App + Handoff TLS + 单阶段固定 driver + 全局失败不回滚），但无任何文档声明"取代了哪些决定"，未来回看会以为两套都有效。
- **C5 · 不变量多处重复、措辞已开始漂移**：gate 发布顺序、保活对象清单（"gate reader""callback backend""typed original"各处列举不一致）、一次性入口语义在 5+ 处重复。单点修改必然漏改。应把不变量集中到总览一处，分册只引用。

### 1.3 过度设计

- **O1 · Debug Control Plane 规模与消费者不成比例**：UDS server、9 种错误码、correlation ID、pending-request 状态机、0600、单连接、topic version、帧上限……占两篇文档约 1/3 篇幅，而 v1 功能插件没有一个需要它。且其 main/callback handler、pending 语义完全可以由公开的跨域 route 原语表达——Debug 应当是**一个普通插件**，而不是 core/plugin-system 的内建子系统（`AppCore::DebugState` 特例、driver 内建阶段、跨三篇文档的特有语义）。
- **O2 · Startup staged-resource 事务是自造 mini-ORM**：staged 可见性、延迟提交、类型冲突检查叠在 bevy_ecs 之上，还引出 F2。owner ledger 已记录插件插入了哪些资源，失败 LIFO 移除即可达到同等安全效果（被移除资源的依赖插件在 param validation 时失败退役，行为正确）。
- **O3 · `Frozen<T>` 无语义**：容器本身因静态 site 保活就是 `'static`，callback 直接借用容器字段即可。它只是一个命名意图，不该进 API 面。
- **O4 · 四个 message endpoint facade 过多**：两个方向 × 两个端本可由方向 branded 类型统一成两个泛型类型（见提案 §2.5）。

### 1.4 欠设计

- **U1 · Hook 注册 API——全项目核心——却是文档最含糊的部分**："插件提供受支持的目标专用 wrapper"、"外部 Hook API"没有 trait/builder 签名；ABI 类型如何声明、static site 如何生成、container 如何绑入，没有一个完整代码示例（有 translation container 的例子，没有 hook 注册的例子）。
- **U2 · `PluginError` 从未定义**（Plugin trait 与 system 返回类型都用它）；`RestoreError` 只有口头描述。
- **U3 · bevy_ecs 版本未固定**：文档使用版本敏感 API（`Messages<M>`/`Message` derive、`InRef`/`InMut` 均为 0.17 引入/改名），依赖表却只对 `il2cpp-bridge-rs` 写了"固定版本"。World/SystemParam 内部在 minor 版本间会变，必须像 IL2CPP backend 一样 pin。
- **U4 · DebugDispatch main handler 如何取得 typed World 资源未说明**。
- **U5 · DataRoot 生产布局未定义**（目录结构、谁创建子目录、config 位置）。
- **U6 · 无统一验证策略**：无游戏 fixture 清单散落各篇，没有一篇"实现顺序 + 每步验收门"。

### 1.5 自己造轮子

- **W1 · Debug wire envelope 是 JSON-RPC 2.0 的劣化版**：`{id, topic, version, payload}` → `{id, ok, payload|error{code,message}}` 与 JSON-RPC 2.0 的 `{id, method, params}` → `{id, result}|{id, error{code,message}}` 同构。自造意味着重新定义一整页 error/边界语义；采用 JSON-RPC 2.0 直接继承成熟规范与现成客户端工具。
- **W2（可接受）**：latest-value mailbox 在 Copy-only payload 下只有约 20 行，自造合理；但应作为"两种 mailbox 语义之一"暴露（见 P1），而非唯一语义。
- **正面**：bevy_ecs 嵌入（不接管 runner）、tracing/os_log、il2cpp-bridge-rs exact-handle、crossbeam、thiserror/serde 的采用决策全部正确，"SCSP 仍负责"列也清楚。

### 1.6 API 不统一 / 设计差

- **A1 · 三套错误词汇并存**：`HookError`（5 变体）、Debug wire error codes（9 个）、`RestoreError`，`PluginError` 缺位。插件作者要面对 3+ 套词汇，且没有映射规则。
- **A2 · 命名漂移**：AppWorld/共享 AppWorld/World；CallbackSiteContainer/callback container/CallbackContainerHandle；C2 的三 M-词。
- **A3 · 输入参数机制混用**：借 Bevy 的 `InRef`/`InMut` 与自造输入 tuple（`(&MainThreadToken, &mut StartupRegistrar)`）混写；`!Send/!Sync` 的 token 走 Bevy 泛型 SystemParam 的可行性未验证。应改用 SCSP 自己的 phase context 参数（顺带获得编译期 phase 区分，见提案 §2.4）。
- **A4 · 跨域 message 一律 latest-value，强行统一"事件"与"状态"**：Texture/LiveMv 类 callback 事件在覆盖语义下会丢中间事件。v1 五插件或许都能绕开，但该约束会在第一个真需要事件流的插件上翻车。`Latest<T>` 与 `Bounded<T, N>` 应作为注册时的类型选择。
- **A5 · 跨域 payload 限定 `Copy` 过度收紧**：排除 String/Vec 等"无 Drop 但非 Copy"类型。约束的真实动机是"无任意析构、无分配"，`Copy` 只是充分不必要条件。

---

## 第二部分：重设计提案——类型驱动 + Bevy 资源模型

设计公理（每条类型级构造都必须对应"删除一条散文不变量"，否则不引入）：

1. **证据锚定区不动**：bootstrap 阶梯（F1 修正后）、MethodPointer CAS/readback/ownership、双 gate、SchedulerFrame/OriginalPhase、Handoff、TLS AppSlot、`panic = "unwind"`、FFI 单入口幂等——全部原样继承自现行 runtime-crate.md，这是实验验证过的资产。
2. **先发布后安装、phase 区分、callback 能力约束**从散文不变量降级为**类型事实**（typestate / branded types / capability token）。
3. **资源模型完全落在 bevy_ecs 上**：World、Resource、SystemParam、惰性初始化的 boxed System；SCSP 只加 owner、phase、gate、panic 边界四层。
4. **Debug 是插件不是子系统**；**Observability 保持现设计**（该部分本身是好的）。

### 2.1 Crate 布局

```text
crates/
  core         平台基础：MainThreadToken、gates、MethodPointerSlot、MethodRef、
                    Il2CppBackend 句柄、callback-safe 原语（mailbox/原子）、CompactEvent
  plugins   插件可见的一切：Plugin trait、AppCtx facade、phase context、
                    route/endpoint、hook typestate API、错误类型（无实现依赖）
  runtime      App、PluginManager、固定 driver、bootstrap worker、scheduler、
                    scsp_start FFI、Observability 装配、内置 debug 插件（可选 feature）
plugins/           功能插件（编译期固定列表，只依赖 plugins）
swift/  patches/   carrier 与构建侧（bundle-build.md / swift-entry.md 不变）
```

变更：**plugin-system 并入 runtime**。plugin-system 与 runtime 都是 runtime 侧内部物，没有"只想要 plugin-system 不要 bootstrap"的第二消费者；少一条 crate 边界就少一类 C5 式漂移。插件依旧只依赖 `plugins`，隔离目标不变。

### 2.2 Core：capability 与原语

```rust,ignore
// —— 线程能力（继承现设计）——
pub struct MainThreadToken {          // !Send + !Sync，构造受审阅 unsafe，无 Clone/Copy
    _not_send_sync: PhantomData<Rc<()>>,
    _private: (),
}

// —— 门（集中定义一次，Release 写 / Acquire 读）——
#[derive(Clone)] pub struct GateReader(Arc<AtomicBool>);
pub struct RuntimeGate(GateReader);   // runtime 独占控制端；关闭后本进程不可重开
impl GateReader { pub fn is_open(&self) -> bool { self.0.load(Ordering::Acquire) } }

// —— MethodPointer（继承现设计：CAS/readback/ownership drift/不盲写）——
pub struct MethodRef { /* assembly/ns/class/name/param_count/MethodInfo/slot 地址 */ }
pub struct MethodPointerSlot { /* 校验、CAS 安装/恢复、readback、ownership */ }

// —— callback-safe 原语（删去 Frozen<T>；mailbox 两种语义）——
pub struct LatestCell<T: Copy + Send + Sync> { /* 单格覆盖，try_send→Sent::{Accepted,Replaced} */ }
pub struct BoundedQueue<T: Copy + Send + Sync, const N: usize> { /* crossbeam ArrayQueue 包装，try_send→Result<(), Full> */ }
```

删除 `Frozen<T>`：CallbackSiteContainer 因静态 site 保活本就是 `'static`，callback 直接借用容器字段；"不可变"由字段类型（非 `&mut` 路径）与审阅保证，不需要一个新类型名。`CompactEvent`（固定大小 Copy、code/level/owner/site/arg0/arg1、进程级 ArrayQueue、drain worker）**原样继承**。

### 2.3 Hook：typestate 把"先发布后安装"变成编译期事实

目标抽象为 `HookTarget`，ABI 与校验谓词绑定在类型上；每个目标一个宏生成的 `'static` site；安装流程用 typestate 状态机表达：

```rust,ignore
pub trait HookTarget: 'static {
    const TARGET: TargetId;                      // assembly/namespace/class/name/param_count
    type Original: Copy;                         // typed fn pointer，如 LateUpdateFn
    fn validate(method: &ResolvedMethod) -> Result<(), HookError>;  // 签名/参数/返回类型谓词
}

// 宏为每个目标生成唯一静态槽与 site 类型（进程期 retention root）
define_hook_site!(LATE_UPDATE: CallbackSite<LateUpdateTarget, C>);

// 注册 API（AppCtx 上），typestate：
let sites: Arc<MySites> = ctx.register_container(MySites { table, writer })?;

ctx.hook::<LateUpdateTarget>(LATE_UPDATE)          // HookBuilder<T, Unpublished>
   .container(sites.clone())
   .handler(my_handler)                            // -> HookBuilder<T, Published>
   .install()?;                                    // -> Result<InstalledHook, HookError>
```

- `HookBuilder<T, Unpublished>` 只提供 `container`/`handler`；`HookBuilder<T, Published>` 才有 `install`。`handler` 这一步构造 `CallbackSite { original, runtime_gate, plugin_gate, container }` 并 `OnceLock::set`——**发布先于安装不再是文档规则，而是类型上唯一可行的调用顺序**。
- `install` 内部完成 MethodRef 解析校验 → CAS → readback，返回 `InstalledHook`；其 Drop/restore 走 owner ledger（继承现设计）。
- `HookError` 保持五变体：`target_unavailable / signature_mismatch / site_already_registered / slot_conflict / installation_failed`。
- `CallbackSite<T, C>`、双 gate、目标专用静态槽、进程期保活、replaced 只透传——语义全部继承现设计，只是把编排从散文挪进类型。

callback 侧约束不变：callback 不碰 App/World/TLS；不阻塞、无界分配、panic 不跨 `extern "C"`；exactly-once original 由目标 wrapper 的 SchedulerFrame 式三阶段 guard 保证（继承 runtime-crate.md 的 OriginalPhase 设计，推广为所有目标 wrapper 复用的 `OriginalGuard` 类型）。

### 2.4 System：编译期 phase 区分 + 惰性初始化

不再用 Bevy 的 `InRef`/`InMut` 输入 tuple（A3），SCSP 定义自己的 phase context 作为 boxed system 的输入类型：

```rust,ignore
pub struct StartupCtx<'a> { pub main: &'a MainThreadToken, pub reg: &'a mut StartupRegistrar }
pub struct UpdateCtx<'a>  { pub main: &'a MainThreadToken }

pub trait PhaseInput { type Ctx<'a>; }             // sealed
impl PhaseInput for StartupPhase { type Ctx<'a> = StartupCtx<'a>; }
impl PhaseInput for UpdatePhase  { type Ctx<'a> = UpdateCtx<'a>; }

// driver 侧 boxed system 按 phase 参数化：
pub struct BoxedSystem<P: PhaseInput>( /* bevy_ecs System<In = P::Ctx<'_>, Out = Result<(), PluginError>> */ );

// 插件注册 API：
ctx.add_startup_system(startup_fn);   // 只接受第一个参数为 StartupCtx 的函数
ctx.add_update_system(update_fn);     // 只接受第一个参数为 UpdateCtx 的函数
```

phase 约束由 `BoxedSystem<P>` 的类型参数承担——**把 Startup system 注册进 Update 列表是编译错误**，不再是"两种 boxed input 类型不能互相注册"的运行时规则。`StartupRegistrar` 只剩窄职责：登记 restore action（`AnyThread`/`MainThread` 两种，继承现设计）。

- **资源插入不做 staging**（F2/O2 修正）：`StartupCtx`/build facade 的 `insert_resource` 直接写共享 World，同时向 owner ledger 记录 `(type_id, 顺序)`；重复类型返回 `PluginError::ResourceConflict`（不用 Bevy 的覆盖语义——保留现设计的这条规则）。插件 Startup 失败 → 关 gate → ledger LIFO 移除其登记的 resource → 执行 restore actions → 依赖缺失资源的其它插件在 param validation 时自然失败退役。
- **惰性初始化**：每个 boxed system 在**首次运行前**才 `System::initialize`。一次性解决：Update system 引用 Startup 阶段资源、跨插件依赖前序插件 Startup 产物、以及任何 `Messages` 类 param 的 init 期存在性要求。
- driver 顺序、per-system `catch_unwind`、owner 退役、AssertUnwindSafe 只在审阅的适配层——全部继承现设计。

### 2.5 跨域 message：方向 branded endpoint + 语义类型选择

```rust,ignore
pub trait CallbackPayload: Copy + Send + Sync + 'static {}   // 仅 callback 侧端点要求

pub trait Domain: 'static {}                                  // sealed
pub struct CallbackToMain; pub struct MainToCallback;
impl Domain for CallbackToMain {} impl Domain for MainToCallback {}

pub struct Endpoint<P, D, M> { /* M: MailboxKind = Latest | Bounded<N> */ }
```

公开面收敛为**两个泛型类型**（替换 O4 的四个 facade）：

- `Endpoint<P, CallbackToMain, M>`：callback 侧 `try_send(&self, cb: &CallbackCtx, msg: P)`；main 侧 `Reader<P>` 作为 SystemParam facade。
- `Endpoint<P, MainToCallback, M>`：main 侧 `try_send(&self, sys: &UpdateCtx<'_>, msg: P)`；callback 侧 `try_read(&self, cb: &CallbackCtx) -> Option<P>`。

- **能力 token**：`CallbackCtx` 是 ZST，只由目标 wrapper 在 callback 进入时发放；`UpdateCtx` 只由 driver 发放。在错误的执行域调用 `try_send` 是编译错误——"callback 只能通过统一 API 提交非阻塞 message""Update system 才能发 main-to-callback"从散文变成类型。
- **语义选择**：注册时 `ctx.route::<P>()` 返回 builder，`.latest()` 或 `.bounded::<N>()`（A4 修正）。Latest 保持现设计语义（accepted/replaced、单 receiver、下一执行边界可见）；Bounded 用 `crossbeam ArrayQueue`（Full 计数、保序、适合事件流）。跨域 payload 统一 `CallbackPayload`（`Copy` 收紧为回调侧两条方向的共同要求，保留；理由：无任意析构，20 行内可证明安全——此处的保守是合理成本）。
- 单 receiver、下一执行边界可见、不重入投递、阶段入口 watermark、`Messages<M>` 仅存在于主线程接收端——语义全部继承现设计。

### 2.6 Runtime 与 scheduler：继承清单

以下内容**原样继承** `runtime-crate.md`（它是最强的一篇，除 F1 一处外不需要重写）：

- `scsp_start` 一次性入口、参数复制前领取标记、ObservabilityRoot 先行；
- bootstrap worker 阶梯，**F1 修正**：1) image 轮询（非 IL2CPP 操作）→ 2) exact handle 上 exports 全可加载 → 3) `il2cpp_domain_get` **恰好一次**，null ⇒ bootstrap 终止（不重试）→ 4) attach + 目标 metadata 解析 → 5) runtime/layout 身份校验；
- `Handoff { Mutex<Option<Box<App>>> }` + `HandoffTake` 三态、临界区只移动 `Box<App>`；
- `AppSlot` TLS 五态 + `SchedulerFrame { context, app, phase, tls_committed }` + `OriginalPhase` 三阶段 + 兜底 Drop 泄漏保活；
- 双 gate、global failure 发布顺序（`RuntimeGate.close(Release) → failed.store(true, Release)`）、bootstrap failure vs runtime global failure 的范围划分、"不跨插件全局回滚"；
- `panic = "unwind"`、panic 不跨 FFI、plugin-system 内层 per-system 边界。

`App { world, core, plugins }`、`App: Send`（World 只收 `Send + Sync` resource、不用 non-send）、`MainThreadToken` 不进 World——继承。**删除** `AppCore::DebugState`（O1）：AppCore 只留 observability-related 与 config；driver 固定阶段收敛为：

```text
首次 LateUpdate：  run_startup（全部插件 Startup，per-plugin 提交/回滚）→ 最后开 RuntimeGate → original
后续 LateUpdate：  MessageMaintenance → CommandDrain → plugin Update systems（注册顺序）→ original
```

DebugDispatch 不再是内建阶段——它是 debug 插件的 Update system（见 §2.7），自然落在 plugin Update 区，晚一帧无语义影响。

### 2.7 Debug 降级为插件 + JSON-RPC

- v1 交付物：独立 `debug` crate 提供一个由配置门控、用**公开 API** 写的 `DebugPlugin`：`Plugin::build` 里注册 UDS transport，`DebugTopic` trait 保留 `NAME/Request/Response`，main-domain handler 注册为自身 Update system。
- wire 用 **JSON-RPC 2.0 over length-prefixed UDS**（W1）：`method = topic name`，`params = request payload`，`result/error` 直接映射 topic response / `PluginError` 映射；`id` 关联保留。删除自造 envelope、9 错误码词汇表、`ok` 字段等一整页定义（A1 的三套错误词汇随之收敛为 `PluginError` 一套 + JSON-RPC 标准码）。
- 删除项：callback-domain debug（等第一个真实需求）、topic 多版本、双执行域 pending 状态机、`AppCore::DebugState` 生命周期特例。`debug.enabled` 来自 `scsp.toml`（F3 修正）。插件退役 → 它的 update system 停跑 + route 随 owner ledger 关闭，无需专有规则。

### 2.8 Config 与 DataRoot（F3/U5 修正）

```rust,ignore
#[derive(Deserialize)] pub struct RuntimeConfig {
    #[serde(default)] pub debug: DebugConfig,       // { enabled: bool } 默认 false
}
```

- `DataRoot = <游戏容器 Documents>/shiny-song-tools/`；布局：`scsp.toml`、`translations/`、`dumps/localify.json`（translation dump 开启时）、`logs/`（v1 无 file sink 时可缺省）、`d.sock`。
- worker 在 `App::new` 前解析：缺失 ⇒ 默认值；解析失败 ⇒ 记录 observability、按全默认值、debug 强制 off（fail-closed：配置只权限能开关，永不解锁未配置的能力）。
- 子目录由各插件 build 时按需创建（worker 侧，`AnyThread` 阶段），创建失败 = 该插件 build 失败退役。

### 2.9 错误体系统一（A1/U2 修正）

```rust,ignore
#[derive(Debug, thiserror::Error)]
pub enum PluginError {
    #[error("resource conflict: {0}")]            ResourceConflict(&'static str),
    #[error("missing dependency: {0}")]           MissingDependency(&'static str),
    #[error("hook: {0}")]                         Hook(#[from] HookError),
    #[error("il2cpp: {0}")]                       Il2Cpp(#[from] Il2CppError),
    #[error("io: {0}")]                           Io(#[from] std::io::Error),
    #[error("{0}")]                               Message(&'static str),
}
pub enum RestoreError { OwnershipLost, Failed }   // 继承现设计语义
```

插件作者只见 `PluginError`/`HookError`/`RestoreError` 一条链；debug 走 JSON-RPC 标准码 + `PluginError` 映射。

### 2.10 散文不变量 → 类型事实映射表

| 现设计散文不变量 | 提案中的类型事实 |
|---|---|
| "callback context 必须先发布，replacement 才能安装" | `HookBuilder<T, Published>::install` 是唯一安装路径 |
| "Startup/Update system 不能交叉注册" | `BoxedSystem<P: PhaseInput>` 泛型 |
| "callback 只能使用 callback-safe 类型提交 message" | `try_send(&self, &CallbackCtx, ..)` + `CallbackPayload` |
| "main-to-callback 发送只能发生在 update system" | `try_send(&self, &UpdateCtx<'_>, ..)` |
| "token 只在当前 frame 有效、不能保存" | `&'a MainThreadToken`（!Send/!Sync）仅存在于 `Ctx<'a>` |
| "重复 resource 类型报错不覆盖" | facade 内置检查（World 之上唯一无法类型化的点，保留运行期检查并诚实标注） |
| "每 plugin 至多一个 container" | 注册期运行检查（跨插件同目标冲突仍只能运行期发现） |

### 2.11 与现设计的差异处置表

| 现设计内容 | 处置 |
|---|---|
| bootstrap 阶梯 / Handoff / TLS / SchedulerFrame / 双 gate / global failure / panic 边界 | **原样继承**（F1 一处修正：domain_get 单次调用） |
| MethodPointerSlot / MethodRef / `Il2CppBackend` 句柄 / attach-detach RAII | 原样继承 |
| Observability（tracing scoped Dispatch + CompactEvent + drain worker） | 原样继承，定义集中到一篇 |
| App / AppWorld / 固定 driver / owner ledger / restore action / 局部回滚 | 继承语义；**Startup staging 改为直接插入 + ledger 移除**（F2/O2），惰性 initialize |
| 双 crate（plugin-system / runtime） | 合并为 runtime |
| `Frozen<T>` | 删除（O3） |
| 四个 message facade + latest-value 唯一语义 | 收敛为方向 branded `Endpoint<P, D, M>`，`Latest`/`Bounded<N>` 双语义（O4/A4） |
| `InRef`/`InMut` 输入 tuple | 换成 SCSP phase context（A3） |
| Debug Control Plane（内建子系统、自造 envelope、双执行域） | 降级为 DebugPlugin + JSON-RPC 2.0，main-domain only（O1/W1/F3） |
| "现有 runtime 配置" | 新增 `scsp.toml` + typed `RuntimeConfig`（F3） |
| `PluginError` 缺位 / 三套错误词汇 | 统一错误链（A1/U2） |
| bevy_ecs 未 pin | 依赖表补"固定版本 bevy_ecs（0.17.x）+ 无游戏 fixture 锁定再升级"（U3） |

### 2.12 验证顺序（无游戏 fixture 门，U6 修正）

1. core 原语单测：CAS/readback、mailbox 双语义、gate 内存序（loom 可选）。
2. **bevy_ecs 集成 fixture（关键新门）**：Send-only World 跨 Handoff；惰性 initialize 解决跨阶段 resource 依赖；per-system `catch_unwind` 后 World 保活；phase 误注册编译失败（trybuild）。
3. hook typestate fixture：编译期拒绝 Unpublished.install；fake target 上 publish→install→restore→quiescence 全链（复用实验仓库 185-export fake runtime 思路）。
4. 调度全链 fixture：Handoff 竞争窗口、TLS 五态转移、global failure 顺序、panic 注入矩阵（继承 runtime-crate.md 的失败分类逐一覆盖）。
5. 上述全绿后，才进入需要批准的实机验证（按 AGENTS.md 有界批准协议，本提案不构成任何授权）。

## 非目标

- 不改变 bundle build 与 Swift FFI 入口的任何决定（它们与实验证据完全一致，包括三个窗口安全修复——实验已证明缺失会导致 SIGTRAP）。
- 不重新讨论注入路线、AppGuard 对抗、功能插件实现细节。
- 不引入 Bevy Schedule executor、entity/component gameplay model、动态 dylib 插件、热重载（维持现设计的非目标）。

## 附：落地决策记录（2026-08-29，与用户两轮需求确认）

本提案已按以下决策落进各分册（v2 修订）：

1. **落地方式**：修订现有设计分册；分册现由各 crate 顶层 Rustdoc 收录，本提案保留为决策与审查记录。
2. **产品定位**：个人使用的插件平台。v1 = 正确的插件系统 + FPS 解锁测试插件 + 配置文件 + Debug socket。翻译/贴图/相机等按同一 API 后续立项；翻译复用 SCSPTranslationData 社区格式。
3. **Debug**：插件化（DebugPlugin）+ JSON-RPC 2.0 over UDS；保留 callback 域；dispatch 走用户提议的流程——request 先落主线程（DebugPlugin Update），按 topic 分发给 owner 插件的 debug handler system（自动登记的普通 Update system，解决审查 U4），callback 域再经容器内 SharedSlot 转发，响应沿原路返回。新增运行时自省 topic（`runtime.plugins` / `runtime.gates` / `runtime.info`）。Debug socket 定位为插件开发调试工具。
4. **消息语义**：Latest / Bounded\<N\> 双语义 + `shared_latest`（`Arc<T>` 单槽）承载 callback 域调试的有主 payload。
5. **Startup 事务**：直接插入 AppWorld + owner ledger 记账、失败 LIFO 移除（放弃 staged/延迟提交，修复审查 F2）；boxed system 首次运行前惰性 initialize。
6. **插件生态**：只服务个人开发——hook 目标（ABI wrapper）由插件作者自定义（受信任边界），plugin API 不承诺 semver；不做插件脚手架/模板交付物。
7. **控制面**：配置文件（`scsp.toml`，fail-closed）+ debug socket；无 overlay GUI、无渲染/输入子系统、无第三执行域。

审查结论与提案正文中的 F1（`il2cpp_domain_get` 单次调用）、F2（staged 语义矛盾）、F3（config 悬空引用）与 C1–C5、O1–O4、U1–U6、W1、A1–A5 的处置见各 crate Rustdoc 中的 v2 内容；本提案其余章节保持原样作为依据。
