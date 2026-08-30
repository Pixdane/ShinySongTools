# Runtime crate

状态：v2 设计（2026-08-29 修订）。本版依据本文后附的“架构审查与类型驱动重设计记录”及用户决策修订。产品定位：**个人使用的插件平台**——v1 交付正确的插件系统、一个 FPS 解锁测试插件、配置文件与面向插件开发的 Debug socket（含运行时自省 topic）；翻译、贴图等功能插件按同一 API 后续逐个立项。

本文是进程内架构的索引，只定义 crate 依赖方向、文档职责和跨层不变量。各系统详细设计以对应分册为准；**跨层不变量以本档为唯一权威清单**，分册只引用、不重复定义。

## 与实验仓库设计的关系

本文档体系取代 `~/Documents/scsp-playcover-hook/research/03-hook-runtime-design.md` 中的进程内架构决定，完整保留其证据边界。主要取代点：

| 实验 repo research/03 | 本设计 | 理由 |
|---|---|---|
| RuntimeKernel（OnceLock）+ thread_local ScspApp | `App` 组合根 + `Handoff` + 主线程 TLS 五态 | 所有权显式化，杜绝后台线程持有游戏状态 |
| PreUpdate/PostUpdate 双阶段 | 单阶段固定 driver（Startup 一次 + Update 每帧） | 少一个阶段边界，行为等价 |
| 全局失败由 worker 兜底回滚全部插件 | 全局失败不回滚，只关总 gate + 保活可达对象 | Handoff 后 worker 已退出，回滚不可达 |
| 三级 severity 状态机 | 双 gate（RuntimeGate/PluginGate）+ owner 局部回滚 | 门是 O(1) 广播，状态机是维护负担 |
| 守护进程式命令队列 | 跨域 message route（类型化 endpoint） | 类型化 + 能力 token 取代运行期分流规则 |

实验事实是生产设计的锚，不得反向弱化：MethodPointer 单 slot CAS/readback、exactly-once original、`il2cpp_domain_get` 恰好一次调用、exact-handle IL2CPP 加载、AKPlugin 三个窗口安全修复。

## 文档与 crate

| Rustdoc | crate | 权威范围 |
|---|---|---|
| Core crate + Plugin API | `corelib` | 平台基础 API、MainThreadToken、gate、MethodPointer 封装、plugin facade、callback-safe 原语、CompactEvent |
| App 与 driver + Bootstrap 与 scheduler + Swift FFI | `shiny_song_tools` | App、PluginManager、owner scope、`scsp_start`、readiness、Handoff、TLS、global failure |
| Unlock FPS plugin | `unlock_fps` | Unity 两个静态 setter hook、main→callback latest route、`unlock_fps.get`/`unlock_fps.set` |
| Debug control plane 与 Observability | `debug` | JSON-RPC over UDS、dispatch、pending/correlation、自省 topic；runtime 负责 observability root |

生产 crate 目前为四个：`core`、`debug`、`unlock_fps`、`runtime`（另有 `crates/testing/fake-unity-framework` 作为无游戏 cdylib fixture，不属于生产依赖图）。原 plugin-system 职责并入 `runtime`；plugin API facade 位于 `core::plugin_api`，功能插件按职责独立成 crate。

## 依赖方向

```text
core
  ↑
core::plugin_api   ←—— 功能插件 API facade
  ↑                    ↑
unlock_fps             debug（DebugPlugin / transport）
  ↑                    ↑
runtime（App/driver + bootstrap/scheduler）
```

## 第三方采用边界

| 能力 | 采用（固定版本） | SCSP 仍负责 |
|---|---|---|
| plugin resource/system | `bevy_ecs`（0.19.x 固定，升级需重跑无游戏 fixture） | 共享 AppWorld、固定顺序 driver、owner/gate/rollback、逐 system panic 边界、惰性初始化 |
| IL2CPP backend | `il2cpp-bridge-rs`（0.1.4 固定） | exact UnityFramework handle、readiness 阶梯、attach 生命周期 |
| structured observability | `tracing`、`tracing-subscriber`、`tracing-os-layer` | 早期初始化、scoped dispatch、Unified Logging、稳定事件码、CompactEvent 队列 |
| bounded queue | `crossbeam-queue::ArrayQueue` | Bounded mailbox、Observability 队列、满载语义 |
| error / wire | `thiserror`、`serde`、`serde_json` | `PluginError` 链、JSON-RPC wire |

这里采用把 `bevy_ecs` 嵌入现有宿主循环的模式：SCSP 自己拥有 App、一个共享 AppWorld 和 LateUpdate driver，只复用 World、Resource、SystemParam 与 System；不把控制流交给 `bevy_app::App` 或 Bevy runner。

采用 0.19.x 的依据：嵌入面只覆盖 World/Resource/SystemParam/System/Messages；0.19 的 `SystemParam::get_param` 签名变更（返回 `Result<_, SystemParamValidationError>`）只影响 SCSP 自有的 boxed adapter 层；`Messages<M>`/`Message` derive 自 0.17 起稳定；0.19 新增的 immutable resource 用于表达只读共享契约（具体标注形式实现时核对）。

## 生产调用链

```text
PlayTools AKPlugin
  → scsp_start
  → bootstrap worker
  → core facilities + App
  → Plugin.build（DebugPlugin 仅在 debug.enabled 时条件注册，注册于生产插件列表首位）
  → 功能 Hook（typestate 发布 site 后安装，gate 关闭）
  → LateUpdate SchedulerHook
  → Handoff<App>
  → Unity 主线程 TLS
  → Startup driver（首帧）/ Update driver（后续帧）
```

`scsp_start` 是进程期一次性入口：重复调用只记录 observability 并返回。bootstrap 只有在 `Handoff.publish(App)` 成功后才完成；此前失败按 bootstrap failure 处理，关闭本次 gate、逆序执行已登记的 restore actions、丢弃未交接的 App。Handoff 成功后的基础设施故障才是 runtime global failure：关闭总 gate、停止业务 driver、保活可达对象，不执行跨插件回滚。

## 跨层不变量（唯一权威清单）

- exact UnityFramework handle 与 IL2CPP API 表属于 core/runtime 边界；不得假定 `RTLD_DEFAULT` 可见性。
- App 是唯一组合根，从 worker 到主线程始终为同一个 `Send` 类型；Handoff 后由主线程 TLS 独占。AppWorld 只接受 `Send + Sync + 'static` resource。
- `MainThreadToken` 是 `!Send + !Sync` 的主线程 capability，由 runtime 在每个 scheduler frame 经 `pthread_main_np()` 校验后构造，只以 `StartupCtx<'_>`/`UpdateCtx<'_>` 的短借用进入 system；不进 AppWorld，不缓存。
- Startup 与 Update 是编译期区分的 phase：boxed system 按 `PhaseInput` 参数化，跨 phase 注册是编译错误。每个 boxed system 在首次运行前惰性 `System::initialize`。
- resource 由 build facade / Startup system **直接插入**共享 AppWorld，owner ledger 记录 (类型, 顺序)；重复类型返回 `PluginError::ResourceConflict`，不覆盖。Build/Startup 失败由 ledger LIFO 移除该 owner 资源并逆序执行 restore actions；Update 失败退役不移除资源（避免连锁），只执行 restore actions。依赖被移除资源的插件在 param validation 时失败退役。
- 插件运行状态统一放在共享 AppWorld 的 typed resource 中；PluginManager 只记录 owner、system、gate、ledger 与 route/container 句柄。
- 所有 plugin system 在同一共享 AppWorld 上顺序执行；同一资源类型只有一份。v1 不构建 before/after 依赖图，不使用 Bevy Schedule executor。
- Hook 安装走 typestate：`HookBuilder<T, Published>::install` 是唯一安装路径，CallbackSite（typed original + 双 gate reader + 容器 Arc）先发布到目标唯一静态 `OnceLock` 再 CAS 安装；site 保活到进程退出，不替换、不注销。hook 目标（ABI wrapper）由插件作者在同仓库内自定义并审阅——个人使用、受信任边界，plugin API 不承诺跨版本稳定。
- callback 不访问 App、AppWorld、PluginManager 或主线程 TLS；只使用所属 CallbackSiteContainer 的字段与注入的 `&CallbackCtx` 能力。callback 修改主线程状态只经跨域 route 提交，由下一次外层 LateUpdate 处理。
- 跨域 route 的 mailbox 语义在注册时按类型选择：`latest`（覆盖）/ `bounded::<N>`（保序 FIFO）/ `shared_latest`（`Arc<T>` 单槽，承载有主结构化数据）。callback 侧 endpoint 操作要求 `&CallbackCtx`，main 侧要求 `&UpdateCtx<'_>`；`latest`/`bounded` 的 payload 满足 `CallbackPayload: Copy + Send + Sync + 'static`，`shared_latest` 的 `T: Send + Sync + 'static` 为无副作用 Drop 的普通数据。
- 所有功能 callback 与 plugin debug route 必须同时通过 RuntimeGate 与所属 PluginGate；global failure 首先关闭 RuntimeGate（Release），之后观察到关闭的 callback 只调 typed original。
- bootstrap readiness 阶梯中，跨过 image/exports gate 后的 `il2cpp_domain_get` **探测恰好一次**；返回 null 即本次一次性 bootstrap 终止，不轮询重试（实验定案，见 runtime-crate 分册）。gate 之后的元数据查询链中，bridge crate 内部（cache hydration 等）会重读 domain_get——该重读已被两次实机 A/B 实证无害，且由无游戏 fixture（bridge_fake_happy）固化调用模式；本条约束的是探测不轮询，不是全进程调用次数。
- `panic = "unwind"`；Rust panic 不跨 FFI。每个 boxed system 有 owner-scoped panic boundary；scheduler 热路径有 `SchedulerFrame`/`OriginalPhase` 三阶段守护，original 恰好调用一次。
- Observability 在最外层启动保护中尽早建立：runtime-owned scoped `tracing::Dispatch` 覆盖所有受控执行根；callback/scheduler 热路径只提交固定大小 `CompactEvent` 到进程级队列，由独立 drain worker 输出。v1 只输出 Apple Unified Logging。
- 配置唯一来源为 `DataRoot/shiny-song-tools/scsp.toml`（typed `RuntimeConfig`）；缺失时自动创建空的 fail-closed 配置并使用默认值，解析失败仍 fail-closed（全默认值、debug 强制关闭）。`debug.enabled` 为真时注册 DebugPlugin（JSON-RPC 2.0 over UDS；dispatch 走"DebugPlugin → owner handler system → callback relay"，见 debug 分册），否则不注册、不建 socket。

## 证据边界

设计可以采用实验仓库已经建立的可行性结论，但实验结果、fixture 结果、生产实现和当前游戏版本实机结果必须分别陈述，任何一层不得自动外推为下一层已经成立。当前实验已为精确版本提供 MethodPointer replacement、callback/original/restore 和 exact-handle IL2CPP 加载的设计依据；生产 workspace 已实现类型驱动 v2 代码骨架与独立 `unlock_fps` crate，**FPS 实机验证未开始**——当前只完成静态 target identity、ABI 声明和无游戏编译/单元测试，不能外推为当前游戏版本已生效。

## 非目标

- Bundle 构建、签名、安装和恢复事务属于仓库根 `docs/bundle-build.md`，不属于 Rust API。
- Swift `AKPlugin` 的入口行为见本 crate Rustdoc 的“Swift FFI 入口”。
- Frida attach、load-time interpose 或其它备选注入路线。
- 某个游戏版本的 SHA、地址或实验批次流水。
- 翻译、贴图、身体参数、Live MV 等具体功能插件（v1 只有 FPS 解锁测试插件；翻译后续立项并复用 SCSPTranslationData 社区格式）。
- 游戏内 overlay GUI、全局热键、输入或渲染子系统（控制面 = 配置文件 + debug socket，两个执行域）。
- 游戏修改、启动、attach、sample 或实机验证授权。
- 动态 dylib 插件发现、热加载、entity/component gameplay model、多线程 system executor。

## 当前待打磨与待设计

待打磨：exact UnityFramework 的 image identity 格式（当前实现仅匹配文件名，身份校验过弱）、`runtime.info` 的 readiness 阶梯结果字段、per-category os_log 句柄（现单 category + target 区分）、生产 DataRoot 下 d.sock 的 `SUN_LEN` 路径长度上限（容器 bundle-id 过长时 bind 会失败，需设计短路径方案）、DebugPlugin 自省外的 request 生命周期压测。已收敛并实现：四 crate 物理布局、phase 类型、route 三种 mailbox（`LatestCell`/`BoundedQueue`/`SharedSlot`）、hook typestate（slot 事实来源 dispatch）、`define_hook_site!` 宏、owner ledger（`ResourceLedgerEntry`）、Debug dispatch 流、错误体系、config fail-closed、readiness 阶梯 1 轮询参数（`IMAGE_POLL_*`）、compact 事件字段、独立 `unlock_fps` 与 `debug` 插件。

待设计：plugin/callback 物理卸载、scheduler quiescence、FPS target 的当前游戏版本实机验证与 restore 证据、翻译插件立项（社区格式兼容 + callback-safe 快照替换协议）、超出薄事件层的高级诊断与 crash artifact。
