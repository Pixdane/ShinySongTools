# Bundle 编译流程

## 构建入口

```sh
zig build bundle
```

Zig 负责描述完整构建图。Cargo 负责编译 Rust staticlib；Zig 负责生成 Swift 输入、链接、组装 bundle、签名、验证、生成外置 manifest 和发布。

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

生产 runtime 的 release profile 必须显式使用 unwind panic strategy：

```toml
[profile.release]
panic = "unwind"
```

scheduler 和插件错误隔离依赖 `catch_unwind`；不得为了缩小产物或其它优化把生产 staticlib 改为 `panic = "abort"`。`catch_unwind` 的具体能力边界见 [Runtime crate 设计](runtime-crate.md#panic-边界)。

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

### 5. 签名并验证

签名步骤以未签名 bundle 为输入，先复制到新的输出目录，再对整个输出执行 ad-hoc 签名。不得就地签名 Zig 缓存中的上游输入，也不得直接签名 `build/AKInterface.bundle`。

等价命令为：

```sh
codesign -f -s - --timestamp=none AKInterface.bundle
codesign --verify --strict AKInterface.bundle
```

验证步骤必须依赖本次签名输出。签名或验证失败时，后续发布步骤不得执行。

### 6. 生成外置 build manifest

Manifest 步骤只能读取已经通过签名验证的最终 bundle，生成与 bundle 相邻的 `AKInterface.bundle.manifest.json`。Manifest 至少记录：

- 最终已签名 bundle executable SHA-256
- 最终 `Info.plist` SHA-256
- `BundleFingerprintV1` 的有序文件条目
- PlayTools repository 与固定 commit
- AKPlugin patch SHA-256
- architecture、deployment target 和签名类型
- `rustc`、`swiftc`、Zig 与 macOS SDK 版本

`BundleFingerprintV1` 不定义一个含义模糊的“bundle SHA-256”，而是按以下规则生成和比较结构：

1. 遍历最终已签名 bundle；v1 拒绝 symlink，以及目录和 regular file 之外的条目。
2. 每个文件使用相对于 bundle 根目录的 POSIX 路径；拒绝绝对路径、空路径和 `..` 分量。
3. 包含所有 regular file，包括 executable、`Info.plist` 和 `_CodeSignature` 中的文件。
4. 路径按 UTF-8 byte order 排序，每个条目记录相对路径和该文件的 SHA-256。
5. 身份比较是整个有序条目向量的结构化相等比较。

sidecar 自身位于 bundle 之外，不属于 fingerprint。Fingerprint 不包含 xattr、mtime、inode、owner；可执行位、bundle 布局和代码签名分别作为验证条件检查。

Manifest 必须属于本次构建图，并直接接收本次签名输出的 `LazyPath`，不能从固定路径读取上一次构建的文件。

### 7. 发布最终产物

发布步骤只接受已经通过签名验证的完整 bundle，以及由该 bundle 生成的本次 sidecar manifest。

发布时分别先写入 `build/` 下的临时兄弟路径，完整复制成功后再替换最终路径。先发布 bundle，最后发布 sidecar；不得逐文件直接覆盖现有 `build/AKInterface.bundle`。candidate 只有在 bundle 与 sidecar 都存在、且重新计算的 fingerprint 与 sidecar 完全一致时才有效，因此中断发布不会把新旧不匹配的文件误认为有效 candidate。

构建失败时：

- 若此前没有成功产物，不存在有效的 bundle/sidecar 对。
- 若此前已有成功产物，尽力保留该完整产物；即使在两个原子替换之间中断，新旧不匹配也会被识别为无效 candidate。

仅有 `build/AKInterface.bundle` 存在不足以证明一次成功发布。消费者必须同时读取相邻 sidecar，并重新计算完整 fingerprint；二者一致才接受该 candidate。

## 最终产物

```text
build/
├── AKInterface.bundle/
│   └── Contents/
│       ├── Info.plist
│       ├── MacOS/
│       │   └── AKInterface
│       └── Resources/
└── AKInterface.bundle.manifest.json
```

构建 candidate 是完整的 `AKInterface.bundle` 与相邻 sidecar manifest。实际部署到游戏的只有 bundle；sidecar 用于部署前验证和事务记录，不复制进游戏。生成的 Swift 源码、未签名 bundle、独立 executable 和 Rust staticlib 都是中间产物，不能单独替换到游戏中。

安装阶段必须原样复制已经签名的 candidate，不得再次签名 staged bundle。再次签名会修改 `_CodeSignature` 或 executable，使构建时批准的 fingerprint 失效。若目标平台后来证明必须在 staging 后重签名，则需要重新设计 candidate 身份和批准边界，不能静默放宽本约束。
