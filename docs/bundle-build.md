# Bundle 编译流程

## 构建入口

```sh
zig build bundle
```

Zig 负责描述完整构建图。Cargo 负责编译 Rust staticlib；Zig 负责生成 Swift 输入、链接、组装 bundle、生成 manifest、签名、验证和发布。

`build/AKInterface.bundle` 是发布路径。构建过程不得直接在该路径中生成或修改中间文件。

## 构建图约定

- 源文件必须通过 `b.path`、`addFileArg` 或 `addFileInput` 声明为输入。
- 外部命令生成的文件或目录必须通过 `addOutputFileArg` 或 `addOutputDirectoryArg` 声明为输出。
- 生成步骤返回的 `LazyPath` 必须直接传给后续步骤，不能改用硬编码的中间路径重新引用。
- `dependOn` 只用于表达步骤顺序，不能代替输入和输出声明。
- Zig 管理的中间产物保存在本地构建缓存中；Cargo 产物保存在 `build/target/`。
- Cargo 编译步骤是唯一的例外：每次进入构建图都执行，由 Cargo 自己处理 Rust 增量编译。
- 只有最后的发布步骤可以写入 `build/AKInterface.bundle`。

这样，已声明输入发生变化时，Zig 会使对应步骤缓存失效，并把新的输出传递给下游步骤。

## 构建输入

- `third_party/PlayTools/Plugin.swift`
- `third_party/PlayTools/AKPlugin.swift`
- 项目内的版本化 AKPlugin patch
- Swift/Rust FFI 源码
- Rust runtime crate
- `AKInterface.bundle` 的 `Info.plist`
- PlayTools submodule 当前固定的 commit

PlayTools submodule 是只读上游源码输入。构建不得直接修改 submodule 内的文件。

## 构建步骤

### 1. 生成 Swift bundle 源码

Patch 步骤声明以下输入：

- `third_party/PlayTools/AKPlugin.swift`
- 版本化 AKPlugin patch

该步骤把修改后的 `AKPlugin.swift` 生成为 `LazyPath`。生成文件位于 Zig 构建缓存中，不直接写入 `build/`。`Plugin.swift` 继续作为只读输入使用。

### 2. 编译 Rust staticlib

Zig 调用 Cargo，为 `aarch64-apple-darwin` 编译 release staticlib：

```sh
cargo build --release --target aarch64-apple-darwin
```

Cargo 步骤不参与 Zig 的文件缓存判断。每次执行 `zig build bundle` 时，Zig 都调用 Cargo；是否重新编译 Rust crate 由 Cargo 根据 manifest、lockfile、build script、源码和依赖自行决定。

Cargo 产物位于：

```text
build/target/aarch64-apple-darwin/release/libshiny_song_tools.a
```

Swift 链接步骤必须显式依赖 Cargo 步骤，并把 Cargo 实际生成的 staticlib 声明为文件输入。这样 Cargo 完成后，Zig 会根据 staticlib 的当前内容判断 Swift 链接输出是否需要更新。Rust 与 Swift 的最低 macOS deployment target 暂定为 12.0。

### 3. 链接 bundle 可执行文件

Zig 通过 `xcrun swiftc` 编译并链接：

- `Plugin.swift`
- Patch 步骤生成的 `AKPlugin.swift` `LazyPath`
- Swift/Rust FFI 源码
- Rust staticlib
- AppKit 和 Foundation 系统 framework

链接结果是 arm64 macOS bundle executable。该 executable 必须声明为链接步骤的输出，并以 `LazyPath` 传给 bundle 组装步骤。

Rust runtime 静态链接进 `AKInterface`，不生成需要部署到游戏目录的第二个项目自有动态库。

### 4. 组装未签名 bundle

Bundle 组装步骤从声明过的输入创建一个新的输出目录：

```text
AKInterface.bundle/
└── Contents/
    ├── Info.plist
    ├── MacOS/
    │   └── AKInterface
    └── Resources/
```

`Info.plist` 至少声明：

- bundle executable：`AKInterface`
- principal class：`AKPlugin`
- package type：`BNDL`

PlayTools 的加载路径为 bundle URL → `principalClass` → `Plugin.Type` → `init()`。

组装结果是未签名 bundle 目录的 `LazyPath`，仍不写入最终发布路径。

### 5. 生成 build manifest

Manifest 步骤读取本次构建的实际输入和输出，生成 `build-manifest.json`，至少记录：

- bundle executable SHA-256
- `Info.plist` SHA-256
- PlayTools repository 与固定 commit
- AKPlugin patch SHA-256
- architecture、deployment target 和签名类型
- `rustc`、`swiftc`、Zig 与 macOS SDK 版本

Manifest 必须属于本次构建图，不能从固定路径读取上一次构建的文件。

生成的 manifest 会在签名前写入本次 bundle 的：

```text
AKInterface.bundle/Contents/Resources/build-manifest.json
```

### 6. 签名并验证

签名步骤以未签名 bundle 和本次 manifest 为输入，先复制到新的输出目录，把 manifest 放入 `Contents/Resources/`，再对整个输出执行 ad-hoc 签名。不得就地签名 Zig 缓存中的上游输入，也不得直接签名 `build/AKInterface.bundle`。

等价命令为：

```sh
codesign -f -s - --timestamp=none AKInterface.bundle
codesign --verify --strict AKInterface.bundle
```

验证步骤必须依赖本次签名输出。签名或验证失败时，后续发布步骤不得执行。

### 7. 发布最终产物

发布步骤只接受包含本次 manifest、并且已经通过签名验证的完整 bundle。

发布时先写入 `build/` 下的临时兄弟路径，完整复制成功后再替换最终路径。不得逐文件直接覆盖现有 `build/AKInterface.bundle`。

构建失败时：

- 若此前没有成功产物，`build/AKInterface.bundle` 不存在。
- 若此前已有成功产物，保留该完整产物，不留下本次构建的半成品。

因此，只要 `build/AKInterface.bundle` 存在，它就一定来自一次完成编译、组装、manifest 生成和签名验证的成功发布。若当前构建失败，该路径可能仍代表上一次成功构建，其准确身份以 bundle 内 manifest 中的 SHA-256 为准。

## 最终产物

```text
build/AKInterface.bundle/
└── Contents/
    ├── Info.plist
    ├── MacOS/
    │   └── AKInterface
    └── Resources/
        └── build-manifest.json
```

可部署产物是完整的 `AKInterface.bundle`。生成的 Swift 源码、未签名 bundle、独立 executable 和 Rust staticlib 都是中间产物，不能单独替换到游戏中。
