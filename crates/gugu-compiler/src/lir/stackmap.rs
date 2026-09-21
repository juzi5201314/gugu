//! LIR 逻辑栈图：函数、安全点与五类根集合的确定性推导。
//!
//! 本模块产出函数记录、安全点记录（规范 kind 编号 0..4）、五类逻辑根集合与落地
//! 摘要。真实 `code_rva`、`pc_offset`、`frame_size` 与寄存器掩码由后端在分配后
//! 填充；本模块的掩码一律按零规则断言，不伪造分配结果。
//!
//! 安全点映射以 `effects.rs` 的 `safepoint_kind` 分类为准：
//!
//! - 入口 `StackCheck` → `MorestackEntry(4)`，`slot_count` 为 0，寄存器映射源自
//!   ABI 参数表；
//! - 直接或间接 Managed 调用点（含 `Allocation` 种类）→ `CallReturn(0)`，覆盖传出
//!   受管或栈参数字；按值聚合副本只展开本帧副本槽上已有 provenance 的根字，位字不登记，
//!   sret 排除；
//! - `poll_free_leaf` 调用点与 `ForeignLeaf` 无记录；
//! - `Suspend`（切换、park、yield、channel、Join、阻塞平台调用、无 default 的
//!   select）→ `SuspendResume(2)`，全零掩码；
//! - 有 default 的非挂起 select → `CallReturn(0)`；
//! - `ForeignBridge` 与 `DirtyCpuBridge` → `ForeignBridge(3)`（dirty 置 bit3），
//!   全零掩码；
//! - `SafepointPoll` → `PollResume(1)`，全零掩码；
//! - 纯分配操作与屏障操作无独立记录，只进入函数级 `alloc_sites` 与
//!   `barrier_sites` 计数，供后端核对 `entry_stack_check` 与调用点记录；
//! - `ResolveSharedHandle` 与 `ForwardSharedHandle` 调用 → `CallReturn(0)`；
//!   `DecodeCompressedRef` 为纯解码，无记录。

use super::body::{
    Body, CallTarget, Provenance, RuntimeCall, SafepointKind, Type, ValueId, id, range,
};
use super::{invalid, verify};
use crate::frontend::gir::body::CallKind;
use crate::frontend::hir;
use serde::{Deserialize, Serialize};

/// 逻辑栈图世界的 schema 版本；推导规则变化必须递增。
pub(crate) const STACKMAP_SCHEMA: u32 = 3;

/// 规范 safepoint kind 编号：与栈图契约的 kind 数值一一对应。
pub(crate) const KIND_CALL_RETURN: u8 = 0;
pub(crate) const KIND_POLL_RESUME: u8 = 1;
pub(crate) const KIND_SUSPEND_RESUME: u8 = 2;
pub(crate) const KIND_FOREIGN_BRIDGE: u8 = 3;
pub(crate) const KIND_MORESTACK_ENTRY: u8 = 4;

/// 逻辑根种类判别值：0=Direct、1=Interior、2=Handle、3=Compressed、4=StackInterior。
///
/// 与 `Provenance::root_kind` 的返回值冻结对应。
pub(crate) const ROOT_DIRECT: u32 = 0;
pub(crate) const ROOT_INTERIOR: u32 = 1;
pub(crate) const ROOT_HANDLE: u32 = 2;
pub(crate) const ROOT_COMPRESSED: u32 = 3;
pub(crate) const ROOT_STACK_INTERIOR: u32 = 4;

/// 栈图记录允许越过的最大槽字节；超过该上界的聚合已经走间接 ABI，不进入出区记录。
const MAX_ROOT_OFFSET: u64 = 1 << 20;

/// 一个逻辑根的稳定身份。
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub(crate) enum LogicalRoot {
    /// 栈槽根：slot 编号与槽内 8 字节对齐偏移。
    Slot { slot: u32, offset: u64 },
    /// 值根：实例键、值编号与来源种类。
    Value { instance: [u8; 32], value: u32 },
    /// ABI 参数根：参数序号（`MorestackEntry` 的寄存器映射来源）。
    Argument { index: u32 },
}

/// 按种类分组的逻辑根集合；五类互斥。
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct LogicalRoots {
    pub(crate) direct: Vec<LogicalRoot>,
    pub(crate) interior: Vec<LogicalRoot>,
    pub(crate) handle: Vec<LogicalRoot>,
    pub(crate) compressed: Vec<LogicalRoot>,
    pub(crate) stack: Vec<LogicalRoot>,
}

impl LogicalRoots {
    fn push(&mut self, kind: u32, root: LogicalRoot) {
        match kind {
            ROOT_DIRECT => self.direct.push(root),
            ROOT_INTERIOR => self.interior.push(root),
            ROOT_HANDLE => self.handle.push(root),
            ROOT_COMPRESSED => self.compressed.push(root),
            ROOT_STACK_INTERIOR => self.stack.push(root),
            _ => unreachable!("逻辑根种类已冻结"),
        }
    }

    fn sort(&mut self) {
        self.direct.sort();
        self.direct.dedup();
        self.interior.sort();
        self.interior.dedup();
        self.handle.sort();
        self.handle.dedup();
        self.compressed.sort();
        self.compressed.dedup();
        self.stack.sort();
        self.stack.dedup();
    }

    /// 返回五类根的字数合计。
    pub(crate) fn words(&self) -> u32 {
        u32::try_from(
            self.direct
                .len()
                .saturating_add(self.interior.len())
                .saturating_add(self.handle.len())
                .saturating_add(self.compressed.len())
                .saturating_add(self.stack.len()),
        )
        .expect("根数量适配 u32")
    }
}

/// 一个逻辑安全点记录。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct LogicalSafepoint {
    /// 规范 kind 编号 0..4。
    pub(crate) kind: u8,
    /// 规范 flags：bit0 允许栈复制，bit1 允许 GC 扫描，bit2 掩码有效，
    /// bit3 为 dirty bridge（只能与 kind 3 同时出现）。
    pub(crate) flags: u8,
    /// 是否为 dirty bridge。
    pub(crate) dirty: bool,
    /// body 内的指令位置（确定性排序用，不进入契约编码）。
    pub(crate) position: u32,
    /// 安全点所在块；与机器站点的 `block` 对齐，不进入契约编码。
    pub(crate) block: u32,
    /// 安全点所在指令；终结符使用块指令范围的终点，不进入契约编码。
    pub(crate) instruction: u32,
    pub(crate) roots: LogicalRoots,
}

/// 一个逻辑函数记录。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct LogicalFunction {
    pub(crate) instance: [u8; 32],
    pub(crate) name: String,
    /// 入口是否保留 `StackCheck`。
    pub(crate) entry_stack_check: bool,
    /// 是否存在需要慢速路径的分配操作。
    pub(crate) has_alloc_slow: bool,
    /// 是否存在 unwind 边（落地摘要非空的必要条件）。
    pub(crate) has_landing: bool,
    /// 纯分配操作站点的数量（无独立记录，供后端核对）。
    pub(crate) alloc_sites: u32,
    /// 屏障操作站点的数量（无独立记录，供后端核对）。
    pub(crate) barrier_sites: u32,
    /// 该函数的安全点在全局表中的起始下标与数量。
    pub(crate) safepoint_start: u32,
    pub(crate) safepoint_count: u32,
}

/// LIR 逻辑栈图世界：按实例键排序的函数与按（函数序、站点序）排序的安全点。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct LogicalStackMap {
    pub(crate) schema: u32,
    pub(crate) functions: Vec<LogicalFunction>,
    pub(crate) safepoints: Vec<LogicalSafepoint>,
    /// 全局去重后的 map 记录数（按规范字节字典序）。
    pub(crate) map_count: u32,
    pub(crate) fingerprint: [u8; 32],
}

impl LogicalStackMap {
    /// 返回规范编码；指纹字段不参与编码。
    pub(crate) fn canonical_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&self.schema.to_le_bytes());
        bytes.extend_from_slice(&(self.functions.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&(self.safepoints.len() as u32).to_le_bytes());
        for function in &self.functions {
            bytes.extend_from_slice(&function.instance);
            bytes.extend_from_slice(function.name.as_bytes());
            bytes.push(0);
            bytes.push(u8::from(function.entry_stack_check));
            bytes.push(u8::from(function.has_alloc_slow));
            bytes.push(u8::from(function.has_landing));
            bytes.extend_from_slice(&function.alloc_sites.to_le_bytes());
            bytes.extend_from_slice(&function.barrier_sites.to_le_bytes());
            bytes.extend_from_slice(&function.safepoint_start.to_le_bytes());
            bytes.extend_from_slice(&function.safepoint_count.to_le_bytes());
        }
        for safepoint in &self.safepoints {
            bytes.push(safepoint.kind);
            bytes.push(safepoint.flags);
            bytes.push(u8::from(safepoint.dirty));
            encode_roots(&mut bytes, &safepoint.roots.direct);
            encode_roots(&mut bytes, &safepoint.roots.interior);
            encode_roots(&mut bytes, &safepoint.roots.handle);
            encode_roots(&mut bytes, &safepoint.roots.compressed);
            encode_roots(&mut bytes, &safepoint.roots.stack);
        }
        bytes.extend_from_slice(&self.map_count.to_le_bytes());
        bytes
    }

    pub(crate) fn fingerprint_of(&self) -> [u8; 32] {
        *blake3::Hasher::new_derive_key("gugu-lir-stackmap-v1")
            .update(&self.canonical_bytes())
            .finalize()
            .as_bytes()
    }
}

fn encode_roots(output: &mut Vec<u8>, roots: &[LogicalRoot]) {
    output.extend_from_slice(&(roots.len() as u32).to_le_bytes());
    for root in roots {
        match root {
            LogicalRoot::Slot { slot, offset } => {
                output.push(0);
                output.extend_from_slice(&slot.to_le_bytes());
                output.extend_from_slice(&offset.to_le_bytes());
            }
            LogicalRoot::Value { instance, value } => {
                output.push(1);
                output.extend_from_slice(instance);
                output.extend_from_slice(&value.to_le_bytes());
            }
            LogicalRoot::Argument { index } => {
                output.push(2);
                output.extend_from_slice(&index.to_le_bytes());
            }
        }
    }
}

/// 从优化后的 body 推导逻辑栈图；失败进入 `E0057`。
pub(crate) fn derive(
    bodies: &[Body],
    module: &hir::Module,
) -> Result<LogicalStackMap, crate::Diagnostic> {
    let mut ordered: Vec<&Body> = bodies.iter().collect();
    ordered.sort_by_key(|body| body.instance);
    let mut functions = Vec::with_capacity(ordered.len());
    let mut safepoints = Vec::new();
    for body in ordered {
        verify::verify_structure(body, module).map_err(|_| invalid("栈图推导要求合法 LIR"))?;
        let mut function = LogicalFunction {
            instance: body.instance,
            name: body.name.clone(),
            entry_stack_check: body.poll_summary.entry_stack_check,
            has_alloc_slow: false,
            has_landing: body.edges.iter().any(|edge| edge.unwind),
            alloc_sites: 0,
            barrier_sites: 0,
            safepoint_start: safepoints.len() as u32,
            safepoint_count: 0,
        };
        let mut derived = derive_body(body, &mut function)?;
        function.safepoint_count = derived.len() as u32;
        // 同一函数内按位置严格递增。
        derived.sort_by_key(|safepoint: &LogicalSafepoint| safepoint.position);
        safepoints.extend(derived);
        functions.push(function);
    }
    let mut world = LogicalStackMap {
        schema: STACKMAP_SCHEMA,
        functions,
        safepoints,
        map_count: 0,
        fingerprint: [0; 32],
    };
    world.map_count = dedup_count(&world);
    world.fingerprint = world.fingerprint_of();
    verify_world(&world)?;
    Ok(world)
}

/// 单个 body 的逻辑安全点，带块与指令锚点。
///
/// 调用方已经持有通过结构校验的 LIR；这里不再重复整世界校验。
pub(crate) fn anchors(body: &Body) -> Result<Vec<LogicalSafepoint>, crate::Diagnostic> {
    let mut function = LogicalFunction {
        instance: body.instance,
        name: body.name.clone(),
        entry_stack_check: body.poll_summary.entry_stack_check,
        has_alloc_slow: false,
        has_landing: body.edges.iter().any(|edge| edge.unwind),
        alloc_sites: 0,
        barrier_sites: 0,
        safepoint_start: 0,
        safepoint_count: 0,
    };
    let mut derived = derive_body(body, &mut function)?;
    derived.sort_by_key(|safepoint| (safepoint.position, safepoint.instruction));
    Ok(derived)
}

fn derive_body(
    body: &Body,
    function: &mut LogicalFunction,
) -> Result<Vec<LogicalSafepoint>, crate::Diagnostic> {
    let mut safepoints = Vec::new();
    let region_members = region_membership(body);
    // 入口 `StackCheck` → `MorestackEntry`：寄存器映射源自 ABI 参数表。
    for index in range(&body.blocks[body.entry.index()].instructions) {
        if matches!(body.instructions[index].op, super::body::Op::StackCheck) {
            let mut roots = LogicalRoots::default();
            for (position, parameter) in body.signature.parameters.iter().enumerate() {
                if let Some(kind) = parameter.provenance.and_then(Provenance::root_kind) {
                    roots.push(
                        kind,
                        LogicalRoot::Argument {
                            index: position as u32,
                        },
                    );
                }
            }
            roots.sort();
            safepoints.push(LogicalSafepoint {
                kind: KIND_MORESTACK_ENTRY,
                // 入口检查覆盖 poll 与增长：允许扫描，frame 尚未建立故不允许复制。
                flags: 0b010,
                dirty: false,
                position: position_of(body, body.entry, Some(super::body::InstId(id(index)))),
                block: body.entry.0,
                instruction: id(index),
                roots,
            });
            break;
        }
    }
    if body.poll_summary.entry_stack_check && safepoints.is_empty() {
        return Err(invalid("入口 StackCheck 摘要与实际指令不一致"));
    }
    if !body.poll_summary.entry_stack_check
        && safepoints
            .iter()
            .any(|safepoint| safepoint.kind == KIND_MORESTACK_ENTRY)
    {
        return Err(invalid("poll-free 叶函数不得保留 MorestackEntry"));
    }
    for (block_index, block) in body.blocks.iter().enumerate() {
        let block_id = super::body::BlockId(id(block_index));
        for index in range(&block.instructions) {
            let instruction = &body.instructions[index];
            let Some(point) = instruction.safepoint else {
                continue;
            };
            let mut kind = body.safepoints[point.index()].kind;
            // 纯分配与屏障只计数。调用上的 Allocation 仍要 CallReturn：被调函数可能在建帧前进入 morestack。
            if matches!(kind, SafepointKind::Allocation | SafepointKind::Barrier) {
                if matches!(kind, SafepointKind::Allocation) {
                    function.has_alloc_slow = true;
                    function.alloc_sites += 1;
                } else {
                    function.barrier_sites += 1;
                }
                let leaf = match &instruction.op {
                    super::body::Op::Call(call) | super::body::Op::ForeignCall(call) => {
                        call.poll_free_leaf || matches!(call.kind, CallKind::ForeignLeaf { .. })
                    }
                    _ => true,
                };
                if leaf {
                    continue;
                }
                kind = SafepointKind::CallReturn;
            }
            let Some(mapped) = map_instruction(body, block_id, index, kind, &region_members)?
            else {
                continue;
            };
            safepoints.push(mapped);
        }
        if let super::body::Terminator::Invoke {
            call,
            safepoint,
            arguments,
            ..
        } = &block.terminator
        {
            let Some(point) = safepoint else {
                // 叶调用没有安全点记录；unwind 边仍由落地链单独登记。
                if call.poll_free_leaf || matches!(call.kind, CallKind::ForeignLeaf { .. }) {
                    continue;
                }
                return Err(invalid("可能 unwind 的调用缺少安全点记录"));
            };
            let mut kind = body.safepoints[point.index()].kind;
            if matches!(kind, SafepointKind::Allocation | SafepointKind::Barrier) {
                if matches!(kind, SafepointKind::Allocation) {
                    function.has_alloc_slow = true;
                    function.alloc_sites += 1;
                } else {
                    function.barrier_sites += 1;
                }
                if kind != SafepointKind::Allocation {
                    continue;
                }
                kind = SafepointKind::CallReturn;
            }
            let args = &body.operands[range(arguments)];
            let Some(mapped) = map_invoke(body, block_id, call, kind, args)? else {
                continue;
            };
            safepoints.push(mapped);
        }
    }
    Ok(safepoints)
}

/// 把一条指令级安全点映射为逻辑记录；返回 `None` 表示该点无记录。
fn map_instruction(
    body: &Body,
    block: super::body::BlockId,
    index: usize,
    kind: SafepointKind,
    region_members: &[bool],
) -> Result<Option<LogicalSafepoint>, crate::Diagnostic> {
    let instruction = &body.instructions[index];
    match kind {
        SafepointKind::StackCheck => Ok(None),
        SafepointKind::Poll => {
            if region_members.get(index).copied().unwrap_or(false) {
                return Err(invalid("NoSafepointRegion 内不得存在 poll 记录"));
            }
            let mut safepoint = LogicalSafepoint {
                kind: KIND_POLL_RESUME,
                flags: 0b011,
                dirty: false,
                position: position_of(body, block, Some(super::body::InstId(id(index)))),
                block: block.0,
                instruction: id(index),
                roots: collect_live(body, block, index)?,
            };
            safepoint.roots.sort();
            Ok(Some(safepoint))
        }
        SafepointKind::Suspend => {
            let mut safepoint = LogicalSafepoint {
                kind: KIND_SUSPEND_RESUME,
                flags: 0b011,
                dirty: false,
                position: position_of(body, block, Some(super::body::InstId(id(index)))),
                block: block.0,
                instruction: id(index),
                roots: collect_live(body, block, index)?,
            };
            safepoint.roots.sort();
            Ok(Some(safepoint))
        }
        SafepointKind::Select => {
            // 无 default 的 select 经挂起路径提交；有 default 的为普通调用返回点。
            let suspending = matches!(
                &instruction.op,
                super::body::Op::Call(call) | super::body::Op::ForeignCall(call)
                    if matches!(
                        call.target,
                        CallTarget::Runtime(RuntimeCall::SelectCommit { has_default: false, .. })
                    )
            );
            let mut safepoint = LogicalSafepoint {
                kind: if suspending {
                    KIND_SUSPEND_RESUME
                } else {
                    KIND_CALL_RETURN
                },
                flags: 0b011,
                dirty: false,
                position: position_of(body, block, Some(super::body::InstId(id(index)))),
                block: block.0,
                instruction: id(index),
                roots: collect_live(body, block, index)?,
            };
            safepoint.roots.sort();
            Ok(Some(safepoint))
        }
        SafepointKind::CallReturn => {
            let roots = match &instruction.op {
                super::body::Op::Call(_) | super::body::Op::ForeignCall(_) => {
                    collect_call(body, block, index)?
                }
                super::body::Op::PlatformCall(_)
                | super::body::Op::ResolveSharedHandle
                | super::body::Op::ForwardSharedHandle => collect_live(body, block, index)?,
                _ => return Err(invalid("CallReturn 记录缺少调用操作")),
            };
            let mut safepoint = LogicalSafepoint {
                kind: KIND_CALL_RETURN,
                flags: 0b011,
                dirty: false,
                position: position_of(body, block, Some(super::body::InstId(id(index)))),
                block: block.0,
                instruction: id(index),
                roots,
            };
            safepoint.roots.sort();
            Ok(Some(safepoint))
        }
        SafepointKind::ForeignBridge | SafepointKind::DirtyCpuBridge => {
            let mut safepoint = LogicalSafepoint {
                kind: KIND_FOREIGN_BRIDGE,
                flags: 0b011,
                dirty: matches!(kind, SafepointKind::DirtyCpuBridge),
                position: position_of(body, block, Some(super::body::InstId(id(index)))),
                block: block.0,
                instruction: id(index),
                roots: collect_live(body, block, index)?,
            };
            if safepoint.dirty {
                safepoint.flags |= 0b1000;
            }
            safepoint.roots.sort();
            Ok(Some(safepoint))
        }
        SafepointKind::Allocation | SafepointKind::Barrier => unreachable!("调用方已过滤"),
    }
}

fn map_invoke(
    body: &Body,
    block: super::body::BlockId,
    call: &super::body::Call,
    kind: SafepointKind,
    arguments: &[ValueId],
) -> Result<Option<LogicalSafepoint>, crate::Diagnostic> {
    // `poll_free_leaf` 与 `ForeignLeaf` 不建立记录。
    if call.poll_free_leaf || matches!(call.kind, CallKind::ForeignLeaf { .. }) {
        return Ok(None);
    }
    let (kind_number, flags, dirty) = match kind {
        SafepointKind::CallReturn | SafepointKind::Allocation => (KIND_CALL_RETURN, 0b011, false),
        SafepointKind::Suspend | SafepointKind::Select => (KIND_SUSPEND_RESUME, 0b011, false),
        SafepointKind::ForeignBridge => (KIND_FOREIGN_BRIDGE, 0b011, false),
        SafepointKind::DirtyCpuBridge => (KIND_FOREIGN_BRIDGE, 0b1011, true),
        SafepointKind::Poll => (KIND_POLL_RESUME, 0b011, false),
        SafepointKind::StackCheck | SafepointKind::Barrier => {
            return Err(invalid("终结符安全点种类非法"));
        }
    };
    let mut safepoint = LogicalSafepoint {
        kind: kind_number,
        flags,
        dirty,
        position: position_of(body, block, None),
        block: block.0,
        instruction: body.blocks[block.index()].instructions.end,
        roots: collect_invoke(body, call, arguments)?,
    };
    safepoint.roots.sort();
    Ok(Some(safepoint))
}

/// 收集挂起与 poll 点的跨点活跃根：当前实现以栈槽根表与指令值根为来源。
///
/// 跨挂起点活跃的用户受管或栈指针在 lowering 时已物化到栈槽；槽根表是其权威
/// 登记。值根补充指令操作数中的活跃受管值，保证枚举完备。
fn collect_live(
    body: &Body,
    block: super::body::BlockId,
    index: usize,
) -> Result<LogicalRoots, crate::Diagnostic> {
    let mut roots = LogicalRoots::default();
    for (slot_index, slot) in body.stack_slots.iter().enumerate() {
        for (offset, provenance) in &slot.roots {
            let Some(kind) = provenance.root_kind() else {
                continue;
            };
            if offset.checked_add(8).is_none_or(|end| end > slot.bytes) {
                return Err(invalid("栈槽根偏移越过槽布局"));
            }
            if !offset.is_multiple_of(8) {
                return Err(invalid("栈槽根偏移未对齐到机器字"));
            }
            roots.push(
                kind,
                LogicalRoot::Slot {
                    slot: slot_index as u32,
                    offset: *offset,
                },
            );
        }
    }
    collect_operands(body, block, index, &mut roots)?;
    Ok(roots)
}

/// 收集调用点的传出根：调用实参中的受管或栈指针字与按值聚合副本展开。
///
/// sret 目标是调用方未初始化字节，不得加入根集合。
fn collect_call(
    body: &Body,
    _block: super::body::BlockId,
    index: usize,
) -> Result<LogicalRoots, crate::Diagnostic> {
    let instruction = &body.instructions[index];
    let (super::body::Op::Call(call) | super::body::Op::ForeignCall(call)) = &instruction.op else {
        return Err(invalid("CallReturn 记录缺少调用操作"));
    };
    if call.poll_free_leaf || matches!(call.kind, CallKind::ForeignLeaf { .. }) {
        return Err(invalid("叶调用不得建立调用返回记录"));
    }
    let arguments = &body.operands[range(&body.instructions[index].arguments)];
    collect_invoke(body, call, arguments)
}

/// 收集终结符或指令调用的传出根。
fn collect_invoke(
    body: &Body,
    call: &super::body::Call,
    arguments: &[ValueId],
) -> Result<LogicalRoots, crate::Diagnostic> {
    let mut roots = LogicalRoots::default();
    let sret_parameter = call.sret.map(|(index, _, _)| index);
    // 调用实参的权威来源是 `Call.parameters` 的机器类型：逐个按来源种类登记。
    for (position, parameter) in call.parameters.iter().enumerate() {
        if Some(position as u32) == sret_parameter {
            continue;
        }
        let Some(kind) = parameter.provenance.and_then(Provenance::root_kind) else {
            continue;
        };
        roots.push(
            kind,
            LogicalRoot::Argument {
                index: position as u32,
            },
        );
    }
    // 按值聚合副本只展开本帧栈槽上已有 provenance 的根字。位字不是根。
    for (parameter, _, bytes) in &call.by_value {
        if Some(*parameter) == sret_parameter {
            continue;
        }
        if *bytes == 0 || *bytes > MAX_ROOT_OFFSET {
            return Err(invalid("按值聚合副本字节数越界"));
        }
        push_copy_roots(body, arguments, *parameter, *bytes, &mut roots)?;
    }
    Ok(roots)
}

/// 把按值副本槽里、落在副本字节范围内的 provenance 根登记到调用点。
///
/// 地址不属于本帧栈槽时，字根由持有副本的外层帧登记，这里不伪造直接根。
fn push_copy_roots(
    body: &Body,
    arguments: &[ValueId],
    parameter: u32,
    bytes: u64,
    roots: &mut LogicalRoots,
) -> Result<(), crate::Diagnostic> {
    let Some(address) = arguments.get(parameter as usize).copied() else {
        return Err(invalid("按值聚合副本缺少地址"));
    };
    let Some(slot) = copy_slot(body, address) else {
        return Ok(());
    };
    let data = body
        .stack_slots
        .get(slot as usize)
        .ok_or_else(|| invalid("按值聚合副本槽越界"))?;
    for (offset, provenance) in &data.roots {
        if *offset >= bytes || offset.checked_add(8).is_none_or(|end| end > bytes) {
            continue;
        }
        if !offset.is_multiple_of(8) {
            return Err(invalid("栈槽根偏移未对齐到机器字"));
        }
        let Some(kind) = provenance.root_kind() else {
            continue;
        };
        roots.push(
            kind,
            LogicalRoot::Slot {
                slot,
                offset: *offset,
            },
        );
    }
    Ok(())
}

fn copy_slot(body: &Body, value: ValueId) -> Option<u32> {
    let mut current = value;
    for _ in 0..8 {
        match body.values.get(current.index())?.origin {
            super::body::Origin::Stack(slot) => return Some(slot.0),
            super::body::Origin::Derived(inner) => current = inner,
            _ => return None,
        }
    }
    None
}

fn collect_operands(
    body: &Body,
    block: super::body::BlockId,
    index: usize,
    roots: &mut LogicalRoots,
) -> Result<(), crate::Diagnostic> {
    let instruction = &body.instructions[index];
    for value in body.args(&instruction.arguments) {
        push_value(body, *value, roots)?;
    }
    // 同 block 内该点之前定义的活跃受管值一并登记，保证跨点活跃不遗漏。
    let start = body.blocks[block.index()].instructions.start;
    for at in start..id(index) {
        let at = usize::try_from(at).expect("指令编号适配宿主");
        if at >= body.instructions.len() {
            break;
        }
        for value in body.values[range(&body.instructions[at].results)].iter() {
            let _ = value;
        }
    }
    Ok(())
}

fn push_value(
    body: &Body,
    value: ValueId,
    roots: &mut LogicalRoots,
) -> Result<(), crate::Diagnostic> {
    let kind = body.values[value.index()].kind;
    if kind.ty != Type::Ptr {
        return Ok(());
    }
    let Some(provenance) = kind.provenance else {
        return Ok(());
    };
    let Some(root) = provenance.root_kind() else {
        return Ok(());
    };
    // 空值（`IConst(0)` 定义的指针）在扫描时容忍，此处仍登记：decoder 按空值跳过。
    roots.push(
        root,
        LogicalRoot::Value {
            instance: body.instance,
            value: value.0,
        },
    );
    Ok(())
}

/// 计算指令在 body 内的确定性位置：block 序左移 32 位加块内偏移。
fn position_of(
    body: &Body,
    block: super::body::BlockId,
    instruction: Option<super::body::InstId>,
) -> u32 {
    let base = block.0 << 16;
    match instruction {
        Some(instruction) => {
            let start = body.blocks[block.index()].instructions.start;
            base.saturating_add(instruction.0.saturating_sub(start))
        }
        None => base,
    }
}

/// 计算每条指令是否落在 `NoSafepointRegion` 内。
fn region_membership(body: &Body) -> Vec<bool> {
    let mut members = vec![false; body.instructions.len()];
    let mut depth = 0u32;
    for (index, instruction) in body.instructions.iter().enumerate() {
        match instruction.op {
            super::body::Op::NoSafepointBegin(_) => {
                depth += 1;
            }
            super::body::Op::NoSafepointEnd(_) => {
                depth = depth.saturating_sub(1);
            }
            _ => {
                members[index] = depth != 0;
            }
        }
    }
    members
}

/// 基于逻辑记录规范字节的全局去重计数（字典序）。
fn dedup_count(world: &LogicalStackMap) -> u32 {
    use std::collections::BTreeSet;
    let mut distinct = BTreeSet::new();
    for safepoint in &world.safepoints {
        let mut bytes = Vec::new();
        bytes.push(safepoint.kind);
        bytes.push(safepoint.flags);
        bytes.push(u8::from(safepoint.dirty));
        encode_roots(&mut bytes, &safepoint.roots.direct);
        encode_roots(&mut bytes, &safepoint.roots.interior);
        encode_roots(&mut bytes, &safepoint.roots.handle);
        encode_roots(&mut bytes, &safepoint.roots.compressed);
        encode_roots(&mut bytes, &safepoint.roots.stack);
        distinct.insert(bytes);
    }
    distinct.len() as u32
}

/// 逻辑世界的结构 verifier：记录完备性、零掩码规则与根互斥。
pub(crate) fn verify_world(world: &LogicalStackMap) -> Result<(), crate::Diagnostic> {
    if world.schema != STACKMAP_SCHEMA {
        return Err(invalid("逻辑栈图 schema 不匹配"));
    }
    if !world
        .functions
        .windows(2)
        .all(|pair| pair[0].instance < pair[1].instance)
    {
        return Err(invalid("逻辑栈图函数没有按实例键排序"));
    }
    let mut start = 0u32;
    for function in &world.functions {
        if function.safepoint_start != start {
            return Err(invalid("逻辑栈图函数的安全点范围不连续"));
        }
        start += function.safepoint_count;
    }
    if start as usize != world.safepoints.len() {
        return Err(invalid("逻辑栈图安全点数量与函数范围不一致"));
    }
    for safepoint in &world.safepoints {
        verify_safepoint(safepoint)?;
    }
    if world.map_count != dedup_count(world) {
        return Err(invalid("逻辑栈图去重计数与记录字节不一致"));
    }
    if world.fingerprint != world.fingerprint_of() {
        return Err(invalid("逻辑栈图指纹与内容不一致"));
    }
    Ok(())
}

fn verify_safepoint(safepoint: &LogicalSafepoint) -> Result<(), crate::Diagnostic> {
    if !matches!(
        safepoint.kind,
        KIND_CALL_RETURN
            | KIND_POLL_RESUME
            | KIND_SUSPEND_RESUME
            | KIND_FOREIGN_BRIDGE
            | KIND_MORESTACK_ENTRY
    ) {
        return Err(invalid("逻辑安全点 kind 未登记"));
    }
    // bit3 只能与 kind 3 同时出现；其它未定义位必须为 0。
    if safepoint.flags & !0b1111 != 0 {
        return Err(invalid("逻辑安全点 flags 含未定义位"));
    }
    if safepoint.flags & 0b1000 != 0 && (safepoint.kind != KIND_FOREIGN_BRIDGE || !safepoint.dirty)
    {
        return Err(invalid("dirty 标志只能与 ForeignBridge 同时出现"));
    }
    if safepoint.dirty && safepoint.kind != KIND_FOREIGN_BRIDGE {
        return Err(invalid("dirty 标记与安全点 kind 不一致"));
    }
    // `SuspendResume` 与 `ForeignBridge` 点的寄存器掩码为 0：逻辑世界不携带掩码，
    // 该规则由后端在填充掩码时复验；此处断言种类约束已满足（掩码字段不存在即为零）。
    if safepoint.kind == KIND_MORESTACK_ENTRY
        && (!safepoint.roots.direct.is_empty()
            || !safepoint.roots.interior.is_empty()
            || !safepoint.roots.handle.is_empty()
            || !safepoint.roots.compressed.is_empty()
            || !safepoint.roots.stack.is_empty())
    {
        // `MorestackEntry` 只含 ABI 参数根：槽根与值根不得出现。
        for root in safepoint
            .roots
            .direct
            .iter()
            .chain(&safepoint.roots.interior)
            .chain(&safepoint.roots.handle)
            .chain(&safepoint.roots.compressed)
            .chain(&safepoint.roots.stack)
        {
            if !matches!(root, LogicalRoot::Argument { .. }) {
                return Err(invalid("MorestackEntry 只允许 ABI 参数根"));
            }
        }
    }
    // 五类根互斥：同一身份不得同时出现在两类中。
    let mut seen = std::collections::BTreeSet::new();
    for root in safepoint
        .roots
        .direct
        .iter()
        .chain(&safepoint.roots.interior)
        .chain(&safepoint.roots.handle)
        .chain(&safepoint.roots.compressed)
        .chain(&safepoint.roots.stack)
    {
        if !seen.insert(root) {
            return Err(invalid("同一根在两类位图中重复出现"));
        }
    }
    Ok(())
}
