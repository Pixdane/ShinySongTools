//! 编译失败：`install` 只存在于 `Published` 态；`Unpublished` 态的 builder
//! 没有 install 方法（发布先于安装是编译期事实）。

use plugins::hook::{HookSite, HookTarget};
use corelib::TargetId;

pub struct FakeTarget;

impl HookTarget for FakeTarget {
    const TARGET: TargetId = TargetId {
        assembly: "A.dll",
        namespace: "N",
        class: "C",
        method: "M",
        param_count: 0,
    };
    type Original = unsafe extern "C" fn(usize) -> usize;
    fn replacement_addr(original: Self::Original) -> usize {
        original as usize
    }
    unsafe fn original_from_raw(addr: usize) -> Self::Original {
        unsafe { core::mem::transmute::<usize, Self::Original>(addr) }
    }
}

pub struct Container;

plugins::define_hook_site!(SITE: HookSite<FakeTarget, Container>);

fn take(ctx: &mut plugins::AppCtx<'_>) {
    // Unpublished: no container/handler set yet — `install` must not exist.
    ctx.hook(&SITE).install();
}

fn main() {
    let _ = take;
}
