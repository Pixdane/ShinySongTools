# 运行时架构总览

状态：v1 设计已收敛；具体 Rust 类型和物理模块可在实现时调整

本文是 Shiny Song Tools 进程内架构的索引，只定义 crate 依赖方向、文档职责和跨层不变量。各系统的详细设计以对应分册为准。

## 文档与 crate

| 文档 | 概念 crate | 权威范围 |
|---|---|---|
| [Core crate 设计](core-crate.md) | `scsp-core` | 平台基础 API、共享设施、IL2CPP handle、callback-safe primitives、MethodPointer slot |
| [Plugin API 设计](plugin-api.md) | `scsp-plugin-api` | 功能插件作者可见的 Plugin、Resource、System、Callback 和跨系统注册边界 |
| [Plugin system 设计](plugin-system.md) | `scsp-plugin-system` | App、PluginManager、共享 `bevy_ecs::World`、system driver、plugin callback site container、owner/effect 和逻辑退役 |
| [Runtime crate 设计](runtime-crate.md) | `scsp-runtime` | `scsp_start`、bootstrap、生产组装、LateUpdate scheduler、Handoff、TLS 和 runtime 级失败 |
| [Debug、Diagnostics 与 Logging](debug-diagnostics-logging.md) | 跨 crate | Debug Control Plane 与薄 Observability 层 |

这些 crate 尚未在 Cargo workspace 中创建；名称和物理拆包仍属于实现阶段，本文当前定义的是依赖和责任边界。

## 依赖方向

```text
scsp-core
  ↑
scsp-plugin-api
  ↑
scsp-plugin-system
  ↑
scsp-runtime
```

功能插件依赖 `scsp-plugin-api` 和必要的 core 类型，不依赖 plugin-system 内部实现或 runtime。低层 crate 不得反向引用 App、PluginManager、scheduler TLS 或具体功能插件。

主要第三方 crate 的采用边界为：

| 能力 | 采用 | SCSP 仍负责 |
|---|---|---|
| plugin resource/system | `bevy_ecs` | 共享 AppWorld、固定顺序 driver、owner/gate/rollback 和逐 system panic boundary |
| structured observability | `tracing`、`tracing-subscriber`、`tracing-os-layer` | 早期初始化、scoped dispatch、Apple Unified Logging、稳定事件码和 callback 延迟上报 |
| bounded queue | 默认 `crossbeam-queue::ArrayQueue`；仅在已证明 SPSC 时用 `rtrb` | 容量、满载语义、单帧预算和固定大小 `Copy` payload 约束 |
| error / wire data | `thiserror`、`serde`、`serde_json` | 领域错误、topic version 和可观测字段 |
| IL2CPP backend | 固定版本的 `il2cpp-bridge-rs` | exact UnityFramework handle、readiness 和主线程 capability |

这里采用的是把 `bevy_ecs` 嵌入现有宿主循环的模式：SCSP 自己拥有 App、一个共享的 AppWorld 和 LateUpdate driver，只复用 World、Resource、SystemParam 与 System；不把控制流交给 `bevy_app::App` 或 Bevy runner。该取舍与 [learn-wgpu 的“在已有程序中集成 Bevy ECS”示例](https://jinleili.github.io/learn-wgpu-zh/integration-and-debugging/bevy/ecs)一致。

## 生产调用链

```text
PlayTools AKPlugin
  → scsp_start
  → bootstrap worker
  → core facilities + App
  → Plugin.build
  → 功能 Hook（gate 关闭）
  → LateUpdate SchedulerHook
  → Handoff<App>
  → Unity 主线程 TLS
  → Startup/Update driver
```

外部 PlayTools carrier 与内部 Rust feature plugins 是两个不同层次。第一版不设计动态 dylib 插件发现、热加载、entity/component gameplay model 或多线程 system scheduler；只复用 `bevy_ecs` 的 resource/system 基础件。

`scsp_start` 是进程期一次性入口：重复调用只记录 observability 并返回，不创建第二个 bootstrap，也不重试已失败的启动。bootstrap 只有在 `Handoff.publish(App)` 成功后才完成；此前失败按 bootstrap failure 处理，关闭本次 gate、逆序尝试恢复已安装的功能 effects、停止已启动的 Debug worker，并丢弃未交接的 App。Handoff 成功后的 scheduler、App 或 plugin-system 基础设施故障才是 runtime global failure；它关闭总 gate、停止业务 driver 并保活 callback 可达对象，不执行跨插件 global rollback。

## 跨层不变量

- exact UnityFramework handle 和 IL2CPP backend 属于 core/runtime 边界；不得假定 `RTLD_DEFAULT` 可见性。
- App 是唯一组合根，从 worker 到主线程始终为同一个 `Send` 类型；Handoff 后由主线程 TLS 独占。
- Unity 主线程敏感安全 API 必须要求 runtime 在当前 scheduler frame 通过 `pthread_main_np()` 后创建的短生命周期 `MainThreadToken`；Swift main queue 不是 token 来源。
- Plugin 是 App 配置器；typed resources 保存状态，Startup/Update systems 保存主线程行为，不保留长期 `PluginRuntime` trait object。AppCore 持有非插件组合状态，包括 DebugState、DebugDispatch 和 CommandDrain；不引入独立常驻 Runtime 或 DebugRoot。
- 插件运行状态和普通资源统一放在共享 AppWorld；PluginManager 只记录注册 owner、system、gate 和需要恢复的外部变更。Update 不能追加恢复记录。
- 所有 plugin system 都在同一个共享 AppWorld 上顺序执行；资源可以被显式共享。同一资源类型只有一份，需要独立状态时由插件使用自己的 newtype。
- v1 driver 只使用固定基础阶段和插件/system 注册顺序，不构建 before/after 依赖图，也不采用 Bevy Schedule executor；MessageMaintenance 维护 Bevy buffer，CommandDrain 以阶段入口 watermark 限定跨域 message 的单帧工作。
- 所有功能 callback 和 plugin debug route 都必须同时通过进程级 RuntimeGate 与所属 PluginGate；global failure 首先关闭 RuntimeGate。
- 普通 Hook callback 不访问 AppWorld 或主线程 TLS，只访问所属 plugin 注册的明确类型 CallbackSiteContainer 和目标专用 CallbackSite；每个目标由唯一静态 OnceLock 定位并保活 site 到进程退出。
- callback 修改主线程状态时只通过统一 API 提交非阻塞、bounded message，由下一次外层 LateUpdate 处理。
- 包含 typed original 的 callback context 必须先发布，MethodPointer replacement 才能安装。
- bootstrap 必须在任何 MethodPointer 写入前完成 exact UnityFramework、IL2CPP domain/worker attach、scheduler metadata 和 runtime/layout readiness；所有等待都有总 deadline。
- `scsp_start` 的一次性标记在参数复制前领取；ObservabilityRoot 进程期只创建一次，重复入口不覆盖首个调用的路径、不创建第二个 worker，也不自动重试 bootstrap。
- Handoff 前的失败只回滚本次 bootstrap 已登记的功能 effects；Handoff 成功后的 runtime global failure 不调用各 plugin restore ledger、不等待未来 callback 认领回滚，也不要求卸载 SchedulerHook。
- MethodPointer 安装和恢复必须使用 CAS、readback 和 ownership-aware 规则；发现未知 owner 时不盲写。
- scheduler callback 用栈上 SchedulerFrame 记录 original 前/调用中/返回后三阶段并守护 App；Rust 失败路径仍须保证 original LateUpdate 恰好调用一次，Rust panic 不得跨越 FFI。
- scheduler global failure 只关闭 RuntimeGate、停止 App/plugin 业务逻辑并保活仍可能被 callback 访问的对象；不等待未来 callback，也不执行跨插件全局回滚。错误线程只调用 original。
- 每个 boxed plugin system 的 `System::run` 具有 owner-scoped panic boundary；插件失败只逻辑退役和回滚所属 effect，并继续其它 owner 的 driver。在 callback quiescence 协议完成前不物理释放可能仍可达的 context。
- Observability 在最外层启动保护中尽早建立，使用一个 runtime-owned `tracing::Dispatch` 覆盖所有受控执行根；callback/hot scheduler 路径只向独立于 App/RuntimeGate 的进程级 queue 提交固定大小 `CompactEvent`，专用 drain worker 再执行 tracing、格式化和 Apple Unified Logging 输出；v1 不创建 file sink。
- Debug I/O worker 由 AppCore::DebugState 持有，只投递消息。main handler 等下一次 LateUpdate；callback handler 等对应 Hook 自然进入，不人工唤起。Debug 仅由现有 runtime 配置的 `debug.enabled` 开启，默认关闭；request/response 使用 bounded pending-request 通道，不采用业务 latest-value 覆盖语义；v1 payload 只支持标准 JSON/serde，wire frame/payload 超过固定内部上限时由 I/O worker 返回 `payload_too_large`，不进入 App、plugin 或 callback；wire 层提供后端友好的统一 JSON envelope。App 退出时停止 Debug worker；worker 停止时所有未完成 pending request 统一回复 `runtime_unavailable`，然后关闭 transport 并尝试删除 `debug.sock`；删除失败只记录 observability。完整设计状态见 [Debug、Diagnostics 与 Logging](debug-diagnostics-logging.md)。

## 证据边界

设计可以采用实验仓库已经建立的可行性结论，但实验结果、fixture 结果、生产实现和当前游戏版本实机结果必须分别陈述，任何一层不得自动外推为下一层已经成立。

当前实验已经为精确版本提供 MethodPointer replacement、callback/original/restore 和 exact-handle IL2CPP 加载的设计依据，但本生产 workspace 目前仍处于文档设计阶段。

## 非目标

- Bundle 构建、签名、安装和恢复事务，见 [Bundle 编译流程](bundle-build.md)。
- Swift `AKPlugin` 的入口行为，见 [Swift 入口行为](swift-entry.md)。
- Frida attach、load-time interpose 或其它备选注入路线。
- 某个游戏版本的 SHA、地址或实验批次流水。
- FPS、翻译、相机、Live MV、纹理替换等具体功能实现。
- 游戏修改、启动、attach、sample 或实机验证授权。

## 当前待打磨与待设计

待打磨：`bevy_ecs` 与 tracing 的具体物理模块、plugin owner/effect 的内部表示、CallbackSiteContainer 的具体 site handle、IL2CPP capability 方法集合，以及 observability 的具体事件字段和队列容量。resource 注册顺序、StartupRegistrar 事务、boxed system phase adapter、跨执行域 endpoint 语义、Observability v1 的 Unified Logging 边界和 Debug v1 的 topic/transport/pending/shutdown 语义均已在对应设计中收敛。每个 plugin 至多一个 container，且注册后不可替换或注销已确认；v1 只提供 `Frozen<T>`，不设计可更新 callback snapshot；callback endpoint message 限定为固定大小的 `Copy + Send + Sync + 'static'` 类型，并采用单 receiver 的 latest-value MPSC mailbox，新值覆盖旧值，不提供 FIFO、竞争消费或 broadcast；跨域 message 在下一执行边界可见，不做重入投递或主动唤醒。

待设计：plugin/callback 物理卸载、scheduler quiescence，以及超出薄事件层的高级诊断查询和 crash artifact。
