//! x64 codegen：按 LIR 实例产出机器码片段，每个实例一次 `CodegenFragment` query。
//!
//! # 决策记录
//!
//! - 输入身份：query key 覆盖 LIR world 指纹、实例稳定键、body 指纹、目标名与 encoder
//!   指纹，因此片段只在本实例内容或编码规则变化时失效。
//! - 依赖记录：登记全部源码快照与 LIR world。cage profile 与类型布局已折进 LIR 输入
//!   指纹（`lir::build` 把 `compression` 与类型冻结结果编入身份），因此片段不再单独
//!   依赖 runtime raw 契约与类型宇宙；控制记录字段表是编译期常量，由 lowering revision 覆盖。
//! - 恢复校验：payload 只存字节与重定位，恢复时重算 payload 指纹，并核对站点顺序、
//!   字节范围、重定位范围与寄存器约束偏移都落在站点内。
//! - 站点文本 `line` 是稳定渲染（助记符 + 操作数），供 dump 与人工核对，不参与身份。

use std::fmt::Write as _;
use std::ops::Range;

use serde::{Deserialize, Serialize};

use crate::Diagnostic;
use crate::SourceMap;
use crate::diagnostics::DiagnosticCode;
use crate::frontend::late::universe::TypeUniverse;
use crate::frontend::mono::keys::hash_domain;
use crate::frontend::semantics::query::{restore_errors, store_errors};
use crate::lir;
use crate::lir::body::Body;
use crate::query::{QueryEngine, QueryKey, QueryKind};
use crate::runtime::{RAW_MODEL_SCHEMA, RuntimeRawContractV1};
use crate::target::TargetName;

use super::alloc;
use super::contract::{self, EncoderContract};
use super::encode::assemble;
use super::inst::{
    ColdEdge, ColdEdgeKind, Operand, RegisterConstraint, RelocKind, RelocTarget, Relocation,
    Sequence,
};
use super::lower::Lowered;
use super::reg::Reg;
use super::select;
use super::table;
use super::verify;

/// 片段 payload schema 版本。
///
/// 版本 5 在每个站点记录调用返回 PC 与 morestack 返回 PC，供分配后的栈图按指令边界回填。
pub(crate) const CODEGEN_SCHEMA: u32 = 5;

/// 片段 query key 的域。
const FRAGMENT_KEY_DOMAIN: &str = "gugu-x64-fragment-key-v1";
/// 片段 payload 指纹的域。
const FRAGMENT_DOMAIN: &str = "gugu-x64-fragment-v1";
/// 片段世界指纹的域。
const WORLD_DOMAIN: &str = "gugu-x64-fragments-v1";

/// 一个 lowered 站点的字节、重定位与元数据。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) struct SitePayload {
    /// LIR block 下标。
    pub(crate) block: u32,
    /// block 内指令下标。
    pub(crate) instruction: u32,
    /// lowering 域内的 op 名。
    pub(crate) op: String,
    /// 操作数值编号。
    pub(crate) operands: Vec<u32>,
    /// 结果值编号。
    pub(crate) results: Vec<u32>,
    /// 稳定文本渲染。
    pub(crate) line: String,
    /// 片段内的字节范围。
    pub(crate) bytes: (u32, u32),
    /// 片段重定位表内的下标范围。
    pub(crate) relocations: (u32, u32),
    /// 站点机器指令数。
    pub(crate) instructions: u32,
    /// 物理分配必须满足的约束；偏移是片段绝对偏移。
    pub(crate) constraints: Vec<RegisterConstraint>,
    /// 站点冷边。
    pub(crate) cold_edges: Vec<ColdEdge>,
    /// 被破坏的物理寄存器位图 `(gpr, xmm)`。
    pub(crate) clobbers: (u32, u32),
    /// 该站点的点位下标范围。
    pub(crate) points: (u32, u32),
    /// 站点内最后一条 `call` 的下一条指令偏移；落在片段末尾时为空。
    pub(crate) call_return_pc: Option<u32>,
    /// `morestack_or_poll` 返回后的指令偏移。
    pub(crate) morestack_pc: Option<u32>,
}

/// frame 布局：prologue/epilogue 与 stack map 的公共输入。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) struct FramePayload {
    /// 函数完成的 prologue 之后的 `rsp` 位移。
    pub(crate) frame_size: u32,
    /// outgoing、locals、spill、scratch 与 save slot 组成的负载字节数。
    pub(crate) payload_bytes: u32,
    /// 调用者的 outgoing 区字节数。
    pub(crate) outgoing_bytes: u32,
    /// 栈检查与动态 alloca 检查使用的上限。
    pub(crate) required_frame: u32,
    /// `required_frame` 加上入口返回地址。协程按这个数选初始栈。
    pub(crate) entry_required_frame: u32,
    /// 直接 leaf 调用声明预留的最大额外栈。
    pub(crate) max_leaf_reserve: u32,
    /// 局部槽 `(与 body.stack_slots 同序)`：`(offset, bytes, align)`。
    pub(crate) locals: Vec<(u32, u32, u32)>,
    /// spill slot：`(offset, size, align, root-class)`。
    pub(crate) spill_slots: Vec<(u32, u32, u32, u8)>,
    /// 16 字节 copy scratch 的偏移；未预留为 `None`。
    pub(crate) scratch_offset: Option<u32>,
    /// 实际保存的 callee-saved GPR 编码，按 `rbp,r12,r13` 顺序。
    pub(crate) save_offsets: Vec<(u8, u32)>,
    /// 是否发射入口容量检查。
    pub(crate) checked: bool,
}

impl FramePayload {
    fn of(allocated: &alloc::Allocated) -> Self {
        let frame = &allocated.frame;
        Self {
            frame_size: frame.frame_size,
            payload_bytes: frame.payload_bytes,
            outgoing_bytes: frame.outgoing_bytes,
            required_frame: frame.required_frame,
            entry_required_frame: frame.entry_required_frame,
            max_leaf_reserve: frame.max_leaf_reserve,
            locals: frame
                .locals
                .iter()
                .map(|local| (local.offset, local.bytes, local.align))
                .collect(),
            spill_slots: frame.spill_slots.clone(),
            scratch_offset: frame.scratch_offset(),
            save_offsets: frame.save_offsets.clone(),
            checked: frame.checked(),
        }
    }
}

/// 一个点位的 payload：stack map 的 PC 区间归属。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) struct PointPayload {
    /// 片段 sites 下标。
    pub(crate) site: u32,
    /// 点位种类：0 Normal / 1 Call / 2 Bridge / 3 Prologue。
    pub(crate) kind: u8,
    pub(crate) use_slot: u32,
    pub(crate) def_slot: u32,
    /// 跨该点位存活的值必须回避的 GPR 位图。
    pub(crate) clobber_gpr: u32,
    /// 跨该点位存活的值必须回避的 XMM 位图。
    pub(crate) clobber_xmm: u32,
    /// managed/stack 指针必须落 frame slot。
    pub(crate) pointer_spill: bool,
    /// 调用点 outgoing 区中的指针字 `(value, root, offset)`。
    pub(crate) outgoing_roots: Vec<(u32, u8, u32)>,
}

/// 一个值的分配结果。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) struct ValuePayload {
    /// 值编号。
    pub(crate) value: u32,
    /// 0 GPR / 1 XMM。
    pub(crate) class: u8,
    /// 0 非指针 / 1 heap pointer / 2 stack pointer。
    pub(crate) root: u8,
    /// 区间 `(start, end)`；已死的值为 `(0, 0)`。
    pub(crate) range: (u32, u32),
    /// `(start, end, location)`：`>= 0` 是物理寄存器（GPR 0..15、XMM 16..31），
    /// `< 0` 时 `!(location)` 是 spill slot 下标；空列表表示可重建或已死。
    pub(crate) segments: Vec<(u32, u32, i64)>,
}

impl ValuePayload {
    fn of(value: &alloc::ValueAllocation) -> Self {
        let segments = match value.placement {
            alloc::Placement::Location(location) => {
                vec![(value.range.0, value.range.1, location_code(location))]
            }
            alloc::Placement::Rematerialize | alloc::Placement::Dead => Vec::new(),
        };
        Self {
            value: value.value,
            class: value.class.code(),
            root: value.root.code(),
            range: if value.is_live() { value.range } else { (0, 0) },
            segments,
        }
    }
}

/// 位置的 payload 编码。
fn location_code(location: alloc::Location) -> i64 {
    match location {
        alloc::Location::Gpr(gpr) => i64::from(gpr.code()),
        alloc::Location::Xmm(xmm) => 16 + i64::from(xmm.code()),
        alloc::Location::Slot(slot) => -i64::from(slot) - 1,
    }
}

/// 分配统计。
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) struct StatsPayload {
    pub(crate) peak_live_gpr: u32,
    pub(crate) peak_live_xmm: u32,
    pub(crate) spill_slot_count: u32,
    pub(crate) spill_bytes: u32,
    pub(crate) spill_stores: u32,
    pub(crate) reloads: u32,
    pub(crate) rematerializations: u32,
    pub(crate) copy_moves: u32,
    pub(crate) copy_cycles: u32,
    pub(crate) call_sites: u32,
    pub(crate) safepoint_spills: u32,
    pub(crate) allocated_values: u32,
    pub(crate) frame_size_max: u32,
}

impl StatsPayload {
    fn of(allocated: &alloc::Allocated) -> Self {
        let stats = &allocated.stats;
        Self {
            peak_live_gpr: stats.peak_live_gpr,
            peak_live_xmm: stats.peak_live_xmm,
            spill_slot_count: stats.spill_slot_count,
            spill_bytes: stats.spill_bytes,
            spill_stores: stats.spill_stores,
            reloads: stats.reloads,
            rematerializations: stats.rematerializations,
            copy_moves: stats.copy_moves,
            copy_cycles: stats.copy_cycles,
            call_sites: stats.call_sites,
            safepoint_spills: stats.safepoint_spills,
            allocated_values: stats.allocated_values,
            frame_size_max: stats.frame_size_max,
        }
    }
}

/// 一个 LIR 实例的机器码片段。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) struct FragmentPayload {
    /// payload schema 版本。
    pub(crate) schema: u32,
    /// 目标名。
    pub(crate) target: String,
    /// 实例稳定键。
    pub(crate) instance: [u8; 32],
    /// 生成时刻的 LIR world 指纹。
    pub(crate) lir_fingerprint: [u8; 32],
    /// 生成时刻的 encoder 契约指纹。
    pub(crate) encoder_fingerprint: [u8; 32],
    /// 内部 mangled 符号。
    pub(crate) symbol: String,
    /// 重定位引用的内部符号名，按文本升序去重。
    pub(crate) symbols: Vec<String>,
    /// 收缩后的 rel8 条数。
    pub(crate) rel8_count: u32,
    /// 热块数。
    pub(crate) hot_block_count: u32,
    /// 冷块数。
    pub(crate) cold_block_count: u32,
    /// 整数参数槽数。
    pub(crate) abi_integer_args: u32,
    /// 浮点参数槽数。
    pub(crate) abi_float_args: u32,
    /// 栈参数 piece 数。
    pub(crate) abi_stack_slots: u32,
    /// 隐藏 sret。
    pub(crate) sret: bool,
    /// frame 布局。
    pub(crate) frame: FramePayload,
    /// 点位，按函数布局顺序。
    pub(crate) points: Vec<PointPayload>,
    /// 逐值分配结果，按值编号升序。
    pub(crate) values: Vec<ValuePayload>,
    /// 分配统计。
    pub(crate) stats: StatsPayload,
    /// 站点，按 `(block, instruction)` 升序，terminator 紧随块内指令。
    pub(crate) sites: Vec<SitePayload>,
    /// 片段字节。
    pub(crate) bytes: Vec<u8>,
    /// 重定位，按 `(站点, 字段偏移)` 升序。
    pub(crate) relocations: Vec<Relocation>,
    /// payload 指纹。
    pub(crate) fingerprint: [u8; 32],
}

impl FragmentPayload {
    /// 重算 payload 指纹；`fingerprint` 字段参与规范字节时先清零。
    pub(crate) fn compute_fingerprint(&self) -> [u8; 32] {
        let mut canonical = self.clone();
        canonical.fingerprint = [0; 32];
        hash_domain(
            FRAGMENT_DOMAIN,
            &serde_json::to_vec(&canonical).expect("机器片段可序列化"),
        )
    }
}

/// 需要宿主修正的一条重定位。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct X64FragmentRelocation {
    /// 字段在片段字节内的偏移。
    pub offset: u32,
    /// `pc-rel32` / `abs64` / `rva32`。
    pub kind: &'static str,
    /// 目标：内部符号名、`cage-control` 或 `cold:<站点>`。
    pub target: String,
    /// 目标内加数。
    pub addend: i64,
}

/// 片段的 frame 布局视图。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct X64FragmentFrame {
    /// 函数完成的 prologue 之后的 `rsp` 位移。
    pub frame_size: u32,
    /// 负载字节数：outgoing、locals、spill、scratch 与 save slot。
    pub payload_bytes: u32,
    /// 调用者 outgoing 区字节数。
    pub outgoing_bytes: u32,
    /// 栈检查与动态 alloca 检查使用的上限。
    pub required_frame: u32,
    /// `required_frame` 加上入口返回地址。
    pub entry_required_frame: u32,
    /// 16 字节 copy scratch 的偏移；未预留为 `None`。
    pub scratch_offset: Option<u32>,
    /// 溢出槽数量。
    pub spill_slot_count: u32,
    /// 保存的 callee-saved GPR 数量。
    pub saved_gpr_count: u32,
    /// 是否发射入口容量检查。
    pub checked: bool,
    /// `[r14 + 该偏移]` 是协程栈上限；`checked` 为假时无意义。
    pub stack_check_offset: u32,
}

/// 一个编译产物的公开视图：宿主据此映射、修正重定位并执行。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct X64Fragment {
    /// 内部 mangled 符号。
    pub symbol: String,
    /// 机器字节。
    pub bytes: Vec<u8>,
    /// 需要宿主修正的重定位。
    pub relocations: Vec<X64FragmentRelocation>,
    /// frame 布局。
    pub frame: X64FragmentFrame,
}

impl X64Fragment {
    /// 从内部 payload 生成公开视图。
    fn of(fragment: &FragmentPayload, stack_check_offset: u32) -> Self {
        let relocations = fragment
            .relocations
            .iter()
            .map(|relocation| X64FragmentRelocation {
                offset: relocation.offset,
                kind: match relocation.kind {
                    RelocKind::PcRel32 => "pc-rel32",
                    RelocKind::Abs64 => "abs64",
                    RelocKind::Rva32 => "rva32",
                },
                target: match &relocation.target {
                    RelocTarget::Lir(symbol) => super::mangle::mangle_symbol(symbol),
                    RelocTarget::CageControl => "cage-control".to_owned(),
                    RelocTarget::Cold(edge) => format!("cold:{}", edge.site),
                },
                addend: relocation.addend,
            })
            .collect();
        let frame = &fragment.frame;
        Self {
            symbol: fragment.symbol.clone(),
            bytes: fragment.bytes.clone(),
            relocations,
            frame: X64FragmentFrame {
                frame_size: frame.frame_size,
                payload_bytes: frame.payload_bytes,
                outgoing_bytes: frame.outgoing_bytes,
                required_frame: frame.required_frame,
                entry_required_frame: frame.entry_required_frame,
                scratch_offset: frame.scratch_offset,
                spill_slot_count: u32::try_from(frame.spill_slots.len())
                    .expect("溢出槽数量适配 u32"),
                saved_gpr_count: u32::try_from(frame.save_offsets.len())
                    .expect("保存寄存器数量适配 u32"),
                checked: frame.checked,
                stack_check_offset,
            },
        }
    }
}

/// 全部片段的公开视图，按内部稳定键升序。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct X64Fragments {
    /// 入口函数符号。
    pub entry_symbol: String,
    /// 片段列表。
    pub fragments: Vec<X64Fragment>,
}

impl X64Fragments {
    /// 按符号查找片段。
    pub fn find(&self, symbol: &str) -> Option<&X64Fragment> {
        self.fragments
            .iter()
            .find(|fragment| fragment.symbol == symbol)
    }
}

/// 全部实例的机器码片段世界。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct X64World {
    pub(crate) schema: u32,
    pub(crate) target: TargetName,
    pub(crate) contract: EncoderContract,
    /// 片段，按实例稳定键升序（LIR world 已强制该顺序）。
    pub(crate) fragments: Vec<FragmentPayload>,
    /// 入口函数 mangled 符号：`RootCategoryV1::Entry` 实例对应的片段。
    pub(crate) entry_symbol: String,
    /// 分配并编码之后的栈图、展开记录与源码记录。
    pub(crate) metadata: super::metadata::MachineMetadata,
    pub(crate) fingerprint: [u8; 32],
}

impl X64World {
    /// 世界指纹。
    pub(crate) const fn fingerprint(&self) -> [u8; 32] {
        self.fingerprint
    }

    /// encoder 契约指纹。
    pub(crate) fn encoder_fingerprint(&self) -> [u8; 32] {
        self.contract.fingerprint()
    }

    /// lowered 站点总数。
    pub(crate) fn site_count(&self) -> u32 {
        to_u32(
            self.fragments
                .iter()
                .map(|fragment| fragment.sites.len())
                .sum(),
        )
    }

    /// 全部站点机器指令数。
    pub(crate) fn instruction_count(&self) -> u32 {
        to_u32(
            self.fragments
                .iter()
                .flat_map(|fragment| &fragment.sites)
                .map(|site| usize::try_from(site.instructions).expect("指令数适配 usize"))
                .sum::<usize>(),
        )
    }

    /// 全部片段字节数。
    pub(crate) fn encoded_bytes(&self) -> u32 {
        to_u32(
            self.fragments
                .iter()
                .map(|fragment| fragment.bytes.len())
                .sum(),
        )
    }

    /// 重定位总数。
    pub(crate) fn relocation_count(&self) -> u32 {
        to_u32(
            self.fragments
                .iter()
                .map(|fragment| fragment.relocations.len())
                .sum(),
        )
    }

    /// 冷边站点数。
    pub(crate) fn cold_edge_count(&self) -> u32 {
        to_u32(
            self.fragments
                .iter()
                .flat_map(|fragment| &fragment.sites)
                .map(|site| site.cold_edges.len())
                .sum(),
        )
    }

    /// 压缩引用解码序列的字节数。
    pub(crate) fn decode_sequence_bytes(&self) -> u32 {
        to_u32(
            self.fragments
                .iter()
                .flat_map(|fragment| &fragment.sites)
                .filter(|site| site.op == "DecodeCompressedRef")
                .map(|site| {
                    usize::try_from(site.bytes.1 - site.bytes.0).expect("站点字节数适配 usize")
                })
                .sum(),
        )
    }

    /// 全部片段收缩后的 rel8 条数。
    pub(crate) fn rel8_count(&self) -> u32 {
        self.fragments
            .iter()
            .map(|fragment| fragment.rel8_count)
            .sum()
    }

    /// 全部片段热块数。
    pub(crate) fn hot_block_count(&self) -> u32 {
        self.fragments
            .iter()
            .map(|fragment| fragment.hot_block_count)
            .sum()
    }

    /// 全部片段冷块数。
    pub(crate) fn cold_block_count(&self) -> u32 {
        self.fragments
            .iter()
            .map(|fragment| fragment.cold_block_count)
            .sum()
    }

    /// 入口函数 mangled 符号：`RootCategoryV1::Entry` 实例对应的片段。
    pub(crate) fn entry_symbol(&self) -> &str {
        &self.entry_symbol
    }

    /// 全部片段里最大的 `frame_size`。
    pub(crate) fn frame_size_max(&self) -> u32 {
        self.fragments
            .iter()
            .map(|fragment| fragment.frame.frame_size)
            .max()
            .unwrap_or(0)
    }

    /// 全部片段峰值活跃 GPR 数的最大值。
    pub(crate) fn peak_live_gpr(&self) -> u32 {
        self.fragments
            .iter()
            .map(|fragment| fragment.stats.peak_live_gpr)
            .max()
            .unwrap_or(0)
    }

    /// 全部片段峰值活跃 XMM 数的最大值。
    pub(crate) fn peak_live_xmm(&self) -> u32 {
        self.fragments
            .iter()
            .map(|fragment| fragment.stats.peak_live_xmm)
            .max()
            .unwrap_or(0)
    }

    /// 全部片段的溢出重载次数之和。
    pub(crate) fn reload_count(&self) -> u32 {
        self.fragments
            .iter()
            .map(|fragment| fragment.stats.reloads)
            .sum()
    }

    /// 全部片段的溢出写回次数之和。
    pub(crate) fn spill_store_count(&self) -> u32 {
        self.fragments
            .iter()
            .map(|fragment| fragment.stats.spill_stores)
            .sum()
    }

    /// 全部片段的并行拷贝移动总数。
    pub(crate) fn copy_move_count(&self) -> u32 {
        self.fragments
            .iter()
            .map(|fragment| fragment.stats.copy_moves)
            .sum()
    }

    /// 全部片段破环次数之和。
    pub(crate) fn copy_cycle_count(&self) -> u32 {
        self.fragments
            .iter()
            .map(|fragment| fragment.stats.copy_cycles)
            .sum()
    }

    /// 全部片段分配器处理的值总数。
    pub(crate) fn allocated_values(&self) -> u32 {
        self.fragments
            .iter()
            .map(|fragment| fragment.stats.allocated_values)
            .sum()
    }

    /// 全部片段的溢出槽总数。
    pub(crate) fn spill_slot_count(&self) -> u32 {
        self.fragments
            .iter()
            .map(|fragment| fragment.frame.spill_slots.len() as u32)
            .sum()
    }

    /// 全部片段的溢出区字节数。
    pub(crate) fn spill_bytes(&self) -> u32 {
        self.fragments
            .iter()
            .map(|fragment| fragment.stats.spill_bytes)
            .sum()
    }

    /// 全部片段保存的 callee-saved GPR 总数。
    pub(crate) fn saved_gpr_count(&self) -> u32 {
        self.fragments
            .iter()
            .map(|fragment| fragment.frame.save_offsets.len() as u32)
            .sum()
    }

    /// 内存里的机器片段数。
    pub(crate) fn fragment_count(&self) -> u32 {
        to_u32(self.fragments.len())
    }

    /// 全部片段的公开视图。
    pub(crate) fn view(&self, stack_check_offset: u32) -> X64Fragments {
        X64Fragments {
            entry_symbol: self.entry_symbol.clone(),
            fragments: self
                .fragments
                .iter()
                .map(|fragment| X64Fragment::of(fragment, stack_check_offset))
                .collect(),
        }
    }

    /// 返回人类可读的世界 dump：摘要、逐片段与逐站点三行式。
    pub(crate) fn dump(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(
            out,
            "x64 schema={} target={} fragments={} sites={} instructions={} bytes={} relocations={} cold-edges={} decode-bytes={} forms={} encoder={} fingerprint={}",
            self.schema,
            self.target.name(),
            self.fragment_count(),
            self.site_count(),
            self.instruction_count(),
            self.encoded_bytes(),
            self.relocation_count(),
            self.cold_edge_count(),
            self.decode_sequence_bytes(),
            self.contract.forms.len(),
            hex_lower(self.contract.fingerprint()),
            hex_lower(self.fingerprint)
        );
        let metadata = &self.metadata;
        let _ = writeln!(
            out,
            "x64-metadata stackmap={} unwind={} source={} functions={} safepoints={} maps={} unwind-functions={} landings={} sources={} fingerprint={}",
            metadata.stackmap_name,
            metadata.unwind_name,
            metadata.source_name,
            metadata.function_count,
            metadata.safepoint_count,
            metadata.map_count,
            metadata.unwind_function_count,
            metadata.landing_count,
            metadata.source_record_count,
            hex_lower(metadata.fingerprint)
        );
        out.push_str(&self.contract.dump());
        for fragment in &self.fragments {
            let _ = writeln!(
                out,
                "x64-fragment {} symbol={} symbols={} rel8={} hot={} cold={} bytes={} sites={} relocations={} fingerprint={}",
                hex_lower(fragment.instance),
                fragment.symbol,
                fragment.symbols.len(),
                fragment.rel8_count,
                fragment.hot_block_count,
                fragment.cold_block_count,
                fragment.bytes.len(),
                fragment.sites.len(),
                fragment.relocations.len(),
                hex_lower(fragment.fingerprint)
            );
            for site in &fragment.sites {
                let _ = writeln!(
                    out,
                    "x64-site {} {}.{} {} [{}] bytes={}..{} relocations={}..{} points={}..{}",
                    hex_lower(fragment.instance),
                    site.block,
                    site.instruction,
                    site.op,
                    site.line,
                    site.bytes.0,
                    site.bytes.1,
                    site.relocations.0,
                    site.relocations.1,
                    site.points.0,
                    site.points.1
                );
            }
            let frame = &fragment.frame;
            let _ = writeln!(
                out,
                "x64-frame {} size={} payload={} outgoing={} required={} entry={} leaf={} locals={} spills={} scratch={} saves={} checked={}",
                hex_lower(fragment.instance),
                frame.frame_size,
                frame.payload_bytes,
                frame.outgoing_bytes,
                frame.required_frame,
                frame.entry_required_frame,
                frame.max_leaf_reserve,
                frame.locals.len(),
                frame.spill_slots.len(),
                frame
                    .scratch_offset
                    .map_or_else(|| "none".to_owned(), |offset| offset.to_string()),
                frame.save_offsets.len(),
                frame.checked
            );
            let stats = &fragment.stats;
            let _ = writeln!(
                out,
                "x64-stats {} peak-gpr={} peak-xmm={} spill-slots={} spill-bytes={} spill-stores={} reloads={} rematerializations={} copy-moves={} copy-cycles={} call-sites={} safepoint-spills={} values={}",
                hex_lower(fragment.instance),
                stats.peak_live_gpr,
                stats.peak_live_xmm,
                stats.spill_slot_count,
                stats.spill_bytes,
                stats.spill_stores,
                stats.reloads,
                stats.rematerializations,
                stats.copy_moves,
                stats.copy_cycles,
                stats.call_sites,
                stats.safepoint_spills,
                stats.allocated_values
            );
            for value in &fragment.values {
                if value.root == 0 {
                    continue;
                }
                let place = value.segments.first().map_or("none", |(_, _, location)| {
                    if *location < 0 { "slot" } else { "reg" }
                });
                let _ = writeln!(
                    out,
                    "x64-root {} v={} root={} place={}",
                    hex_lower(fragment.instance),
                    value.value,
                    value.root,
                    place
                );
            }
            let outgoing = fragment
                .points
                .iter()
                .map(|point| point.outgoing_roots.len())
                .sum::<usize>();
            if outgoing > 0 {
                let _ = writeln!(
                    out,
                    "x64-outgoing {} n={outgoing}",
                    hex_lower(fragment.instance)
                );
            }
        }
        out
    }
}
/// 构建机器片段世界；`entry` 是 `MonoRoots` 里 `RootCategoryV1::Entry` 类别实例的键。
pub(crate) fn build(
    lir: &lir::Validated,
    universe: &TypeUniverse,
    raw: &RuntimeRawContractV1,
    target: TargetName,
    queries: &QueryEngine,
    sources: &SourceMap,
    entry: &[u8; 32],
) -> Result<X64World, Vec<Diagnostic>> {
    let contract = EncoderContract::build(target.descriptor().cpu_baseline);
    contract
        .verify()
        .map_err(|error| vec![invalid(error.message())])?;
    let encoder = contract.fingerprint();
    let lir_fingerprint = lir.fingerprint();
    let mut fragments = Vec::with_capacity(lir.bodies().len());
    let mut entry_symbol = None;
    for body in lir.bodies() {
        let key = QueryKey::new(
            QueryKind::CodegenFragment,
            CODEGEN_SCHEMA,
            fragment_key(lir_fingerprint, body, target, encoder, raw, universe),
        );
        let mut fresh = None;
        let computed = queries.compute(key, |context| {
            for source in sources.snapshots() {
                context.record_dependency(
                    QueryKey::new(QueryKind::SourceSnapshot, 1, source.logical_path()),
                    source.content_hash(),
                );
            }
            context.record_dependency(
                QueryKey::new(QueryKind::BuildLir, lir::SCHEMA, lir_fingerprint),
                lir_fingerprint,
            );
            context.record_dependency(
                QueryKey::new(
                    QueryKind::RuntimeRawModel,
                    RAW_MODEL_SCHEMA,
                    raw.fingerprint(),
                ),
                raw.fingerprint(),
            );
            let fragment = assemble_fragment(
                body,
                universe,
                raw,
                target,
                encoder,
                lir_fingerprint,
                &contract,
            )
            .map_err(|errors| store_errors(&errors))?;
            let bytes = serde_json::to_vec(&fragment).expect("机器片段可序列化");
            fresh = Some(fragment);
            Ok((bytes, Vec::new()))
        });
        let computed = computed.map_err(|error| restore_errors(error, sources))?;
        let fragment = match fresh {
            Some(fragment) => fragment,
            None => serde_json::from_slice(computed.payload())
                .map_err(|_| vec![invalid("缓存机器片段不是合法 schema")])?,
        };
        validate_fragment(&fragment, body, target, encoder, lir_fingerprint)?;
        if body.instance == *entry {
            entry_symbol = Some(fragment.symbol.clone());
        }
        fragments.push(fragment);
    }
    // 入口实例必然有 LIR body（它是闭世界根之一）；缺失说明入口没有走到选指。
    let entry_symbol = entry_symbol.ok_or_else(|| vec![invalid("入口实例没有机器片段")])?;
    let metadata = super::metadata::build(lir, &fragments, sources, target, raw)?;
    let mut world = X64World {
        schema: CODEGEN_SCHEMA,
        target,
        contract,
        fragments,
        entry_symbol,
        metadata,
        fingerprint: [0; 32],
    };
    world.fingerprint = world_fingerprint(&world);
    Ok(world)
}

/// 单个实例的片段 query key。
fn fragment_key(
    lir_fingerprint: [u8; 32],
    body: &Body,
    target: TargetName,
    encoder: [u8; 32],
    raw: &RuntimeRawContractV1,
    universe: &TypeUniverse,
) -> [u8; 32] {
    let mut canonical = Vec::with_capacity(32 * 8 + 16);
    canonical.extend_from_slice(&lir_fingerprint);
    canonical.extend_from_slice(&body.instance);
    canonical.extend_from_slice(&body.fingerprint());
    canonical.extend_from_slice(target.name().as_bytes());
    canonical.extend_from_slice(&encoder);
    canonical.extend_from_slice(&raw.scheduler().fingerprint());
    canonical.extend_from_slice(&raw.coroutine().fingerprint());
    canonical.extend_from_slice(&universe.fingerprint);
    canonical.extend_from_slice(&contract::LOWERING_REVISION.to_le_bytes());
    canonical.extend_from_slice(&contract::ALLOCATION_REVISION.to_le_bytes());
    canonical.extend_from_slice(&contract::METADATA_REVISION.to_le_bytes());
    hash_domain(FRAGMENT_KEY_DOMAIN, &canonical)
}

fn world_fingerprint(world: &X64World) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new_derive_key(WORLD_DOMAIN);
    hasher.update(&world.schema.to_le_bytes());
    hasher.update(world.target.name().as_bytes());
    hasher.update(&world.contract.fingerprint());
    for fragment in &world.fragments {
        hasher.update(&fragment.instance);
        hasher.update(&fragment.fingerprint);
    }
    hasher.update(&world.metadata.fingerprint);
    *hasher.finalize().as_bytes()
}

/// select → layout → encode 整函数片段。
fn assemble_fragment(
    body: &Body,
    universe: &TypeUniverse,
    raw: &RuntimeRawContractV1,
    target: TargetName,
    encoder: [u8; 32],
    lir_fingerprint: [u8; 32],
    contract: &EncoderContract,
) -> Result<FragmentPayload, Vec<Diagnostic>> {
    let selected = select::select_body(body, universe, raw, target)
        .map_err(|error| vec![invalid(&format!("{} 的指令选择失败：{error}", body.name))])?;
    verify::verify_function_sequence(&selected.sequence, contract.baseline)
        .map_err(|error| vec![invalid(&format!("{} 的选指序列非法：{error}", body.name))])?;
    let allocated = alloc::allocate(body, selected, target, raw)?;
    let assembled = assemble(&allocated.sequence)
        .map_err(|error| vec![invalid(&format!("{} 的片段编码失败：{error}", body.name))])?;
    let mut sites = Vec::new();
    let mut inst_cursor = 0_usize;
    let mut site_ordinal = 0_usize;
    for block in &allocated.blocks {
        for site in &block.sites {
            sites.push(site_payload(
                body,
                block.id.0,
                site.instruction,
                site.op,
                &site.lowered,
                allocated.sites.get(site_ordinal),
                &assembled,
                &mut inst_cursor,
            ));
            site_ordinal += 1;
        }
        let term_index = body.blocks[block.id.index()].instructions.end;
        sites.push(site_payload(
            body,
            block.id.0,
            term_index,
            terminator_name(&body.blocks[block.id.index()].terminator),
            &block.terminator,
            allocated.sites.get(site_ordinal),
            &assembled,
            &mut inst_cursor,
        ));
        site_ordinal += 1;
    }
    fill_relocation_ranges(&mut sites, &assembled.relocations);
    let mut fragment = FragmentPayload {
        schema: CODEGEN_SCHEMA,
        target: target.name().to_owned(),
        instance: body.instance,
        lir_fingerprint,
        encoder_fingerprint: encoder,
        symbol: allocated.symbol.clone(),
        symbols: reference_symbols(&assembled.relocations),
        rel8_count: allocated.rel8_count,
        hot_block_count: to_u32(
            allocated
                .layout
                .blocks
                .iter()
                .filter(|block| block.hot)
                .count(),
        ),
        cold_block_count: to_u32(
            allocated
                .layout
                .blocks
                .iter()
                .filter(|block| !block.hot)
                .count(),
        ),
        abi_integer_args: allocated.abi.integer_args,
        abi_float_args: allocated.abi.float_args,
        abi_stack_slots: allocated.abi.stack_slots,
        sret: allocated.abi.sret,
        frame: FramePayload::of(&allocated),
        points: allocated
            .points
            .iter()
            .map(|point| PointPayload {
                site: point.site,
                kind: point.kind.code(),
                use_slot: point.use_slot,
                def_slot: point.def_slot,
                clobber_gpr: point.mask.gpr,
                clobber_xmm: point.mask.xmm,
                pointer_spill: point.pointer_spill,
                outgoing_roots: point
                    .outgoing_roots
                    .iter()
                    .map(|root| (root.value, root.root, root.offset))
                    .collect(),
            })
            .collect(),
        values: allocated.values.iter().map(ValuePayload::of).collect(),
        stats: StatsPayload::of(&allocated),
        sites,
        bytes: assembled.bytes,
        relocations: assembled.relocations,
        fingerprint: [0; 32],
    };
    fragment.fingerprint = fragment.compute_fingerprint();
    Ok(fragment)
}

fn terminator_name(terminator: &lir::body::Terminator) -> &'static str {
    match terminator {
        lir::body::Terminator::Jump(_) => "Jump",
        lir::body::Terminator::Branch { .. } => "Branch",
        lir::body::Terminator::Switch { .. } => "Switch",
        lir::body::Terminator::Invoke { .. } => "Invoke",
        lir::body::Terminator::Return { .. } => "Return",
        lir::body::Terminator::ResumePanic { .. } => "ResumePanic",
        lir::body::Terminator::TailCall { .. } => "TailCall",
        lir::body::Terminator::Trap { .. } => "Trap",
        lir::body::Terminator::Unreachable { .. } => "Unreachable",
    }
}

/// 片段引用的内部符号名：按 mangled 文本升序去重。镜像规划按这份集合检查内部符号冲突，
/// 不必再从重定位现场二次推导符号文本。
fn reference_symbols(relocations: &[Relocation]) -> Vec<String> {
    let mut symbols: Vec<String> = relocations
        .iter()
        .filter_map(|relocation| match &relocation.target {
            RelocTarget::Lir(symbol) => Some(super::mangle::mangle_symbol(symbol)),
            RelocTarget::Cold(_) | RelocTarget::CageControl => None,
        })
        .collect();
    symbols.sort_unstable();
    symbols.dedup();
    symbols
}

/// 内部符号形状：`__gugu_<kind>_<64 lowercase hex>`；C 名不加该前缀，不参与检查。
fn internal_symbol_shape(text: &str) -> bool {
    let Some(rest) = text.strip_prefix("__gugu_") else {
        return false;
    };
    let Some((kind, hex)) = rest.rsplit_once('_') else {
        return false;
    };
    matches!(
        kind,
        "fn" | "static" | "vtable" | "glue" | "runtime" | "const" | "veneer"
    ) && hex.len() == 64
        && hex
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[expect(
    clippy::too_many_arguments,
    reason = "站点 payload 需要来源、位置、点位与已编码字节四类输入"
)]
fn site_payload(
    body: &Body,
    block: u32,
    instruction: u32,
    op: &str,
    lowered: &Lowered,
    points: Option<&alloc::SitePoints>,
    assembled: &super::inst::Assembled,
    inst_cursor: &mut usize,
) -> SitePayload {
    let code_len = to_u32(assembled.bytes.len());
    let (call_return_pc, morestack_pc) = call_pcs(lowered, assembled, *inst_cursor, code_len);
    let start_off = assembled
        .instruction_offsets
        .get(*inst_cursor)
        .map(|(_, offset)| *offset)
        .unwrap_or(code_len);
    let count = lowered.sequence.instructions.len();
    *inst_cursor = inst_cursor.saturating_add(count);
    let end_off = assembled
        .instruction_offsets
        .get(*inst_cursor)
        .map(|(_, offset)| *offset)
        .unwrap_or_else(|| to_u32(assembled.bytes.len()));
    let inst = body
        .instructions
        .get(usize::try_from(instruction).expect("指令编号适配 usize"));
    let operands = inst
        .map(|inst| ids(body.args(&inst.arguments)))
        .unwrap_or_default();
    let results = inst
        .map(|inst| result_ids(&inst.results))
        .unwrap_or_default();
    let cold_edges = lowered
        .sequence
        .instructions
        .iter()
        .flat_map(|inst| inst.operands.iter())
        .filter_map(|operand| match operand {
            Operand::Reloc(RelocTarget::Cold(edge), _) => Some(edge.clone()),
            _ => None,
        })
        .collect();
    let points = points
        .map(|points| (points.points.start, points.points.end))
        .unwrap_or((0, 0));
    SitePayload {
        block,
        instruction,
        op: op.to_owned(),
        operands,
        results,
        line: site_line(&lowered.sequence),
        bytes: (start_off, end_off),
        relocations: (0, 0),
        instructions: to_u32(count),
        constraints: Vec::new(),
        cold_edges,
        clobbers: (lowered.clobbers.gpr, lowered.clobbers.xmm),
        points,
        call_return_pc,
        morestack_pc,
    }
}

/// 站点内调用的返回 PC。`morestack_or_poll` 单独记下，供入口检查使用。
fn call_pcs(
    lowered: &Lowered,
    assembled: &super::inst::Assembled,
    inst_cursor: usize,
    code_len: u32,
) -> (Option<u32>, Option<u32>) {
    let morestack = super::mangle::runtime_symbol("morestack_or_poll");
    let mut call_return = None;
    let mut morestack_pc = None;
    for (offset, instruction) in lowered.sequence.instructions.iter().enumerate() {
        if table::form(instruction.form).mnemonic != "call" {
            continue;
        }
        let next_index = inst_cursor + offset + 1;
        let next = assembled
            .instruction_offsets
            .get(next_index)
            .map(|(_, byte)| *byte)
            .unwrap_or(code_len);
        if next >= code_len {
            continue;
        }
        call_return = Some(next);
        let targets_morestack = instruction.operands.iter().any(|operand| match operand {
            Operand::Reloc(RelocTarget::Lir(symbol), _)
            | Operand::Rip(RelocTarget::Lir(symbol), _) => symbol == &morestack,
            _ => false,
        });
        if targets_morestack {
            morestack_pc = Some(next);
        }
    }
    (call_return, morestack_pc)
}

fn fill_relocation_ranges(sites: &mut [SitePayload], relocations: &[Relocation]) {
    let mut index = 0_usize;
    for site in sites {
        let start = index;
        while index < relocations.len()
            && relocations[index].offset >= site.bytes.0
            && relocations[index].offset < site.bytes.1
        {
            index += 1;
        }
        site.relocations = (to_u32(start), to_u32(index));
    }
}

/// 校验片段与当前输入、站点结构与指纹自洽。
fn validate_fragment(
    fragment: &FragmentPayload,
    body: &Body,
    target: TargetName,
    encoder: [u8; 32],
    lir_fingerprint: [u8; 32],
) -> Result<(), Vec<Diagnostic>> {
    if fragment.schema != CODEGEN_SCHEMA
        || fragment.target != target.name()
        || fragment.instance != body.instance
        || fragment.lir_fingerprint != lir_fingerprint
        || fragment.encoder_fingerprint != encoder
    {
        return Err(vec![invalid("机器片段没有绑定当前输入、目标或编码规则")]);
    }
    if fragment.fingerprint != fragment.compute_fingerprint() {
        return Err(vec![invalid("机器片段指纹与内容不一致")]);
    }
    let expected = expected_sites(body);
    if fragment.sites.len() != expected.len() {
        return Err(vec![invalid("机器片段站点数与 LIR 不匹配")]);
    }
    let mut cursor = 0_u32;
    let mut relocation_cursor = 0_u32;
    for (site, (block, instruction)) in fragment.sites.iter().zip(expected.iter()) {
        if site.block != *block || site.instruction != *instruction {
            return Err(vec![invalid("机器片段站点顺序与 LIR 不匹配")]);
        }
        if site.bytes.0 != cursor || site.bytes.1 < site.bytes.0 {
            return Err(vec![invalid("机器片段站点字节范围不连续")]);
        }
        cursor = site.bytes.1;
        if site.relocations.0 != relocation_cursor || site.relocations.1 < site.relocations.0 {
            return Err(vec![invalid("机器片段站点重定位范围不连续")]);
        }
        relocation_cursor = site.relocations.1;
        if site
            .constraints
            .iter()
            .any(|constraint| constraint.offset < site.bytes.0 || constraint.offset >= site.bytes.1)
        {
            return Err(vec![invalid("机器片段的寄存器约束偏移越过站点范围")]);
        }
    }
    if cursor != to_u32(fragment.bytes.len())
        || relocation_cursor != to_u32(fragment.relocations.len())
    {
        return Err(vec![invalid("机器片段字节或重定位总数与站点范围不一致")]);
    }
    if !fragment
        .relocations
        .windows(2)
        .all(|pair| pair[0].offset <= pair[1].offset)
    {
        return Err(vec![invalid("机器片段重定位没有按字段偏移升序")]);
    }
    if fragment.symbols != reference_symbols(&fragment.relocations) {
        return Err(vec![invalid("机器片段符号集合与重定位不一致")]);
    }
    if fragment
        .symbols
        .iter()
        .filter(|symbol| symbol.starts_with("__gugu_"))
        .any(|symbol| !internal_symbol_shape(symbol))
    {
        return Err(vec![invalid("机器片段内部符号不是 __gugu_<kind>_<64 hex>")]);
    }
    Ok(())
}

/// 按热/冷布局顺序列出指令站点，每个 terminator 额外一条。
fn expected_sites(body: &Body) -> Vec<(u32, u32)> {
    let layout = super::layout::schedule(body);
    let mut expected = Vec::new();
    for block in &layout.blocks {
        let data = &body.blocks[block.id.index()];
        for index in data.instructions.clone() {
            expected.push((block.id.0, index));
        }
        expected.push((block.id.0, data.instructions.end));
    }
    expected
}

/// 取值编号列表。
fn ids(args: &[lir::body::ValueId]) -> Vec<u32> {
    args.iter().map(|value| value.0).collect()
}

/// 结果值编号列表。
fn result_ids(results: &Range<u32>) -> Vec<u32> {
    lir::body::range(results).map(to_u32).collect()
}

/// 稳定文本渲染：`L0:` 标签前缀、`; ` 分隔、`lock ` 前缀与操作数记忆。
fn site_line(sequence: &Sequence) -> String {
    let mut out = String::new();
    for (index, instruction) in sequence.instructions.iter().enumerate() {
        if !out.is_empty() {
            out.push_str("; ");
        }
        let position = to_u32(index);
        for label in sequence
            .labels
            .iter()
            .filter(|definition| definition.at == position)
        {
            let _ = write!(out, "L{}: ", label.label.0);
        }
        if instruction.lock {
            out.push_str("lock ");
        }
        let form = table::form(instruction.form);
        out.push_str(form.mnemonic);
        for (position, operand) in instruction.operands.iter().enumerate() {
            out.push(if position == 0 { ' ' } else { ',' });
            render_operand(&mut out, operand);
        }
    }
    out
}

/// 渲染一个操作数。
fn render_operand(out: &mut String, operand: &Operand) {
    match operand {
        Operand::Reg(Reg::Gpr(gpr)) => out.push_str(gpr.name()),
        Operand::Reg(Reg::Xmm(xmm)) => out.push_str(xmm.name()),
        Operand::Reg(Reg::Virtual(id)) => {
            let _ = write!(out, "v{id}");
        }
        Operand::Imm(value) => {
            let _ = write!(out, "0x{value:x}");
        }
        Operand::Mem(mem) => {
            out.push('[');
            let mut separator = "";
            if let Some(base) = mem.base {
                render_register(out, base);
                separator = " + ";
            }
            if let Some(index) = mem.index {
                out.push_str(separator);
                render_register(out, index);
                if mem.scale != super::inst::Scale::One {
                    let _ = write!(out, " * {}", 1_u8 << mem.scale.shift());
                }
                separator = " + ";
            }
            if mem.disp != 0 || separator.is_empty() {
                if separator.is_empty() {
                    let _ = write!(out, "{}", mem.disp);
                } else {
                    let _ = write!(out, "{:+}", mem.disp);
                }
            }
            out.push(']');
        }
        Operand::Rip(target, addend) => {
            let _ = write!(out, "[rip+");
            render_target(out, target);
            let _ = write!(out, "{addend:+}]");
        }
        Operand::Reloc(target, kind) => {
            out.push_str("reloc(");
            render_target(out, target);
            out.push(':');
            out.push_str(match kind {
                RelocKind::PcRel32 => "pc-rel32",
                RelocKind::Abs64 => "abs64",
                RelocKind::Rva32 => "rva32",
            });
            out.push(')');
        }
        Operand::Label(label) => {
            let _ = write!(out, "L{}", label.0);
        }
    }
}

fn render_register(out: &mut String, reg: Reg) {
    match reg {
        Reg::Gpr(gpr) => out.push_str(gpr.name()),
        Reg::Xmm(xmm) => out.push_str(xmm.name()),
        Reg::Virtual(id) => {
            let _ = write!(out, "v{id}");
        }
    }
}

fn render_target(out: &mut String, target: &RelocTarget) {
    match target {
        RelocTarget::CageControl => out.push_str("ctl"),
        RelocTarget::Cold(edge) => {
            out.push_str("cold:");
            out.push_str(match edge.kind {
                ColdEdgeKind::CompressionDecodeRejected => "compression-decode-rejected",
                ColdEdgeKind::Trap => "trap",
            });
        }
        RelocTarget::Lir(symbol) => render_symbol(out, symbol),
    }
}

fn render_symbol(out: &mut String, symbol: &lir::body::Symbol) {
    match symbol {
        lir::body::Symbol::Instance(key) => {
            let _ = write!(out, "instance:{}", short(key));
        }
        lir::body::Symbol::External { name, .. } => {
            let _ = write!(out, "extern:{name}");
        }
        lir::body::Symbol::Global { thread_local, .. } => {
            out.push_str(if *thread_local {
                "global:tls"
            } else {
                "global:data"
            });
        }
        lir::body::Symbol::TypeDescriptor(key) => {
            let _ = write!(out, "type-descriptor:{}", short(key));
        }
        lir::body::Symbol::TypeId(key) => {
            let _ = write!(out, "type-id:{}", short(key));
        }
        lir::body::Symbol::TypeRecords => out.push_str("type-records"),
        lir::body::Symbol::TypeNames => out.push_str("type-names"),
        lir::body::Symbol::Vtable { interface, .. } => {
            let _ = write!(out, "vtable:{}", short(interface));
        }
        lir::body::Symbol::Data(index) => {
            let _ = write!(out, "data:{index}");
        }
    }
}

/// 片段里的站点数、字节数等都需要 `u32`；超出上界说明片段规模非法。
fn to_u32(value: usize) -> u32 {
    u32::try_from(value).expect("机器片段规模适配 u32")
}

fn hex_lower(bytes: [u8; 32]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(64);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

fn short(bytes: &[u8; 32]) -> String {
    hex_lower(*bytes)[..8].to_owned()
}

fn invalid(message: &str) -> Diagnostic {
    invalid_metadata(message)
}

pub(crate) fn invalid_metadata(message: &str) -> Diagnostic {
    Diagnostic::error(DiagnosticCode::BackendInvariant, message, None)
}
