# Plugin system 设计

状态：草案；AppWorld、schedule 和 effect owner 细节待打磨

本文定义概念 crate `scsp-plugin-system`。它实现 `scsp-plugin-api` 的 App 配置、typed resources、systems、CallbackWorld、插件生命周期和 effect 回滚，但不提供生产 FFI 入口或 LateUpdate scheduler。

## 依赖边界

```text
scsp-core
  ↑
scsp-plugin-api
  ↑
scsp-plugin-system
  ↑
scsp-runtime
```

功能插件只依赖 plugin API 和必要的 core 类型，不依赖 plugin-system 内部结构。runtime 负责构造 App、注册具体插件并驱动生命周期。

## App 是组合根

`App` 从 bootstrap worker 构造到主线程运行始终是同一个 `Send` 类型，不引入 `PreparedApp`：

```rust
pub struct App {
    world: AppWorld,
    startup: Schedule,
    update: Schedule,
    plugins: PluginManager,
}
```

App 在 worker 阶段只执行线程无关的 build；经过 Handoff 后由游戏主线程 TLS 独占。Unity 主线程操作仍要求 runtime 创建的 `MainThreadToken`。

## PluginManager 与 owner scope

`PluginManager` 从 `App::new` 开始就是 App 的内部成员。`App::add_plugin` 为每个插件建立唯一 owner scope，并在该 scope 内调用 `Plugin.build`。

插件配置对象不作为长期 runtime 保存。manager 保留的是运行期所有权记录：

```rust
struct Plugin {
    state: PluginState,
    effects: Vec<Effect>,
    systems: Vec<SystemId>,
    callback_sites: Vec<CallbackSiteHandle>,
    debug_routes: Vec<DebugRouteId>,
}
```

PluginState 只保存 schedule 和路由实际消费的最小逻辑状态，例如能否运行 Startup/Update、能否接受 debug request 以及是否已逻辑退役。失败原因属于 diagnostics，不为每一种错误创建持久状态枚举。

PluginBuildContext 不直接暴露 manager。所有 resource、system、Hook、CallbackWorld 和 debug route 注册都自动带当前 owner；嵌套注册、重复 plugin 和 owner 泄漏规则待打磨。

## Build、Startup 与 Update

```text
worker：App::add_plugin
  → 建立 owner scope
  → Plugin.build
  → 插入 resources
  → 注册 Startup/Update systems
  → 构造 CallbackWorld
  → 安装功能 Hook（gate 关闭）

首次外层 LateUpdate
  → 运行 Startup schedule
  → 成功插件进入 Active
  → 开启该插件的功能 gate

后续外层 LateUpdate
  → 运行 Update schedule
  → 跳过已经逻辑退役的 owner
```

首次 callback 不在 Startup 成功后继续运行 Update；Update 从下一次外层 LateUpdate 开始。各 schedule 内执行顺序固定并可复现，第一版不并行执行 systems。

单个插件 build 或 Startup 失败只回滚该插件；其它插件和 App 可以继续运行。只有 scheduler 核心、主线程交接或 AppWorld 安全边界失败才升级为 runtime 级故障。

## AppWorld

AppWorld 保存 `Send + Sync + 'static` 的 typed resources，并为顺序 system 执行提供共享/独占借用：

```text
resource::<T>()
resource_mut::<T>()
```

资源条目同时记录 owner：

- core resource 由 App/runtime owner 持有。
- plugin-local resource 归属注册插件。
- 显式共享 resource 的可见性和移除权限必须单独声明。

Rust 类型系统保证 downcast 后类型正确，但资源是否存在、依赖是否满足和动态借用是否冲突仍需要 build/运行时检查。一次获取多个资源的 typed query、冲突报告和 shared resource 重名规则标记为待打磨。

## CallbackWorld

每个插件拥有自己的 CallbackWorld；同一插件的多个 callback site 可以共享它。CallbackWorld builder 在 Hook 安装前完成插入和依赖校验，随后冻结为 `Arc<CallbackWorld>`。

CallbackWorld 只暴露共享查询，不暴露普通 `&mut` resource。App resource 与 callback resource 可以通过不同的新类型 handle 指向同一个 `Arc` backend，但 callback 不借用 AppWorld 本身。

PluginManager 保留 callback site 和相关 effect 的 ownership handle。第一版不在进程运行期间释放已经可能被外部代码缓存的 callback context；CallbackWorld 因此可以保活到进程退出。

## Effect 与局部回滚

插件通过 build、Startup system、Update system 或运行期操作产生的 Hook、callback site、debug route、system registration、临时覆盖和其它资源，都必须归属于该插件的 effect 记录。

创建过程使用插件局部 transaction：

```text
effect 1
  → effect 2
  → effect 3 失败

回滚：effect 2
  → effect 1
```

失败范围为：

```text
build 失败
  → 回滚该插件已经创建的全部 effect

Startup system 失败
  → 回滚该插件从 build 到 Startup 创建的全部 effect

Update system error/panic
  → 逻辑退役并回滚该插件全部 effect
```

回滚固定顺序为：

1. 原子关闭插件全部功能 gate。
2. 禁用其 Update systems 和 debug routes。
3. 结束或取消尚未执行的 owned debug requests。
4. 逆序恢复能够安全恢复的外部 effect。
5. 把插件标记为逻辑退役并停止之后的业务行为。

MethodPointer Hook 只有在实际 slot 仍属于该插件时才 CAS 恢复；ownership drift 时不得盲写。恢复失败、未知 owner 或 callback 可达性无法确认时，仍保留 Hook、CallbackWorld、callback site 和相关 backend handles。

“逻辑回滚完成”只表示功能停用且可安全恢复的外部状态已经恢复，不表示 Rust 对象已释放。其它插件和 App 继续运行。

## 物理卸载与资源回收

第一版不支持进程内物理卸载插件。真正释放插件对象、Hook 和 callback context 的协议仍为待设计：

```text
关闭 gate
  → ownership-aware 恢复 slot
  → 阻止新 callback 进入插件逻辑
  → 等待在途 callback 归零
  → 处理外部缓存的 replacement pointer
  → 释放 context、effect 和 resources
```

在该协议完成前，不得因为 slot 已恢复或短时间未观察到 callback 就推断 context 不可达，也不得删除需要保活的 retired plugin ownership record。

## Debug route 所有权

plugin-system 负责把 debug topic route 归属到 plugin owner，并在插件逻辑退役时先禁用 route；它不实现 wire transport。topic 唯一性、main/callback 执行域、pending request 和回复竞态的完整设计集中在 [Debug、Diagnostics 与 Logging](debug-diagnostics-logging.md)。

## App 级失败后的保活

SchedulerHook 安装失败时 App 尚未进入 Handoff。只有所有功能 effect 都确认恢复、所有 callback context 都确认不可达时，worker 才允许丢弃 App。

任一功能 Hook 存在 ownership drift 或恢复结果无法确认时，必须继续保活对应插件的 effect、CallbackWorld、callback site 和 backend handles。具体失败所有权从 worker 转移到哪里仍为插件系统待设计项；当前不预先选择泄漏、额外全局槽或其它实现。

## 待打磨项

- PluginState 的最小实际消费者集合。
- resource owner、共享依赖和重复 TypeId 规则。
- system representation、顺序和多资源 borrow API。
- Startup 部分成功时 gate 的提交边界。
- effect 类型擦除与逆序恢复接口。
- callback site/process-lifetime root 的具体持有方式。
- debug route pending request 的取消和 deadline 竞态。
- bootstrap 失败时 retired plugin ownership 的保活位置。
- 未来物理卸载的 in-flight/quiescence 协议。
