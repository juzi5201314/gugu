//! 内建函数表的 x64 层：录制 absorb 之前的片段 payload，安装时跳过 `CodegenFragment` query。
//!
//! 片段是 body、目标、encoder 与 runtime 布局常量的函数：runtime raw 契约里 lowering 读取的
//! processor/协程偏移由 verifier 钉在 `offset_of!` 常量上，与本 world 的需求无关；
//! `TypeDescriptor`、常量数据与调用目标都以符号重定位留到 world 级解析。唯一读本 world 的是
//! `GcAlloc` / `RegionAlloc` 的 TypeId 立即数（类型宇宙稠密编号），这类 body 不录制本层。
//!
//! 只有 LIR 层同样安装的 body 才安装本层：安装 body 与录制时 codegen 消费的 body 逐字节相同，
//! 压缩策略不同而现算 LIR 的实例照常现算片段，这是确定的分区而不是回退。

use super::codegen::{FragmentPayload, fragment_symbol, invalid_metadata as invalid};
use super::contract;
use super::glue::RuntimeGlueBindings;
use crate::Diagnostic;
use crate::lir::body::{Body, Op};
use crate::runtime::{RuntimeFunctionArtifactV1, RuntimePrebuiltV1};
use gugu_gir::functions::InstalledFunctions;

/// 层名前缀：`function_revisions` 里属于本层的条目。
const LAYER: &str = "x64.";

/// 本层的 revision，按层名排序；安装端要求与物化表里 `x64.` 前缀的条目完全一致。
pub fn revisions() -> Vec<(String, u32)> {
    vec![
        ("x64.allocation".to_owned(), contract::ALLOCATION_REVISION),
        ("x64.codegen".to_owned(), super::codegen::CODEGEN_SCHEMA),
        ("x64.lowering".to_owned(), contract::LOWERING_REVISION),
        ("x64.metadata".to_owned(), contract::METADATA_REVISION),
    ]
}

/// 该 body 的片段是否与 world 无关：分配 lowering 把类型宇宙编号写进立即数。
pub fn recordable(body: &Body) -> bool {
    !body
        .instructions
        .iter()
        .any(|instruction| matches!(instruction.op, Op::GcAlloc { .. } | Op::RegionAlloc { .. }))
}

/// 录制：absorb 之前片段的规范编码。
pub fn encode(fragment: &FragmentPayload) -> Vec<u8> {
    crate::canonical::to_vec(fragment).expect("机器片段编码")
}

/// 本次编译的函数表项；层 revision 与当前编译器不符是 compiler invariant。
pub(super) fn entries(
    prebuilt: Option<&RuntimePrebuiltV1>,
) -> Result<&[RuntimeFunctionArtifactV1], Diagnostic> {
    let Some(prebuilt) = prebuilt.filter(|prebuilt| !prebuilt.functions.is_empty()) else {
        return Ok(&[]);
    };
    let declared: Vec<(String, u32)> = prebuilt
        .function_revisions
        .iter()
        .filter(|(layer, _)| layer.starts_with(LAYER))
        .cloned()
        .collect();
    if declared != revisions() {
        return Err(invalid("内建函数表的 x64 层 revision 与当前编译器不符"));
    }
    Ok(&prebuilt.functions)
}

/// 安装：解码 LIR 层已安装 body 的 x64 层；未录制本层返回 `None`。
///
/// 片段的结构与指纹由调用方按现算路径同一个 verifier 校验；这里另核对符号：glue 绑定按定义
/// 身份解析，录制与安装两侧必须给出同一个函数体符号。
pub(super) fn decode(
    installed: &InstalledFunctions,
    entries: &[RuntimeFunctionArtifactV1],
    body: &Body,
    glues: &RuntimeGlueBindings,
) -> Result<Option<FragmentPayload>, Diagnostic> {
    let entry = installed
        .entry(entries, &body.instance)
        .ok_or_else(|| invalid("LIR 层已安装的实例不在内建函数表的可安装集"))?;
    if entry.x64.is_empty() {
        return Ok(None);
    }
    let fragment: FragmentPayload = crate::canonical::from_slice(&entry.x64)
        .map_err(|_| invalid("内建函数表的 x64 层不能解码"))?;
    if fragment.symbol != fragment_symbol(body, &super::mangle::mangle_function(body), glues) {
        return Err(invalid("内建函数表的 x64 层符号与当前 glue 绑定不符"));
    }
    Ok(Some(fragment))
}
