# Shiny Song Tools

---

目标：将 [chinosk6/scsp-localify](https://github.com/chinosk6/scsp-localify) 的功能移植到运行在 MacOS PlayCover 的 iOS 版偶像大师 闪耀色彩 棱镜之歌上。

## 获取源码

项目通过 Git submodule 固定第三方源码。克隆后运行：

```sh
git submodule update --init --recursive
```

第三方组件及其许可证见 [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md)。

## Rust 文档

架构、公共 API、生命周期和安全边界归属到各 crate 的顶层 Rustdoc：

- [`corelib`](crates/core/README.md)：平台原语与 plugin API。
- [`debug`](crates/debug/README.md)：Debug control plane 与 Observability。
- [`translation_dump`](crates/translation_dump/README.md)：翻译文本的 callback-safe 采集与离线词典落盘。
- [`unlock_fps`](crates/unlock_fps/README.md)：FPS 功能插件。
- [`shiny_song_tools`](crates/runtime/README.md)：App、bootstrap、scheduler 与 Swift FFI。
- [`fake-unity-framework`](crates/testing/fake-unity-framework/README.md)：无游戏测试 fixture。

生成完整文档：

```sh
cargo doc --workspace --no-deps
```

非 Rust API 的操作文档继续保留在仓库根目录：

- [Bundle 编译流程](docs/bundle-build.md)
- [Babashka Tasks](docs/tasks.md)
