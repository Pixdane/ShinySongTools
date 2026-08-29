//! 编译失败：`MainThreadToken` 是 `!Send` 的主线程 capability；把引用或
//! 值跨线程转移必须被类型系统拒绝。

use corelib::MainThreadToken;

fn leak_across_threads(token: &'static MainThreadToken) {
    std::thread::spawn(move || {
        // `&MainThreadToken` is not Send (`MainThreadToken` is !Send+!Sync):
        // capturing the reference in a spawned closure must not compile.
        // (A bare `let _ = token;` elides the capture entirely and would
        // compile — mem::forget forces the capture.)
        core::mem::forget(token);
    });
}

fn main() {
    // SAFETY: not reached; this is a compile-fail fixture.
    unsafe {
        let _ = MainThreadToken::assume_main_thread();
    }
    let _ = leak_across_threads;
}
