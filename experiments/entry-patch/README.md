# entry-patch 可行性实验结果

日期:2026-08-30。设计与背景见 `experiments/entry-patch-inline-hook.md`。
运行:`cargo build --release`(产物在 `build/experiments/entry-patch/`),直接执行二进制。

## 结论

**入口 inline hook 在本机(aarch64-darwin,adhoc 签名进程)完全可行,但不经 sighook。**
自研的最小引擎(remap 页替换 + JIT 暂存页 + 单页跳板 + 规范 icache 同步)全部验证通过,
最终套件 8/8 稳定通过(3 个近跳用例 + 1 个强制远跳用例,含并发不变量)。

## 关键事实(按发现顺序)

1. **sighook 0.10 的写路径在本机不可用**:`inline_hook_jump` 对签名 text 页执行
   `mach_vm_protect(R|W|VM_PROT_COPY)`——protect 返回成功,随后 memcpy 写入
   SIGBUS(KERN_PROTECTION_FAILURE,见 DiagnosticReports)。它对 aarch64-apple-darwin
   的"全 API 支持"不覆盖签名进程内 text 页 in-place 写这个场景。
2. **remap 副本提权也不行**:copy=TRUE 拷出的页 max_protection 继承源页(仅 R|X),
   `mach_vm_protect` 提权到 RW 直接 KERN_PROTECTION_FAILURE。
3. **可行路径 = Frida 同款页替换**:自有 RWX-max 暂存页(MAP_JIT;写入时 RW,
   写完 mprotect 降为 R|X,JIT 映射豁免 W^X),把整页内容拷入暂存页打补丁,
   `mach_vm_remap(VM_FLAGS_OVERWRITE, copy=TRUE)` 替换回原 text 地址。
   全程不 in-place 写签名页、不做 W→X 提权。
4. **跳板(trampoline)验证通过**:复制被覆盖指令 + `ldr x16/br x16` + 绝对跳回
   `entry+N`,作为 typed original 调用,结果与 baseline 逐一一致。
5. **⚠ I-cache 可见性陷阱(本实验最重要的教训)**:`sys_icache_invalidate` +
   规范 `dc cvau`/`ic ivau`/`dsb ish`/`isb` 序列之后,**仍观察到补丁后前两次
   执行命中陈旧指令**;同一段代码加入"补丁后数据侧读回 + isb"后 8/8 稳定。
   生产实现必须把"数据侧读回校验 + isb"作为 `remap_patch` 的标准收尾,
   且上线前要在真机游戏进程里再次确认(本机 M 系列行为不代表所有目标机)。
6. **sighook 复现探针的补充事实**:在本引擎替换过的页(匿名映射)上,
   sighook 的写路径反而能成功(`SIGHOOK_PROBE=1` 不再崩溃)——印证根因是
   **file-backed 签名页 vs 匿名页**的写保护差异,不是 protect API 本身。
7. 并发:hook 生效期间 4 线程 × 300ms 锤调用,结果不变量
   (返回值 ∈ {original, 2×original})零违约,进程存活。

## 对生产实现的直接指导(core 的 EntryPatchMemory)

- 补丁原语:近跳 `b`(±128MB 内,覆盖 1 条指令)/ 远跳 `ldr x16, #8; br x16; .quad target`
  (覆盖 4 条指令)——两路均已验证。
- 跳板:mmap RW 页 → 复制 N 条指令 + ldr/br/绝对跳回 → mprotect R|X → icache 同步。
- `unhook` 等价物:同一 `remap_patch` 写回 bind 时保存的原始字节(已验证)。
- 安装前必须分类被覆盖指令:PC 相对/控制流形态(adrp/adr/b/bl/b.cond/cbz/tbz/
  literal load/ret)不可 verbatim 搬迁,要么重定位要么拒绝安装(实验只验证了
  拒绝路径的判定逻辑,重定位引擎是 core 集成阶段的工作)。
- sighook 依赖可以完全移除:其可用部分(encode_b、跳转编码)各 ~20 行,
  有价值的重定位逻辑(replay.rs)是 crate 私有的,拿不到。LGPL 依赖随之消失。

## 运行方式

```sh
cd experiments/entry-patch
CARGO_TARGET_DIR=<repo>/build/experiments/entry-patch cargo build --release
<repo>/build/experiments/entry-patch/release/entry-patch-feasibility
# 可选:SIGHOOK_PROBE=1 复现 sighook 在已替换页上成功的新事实
# 可选:FAR_ONLY=1 只跑远跳用例
```
