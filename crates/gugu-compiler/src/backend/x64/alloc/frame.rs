//! frame layout 与 prologue/epilogue 合成。
//!
//! # 布局顺序（从完成 prologue 后的 `rsp` 低地址向高地址）
//!
//! 1. outgoing 区：函数所有调用点所需的最大值；目标为 Windows 且函数内出现任何调用时
//!    至少 32 字节 shadow space；
//! 2. `body.stack_slots`（address-taken local 与 stack aggregate），按 `(align 降序, slot id 升序)`；
//! 3. heap-pointer spill 组；4. stack-pointer spill 组；5. non-pointer GPR 与 XMM spill 组；
//! 6. 16 字节 copy scratch（只有并行拷贝解析真的用到环打断时才预留）；
//! 7. 实际使用的 `rbp`/`r12`/`r13` save slot，按该顺序各 8 字节；8. 零 padding。
//!
//! ```text
//! payload_bytes  = 布局结束偏移（各槽按自身 align 自然对齐，最终补 0）
//! frame_size     = 无调用且 payload_bytes == 0 → 0
//!                  否则 align_up(payload_bytes + 8, 16) - 8
//! required_frame = frame_size + max_leaf_reserve
//! ```
//!
//! `frame_size % 16 == 8` 使调用点上的 `rsp` 16 字节对齐；函数入口 `rsp % 16 == 8` 加
//! `sub rsp, frame_size` 正好回到 16 对齐，callee 入口又是 8。`frame_size` 超出 i32 位移
//! 可编码范围或 `required_frame` 加法溢出都按内部不变量失败报告。
//!
//! # prologue
//!
//! ```text
//! check: lea  r11, [rsp - required_frame]      ; 位移超出 i32 时 mov r11, imm64 + add r11, rsp
//!        cmp  [r14 + stack_check_offset], r11   ; 内存在前 + signed jg：容量不足与 poison 同一次比较捕获
//!        jg   cold
//!        sub  rsp, frame_size                  ; frame_size > 0 时
//!        mov  [rsp + off], rbp/r12/r13         ; 仅实际使用的 save slot
//!        <入口参数并行拷贝组>
//!        <栈参数逐个 mov v, [rsp + frame_size + 8 + offset]>
//!        jmp  done
//! cold:  call morestack_or_poll
//!        jmp  check                            ; 恢复/扩容后重试原 StackCheck
//! done:
//! ```
//!
//! 冷路必须回到 `check` 重试：`morestack_or_poll` 可能更换栈后再恢复同一协程，候选地址与
//! `stack_low` 都已变化，不能从旧 candidate 直接建立 frame，也不能跳过 frame 建立。
//! 没有入口 `StackCheck` 站点（`PollFreeLeaf`）的函数不发射容量检查；`frame_size > 0` 时
//! 仍要发射 `sub rsp` 与保存，否则 spill slot 会落进 caller 的 frame。

use super::super::abi::{self, AbiLayout, AbiSlot};
use super::super::inst::{Operand, RelocKind, RelocTarget};
use super::super::lower::{self, Builder, gpr, imm, mem_base, reg};
use super::super::mangle;
use super::super::reg::{Gpr, Reg};
use super::super::select::SelectedFunction;
use super::super::table::{self, Access, OperandKind};
use super::parallel::{Loc, ResolvedGroup};
use super::rewrite::Emit;
use super::slots::Slots;
use super::{AllocError, Location, RootClass, ValueAllocation};
use crate::frontend::gir::body::CallKind;
use crate::lir::body::{Body, Call, CallTarget, Op, Terminator, Type};
use crate::target::TargetName;

const REL32: &[OperandKind] = &[OperandKind::Rel32];
const RM64_R64: &[OperandKind] = &[OperandKind::Rm64, OperandKind::R64];
const RM64_IMM32: &[OperandKind] = &[OperandKind::Rm64, OperandKind::Imm32];
const R64_IMM32: &[OperandKind] = &[OperandKind::R64, OperandKind::Imm32];
const R64_IMM64: &[OperandKind] = &[OperandKind::R64, OperandKind::Imm64];
const MEM_R64: &[OperandKind] = &[OperandKind::Mem, OperandKind::R64];

/// Windows x64 调用者必须预留的 shadow space。
const WIN_SHADOW_BYTES: u32 = 32;

/// 16 字节 copy scratch 的字节数。
pub(crate) const SCRATCH_BYTES: u32 = 16;

/// 栈上的一个局部槽。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct LocalSlot {
    pub(crate) offset: u32,
    pub(crate) bytes: u32,
    pub(crate) align: u32,
}

/// 一个函数的 frame 布局。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct FrameLayout {
    pub(crate) frame_size: u32,
    pub(crate) payload_bytes: u32,
    pub(crate) outgoing_bytes: u32,
    pub(crate) required_frame: u32,
    pub(crate) max_leaf_reserve: u32,
    /// 与 `body.stack_slots` 同序的局部槽偏移。
    pub(crate) locals: Vec<LocalSlot>,
    pub(crate) scratch: Option<u32>,
    /// 实际保存的 callee-saved GPR，按 `rbp,r12,r13` 顺序。
    pub(crate) saved: Vec<Gpr>,
    /// `(GPR 编码, 偏移)`，与 `saved` 同序。
    pub(crate) save_offsets: Vec<(u8, u32)>,
    /// 每个 spill slot 的字节偏移，与 [`Slots::slots`] 同序。
    pub(crate) spill_offsets: Vec<u32>,
    /// spill slot 元数据 `(offset, size, align, root class)`，与 [`Slots::slots`] 同序。
    pub(crate) spill_slots: Vec<(u32, u32, u32, u8)>,
    /// 是否发射入口容量检查（存在入口 `StackCheck` 站点）。
    pub(crate) checked: bool,
}

impl FrameLayout {
    /// 16 字节 copy scratch 的偏移。
    pub(crate) const fn scratch_offset(&self) -> Option<u32> {
        self.scratch
    }

    /// 单个 GPR 的 save 偏移。
    pub(crate) fn save_offset(&self, gpr: Gpr) -> Option<u32> {
        self.save_offsets
            .iter()
            .find(|(code, _)| *code == gpr.code())
            .map(|(_, offset)| *offset)
    }

    /// 局部槽的存放位置。
    pub(crate) fn local(&self, index: usize) -> Option<LocalSlot> {
        self.locals.get(index).copied()
    }

    /// spill slot 的字节偏移。
    pub(crate) fn spill_offset(&self, slot: u32) -> Result<u32, AllocError> {
        self.spill_offsets
            .get(slot as usize)
            .copied()
            .ok_or_else(|| AllocError::new(format!("spill slot {slot} 不在 frame 内")))
    }

    /// 是否发射栈检查。
    pub(crate) const fn checked(&self) -> bool {
        self.checked
    }
}

/// 计算 frame 布局并回填 spill slot 偏移。
pub(crate) fn layout(
    body: &Body,
    selected: &SelectedFunction,
    target: TargetName,
    values: &[ValueAllocation],
    slots: &mut Slots,
    scratch: bool,
) -> Result<FrameLayout, AllocError> {
    let calls = call_layouts(body, selected, target)?;
    let mut has_call = instruction_has_call(selected);
    let mut outgoing = 0_u32;
    for (layout, _) in &calls {
        has_call = true;
        outgoing = outgoing.max(call_stack_bytes(layout));
    }
    if target == TargetName::X86_64Windows && has_call {
        outgoing = outgoing.max(WIN_SHADOW_BYTES);
    }
    let max_leaf_reserve = max_leaf_reserve(body)?;
    let mut offset = outgoing;
    let mut locals = vec![
        LocalSlot {
            offset: 0,
            bytes: 0,
            align: 1,
        };
        body.stack_slots.len()
    ];
    let mut order: Vec<usize> = (0..body.stack_slots.len()).collect();
    order.sort_by_key(|index| {
        let slot = &body.stack_slots[*index];
        (std::cmp::Reverse(slot.align), *index)
    });
    for index in order {
        let slot = &body.stack_slots[index];
        let bytes = u32::try_from(slot.bytes).map_err(|_| AllocError::new("局部槽超过 4 GiB"))?;
        offset = align_up(offset, slot.align)?;
        locals[index] = LocalSlot {
            offset,
            bytes,
            align: slot.align,
        };
        offset = offset
            .checked_add(bytes)
            .ok_or_else(|| AllocError::new("frame 负载溢出"))?;
    }
    for root in [
        RootClass::HeapPointer,
        RootClass::StackPointer,
        RootClass::NonPointer,
    ] {
        for slot in slots.slots.iter_mut() {
            if slot.root != root {
                continue;
            }
            slot.offset = align_up(offset, slot.align)?;
            offset = slot
                .offset
                .checked_add(slot.size)
                .ok_or_else(|| AllocError::new("frame 负载溢出"))?;
        }
    }
    let scratch_offset = if scratch {
        offset = align_up(offset, 16)?;
        let at = offset;
        offset = offset
            .checked_add(SCRATCH_BYTES)
            .ok_or_else(|| AllocError::new("frame 负载溢出"))?;
        Some(at)
    } else {
        None
    };
    let saved = saved_registers(values);
    let mut save_offsets = Vec::with_capacity(saved.len());
    for gpr in &saved {
        offset = align_up(offset, 8)?;
        save_offsets.push((gpr.code(), offset));
        offset = offset
            .checked_add(8)
            .ok_or_else(|| AllocError::new("frame 负载溢出"))?;
    }
    let payload_bytes = offset;
    let frame_size = if has_call || payload_bytes != 0 {
        let padded = align_up(
            payload_bytes
                .checked_add(8)
                .ok_or_else(|| AllocError::new("frame 负载溢出"))?,
            16,
        )?;
        padded
            .checked_sub(8)
            .ok_or_else(|| AllocError::new("frame size 下溢"))?
    } else {
        0
    };
    if i32::try_from(frame_size).is_err() {
        return Err(AllocError::new("frame size 超出 i32 位移可编码范围"));
    }
    let required_frame = frame_size
        .checked_add(max_leaf_reserve)
        .ok_or_else(|| AllocError::new("required frame 溢出"))?;
    let checked = checked_entry(selected);
    let spill_offsets = slots.slots.iter().map(|slot| slot.offset).collect();
    let spill_slots = slots
        .slots
        .iter()
        .map(|slot| (slot.offset, slot.size, slot.align, slot.root.code()))
        .collect();
    Ok(FrameLayout {
        frame_size,
        payload_bytes,
        outgoing_bytes: outgoing,
        required_frame,
        max_leaf_reserve,
        locals,
        scratch: scratch_offset,
        saved,
        save_offsets,
        spill_offsets,
        spill_slots,
        checked,
    })
}

/// 实际使用的 callee-saved GPR：分配结果里出现过的 `rbp`/`r12`/`r13`，按固定顺序。
///
/// 只有这里出现的寄存器才会写 save slot，也只有它们才允许被 scratch 复用：未保存的
/// callee-saved 寄存器一旦被 scratch 写过就会破坏 caller 的现场。
pub(crate) fn saved_registers(values: &[ValueAllocation]) -> Vec<Gpr> {
    [Gpr::Rbp, Gpr::R12, Gpr::R13]
        .into_iter()
        .filter(|candidate| {
            values.iter().any(|value| {
                matches!(
                    value.placement,
                    super::Placement::Location(Location::Gpr(gpr)) if gpr == *candidate
                )
            })
        })
        .collect()
}

/// 入口是否有 `StackCheck` 站点。
fn checked_entry(selected: &SelectedFunction) -> bool {
    selected
        .blocks
        .iter()
        .flat_map(|block| block.sites.iter())
        .any(|site| site.kind == super::super::select::SiteKind::Prologue)
}

/// 序列里是否出现调用指令（含 runtime glue 与 prologue 的 morestack）。
fn instruction_has_call(selected: &SelectedFunction) -> bool {
    selected
        .blocks
        .iter()
        .flat_map(|block| {
            block
                .sites
                .iter()
                .map(|site| &site.lowered)
                .chain(std::iter::once(&block.terminator))
        })
        .any(|lowered| {
            lowered
                .sequence
                .instructions
                .iter()
                .any(|instruction| table::form(instruction.form).mnemonic == "call")
        })
}

/// 每个调用点的 ABI 布局。
fn call_layouts(
    body: &Body,
    selected: &SelectedFunction,
    target: TargetName,
) -> Result<Vec<(AbiLayout, CallKind)>, AllocError> {
    let mut layouts = Vec::new();
    for block in &selected.blocks {
        for site in &block.sites {
            let instruction = &body.instructions[site.instruction as usize];
            if let Op::Call(call) | Op::ForeignCall(call) = &instruction.op {
                layouts.push(classify(call, target)?);
            }
        }
        match &body.blocks[block.id.index()].terminator {
            Terminator::Invoke { call, .. } | Terminator::TailCall { call, .. } => {
                layouts.push(classify(call, target)?);
            }
            _ => {}
        }
    }
    Ok(layouts)
}

/// 调用分类。
fn classify(call: &Call, target: TargetName) -> Result<(AbiLayout, CallKind), AllocError> {
    let layout = abi::classify_call(call, target)
        .map_err(|error| AllocError::new(format!("调用 ABI 分类失败：{error}")))?;
    Ok((layout, call.kind))
}

/// 一个调用点需要的 outgoing 字节数（含 shadow space 基址）。
fn call_stack_bytes(layout: &AbiLayout) -> u32 {
    let mut bytes = 0_u32;
    for value in layout.arguments.iter().chain(&layout.results) {
        if let Some(AbiSlot::Stack { offset }) = value.slot {
            bytes = bytes.max(offset.saturating_add(lower::stack_piece_bytes(value.ty.ty)));
        }
    }
    bytes
}

/// 函数内 direct `ForeignLeaf { stack }` 调用的声明预算最大值；间接目标不计。
fn max_leaf_reserve(body: &Body) -> Result<u32, AllocError> {
    let mut reserve = 0_u32;
    let mut consider = |call: &Call| -> Result<(), AllocError> {
        if matches!(call.target, CallTarget::Indirect) {
            return Ok(());
        }
        if let CallKind::ForeignLeaf { stack } = call.kind {
            let stack =
                u32::try_from(stack).map_err(|_| AllocError::new("ForeignLeaf 栈预算超出 u32"))?;
            reserve = reserve.max(stack);
        }
        Ok(())
    };
    for instruction in &body.instructions {
        if let Op::Call(call) | Op::ForeignCall(call) = &instruction.op {
            consider(call)?;
        }
    }
    for block in &body.blocks {
        if let Terminator::Invoke { call, .. } | Terminator::TailCall { call, .. } =
            &block.terminator
        {
            consider(call)?;
        }
    }
    Ok(reserve)
}

/// 向上对齐；`align` 必须是 2 的幂。
pub(crate) fn align_up(value: u32, align: u32) -> Result<u32, AllocError> {
    debug_assert!(align.is_power_of_two());
    value
        .checked_add(align - 1)
        .map(|sum| sum & !(align - 1))
        .ok_or_else(|| AllocError::new("frame 对齐溢出"))
}

/// 发射 prologue：容量检查、frame 建立、callee-saved 保存、参数落位与栈参数搬运。
///
/// ```text
///         jmp   check                          ; 冷路在热路之前，入口直接跳过
/// cold:   call  morestack_or_poll
///         jmp   check                          ; 扩容/恢复后重新进入原 prologue
/// check:  lea   r11, [rsp - required_frame]    ; 位移超出 i32 时 mov r11, imm64 + add r11, rsp
///         cmp   [r14 + stack_check_offset], r11
///         jg    cold
///         <frame 建立与参数落位>                ; 热路 fall-through
/// ```
///
/// 只用站点原有的两个标签 id（`check` 与 `cold`）：`select` 按 `label_usage` 记账终结符的
/// 标签基址，多用一个标签就会破坏记账。冷路必须回到 `check` 重试：`morestack_or_poll`
/// 可能更换栈后再恢复同一协程，候选地址与 `stack_low` 都已变化，不能从旧 candidate 直接
/// 建立 frame，也不能跳过 frame 建立——runtime 恰恰是从 `call` 之后的 PC 重新进入 prologue。
pub(crate) fn emit_prologue(
    emit: &Emit<'_>,
    builder: &mut Builder,
    group: Option<&ResolvedGroup>,
    stack_arguments: &[(u32, u32, Type)],
    stack_check_offset: u32,
) -> Result<(), AllocError> {
    if !emit.frame.checked {
        return emit_frame_setup(emit, builder, group, stack_arguments);
    }
    let check = builder.label();
    let cold = builder.label();
    builder.emit("jmp", REL32, Access::Read, vec![Operand::Label(check)]);
    builder.define(cold);
    builder.emit(
        "call",
        REL32,
        Access::Read,
        vec![Operand::Reloc(
            RelocTarget::Lir(mangle::runtime_symbol("morestack_or_poll")),
            RelocKind::PcRel32,
        )],
    );
    builder.emit("jmp", REL32, Access::Read, vec![Operand::Label(check)]);
    builder.define(check);
    let required = emit.frame.required_frame;
    if let Ok(displacement) = i32::try_from(i64::from(required)) {
        builder.emit(
            "lea",
            MEM_R64,
            Access::Address,
            vec![mem_base(gpr(Gpr::Rsp), -displacement), reg(gpr(Gpr::R11))],
        );
    } else {
        emit_imm64(builder, Gpr::R11, 0_u64.wrapping_sub(u64::from(required)));
        builder.emit(
            "add",
            RM64_R64,
            Access::ReadWrite,
            vec![reg(gpr(Gpr::R11)), reg(gpr(Gpr::Rsp))],
        );
    }
    let offset = i32::try_from(stack_check_offset)
        .map_err(|_| AllocError::new("stack_check 偏移超出 i32"))?;
    builder.emit(
        "cmp",
        RM64_R64,
        Access::Read,
        vec![mem_base(gpr(Gpr::R14), offset), reg(gpr(Gpr::R11))],
    );
    builder.emit("jg", REL32, Access::Read, vec![Operand::Label(cold)]);
    emit_frame_setup(emit, builder, group, stack_arguments)
}

/// frame 建立：`sub rsp`、callee-saved 保存、参数落位与栈参数搬运。
fn emit_frame_setup(
    emit: &Emit<'_>,
    builder: &mut Builder,
    group: Option<&ResolvedGroup>,
    stack_arguments: &[(u32, u32, Type)],
) -> Result<(), AllocError> {
    let frame = emit.frame;
    if frame.frame_size > 0 {
        emit_sub_rsp(builder, frame.frame_size)?;
    }
    for (code, offset) in &frame.save_offsets {
        let gpr =
            gpr_from_code(*code).ok_or_else(|| AllocError::new("save slot 寄存器编码非法"))?;
        lower::store_stack(builder, Reg::Gpr(gpr), *offset, Type::I64)
            .map_err(|error| AllocError::new(format!("保存 callee-saved 失败：{error}")))?;
    }
    if let Some(group) = group {
        emit.emit_moves(builder, &group.moves)?;
    }
    for (offset, value, ty) in stack_arguments {
        let index = usize::try_from(*value).expect("值编号适配 usize");
        let dest = emit.value_location(index)?;
        let caller_offset = frame
            .frame_size
            .checked_add(8)
            .and_then(|base| base.checked_add(*offset))
            .ok_or_else(|| AllocError::new("栈参数偏移溢出"))?;
        emit.emit_load(builder, caller_offset, dest, *ty)?;
    }
    Ok(())
}

/// 发射 epilogue：恢复 callee-saved 再释放 frame。
pub(crate) fn emit_epilogue(builder: &mut Builder, frame: &FrameLayout) -> Result<(), AllocError> {
    for (code, offset) in &frame.save_offsets {
        let gpr =
            gpr_from_code(*code).ok_or_else(|| AllocError::new("save slot 寄存器编码非法"))?;
        lower::load_stack(builder, *offset, Reg::Gpr(gpr), Type::I64)
            .map_err(|error| AllocError::new(format!("恢复 callee-saved 失败：{error}")))?;
    }
    if frame.frame_size > 0 {
        emit_add_rsp(builder, frame.frame_size)?;
    }
    Ok(())
}

/// `sub rsp, imm`；`add r/m64, imm32` 的位移形式。
fn emit_sub_rsp(builder: &mut Builder, bytes: u32) -> Result<(), AllocError> {
    let imm32 = i32::try_from(bytes)
        .map(i64::from)
        .map_err(|_| AllocError::new("frame size 超出 imm32"))?;
    builder.emit(
        "sub",
        RM64_IMM32,
        Access::ReadWrite,
        vec![reg(gpr(Gpr::Rsp)), imm(imm32 as u64)],
    );
    Ok(())
}

/// `add rsp, imm`；超出 imm32 时经 `r11` 物化。
fn emit_add_rsp(builder: &mut Builder, bytes: u32) -> Result<(), AllocError> {
    if let Ok(imm32) = i32::try_from(bytes) {
        builder.emit(
            "add",
            RM64_IMM32,
            Access::ReadWrite,
            vec![reg(gpr(Gpr::Rsp)), imm(imm32 as u64)],
        );
        return Ok(());
    }
    emit_imm64(builder, Gpr::R11, u64::from(bytes));
    builder.emit(
        "add",
        RM64_R64,
        Access::ReadWrite,
        vec![reg(gpr(Gpr::Rsp)), reg(gpr(Gpr::R11))],
    );
    Ok(())
}

/// `mov r64, imm64`（或 32 位形式）。
pub(crate) fn emit_imm64(builder: &mut Builder, dest: Gpr, value: u64) {
    if value <= u64::from(u32::MAX) {
        builder.emit(
            "mov",
            R64_IMM32,
            Access::Write,
            vec![reg(gpr(dest)), imm(value)],
        );
    } else {
        builder.emit(
            "mov",
            R64_IMM64,
            Access::Write,
            vec![reg(gpr(dest)), imm(value)],
        );
    }
}

/// GPR 编码 → 寄存器。
fn gpr_from_code(code: u8) -> Option<Gpr> {
    [
        Gpr::Rax,
        Gpr::Rcx,
        Gpr::Rdx,
        Gpr::Rbx,
        Gpr::Rsp,
        Gpr::Rbp,
        Gpr::Rsi,
        Gpr::Rdi,
        Gpr::R8,
        Gpr::R9,
        Gpr::R10,
        Gpr::R11,
        Gpr::R12,
        Gpr::R13,
        Gpr::R14,
        Gpr::R15,
    ]
    .into_iter()
    .find(|gpr| gpr.code() == code)
}

/// 分配位置 → `Loc`。
pub(crate) const fn loc_of(location: Location) -> Loc {
    match location {
        Location::Gpr(gpr) => Loc::Gpr(gpr),
        Location::Xmm(xmm) => Loc::Xmm(xmm),
        Location::Slot(slot) => Loc::Slot(slot),
    }
}
