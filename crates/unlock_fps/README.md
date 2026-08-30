# Unlock FPS plugin

`unlock_fps` 是 Shiny Song Tools 的生产功能插件，也是 plugin API 的最小完整示例。它不持有 runtime 或 App；所有状态都通过 typed resource、受 owner 管理的 Hook 和跨执行域 route 注册。

## 行为

插件为 Unity 的帧率 setter 与 vSync setter 各持有一个静态 Hook site。目标由 `corelib::TargetId` 描述并经 runtime method resolver 校验，不硬编码进程地址。

- `FpsPlugin::build` 插入插件资源、发布 callback container、安装两个 Hook，并注册 Startup system。
- Startup 在 Unity 主线程应用配置中的初始值。
- main → callback 的 latest-value route 只传播当前开关状态；中间值可以覆盖。
- callback 通过目标专用 ABI 调用 original；gate 关闭或 site 不可用时保持透传。

## Debug topics

启用 Debug plugin 后，本 crate 注册两个 main-domain topic：

- `unlock_fps.get`：读取当前开关、应用次数与两个 Hook 的命中计数。
- `unlock_fps.set`：修改开关并立即在主线程应用，响应字段为 `applied`。

Debug 只是运行时控制面；配置文件仍是启动时默认值的唯一来源。

## 安全与证据边界

本 crate 的 callback 不访问 App、World、主线程 TLS 或 socket，也不在热路径解析 wire 数据。静态 target identity、ABI 声明及无游戏 fixture 已纳入 workspace 测试；这些结果不等于当前游戏版本已经完成实机验证。
