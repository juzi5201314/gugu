//! `gugu-compiler` 端到端集成测试统一入口。
//!
//! 将原本 14 个独立集成测试二进制合并为单一目标，消除 14 份 174 MiB 嵌入 artifact
//! `.rodata` 的重复编译与链接开销。`docs_links.rs` 独立保留以供纯文档门禁快速运行。

mod abi_image_consistency;
mod callback;
mod channel_resume;
mod doc;
mod filesystem;
mod foreign_roots;
mod harness;
mod materialization_equivalence;
mod native_link;
mod network;
mod pin;
mod process;
mod runtime_stats;
mod shared_handle;

#[test]
fn embedded_targets_decode_and_have_distinct_manifests() {
    let linux = gugu_compiler::embedded_artifact(gugu_compiler::TargetName::X86_64Linux)
        .expect("linux artifact");
    let windows = gugu_compiler::embedded_artifact(gugu_compiler::TargetName::X86_64Windows)
        .expect("windows artifact");
    assert_ne!(linux.manifest.target, windows.manifest.target);
}
