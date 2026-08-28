# Debug、Diagnostics 与 Logging

状态：

- Debug Control Plane：v1 设计已收敛，具体物理模块与内部常量可在实现时调整。
- Observability（Logging + Diagnostics）：v1 薄层设计已收敛。

本文记录两个横跨 core、plugin API、plugin system 和 runtime 的系统：基于 `tracing` 的 Observability，以及可选的 Debug Control Plane。

## 系统边界

Logging 与 Diagnostics 不再建立两套 backend、handle 或事件模型，而是统一为 `tracing` event/span：

```text
Observability
  → tracing event/span
  → Apple Unified Logging

Debug Control Plane
  → 外部双向 request / response
```

普通日志与诊断事实通过相同事件表达：level/target/message 用于人读日志，稳定的 `code`、`owner`、`phase` 和结果字段用于诊断筛选。Observability 不复制 scheduler/plugin 状态机；owner 仍保存权威运行状态，事件只记录发生过的事实。

v1 Debug 只提供显式 request/response，不订阅 Observability，也不提供 live snapshot 或事件流；Observability 独立工作。关闭 Debug 后，正常事件、故障记录和游戏行为不能改变。

## Observability：crate 与初始化

crate 分工固定为：

| crate | 职责 |
|---|---|
| `tracing` | core、plugin API、plugin system 与 runtime 共用的 event/span facade |
| `tracing-subscriber` | runtime 组装 filter 与 layers |
| `tracing-os-layer` | 从 `scsp_start` 最早阶段可用的 Apple Unified Logging layer |
| `crossbeam-queue` | callback/scheduler 热路径到 Observability worker 的固定容量 compact event queue |

runtime 在 `scsp_start` 的最外层 unwind boundary 内、解析 Documents path 和构造 App 之前创建进程期 Observability root。它持有 `tracing::Dispatch`、compact event queue 和专用 drain worker，并保活到进程退出。v1 只写入 Apple Unified Logging，固定 subsystem 为 `com.shinysongtools.runtime`，category 使用 `runtime`、`plugin`、`hook`、`debug`；OS layer 初始化失败时退化到 stderr fmt layer。drain worker 无法启动时记录一次普通启动事件，并把 `CallbackObservability` 置为只累计 dropped/unavailable counter 的 disabled producer。Observability 初始化、worker 或 sink 错误不得使游戏启动失败。日志读取由 Console.app 或 macOS `log` 工具完成，v1 不提供自有日志读取 API。

注入 bundle 不假定自己拥有宿主进程的全局 tracing subscriber。runtime 不调用只能成功一次的 `set_global_default`，而是在每个自己控制的执行根使用同一个 scoped Dispatch：`scsp_start` body、bootstrap worker、外层 scheduler execution、plugin system 调用、Observability drain worker 和已由 AppCore::DebugState 启动的 Debug I/O worker。插件 system 在这些 scope 内使用普通 `tracing` macros，不需要持有自定义 logger handle。插件自行创建的线程不会自动继承 Dispatch；v1 本来也不允许插件建立长期独立 tick。

v1 不创建文件 sink，不支持动态 reload，也不依赖 DataRoot。需要持久化日志时，后续另行增加独立 sink，不改变 callback producer 和现有事件字段。

## Observability：事件边界

必须使用稳定 code 的生产事件至少包括：

- bootstrap 阶段与重复入口。
- UnityFramework/IL2CPP readiness 成功、超时和版本不匹配。
- SchedulerHook 与功能 Hook 安装、恢复、ownership drift。
- plugin build/startup/update 失败、panic、退役与 rollback 结果。
- RuntimeGate 关闭和 scheduler global failure。
- tracing/callback event queue 的 dropped count。

v1 Diagnostics 的含义仅是这些结构化事件及少量由现有原子直接读取的 dropped counter；不承诺任意 live object 查询、完整内存快照、独立诊断状态机或崩溃转储。

## Observability：统一事件 API 与 callback 边界

统一的是事件身份和结构化字段，不是所有执行域使用同一个具体 writer：

```text
普通执行域
  → tracing::event!(code, owner, phase, ...)
  → scoped Dispatch

callback / scheduler 热路径
  → CallbackObservability::try_emit(CompactEvent)
  → process-wide bounded queue
  → Observability drain worker
  → tracing::event!(code, owner, phase, ...)
```

普通 plugin system、bootstrap 和 Debug worker 直接使用 `tracing` macros。普通 `tracing` event 进入当前 scoped Dispatch 并写入 Apple Unified Logging；普通功能 callback 和 outer scheduler 的关键热路径不得直接调用通用 tracing facade、formatter、OS logging layer 或文件 writer。

callback 只提交固定大小、无 owning string/collection、无任意 plugin Drop 的 `CompactEvent`。v1 只允许 core/runtime 预先定义的事件代码；插件 callback 不注册自定义 descriptor，也不携带动态字符串或插件 payload。概念形状为：

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

具体位宽使用固定内部常量，`CompactEvent` 的构造、入队失败和析构都不分配、不阻塞、不调用插件代码。底层使用进程级 `crossbeam_queue::ArrayQueue`；满载时只增加原子 dropped counter 并返回，不改变 original 行为，不触发插件退役或 runtime failure。drain worker 持续取出记录，在自己的 scoped Dispatch 中根据固定事件代码转换为正常 tracing event；它不依赖 LateUpdate、App、PluginGate、RuntimeGate 或 Debug transport，因此 global failure 和 App 退出后仍可继续记录基础设施事件。v1 不提供 dropped counter 周期性合成日志，也不把它暴露为 Debug topic；需要时由内部 diagnostics 直接读取。

core/runtime 的 compact code 来自固定生产表。事件至少携带 code、level、owner、phase 和两个无符号标量参数；target 使用固定的 `com.shinysongtools.runtime` subsystem 及 core/runtime category。插件普通执行域可以使用统一 `tracing` event，但 v1 不提供插件 callback 自定义事件注册。

Observability queue 与 plugin typed Message route 可以复用相同的底层 bounded queue crate，但不共享 registration、owner gate 或生命周期。业务 message 是 owner-scoped 数据平面；Observability 是在 App 之前建立并独立于 App 退出的进程级基础设施。

## Debug：执行域与自然调度

Unix domain socket 的 I/O worker 不直接调用 App system、插件业务逻辑或游戏 Hook callback。已启用 Debug 时，worker、DebugHub 和 pending request 状态由 AppCore::DebugState 持有；DebugState 只在 App 创建和 plugin build 处理完成、App 仍可继续运行后启动 worker。某个 plugin build 局部失败不阻止其它 plugin 或 Debug 启动。worker 或 socket 启动失败时，DebugState 进入 `Unavailable`，不重试，后续 request 返回 `runtime_unavailable`；App 只通过窄 facade 在 `DebugDispatch` 阶段消费主线程请求。DebugHub 只把已经完成 wire 解码的 owned typed request 投递到目标执行域：

```text
Debugger
  ↕ transport
Debug I/O worker
  ↕ DebugHub
  ├─ MainInbox
  │    → 下一次外层 LateUpdate
  │    → DebugDispatch system
  │    → plugin main handler
  │    → ReplyOutbox
  │
  └─ CallbackInbox
       → 下一次对应游戏 Hook callback
       → callback handler
       → ReplyOutbox
```

main handler 在下一次外层 LateUpdate 的固定 DebugDispatch 阶段执行，正常帧循环下可视为近似即时响应，但不承诺与 I/O 接收处于同一帧。

MessageMaintenance 位于 DebugDispatch 之前。main handler 若通过统一 route 提交 typed message，该 message 可以进入随后同帧 CommandDrain 在阶段入口捕获的有界批次。DebugDispatch 按接收顺序、每次最多处理一个固定内部上限的 ready request；达到上限的请求留到下一次 LateUpdate，不开放预算配置，也不向 wire 层承诺同帧响应。

callback handler 不要求即时响应，也不由 DebugHub 人工唤起。它只在对应 Hook 自然再次进入时非阻塞地处理有限数量的 pending request；v1 每次对应 Hook entry 最多执行一个 Debug request，剩余请求等待下一次自然进入。Hook 长时间不触发时请求保持 pending，直到执行、所属 route 被禁用或 RuntimeGate 关闭。v1 不支持客户端主动取消或 per-request deadline；正常 pending 不得被误报成 transport 卡死。客户端主动断开 socket 时，该连接上尚未开始执行的 request 直接从 pending 状态释放；已经开始执行的 handler 不强行取消，完成后的 response 若无法发送则丢弃并记录 observability。Debug worker 停止时，所有尚未完成的 pending request 统一以 `runtime_unavailable` 回复，然后关闭 transport；不再等待 handler 或重试。

Debug 的 request/response 与业务状态 message 使用同一套 endpoint 外形，但不共享 latest-value mailbox 语义。每个已接受的 Debug request 都作为独立 pending 项保留，受固定内部最大 pending 数和每个执行域的固定处理预算限制；这些常量不开放配置。新 request 不覆盖旧 request；没有可用 pending slot 时返回 `queue_full`。response 通过原 request 的 correlation ID 返回。

I/O worker 使用 `serde`/`serde_json` 只负责 framing、strongly typed 反序列化、路由、pending request 管理和响应序列化。v1 只允许一个 Debug socket 连接；已有连接时，新连接直接返回 `queue_full` 并关闭，不维护连接集合或跨连接协调。这个唯一连接内同时 active 的 request `id` 必须唯一；重复 active `id` 直接返回 `invalid_request`，不覆盖或影响原 request。response 成功写回后释放该 `id`，后续 request 可以复用。request 缺少必要字段或 `version` 不受支持时，也返回 `invalid_request`，不进入 plugin route，不关闭当前连接。payload 无法反序列化为该 topic 的 typed request 时返回 `invalid_request`，不调用 handler，也不关闭当前连接。main/callback handler 都不执行 socket I/O；callback 还不得解析 wire payload、阻塞等待或进行无界分配。

## Debug：Typed topic

进程内部使用强类型 topic，wire 层再映射到稳定名称和 topic version：

```rust
trait DebugTopic: 'static {
    const NAME: &'static str;
    const VERSION: u16;

    type Request: serde::de::DeserializeOwned + Send + 'static;
    type Response: serde::Serialize + Send + 'static;
}
```

插件在 `Plugin.build` 的 owner scope 中选择性注册：

```text
register_main<T>
  → MainInbox
  → DebugDispatch driver 调用 owner handler，使用该插件 World resources

register_callback<T>
  → 对应 CallbackInbox
  → Hook callback 使用所属 plugin 的 CallbackSiteContainer
```

一个 request topic 只能有一个 owner、一个 handler 和一个执行域。topic 名称是稳定的非空字符串；同名 topic 不允许注册第二个版本或第二个 handler。v1 topic 的 request/response 必须分别满足 `DeserializeOwned`/`Serialize` 与 `Send + 'static`，并通过标准 JSON 做有界反序列化/序列化；不提供二进制 codec 或插件自定义 codec。重复名称、版本不兼容、缺少标准 JSON 支持或多重 handler 必须使当前插件 build 失败，不得覆盖已有 route。wire payload 无法匹配 request schema 时返回 `invalid_request`，不调用 handler。handler 概念上返回 `Result<Response, HandlerError>`：已声明的业务错误统一映射为 wire 层的 `handler_error`，只回复当前 request，不开放插件自定义 error code，也不退役 plugin；handler panic 才记录 observability、让当前 request 返回 `plugin_unavailable`，并按 owner-local failure 规则禁用该 owner 的 debug routes，不影响其它 plugin。

main/callback handler 都通过 correlation ID 把 typed response 写入 ReplyOutbox，wire 序列化由 I/O worker 完成。response 无法序列化或超过固定 wire frame 上限时，I/O worker 返回小型 `internal_error` 或 `payload_too_large` error response，并记录 observability；不把序列化失败传播到游戏 callback。v1 不提供无需回复的 Debug event；需要向外部报告状态时，也注册为带 typed response 的 Debug request。

插件逻辑退役时必须先原子禁用其 debug routes。禁用后的新请求返回 `plugin_unavailable`；已经 pending 但未执行的请求也统一回复 `plugin_unavailable`，不执行 handler。queue 满、transport 关闭或 response 发送失败不得改变游戏 callback 的 original 行为，也不得单独升级为插件失败。

所有 plugin-owned route 的有效条件还包含进程级 RuntimeGate。总 gate 关闭后，DebugHub 不再向 main/callback pending-request 通道投递新的插件请求；main route 返回 `runtime_unavailable`，callback route 等待中的请求统一回复 `runtime_unavailable`。core transport 在 worker 尚未停止时可以继续存活以报告 transport-level 状态，但不得绕过总 gate 调用插件 handler；worker 停止后按 shutdown 语义结束全部 pending request 并关闭 transport。

Debug topic 不得提供任意地址读写、任意 IL2CPP 调用或绕过 core capability 的操作。所有外部可执行行为都必须对应显式注册并接受 owner 管理的 typed topic。

## Debug：Wire 与 transport

v1 wire 只定义 request 和 response 两种 envelope；length-prefixed frame 的方向已经表达消息类别，不额外发送 message type 或独立 protocol version。每个 frame 先写一个 4 字节 big-endian 无符号长度，再写 JSON UTF-8 bytes；长度和 JSON bytes 都受固定内部最大大小限制。长度超过上限直接返回 `payload_too_large`；长度前缀已读出但 frame 尚未读完整时遇到 EOF，按客户端断开处理，不额外发送 response。请求在各执行域按接收顺序处理，不承诺 main 与 callback 之间的全局顺序。共同字段为：

- correlation ID。
- topic 名称与 topic version。
- 有界 payload。

为了方便后端调用，v1 使用稳定的 JSON envelope。request 形状为：

```json
{
  "id": "req-123",
  "topic": "translation.get",
  "version": 1,
  "payload": {}
}
```

成功 response 形状为：

```json
{
  "id": "req-123",
  "ok": true,
  "payload": {}
}
```

失败 response 形状为：

```json
{
  "id": "req-123",
  "ok": false,
  "error": {
    "code": "plugin_unavailable",
    "message": "..."
  }
}
```

后端只需要按 `request(topic, payload) -> response` 调用；执行域、plugin owner、mailbox 和 Bevy 不进入 wire API。内部 topic 仍使用 typed request/response，wire envelope 只负责稳定字段和序列化边界。response 的 `id` 在能够解析出合法 request `id` 时原样回显；完整 frame 的 JSON 无法解析或 request 缺少合法 `id` 时使用 `id: null`。这种错误没有可关联的 request，不进入 pending 状态，但仍保持当前连接；如果连接在 frame 读取完成前 EOF，则按客户端断开处理，不额外发送 response。

第一版 Debug 消息类别限定为 request frame 和 response frame；error 是 response 的一种结果，不单独定义第三种 wire frame。I/O worker 按以下顺序处理每个 frame：检查长度上限，读取完整 frame，解析 JSON，校验 envelope 字段与 request `id`，检查 topic/version，反序列化 typed payload，检查 pending 容量，最后投递到执行域。任一步失败都不调用 handler；能关联 `id` 的错误回显该 `id`，不能关联时使用 `id: null`。超过大小上限时直接返回 `payload_too_large`，不进行完整反序列化，也不进入 App、plugin 或 callback。未知 `topic` 直接返回 `unknown_topic`，不进入任何 plugin route，也不影响当前连接。格式错误或无法解码的 JSON 返回 `decode_error`；缺少必要字段、不支持的 `version`、request payload 不符合 typed schema 或重复 `id` 返回 `invalid_request`；这些协议层错误都保持当前连接并继续处理后续请求，不进入 plugin route。错误至少区分 unknown topic、plugin unavailable、handler error、runtime unavailable、queue full、payload too large、decode error、invalid request 和 internal error；v1 不定义 deadline exceeded、客户端取消或事件订阅。

DebugHub 与 transport 解耦。v1 只使用本机 Unix domain socket 与 length-prefixed JSON，不加入 HTTP 或 WebSocket，也不设计浏览器 bridge。唯一 socket 路径为入口传入的游戏容器 `Documents/shiny-song-tools/debug.sock`；运行时将其解释为 `Documents` 根下的固定相对路径，不使用个人绝对路径、项目根路径或外部配置的 socket 文件名。socket 创建后权限限制为当前用户可读写（概念权限 `0600`），不增加应用层 token 或握手协议。由于 v1 默认不存在游戏双开，Debug 启动时直接删除同名残留 socket 文件后再创建新 socket；运行期间只接受一个客户端连接，已有连接时新连接返回 `queue_full` 并关闭，不维护连接集合或双开协调。客户端断开后 listener 可以接受下一个新连接，新连接不继承旧 pending request 或 request `id` 集合。删除或创建失败只使 Debug 不可用，不影响 runtime 和游戏。

WebSocket、浏览器 bridge 和其它网络 transport 不属于当前设计范围，留到后续需求明确后再单独讨论。

Debug 默认关闭，不绑定所有网络接口。v1 的唯一启用配置项是语义字段 `debug.enabled`，默认值为 `false`；复用现有 runtime 配置，不新增环境变量或额外启动参数，也不开放 socket 路径、端口、协议或 transport 配置。只有该字段明确为 `true` 时，AppCore::DebugState 才基于入口传入的容器路径删除残留 socket、创建 Debug I/O worker 并监听本机 Unix domain socket；worker 或 socket 启动失败时进入 `Unavailable`、记录 observability 并返回 `runtime_unavailable`，不重试，不影响 runtime 和游戏。未启用时不创建 worker、不监听、不创建 socket，也不影响正常插件和游戏行为。运行期间只接受一个客户端连接，已有连接时新连接返回 `queue_full` 并关闭；客户端断开后，listener 可以接受下一个新连接，不恢复旧 session。App 退出时停止 Debug worker；worker 停止时所有尚未完成的 pending request 统一回复 `runtime_unavailable`，然后关闭 transport，并尝试删除本次使用的 `debug.sock`；删除失败只记录 observability，不影响 App、runtime 或游戏。消息大小、pending 数和处理预算都使用固定内部上限，不开放配置；socket 权限采用当前用户可读写，v1 不提供客户端取消、per-request deadline 或事件订阅。

## Debug：crate 集成边界

具体物理模块可以在实现前调整，但责任固定为：

| 层 | Debug 职责 |
|---|---|
| core | `tracing` facade 使用、transport-neutral envelope、队列 wrapper 和 callback-safe I/O handle；不安装 subscriber，不持有 plugin route |
| plugin API | re-export/约定插件可用的 tracing macros/fields；`DebugTopic` 与 main/callback 注册接口；不暴露 route table |
| plugin system | system/owner tracing span，topic 唯一性、owner、执行域、route disable 和 pending request 生命周期 |
| runtime | 创建 scoped Dispatch、保活 Apple Unified Logging layer，根据配置启动 transport，安排 DebugDispatch 阶段 |

## 待打磨与待设计汇总

Observability v1 已收敛：普通事件使用 `tracing` 的 `code`、`owner`、`phase`、`result` 等稳定字段；callback 只提交 core/runtime 固定事件代码、level、owner、phase 和两个无符号标量参数；使用固定容量进程级 `ArrayQueue`，满载只累计 dropped counter；独立 drain worker 转回 scoped `tracing`；只输出 Apple Unified Logging，失败时退化到 stderr；subsystem 固定为 `com.shinysongtools.runtime`，category 固定为 `runtime`、`plugin`、`hook`、`debug`；ObservabilityRoot 保活到进程退出。v1 不包含文件 sink、动态 reload、插件 callback descriptor、自定义 callback event、周期性 dropped summary 或 Debug 订阅。

Debug v1 已收敛：topic 使用标准 serde JSON typed request/response；一个 topic 只有一个 owner、一个 handler 和一个执行域；schema/协议错误不进入 route；业务 handler 错误只影响当前 request，handler panic 才按 owner-local failure 禁用该 owner 的 Debug routes；pending request 使用固定内部有界队列，不覆盖、不取消、不设置 deadline；只允许一个连接；未知 topic、plugin/runtime 不可用、handler error、队列满、payload 超限、decode/schema 错误和 response 序列化错误都有明确 error code；主线程按固定批次处理，callback 按自然 Hook entry 每次最多处理一个；worker 停止时先回复 `runtime_unavailable`，再关闭 transport、删除 `debug.sock`。v1 不包含事件订阅、live snapshot、客户端取消、per-request deadline、WebSocket、浏览器 bridge、二进制 codec、自定义 codec、连接集合或跨连接 session。
