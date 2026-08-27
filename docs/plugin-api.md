# Plugin API 设计

状态：草案；typed resource、system 参数和 callback API 待打磨

本文定义功能插件作者可见的公共 API，概念 crate 名为 `scsp-plugin-api`。它依赖 `scsp-core`，但不暴露 PluginManager、调度器 TLS、Handoff、effect 存储或 runtime bootstrap 内部实现。

## API 目标

插件采用 Bevy-style 的 App 配置模型，但不引入 ECS：

```text
Plugin 配置 App
typed resources 保存状态
Startup/Update systems 保存主线程行为
CallbackWorld resources 保存 Hook callback 可见状态
```

插件配置对象不承担逐帧 runtime 职责，不再定义长期 `PluginRuntime` trait object，也不引入 `PreparedPlugin` 变体。

## Plugin 入口

```rust
pub trait Plugin: Send + Sync + 'static {
    fn build(
        &self,
        app: &mut PluginBuildContext<'_>,
    ) -> Result<(), PluginError>;
}
```

`PluginBuildContext` 是带当前插件 owner 的受限 facade。插件可以：

- 读取 DataRoot 和被授权的 core facilities。
- 插入 typed App resources。
- 注册 Startup/Update systems。
- 构造自己的 CallbackWorld resources。
- 通过外部 Hook API 准备和安装目标专用 Hook，初始 gate 保持关闭。
- 注册 main-domain 或 callback-domain debug topic。

插件不能绕过 context 直接取得 PluginManager、修改其它插件的 owner scope、直接写 MethodPointer slot，或者制造未登记的外部 effect。

## App Resource API

普通 App resource 满足：

```rust
pub trait Resource: Send + Sync + 'static {}
```

主线程 system 通过短生命周期 context 查询：

```rust
ctx.resource::<T>()
ctx.resource_mut::<T>()
```

App 从 worker 构造并经过 Handoff，因此第一版不接受 `!Send` resource。Unity 主线程敏感操作通过 `MainThreadToken` 限权；`Resource: Send` 不表示其中的 Unity 操作可在任意线程执行。

插件私有状态默认放入自己的 typed resource，不放入通用 context 字段。真正跨插件共享的能力使用明确的新类型 resource，并在 build 时声明或检查依赖。资源缺失、重复 TypeId、覆盖规则和一次借用多个资源的 typed query 语法仍待打磨。

## Startup 与 Update systems

插件按行为注册 system：

- Startup system：首次外层 LateUpdate 在主线程执行一次；成功后插件 gate 才能开启。
- Update system：后续外层 LateUpdate 按固定顺序执行。
- 事件驱动插件：可以不注册 Update，只安装 Hook 和 callback resources。

概念接口为：

```rust
pub trait System: Send + 'static {
    fn run(
        &mut self,
        ctx: &mut SystemContext<'_>,
    ) -> Result<(), PluginError>;
}
```

`SystemContext` 提供 AppWorld resource 借用、`&MainThreadToken` 和当前 plugin owner 的受限操作。context 的借用不能被 system 保存到下一帧。插件不得创建自己的长期 tick 或绕过 App schedule 建立独立更新线程。

## Callback Resource API

普通游戏 Hook callback 不访问 App、AppWorld、PluginManager 或主线程 TLS。插件为自己的一个或多个 callback site 构造并冻结 CallbackWorld：

```text
CallbackWorld
  → resource::<T>()
  → 不提供 resource_mut::<T>()
```

callback resource 必须为 `Send + Sync + 'static`，并满足额外的 callback-safe 契约。共享只读数据优先使用不可变 `Arc<T>`；需要跨域修改时使用原子、snapshot、latest-value mailbox 或有界 inbox，而不是让 callback 获得 App resource 的 `&mut` 引用。

目标专用 callback context 的形状为：

```rust
pub struct CallbackContext<'a, F> {
    site: &'a CallbackSite<F>,
}

pub struct CallbackSite<F> {
    original: F,
    gate: GateReader,
    world: Arc<CallbackWorld>,
}
```

typed original 和 gate 是 CallbackSite 的结构字段，不放入动态资源表。缺少 feature resource 或插件 gate 关闭时，callback 仍能通过 site 中的 original 透明回退。callback context 必须先完整发布，外部 Hook API 才能安装 MethodPointer replacement。

callback 不做阻塞 I/O、wire 解码、无界分配或等待 mutex。callback handler panic 不得跨越 `extern "C"` 边界；具体 exactly-once original 调用由目标专用 wrapper 保证。

## Callback command

callback 需要修改主线程状态时，只向 callback-safe inbox 提交 owned command：

```text
Hook callback
  → try_push(Command)
  → 返回

下一次外层 LateUpdate
  → CommandDrain system
  → 修改 App resources
```

即使 Hook callback 当前碰巧位于主线程，也不直接借用 AppWorld。command 必须满足 `Send + 'static`，不得携带 callback 栈借用、IL2CPP 临时参数地址或其它短生命周期指针。

只关心最终值的请求使用 latest-value mailbox；必须保序的事件才使用有界队列。队列满载策略必须由插件显式选择，callback 不得阻塞等待容量。

## Hook 注册 API

功能 Hook 必须通过 `PluginBuildContext` 暴露的外部 Hook API 完成。插件提供目标身份、目标专用 typed ABI、replacement callback 和已经发布的 CallbackSite；API 负责：

1. 解析并校验 MethodRef。
2. 构造目标专用 typed Hook。
3. 调用 core `MethodPointerSlot` 完成 ownership-aware 安装。
4. 把 Hook、gate、callback site 和恢复动作登记为当前插件 effect。

插件不能取得底层 slot 的无约束写权限。第一版不提供通用 Hook backend trait、ABI 自动推断、共享 slot chaining、热更新或任意地址 inline hook。

## Debug topic API

Plugin API 未来提供 typed topic 以及 main-domain/callback-domain 的选择性注册入口，但具体 trait、codec bound、reply capability 和错误类型集中在 [Debug、Diagnostics 与 Logging](debug-diagnostics-logging.md) 打磨。本 API 只坚持两条边界：main handler 由下一次外层 LateUpdate 调度，callback handler 等对应 Hook 自然进入；两者都不由 I/O worker 直接调用。

## 功能模式示例

```text
Translation
  → TranslationSnapshot App resource
  → 共享只读 handle 导入 CallbackWorld
  → Localization Hook callback 查询 snapshot

Camera
  → CameraState App resource
  → Update system 使用 MainThreadToken
  → callback 输入通过 command inbox 交给主线程

FPS
  → 根据最终实现注册一次性 Startup system 或目标专用 Hook
```

Translation 文件缺失、解析失败或目标解析失败时只退役 Translation 插件并回退原文，不阻塞其它插件或 runtime。

## API 待打磨项

- Resource 重复插入、依赖声明和跨插件可见性。
- 多资源 typed query 与可变借用冲突诊断。
- closure system 与 trait object system 的具体表示。
- CallbackResource 的 sealed/unsafe marker 选择。
- CallbackWorld 的资源查询是否在安装时缓存。
- command/inbox 的统一 trait 与容量策略。
- debug request 的 codec bound、取消和 deadline API。
- target-specific Hook builder 的公开程度和错误类型。
