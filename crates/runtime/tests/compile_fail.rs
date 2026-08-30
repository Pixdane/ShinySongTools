//! trybuild 编译期 fixture 门：跨 phase 注册与 Unpublished 态 install 必须
//! 编译失败。对应 core crate Rustdoc「Phase systems」「Hook typestate」与
//! 验证顺序 §2.12 第 2、3 条。

#[test]
fn ui() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/ui/*.rs");
}
