# Plugin API

状态：v2 设计（2026-08-29 修订）。本文定义功能插件作者可见的公共 API，现由 `core::plugin_api` 承载。它不暴露 PluginManager、driver、Handoff、ledger 或 runtime bootstrap 内部实现。

## 定位与信任边界

插件平台为个人使用设计：插件与 runtime 同仓库编译，插件作者即审阅者。因此：

- hook 目标（ABI wrapper）由插件作者自定义，不经 runtime 统一注册；每个新 target 的 ABI 与校验谓词由作者审阅并对后果负责。
+ `core::plugin_api` 不承诺 semver 稳定；API 演进以同仓库内所有插件同步修改为准。
- 这不放松安全边界：插件不能绕过 facade 取得 `World`、slot 写权限或未登记的外部 effect，这些由类型与可见性强制。

插件采用 Bevy-style 的 App 配置模型，复用 `bevy_ecs` 的 resource、SystemParam 与 System；不引入 entity/component gameplay model、`bevy_app::App` 或 Bevy runner：

```text
Plugin 配置 App
typed resources 保存状态
Startup/Update systems 保存主线程行为
plugin-defined CallbackSiteContainer 保存 Hook callback 可见状态
```

## Plugin 入口与错误类型

```rust,ignore
pub trait Plugin: Send + Sync + 'static {
    fn build(&self, ctx: &mut AppCtx<'_>) -> Result<(), PluginError>;
}

#[derive(Debug, thiserror::Error)]
pub enum PluginError {
    #[error("resource conflict: {0}")]
    ResourceConflict(&'static str),
    #[error("missing dependency: {0}")]
    MissingDependency(&'static str),
    #[error("hook: {0}")]
    Hook(#[from] HookError),
    #[error("il2cpp: {0}")]
    Il2Cpp(#[from] Il2CppError),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Message(&'static str),
}
```

插件作者只见这一条错误链（`PluginError` / `HookError` / `Il2CppError` / `RestoreError`）；debug wire 的错误映射见 debug 分册。失败的具体原因与上下文走 observability 事件，不为错误创建持久状态枚举。

`AppCtx` 是带当前插件 owner 的受限 facade。插件可以：

- 读取 `RuntimeConfig` 与 DataRoot。
- 向共享 AppWorld 插入 typed resources（直接插入，见下节）。
- 注册 Startup/Update systems。
- 注册一个插件自定义的 CallbackSiteContainer。
- 声明跨域 message route。
- 通过 hook typestate API 安装目标专用 Hook（初始 gate 关闭）。
- 注册 main/callback 两域 debug topic。

插件不能绕过 context 直接取得 PluginManager 或 `World`、修改其它插件的 owner scope、直接写 MethodPointer slot，或制造未登记的外部 effect。

## Resource API：直接插入 + ledger 记账

```rust,ignore
#[derive(Resource)]
struct FpsState { current: FpsSetting }
```

- `ctx.insert_resource::<T>(value)` 直接写入共享 AppWorld，同时向 owner ledger 记录 `(TypeId, 顺序)`；类型已存在（含本 owner 重复插入）返回 `PluginError::ResourceConflict`，不使用 Bevy 的覆盖语义。
- 插件需要独立状态时用自己的 newtype；需要跨插件共享时直接约定公开 resource 类型，并接受固定注册顺序（前序插件已插入的 resource 对后续插件可见；缺失依赖是 `PluginError::MissingDependency`）。v1 不提供依赖声明或自动排序。
- Build/Startup 失败时 driver 按 ledger LIFO 移除该 owner 插入的资源；依赖被移除资源的其它插件在 param validation 时失败退役（行为正确，不是隐式容错）。
- AppWorld 只接受 `Send + Sync + 'static` resource（App 必须跨 Handoff 保持 `Send`）。`Resource: Send` 不表示其中的 Unity 操作可在任意线程执行——Unity 主线程操作必须经 `MainThreadToken`。
- 只读共享契约（未来的社区翻译表等）可用 bevy 0.19 的 immutable resource 表达，使任何插件都无法取得 `ResMut`；具体标注形式实现时核对。
- Update system 只能修改已提交的 resource；API 不 re-export `Commands`/`World`，运行期不能插入资源、注册 Hook/route 或登记恢复动作。

## Phase systems：编译期区分 Startup/Update

system 的第一个参数是 phase context，phase 由类型区分：

```rust,ignore
pub struct StartupCtx<'a> {
    pub main: &'a MainThreadToken,
    pub reg: &'a mut StartupRegistrar,   // 只提供登记 restore action 的窄 API
}

pub struct UpdateCtx<'a> {
    pub main: &'a MainThreadToken,
}

// 注册 API 只接受对应 phase 的函数；跨 phase 注册是编译错误。
ctx.add_startup_system(startup)?;
ctx.add_update_system(update)?;
```

```rust,ignore
fn startup(ctx: StartupCtx<'_>, config: Res<FpsConfig>) -> Result<(), PluginError> { /* ... */ }

fn update(ctx: UpdateCtx<'_>, mut state: ResMut<FpsState>) -> Result<(), PluginError> { /* ... */ }
```

- Startup system 在首个外层 LateUpdate 的主线程执行一次（按插件与注册顺序）；它插入的资源立即进入 AppWorld 并记入 ledger，登记的 restore action 在全部成功后随 owner 事务提交。
- Update system 在后续每个外层 LateUpdate 按固定顺序执行；只能操作已提交资源。
- 事件驱动插件可以不注册 Update，只安装 Hook 与 callback container。
- `MainThreadToken` 只以本次调用的短借用出现在 phase context 中；借用不能保存到下一帧。
- 每个 boxed system 由 driver 在**首次运行前**惰性 `System::initialize`：跨阶段资源依赖（Update 引用别家 Startup 插入的资源、后注册插件引用前序插件 Startup 产物）因此天然成立，不要求 build 期资源已存在。
- `StartupRegistrar` 只登记 restore action（`AnyThread` / `MainThread` 两种，语义见 plugin-system 分册）；资源插入走 `StartupCtx` 的 facade，不再有 staged/延迟提交语义。

## 跨域 message：方向 branded endpoint

callback 与主线程分属两个执行域，World 不能跨域共享。插件用统一 API 声明 route，语义与方向都是类型的一部分：

```rust,ignore
// main → callback：Update system 发送，Hook callback 读取
let (tx, rx) = ctx.main_to_callback::<FpsSetting>(Mailbox::latest())?;
// tx: MainWriter<FpsSetting, Latest>     —— 在 Update system 内以 &UpdateCtx 调用
// rx: CallbackReader<FpsSetting, Latest> —— 放进 CallbackSiteContainer，callback 内以 &CallbackCtx 调用

// callback → main：Hook callback 发送，main 侧 system 读取
let (tx, rx) = ctx.callback_to_main::<FpsEvent>(Mailbox::bounded::<16>())?;

// 有主结构化数据（非 Copy），为 debug 域与复杂对象准备：
let (tx, rx) = ctx.main_to_callback::<DebugRequest>(Mailbox::shared_latest())?;
```

- 语义三选一（见 core 分册）：`Mailbox::latest()`（覆盖，丢中间值）、`Mailbox::bounded::<N>()`（保序 FIFO，满载 `Full` 计数）、`Mailbox::shared_latest()`（`Arc<T>` 单槽）。
- 方向 branded：`MainWriter` 只能在 Update system 调用（要求 `&UpdateCtx<'_>`），`CallbackReader`/`CallbackWriter` 只能在 hook callback 调用（要求 `&CallbackCtx`）。在错误执行域调用是编译错误——"callback 只经统一 API 非阻塞提交"不再是散文规则。
- `latest`/`bounded` 的 payload 满足 `CallbackPayload: Copy + Send + Sync + 'static`；`shared_latest` 的 `T: Send + Sync + 'static` 为无副作用 Drop 的普通数据。
- 下一执行边界可见：本帧写入的跨域 message 不对同帧重入立即可见，message 系统不主动唤醒 callback。main 侧接收端在固定 `CommandDrain` 阶段以阶段入口 watermark 限定单帧工作量；没有主线程接收端的 route 不参与维护。
- 需要多个独立 callback 消费者时注册多条 route；不提供竞争消费、MPMC 或 broadcast。

普通 AppWorld 内的插件间消息不受上述约束，直接使用 Bevy `Message` derive 与 `Messages<M>`（driver 的 `MessageMaintenance` 阶段负责 buffer 维护）。

## Hook typestate：发布先于安装是编译期事实

目标抽象为 `HookTarget`，ABI 与校验谓词绑定在类型上；每个目标一个宏生成的进程期静态 site；安装流程用 typestate 表达：

```rust,ignore
pub trait HookTarget: 'static {
    const TARGET: TargetId;                 // identity + static/return/parameter types
    const MECHANISM: HookMechanism = HookMechanism::MethodPointerSlot;
    type Original: Copy;                    // typed fn pointer（含隐式 MethodInfo 参数）
    fn validate(method: &MethodRef) -> Result<(), HookError>;
}

// 为每个目标生成唯一的进程期静态槽（retention root）
define_hook_site!(FPS_TARGET_RATE_SITE: HookSite<SetTargetFrameRateTarget, FpsSites>);
```

注册 API（AppCtx 上）：

```rust,ignore
let sites: Arc<FpsSites> = ctx.register_container(FpsSites { setting: rx, /* ... */ })?;

ctx.hook(FPS_TARGET_RATE_SITE)          // HookBuilder<T, Unpublished>
   .container(sites.clone())
   .handler(fps_rate_replacement)       // -> HookBuilder<T, Published>
   .install()?;                         // -> Result<InstalledHook, HookError>
```

- `HookBuilder<T, Unpublished>` 只提供 `container` / `handler`；`handler` 构造完整 `HookSite`（typed original、RuntimeGateReader、PluginGateReader、容器 Arc）并发布到目标唯一静态 `OnceLock`，产生 `HookBuilder<T, Published>`。**发布先于安装由类型保证**：`install` 只在 `Published` 态存在。
- `install` 内部完成 MethodRef 解析校验 → 机制安装 → readback，返回 `InstalledHook`；其 restore 记录进入 owner ledger（ownership-aware 恢复、drift 不盲写，语义见 core 与 plugin-system 分册）。
- **安装机制（`HookTarget::MECHANISM`，默认 `MethodPointerSlot`）**：槽替换只拦截经槽分派的调用（虚/接口分派、委托、反射、Unity 生命周期回调）；被游戏代码以 AOT 直接调用方式进入的方法必须声明 `HookMechanism::EntryPatch`，由 `crates/core/src/entry_patch.rs` 在函数入口写内联跳转（macOS 签名 text 页通过 JIT 暂存页整页 `mach_vm_remap` 替换，而非 in-place 写）。两种机制对外呈现完全相同的 CAS/readback/ownership 协议（`SlotMemory`），typed original、dispatch、restore 对插件无差别。EntryPatch 安装时若入口序言含不可 verbatim 搬迁的 PC 相对指令则 fail closed（`HookError::EntryPatchUnsupported`）。
- 静态槽一旦发布便占用该目标直到进程退出；即使 CAS 安装失败、Hook 已恢复或插件退役，也不清空或复用。一个目标一个静态 site：不支持同目标多实例、重复注册、重装、slot chaining 或物理卸载。
- callback 上下文形状：

```rust,ignore
pub struct Callback<'a, T: HookTarget, C> {
    // gates 已由 wrapper 检查；任一 gate 关闭时 wrapper 直接透传 original，handler 不可达
}
impl<'a, T, C> Callback<'a, T, C> {
    pub fn cap(&self) -> &CallbackCtx;          // 跨域 endpoint 的能力 token
    pub fn container(&self) -> &C;
    pub fn call_original(&self, /* typed args */) -> T::Return;
}
```

- callback 不访问 App、World、PluginManager 或主线程 TLS；不阻塞、无界分配；handler panic 不跨 `extern "C"` 边界——exactly-once original 由目标 wrapper 的三阶段 guard 保证（与 scheduler 的 `OriginalPhase` 同型，复用同一 `OriginalGuard` 实现）。
- `HookError` 至少区分 `target_unavailable` / `signature_mismatch` / `site_already_registered` / `slot_conflict` / `installation_failed`；不把 CAS、readback 或具体地址暴露给插件。

## Debug topic API

插件为自己的开发调试注册 typed topic；执行域决定 handler 的运行位置：

```rust,ignore
trait DebugTopic: 'static {
    const NAME: &'static str;
    type Request: serde::de::DeserializeOwned + Send + 'static;
    type Response: serde::Serialize + Send + 'static;
}

// main 域：request 经 DebugPlugin 投递到本插件自动登记的 Update system（可访问 World resources）
ctx.register_main_debug::<FpsGet>()?;

// callback 域：request 经本插件的 debug handler system 转发进 CallbackSiteContainer 的
// 专用 SharedSlot，callback 自然进入时处理；响应沿原路返回
ctx.register_callback_debug::<FpsProbe>()?;
```

- 注册自动完成三件事：向 AppCore 的 topic registry 登记（name → owner/域/vtable/gate readers）、生成本 topic 专属的跨域 mailbox（callback 域，`shared_latest` 语义）、把 handler/relay system 自动登记为本插件的 Update system。插件作者只写 handler 本体。
- 一个 topic 一个 owner、一个 handler、一个执行域；重名注册使当前插件 build 失败。
- dispatch 流、pending/correlation、错误映射与运行时自省 topic 见 `debug` crate 的 Rustdoc。

## 功能模式示例：FPS 解锁（`unlock_fps` crate）

```text
`unlock_fps::FpsPlugin`
  build（worker 线程）
    → 读 config：[fps] unlock_fps（bool）
    → 定义 targets：UnityEngine.CoreModule.dll::Application::set_targetFrameRate / QualitySettings::set_vSyncCount
    → 注册 route：main→callback latest<FpsSetting>
    → 注册 container：FpsSites { setting: rx }
    → hook 两个 target（typestate 发布→安装，gate 关闭）
    → 注册 main 域 debug topic：unlock_fps.get / unlock_fps.set（bool 控制）
  startup（首个 LateUpdate，主线程）
    → 插入 FpsState resource（setter callback 读取 unlock_fps）
  setter replacement（游戏调用 setter 时触发）
    → 读容器中 latest<FpsSetting>：unlock 时覆盖为 120，locked 时透传原值
  debug handler（Update system）
    → unlock_fps.set：更新布尔状态 + MainWriter 写入 latest<FpsSetting>，并在主线程立即应用 120/60
```

该插件覆盖全部机制（target 定义、container、route、hook typestate、debug topic），且只依赖 `core` 中的 plugin API 与 platform primitives。当前仅完成无游戏验证；真实游戏启动、patch、attach 与 FPS 生效测试需绑定 candidate SHA 另行批准。翻译、相机、贴图等后续插件按同一 API 立项；翻译复用 SCSPTranslationData 社区格式（只读快照放容器字段，热重载协议届时另设计）。

## API 待打磨项

- `define_hook_site!` 宏形态、`TargetId` 的具体表示。
- `Callback` 是否需要按 target 提供参数元组泛型（当前以 `HookTarget::Original` 携带 ABI）。
- debug handler/relay 自动登记的 system 参数集合。
- `Mailbox` kind token 的具体类型呈现。
