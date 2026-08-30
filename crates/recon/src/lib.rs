#![doc = include_str!("../README.md")]

use corelib::debug::{DebugHandlerError, MainDebugTopic};
use corelib::il2cpp_recon;
use corelib::plugin_api::phase::UpdateCtx;
use corelib::{Plugin, PluginError};
use serde::{Deserialize, Serialize};

/// Development-only runtime reconnaissance plugin. Answers, against the live
/// decrypted metadata cache and the mapped game image:
///
/// * `recon.class` — full method/field surface of one class, including
///   native RVAs (the runtime equivalent of an IL2CPP dump, scoped to the
///   class) and raw static field storage for singleton discovery.
/// * `recon.callers` — direct `bl` callers of one function RVA inside the
///   UnityFramework `__TEXT` section, to establish whether an entry is
///   actually invoked by AOT code.
pub struct ReconPlugin;

impl Plugin for ReconPlugin {
    fn name(&self) -> &'static str {
        "recon"
    }

    fn build(&self, ctx: &mut corelib::AppCtx<'_>) -> Result<(), PluginError> {
        ctx.register_main_debug::<fn(), ClassQuery, _>(class_query_handler)?;
        ctx.register_main_debug::<fn(), CallersQuery, _>(callers_query_handler)?;
        Ok(())
    }
}

#[derive(Deserialize)]
pub struct ClassQuery {
    pub assembly: String,
    pub class: String,
}

impl MainDebugTopic for ClassQuery {
    const NAME: &'static str = "recon.class";
    type Request = ClassQuery;
    type Response = Result<il2cpp_recon::ClassSurface, String>;
}

#[derive(Deserialize)]
pub struct CallersQuery {
    /// Static offset (image base relative) of the target function.
    pub rva: u64,
    /// Optional maximum number of reported caller sites (default 64).
    pub limit: Option<usize>,
}

#[derive(Serialize)]
pub struct CallersResponse {
    pub image_base: u64,
    pub text_rva: u64,
    pub text_size: u64,
    pub total: usize,
    pub truncated: bool,
    pub caller_rvas: Vec<u64>,
}

impl MainDebugTopic for CallersQuery {
    const NAME: &'static str = "recon.callers";
    type Request = CallersQuery;
    type Response = Result<CallersResponse, String>;
}

fn class_query_handler(
    _ctx: UpdateCtx<'_>,
    request: ClassQuery,
) -> Result<Result<il2cpp_recon::ClassSurface, String>, DebugHandlerError> {
    Ok(
        il2cpp_recon::class_surface(&request.assembly, &request.class)
            .map_err(|error| error.to_string()),
    )
}

fn callers_query_handler(
    _ctx: UpdateCtx<'_>,
    request: CallersQuery,
) -> Result<Result<CallersResponse, String>, DebugHandlerError> {
    let Some(base) = il2cpp_recon::image_base() else {
        return Ok(Err("UnityFramework image base unknown".into()));
    };
    let Some((text_va, text_size)) = text_section() else {
        return Ok(Err("__TEXT.__text not found in the image".into()));
    };
    let target_va = base as u64 + request.rva;
    let limit = request.limit.unwrap_or(64);
    let mut total = 0usize;
    let mut truncated = false;
    let mut caller_rvas: Vec<u64> = Vec::new();
    let target_va_i = target_va as i64;
    let text_va_i = text_va as i64;
    // SAFETY: the __text section of a loaded image is mapped readable for
    // the process lifetime; every word access stays inside [text_va,
    // text_va + size) and the section size is a multiple of 4 on arm64.
    for offset in (0..text_size).step_by(4) {
        let word = unsafe { std::ptr::read_volatile((text_va as u64 + offset) as *const u32) };
        if word & 0xFC00_0000 != 0x9400_0000 {
            continue; // not a `bl`
        }
        let imm26 = (word & 0x03FF_FFFF) as i32;
        let signed = ((imm26 << 6) >> 6) as i64; // sign-extend imm26
        let target = text_va_i + offset as i64 + signed * 4;
        if target == target_va_i {
            total += 1;
            if caller_rvas.len() < limit {
                caller_rvas.push(text_va as u64 + offset - base as u64);
            } else {
                truncated = true;
            }
        }
    }
    Ok(Ok(CallersResponse {
        image_base: base as u64,
        text_rva: text_va as u64 - base as u64,
        text_size,
        total,
        truncated,
        caller_rvas,
    }))
}

/// Locate the mapped `__TEXT.__text` section of the UnityFramework image:
/// walk the Mach-O load commands from the dyld image header.
fn text_section() -> Option<(usize, u64)> {
    // SAFETY: dyld image enumerators are thread-safe reads of the loader's
    // own tables; the index stays below the reported count.
    unsafe {
        let count = _dyld_image_count();
        for index in 0..count {
            let name = _dyld_get_image_name(index);
            if name.is_null() {
                continue;
            }
            let path = std::ffi::CStr::from_ptr(name).to_string_lossy();
            if !path.ends_with("UnityFramework") {
                continue;
            }
            let header = _dyld_get_image_header(index) as usize;
            if header == 0 {
                continue;
            }
            return macho_text_section(header);
        }
    }
    None
}

/// Walk LC_SEGMENT_64 load commands of a mapped arm64 Mach-O header and
/// return the (`addr`, `size`) of `__TEXT.__text`.
unsafe fn macho_text_section(header: usize) -> Option<(usize, u64)> {
    const MH_MAGIC_64: u32 = 0xfeed_facf;
    const LC_SEGMENT_64: u32 = 0x19;
    // SAFETY: `header` is the mapped mach-o header of a live image; the walk
    // below only reads within [header, header + header_size + sizeofcmds).
    unsafe {
        let magic = std::ptr::read_volatile(header as *const u32);
        if magic != MH_MAGIC_64 {
            return None;
        }
        let ncmds = std::ptr::read_volatile((header + 16) as *const u32);
        let sizeofcmds = std::ptr::read_volatile((header + 20) as *const u32) as usize;
        let mut offset = 32usize; // struct mach_header_64
        let end = 32usize.checked_add(sizeofcmds)?;
        for _ in 0..ncmds {
            if offset + 8 > end {
                break;
            }
            let cmd = std::ptr::read_volatile((header + offset) as *const u32);
            let cmdsize = std::ptr::read_volatile((header + offset + 4) as *const u32) as usize;
            if cmdsize == 0 || offset + cmdsize > end {
                break;
            }
            if cmd == LC_SEGMENT_64 {
                // struct segment_command_64: cmd(4) cmdsize(4) segname(16)...
                let segname = std::ptr::read_volatile((header + offset + 8) as *const [u8; 16]);
                if segname.starts_with(b"__TEXT\0") {
                    // section_64 entries start at segment offset 72; each is
                    // 80 bytes: sectname(16) segname(16) addr(8) size(8)...
                    let nsects =
                        std::ptr::read_volatile((header + offset + 64) as *const u32) as usize;
                    let sections = header + offset + 72;
                    for s in 0..nsects {
                        let base = sections + s * 80;
                        let sectname = std::ptr::read_volatile(base as *const [u8; 16]);
                        if sectname.starts_with(b"__text\0") {
                            let addr = std::ptr::read_volatile((base + 32) as *const u64);
                            let size = std::ptr::read_volatile((base + 40) as *const u64);
                            // Section `addr` is a vmaddr; the runtime pointer
                            // needs the image slide. The dyld header address
                            // equals the slide for the __TEXT segment, so
                            // header serves as the slide here.
                            return Some((header.wrapping_add(addr as usize), size));
                        }
                    }
                }
            }
            offset += cmdsize;
        }
    }
    None
}

unsafe extern "C" {
    fn _dyld_image_count() -> u32;
    fn _dyld_get_image_name(index: u32) -> *const std::ffi::c_char;
    fn _dyld_get_image_header(index: u32) -> *const std::ffi::c_void;
}
