# Shiny Song Tools

---

目标：将 [chinosk6/scsp-localify](https://github.com/chinosk6/scsp-localify) 的功能移植到运行在 MacOS PlayCover 的 iOS 版偶像大师 闪耀色彩 棱镜之歌上。

## 获取源码

项目通过 Git submodule 固定第三方源码。克隆后运行：

```sh
git submodule update --init --recursive
```

第三方组件及其许可证见 [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md)。

## 开发文档

- [Bundle 编译流程](docs/bundle-build.md)
- [Swift 入口行为](docs/swift-entry.md)
- [运行时架构总览](docs/runtime-architecture.md)
  - [Core crate 设计](docs/core-crate.md)
  - [Plugin API 设计](docs/plugin-api.md)
  - [Plugin system 设计](docs/plugin-system.md)
  - [Runtime crate 设计](docs/runtime-crate.md)
  - [Debug、Diagnostics 与 Logging](docs/debug-diagnostics-logging.md)
- [Babashka Tasks](docs/tasks.md)
