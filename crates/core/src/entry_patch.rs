//! Function-entry inline patching (aarch64) behind the [`SlotMemory`] protocol.
//!
//! The MethodPointer slot swap only intercepts callers that dispatch through
//! the slot; AOT-compiled direct branches jump straight into the function
//! entry and never read the slot. This module rewrites the entry itself and
//! exposes the identical CAS + readback + ownership contract, so
//! [`MethodPointerSlot`] and the whole hook typestate work unchanged.
//!
//! Physical mechanism (validated in `experiments/entry-patch`):
//!
//! 1. Bind saves the original entry bytes and builds a trampoline page
//!    (relocated instructions + absolute jump back to `entry + N`). The
//!    trampoline address plays the role of the "original pointer": pristine
//!    state word = trampoline address, installed state word = replacement
//!    address.
//! 2. Install patches the entry (near `b`, or `ldr x16/br x16` + 8-byte
//!    literal) by full-page replacement: the target page is copied into a
//!    private JIT staging page, patched there, and `mach_vm_remap`ed over the
//!    original address. macOS refuses in-place writes to signed, file-backed
//!    text pages (`mach_vm_protect` "succeeds" but the write faults), while
//!    remapped anonymous pages are writable.
//! 3. Every patch ends with `dc cvau`/`ic ivau`/`dsb`/`isb` synchronization
//!    plus a data-side readback; without it, stale instruction fetches were
//!    observably served after remapping.
//!
//! Install races (another thread executing the entry while it is rewritten)
//! are mitigated by installing at bootstrap, before game code runs; this
//! matches the publish-before-install discipline of the hook typestate.

use crate::error::HookError;
use crate::method_slot::{MethodRef, SlotMemory};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

const LDR_X16_LITERAL_8: u32 = 0x5800_0050; // ldr x16, #8
const BR_X16: u32 = 0xD61F_0200; // br x16
const PACIBSP: u32 = 0xD503_237F;
const BRANCH_IMM26_WORDS: i64 = 1 << 25; // ±128 MiB reach of an arm64 `b`

const VM_FLAGS_OVERWRITE: i32 = 0x4000;
const VM_INHERIT_COPY: u32 = 1;

type KernReturn = i32;
type MachPort = u32;

#[cfg(target_arch = "aarch64")]
unsafe extern "C" {
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

unsafe fn page_size() -> Result<usize, HookError> {
    let value = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if value <= 0 {
        return Err(HookError::EntryPatchUnsupported("page size unavailable"));
    }
    Ok(value as usize)
}

/// `dc cvau` / `ic ivau` per cache line over the range, then `dsb ish` + `isb`.
fn sync_icache(start: usize, len: usize) {
    let line = 64usize;
    let first = start & !(line - 1);
    let end = (start + len + line - 1) & !(line - 1);
    let mut va = first;
    while va < end {
        // SAFETY: `va` is the address of a mapped, executable instruction
        // word inside the patched page; the cache maintenance instructions
        // take any mapped VA and have no memory side effects.
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
    // SAFETY: barrier instructions only.
    unsafe {
        core::arch::asm!("dsb ish", options(nostack));
        core::arch::asm!("isb", options(nostack));
    }
}

/// Private RW staging page shared by all entry patches. Created as RW with
/// `MAP_JIT`, flipped to RX before remapping (JIT mappings are exempt from
/// the W^X transition restriction), so the remapped-in text mapping is
/// executable without any protection raise afterwards.
fn staging_page() -> Result<usize, HookError> {
    static STAGE: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    if let Some(stage) = STAGE.get() {
        return Ok(*stage);
    }
    let ps = unsafe { page_size()? };
    // SAFETY: anonymous private mapping; MAP_JIT marks it as a JIT region so
    // the RW -> RX transition below is permitted on Apple Silicon.
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
        return Err(HookError::EntryPatchUnsupported("staging page mmap failed"));
    }
    Ok(*STAGE.get_or_init(|| mem as usize))
}

/// Copy `bytes` over the code at `addr` by full-page replacement through the
/// private staging page. Never writes the signed text page in place.
///
/// Serialized by a process-wide lock: the staging page and its protection
/// flips are shared across all hooks, and install/restore only ever run on
/// bootstrap or teardown paths, never on hot paths.
fn remap_patch(addr: usize, bytes: &[u8]) -> Result<(), HookError> {
    static REMAP_LOCK: Mutex<()> = Mutex::new(());
    let _remap_guard = REMAP_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let ps = unsafe { page_size()? };
    let page = addr & !(ps - 1);
    let off = addr - page;
    if off + bytes.len() > ps {
        return Err(HookError::EntryPatchUnsupported(
            "patch crosses a page boundary",
        ));
    }
    let task = unsafe { mach_task_self() };
    let stage = staging_page()?;

    // SAFETY: `stage` and `page` are mapped, page-aligned regions of `ps`
    // bytes; the copies stay inside their bounds and the protections are
    // valid for a MAP_JIT anonymous mapping.
    unsafe {
        if libc::mprotect(
            stage as *mut libc::c_void,
            ps,
            libc::PROT_READ | libc::PROT_WRITE,
        ) != 0
        {
            return Err(HookError::InstallationFailed);
        }
        std::ptr::copy_nonoverlapping(page as *const u8, stage as *mut u8, ps);
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), (stage + off) as *mut u8, bytes.len());
        if libc::mprotect(
            stage as *mut libc::c_void,
            ps,
            libc::PROT_READ | libc::PROT_EXEC,
        ) != 0
        {
            return Err(HookError::InstallationFailed);
        }
        sys_icache_invalidate((stage + off) as *mut libc::c_void, bytes.len());
    }

    // SAFETY: OVERWRITE-remap replaces the mapping at `page` (page-aligned,
    // mapped) with a copy of the patched staging page. All out-parameters are
    // valid kernel-written locations.
    let mut remote = page as u64;
    let mut current: i32 = 0;
    let mut max: i32 = 0;
    let kr = unsafe {
        mach_vm_remap(
            task,
            &mut remote,
            ps as u64,
            0,
            VM_FLAGS_OVERWRITE,
            task,
            stage as u64,
            1,
            &mut current,
            &mut max,
            VM_INHERIT_COPY,
        )
    };
    if kr != 0 {
        return Err(HookError::InstallationFailed);
    }

    // SAFETY: the remapped range is exactly the mapped page.
    unsafe { sys_icache_invalidate(page as *mut libc::c_void, ps) };
    sync_icache(page + off, bytes.len());

    // Data-side readback: integrity check, and a real access to the patched
    // page before any execution (stale fetches were observed without it).
    for (index, &expected) in bytes.iter().enumerate() {
        // SAFETY: inside the patched page, mapped for the process lifetime.
        let got = unsafe { std::ptr::read_volatile((page + off + index) as *const u8) };
        if got != expected {
            return Err(HookError::InstallationFailed);
        }
    }
    // SAFETY: barrier instruction only.
    unsafe {
        core::arch::asm!("isb", options(nostack));
    }
    Ok(())
}

/// Near `b` within ±128 MiB (1 displaced instruction), otherwise the 16-byte
/// `ldr x16 / br x16 / .quad target` far jump (4 displaced instructions).
fn entry_patch_bytes(entry: usize, replacement: usize) -> Vec<u8> {
    let offset = replacement as i128 - entry as i128;
    let imm26 = (offset >> 2) as i64;
    let reachable = offset & 0b11 == 0
        && (-(BRANCH_IMM26_WORDS as i128)..(BRANCH_IMM26_WORDS as i128)).contains(&(imm26 as i128));
    if reachable {
        (0x1400_0000u32 | (imm26 as u32 & 0x03FF_FFFF))
            .to_le_bytes()
            .to_vec()
    } else {
        let mut bytes = Vec::with_capacity(16);
        bytes.extend_from_slice(&LDR_X16_LITERAL_8.to_le_bytes());
        bytes.extend_from_slice(&BR_X16.to_le_bytes());
        bytes.extend_from_slice(&(replacement as u64).to_le_bytes());
        bytes
    }
}

/// Relocation plan for one displaced instruction.
///
/// Branch families (b/bl/b.cond/cbz/cbnz/tbz/tbnz) are *relocated*: a branch
/// whose target stays inside the displaced window is re-encoded in the
/// trampoline with an adjusted immediate; an out-of-window target uses an
/// inverted-condition branch over a far absolute jump (x16/x30 scratch per
/// AAPCS). adr/adrp/literal loads still fail closed — they need data-address
/// fixups. Everything else copies verbatim.
enum Plan {
    Verbatim {
        op: u32,
    },
    B {
        target: usize,
    },
    Bl {
        target: usize,
    },
    BCond {
        cond: u32,
        inverted: bool,
        target: usize,
    },
    CbzCbnz {
        op: u32,
        rt: u32,
        target: usize,
    },
    TbzTbnz {
        op: u32,
        rt: u32,
        target: usize,
    },
}

fn sext(value: i32, bits: u32) -> i64 {
    let shift = 32 - bits;
    ((value << shift) >> shift) as i64
}

fn plan_op(op: u32, orig_pc: usize) -> Result<Plan, HookError> {
    let target_of = |imm: i64| (orig_pc as i64 + imm) as usize;
    if op == PACIBSP {
        return Ok(Plan::Verbatim { op });
    }
    if op & 0x7C00_0000 == 0x1400_0000 || op & 0xFC00_0000 == 0x9400_0000 {
        // b (0b000101) and bl (0b100101), ignoring the low imm26 bits.
        let imm26 = sext((op & 0x03FF_FFFF) as i32, 26) * 4;
        let target = target_of(imm26);
        return if op >> 26 == 0b100101 {
            Ok(Plan::Bl { target })
        } else {
            Ok(Plan::B { target })
        };
    }
    let top8 = op >> 24;
    if top8 & 0xFE == 0x54 {
        let imm19 = sext(((op >> 5) & 0x7_FFFF) as i32, 19) * 4;
        return Ok(Plan::BCond {
            cond: op & 0xF,
            inverted: false,
            target: target_of(imm19),
        });
    }
    if top8 & 0x9F == 0x10 || top8 & 0x9F == 0x90 {
        return Err(HookError::EntryPatchUnsupported(
            "adr/adrp in the displaced prologue",
        ));
    }
    if top8 & 0x1F == 0x18 {
        return Err(HookError::EntryPatchUnsupported(
            "literal load in the displaced prologue",
        ));
    }
    if top8 & 0x7E == 0x34 {
        let imm19 = sext(((op >> 5) & 0x7_FFFF) as i32, 19) * 4;
        return Ok(Plan::CbzCbnz {
            op,
            rt: op & 0x1F,
            target: target_of(imm19),
        });
    }
    if top8 & 0x7E == 0x36 {
        let imm14 = sext(((op >> 5) & 0x3_FFFF) as i32, 14) * 4;
        return Ok(Plan::TbzTbnz {
            op,
            rt: op & 0x1F,
            target: target_of(imm14),
        });
    }
    if op & 0xFFFF_FC1F == 0xD65F_0000 {
        return Ok(Plan::Verbatim { op }); // ret: LR-based, position independent
    }
    Ok(Plan::Verbatim { op })
}

fn plan_size(plan: &Plan, window: &std::ops::Range<usize>) -> Result<usize, HookError> {
    let in_window = |t: usize| window.contains(&t);
    Ok(match plan {
        Plan::Verbatim { .. } => 1,
        Plan::B { target } if in_window(*target) => 1,
        Plan::B { .. } => 4,
        Plan::Bl { target, .. } if in_window(*target) => 1,
        Plan::Bl { .. } => 7,
        Plan::BCond { target, .. }
        | Plan::CbzCbnz { target, .. }
        | Plan::TbzTbnz { target, .. }
            if in_window(*target) =>
        {
            1
        }
        Plan::BCond { .. } | Plan::CbzCbnz { .. } | Plan::TbzTbnz { .. } => 5,
    })
}

fn b_enc(top6: u32, imm26_words: i64) -> u32 {
    (top6 << 26) | ((imm26_words as u32) & 0x03FF_FFFF)
}

/// MOVZ/MOVK materialization of a 48-bit address into `rd` (3 instructions).
fn mov_imm48(rd: u32, value: usize) -> [u32; 3] {
    let lo = (value & 0xFFFF) as u32;
    let mid = ((value >> 16) & 0xFFFF) as u32;
    let hi = ((value >> 32) & 0xFFFF) as u32;
    [
        0xD280_0000 | (lo << 5) | rd,                // movz rd, lo
        0xF280_0000 | 0x0020_0000 | (mid << 5) | rd, // movk rd, mid, lsl #16
        0xF280_0000 | 0x0040_0000 | (hi << 5) | rd,  // movk rd, hi, lsl #32
    ]
}

fn far_jump_seq(buf: &mut Vec<u32>, target: usize) {
    let seq = mov_imm48(16, target);
    buf.extend_from_slice(&seq);
    buf.push(0xD61F_0200); // br x16
}
/// Emit one relocated instruction. `pos` is the buffer word offset of this
/// instruction; `ops_pos[i]` are the trampoline word offsets of the displaced
/// instructions (for intra-window branch retargeting).
fn emit_op(
    buf: &mut Vec<u32>,
    plan: &Plan,
    index: usize,
    pos: usize,
    ops_pos: &[usize],
    window: &std::ops::Range<usize>,
    tramp_base: usize,
) -> Result<(), HookError> {
    let in_window = |t: usize| window.contains(&t);
    let word_delta = |target_index: usize| ops_pos[target_index] as i64 - pos as i64;
    match plan {
        Plan::Verbatim { op } => buf.push(*op),
        Plan::B { target } if in_window(*target) => {
            let index = target_index(*target, window);
            buf.push(b_enc(0b000101, word_delta(index)));
        }
        Plan::B { target } => far_jump_seq(buf, *target),
        Plan::Bl { target } if in_window(*target) => {
            let target_index = target_index(*target, window);
            buf.push(b_enc(0b100101, word_delta(target_index)));
        }
        Plan::Bl { target } => {
            // Far call: x16 = target (MOVZ/MOVK), x30 = return address (the
            // relocated next instruction's trampoline position, or the
            // window end), then BR. x30 CANNOT be orig_pc+4: that address
            // is inside the patched window.
            let next_index = index + 1;
            let ret = if next_index < ops_pos.len() {
                tramp_base + ops_pos[next_index] * 4
            } else {
                window.end
            };
            #[cfg(test)]
            eprintln!(
                "[dbg] bl: index={index} next={next_index} ret={ret:#x} target={target:#x} tramp={tramp_base:#x}"
            );
            buf.extend_from_slice(&mov_imm48(16, *target));
            buf.extend_from_slice(&mov_imm48(30, ret));
            buf.push(0xD61F_0200); // br x16
        }
        Plan::BCond {
            cond,
            inverted,
            target,
        } if in_window(*target) => {
            let target_index = target_index(*target, window);
            let delta = word_delta(target_index);
            let imm19 = (delta as u32) & 0x7_FFFF;
            let cond_v = if *inverted { cond ^ 1 } else { *cond };
            buf.push(0x5400_0000 | (imm19 << 5) | cond_v);
        }
        Plan::BCond {
            cond,
            inverted,
            target,
        } => {
            // Invert the condition, branch over the far sequence.
            let inv = if *inverted { *cond } else { *cond ^ 1 };
            buf.push(0x5400_0000 | (5 << 5) | inv);
            far_jump_seq(buf, *target);
        }
        Plan::CbzCbnz { op, rt, target } if in_window(*target) => {
            let target_index = target_index(*target, window);
            let delta = word_delta(target_index);
            let imm19 = (delta as u32) & 0x7_FFFF;
            // Keep sf/bit31 + fixed pattern + op(bit24); rewrite imm19 + Rt.
            buf.push((*op & 0xFF00_0000) | (imm19 << 5) | rt);
        }
        Plan::CbzCbnz { op, rt, target } => {
            // Invert op (bit24): CBZ<->CBNZ, skipping the 20-byte far jump.
            // Clear the old imm19 field before inserting the skip distance.
            buf.push(((*op ^ 0x0100_0000) & 0xFF00_001F) | (5 << 5) | rt);
            far_jump_seq(buf, *target);
        }
        Plan::TbzTbnz { op, rt, target } if in_window(*target) => {
            let target_index = target_index(*target, window);
            let delta = word_delta(target_index);
            let imm14 = (delta as u32) & 0x3_FFFF;
            // Keep sf/b5 + fixed pattern + op(bit24) + b40(23..19); rewrite
            // imm14 + Rt.
            buf.push((*op & 0xFFFF_8000) | (imm14 << 5) | rt);
        }
        Plan::TbzTbnz { op, rt, target } => {
            // Invert op (bit24): TBZ<->TBNZ, skipping the 20-byte far jump.
            // Clear the old imm14 field before inserting the skip distance.
            buf.push(((*op ^ 0x0100_0000) & 0xFFFF_001F) | (5 << 5) | rt);
            far_jump_seq(buf, *target);
        }
    }
    Ok(())
}

fn target_index(target: usize, window: &std::ops::Range<usize>) -> usize {
    (target - window.start) / 4
}

fn build_trampoline(entry: usize, ops: &[u32]) -> Result<usize, HookError> {
    let window = std::ops::Range {
        start: entry,
        end: entry + ops.len() * 4,
    };
    let plans: Vec<Plan> = ops
        .iter()
        .enumerate()
        .map(|(index, &op)| plan_op(op, entry + index * 4))
        .collect::<Result<_, _>>()?;
    let sizes: Vec<usize> = plans
        .iter()
        .map(|plan| plan_size(plan, &window))
        .collect::<Result<_, _>>()?;
    let mut ops_pos = Vec::with_capacity(ops.len());
    let mut acc = 0usize; // word offset
    for size in &sizes {
        ops_pos.push(acc);
        acc += size;
    }

    let ps = unsafe { page_size()? };
    // SAFETY: anonymous private mapping; it is written while RW and switched
    // to RX before any execution.
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
        return Err(HookError::EntryPatchUnsupported("trampoline mmap failed"));
    }
    let base = mem as usize;
    let mut buf: Vec<u32> = Vec::with_capacity(acc);
    for (index, plan) in plans.iter().enumerate() {
        let pos = ops_pos[index];
        while buf.len() < pos {
            buf.push(0xD503_201F); // nop padding for multi-word sequences
        }
        emit_op(&mut buf, plan, index, pos, &ops_pos, &window, base)?;
    }
    let displaced = ops.len() * 4;
    for (index, word) in buf.iter().enumerate() {
        // SAFETY: inside the page; `buf` is bounded by the pass-1 size check.
        unsafe { std::ptr::write_unaligned((base + index * 4) as *mut u32, word.to_le()) };
    }
    // The jump-back tail sits after the emitted sequence, which can be longer
    // than the displaced window once branch relocation expands instructions.
    let tail = buf.len() * 4;
    // SAFETY: the tail layout (4 + 4 + 8 bytes) stays inside the page.
    unsafe {
        std::ptr::write_unaligned((base + tail) as *mut u32, LDR_X16_LITERAL_8.to_le());
        std::ptr::write_unaligned((base + tail + 4) as *mut u32, BR_X16.to_le());
        std::ptr::write_unaligned((base + tail + 8) as *mut u64, (entry + displaced) as u64);
        if libc::mprotect(mem, ps, libc::PROT_READ | libc::PROT_EXEC) != 0 {
            return Err(HookError::EntryPatchUnsupported(
                "trampoline mprotect failed",
            ));
        }
        sys_icache_invalidate(mem, tail + 16);
    }
    sync_icache(base, tail + 16);
    Ok(base)
}

/// [`SlotMemory`] implementation over a function entry. The state word is
/// virtual: pristine = trampoline address, installed = replacement address.
/// The CAS contract of [`MethodPointerSlot`](crate::method_slot::MethodPointerSlot)
/// therefore drives the physical patch without any changes to the hook
/// typestate.
pub struct EntryPatchMemory {
    entry: usize,
    replacement: usize,
    trampoline: usize,
    patch: Vec<u8>,
    original: Vec<u8>,
    installed: AtomicBool,
    failure: Mutex<Option<&'static str>>,
}

impl EntryPatchMemory {
    /// # Safety
    ///
    /// `method.method_pointer_slot` must address a live, readable
    /// pointer-sized word holding the entry address of the resolved IL2CPP
    /// method; the entry must be mapped executable code. Called by the
    /// backend with backend-validated method references.
    pub unsafe fn new(method: &MethodRef, replacement: usize) -> Result<Self, HookError> {
        let slot = method.method_pointer_slot;
        if slot == 0 || !slot.is_multiple_of(std::mem::align_of::<usize>()) {
            return Err(HookError::SlotMalformed);
        }
        // SAFETY: caller contract guarantees a live, aligned pointer word.
        let entry = unsafe { std::ptr::read_volatile(slot as *const usize) };
        if entry == 0 || entry % 4 != 0 {
            return Err(HookError::SlotMalformed);
        }
        let patch = entry_patch_bytes(entry, replacement);
        let displaced = patch.len();
        // SAFETY: `displaced` bytes at the entry of mapped executable code.
        let original: Vec<u8> = unsafe {
            (0..displaced)
                .map(|offset| std::ptr::read_volatile((entry + offset) as *const u8))
                .collect()
        };
        let ops: Vec<u32> = original
            .chunks_exact(4)
            .map(|chunk| u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
            .collect();
        let trampoline = build_trampoline(entry, &ops)?;
        Ok(Self {
            entry,
            replacement,
            trampoline,
            patch,
            original,
            installed: AtomicBool::new(false),
            failure: Mutex::new(None),
        })
    }

    /// Description of the last physical patch failure, for diagnostics.
    pub fn last_failure(&self) -> Option<&'static str> {
        self.failure.lock().ok().and_then(|guard| *guard)
    }

    fn state_word(&self) -> usize {
        if self.installed.load(Ordering::Acquire) {
            self.replacement
        } else {
            self.trampoline
        }
    }

    fn patch_entry(&self) -> Result<(), HookError> {
        match remap_patch(self.entry, &self.patch) {
            Ok(()) => Ok(()),
            Err(error) => {
                if let Ok(mut guard) = self.failure.lock() {
                    *guard = Some("entry patch write failed");
                }
                tracing::warn!(target: "core::entry_patch", entry = format_args!("{:#x}", self.entry), "entry patch write failed");
                Err(error)
            }
        }
    }

    fn unpatch_entry(&self) -> Result<(), HookError> {
        match remap_patch(self.entry, &self.original) {
            Ok(()) => Ok(()),
            Err(error) => {
                if let Ok(mut guard) = self.failure.lock() {
                    *guard = Some("entry restore write failed");
                }
                tracing::warn!(target: "core::entry_patch", entry = format_args!("{:#x}", self.entry), "entry restore write failed");
                Err(error)
            }
        }
    }
}

impl SlotMemory for EntryPatchMemory {
    fn read(&self) -> Option<usize> {
        Some(self.state_word())
    }

    fn compare_exchange(&self, expected: usize, new: usize) -> Result<(), usize> {
        let current = self.state_word();
        if expected != current {
            return Err(current);
        }
        let result = if new == self.replacement && !self.installed.load(Ordering::Acquire) {
            self.patch_entry().map_err(|_| current)
        } else if new == self.trampoline && self.installed.load(Ordering::Acquire) {
            self.unpatch_entry().map_err(|_| current)
        } else {
            Err(current)
        };
        if result.is_ok() {
            let next_installed = new == self.replacement;
            self.installed.store(next_installed, Ordering::Release);
        }
        result
    }
}

#[cfg(all(test, target_arch = "aarch64"))]
mod tests {
    use super::*;
    use crate::method_slot::{MethodPointerSlot, MethodRef};

    const ADD_X0_X1: u32 = 0x8B01_0000; // add x0, x0, x1
    const RET: u32 = 0xD65F_03C0;

    type TestFn = extern "C" fn(i64, i64) -> i64;

    extern "C" fn test_replacement(a: i64, b: i64) -> i64 {
        // Self-contained: does not call the original, so the hook contract
        // test observes the replacement marker directly.
        a.wrapping_mul(2) + b
    }

    /// A real executable page holding `ops` + `ret`, backing a fake
    /// MethodRef whose slot word holds the page's entry address. The fields
    /// are never read: they exist to keep the page mapping and the slot word
    /// allocation alive for the lifetime of the fake.
    #[allow(dead_code)]
    struct FakeMethod {
        page: usize,
        slot_word: Box<usize>,
        method: MethodRef,
    }

    unsafe fn fake_method(ops: &[u32]) -> FakeMethod {
        let ps = unsafe { page_size().expect("page size") };
        // SAFETY: anonymous private page, written while RW and switched to RX
        // like the trampoline builder does.
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
        assert_ne!(mem, libc::MAP_FAILED);
        let page = mem as usize;
        for (index, &op) in ops.iter().enumerate() {
            // SAFETY: `index` is far below the page size.
            unsafe { std::ptr::write_unaligned((page + index * 4) as *mut u32, op.to_le()) };
        }
        // SAFETY: the page was just mapped and fully written above.
        unsafe {
            assert_eq!(
                libc::mprotect(mem, ps, libc::PROT_READ | libc::PROT_EXEC),
                0
            );
        }
        let slot_word = Box::new(page);
        let method = MethodRef {
            assembly: "test".to_owned(),
            namespace: "test".to_owned(),
            class: "test".to_owned(),
            method: "test".to_owned(),
            param_count: 2,
            is_static: false,
            return_type: "long".to_owned(),
            parameter_types: vec!["long".to_owned(), "long".to_owned()],
            is_generic: false,
            is_inflated: false,
            method_info: page,
            method_pointer_slot: std::ptr::addr_of!(*slot_word) as usize,
        };
        FakeMethod {
            page,
            slot_word,
            method,
        }
    }

    fn call(addr: usize, a: i64, b: i64) -> i64 {
        let f: TestFn = unsafe { std::mem::transmute(addr) };
        f(a, b)
    }

    #[test]
    fn entry_patch_protocol_round_trips_through_method_pointer_slot() {
        // add ×4 then ret: verbatim-safe under both the near (1 insn) and
        // far (4 insn) patch widths, and the tail terminates cleanly.
        const IMAGE: [u32; 5] = [ADD_X0_X1, ADD_X0_X1, ADD_X0_X1, ADD_X0_X1, RET];
        let fake = unsafe { fake_method(&IMAGE) };
        let entry = fake.method.method_info;
        let replacement = test_replacement as TestFn as usize;
        let memory = unsafe { EntryPatchMemory::new(&fake.method, replacement).expect("bind") };
        let trampoline = match memory.read() {
            Some(word) => word,
            None => panic!("read must report the pristine state word"),
        };
        assert_ne!(trampoline, replacement);
        assert_ne!(trampoline, entry);

        // Pristine trampoline reproduces the original result: four
        // `add x0, x0, x1` instructions turn (2, 3) into 2+3+3+3+3 = 14.
        assert_eq!(call(trampoline, 2, 3), 14);

        let slot = MethodPointerSlot::bind(std::sync::Arc::new(memory)).expect("slot bind");
        assert_eq!(slot.original(), trampoline);
        slot.install(replacement).expect("install");

        // Hooked entry routes to the replacement.
        assert_eq!(call(entry, 2, 3), 7); // 2*2 + 3

        // Restore returns the original behavior and re-arms nothing.
        slot.restore(replacement).expect("restore");
        assert_eq!(call(entry, 2, 3), 14);
        assert_eq!(call(trampoline, 2, 3), 14);
    }

    #[test]
    fn entry_patch_rejects_pc_relative_prologues() {
        // adrp x0, #0 (0x90000000): must fail closed at bind.
        let fake = unsafe { fake_method(&[0x9000_0000, ADD_X0_X1, ADD_X0_X1, RET]) };
        let replacement = test_replacement as TestFn as usize;
        let error = unsafe { EntryPatchMemory::new(&fake.method, replacement) }
            .err()
            .expect("adrp prologue must be rejected");
        assert!(matches!(error, HookError::EntryPatchUnsupported(_)));
    }

    #[test]
    fn cbz_branch_is_relocated_across_the_trampoline() {
        // Image: CBZ X1, ->RET; ADD X0,X0,X1; ADD X0,X0,X1; RET — the branch
        // skips both adds when b == 0. Original: f(a,b) = a if b == 0 else
        // a + 2b.
        const CBZ_X1_3: u32 = 0xB400_0061;
        let fake = unsafe { fake_method(&[CBZ_X1_3, ADD_X0_X1, ADD_X0_X1, RET]) };
        let entry = fake.method.method_info;
        let tramp =
            build_trampoline(entry, &[CBZ_X1_3, ADD_X0_X1, ADD_X0_X1, RET]).expect("trampoline");
        let tramp_fn: extern "C" fn(i64, i64) -> i64 = unsafe { core::mem::transmute(tramp) };
        // Both branch directions must behave identically to the original.
        assert_eq!(tramp_fn(5, 0), 5, "b==0 path");
        assert_eq!(tramp_fn(5, 3), 11, "b!=0 path");
    }

    extern "C" fn bl_helper(a: i64, _b: i64) -> i64 {
        a.wrapping_mul(2)
    }

    #[test]
    fn bl_call_is_relocated_with_correct_return_address() {
        // Proper frame: [STP x29,x30][BL bl_helper][LDP x29,x30][RET] — the
        // far window displaces all four instructions including the BL and
        // the RET, exercising LR save/restore across relocation.
        const STP_X29_X30: u32 = 0xA9BF_7BFD; // stp x29, x30, [sp, #-16]!
        const LDP_X29_X30: u32 = 0xA8C1_7BFD; // ldp x29, x30, [sp], #16
        let ps = unsafe { page_size().expect("page size") };
        // SAFETY: anonymous private page, written while RW and switched to RX.
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
        assert_ne!(mem, libc::MAP_FAILED);
        let entry = mem as usize;
        let helper = bl_helper as extern "C" fn(i64, i64) -> i64 as usize;
        let delta_words = (helper as i64 - (entry + 4) as i64) / 4;
        let image = [STP_X29_X30, b_enc(0b100101, delta_words), LDP_X29_X30, RET];
        // SAFETY: freshly mapped RW page; image fits far inside it.
        unsafe {
            for (index, &word) in image.iter().enumerate() {
                std::ptr::write_unaligned((entry + index * 4) as *mut u32, word.to_le());
            }
            assert_eq!(
                libc::mprotect(mem, ps, libc::PROT_READ | libc::PROT_EXEC),
                0
            );
        }
        let slot_word = Box::new(entry);
        #[allow(unused_variables)]
        let method = MethodRef {
            assembly: "test".to_owned(),
            namespace: "test".to_owned(),
            class: "test".to_owned(),
            method: "test".to_owned(),
            param_count: 2,
            is_static: false,
            return_type: "long".to_owned(),
            parameter_types: vec!["long".to_owned(), "long".to_owned()],
            is_generic: false,
            is_inflated: false,
            method_info: entry,
            method_pointer_slot: std::ptr::addr_of!(*slot_word) as usize,
        };
        let _keep_slot = slot_word;
        let tramp = build_trampoline(entry, &image).expect("trampoline");
        #[cfg(test)]
        {
            eprintln!("[dbg] entry={entry:#x} helper={helper:#x} tramp={tramp:#x}");
            for i in 0..16u32 {
                let w = unsafe { std::ptr::read_volatile((tramp + i as usize * 4) as *const u32) };
                eprintln!("[dbg] w{i:02}: {w:08x}");
            }
        }
        let tramp_fn: extern "C" fn(i64, i64) -> i64 = unsafe { core::mem::transmute(tramp) };
        // The relocated BL calls the helper (x0*2); the LDP copy restores the
        // caller's LR and the RET copy returns it — both branch directions of
        // the frame survive relocation.
        assert_eq!(tramp_fn(5, 9), 10);
        assert_eq!(tramp_fn(21, 3), 42);
    }

    #[test]
    fn install_conflict_and_drift_follow_the_slot_contract() {
        let fake = unsafe { fake_method(&[ADD_X0_X1, ADD_X0_X1, ADD_X0_X1, ADD_X0_X1, RET]) };
        let entry = fake.method.method_info;
        let replacement = test_replacement as TestFn as usize;
        let memory = unsafe { EntryPatchMemory::new(&fake.method, replacement).expect("bind") };
        let slot = MethodPointerSlot::bind(std::sync::Arc::new(memory)).expect("slot bind");
        slot.install(replacement).expect("install");

        // A second install with a stale expected value reports conflict and
        // does not write.
        assert!(matches!(
            slot.install(replacement),
            Err(HookError::SlotConflict)
        ));
        assert_eq!(call(entry, 2, 3), 7);

        slot.restore(replacement).expect("restore");
        // Double restore reports ownership drift (slot no longer holds the
        // replacement).
        assert!(matches!(
            slot.restore(replacement),
            Err(HookError::OwnershipDrift)
        ));
        assert_eq!(call(entry, 2, 3), 14);
    }
}
