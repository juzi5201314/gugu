//! 资源隔离闸门：resource 类描述符不得进入 managed region。
//!
//! 资源值（`PassingClass::RESOURCE`，当前登记为 `ResourceCell`）的存储由 runtime
//! 资源域独占管理：`ResourceAcquire`/`ResourceRelease`/`ResourceTransfer`/
//! `ResourceFinalize` 以登记的类型描述符标识它的布局与 slab 归属。managed region 的
//! 分配入口是 `RegionAlloc`（以及逃逸到 `GcAlloc` 的 `TurnRegion` placement），它把值
//! 放进 turn 结束后整体回收的 arena；这与资源显式 lease、显式释放的值语义不相容。
//!
//! `RegionPublish`/`RegionReset` 只作用于 `RegionAlloc` 返回的 region 指针，因此拒绝
//! 资源描述符的 `RegionAlloc` 同时封锁了资源的 publish 与 reset 路径：没有资源值能取得
//! region 指针，也就不可能被发布成跨 turn 快照或被重置回收。分配闸门即覆盖三者。
//!
//! 资源描述符从已合法 lower 的资源调用参数反查：调用参数中由
//! `Op::SymbolAddr(Symbol::TypeDescriptor(key))` 定义的值即该资源的类型描述符键。该扫描
//! 同时覆盖冷编译与缓存恢复两条路径，因此闸门不依赖构造器的正确性。

use crate::Diagnostic;
use crate::frontend::gir::placement::PlacementKind;
use crate::lir::body::{Body, Call, CallTarget, Definition, Op, RuntimeCall, Symbol, Terminator};
use crate::lir::invalid_resource;
use std::collections::BTreeSet;

/// 从所有 body 的资源调用中收集登记的类型描述符键。
///
/// 资源调用必须携带由 `SymbolAddr(Symbol::TypeDescriptor(_))` 定义的描述符参数；缺失即
/// 违反资源域契约，直接按 `E0059` 拒绝，而不是退化成空集合放过后续闸门。
pub(crate) fn resource_descriptors(bodies: &[Body]) -> Result<BTreeSet<[u8; 32]>, Diagnostic> {
    let mut descriptors = BTreeSet::new();
    for body in bodies {
        for instruction in &body.instructions {
            match &instruction.op {
                Op::Call(call) | Op::ForeignCall(call) => {
                    collect(body, call, &instruction.arguments, &mut descriptors)?;
                }
                Op::GcAlloc {
                    descriptor,
                    placement,
                    ..
                } if *placement == PlacementKind::Resource => {
                    descriptors.insert(*descriptor);
                }
                _ => {}
            }
        }
        for block in &body.blocks {
            match &block.terminator {
                Terminator::Invoke {
                    call, arguments, ..
                }
                | Terminator::TailCall {
                    call, arguments, ..
                } => {
                    collect(body, call, arguments, &mut descriptors)?;
                }
                _ => {}
            }
        }
    }
    Ok(descriptors)
}

/// 校验资源值没有进入 managed region 分配。
pub(crate) fn verify(bodies: &[Body]) -> Result<(), Diagnostic> {
    let descriptors = resource_descriptors(bodies)?;
    if descriptors.is_empty() {
        return Ok(());
    }
    for body in bodies {
        for instruction in &body.instructions {
            match &instruction.op {
                Op::RegionAlloc { descriptor, .. } => managed(&descriptors, descriptor)?,
                Op::GcAlloc {
                    descriptor,
                    placement,
                    ..
                } if *placement != PlacementKind::Resource => managed(&descriptors, descriptor)?,
                _ => {}
            }
        }
    }
    Ok(())
}

/// 一条资源调用的描述符必须来自登记的类型描述符符号地址。
fn collect(
    body: &Body,
    call: &Call,
    arguments: &std::ops::Range<u32>,
    descriptors: &mut BTreeSet<[u8; 32]>,
) -> Result<(), Diagnostic> {
    let CallTarget::Runtime(runtime) = call.target else {
        return Ok(());
    };
    if !matches!(
        runtime,
        RuntimeCall::ResourceAcquire
            | RuntimeCall::ResourceRelease
            | RuntimeCall::ResourceTransfer
            | RuntimeCall::ResourceFinalize
    ) {
        return Ok(());
    }
    let values = body.args(arguments);
    let Some(value) = values.get(1) else {
        return Err(invalid_resource("资源调用缺少类型描述符参数"));
    };
    let Definition::Instruction { instruction, .. } = body.values[value.index()].definition else {
        return Err(invalid_resource("资源调用的描述符不是符号地址"));
    };
    if let Op::SymbolAddr(Symbol::TypeDescriptor(key)) = &body.instructions[instruction.index()].op
    {
        descriptors.insert(*key);
        Ok(())
    } else {
        Err(invalid_resource("资源调用描述符不是登记的类型描述符"))
    }
}

/// 拒绝落在资源描述符集合上的 managed region 分配。
fn managed(descriptors: &BTreeSet<[u8; 32]>, descriptor: &[u8; 32]) -> Result<(), Diagnostic> {
    if descriptors.contains(descriptor) {
        return Err(invalid_resource("资源值不能进入 managed region 分配"));
    }
    Ok(())
}
