//! 内建函数表的 LIR 层：录制 poll 之后的 body，安装时跳过 lowering 与逐 body pass。
//!
//! 表项按 GIR 层判定的可安装集选中（单实例 owner、callee 闭合、与类型宇宙编号无关）。录制
//! 固定用 [`RECORDED_COMPRESSION`]：压缩策略改变 lowering，策略不同的编译不安装本层，这是确定
//! 的分区而不是回退。安装的 body 在 world 级 poll 中按 `poll_interface` 播种，最终 verifier
//! 与现算 body 一样完整运行。

use crate::body::{self, Body};
use crate::runtime::{CompressionPolicyV1, RuntimeFunctionArtifactV1, RuntimePrebuiltV1};
use crate::{Diagnostic, invalid};
use gugu_gir::functions::InstalledFunctions;

/// 层名前缀：`function_revisions` 里属于本层的条目。
const LAYER: &str = "lir.";

/// 录制函数表时使用的压缩策略。
pub const RECORDED_COMPRESSION: CompressionPolicyV1 = CompressionPolicyV1::disabled();

/// 本层的 revision，按层名排序；安装端要求与物化表里 `lir.` 前缀的条目完全一致。
pub fn revisions() -> Vec<(String, u32)> {
    vec![
        ("lir.body".to_owned(), body::REVISION),
        (
            "lir.passes".to_owned(),
            crate::pass::policy::PASS_PIPELINE_REVISION,
        ),
        (
            "lir.poll_cost".to_owned(),
            crate::pass::policy::POLL_COST_REVISION,
        ),
        ("lir.schema".to_owned(), crate::SCHEMA),
    ]
}

/// 录制：poll 之后 body 的规范编码。
pub fn encode(body: &Body) -> Vec<u8> {
    crate::canonical::to_vec(body).expect("LIR body 编码")
}

/// 本次编译可安装的 LIR 表项；压缩策略与录制不同则为空。
///
/// 层 revision 与当前编译器不符是 compiler invariant：同一编译器构建的物化表必须一致。
pub(crate) fn entries<'a>(
    prebuilt: Option<&'a RuntimePrebuiltV1>,
    compression: CompressionPolicyV1,
) -> Result<&'a [RuntimeFunctionArtifactV1], Diagnostic> {
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
        return Err(invalid("内建函数表的 LIR 层 revision 与当前编译器不符"));
    }
    Ok(if compression == RECORDED_COMPRESSION {
        &prebuilt.functions
    } else {
        &[]
    })
}

/// 安装：解码表项的 LIR 层，并核对 revision、实例与目标。
pub(crate) fn decode(
    installed: &InstalledFunctions,
    entries: &[RuntimeFunctionArtifactV1],
    instance: &[u8; 32],
    target: &str,
) -> Result<Option<Body>, Diagnostic> {
    if entries.is_empty() {
        return Ok(None);
    }
    let Some(entry) = installed.entry(entries, instance) else {
        return Ok(None);
    };
    let body: Body = crate::canonical::from_slice(&entry.lir)
        .map_err(|_| invalid("内建函数表的 LIR 层不能解码"))?;
    if body.revision != body::REVISION || body.instance != *instance || body.target != target {
        return Err(invalid("内建函数表的 LIR 层没有绑定当前实例或目标"));
    }
    Ok(Some(body))
}
