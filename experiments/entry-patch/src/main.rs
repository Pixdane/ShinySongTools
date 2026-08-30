//! 入口补丁可行性实验(aarch64-darwin,不碰游戏)。
//!
//! 结论先行(2026-08-30):
//! - sighook 0.10 的 `inline_hook_jump` 在 Apple Silicon 上对签名 text 页
//!   `mach_vm_protect(R|W|VM_PROT_COPY)` 返回成功但写入 SIGBUS
//!   (KERN_PROTECTION_FAILURE,见 crash report)——其写路径对本机不可用。
//! - 可行路径是 Frida 同款 `mach_vm_remap` 页替换:把目标页拷为匿名映射、
//!   在副本上改、再 remap 回原地址。本程序自实现该路径并验证。
//!
//! 验证项:
//! 1. remap 补丁可写签名 text 页且生效;
//! 2. 自建单页跳板(复制被覆盖指令 + 绝对跳回)作为 typed original 正确工作;
//! 3. 补丁回写原始字节即完全恢复;
//! 4. 补丁生效期间多线程并发调用,进程存活且结果始终合法;
//! 5. PC 相对指令检测:识别不可 verbatim 搬迁的序言并拒绝安装。
//!
//! `SIGHOOK_PROBE=1` 时末尾复现 sighook 写路径崩溃(预期 SIGBUS)。

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
use std::thread;
use std::time::Duration;

const LDR_X16_LITERAL_8: u32 = 0x5800_0050; // ldr x16, #8
const BR_X16: u32 = 0xD61F_0200; // br x16
const PACIBSP: u32 = 0xD503_237F;

const VM_FLAGS_OVERWRITE: i32 = 0x4000;
const VM_INHERIT_COPY: u32 = 1;

static HITS: AtomicUsize = AtomicUsize::new(0);

type KernReturn = i32;
type MachPort = u32;

extern "C" {
    fn mach_task_self() -> MachPort;
    fn mach_vm_remap(
        target_task: MachPort,
        target_address: *mut u64,
        size: u64,
        mask: u64,
        flags: i32,
        src_task: MachPort,
        src_address: u64,
        copy: i32,
        current_protection: *mut i32,
        max_protection: *mut i32,
        inheritance: u32,
    ) -> KernReturn;
    fn sys_icache_invalidate(start: *mut libc::c_void, len: usize);
}

unsafe fn page_size() -> usize {
    let n = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    assert!(n > 0);
    n as usize
}

static STAGE: AtomicUsize = AtomicUsize::new(0);

/// 暂存页:MAP_JIT 匿名页。写入时为 RW,写完降为 R|X(JIT 映射豁免 W^X 限制),
/// 这样 OVERWRITE remap 回 text 地址的映射天生 R|X,不再有任何 W→X 提权。
unsafe fn staging_page() -> Result<usize, String> {
    let cached = STAGE.load(Ordering::Relaxed);
    if cached != 0 {
        return Ok(cached);
    }
    let ps = unsafe { page_size() };
    let mem = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            ps,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANON | libc::MAP_JIT,
            -1,
            0,
        )
    };
    if mem == libc::MAP_FAILED {
        return Err(format!("mmap staging failed, errno={}", unsafe {
            *libc::__error()
        }));
    }
    STAGE.store(mem as usize, Ordering::Relaxed);
    Ok(mem as usize)
}

/// arm64 规范的自修改代码同步序列(ARM ARM):对目标 VA 逐 cache line
/// clean D-cache 到 PoU、invalidate I-cache,再 dsb+isb。
#[cfg(target_arch = "aarch64")]
unsafe fn sync_icache(start: usize, len: usize) {
    let line = 64usize;
    let first = start & !(line - 1);
    let end = (start + len + line - 1) & !(line - 1);
    let mut va = first;
    while va < end {
        unsafe {
            core::arch::asm!(
                "dc cvau, {a}",
                "ic ivau, {a}",
                a = in(reg) va,
                options(nostack)
            );
        }
        va += line;
    }
    unsafe {
        core::arch::asm!("dsb ish", options(nostack));
        core::arch::asm!("isb", options(nostack));
    }
}

/// 用 `mach_vm_remap` OVERWRITE 把改好的整页替换到签名 text 地址。
/// 全程不直接写签名页、不做 W→X 提权:改写发生在 RW 状态的 JIT 暂存页,
/// 降为 R|X 后由内核完成整页替换。
unsafe fn remap_patch(addr: usize, bytes: &[u8]) -> Result<(), String> {
    let ps = unsafe { page_size() };
    let page = addr & !(ps - 1);
    let off = addr - page;
    assert!(off + bytes.len() <= ps, "experiment patch stays in one page");
    let task = unsafe { mach_task_self() };
    let stage = unsafe { staging_page()? };

    // 1. 暂存页转 RW,拷入目标页内容并打补丁,再降回 R|X(JIT 映射允许该转换)。
    unsafe {
        if libc::mprotect(stage as *mut libc::c_void, ps, libc::PROT_READ | libc::PROT_WRITE)
            != 0
        {
            return Err(format!("stage->RW failed errno={}", *libc::__error()));
        }
        std::ptr::copy_nonoverlapping(page as *const u8, stage as *mut u8, ps);
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), (stage + off) as *mut u8, bytes.len());
        if libc::mprotect(stage as *mut libc::c_void, ps, libc::PROT_READ | libc::PROT_EXEC)
            != 0
        {
            return Err(format!("stage->RX failed errno={}", *libc::__error()));
        }
        sys_icache_invalidate((stage + off) as *mut libc::c_void, bytes.len());
    }

    // 2. OVERWRITE remap:把暂存页对象映射到原 text 地址(copy=true,暂存页可复用)。
    let mut remote = page as u64;
    let mut cur: VmProt = 0;
    let mut max: VmProt = 0;
    let kr = unsafe {
        mach_vm_remap(
            task,
            &mut remote,
            ps as u64,
            0,
            VM_FLAGS_OVERWRITE,
            task,
            stage as u64,
            1, // copy
            &mut cur,
            &mut max,
            VM_INHERIT_COPY,
        )
    };
    if kr != 0 {
        return Err(format!("remap overwrite: kr={kr}"));
    }

    unsafe { sys_icache_invalidate(page as *mut libc::c_void, ps) };
    unsafe { sync_icache(page + off, bytes.len()) };
    // 数据侧读回校验:既是补丁完整性检查,也保证执行前对该页有一次真实访问。
    for (i, &b) in bytes.iter().enumerate() {
        let got = unsafe { std::ptr::read_volatile((page + off + i) as *const u8) };
        assert_eq!(got, b, "patch readback mismatch at +{i}");
    }
    unsafe {
        core::arch::asm!("isb", options(nostack));
    }
    Ok(())
}

/// 入口补丁字节:near 直接 `b replacement`,超程用 ldr x16/br x16 + 8 字节绝对地址。
fn entry_patch_bytes(entry: usize, replacement: usize) -> Vec<u8> {
    let offset = replacement as i128 - entry as i128;
    let imm26 = offset >> 2;
    if offset & 0b11 == 0 && (-(1i128 << 25)..(1i128 << 25)).contains(&imm26) {
        (0x1400_0000u32 | (imm26 as u32 & 0x03FF_FFFF)).to_le_bytes().to_vec()
    } else {
        let mut bytes = Vec::with_capacity(16);
        bytes.extend_from_slice(&LDR_X16_LITERAL_8.to_le_bytes());
        bytes.extend_from_slice(&BR_X16.to_le_bytes());
        bytes.extend_from_slice(&(replacement as u64).to_le_bytes());
        bytes
    }
}

unsafe fn read_bytes(entry: usize, n: usize) -> Vec<u8> {
    (0..n)
        .map(|i| unsafe { std::ptr::read_volatile((entry + i) as *const u8) })
        .collect()
}

// --- 实验目标函数(#[no_mangle] 保证符号与地址稳定) ---

#[no_mangle]
#[inline(never)]
pub extern "C" fn ep_plain_sum(a: i64, b: i64) -> i64 {
    a.wrapping_add(b)
}

#[no_mangle]
#[inline(never)]
fn ep_inner(x: i64) -> i64 {
    x.rotate_left(7) ^ 0x5A5A
}

#[no_mangle]
#[inline(never)]
pub extern "C" fn ep_frame_fn(a: i64, b: i64) -> i64 {
    // 强制栈帧 + 内部调用,得到典型 stp/sub 序言。
    let buf = [a, b, a ^ b, a.wrapping_mul(3)];
    ep_inner(buf.len() as i64) + buf.iter().sum::<i64>()
}

static EP_GLOBAL: AtomicI64 = AtomicI64::new(1234);

#[no_mangle]
#[inline(never)]
pub extern "C" fn ep_global_fn(a: i64, _b: i64) -> i64 {
    // 预期序言含 adrp/literal load,应被分类器拒绝(演示检测路径)。
    EP_GLOBAL.load(Ordering::Relaxed).wrapping_add(a)
}

// --- 最小跳板 ---

/// 复制 `insns` 到新的可执行页,尾部 ldr x16/br x16 绝对跳回 `entry + 4*n`。
/// 仅对 verbatim 安全的指令成立(由 `prologue_is_verbatim_safe` 保证)。
unsafe fn build_trampoline(entry: usize, insns: &[u32]) -> Result<usize, String> {
    let ps = unsafe { page_size() };
    let mem = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            ps,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANON,
            -1,
            0,
        )
    };
    if mem == libc::MAP_FAILED {
        return Err(format!("mmap failed, errno={}", unsafe { *libc::__error() }));
    }
    let base = mem as usize;
    for (i, &op) in insns.iter().enumerate() {
        unsafe { std::ptr::write_unaligned((base + i * 4) as *mut u32, op.to_le()) };
    }
    let n = insns.len();
    unsafe {
        std::ptr::write_unaligned((base + n * 4) as *mut u32, LDR_X16_LITERAL_8.to_le());
        std::ptr::write_unaligned((base + n * 4 + 4) as *mut u32, BR_X16.to_le());
        std::ptr::write_unaligned((base + n * 4 + 8) as *mut u64, (entry + n * 4) as u64);
    }
    if unsafe { libc::mprotect(mem, ps, libc::PROT_READ | libc::PROT_EXEC) } != 0 {
        return Err("mprotect trampoline failed".into());
    }
    unsafe { sys_icache_invalidate(mem as *mut libc::c_void, n * 4 + 16) };
    Ok(base)
}

/// 实验级分类器:检测不可 verbatim 搬迁的 PC 相对/控制流指令。
/// 生产实现应换成真正的重定位引擎;此处只做"识别并拒绝"。
fn prologue_is_verbatim_safe(ops: &[u32]) -> Result<(), &'static str> {
    for &op in ops {
        if op == PACIBSP {
            continue; // 非 PC 相对;签 LR/SP 语义与执行位置无关
        }
        let top6 = op >> 26;
        if top6 == 0b000101 || top6 == 0b100101 {
            return Err("b/bl in prologue");
        }
        let top8 = op >> 24;
        if top8 & 0xFE == 0x54 {
            return Err("b.cond in prologue");
        }
        if top8 & 0x9F == 0x10 || top8 & 0x9F == 0x90 {
            return Err("adr/adrp in prologue");
        }
        if top8 & 0x1F == 0x18 {
            return Err("literal load in prologue");
        }
        if top8 & 0x7E == 0x34 {
            return Err("cbz/cbnz in prologue");
        }
        if top8 & 0x7E == 0x36 {
            return Err("tbz/tbnz in prologue");
        }
        if op & 0xFFFF_FC1F == 0xD65F_0000 {
            return Err("ret in prologue");
        }
    }
    Ok(())
}

type TargetFn = extern "C" fn(i64, i64) -> i64;

fn call_at(addr: usize, a: i64, b: i64) -> i64 {
    let f: TargetFn = unsafe { std::mem::transmute(addr) };
    f(a, b)
}

static TRAMP_PLAIN: AtomicUsize = AtomicUsize::new(0);
static TRAMP_FRAME: AtomicUsize = AtomicUsize::new(0);
static TRAMP_GLOBAL: AtomicUsize = AtomicUsize::new(0);

extern "C" fn ep_plain_sum_replacement(a: i64, b: i64) -> i64 {
    HITS.fetch_add(1, Ordering::Relaxed);
    call_at(TRAMP_PLAIN.load(Ordering::Relaxed), a, b).wrapping_mul(2)
}

extern "C" fn ep_frame_fn_replacement(a: i64, b: i64) -> i64 {
    HITS.fetch_add(1, Ordering::Relaxed);
    call_at(TRAMP_FRAME.load(Ordering::Relaxed), a, b).wrapping_mul(2)
}

extern "C" fn ep_global_fn_replacement(a: i64, b: i64) -> i64 {
    HITS.fetch_add(1, Ordering::Relaxed);
    call_at(TRAMP_GLOBAL.load(Ordering::Relaxed), a, b).wrapping_mul(2)
}

/// 并发 worker 的完整性不变量:被 hook 函数的返回值只能是
/// original(未 hook)或 2×original(hook 生效),绝不允许其它值。
fn hammer(entry: usize, tramp: usize, stop: &AtomicBool, out: &AtomicUsize) {
    let mut i = 0i64;
    let mut bad = 0usize;
    while !stop.load(Ordering::Relaxed) {
        let original = call_at(tramp, i, 3);
        let got = call_at(entry, i, 3);
        if got != original && got != original.wrapping_mul(2) {
            bad += 1;
        }
        i = i.wrapping_add(1);
    }
    out.fetch_add(bad, Ordering::Relaxed);
}

extern "C" {
    fn pthread_jit_write_protect_np(enabled: i32);
}

unsafe fn probe_wx() {
    let ps = unsafe { page_size() };
    let m = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            ps,
            libc::PROT_READ | libc::PROT_WRITE | libc::PROT_EXEC,
            libc::MAP_PRIVATE | libc::MAP_ANON | libc::MAP_JIT,
            -1,
            0,
        )
    };
    if m == libc::MAP_FAILED {
        println!("probe: mmap RWX+MAP_JIT failed errno={}", unsafe {
            *libc::__error()
        });
        return;
    }
    println!("probe: mmap RWX+MAP_JIT at {:#x}", m as usize);
    unsafe { pthread_jit_write_protect_np(0) };
    unsafe { std::ptr::write_volatile(m as *mut u8, 0xC3) };
    println!("probe: write with jit_write_protect(false) ok");
    unsafe { pthread_jit_write_protect_np(1) };
    println!("probe: jit_write_protect(true) ok");
    unsafe { libc::munmap(m, ps) };
}

struct Case {
    name: &'static str,
    entry: usize,
    replacement: usize,
    trampoline_slot: &'static AtomicUsize,
}

fn main() {
    unsafe { probe_wx() };

    // 补丁字节宽度决定被覆盖的指令数:near 1 条,远跳 4 条。
    let cases = [
        Case {
            name: "plain_sum",
            entry: ep_plain_sum as TargetFn as usize,
            replacement: ep_plain_sum_replacement as TargetFn as usize,
            trampoline_slot: &TRAMP_PLAIN,
        },
        Case {
            name: "frame_fn",
            entry: ep_frame_fn as TargetFn as usize,
            replacement: ep_frame_fn_replacement as TargetFn as usize,
            trampoline_slot: &TRAMP_FRAME,
        },
        Case {
            name: "global_fn(预期被分类器拒绝)",
            entry: ep_global_fn as TargetFn as usize,
            replacement: ep_global_fn_replacement as TargetFn as usize,
            trampoline_slot: &TRAMP_GLOBAL,
        },
    ];

    let far_only = std::env::var("FAR_ONLY").is_ok();
    let mut passed = 0usize;
    let mut skipped = 0usize;
    let mut failed = 0usize;

    if !far_only {
        for case in cases {
        println!("=== {} entry={:#x} ===", case.name, case.entry);
        let patch = entry_patch_bytes(case.entry, case.replacement);
        let n_insns = patch.len() / 4;
        println!("patch_len={} insns={n_insns}", patch.len());

        let baseline = call_at(case.entry, 2, 3);
        println!("baseline(2,3)={baseline}");

        let ops: Vec<u32> = unsafe { read_bytes(case.entry, n_insns * 4) }
            .chunks_exact(4)
            .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect();
        println!(
            "prologue={:?}",
            ops.iter().map(|op| format!("{op:#010x}")).collect::<Vec<_>>()
        );

        if let Err(reason) = prologue_is_verbatim_safe(&ops) {
            println!("SKIP: {reason}(检测路径按设计工作)");
            skipped += 1;
            continue;
        }
        println!("prologue verbatim safe");

        let tramp = match unsafe { build_trampoline(case.entry, &ops) } {
            Ok(t) => t,
            Err(e) => {
                println!("FAIL: trampoline: {e}");
                failed += 1;
                continue;
            }
        };
        case.trampoline_slot.store(tramp, Ordering::Relaxed);
        println!("trampoline={tramp:#x}");

        // 未安装 hook,先单独验证跳板:结果必须等于 baseline。
        let via_tramp = call_at(tramp, 2, 3);
        if via_tramp != baseline {
            println!("FAIL: trampoline result {via_tramp} != baseline {baseline}");
            failed += 1;
            continue;
        }
        println!("trampoline original call ok: {via_tramp}");

        // 保存原始字节并安装补丁。
        let original = unsafe { read_bytes(case.entry, patch.len()) };
        if let Err(e) = unsafe { remap_patch(case.entry, &patch) } {
            println!("FAIL: remap_patch: {e}");
            failed += 1;
            continue;
        }
        let before = HITS.load(Ordering::Relaxed);
        let marked = call_at(case.entry, 2, 3);
        let original_b = call_at(tramp, -7, 11);
        let marked_b = call_at(case.entry, -7, 11);
        let hits = HITS.load(Ordering::Relaxed) - before;
        let expected2 = baseline.wrapping_mul(2);
        if hits != 2 || marked != expected2 || marked_b != original_b.wrapping_mul(2) {
            println!(
                "FAIL: hook path hits={hits} marked={marked}(expect {expected2}) marked_b={marked_b}(expect {})",
                original_b.wrapping_mul(2)
            );
            failed += 1;
        } else {
            println!("hook path ok: (2,3)->{marked}, (-7,11)->{marked_b}, hits=2");
            passed += 1;
        }

        // 并发:hook 生效状态下 4 线程锤调用,校验结果不变量。
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let bad_counter = std::sync::Arc::new(AtomicUsize::new(0));
        let mut workers = Vec::new();
        for _ in 0..4 {
            let stop_ref = std::sync::Arc::clone(&stop);
            let bad_ref = std::sync::Arc::clone(&bad_counter);
            workers.push(thread::spawn(move || {
                hammer(case.entry, tramp, &stop_ref, &bad_ref);
            }));
        }
        thread::sleep(Duration::from_millis(300));
        stop.store(true, Ordering::Relaxed);
        for w in workers {
            let _ = w.join();
        }
        let bad = bad_counter.load(Ordering::Relaxed);
        if bad != 0 {
            println!("FAIL: concurrent invariant violations={bad}");
            failed += 1;
        } else {
            println!("concurrent hammering (hooked) ok: 0 violations");
        }

        // 恢复:写回原始字节。
        if let Err(e) = unsafe { remap_patch(case.entry, &original) } {
            println!("FAIL: restore patch: {e}");
            failed += 1;
            continue;
        }
        let restored = call_at(case.entry, 2, 3);
        let restored_again = call_at(case.entry, 5, 6);
        if restored != baseline || restored_again != call_at(tramp, 5, 6) {
            println!("FAIL: restore mismatch {restored} / {restored_again}");
            failed += 1;
        } else {
            println!("restore ok: (2,3)->{restored} == baseline");
        }
        }
    }

    // 强制远跳路径(生产场景:游戏 text → dylib,必然超 ±128MB):
    // 16 字节补丁覆盖 4 条指令,跳板需重定位 4 条。
    println!("\n=== forced far-jump (frame_fn, 4-insn trampoline) ===");
    {
        let entry = ep_frame_fn as TargetFn as usize;
        let ops: Vec<u32> = unsafe { read_bytes(entry, 16) }
            .chunks_exact(4)
            .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect();
        println!(
            "prologue[4]={:?}",
            ops.iter().map(|op| format!("{op:#010x}")).collect::<Vec<_>>()
        );
        match prologue_is_verbatim_safe(&ops) {
            Err(reason) => {
                println!("SKIP: {reason}(4 条指令内含不可 verbatim 搬迁形态)");
                skipped += 1;
            }
            Ok(()) => {
                let baseline = call_at(entry, 2, 3);
                let tramp = unsafe { build_trampoline(entry, &ops) }.expect("trampoline");
                TRAMP_FRAME.store(tramp, Ordering::Relaxed);
                let mut far = Vec::with_capacity(16);
                far.extend_from_slice(&LDR_X16_LITERAL_8.to_le_bytes());
                far.extend_from_slice(&BR_X16.to_le_bytes());
                far.extend_from_slice(&(ep_frame_fn_replacement as TargetFn as u64).to_le_bytes());
                println!(
                    "replacement={:#x} tramp={tramp:#x} entry={entry:#x}",
                    ep_frame_fn_replacement as TargetFn as usize
                );
                let original = unsafe { read_bytes(entry, 16) };
                let via_tramp4 = call_at(tramp, 2, 3);
                if via_tramp4 != baseline {
                    println!("FAIL: 4-insn trampoline {via_tramp4} != baseline {baseline}");
                    failed += 1;
                } else {
                    println!("4-insn trampoline standalone ok: {via_tramp4}");
                    unsafe { remap_patch(entry, &far) }.expect("far patch");
                    let patched_bytes: Vec<String> = unsafe { read_bytes(entry, 16) }
                        .iter()
                        .map(|b| format!("{b:02x}"))
                        .collect();
                    println!("entry bytes after far patch: {:02?}", patched_bytes);
                    let far_check: Vec<String> = far.iter().map(|b| format!("{b:02x}")).collect();
                    println!("expected far bytes:          {:02?}", far_check);
                    let hits0 = HITS.load(Ordering::Relaxed);
                    let marked = call_at(entry, 2, 3);
                    let marked2 = call_at(entry, 2, 3);
                    let original_b = call_at(tramp, -7, 11);
                    let marked_b = call_at(entry, -7, 11);
                    let hits = HITS.load(Ordering::Relaxed) - hits0;
                    println!("marked={marked} marked2={marked2} marked_b={marked_b} original_b={original_b} hits={hits}");
                if marked != baseline.wrapping_mul(2)
                    || marked_b != original_b.wrapping_mul(2)
                    || HITS.load(Ordering::Relaxed) < 2
                {
                    println!("FAIL: far-jump hook path marked={marked} marked_b={marked_b}");
                    failed += 1;
                } else {
                    println!("far-jump hook path ok: (2,3)->{marked}, (-7,11)->{marked_b}");
                    passed += 1;
                }
                unsafe { remap_patch(entry, &original) }.expect("far restore");
                let restored = call_at(entry, 2, 3);
                if restored != baseline {
                    println!("FAIL: far-jump restore {restored}");
                    failed += 1;
                } else {
                    println!("far-jump restore ok: {restored} == baseline");
                }
                }
            }
        }
    }

    println!("\nresult: passed={passed} skipped={skipped} failed={failed}");
    if std::env::var("SIGHOOK_PROBE").is_ok() {
        println!("--- sighook probe: 预期 SIGBUS(写签名 text 页) ---");
        let r = sighook::inline_hook_jump(
            ep_plain_sum as TargetFn as u64,
            ep_plain_sum_replacement as TargetFn as u64,
        );
        println!("sighook probe returned {r:?}(没有崩溃则记录新事实)");
    }
    if failed > 0 {
        std::process::exit(1);
    }
    println!("process survived; exit 0");
}
