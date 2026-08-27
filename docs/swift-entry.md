# Swift 入口行为

`AKInterface.bundle` 的 principal class 是 `AKPlugin`。PlayTools 加载 bundle 后创建 `AKPlugin`，正式项目在其 `init()` 中将控制交给 Rust，同时保留上游 PlayTools 的窗口与输入实现。

## FFI 接口

Swift 侧只声明一个正式 Rust 入口：

```swift
import Foundation

@_silgen_name("scsp_start")
func scsp_start(_ documentsPath: UnsafePointer<CChar>)
```

该接口传入游戏 sandbox 的 Documents 目录。它不是项目根目录、`local.edn` 所在目录或 macOS `.app` 路径。

Swift 不保留 Rust 状态，也不声明实验使用的 variant、event、image 或 evidence 接口。

## 初始化流程

`AKPlugin.init()` 按以下顺序执行：

1. 调用 `super.init()`。
2. 通过 `FileManager.default.urls(for: .documentDirectory, in: .userDomainMask)` 获取游戏 sandbox 的 Documents URL。
3. 将 URL 的 `path` 保存为 Swift `String`。
4. 使用 `DispatchQueue.main.async` 把 Rust 启动投递到主队列。
5. 继续执行上游 `AKPlugin.init()` 的窗口配置和通知注册。
6. 主队列执行异步闭包时，用 `withCString` 将 Documents 路径临时转换成 NUL 结尾的 C 字符串并调用 `scsp_start`。

对应代码形态为：

```swift
required override init() {
    super.init()

    if let documents = FileManager.default.urls(
        for: .documentDirectory,
        in: .userDomainMask
    ).first {
        let documentsPath = documents.path

        DispatchQueue.main.async {
            documentsPath.withCString { path in
                scsp_start(path)
            }
        }
    }

    // 上游 PlayTools 窗口初始化继续执行。
}
```

若 Documents URL 不存在，Swift 不调用 Rust，但仍继续执行上游 PlayTools 初始化。

`DispatchQueue.main.async` 只把调用推迟到当前主队列任务之后，使 Rust 入口不位于当前 `AKPlugin.init()` 调用栈中。它不是固定时间延迟，也不表示 Unity 或 IL2CPP 已经完成初始化。

## 路径生命周期

`withCString` 传出的指针只在闭包执行期间有效。Rust 必须在 `scsp_start` 返回前复制路径内容；不得保存该指针供后台线程使用。

Rust 侧的入口至少满足：

- 拒绝空指针或空路径。
- 在当前调用中把 C 字符串复制为 Rust 拥有的字符串或 `PathBuf`。
- 保证运行时只启动一次。
- 将后续工作交给后台执行，并尽快返回 Swift 主队列。
- 不允许 panic 跨越 FFI 边界。

Documents 路径只负责向 Rust 提供之后需要使用的游戏本地可写根路径。Swift 不负责创建具体子目录、读取业务配置或写入运行时文件。

## 无窗口安全修复

正式 patch 同时保留实验中验证过的三个 PlayTools 无窗口修复。

主屏幕尚不存在时返回空矩形：

```swift
var mainScreenFrame: CGRect {
    NSScreen.main?.frame ?? .zero
}
```

应用窗口尚不存在时报告非全屏：

```swift
var isFullscreen: Bool {
    NSApplication.shared.windows.first?
        .styleMask.contains(.fullScreen) ?? false
}
```

处理 traffic-light 区域的鼠标事件时，只在首个窗口存在时比较窗口身份：

```swift
if let firstWindow = NSApplication.shared.windows.first,
   event.window != firstWindow {
    return event
}
```

这些修复只移除启动早期的强制解包。窗口存在时，行为保持与上游 PlayTools 一致。

## 保留的上游行为

Rust 调用的投递不会替代 PlayTools 初始化。`AKPlugin.init()` 仍负责：

- 将当前窗口设置为可缩放、可移动并参与全屏管理。
- 应用隐藏标题栏、悬浮窗口和固定宽高比设置。
- 启用自动窗口标签行为。
- 监听 `NSWindow.didBecomeKeyNotification`，对之后出现的窗口应用相同设置。

键盘、鼠标、滚轮、光标和菜单栏接口继续由上游 `AKPlugin` 实现。

## 不包含的实验行为

正式 Swift 入口不包含：

- `scsp_carrier_arm` 或 `scsp_carrier_ready`。
- carrier event 和 image 记录。
- Swift 侧 evidence 目录设置。
- `scsp_lateupdate_variant_id`。
- 一秒定时事件。
- Unity/IL2CPP readiness 判断。
- talagent saved-state 查找或修改。

运行时何时访问 Unity、如何等待 readiness，以及如何使用 Documents 路径，都由 Rust 侧实现。
