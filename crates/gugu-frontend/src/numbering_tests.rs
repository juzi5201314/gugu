//! 编号稳定性：物化内建块的安装路径必须与内建块现算路径派生同一份世界。
//!
//! 两条路径只差「内建块从哪来」：安装路径按 artifact 物化表装载名称与身份表，现算路径把内建
//! 源与用户模块一起重算。编号无关指纹（语义、类型宇宙、mono 图、晚期常量、GIR 放置、LIR、
//! runtime 契约）与诊断集合必须逐项一致；否则 query 缓存会把一条路径的派生结果当成另一条
//! 路径的，而编号前缀不变式也不再成立。
//!
//! 强制现算只在测试构建存在（`Compiler::deriving_builtins`）；生产路径缺少物化表仍是
//! compiler invariant 失败。

use std::sync::LazyLock;

use crate::testing;
use crate::{Compilation, CompileRequest, Compiler, TargetName};

/// 会话：安装路径与现算路径共用同一 query session，跨用例复用。
///
/// 共用 session 意味着两条路径的派生结果必须靠查询键区分；把缓存污染也纳入不变量。
fn session() -> &'static Compiler {
    static SESSION: LazyLock<Compiler> =
        LazyLock::new(|| Compiler::with_artifacts(testing::provider()));
    &SESSION
}

/// 编号无关指纹与诊断快照。
///
/// 语义、类型宇宙、mono 图、晚期常量与放置指纹由 `ImagePlan` 提供；GIR/LIR/runtime/x64
/// 指纹由编译结果提供。两处都取值，任何一处漂移都会被比出来。
#[derive(Debug, Eq, PartialEq)]
struct Snapshot {
    semantic: Option<[u8; 32]>,
    types: Option<[u8; 32]>,
    mono: Option<[u8; 32]>,
    instances: Option<u32>,
    roots: Option<u32>,
    late: Option<[u8; 32]>,
    placement: Option<[u8; 32]>,
    gir: Option<[u8; 32]>,
    lir: Option<[u8; 32]>,
    runtime: Option<[u8; 32]>,
    x64: Option<[u8; 32]>,
    diagnostics: String,
}

fn snapshot(compilation: &Compilation) -> Snapshot {
    let plan = compilation.image_plan();
    Snapshot {
        semantic: plan.map(|plan| plan.semantic_fingerprint()),
        types: plan.map(|plan| plan.type_universe_fingerprint()),
        mono: plan.map(|plan| plan.mono_graph_fingerprint()),
        instances: plan.map(|plan| plan.mono_instance_count()),
        roots: plan.map(|plan| plan.mono_root_count()),
        late: plan.map(|plan| plan.late_constants_fingerprint()),
        placement: plan.map(|plan| plan.placement_fingerprint()),
        gir: compilation.gir_fingerprint(),
        lir: compilation.lir_fingerprint(),
        runtime: compilation.runtime_raw_fingerprint(),
        x64: compilation.x64_fingerprint(),
        diagnostics: format!("{:?}", compilation.diagnostics()),
    }
}

/// 同一程序在两条路径上的编号无关指纹与诊断必须逐项一致。
fn assert_numbering_stable(label: &str, request: impl Fn() -> CompileRequest) {
    let installed = session().compile(request());
    let derived = session().deriving_builtins().compile(request());
    assert!(
        !installed.diagnostics().has_errors(),
        "{label}：语料本身必须编译通过：{:?}",
        installed.diagnostics()
    );
    assert_eq!(
        snapshot(&installed),
        snapshot(&derived),
        "{label}：安装物化内建块与现算内建块的派生结果不一致"
    );
}

/// 空程序：只含 `main`。
#[test]
fn empty_program_installs_like_a_fresh_derivation() {
    assert_numbering_stable("empty-main", || {
        CompileRequest::single_file("main.gg", "fn main() {}\n", TargetName::X86_64Linux)
    });
}

/// 泛型约束：接口 bound 驱动的实例化。
#[test]
fn bounded_generics_install_like_a_fresh_derivation() {
    let source = include_str!("frontend/gir/fixtures/bounds.gg");
    assert_numbering_stable("gir/bounds", || {
        CompileRequest::single_file("main.gg", source, TargetName::X86_64Linux)
    });
}

/// 资源类型：字段布局与隐藏类型。
#[test]
fn resource_fields_install_like_a_fresh_derivation() {
    let source = include_str!("frontend/gir/fixtures/resource_fields.gg");
    assert_numbering_stable("gir/resource_fields", || {
        CompileRequest::single_file("main.gg", source, TargetName::X86_64Linux)
    });
}

/// 协程：挂起与恢复路径上的内建机器原语。
#[test]
fn coroutines_install_like_a_fresh_derivation() {
    let source = include_str!("runtime/fixtures/coroutines.gg");
    assert_numbering_stable("runtime/coroutines", || {
        CompileRequest::single_file("main.gg", source, TargetName::X86_64Linux)
    });
}

/// 标准库导入：用户模块引用内建公开面。
#[test]
fn std_imports_install_like_a_fresh_derivation() {
    let source = include_str!("lir/fixtures/effects.gg");
    assert_numbering_stable("lir/effects", || {
        CompileRequest::single_file("main.gg", source, TargetName::X86_64Linux)
    });
}

/// GC 探针：探针库与探针入口程序未被物化，模块位次必须与现算路径一致。
///
/// 用最轻的 `gc-alloc` 用例：探针编译比普通编译重，`all` 用例会顶到 slow-timeout。
#[cfg(feature = "runtime-gc-probes")]
#[test]
fn gc_probe_installs_like_a_fresh_derivation() {
    assert_numbering_stable("probe/gc-alloc", || {
        CompileRequest::runtime_gc_probe("gc-alloc", TargetName::X86_64Linux)
    });
}
