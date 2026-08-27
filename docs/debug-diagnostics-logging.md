# Debug、Diagnostics 与 Logging

状态：

- Debug Control Plane：设计草案，待打磨。
- Diagnostics：未设计。
- Logging：未设计。

本文单独记录三个横跨 core、plugin API、plugin system 和 runtime 的系统，避免把尚未确定的接口提前写死在任一 crate 中。

## 系统边界

三个系统保持独立：

```text
Logging
  → 单向运行记录

Diagnostics
  → runtime 状态、计数和故障证据

Debug Control Plane
  → 外部双向 request / response / event
```

未来 Debug 可以查询 Diagnostics 或订阅 Logging，但 Logging 和 Diagnostics 不得依赖 Debug transport 才能工作。关闭 Debug 后，正常日志、故障记录和游戏行为不能改变。

## Debug：执行域与自然调度

socket 或 WebSocket 的 I/O worker 不直接调用 App system、插件业务逻辑或游戏 Hook callback。DebugHub 只把已经完成 wire 解码的 owned typed request 投递到目标执行域：

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

callback handler 不要求即时响应，也不由 DebugHub 人工唤起。它只在对应 Hook 自然再次进入时非阻塞地轮询 inbox；Hook 长时间不触发时请求保持 pending，直到执行、route 被禁用或未来确定的取消/deadline 条件成立。正常 pending 不得被误报成 transport 卡死。

I/O worker 只负责 framing、反序列化、路由、pending request 管理和响应序列化。main/callback handler 都不执行 socket I/O；callback 还不得解析 wire payload、阻塞等待或进行无界分配。

## Debug：Typed topic

进程内部使用强类型 topic，wire 层再映射到稳定名称和版本：

```rust
trait DebugTopic: 'static {
    const NAME: &'static str;
    const VERSION: u16;

    type Request: Send + 'static;
    type Response: Send + 'static;
}
```

插件在 `Plugin.build` 的 owner scope 中选择性注册：

```text
register_main<T>
  → MainInbox
  → DebugDispatch system 使用 AppWorld resources

register_callback<T>
  → 对应 CallbackInbox
  → Hook callback 使用 CallbackWorld resources
```

一个 request topic 只能有一个 owner、一个 handler 和一个执行域。重复名称、版本不兼容、缺少 codec 或多重 handler 必须使当前插件 build 失败，不得覆盖已有 route。

main/callback handler 都通过 correlation ID 把 typed response 写入 ReplyOutbox，wire 序列化由 I/O worker 完成。CallbackWorld 也可以非阻塞发布无需回复的 debug event。

插件逻辑退役时必须先原子禁用其 debug routes。禁用后的新请求返回 `plugin_unavailable`；已经入队但未执行的请求如何取消、与同时产生的 reply 如何仲裁仍待打磨。queue 满、transport 关闭或 event 发送失败不得改变游戏 callback 的 original 行为，也不得单独升级为插件失败。

Debug topic 不得提供任意地址读写、任意 IL2CPP 调用或绕过 core capability 的操作。所有外部可执行行为都必须对应显式注册并接受 owner 管理的 typed topic。

## Debug：Wire 与 transport

共同 wire envelope 至少包含：

- 协议版本和消息类别。
- correlation ID。
- topic 名称与 topic version。
- 有界 payload。

第一版消息类别限定为 `request`、`response`、`event` 和 `error`。错误至少区分 unknown topic、plugin unavailable、queue full、decode error 和未来确定的 deadline exceeded。

DebugHub 与 transport 解耦。第一版优先验证本机 Unix domain socket 与 length-prefixed JSON。如果需要浏览器 UI，优先采用进程外 bridge：

```text
游戏 runtime
  ↕ Unix domain socket
Debug bridge
  ↕ WebSocket
Browser UI
```

这避免在注入进程内加入 HTTP/WebSocket server。PlayCover sandbox 下 socket 位置、外部进程可达性、文件权限、残留 socket 恢复和 TCP/WebSocket entitlement 都需要独立 fixture 或实验，当前不视为已证明。

Debug 默认关闭，不绑定所有网络接口。消息大小、队列容量、最大 pending 数、deadline、鉴权、wire codec、断线重连和 backend 生命周期仍待打磨。

## Debug：crate 集成边界

具体物理模块可以在实现前调整，但责任固定为：

| 层 | Debug 职责 |
|---|---|
| core | transport-neutral envelope、队列原语和 callback-safe I/O handle；不持有 plugin route |
| plugin API | `DebugTopic` 与 main/callback 注册接口；不暴露 route table |
| plugin system | topic 唯一性、owner、执行域、route disable 和 pending request 生命周期 |
| runtime | 根据配置启动 transport，安排 DebugDispatch 阶段，不直接执行 handler |

## Diagnostics：未设计

当前不定义 Diagnostics 的：

- 状态结构和状态机。
- event/counter schema。
- 内存保留和持久化策略。
- scheduler、plugin、Hook 与 transport 的记录接口。
- 查询、快照和导出格式。
- 是否以及如何通过 Debug 暴露。

其它文档可以陈述“某个失败需要被诊断”，但不得据此发明 `Diagnostics`、`DiagnosticsHandle`、`SchedulerDiagnosticsSink` 等具体字段或行为。设计 Diagnostics 时再决定哪些执行域存在真实消费者，以及 callback 是否需要窄、非阻塞接口。

## Logging：未设计

当前不定义 Logging 的：

- facade、backend、sink 和 handle 类型。
- 格式化、结构化字段和 level。
- 文件路径、轮转、刷新和进程退出行为。
- callback log queue、丢弃策略和容量。
- 与 Diagnostics 或 Debug event 的桥接关系。

已经确定的只有 callback 安全边界：普通 Hook callback 不得直接执行文件 I/O、阻塞等待、复杂格式化或无界分配。这个限制不等于已经选择 `LoggerHandle`、`CallbackLogSink`、ring buffer 或后台线程实现。

## 待打磨与待设计汇总

Debug 待打磨：topic codec bound、route registration API、queue capacity、pending cancellation、deadline、鉴权、transport backend 生命周期和 sandbox fixture。

Diagnostics 与 Logging 整体待设计；在独立讨论完成前，core/plugin/runtime 文档只保留集成边界或安全约束，不定义具体 API。
