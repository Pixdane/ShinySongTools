# Babashka Tasks

## `bundle`

### `build`

构建并发布 `AKInterface.bundle`：

```sh
bb bundle build
```

对应的 `bb.edn` 定义：

```clojure
{:tasks
 {bundle
  {:doc "Bundle 操作。用法: bb bundle build"
   :requires ([cheshire.core :as json])
   :task
   (let [[command & args] *command-line-args*
         manifest-path
         "build/AKInterface.bundle.manifest.json"]
     (case command
       "build"
       (do
         (apply shell "zig build bundle" args)
         (let [manifest (json/parse-string (slurp manifest-path))]
           (println)
           (println "Bundle built successfully")
           (println)
           (doseq [[label value]
                   [["Artifact" "build/AKInterface.bundle"]
                    ["Executable" (get-in manifest ["bundle" "executable"])]
                    ["Exec SHA" (get-in manifest ["bundle" "executable_sha256"])]
                    ["Target" (get-in manifest ["bundle" "target"])]
                    ["PlayTools" (get-in manifest ["upstream" "commit"])]
                    ["Patch" (get-in manifest ["patch" "sha256"])]
                    ["Signature" "ad-hoc, verified"]
                    ["Manifest" manifest-path]]]
             (println (clojure.core/format "%-12s %s" label value)))))

       (throw
        (ex-info "用法: bb bundle build"
                 {:args *command-line-args*}))))}}}
```

`build` 子命令调用：

```sh
zig build bundle
```

构建图、缓存、Cargo 调用、Swift 链接、bundle 组装、manifest、签名验证和最终发布全部由 `build.zig` 负责。Babashka 不复制这些逻辑。

`shell` 默认继承当前进程的标准输入、输出和错误输出，并在子进程返回非零状态时使 task 失败。只有 `zig build bundle` 成功后，task 才读取本次发布的 manifest 并打印摘要。

默认输出格式：

```text
Bundle built successfully

Artifact     build/AKInterface.bundle
Executable   AKInterface
Exec SHA     <executable-sha256>
Target       arm64-apple-macos12.0
PlayTools    <playtools-commit>
Patch        <patch-sha256>
Signature    ad-hoc, verified
Manifest     build/AKInterface.bundle.manifest.json
```

完整 manifest 保存在：

```text
build/AKInterface.bundle.manifest.json
```

该 task 不读取、修改或启动游戏。

### `status`、`patch`、`restore` 设计

以下子命令是待实现的接口设计：

```sh
bb bundle status
bb bundle patch --expected-executable-sha <candidate-executable-sha256>
bb bundle restore
```

这三个子命令只管理游戏本地 `PlugIns/AKInterface.bundle`。它们不启动游戏，也不修改全局 PlayTools 安装。

#### 本地游戏配置

所有需要读取或修改本地游戏的 Babashka 操作都从项目根目录的 `local.edn` 读取配置。该文件包含机器本地路径并由 `.gitignore` 排除，不得提交：

```clojure
{:game
 {:app "/absolute/path/to/Game.app"}}
```

`bb bundle build` 不读取游戏，也不读取 `local.edn`。`status`、`patch`、`restore` 以及以后新增的游戏读取、启动或修改操作都必须先走同一个配置加载和校验入口。

若 `local.edn` 不存在，相关 task 创建以下不包含个人路径的空白模板：

```clojure
{:game
 {:app ""}}
```

创建模板后必须以非零状态退出，并提示用户填写路径；不得在同一次调用中继续接触游戏。

若文件存在但无法解析，或内容不符合配置 schema，task 必须以非零状态退出并报告具体字段错误，不得猜测默认值、覆盖现有文件或继续操作。初版校验要求：

- 顶层值是 EDN map，且只包含已声明的配置字段。
- `:game` 是 map。
- `:game/:app` 是非空绝对路径字符串。
- 路径解析后指向现有 `.app`，且满足游戏目标路径保护规则。

从 `.app` 内 `Info.plist` 能可靠推导的 bundle ID、executable 名称和进程身份不重复写入 `local.edn`。PlayCover 装的是 iOS 扁平 bundle（`Info.plist` 在 bundle 根），加载器同时兼容 macOS 布局（`Contents/Info.plist`）；PlugIns 路径按检测到的布局派生。bundle 相对路径、artifact 路径、构建参数、事务 fingerprint 和每次操作的 executable SHA-256 确认值也不属于本地配置。

配置文件创建、EDN 解析、schema 校验、路径规范化和目标路径保护必须集中在一个可复用的公共加载入口中，例如：

```clojure
(local-config/load!)
```

`tools/local_config.clj` 只公开这个加载入口；创建模板、读取文件和逐项校验等辅助函数保持为 namespace 私有实现。调用方只处理两种结果：

1. 成功时返回已经校验并规范化的游戏上下文，例如：

   ```clojure
   {:game {:app    "/canonical/path/to/Game.app"
           :bundle "/canonical/path/to/Game.app/Contents/PlugIns/AKInterface.bundle"}}
   ```

   `:bundle` 等派生值只存在于返回结果中，不写回 `local.edn`。

2. 失败时抛出带有稳定 `:type` 和诊断数据的 `ExceptionInfo`，例如：

   ```clojure
   {:type :local-config/template-created
    :path "local.edn"}

   {:type   :local-config/invalid
    :path   "local.edn"
    :issues [...]}
   ```

最外层 Babashka 命令统一捕获这些错误、打印面向用户的消息并以非零状态退出。`status`、`patch`、`restore` 等子命令不得重复实现配置检查，也不得根据错误类型自行补全或修复配置。

#### 状态与备份路径

所有持久状态、游戏原始 bundle 备份和事务记录都放在 gitignored 的 `artifacts/` 中：

```text
artifacts/bundle/
├── state.edn
├── baseline/
│   └── AKInterface.bundle/
└── transactions/
    ├── current.edn
    └── history/
        └── <transaction-id>.edn
```

- `baseline/AKInterface.bundle` 保存首次受控 patch 前经过校验的游戏原始 bundle。
- `state.edn` 记录 baseline 的完整 fingerprint、最后一次成功安装的完整 fingerprint、对应 executable SHA-256、目标身份和当前状态。
- `transactions/current.edn` 是操作前原子写入的事务日志，用于识别和恢复中断操作。
- `transactions/history/` 保存成功、失败和回滚结果，但不备份每个 candidate；candidate 应由构建系统重现。

candidate 由 `build/AKInterface.bundle` 和相邻的 `build/AKInterface.bundle.manifest.json` 共同组成，不得复制到 `artifacts/` 充当安装状态记录。sidecar 不安装进游戏。

为了保证替换使用同一文件系统上的原子重命名，执行 `patch` 或 `restore` 时使用的 `.stage-*` 和 `.old-*` 临时目录必须短暂创建在游戏目标旁边。它们不是持久备份：成功后必须删除；若操作中断，`status` 必须结合 `transactions/current.edn` 识别这些残留。不得为了把临时文件放进 `artifacts/` 而放弃同卷原子替换。

路径边界为：

```text
持久状态与备份    artifacts/bundle/
构建候选产物      build/AKInterface.bundle
同卷事务临时文件  游戏 PlugIns/，成功后清除
```

#### Bundle 身份

状态判断必须区分三个相互独立的身份：

- `baseline`：首次受控替换前保存的游戏原始 bundle。
- `installed`：当前位于游戏 `PlugIns` 中的 bundle。
- `candidate`：当前 `build/AKInterface.bundle` 构建产物。

成功执行 `bb bundle build` 只可能更新 `candidate`。它不得修改 `baseline`、游戏中的 `installed` 或已记录的安装事务。

因此，重新编译产生了新的 candidate，而游戏中仍安装着上一次 patch 的 bundle 时，不属于漂移。状态应显示为 `patched`，并额外显示 candidate 有更新：

```text
Bundle status

Game              stopped
State             patched
Installed exec    <installed-executable-sha256>
Baseline exec     <baseline-executable-sha256>
Candidate exec    <candidate-executable-sha256>
Candidate status  update available
Signature         valid
Residue           none
```

#### 状态判定

状态不能通过比较 `installed` 与最新 `candidate` 得出。事务记录至少保存 `baseline` 和最后一次成功安装的 bundle 身份；`status` 再结合实际文件计算结果：

- `unmanaged`：尚未建立受信任的 baseline。
- `clean`：installed 与 baseline 的完整 fingerprint 相同。
- `patched`：installed 与最后一次成功 patch 记录的完整 fingerprint 相同。
- `drifted`：installed 既不匹配 baseline，也不匹配任何受信任的成功安装记录。
- `interrupted`：存在未完成事务，或存在能够关联到该事务的临时、旧版本残留。

`candidate` 与 installed 的关系单独显示：

- 完整 fingerprint 相同：`installed`。
- 完整 fingerprint 不同，且主状态是 `patched`：`update available`。
- candidate 不存在：`not built`。
- bundle 或 sidecar 只有一方存在，或重新计算结果不匹配：`invalid`。

每次 `patch` 成功后才更新最后一次成功安装的完整 fingerprint 和 executable SHA-256。构建失败或仅执行 `build` 时不得更新该记录。

#### `status`

`bb bundle status` 对游戏、bundle 和 `artifacts/` 是只读操作。唯一允许的写入是在 `local.edn` 缺失时创建空白模板并立即退出。配置有效后，它检查：

- 游戏是否正在运行。
- candidate sidecar、重新计算的完整 fingerprint、可执行文件 SHA-256 和签名。
- installed、baseline 和最后一次成功安装记录的完整 fingerprint；executable SHA-256 另外用于显示和人工批准。
- 是否存在未完成事务或受控临时路径残留。

`status` 不得捕获 baseline、清理残留、修复签名或修改任何 bundle。

#### `patch`

```sh
bb bundle patch --expected-executable-sha <candidate-executable-sha256>
```

`patch` 只接受已经由 `zig build bundle` 成功发布的完整 candidate。执行前必须满足：

- 游戏未运行。
- candidate bundle 与 sidecar 都存在，重新计算的完整 fingerprint、可执行文件 SHA-256、manifest 和签名相互一致。
- `--expected-executable-sha` 与 candidate manifest 中的 executable SHA-256 完全一致。
- installed 处于 `clean` 状态；或者 installed 同时匹配该 candidate 和最后一次成功 patch 记录，此时作为幂等成功处理。
- 不存在 `drifted`、`interrupted` 或无法解释的事务残留。

首次 patch 尚无 baseline 时，还必须提供 status 所报告的当前 installed 身份：

```sh
bb bundle patch \
  --expected-executable-sha <candidate-executable-sha256> \
  --expected-installed-executable-sha <current-installed-executable-sha256>
```

只有 installed 的 executable SHA-256 与 `--expected-installed-executable-sha` 一致，并且本次读取的完整 fingerprint 在复制后再次验证一致时，才能将它保存为 baseline。不得把未知或已经被外部修改的 bundle 自动认定为原始版本。

替换采用同卷事务：先在目标旁原样复制并验证完整 staged bundle，再依次将 installed 重命名为受控 old 路径、将 staged bundle 重命名为正式路径。staged bundle 不得重新签名；其完整 fingerprint 必须与构建 candidate 一致。最终验证失败时必须用 old 路径回滚。不得逐文件覆盖正式 bundle。

若已处于 `patched` 且 candidate 发生更新，应先执行 `restore` 回到 baseline，再安装新的 candidate。初版不直接在两个 patched 版本之间切换。

#### `restore`

`bb bundle restore` 将受信任的 baseline 事务性恢复到游戏 `PlugIns`：

- 游戏必须未运行。
- baseline 必须存在并通过记录的完整 fingerprint 校验。
- installed 必须匹配最后一次成功安装记录；若已经匹配 baseline，则只检查状态并作为幂等成功处理。
- `drifted` 状态下拒绝覆盖，初版不提供 `--force`。

恢复同样先创建并验证同卷 staged bundle，再进行重命名替换。不得先删除 installed 后直接复制 baseline，以免中途失败时留下不完整的正式路径。

`patch` 和 `restore` 的实际执行属于游戏修改操作，需要针对确切 executable SHA-256 的明确批准。完整 fingerprint 负责事务完整性；executable SHA-256 作为简短、明确的人工批准身份。task 的存在或先前执行过 `status` 不构成修改授权。

以上 status/patch/restore 与事务逻辑已实现于 `tools/bundle_ops.clj`（fingerprint 采用 BundleFingerprintV1 的结构化条目比较；`.stage-*`/`.old-*` 同卷原子换入；drifted/interrupted 一律拒绝）。`bb bundle selftest` 在 `build/tmp/` 的沙箱 `.app` 上全生命周期演练这套事务（unmanaged → patch 拒绝路径 → patch → patched → 幂等 → drift 拒绝 → restore → clean → interrupted 拒绝），不接触真实游戏；真实 patch/restore 仍需上述批准。

## `debug`

调用运行时 debug socket（JSON-RPC 2.0 over Unix domain socket，协议见 [Debug、Diagnostics 与 Logging](debug-diagnostics-logging.md)）：

```sh
bb debug runtime.plugins
bb debug fps.set '{"target":120}'
bb debug --socket <path> runtime.info
```

- socket 默认从 `local.edn` 推导：`.app` → `Info.plist` 的 bundle ID → 容器 `Documents/shiny-song-tools/debug.sock`；`--socket` 可显式覆盖。
- local.edn 不存在时自动创建空白模板并以非零状态退出；游戏未运行或 `debug.enabled` 未开时 socket 不存在，调用报 `socket-missing`。
- 响应为完整 JSON-RPC envelope（成功 `result`，失败 `error.code` + `data.code`），pretty print 输出。

交互式 REPL（推荐调试工作流）：

```sh
bb --init tools/debug_client.clj -r
;; => (call "runtime.plugins")
;; => (call "fps.set" {:target 120})
;; => (call "runtime.gates")
```

socket 解析顺序：显式传参 > `SCSP_DEBUG_SOCKET` 环境变量 > `local.edn` 推导。

## `fix-game`

`fix-game` 用于修复实验仓库中已经确认的一类 macOS saved-state 启动循环：游戏崩溃后，talagent 保存的 `restorecount.plist` 可能在后续启动时反复触发同一条失败恢复路径。

```sh
bb fix-game
```

该 task 只处理与当前游戏身份精确匹配的 talagent `restorecount.plist`。它不是通用崩溃修复，不删除整个 saved-state 目录，也不修改游戏 bundle、游戏数据或 carrier。

### 配置与定位

`fix-game` 必须先调用公共配置入口：

```clojure
(local-config/load!)
```

它从返回的游戏上下文取得 `.app`、bundle ID、签名身份和 executable 名称，不重复解析 `local.edn`。

talagent 的 daemon container 和 saved-state 目录包含本机生成的标识，不能硬编码。task 应扫描当前用户的 talagent `ApplicationMapping.plist`，用当前游戏的 bundle ID 或签名身份解析对应的 saved-state UUID，再将目标严格限定为：

```text
<matched-saved-state-uuid>.savedState/restorecount.plist
```

若映射、saved-state 目录或 live marker 匹配到多个候选，task 必须失败关闭并报告歧义，不得选择第一个结果。

### 修复流程

执行顺序为：

1. 确认游戏进程未运行。
2. 精确定位 live `restorecount.plist`，拒绝符号链接和路径越界。
3. 将原文件完整备份到本次专用的 artifact 目录。
4. 备份成功后，以可恢复方式隔离 live marker，并确认原路径不再存在。
5. 写入最终结果和备份位置。

持久证据和备份放在：

```text
artifacts/runtime-recovery/<transaction-id>/
├── restorecount.plist
└── manifest.edn
```

`manifest.edn` 至少记录游戏身份、原始路径、备份路径、操作时间和最终状态。`fix-game` 不计算或校验 marker 的 SHA-256。不得把本机 daemon container 路径或 saved-state UUID 写入版本化文件。

### 结果与失败行为

- live marker 不存在时，task 幂等成功并报告无需修复，不创建空备份。
- 游戏正在运行时，以非零状态退出且不修改任何文件。
- 配置无效、身份映射缺失或存在歧义时，以非零状态退出。
- 备份失败时，保留 live marker 并退出。
- 隔离过程中失败时，优先恢复原路径，并在 artifact manifest 中记录失败或回滚状态。
- 成功时只报告 artifact 备份路径，并明确不自动启动游戏验证。

实验结果只支持“隔离该 marker 修复了当次已观察到的启动故障”。`fix-game` 的成功仅表示可逆隔离事务完成，不证明当前或未来的崩溃都由 talagent saved state 引起。

实际执行 `fix-game` 会修改游戏对应的外部 saved state，需要明确批准；读取实验记录或运行其它只读 task 不构成该操作的授权。
