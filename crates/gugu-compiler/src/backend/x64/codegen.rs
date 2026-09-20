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
use crate::frontend::mono::keys::hash_domain;
use crate::frontend::semantics::query::{restore_errors, store_errors};
use crate::lir;
use crate::lir::body::Body;
use crate::query::{QueryEngine, QueryKey, QueryKind};
use crate::target::TargetName;

use super::contract::EncoderContract;
use super::encode::assemble;
use super::inst::{
    ColdEdge, ColdEdgeKind, Operand, RegisterConstraint, RelocKind, RelocTarget, Relocation,
    Sequence,
};
use super::lower::{self, SiteValue};
use super::reg::Reg;
use super::table;
use super::verify;

/// 片段 payload schema 版本。
pub(crate) const CODEGEN_SCHEMA: u32 = 1;

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
    /// 站点，按 `(block, instruction)` 升序。
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

/// 全部实例的机器码片段世界。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct X64World {
    pub(crate) schema: u32,
    pub(crate) target: TargetName,
    pub(crate) contract: EncoderContract,
    /// 片段，按实例稳定键升序（LIR world 已强制该顺序）。
    pub(crate) fragments: Vec<FragmentPayload>,
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

    /// 内存里的机器片段数。
    pub(crate) fn fragment_count(&self) -> u32 {
        to_u32(self.fragments.len())
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
        out.push_str(&self.contract.dump());
        for fragment in &self.fragments {
            let _ = writeln!(
                out,
                "x64-fragment {} bytes={} sites={} relocations={} fingerprint={}",
                hex_lower(fragment.instance),
                fragment.bytes.len(),
                fragment.sites.len(),
                fragment.relocations.len(),
                hex_lower(fragment.fingerprint)
            );
            for site in &fragment.sites {
                let _ = writeln!(
                    out,
                    "x64-site {} {}.{} {} [{}] bytes={}..{} relocations={}..{}",
                    hex_lower(fragment.instance),
                    site.block,
                    site.instruction,
                    site.op,
                    site.line,
                    site.bytes.0,
                    site.bytes.1,
                    site.relocations.0,
                    site.relocations.1
                );
            }
        }
        out
    }
}

/// 构建全部实例的机器码片段世界。
///
/// 失败进入 `E0060`；无入口的程序不由本函数处理（调用方在 BuildIr 之后判断）。
pub(crate) fn build(
    lir: &lir::Validated,
    target: TargetName,
    queries: &QueryEngine,
    sources: &SourceMap,
) -> Result<X64World, Vec<Diagnostic>> {
    let contract = EncoderContract::build(target.descriptor().cpu_baseline);
    contract
        .verify()
        .map_err(|error| vec![invalid(error.message())])?;
    let encoder = contract.fingerprint();
    let lir_fingerprint = lir.fingerprint();
    let mut fragments = Vec::with_capacity(lir.bodies().len());
    for body in lir.bodies() {
        let key = QueryKey::new(
            QueryKind::CodegenFragment,
            CODEGEN_SCHEMA,
            fragment_key(lir_fingerprint, body, target, encoder),
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
            let fragment = assemble_fragment(body, target, encoder, lir_fingerprint, &contract)
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
        fragments.push(fragment);
    }
    let mut world = X64World {
        schema: CODEGEN_SCHEMA,
        target,
        contract,
        fragments,
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
) -> [u8; 32] {
    let mut canonical = Vec::with_capacity(32 * 4 + 16);
    canonical.extend_from_slice(&lir_fingerprint);
    canonical.extend_from_slice(&body.instance);
    canonical.extend_from_slice(&body.fingerprint());
    canonical.extend_from_slice(target.name().as_bytes());
    canonical.extend_from_slice(&encoder);
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
    *hasher.finalize().as_bytes()
}

/// 按 `(block, instruction)` 顺序 lower 出片段。
fn assemble_fragment(
    body: &Body,
    target: TargetName,
    encoder: [u8; 32],
    lir_fingerprint: [u8; 32],
    contract: &EncoderContract,
) -> Result<FragmentPayload, Vec<Diagnostic>> {
    let mut bytes = Vec::new();
    let mut relocations: Vec<Relocation> = Vec::new();
    let mut sites = Vec::new();
    for (block, block_data) in body.blocks.iter().enumerate() {
        for index in block_data.instructions.clone() {
            let instruction =
                &body.instructions[usize::try_from(index).expect("指令编号适配 usize")];
            let Some(op) = lower::domain(&instruction.op) else {
                continue;
            };
            let operands = values(body, body.args(&instruction.arguments));
            let results = result_values(body, &instruction.results);
            let lowered = lower::lower(&instruction.op, &operands, &results, &instruction.source)
                .map_err(|error| {
                vec![invalid(&format!(
                    "{} 的 {op} 没有机器序列：{error}",
                    body.name
                ))]
            })?;
            verify::verify_sequence(&lowered.sequence, contract.baseline).map_err(|error| {
                vec![invalid(&format!("{} 的 {op} 序列非法：{error}", body.name))]
            })?;
            let assembled = assemble(&lowered.sequence).map_err(|error| {
                vec![invalid(&format!(
                    "{} 的 {op} 片段编码失败：{error}",
                    body.name
                ))]
            })?;
            let start = to_u32(bytes.len());
            let relocation_start = to_u32(relocations.len());
            let line = site_line(&lowered.sequence);
            bytes.extend_from_slice(&assembled.bytes);
            for relocation in &assembled.relocations {
                relocations.push(Relocation {
                    offset: relocation
                        .offset
                        .checked_add(start)
                        .expect("重定位偏移适配 u32"),
                    ..relocation.clone()
                });
            }
            let constraints = assembled
                .constraints
                .iter()
                .map(|constraint| RegisterConstraint {
                    offset: constraint
                        .offset
                        .checked_add(start)
                        .expect("约束偏移适配 u32"),
                    ..*constraint
                })
                .collect();
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
            sites.push(SitePayload {
                block: to_u32(block),
                instruction: index,
                op: op.to_owned(),
                operands: ids(body.args(&instruction.arguments)),
                results: result_ids(&instruction.results),
                line,
                bytes: (start, to_u32(bytes.len())),
                relocations: (relocation_start, to_u32(relocations.len())),
                instructions: to_u32(assembled.instruction_offsets.len()),
                constraints,
                cold_edges,
                clobbers: (lowered.clobbers.gpr, lowered.clobbers.xmm),
            });
        }
    }
    let mut fragment = FragmentPayload {
        schema: CODEGEN_SCHEMA,
        target: target.name().to_owned(),
        instance: body.instance,
        lir_fingerprint,
        encoder_fingerprint: encoder,
        sites,
        bytes,
        relocations,
        fingerprint: [0; 32],
    };
    fragment.fingerprint = fragment.compute_fingerprint();
    Ok(fragment)
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
    Ok(())
}

/// 按 `(block, instruction)` 顺序列出需要在片段里出现的指令。
fn expected_sites(body: &Body) -> Vec<(u32, u32)> {
    let mut expected = Vec::new();
    for (block, block_data) in body.blocks.iter().enumerate() {
        for index in block_data.instructions.clone() {
            let instruction =
                &body.instructions[usize::try_from(index).expect("指令编号适配 usize")];
            if lower::domain(&instruction.op).is_some() {
                expected.push((to_u32(block), index));
            }
        }
    }
    expected
}

/// 取值的站点表示：虚拟寄存器编号 = 值编号。
fn values(body: &Body, args: &[lir::body::ValueId]) -> Vec<SiteValue> {
    args.iter()
        .map(|value| SiteValue {
            ty: body.values[value.index()].kind,
            reg: Reg::Virtual(value.0),
        })
        .collect()
}

/// 结果的站点表示：`Instruction::results` 是 `body.values` 的下标范围。
fn result_values(body: &Body, results: &Range<u32>) -> Vec<SiteValue> {
    lir::body::range(results)
        .map(|index| SiteValue {
            ty: body.values[index].kind,
            reg: Reg::Virtual(to_u32(index)),
        })
        .collect()
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
    Diagnostic::error(DiagnosticCode::BackendInvariant, message, None)
}
