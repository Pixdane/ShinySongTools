# 运行时架构总览

状态：草案

本文是 Shiny Song Tools 进程内架构的索引，只定义 crate 依赖方向、文档职责和跨层不变量。各系统的详细设计以对应分册为准。

## 文档与 crate

| 文档 | 概念 crate | 权威范围 |
|---|---|---|
| [Core crate 设计](core-crate.md) | `scsp-core` | 平台基础 API、共享设施、IL2CPP handle、callback-safe primitives、MethodPointer slot |
| [Plugin API 设计](plugin-api.md) | `scsp-plugin-api` | 功能插件作者可见的 Plugin、Resource、System、Callback 和跨系统注册边界 |
| [Plugin system 设计](plugin-system.md) | `scsp-plugin-system` | App、PluginManager、AppWorld、CallbackWorld、schedule、owner/effect 和逻辑退役 |
| [Runtime crate 设计](runtime-crate.md) | `scsp-runtime` | `scsp_start`、bootstrap、生产组装、LateUpdate scheduler、Handoff、TLS 和 runtime 级失败 |
| [Debug、Diagnostics 与 Logging](debug-diagnostics-logging.md) | 跨 crate | Debug Control Plane 草案；Diagnostics 和 Logging 未设计状态 |

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
  → Startup/Update schedules
```

外部 PlayTools carrier 与内部 Rust feature plugins 是两个不同层次。第一版不设计动态 dylib 插件发现、热加载、ECS 或多线程 system scheduler。

## 跨层不变量

- exact UnityFramework handle 和 IL2CPP backend 属于 core/runtime 边界；不得假定 `RTLD_DEFAULT` 可见性。
- App 是唯一组合根，从 worker 到主线程始终为同一个 `Send` 类型；Handoff 后由主线程 TLS 独占。
- Unity 主线程敏感安全 API 必须要求 runtime 验证后创建的 `MainThreadToken`。
- Plugin 是 App 配置器；typed resources 保存状态，Startup/Update systems 保存主线程行为，不保留长期 `PluginRuntime` trait object。
- 普通 Hook callback 不访问 AppWorld 或主线程 TLS，只访问冻结的 CallbackWorld 和目标专用 CallbackSite。
- callback 修改主线程状态时只提交非阻塞 command，由下一次外层 LateUpdate 处理。
- 包含 typed original 的 callback context 必须先发布，MethodPointer replacement 才能安装。
- MethodPointer 安装和恢复必须使用 CAS、readback 和 ownership-aware 规则；发现未知 owner 时不盲写。
- scheduler callback 的 Rust 失败路径仍须保证 original LateUpdate 恰好调用一次，Rust panic 不得跨越 FFI。
- 插件失败只逻辑退役和回滚所属 effect；在 callback quiescence 协议完成前不物理释放可能仍可达的 context。
- Debug I/O worker 只投递消息。main handler 等下一次 LateUpdate；callback handler 等对应 Hook 自然进入，不人工唤起。完整设计状态见 [Debug、Diagnostics 与 Logging](debug-diagnostics-logging.md)。

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

待打磨：typed resource query、plugin owner/effect 表示、CallbackResource 契约、command 容量、Debug topic/transport、IL2CPP capability 方法集合。

待设计：Diagnostics、Logging、plugin/callback 物理卸载、scheduler quiescence、bootstrap 失败后的资源保活位置。
