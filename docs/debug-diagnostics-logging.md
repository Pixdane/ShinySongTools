# Debug、Diagnostics 与 Logging

状态：v2 设计（2026-08-29 修订）。

- Observability（Logging + Diagnostics）：v1 薄层设计沿用，未改动语义。
- Debug：改为 **DebugPlugin**（`scsp-runtime` 内置、feature 门控的普通插件）+ **JSON-RPC 2.0** over UDS；定位为**插件开发调试工具**（含运行时自省 topic），不是终端用户控制面板。v1 交付 main 与 callback 两个执行域。

本文记录两个横跨各 crate 的系统：基于 `tracing` 的 Observability，以及可选的 Debug Control Plane。

## 系统边界

```text
Observability
  → tracing event/span
  → Apple Unified Logging

Debug Control Plane（DebugPlugin）
  → 外部 JSON-RPC request / response
  → dispatch：DebugPlugin Update → owner 插件的 debug handler system →（callback 域）callback relay
```

普通日志与诊断事实通过相同事件表达：level/target/message 用于人读日志，稳定的 `code`、`owner`、`phase`、`result` 字段用于诊断筛选。Observability 不复制 scheduler/plugin 状态机；owner 保存权威运行状态，事件只记录发生过的事实。关闭 Debug 后，正常事件、故障记录和游戏行为不能改变。

## Observability：crate 与初始化

| crate | 职责 |
|---|---|
| `tracing` | core、plugin API、runtime 共用的 event/span facade |
| `tracing-subscriber` | runtime 组装 filter 与 layers |
| `tracing-os-layer` | 从 `scsp_start` 最早阶段可用的 Apple Unified Logging layer |
| `crossbeam-queue` | callback/scheduler 热路径到 drain worker 的固定容量 compact event queue |

runtime 在 `scsp_start` 的最外层 unwind boundary 内、解析 Documents path 与构造 App 之前创建进程期 ObservabilityRoot：持有 `tracing::Dispatch`、compact event queue 与专用 drain worker，保活到进程退出。v1 只写 Apple Unified Logging，固定 subsystem `com.shinysongtools.runtime`，category `runtime`、`plugin`、`hook`、`debug`；OS layer 初始化失败退化到 stderr fmt layer。drain worker 无法启动时记录一次普通启动事件，并把 `CallbackObservability` 置为只累计 dropped/unavailable counter 的 disabled producer。Observability 初始化、worker 或 sink 错误不得使游戏启动失败。日志读取由 Console.app 或 macOS `log` 工具完成，v1 不提供自有日志读取 API。

注入 bundle 不假定自己拥有宿主进程的全局 tracing subscriber。runtime 不调用只能成功一次的 `set_global_default`，而是在每个自己控制的执行根使用同一个 scoped Dispatch：`scsp_start` body、bootstrap worker、外层 scheduler execution、plugin system 调用、drain worker。插件 system 在这些 scope 内直接使用普通 `tracing` macros。插件不允许建立长期独立 tick，因此不存在 Dispatch 继承问题。

v1 不创建文件 sink，不支持动态 reload，不依赖 DataRoot。需要持久化日志时后续另行增加独立 sink，不改变 callback producer 与事件字段。

## Observability：事件边界与 callback 通道

必须使用稳定 code 的生产事件至少包括：bootstrap 阶段与重复入口；UnityFramework/IL2CPP readiness 成功、超时和身份不匹配；SchedulerHook 与功能 Hook 安装、恢复、ownership drift；plugin build/startup/update 失败、panic、退役与 rollback 结果；RuntimeGate 关闭与 scheduler global failure；callback event queue 的 dropped count。v1 Diagnostics 的含义仅是这些结构化事件与少量原子计数；不承诺任意 live object 查询、完整内存快照、独立诊断状态机或崩溃转储。

统一的是事件身份与结构化字段，不是所有执行域用同一个 writer：

```text
普通执行域
  → tracing::event!(code, owner, phase, ...)
  → scoped Dispatch → Apple Unified Logging

callback / scheduler 热路径
  → CallbackObservability::try_emit(CompactEvent)
  → 进程级 bounded ArrayQueue
  → drain worker（自己的 scoped Dispatch）
  → tracing::event!(...)
```

callback 只提交固定大小、无 owning string/collection、无任意 plugin Drop 的 `CompactEvent`（形状见 core 分册）；v1 只允许 core/runtime 预定义事件代码，插件 callback 不注册自定义 descriptor。queue 满只增加 dropped counter，不改变 original 行为，不触发退役或 failure。drain worker 不依赖 LateUpdate、App、gate 或 Debug transport，global failure 与 App 退出后仍可记录基础设施事件。v1 不提供 dropped counter 周期性合成日志，也不把它暴露为 Debug topic。

Observability queue 与业务 route 可复用同一底层 bounded queue crate，但不共享 registration、owner gate 或生命周期：业务 message 是 owner-scoped 数据平面，Observability 是 App 之前建立、独立于 App 退出的进程级基础设施。

## Debug：定位与生命周期

- DebugPlugin 是 `scsp-runtime` 内的普通插件（feature `debug` 编译），使用公开 Plugin API + 同 crate 内部的 transport 设施；不再有 `AppCore::DebugState` 特例或内建 driver 阶段。
- 唯一启用配置为 `scsp.toml` 的 `debug.enabled`（默认 `false`）。为 `true` 时 runtime 把 DebugPlugin 注册在生产插件列表**首位**；为 `false` 时不注册、不建 socket、无任何运行时成本。
- DebugPlugin 的 build 在 worker 阶段创建 UDS listener 与 I/O worker（AnyThread 阶段允许），启动失败只使 Debug 不可用（I/O worker 记录 observability、后续 request 回 `runtime_unavailable`），不影响其它插件与游戏。transport 生命周期到进程退出为止：没有运行期停止协议；客户端感知的"服务消失"只有进程退出（连接关闭）。socket 残留由下次启动 build 时 unlink 前置清理。
- 所有 debug route 的有效条件包含 RuntimeGate 与 owner PluginGate。总 gate 关闭后不再向任何 handler 投递新 request；已 pending 的统一回复 `runtime_unavailable`。owner 退役后其 topics 的 request 回复 `plugin_unavailable`。

## Debug：wire 与 transport（JSON-RPC 2.0）

v1 transport 为本机 Unix domain socket + length-prefixed JSON-RPC 2.0，不加 HTTP/WebSocket/浏览器 bridge。唯一 socket 路径为入口 Documents 路径下 `shiny-song-tools/debug.sock`；权限 0600；v1 只接受一个客户端连接，已有连接时新连接直接关闭（不维护连接集合）。每个 frame 先 4 字节 big-endian 长度再 JSON UTF-8 bytes，长度与 frame 受固定内部上限；超限回 `payload_too_large` 并保持连接。

request（JSON-RPC 2.0）：

```json
{ "jsonrpc": "2.0", "id": "req-123", "method": "fps.set", "params": { "target": 120 } }
```

成功 response：

```json
{ "jsonrpc": "2.0", "id": "req-123", "result": { "applied": true } }
```

失败 response：

```json
{ "jsonrpc": "2.0", "id": "req-123",
  "error": { "code": -32000, "data": { "code": "plugin_unavailable", "message": "..." } } }
```

错误映射：

| 情形 | JSON-RPC code | data.code |
|---|---|---|
| JSON 解析失败 | -32700 parse error | — |
| envelope 字段不合法、缺 id、带 method 无 id（notification 不支持）、batch 不支持、version 不符 | -32600 invalid request | — |
| 未知 method（topic 不存在） | -32601 method not found | — |
| params 不符合该 topic 的 typed schema | -32602 invalid params | — |
| runtime 不可用 / 队列满 / payload 超限 / 插件不可用 / handler 业务错误 / 内部错误 | -32000 server error | `runtime_unavailable` / `queue_full` / `payload_too_large` / `plugin_unavailable` / `handler_error` / `internal_error` |

无法关联 request 的协议错误用 `id: null` 回复，不进入 pending；这些协议错误保持连接并继续服务后续 frame。I/O worker 处理顺序：长度上限 → 读完整 frame → JSON 解析 → envelope/id 校验 → topic 查找 → typed params 反序列化 → pending 容量 → 投递 DebugPlugin。任一步失败都不触达插件。

## Debug：typed topic 与 dispatch 流

进程内部使用强类型 topic；wire 层映射到稳定 method 名：

```rust
trait DebugTopic: 'static {
    const NAME: &'static str;                 // wire method，如 "fps.set"
    type Request: serde::de::DeserializeOwned + Send + 'static;
    type Response: serde::Serialize + Send + 'static;
}
```

插件在 build 中 `register_main_debug::<T>()` 或 `register_callback_debug::<T>()`。注册自动：向 AppCore topic registry 登记（name → owner id、执行域、decode/encode vtable、owner gate readers）；创建本 topic 专属 mailbox（callback 域为 `shared_latest` 语义的 request/response 两条跨域 route，句柄交本插件 CallbackSiteContainer）；把 handler/relay system 自动登记为本插件 Update system。一个 topic 一个 owner、一个 handler、一个执行域；重名注册使当前插件 build 失败。

**统一 dispatch 流**（两个执行域共用同一骨架，request 先落地主线程）：

```text
Debugger ⇄ UDS (JSON-RPC)
  ⇄ Debug I/O worker（framing、typed 解码为 Arc<Request>、pending + correlation 归 DebugPlugin）

帧 N：DebugPlugin Update system
  → 查 registry → RuntimeGate + owner PluginGate 检查
  → 经主线程 inbox 把 Arc<Request> 投递给 owner 的 debug handler system

[main 域]
  帧 N（同帧，owner 在 DebugPlugin 之后）或帧 N+1：owner debug handler system
    → 用自己的 World resources 执行
    → 响应写回 DebugPlugin 响应 inbox
  帧 N+1：DebugPlugin → correlation → wire

[callback 域]
  owner debug handler system → Arc<Request> 写入本插件容器中的 request SharedSlot
  owner callback 自然进入 → try_read(&CallbackCtx) → handler（有界分配允许，不阻塞、不解析 wire）
    → Arc<Response> 写入容器中的 response slot（callback→main）
  owner debug handler system（下一帧）→ 读到响应 → 转交 DebugPlugin → wire
```

- pending/correlation 状态归 DebugPlugin（主线程）：固定内部最大 pending 数；新 request 不覆盖旧 request，无 slot 回 `queue_full`；同一连接内 active `id` 必须唯一，重复回 `invalid_request`，response 写回后 id 可复用。
- callback 域预算：每次对应 Hook 自然进入最多处理一个 request；Hook 长时间不触发时 request 保持 pending，直到执行、owner route 禁用或 gate 关闭。v1 不支持客户端取消或 per-request deadline。
- 客户端断开：释放尚未开始执行的 request；已投递到插件的不强行取消；响应到达时 correlation 已释放则丢弃并记 observability。
- handler 返回已声明业务错误只回复当前 request 的 `handler_error`；handler panic 记 observability、当前 request 回 `plugin_unavailable`，并按 owner-local 规则禁用该 owner 的 debug routes，不影响其它插件。
- main/callback handler 都不执行 socket I/O；callback handler 不解析 wire、不阻塞等待、允许有界分配（构造 typed response）。
- topic 不得提供任意地址读写、任意 IL2CPP 调用或绕过 core capability 的操作；所有外部可执行行为都必须对应显式注册并接受 owner 管理的 typed topic。

延迟口径：main 域整链约 2 帧，callback 域约 2–3 帧；不承诺与 I/O 接收同帧响应。对开发调试用途足够。

## Debug：运行时自省 topic（v1 内置）

DebugPlugin 自带只读自省 topic（main 域），数据取自 `PluginInventory` 与原子计数（driver 在状态迁移时更新快照）：

- `runtime.plugins` → 插件列表：id、state（`active` / `retired`）、gate 开关、startup/update system 数、restore action 数、是否有 container、注册的 topic 名单。
- `runtime.gates` → RuntimeGate 与各 plugin gate 当前值。
- `runtime.info` → 版本、uptime、帧计数、readiness 各阶梯结果、config 非敏感摘要、observability dropped 计数、各 route mailbox 深度。

自省只读、不触发任何迁移；失败原因等细节仍以 observability 事件为准（`retired` 不携带原因字段，归因看 Unified Logging）。

## Debug：crate 集成边界

| 层 | Debug 职责 |
|---|---|
| core | transport-neutral envelope 之外的公共件：typed mailbox 原语（`SharedSlot`）、`CompactEvent`；不持有 plugin route |
| plugin API | `DebugTopic` trait 与 `register_main_debug` / `register_callback_debug`；自动登记 handler/relay system |
| runtime（App/driver） | AppCore 持有 topic registry 与 PluginInventory；debug handler/relay 是普通 Update system，无常设阶段 |
| runtime（DebugPlugin） | UDS transport、JSON-RPC framing、pending/correlation、dispatch、自省 topic |

## 待打磨与待设计汇总

Observability v1 沿用已收敛边界：统一事件字段、固定容量队列、drain worker、仅 Unified Logging（失败退化 stderr）、固定 subsystem/category、无文件 sink/动态 reload/插件自定义 callback 事件/Debug 订阅。

Debug v2 已收敛：DebugPlugin 插件化；JSON-RPC 2.0 wire 与标准错误码映射；request 先落主线程、经 owner debug handler system 分发、callback 域经容器内 SharedSlot relay 的统一 dispatch 流；pending bounded + correlation 归 DebugPlugin；单连接、0600、unlink 前置清理；三个内置自省 topic。待打磨：自省字段集合、mailbox 容量常量、`runtime.info` 的 config 摘要边界。待设计：事件订阅/live snapshot（出现需求时另立 change）、worker 运行期停止协议（当前生命周期=进程）。
