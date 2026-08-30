# 入口 inline hook 设计(sighook 方案)

状态:设计定稿,待可行性实验验证。前置阅读:`experiments/translation-dump-live-validation.md`
(MethodPointer 槽替换拦不住 IL2CPP AOT 直接调用的根因)。

## 目标

为 core 增加函数入口 inline hook 能力,拦截 AOT 直接调用;**插件侧 API 保持与槽替换完全一致**
(`HookTarget` / `define_hook_site!` / typestate builder / `dispatch` / `InstalledHook` /
restore ledger 全部不动),机制差异收敛在 `SlotMemory` 抽象的一个新实现里。

## 可行性前提(已核实,2026-08-30)

- 游戏 bundle 与 UnityFramework 均为 `flags=0x2(adhoc)` 签名,无 hardened runtime,
  进程内 `vm_protect`/`mach_vm_remap` 改写自身 text 页不被内核拒绝(Frida 可用即旁证)。
- 风险:**AppGuard 完整性扫描**。代码页补丁是经典篡改特征,暴露面大于槽替换
  (槽只动元数据)。每次实机验证按 AGENTS.md 规则另行申请批准,本文档不授权任何游戏操作。
- 上游旁证(2026-08-30 核对 scsp-localify `hook.cpp`):DMM(PC)版带同厂商反作弊
  nProtect GameGuard(`PreInitNPGameMonW`、`InitNPGameMon`、托管侧 `GGIregualDetector`),
  而上游用 MinHook 长期 inline hook 甚至直接中和 GameGuard 回调(`NPGameMonCallback`
  恒返回 1、`InitNPGameMon` 返回假成功),工具链存活多年。说明该游戏反作弊未做
  激进的入口代码完整性阻断。注意:移动端 AppGuard 是不同产品,策略可能更严,
  该旁证只降低风险估计,不豁免实机验证流程。

## crate 选型(2026-08-30 可行性实验后修订)

**不依赖任何 hook crate,core 自研最小引擎(~200 行)。** 可行性实验
(`experiments/entry-patch/`)推翻了 sighook 方案:

- sighook 0.10 的 `inline_hook_jump` 在签名 text 页上 `mach_vm_protect(R|W|VM_PROT_COPY)`
  返回成功但写入 SIGBUS(KERN_PROTECTION_FAILURE),其 aarch64-apple-darwin 支持不覆盖
  本场景;有价值的重定位逻辑是 crate 私有,拿不到。
- 验证通过的替代:自有 RWX-max 暂存页(MAP_JIT,写完降 R|X)+ 整页内容拷入打补丁 +
  `mach_vm_remap(VM_FLAGS_OVERWRITE, copy=TRUE)` 替换回 text 地址 + 数据侧读回/isb 收尾。
- 备选退役:retour(仅 x86)、frida-gum 绑定(重依赖)、anglerkit(不成熟)均无需再评估。
- LGPL-2.1 依赖随 sighook 一起消失,许可问题不复存在。

实验证据与生产指导:`experiments/entry-patch/README.md`。

| 候选 | arm64-darwin | 结论 |
| --- | --- | --- |
| **自研 remap 引擎** | ✅ 实验验证通过 | **选它**,无外部依赖 |
| sighook | 补丁写路径在本机崩溃 | 退役 |
| retour(主线) | ❌ 仅 x86/x86_64 | fork 分支未合并,不用 |
| frida-gum 绑定 | ✅ 成熟 | 拖整个 frida-gum C 库,伤害仓库自包含,不用 |
| anglerkit | roadmap | 不成熟,观望 |

## 架构:机制差异收敛在 `SlotMemory` 后面

框架四操作语义(`method_slot.rs::SlotMemory`)与入口补丁一一对应:

```text
SlotMemory 操作            EntryPatchMemory 实现
─────────────────────      ─────────────────────────────────────────────
read()                     探测入口补丁状态:已装 → Some(replacement 地址)
                           原始态 → Some(入口地址);该"虚拟状态字"即所有权真值
compare_exchange(原, 新)    原始态下:sighook 在入口安装 detour(重定位序言 +
                           跳板)并回读验证;非原始态一律拒绝(不写)
bind()                     捕获入口地址与序言字节;跳板地址 = typed original
restore(新)                仅当当前补丁属于本 hook 时移除、恢复原始指令
```

要点:

1. **original 的含义变了,接口没变**:槽替换下 original 是槽内旧指针;inline 下是
   跳板地址(重定位序言 + 跳回入口+N)。`T::original_from_raw(跳板地址)` 照常工作,
   插件的类型化 ABI 不变。
2. **入口补丁是槽替换的严格超集**:直接调用跳到入口(被拦);经槽分派的调用读出的
   槽值仍是入口地址,同样跳进被补丁的入口。inline 路径下槽完全不动,消除双状态漂移。
3. **dispatch 真值检查不变**:`current() == Some(replacement_addr)`,安装/恢复竞态
   窗口语义与现槽实现相同(补丁可达期间,original 始终可经跳板 passthrough)。
4. `SiteInner` / `ActiveHook` / `HookState` / restore ledger / 门控 / exactly-once
   契约零改动。

## 代码落点(2026-08-30 集成后实际状态)

```text
crates/core/src/entry_patch.rs        # 新:EntryPatchMemory(SlotMemory impl),
                                      #   cfg(target_arch = "aarch64"),含单测
                                      #   (JIT 暂存页整页 remap、跳板、分类器)
crates/core/src/method_slot.rs        # +HookMechanism 枚举(槽替换 / 入口补丁)
crates/core/src/error.rs              # +HookError::EntryPatchUnsupported
crates/core/src/backend.rs            # MethodResolver::entry_patch_memory(默认不支持)
crates/core/src/il2cpp_bridge.rs      # BridgeBackend 实现 entry_patch_memory
crates/core/src/plugin_api/hook.rs    # HookTarget::const MECHANISM(默认 MethodPointerSlot);
                                      #   install() 按机制取 SlotMemory,其余协议不变
crates/translation_dump/src/targets.rs # MECHANISM = EntryPatch(唯一开关点)
```

与原设计的差异:未引入 cargo feature——无重依赖后,按 `target_arch` cfg 门控即可,
x86_64 上 resolver 默认返回 `EntryPatchUnsupported`。机制声明放在 `HookTarget`
而不是 builder:机制是目标的静态属性(由目标被调用的方式决定),与
container/handler 这类运行时装配不同;typestate 面不膨胀。

## 残余风险与缓解

- **安装竞态**:其他线程正在执行入口指令时有撕裂窗口。缓解:bootstrap 阶段尽早
  安装(此时目标方法几乎未被调用),与所有 inline hook 库的既有做法一致;契约写进
  `entry_patch.rs` 模块文档。
- **PC 相对指令形态**:入口序言含 adrp/adr/b/bl/b.cond/cbz/tbz/literal load/ret 时
  不可 verbatim 搬迁,安装 fail closed(`HookError::EntryPatchUnsupported`);
  常见形态的重定位引擎是后续增强。
- **跳板内存**:每个 hook 目标一个 mmap 页,进程期存活,与"一个目标一个静态 site"
  的 retention-root 语义一致。
- **I-cache 可见性**(实验教训):补丁收尾必须包含数据侧读回 + `isb`,已固化在
  `remap_patch` 中;实机验证时需再次确认。

## 验证计划

1. ✅ **可行性实验(不碰游戏,无需批准)**:结论——sighook 写路径在本机 SIGBUS,
   不可用;自研 remap + JIT 暂存页 + 跳板方案 8/8 稳定通过。证据与生产指导见
   `experiments/entry-patch/README.md`。
2. ✅ **框架集成(2026-08-30)**:core `EntryPatchMemory` + `HookMechanism` +
   `HookTarget::MECHANISM` + resolver 接入;单测在真实可执行页上驱动完整
   install/hook/restore/冲突/drift 协议。fmt/clippy(-D warnings)/test 全 workspace 通过。
3. **实机验证**:translation_dump 已切 EntryPatch 机制;待重新构建 bundle 并申请启动
   批准,复测 `hook_hits`(预期:进入文本页面后 > 0),flush 验证 `dumps/localify.json`。
