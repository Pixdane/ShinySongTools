# translation_dump 实机验证

## 最终结论(2026-08-30)

**✅ translation_dump 全链路实机验证通过,并已完成全量表 dump。**

- `dicdump` topic(Runtime 逆向版"上游 Waiting Extract Text"):捕获
  `LocalizationManager` 单例,手动遍历 `dic`(`Dictionary<string, Dictionary<int,string>>`,
  Newtonsoft 被裁剪不可用),**一次合并 5,613 类别 / 137,124 条文本**(123,850 条非空),
  `localify.json` 11.2MB 落盘——比上游底表(1,823 类 / 44,860 条)多 3 倍。
- 每调用增量采集(`GetText(2)` 主力 + `GetTextOrNull`)持续可用,与全量表互补。
- 收尾:restore 至 baseline(clean)。

### 全量表 vs 上游覆盖度(ShinyGroup/SCSPTranslationData)

- 上游底表仅覆盖我们实机表的 **29.0%**(39,711/137,124 槽位)——71% 的实机文本
  上游底表不存在(上游 dump 版本旧/覆盖面窄)。
- 上游已译(无假名 + 有汉字):4,425 条,占我们全量表的 **3.2%**
  (占上游自身底表的 10.2%)。剧情翻译(scenario/)仅 2 文件 46 条。
- 结论:自建汉化管线必要且数据基础已就绪;我们的 dump 也可反哺上游补槽。

## 技术里程碑(本实验验证的完整链路)

EntryPatch inline hook(签名 text 页整页 remap 替换)在实机游戏进程内安装、
运行、与槽替换 hook 并存,均无崩溃;runtime 全链路(bootstrap → 插件 →
scheduler → debug socket → 采集 → 原子落盘)可用。

已知问题:类名含反引号的 recon 查询会使 debug 服务端连接崩溃(待修);
Newtonsoft SerializeObject 被 iOS 裁剪,序列化走手动字典遍历。

## 证据

### 尝试 6(2026-08-30,歌词捕获attempt + TMP 兜底)

- 新增 `TMP_Text.set_text` 兜底 hook(所有 TMP 显示文本必经)。
- 播放两个 MV 后歌词仍零捕获;`DataFile.GetBytes` 被动层在 MV 期间也未抓到
  歌词数据文件(MV 资源不经 DataFile)。三个假设均被否定:
  TMP 基类 setter / UpdateLyrics / SetLyric 在 iOS 上都不是歌词路径。
- recon 查询 `LiveMVOverlayView` / `TextMeshProUGUI` / `Dictionary\`2` 会崩溃
  debug 服务端(socket closed by peer)——特定类名查询触发 panic 且 transport
  层无防护,**阻塞后续侦察,列为待修**。
- 收尾:restore 至 baseline(clean)。

### 歌词下一步

1. 修 debug transport 的 panic 防护(handler panic 已有边界,transport 层没有);
2. recon 类表面 + 调用者扫描:`LiveMVOverlayView`、`TimelineController`、
   `TextMeshProUGUI` 的派生类 set_text;
3. 备选:歌词可能是资源 bundle 内的 TextAsset(MV 资源不走 DataFile),
   需要资产层解析(UnityFS 明文,可离线)。

### 尝试 5(2026-08-30,歌词/剧情/DataFile 采集)

- 分支族重定位引擎上线(cbz/cbnz/b.cond/tbz/tbnz/b/bl 变长跳板),歌词 hook 不再
  被分类器拒绝——5 个 hook 全部实机安装成功。
- **剧情全量 dump 成功**:`scenariodump` topic 从 dic 的 1,533 个 `mlADVInfo_Title_*`
  sid 主动探测 `DataFile.GetBytes`,共落盘 **3,461 个剧情文本文件**(15MB,含
  60 个镜头/动作等数据文件)。注意:游戏数据 JSON 是带尾逗号的宽松格式
  (上游 fmtAndDumpJsonBytesData 同款问题),消费端需容错解析。
- **被动 `DataFile.GetBytes` hook**:拷贝 bytes 直写回调上下文(上游同款),采到
  3,521 个文件中的 60 个非剧情数据文件。
- **歌词零捕获**:MV 播放后 `lyrics_dump.json` 未生成。recon 显示
  `TimelineController.SetLyric` 在 iOS 上 0 个直接调用者——iOS 歌词路径与 DMM
  不同,需要继续侦察(候选:其他歌词显示类/方法)。
- **事故与修复**:第一轮探测每次同步阻塞主线程处理 150 个 sid 导致游戏"未响应";
  worker 从主线程重入 GetBytes 有死锁风险。已改为:回调直写文件(上游同款,
  ≤1MB 限幅)+ 探测限流(每批 25 sid,文档注明会短暂冻结)。
- 收尾:游戏 kill + restore 至 baseline(clean)。

### 尝试 4(2026-08-30,GetText(2) 双 hook 验证)

- bundle `44fcab2b…`(GetTextOrNull + GetText(2) 双 EntryPatch),patch/启动正常,
  `plugin build ok name=translation_dump`。
- **启动即 `get_text_hits=97`(87 条入册)**;游玩约 6 分钟:`get_text_hits=23,646、
  hook_hits=1,973、merged=3,595、entries=1,157、categories=187、自动 flush 50 次`。
- `translation_dump.flush` 确认无未落盘数据;`localify.json` 217KB,类别名如
  `mlADVInfo_Summary_s01_*`,值为 id → 文本,scsp-localify 兼容形状。
- 收尾:停止游戏 + restore 至 baseline(clean);dump 文件备份至 /tmp。

### 尝试 3(2026-08-30,EntryPatch 机制实机验证)

- bundle 重建:candidate `114c53b0b020678dcb01c63b6eb6572e84d0cd599ddefb91db3bc195bffca7ff`,
  patch/staging 成功(需显式 `--expected-executable-sha`),签名有效、无残留。
- 启动日志:bootstrap 全链路正常;**`plugin build ok name=translation_dump` 在
  EntryPatch 机制下意味着对 live 游戏进程签名 text 页的入口补丁安装成功**
  (解析、序言分类、跳板构建、remap 替换、回读全部通过)。
- 用户阅读文本(剧情类)→ `hook_hits` 0;导航 UI 菜单界面 → `hook_hits` 仍 0。
  scheduler 全程存活(frame 11343+,LateUpdate 槽 hook 每帧命中)。
- 补丁位置语义已离线核实:il2cpp-bridge-rs 的 `Method.address` = MethodInfo 结构体
  地址,第一字段(偏移 0)= methodPointer = 编译后函数入口,与 AOT 直接调用目标一致。
- 收尾:游戏停止;**bundle restore 到 baseline(状态 clean)**——本次 patch 身份变更,
  按规则必须恢复。
- 无 `dumps/localify.json` 生成。

### 尝试 1/2(2026-08-30 前次,槽替换机制)

同样配置与 SHA,插件注册成功、游戏存活,`hook_hits` 全 0(当时归因于槽机制缺陷)。

## 根因分析

1. **入口身份无误**(与上游 scsp-localify 当前 main 逐字段核对):
   `PRISM.Legacy.dll / ENTERPRISE.Localization / LocalizationManager / GetTextOrNull / 2 参数`。
   上游即在此方法内返回译文并生效;其 v1.3.13(游戏 v2.16.0 hook 修复)未改动该目标。
2. **解析与安装成功**:`MethodPointerSlot::install` 走 CAS + 回读确认,失败会让插件 build 报错;
   日志显示 `plugin build ok name=translation_dump`,说明槽已写入。
3. **机制不匹配**(根因):`crates/core/src/method_slot.rs` 的 hook 是替换
   `MethodInfo.methodPointer` 槽,只拦截**经槽分派**的调用(虚/接口分派、委托、反射、
   Unity 生命周期回调)。游戏 C# 代码对 `GetTextOrNull` 的普通调用是 IL2CPP AOT
   直接分支到函数入口,不读槽。
   旁证:scheduler 的 LateUpdate hook(Unity 经槽调用)每帧命中,文本 hook 零命中。
4. 上游之所以能生效,是因为它用 MinHook 在**函数入口做 inline trampoline**,对直接调用同样有效。

## 下一步(诊断方向)

入口补丁机制已验证可用(尝试 3 的 `plugin build ok` 即补丁成功),现在的阴性结果
指向"该入口在 iOS 版未被调用"或"补丁地址仍非真实调用目标",需要区分:

1. **运行时自证**:给 runtime 加一个 debug topic,上报解析到的 method_info /
   function_ptr / rva 与补丁前后入口字节,确认补丁位置;离线用 otool 反汇编
   游戏二进制,交叉核对 rva 并搜索 `bl` 调用者——直接回答"iOS 版是否调用
   GetTextOrNull"。
2. ** Frida 对照**(需单独批准):attach 后用 Interceptor 挂同一地址,独立于我们的
   hook 观察调用活动,一次区分"补丁问题"与"入口本身死代码"。
3. **换/加入口**:上游对剧情文本走独立的 scenario dump(`dumpAllScenarioData`),
   `GetTextOrNull` 可能只覆盖部分 UI 类别;考虑读 `LocalizationManager.dic` 字段
   整体序列化的方案,或 hook 实际被调用的文本管线入口。

## 运行时逆向发现(2026-08-30,尝试 3 同批次的 recon 插件)

新增 `recon` dev 插件(`recon.class` / `recon.callers`)后的关键事实:

- `ENTERPRISE.Localization.LocalizationManager` 方法面(iOS 2.17.0):
  `static get_Instance()`、`Load()`、`_load(1)`、`GetTextSlow(1)`、`GetText(1)`、
  `GetText(2)`、`GetTextOrNull(2)`、`.ctor`;实例字段 `dic`(+0x10,
  `Dictionary<String, Dictionary<…>>`,与上游一致)。类无静态字段。
- **`GetTextOrNull` 有 10 个直接 `bl` 调用点;`GetText(2)` 有 896 个** ——
  UI 文本主力是 `GetText(2)`,这解释了两次会话菜单/剧情导航中 GetTextOrNull 零命中
  (GetTextOrNull 只覆盖少数场景)。
- il2cpp-bridge-rs 缓存的 `Method.rva` 字段使用错误基址(与 dyld header 不一致),
  recon 门面已改为按 `va - image_base` 自行推导;`Method.va`/slot 值可靠
  (EntryPatch 安装回读验证)。
- 用户导航后 `hook_hits` 仍 0(当时 hook 在 GetTextOrNull 上)→ 与调用点分布一致。

## 采取措施

- translation_dump 新增 `GetText(2)` EntryPatch hook(弹性安装:签名漂移时仅告警
  跳过),与 GetTextOrNull 并行;status 增加 `get_text_hits`。
- recon 门面修正 rva 推导并输出参数类型名。

## 环境注意

- 本机 zsh 的 `log` 是内建命令,查 Unified Log 必须用 `/usr/bin/log`;且必须带
  `--info --debug`(runtime 日志为 Debug 级)。
- debug 通道:`nix develop --offline -c bb debug translation_dump.status '{}'`。
