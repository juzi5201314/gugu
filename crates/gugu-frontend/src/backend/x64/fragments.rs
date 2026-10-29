//! 登记机器片段：用内部 encoder 直接编码 x86_64，不调用系统 assembler/linker。
//!
//! 三类片段：
//! - `morestack_or_poll`：prologue 冷路径入口胶合。保存 return PC、九个整数参数寄存器与八个
//!   浮点参数寄存器到当前协程的 `CoroutineCold.morestack_scratch`，把 `hot/entry_rsp/candidate/
//!   required` 写入 runtime mailbox，再经 `context_switch` 保存协程 context、恢复系统 context；
//!   系统侧（rt0）完成 poll/增长后重置 `cold.context` 并经 `context_restore` 回到本片段的续点，
//!   续点装载修正后的 scratch 并 `ret` 回 prologue 的 `jmp check`。
//! - syscall/薄导入封装：`process_exit`、`report_write` 与栈 arena 使用的四个平台原语。
//! - context 片段：`context_switch`/`context_restore` 复用 `ContextSwitchCode` 字节串。
//!
//! 片段内 `rel32`/`disp32` 修补点以 [`Relocation`] 形式登记（偏移相对片段起点）：目标 glue
//! 名经 `mangle_runtime`/`mangle_symbol` 生成符号名，由镜像 writer 按绑定表与目录解析。
//! morestack 与 main 返回片段同时进入 `CoroutineRuntimeContract` schema 2：driver 注入本模块
//! 编码的字节，[`verify_coroutine_code`] 在消费契约前逐字节比对。

use crate::backend::x64::encode;
use crate::backend::x64::inst::{Operand, RelocKind, RelocTarget, Relocation};
use crate::backend::x64::lower::{Builder, gpr, imm, mem_base, reg, xmm};
use crate::backend::x64::mangle;
use crate::backend::x64::reg::Gpr;
use crate::backend::x64::table::{Access, OperandKind};
use crate::hash::hash_domain;
use crate::lir::body::Symbol;
use crate::runtime::PlatformOp;
use crate::runtime::coroutine::{CoroutineCold, MorestackMailbox, MorestackScratch};
use crate::runtime::{
    COROUTINE_PACK_FLOAT_LANES, ContextSwitchCode, CoroutineRuntimeContract, FragmentPatch,
    MainReturnCode, MorestackEntryCode, RawModelError, coroutine_pack_float_offset,
    coroutine_pack_integer_offset, processor,
};
use crate::target::TargetName;
use std::mem::offset_of;

/// 片段放置对齐字节数。
pub(crate) const FRAGMENT_ALIGNMENT: u64 = 16;

const REL32: &[OperandKind] = &[OperandKind::Rel32];
const RM64: &[OperandKind] = &[OperandKind::Rm64];
const MEM_R64: &[OperandKind] = &[OperandKind::Mem, OperandKind::R64];
const RM64_R64: &[OperandKind] = &[OperandKind::Rm64, OperandKind::R64];
const RM64_IMM32: &[OperandKind] = &[OperandKind::Rm64, OperandKind::Imm32];
const R64_IMM32: &[OperandKind] = &[OperandKind::R64, OperandKind::Imm32];
const R32_IMM32: &[OperandKind] = &[OperandKind::R32, OperandKind::Imm32];
const RM32_IMM8: &[OperandKind] = &[OperandKind::Rm32, OperandKind::Imm8];
const RM32_R32: &[OperandKind] = &[OperandKind::Rm32, OperandKind::R32];
const XMMRM_XMM: &[OperandKind] = &[OperandKind::XmmRm, OperandKind::Xmm];
/// 调度返回原因，写在 runtime 状态区邮箱之后的空闲字。0 是 morestack，1 是主动让出，2 是协程返回。
///
/// 偏移来自 `entry::state` 的唯一契约，片段与镜像 runtime 共用同一份；这里只做窄化。
const SCHEDULE_REASON: i32 = crate::runtime::entry::state::REASON as i32;

/// 片段内的一个入口符号。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct FragmentEntry {
    /// glue 名（`mangle_runtime` 的输入）。
    pub(crate) glue: &'static str,
    /// 入口在片段字节内的偏移。
    pub(crate) offset: u32,
}

/// 片段入口符号所在的 mangling 域。
///
/// runtime 域（`mangle_runtime`）用于 `std.runtime.*` 机器原语与 rt0 侧 runtime 片段；glue 域
/// （`mangle_glue`）用于 lowering 直接引用的编译器生成体（内存操作等），调用点写在 LIR 里。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FragmentDomain {
    /// `mangle_runtime`：源级机器原语与 runtime 片段。
    Runtime,
    /// `mangle_glue`：编译器生成体，调用点由 lowering 发出。
    Glue,
}

impl FragmentDomain {
    /// 该域下 glue 名对应的镜像符号名。
    pub(crate) fn mangle(self, glue: &str) -> String {
        match self {
            Self::Runtime => mangle::mangle_runtime(glue),
            Self::Glue => mangle::mangle_glue(glue),
        }
    }
}

/// 一个已登记机器片段的放置描述。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct FragmentBlob {
    /// 放置对齐字节数。
    pub(crate) alignment: u64,
    /// 入口符号的 mangling 域。
    pub(crate) domain: FragmentDomain,
    /// 可直接放入只执行代码区的 x86_64 指令。
    pub(crate) bytes: Vec<u8>,
    /// 入口符号与偏移，按偏移升序。
    pub(crate) entries: Vec<FragmentEntry>,
    /// 片段内的 PC 相对修补点，偏移相对片段起点升序。
    pub(crate) relocations: Vec<Relocation>,
}

/// morestack 邮箱字段偏移的唯一来源是 `MorestackMailbox` 结构；片段是这些偏移的机器级消费者。

/// 编码器输出的通用片段：固定字节 + 修补点。
#[derive(Clone, Debug, Eq, PartialEq)]
struct Encoded {
    bytes: Vec<u8>,
    relocations: Vec<Relocation>,
}

/// PE 导入的 IAT 槽符号；`pe.rs` 按同一名字前缀解析。
fn iat_symbol(name: &str) -> Symbol {
    Symbol::External {
        key: hash_domain("gugu-pe-iat-symbol-v1", name.as_bytes()),
        name: format!("__gugu_iat_{name}"),
    }
}

/// mysql glue 符号：`runtime_named(name)` 的目标。
fn glue_symbol(glue: &str) -> Symbol {
    mangle::runtime_symbol(glue)
}

/// 汇编一个 builder：字节与 PC 相对修补点（偏移相对片段起点）。
fn finish(builder: Builder) -> Encoded {
    let assembled = encode::assemble(&builder.finish().sequence).expect("登记片段编码");
    Encoded {
        bytes: assembled.bytes,
        relocations: assembled.relocations,
    }
}

/// 固定内部边界：入口 `rsp` 指向 return PC，`r14 = CoroutineSlot*`，`r15 = LogicalProcessor*`，
/// `r11` 是候选低水位。除 `r11`/`rsp` 外不写入口寄存器，也不读写候选以下的用户栈。
pub(crate) fn morestack_entry_code() -> MorestackEntryCode {
    let scratch = field_offset(offset_of!(CoroutineCold, morestack_scratch));
    let return_pc = scratch + field_offset(offset_of!(MorestackScratch, return_pc));
    let gprs = scratch + field_offset(offset_of!(MorestackScratch, gpr));
    let xmms = scratch + field_offset(offset_of!(MorestackScratch, xmm));
    let context = field_offset(offset_of!(CoroutineCold, context));
    let mailbox_hot = field_offset(offset_of!(MorestackMailbox, hot));
    let mailbox_entry_rsp = field_offset(offset_of!(MorestackMailbox, entry_rsp));
    let mailbox_candidate = field_offset(offset_of!(MorestackMailbox, candidate));
    let mailbox_required = field_offset(offset_of!(MorestackMailbox, required));
    let mailbox_cold = field_offset(offset_of!(MorestackMailbox, cold));
    let mailbox_system_context = field_offset(offset_of!(MorestackMailbox, system_context));
    let mut builder = Builder::new();
    let mut planned: Vec<(&'static str, i32)> = Vec::new();
    // 入口登记与 required：candidate 在 r11，算完即释放 r11。
    builder.emit(
        "mov",
        RM64_R64,
        Access::Write,
        vec![
            mailbox_operand(&mut planned, mailbox_hot),
            reg(gpr(Gpr::R14)),
        ],
    );
    builder.emit(
        "mov",
        RM64_R64,
        Access::Write,
        vec![
            mailbox_operand(&mut planned, mailbox_entry_rsp),
            reg(gpr(Gpr::Rsp)),
        ],
    );
    builder.emit(
        "mov",
        RM64_R64,
        Access::Write,
        vec![
            mailbox_operand(&mut planned, mailbox_candidate),
            reg(gpr(Gpr::R11)),
        ],
    );
    // required = entry_rsp + 8 - candidate：candidate 已入 mailbox，r11 复用为计算寄存器。
    builder.emit(
        "sub",
        RM64_R64,
        Access::ReadWrite,
        vec![reg(gpr(Gpr::R11)), reg(gpr(Gpr::Rsp))],
    );
    builder.emit("neg", RM64, Access::ReadWrite, vec![reg(gpr(Gpr::R11))]);
    builder.emit(
        "add",
        RM64_IMM32,
        Access::ReadWrite,
        vec![reg(gpr(Gpr::R11)), imm(8)],
    );
    builder.emit(
        "mov",
        RM64_R64,
        Access::Write,
        vec![
            mailbox_operand(&mut planned, mailbox_required),
            reg(gpr(Gpr::R11)),
        ],
    );
    builder.emit(
        "mov",
        RM64_R64,
        Access::Read,
        vec![
            mailbox_operand(&mut planned, mailbox_cold),
            reg(gpr(Gpr::R11)),
        ],
    );
    save_parameter_registers(&mut builder, reg(gpr(Gpr::R11)), gprs, xmms);
    // return PC 与 hot：rax 的入口值已入 scratch，可自由中转 `[rsp]`。
    builder.emit(
        "mov",
        RM64_R64,
        Access::Read,
        vec![mem_base(gpr(Gpr::Rsp), 0), reg(gpr(Gpr::Rax))],
    );
    builder.emit(
        "mov",
        RM64_R64,
        Access::Write,
        vec![mem_base(gpr(Gpr::R11), return_pc), reg(gpr(Gpr::Rax))],
    );
    // context_switch 的内部 ABI：rax=save、rbx=restore、rcx=hot、rdx=processor。
    // 入口 rbx 已在 scratch 中保存，此处可安全复用。
    builder.emit(
        "lea",
        MEM_R64,
        Access::Address,
        vec![mem_base(gpr(Gpr::R11), context), reg(gpr(Gpr::Rax))],
    );
    builder.emit(
        "lea",
        MEM_R64,
        Access::Address,
        vec![
            mailbox_operand(&mut planned, mailbox_system_context),
            reg(gpr(Gpr::Rbx)),
        ],
    );
    builder.emit(
        "mov",
        RM64_R64,
        Access::Write,
        vec![reg(gpr(Gpr::Rcx)), reg(gpr(Gpr::R14))],
    );
    builder.emit(
        "mov",
        RM64_R64,
        Access::Write,
        vec![reg(gpr(Gpr::Rdx)), reg(gpr(Gpr::R15))],
    );
    planned.push(("context_switch", 0));
    builder.emit(
        "call",
        REL32,
        Access::Read,
        vec![Operand::Reloc(
            RelocTarget::Lir(glue_symbol("context_switch")),
            RelocKind::PcRel32,
        )],
    );
    let resume_index = builder.instruction_count();
    // 续点：系统侧恢复 context 后落回这里；装载修正后的 scratch 并 `ret` 回 prologue。
    builder.emit(
        "mov",
        RM64_R64,
        Access::Read,
        vec![
            mailbox_operand(&mut planned, mailbox_cold),
            reg(gpr(Gpr::R11)),
        ],
    );
    restore_parameter_registers(&mut builder, reg(gpr(Gpr::R11)), gprs, xmms);
    builder.emit("ret", &[], Access::Read, Vec::new());
    let lowered = builder.finish();
    let assembled = encode::assemble(&lowered.sequence).expect("morestack 片段编码");
    let resume_offset = assembled
        .instruction_offsets
        .iter()
        .find_map(|(index, offset)| (*index == resume_index).then_some(*offset))
        .expect("续点指令存在偏移");
    MorestackEntryCode {
        revision: 1,
        resume_offset,
        bytes: assembled.bytes,
        patches: patch_list(&assembled.relocations, &planned),
    }
}

/// 取 mailbox 字段的 RIP 相对操作数，并登记修补点；每个带修补点的发射恰有一条后端重定位。
fn mailbox_operand(planned: &mut Vec<(&'static str, i32)>, field: i32) -> Operand {
    planned.push(("morestack_mailbox", field));
    Operand::Rip(RelocTarget::Lir(glue_symbol("morestack_mailbox")), field)
}

/// 把后端重定位按发射顺序翻译为公开修补点视图；每个带修补点的发射恰有一条重定位。
fn patch_list(relocations: &[Relocation], planned: &[(&'static str, i32)]) -> Vec<FragmentPatch> {
    debug_assert_eq!(
        relocations.len(),
        planned.len(),
        "带修补点的发射与后端重定位一一对应"
    );
    relocations
        .iter()
        .zip(planned)
        .map(|(relocation, (glue, addend))| FragmentPatch {
            field: relocation.offset,
            glue: (*glue).to_string(),
            addend: *addend,
        })
        .collect()
}

/// 九个整数参数寄存器与八个浮点参数寄存器写入 scratch。
fn save_parameter_registers(builder: &mut Builder, base: Operand, gprs: i32, xmms: i32) {
    for (index, register) in crate::backend::x64::abi::INTERNAL_INTEGER
        .iter()
        .enumerate()
    {
        let slot = gprs + 8 * i32::try_from(index).expect("参数槽下标适配 i32");
        builder.emit(
            "mov",
            RM64_R64,
            Access::Write,
            vec![add_base(&base, slot), reg(gpr(*register))],
        );
    }
    for (index, register) in crate::backend::x64::abi::INTERNAL_FLOAT.iter().enumerate() {
        let slot = xmms + 16 * i32::try_from(index).expect("浮点槽下标适配 i32");
        builder.emit(
            "movups",
            XMMRM_XMM,
            Access::Write,
            vec![add_base(&base, slot), xmm(*register)],
        );
    }
}

/// 从 scratch 装载九个整数参数寄存器与八个浮点参数寄存器。
fn restore_parameter_registers(builder: &mut Builder, base: Operand, gprs: i32, xmms: i32) {
    for (index, register) in crate::backend::x64::abi::INTERNAL_INTEGER
        .iter()
        .enumerate()
    {
        let slot = gprs + 8 * i32::try_from(index).expect("参数槽下标适配 i32");
        builder.emit(
            "mov",
            RM64_R64,
            Access::Read,
            vec![add_base(&base, slot), reg(gpr(*register))],
        );
    }
    for (index, register) in crate::backend::x64::abi::INTERNAL_FLOAT.iter().enumerate() {
        let slot = xmms + 16 * i32::try_from(index).expect("浮点槽下标适配 i32");
        builder.emit(
            "movups",
            XMMRM_XMM,
            Access::Read,
            vec![add_base(&base, slot), xmm(*register)],
        );
    }
}

/// 片段 scratch 基址从寄存器构造 `[base + disp]`。
fn add_base(base: &Operand, disp: i32) -> Operand {
    let Operand::Reg(register) = base else {
        unreachable!("片段 scratch 基址必须是寄存器");
    };
    mem_base(*register, disp)
}

/// 布局字段偏移按 i32 位移编码；偏移超出 i32 说明契约布局被破坏。
fn field_offset(offset: usize) -> i32 {
    i32::try_from(offset).expect("片段字段偏移适配 i32")
}

/// `process_exit(code)` 片段：终止当前进程，不返回。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ProcessExitCode {
    /// 目标平台内部边界的版本。
    pub(crate) revision: u32,
    /// 指令字节。
    pub(crate) bytes: Vec<u8>,
    /// 修补点。
    pub(crate) relocations: Vec<Relocation>,
}

impl ProcessExitCode {
    /// Linux 走 `exit_group(2)` 的 RT 等价 `exit(60)`；Windows 走 `ExitProcess`。
    pub(crate) fn fixed(target: TargetName) -> Self {
        let mut builder = Builder::new();
        match target {
            TargetName::X86_64Linux => {
                // 内部 ABI 的第一个整数实参在 rax，退出码由 LIR 放在那里。
                builder.emit(
                    "mov",
                    RM32_R32,
                    Access::Write,
                    vec![reg(gpr(Gpr::Rdi)), reg(gpr(Gpr::Rax))],
                );
                builder.emit(
                    "mov",
                    R32_IMM32,
                    Access::Write,
                    vec![reg(gpr(Gpr::Rax)), imm(60)],
                );
                builder.emit("syscall", &[], Access::Read, Vec::new());
            }
            TargetName::X86_64Windows => {
                builder.emit(
                    "sub",
                    RM64_IMM32,
                    Access::ReadWrite,
                    vec![reg(gpr(Gpr::Rsp)), imm(0x28)],
                );
                builder.emit(
                    "mov",
                    RM32_R32,
                    Access::Write,
                    vec![reg(gpr(Gpr::Rcx)), reg(gpr(Gpr::Rax))],
                );
                builder.emit(
                    "call",
                    &[OperandKind::Mem],
                    Access::Read,
                    vec![Operand::Rip(RelocTarget::Lir(iat_symbol("ExitProcess")), 0)],
                );
            }
        }
        let encoded = finish(builder);
        Self {
            revision: 1,
            bytes: encoded.bytes,
            relocations: encoded.relocations,
        }
    }
}

/// 用户 main 返回后记录退出值、清除当前协程，再以内部 ABI
/// （rax=system context、rbx=hot、rcx=processor）跳入独立 restore 入口。
pub(crate) fn main_return_code() -> MainReturnCode {
    let return_value = field_offset(offset_of!(MorestackMailbox, return_value));
    let system_context = field_offset(offset_of!(MorestackMailbox, system_context));
    let hot = field_offset(offset_of!(MorestackMailbox, hot));
    let processor = field_offset(offset_of!(MorestackMailbox, processor));
    let mut builder = Builder::new();
    let mut planned: Vec<(&'static str, i32)> = Vec::new();
    builder.emit(
        "mov",
        RM64_R64,
        Access::Write,
        vec![
            mailbox_operand(&mut planned, return_value),
            reg(gpr(Gpr::Rax)),
        ],
    );
    builder.emit(
        "lea",
        MEM_R64,
        Access::Address,
        vec![
            mailbox_operand(&mut planned, system_context),
            reg(gpr(Gpr::Rax)),
        ],
    );
    builder.emit(
        "mov",
        RM64_R64,
        Access::Read,
        vec![mailbox_operand(&mut planned, hot), reg(gpr(Gpr::Rbx))],
    );
    builder.emit(
        "mov",
        RM64_R64,
        Access::Read,
        vec![mailbox_operand(&mut planned, processor), reg(gpr(Gpr::Rcx))],
    );
    builder.emit(
        "mov",
        R32_IMM32,
        Access::Write,
        vec![reg(gpr(Gpr::Rdx)), imm(2)],
    );
    builder.emit(
        "mov",
        RM64_R64,
        Access::Write,
        vec![
            mailbox_operand(&mut planned, SCHEDULE_REASON),
            reg(gpr(Gpr::Rdx)),
        ],
    );
    planned.push(("context_restore", 0));
    builder.emit(
        "jmp",
        REL32,
        Access::Read,
        vec![Operand::Reloc(
            RelocTarget::Lir(glue_symbol("context_restore")),
            RelocKind::PcRel32,
        )],
    );
    let encoded = finish(builder);
    MainReturnCode {
        revision: 2,
        patches: patch_list(&encoded.relocations, &planned),
        bytes: encoded.bytes,
    }
}

/// 契约携带的 morestack 与 main 返回片段必须与本编码器的产物逐字节一致。
pub(crate) fn verify_coroutine_code(
    contract: &CoroutineRuntimeContract,
) -> Result<(), RawModelError> {
    if contract.morestack != morestack_entry_code() || contract.main_return != main_return_code() {
        return Err(RawModelError::new(
            "协程契约的 morestack 或 main 返回片段与 backend 编码器产物不一致",
        ));
    }
    Ok(())
}

/// 由公开修补点视图生成片段重定位。
fn relocations_of(patches: &[FragmentPatch]) -> Vec<Relocation> {
    patches
        .iter()
        .map(|patch| Relocation {
            offset: patch.field,
            kind: RelocKind::PcRel32,
            target: RelocTarget::Lir(glue_symbol(&patch.glue)),
            addend: i64::from(patch.addend),
        })
        .collect()
}

/// `report_write(fd, data, len) -> uint` 片段：阻塞写出报告字节。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ReportWriteCode {
    /// 目标平台内部边界的版本。
    pub(crate) revision: u32,
    /// 指令字节。
    pub(crate) bytes: Vec<u8>,
    /// 修补点。
    pub(crate) relocations: Vec<Relocation>,
}

impl ReportWriteCode {
    /// 入口即 LIR 调用约定：`rdi = fd`、`rsi = data`、`rdx = len`；返回 `rax = 写出字节数`。
    ///
    /// 报告长度远小于一次 `write(2)` 的原子上限，且 stderr 是阻塞 fd，因此单次 syscall 就是
    /// 「阻塞写出」的完整实现；部分写与 EINTR 的处理留给报告循环（多段写出时逐段调用）。
    pub(crate) fn fixed(target: TargetName) -> Self {
        let mut builder = Builder::new();
        match target {
            TargetName::X86_64Linux => {
                // 内部 ABI：rax = fd、rbx = data、rcx = len；搬到 syscall 的 rdi/rsi/rdx。
                builder.emit(
                    "mov",
                    RM64_R64,
                    Access::Write,
                    vec![reg(gpr(Gpr::Rdi)), reg(gpr(Gpr::Rax))],
                );
                builder.emit(
                    "mov",
                    RM64_R64,
                    Access::Write,
                    vec![reg(gpr(Gpr::Rsi)), reg(gpr(Gpr::Rbx))],
                );
                builder.emit(
                    "mov",
                    RM64_R64,
                    Access::Write,
                    vec![reg(gpr(Gpr::Rdx)), reg(gpr(Gpr::Rcx))],
                );
                builder.emit(
                    "mov",
                    R32_IMM32,
                    Access::Write,
                    vec![reg(gpr(Gpr::Rax)), imm(1)],
                );
                builder.emit("syscall", &[], Access::Read, Vec::new());
                builder.emit("ret", &[], Access::Read, Vec::new());
            }
            TargetName::X86_64Windows => {
                // 0x38 = 0x20 shadow + overlapped 槽 + written 槽 + 对齐。
                builder.emit(
                    "sub",
                    RM64_IMM32,
                    Access::ReadWrite,
                    vec![reg(gpr(Gpr::Rsp)), imm(0x38)],
                );
                builder.emit(
                    "mov",
                    RM64_R64,
                    Access::Write,
                    vec![reg(gpr(Gpr::R11)), reg(gpr(Gpr::Rcx))],
                );
                builder.emit(
                    "mov",
                    RM64_IMM32,
                    Access::Write,
                    vec![mem_base(gpr(Gpr::Rsp), 0x20), imm(0)],
                );
                builder.emit(
                    "mov",
                    RM64_IMM32,
                    Access::Write,
                    vec![mem_base(gpr(Gpr::Rsp), 0x30), imm(0)],
                );
                let stdout = builder.label();
                let resolve = builder.label();
                builder.emit(
                    "cmp",
                    RM64_IMM32,
                    Access::Read,
                    vec![reg(gpr(Gpr::Rax)), imm(2)],
                );
                builder.emit(
                    "jne",
                    &[OperandKind::Rel32],
                    Access::Read,
                    vec![Operand::Label(stdout)],
                );
                builder.emit(
                    "mov",
                    R32_IMM32,
                    Access::Write,
                    vec![reg(gpr(Gpr::Rcx)), imm(0xFFFF_FFF4)],
                );
                builder.emit(
                    "jmp",
                    &[OperandKind::Rel32],
                    Access::Read,
                    vec![Operand::Label(resolve)],
                );
                builder.define(stdout);
                builder.emit(
                    "mov",
                    R32_IMM32,
                    Access::Write,
                    vec![reg(gpr(Gpr::Rcx)), imm(0xFFFF_FFF5)],
                );
                builder.define(resolve);
                builder.emit(
                    "call",
                    &[OperandKind::Mem],
                    Access::Read,
                    vec![Operand::Rip(
                        RelocTarget::Lir(iat_symbol("GetStdHandle")),
                        0,
                    )],
                );
                builder.emit(
                    "mov",
                    RM64_R64,
                    Access::Write,
                    vec![reg(gpr(Gpr::Rcx)), reg(gpr(Gpr::Rax))],
                );
                builder.emit(
                    "mov",
                    RM64_R64,
                    Access::Write,
                    vec![reg(gpr(Gpr::Rdx)), reg(gpr(Gpr::Rbx))],
                );
                builder.emit(
                    "mov",
                    RM64_R64,
                    Access::Write,
                    vec![reg(gpr(Gpr::R8)), reg(gpr(Gpr::R11))],
                );
                builder.emit(
                    "lea",
                    MEM_R64,
                    Access::Address,
                    vec![mem_base(gpr(Gpr::Rsp), 0x30), reg(gpr(Gpr::R9))],
                );
                builder.emit(
                    "call",
                    &[OperandKind::Mem],
                    Access::Read,
                    vec![Operand::Rip(RelocTarget::Lir(iat_symbol("WriteFile")), 0)],
                );
                builder.emit(
                    "mov",
                    RM64_R64,
                    Access::Read,
                    vec![mem_base(gpr(Gpr::Rsp), 0x30), reg(gpr(Gpr::Rax))],
                );
                builder.emit(
                    "add",
                    RM64_IMM32,
                    Access::ReadWrite,
                    vec![reg(gpr(Gpr::Rsp)), imm(0x38)],
                );
                builder.emit("ret", &[], Access::Read, Vec::new());
            }
        }
        let encoded = finish(builder);
        Self {
            revision: 1,
            bytes: encoded.bytes,
            relocations: encoded.relocations,
        }
    }
}

/// 栈 arena 使用的平台原语片段：`reserve_aligned`、`commit`、`decommit`、`release`。
///
/// 除 `reserve_aligned` 外，实参是一个指向 `(base: uint, bytes: uint)` 描述符的机器字；
/// 描述符由 Gugu 侧 arena 维护，片段只做 syscall/薄导入封装并把结果原样返回。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PlatformRangeCode {
    /// 操作。
    pub(crate) op: PlatformOp,
    /// 目标平台内部边界的版本。
    pub(crate) revision: u32,
    /// 指令字节。
    pub(crate) bytes: Vec<u8>,
    /// 修补点。
    pub(crate) relocations: Vec<Relocation>,
}

impl PlatformRangeCode {
    pub(crate) fn fixed(op: PlatformOp, target: TargetName) -> Self {
        let mut builder = Builder::new();
        match (op, target) {
            (PlatformOp::ReserveAligned, TargetName::X86_64Linux) => {
                linux_reserve_aligned(&mut builder);
            }
            (PlatformOp::ReserveAligned, TargetName::X86_64Windows) => {
                windows_reserve_aligned(&mut builder);
            }
            (PlatformOp::Commit, TargetName::X86_64Linux) => {
                linux_protect(&mut builder, 10, 3);
            }
            (PlatformOp::Commit, TargetName::X86_64Windows) => {
                windows_allocate(&mut builder, 0x1000);
            }
            (PlatformOp::Decommit, TargetName::X86_64Linux) => {
                linux_protect(&mut builder, 28, 4);
            }
            (PlatformOp::Decommit, TargetName::X86_64Windows) => {
                windows_free(&mut builder, 0x4000);
            }
            (PlatformOp::Release, TargetName::X86_64Linux) => {
                linux_release(&mut builder);
            }
            (PlatformOp::Release, TargetName::X86_64Windows) => {
                windows_free(&mut builder, 0x8000);
            }
            (op, target) => unreachable!("平台原语 {op} 不支持目标 {target}"),
        }
        let encoded = finish(builder);
        Self {
            op,
            revision: 1,
            bytes: encoded.bytes,
            relocations: encoded.relocations,
        }
    }
}

/// Linux `reserve_aligned(bytes = rax, align = rbx) -> base`：以 PROT_NONE 只预留虚址；
/// 首段与增长区由 `platform_commit` 单独提交，失败返回负 errno。
fn linux_reserve_aligned(builder: &mut Builder) {
    // rsi = totalsize = bytes + align；随后进入 mmap 参数位。
    builder.emit(
        "mov",
        RM64_R64,
        Access::Write,
        vec![reg(gpr(Gpr::Rsi)), reg(gpr(Gpr::Rbx))],
    );
    builder.emit(
        "add",
        RM64_R64,
        Access::ReadWrite,
        vec![reg(gpr(Gpr::Rsi)), reg(gpr(Gpr::Rax))],
    );
    builder.emit(
        "xor",
        RM32_R32,
        Access::ReadWrite,
        vec![reg(gpr(Gpr::Rdi)), reg(gpr(Gpr::Rdi))],
    );
    builder.emit(
        "mov",
        R32_IMM32,
        Access::Write,
        vec![reg(gpr(Gpr::Rdx)), imm(0)],
    );
    builder.emit(
        "mov",
        R32_IMM32,
        Access::Write,
        vec![reg(gpr(Gpr::R10)), imm(0x22)],
    );
    builder.emit(
        "mov",
        R64_IMM32,
        Access::Write,
        vec![reg(gpr(Gpr::R8)), imm(u64::MAX)],
    );
    builder.emit(
        "xor",
        RM32_R32,
        Access::ReadWrite,
        vec![reg(gpr(Gpr::R9)), reg(gpr(Gpr::R9))],
    );
    builder.emit(
        "mov",
        R32_IMM32,
        Access::Write,
        vec![reg(gpr(Gpr::Rax)), imm(9)],
    );
    builder.emit("syscall", &[], Access::Read, Vec::new());
    let failed = builder.label();
    builder.emit(
        "test",
        RM64_R64,
        Access::Read,
        vec![reg(gpr(Gpr::Rax)), reg(gpr(Gpr::Rax))],
    );
    builder.emit("js", REL32, Access::Read, vec![Operand::Label(failed)]);
    // r8 = base，r9 = 范围末端，rdx = aligned。
    builder.emit(
        "mov",
        RM64_R64,
        Access::Write,
        vec![reg(gpr(Gpr::R8)), reg(gpr(Gpr::Rax))],
    );
    builder.emit(
        "mov",
        RM64_R64,
        Access::Write,
        vec![reg(gpr(Gpr::R9)), reg(gpr(Gpr::R8))],
    );
    builder.emit(
        "add",
        RM64_R64,
        Access::ReadWrite,
        vec![reg(gpr(Gpr::R9)), reg(gpr(Gpr::Rsi))],
    );
    builder.emit(
        "mov",
        RM64_R64,
        Access::Write,
        vec![reg(gpr(Gpr::Rdx)), reg(gpr(Gpr::Rbx))],
    );
    builder.emit("neg", RM64, Access::ReadWrite, vec![reg(gpr(Gpr::Rdx))]);
    builder.emit(
        "mov",
        RM64_R64,
        Access::Write,
        vec![reg(gpr(Gpr::Rdi)), reg(gpr(Gpr::R8))],
    );
    builder.emit(
        "add",
        RM64_R64,
        Access::ReadWrite,
        vec![reg(gpr(Gpr::Rdi)), reg(gpr(Gpr::Rbx))],
    );
    builder.emit(
        "sub",
        RM64_IMM32,
        Access::ReadWrite,
        vec![reg(gpr(Gpr::Rdi)), imm(1)],
    );
    builder.emit(
        "and",
        RM64_R64,
        Access::ReadWrite,
        vec![reg(gpr(Gpr::Rdi)), reg(gpr(Gpr::Rdx))],
    );
    builder.emit(
        "mov",
        RM64_R64,
        Access::Write,
        vec![reg(gpr(Gpr::Rsi)), reg(gpr(Gpr::Rdi))],
    );
    builder.emit(
        "sub",
        RM64_R64,
        Access::ReadWrite,
        vec![reg(gpr(Gpr::Rsi)), reg(gpr(Gpr::R8))],
    );
    builder.emit(
        "mov",
        RM64_R64,
        Access::Write,
        vec![reg(gpr(Gpr::Rdx)), reg(gpr(Gpr::Rdi))],
    );
    builder.emit(
        "mov",
        RM64_R64,
        Access::Write,
        vec![reg(gpr(Gpr::Rdi)), reg(gpr(Gpr::R8))],
    );
    builder.emit(
        "mov",
        R32_IMM32,
        Access::Write,
        vec![reg(gpr(Gpr::Rax)), imm(11)],
    );
    builder.emit("syscall", &[], Access::Read, Vec::new());
    // 尾部：tail_len = align - front，tail_start = end - tail_len。
    builder.emit(
        "mov",
        RM64_R64,
        Access::Write,
        vec![reg(gpr(Gpr::R10)), reg(gpr(Gpr::Rbx))],
    );
    builder.emit(
        "sub",
        RM64_R64,
        Access::ReadWrite,
        vec![reg(gpr(Gpr::R10)), reg(gpr(Gpr::Rsi))],
    );
    builder.emit(
        "mov",
        RM64_R64,
        Access::Write,
        vec![reg(gpr(Gpr::Rdi)), reg(gpr(Gpr::R9))],
    );
    builder.emit(
        "sub",
        RM64_R64,
        Access::ReadWrite,
        vec![reg(gpr(Gpr::Rdi)), reg(gpr(Gpr::R10))],
    );
    builder.emit(
        "mov",
        RM64_R64,
        Access::Write,
        vec![reg(gpr(Gpr::Rsi)), reg(gpr(Gpr::R10))],
    );
    builder.emit(
        "mov",
        R32_IMM32,
        Access::Write,
        vec![reg(gpr(Gpr::Rax)), imm(11)],
    );
    builder.emit("syscall", &[], Access::Read, Vec::new());
    builder.emit(
        "mov",
        RM64_R64,
        Access::Write,
        vec![reg(gpr(Gpr::Rax)), reg(gpr(Gpr::Rdx))],
    );
    builder.define(failed);
    builder.emit("ret", &[], Access::Read, Vec::new());
}

/// Linux 描述符调用：`(base = [rax], bytes = [rax + 8])`，syscall 号与第三实参由调用方给出。
fn linux_protect(builder: &mut Builder, call: u64, argument: u64) {
    builder.emit(
        "mov",
        RM64_R64,
        Access::Write,
        vec![reg(gpr(Gpr::Rbx)), reg(gpr(Gpr::Rax))],
    );
    builder.emit(
        "mov",
        RM64_R64,
        Access::Read,
        vec![mem_base(gpr(Gpr::Rbx), 0), reg(gpr(Gpr::Rdi))],
    );
    builder.emit(
        "mov",
        RM64_R64,
        Access::Read,
        vec![mem_base(gpr(Gpr::Rbx), 8), reg(gpr(Gpr::Rsi))],
    );
    builder.emit(
        "mov",
        R32_IMM32,
        Access::Write,
        vec![reg(gpr(Gpr::Rdx)), imm(argument)],
    );
    builder.emit(
        "mov",
        R32_IMM32,
        Access::Write,
        vec![reg(gpr(Gpr::Rax)), imm(call)],
    );
    builder.emit("syscall", &[], Access::Read, Vec::new());
    builder.emit("ret", &[], Access::Read, Vec::new());
}

/// Linux `release`：`munmap([rax], [rax + 8])`。
fn linux_release(builder: &mut Builder) {
    builder.emit(
        "mov",
        RM64_R64,
        Access::Write,
        vec![reg(gpr(Gpr::Rbx)), reg(gpr(Gpr::Rax))],
    );
    builder.emit(
        "mov",
        RM64_R64,
        Access::Read,
        vec![mem_base(gpr(Gpr::Rbx), 0), reg(gpr(Gpr::Rdi))],
    );
    builder.emit(
        "mov",
        RM64_R64,
        Access::Read,
        vec![mem_base(gpr(Gpr::Rbx), 8), reg(gpr(Gpr::Rsi))],
    );
    builder.emit(
        "mov",
        R32_IMM32,
        Access::Write,
        vec![reg(gpr(Gpr::Rax)), imm(11)],
    );
    builder.emit("syscall", &[], Access::Read, Vec::new());
    builder.emit("ret", &[], Access::Read, Vec::new());
}

/// `reserve_aligned(bytes = rax, align = rbx) -> base`：传给 NT 的是栈上 in/out 描述符，
/// 不能把 `bytes` 当描述符地址解引用。Windows 预约基址至少按 64 KiB 对齐。
fn windows_reserve_aligned(builder: &mut Builder) {
    let failed = builder.label();
    let done = builder.label();
    builder.emit(
        "cmp",
        RM64_IMM32,
        Access::Read,
        vec![reg(gpr(Gpr::Rbx)), imm(65536)],
    );
    builder.emit("ja", REL32, Access::Read, vec![Operand::Label(failed)]);
    builder.emit(
        "sub",
        RM64_IMM32,
        Access::ReadWrite,
        vec![reg(gpr(Gpr::Rsp)), imm(0x48)],
    );
    builder.emit(
        "mov",
        RM64_IMM32,
        Access::Write,
        vec![mem_base(gpr(Gpr::Rsp), 0x30), imm(0)],
    );
    builder.emit(
        "mov",
        RM64_R64,
        Access::Write,
        vec![mem_base(gpr(Gpr::Rsp), 0x38), reg(gpr(Gpr::Rax))],
    );
    builder.emit(
        "mov",
        R64_IMM32,
        Access::Write,
        vec![reg(gpr(Gpr::Rcx)), imm(u64::MAX)],
    );
    builder.emit(
        "lea",
        MEM_R64,
        Access::Address,
        vec![mem_base(gpr(Gpr::Rsp), 0x30), reg(gpr(Gpr::Rdx))],
    );
    builder.emit(
        "xor",
        RM32_R32,
        Access::ReadWrite,
        vec![reg(gpr(Gpr::R8)), reg(gpr(Gpr::R8))],
    );
    builder.emit(
        "lea",
        MEM_R64,
        Access::Address,
        vec![mem_base(gpr(Gpr::Rsp), 0x38), reg(gpr(Gpr::R9))],
    );
    builder.emit(
        "mov",
        RM64_IMM32,
        Access::Write,
        vec![mem_base(gpr(Gpr::Rsp), 0x20), imm(0x2000)],
    );
    builder.emit(
        "mov",
        RM64_IMM32,
        Access::Write,
        vec![mem_base(gpr(Gpr::Rsp), 0x28), imm(1)],
    );
    builder.emit(
        "call",
        &[OperandKind::Mem],
        Access::Read,
        vec![Operand::Rip(
            RelocTarget::Lir(iat_symbol("NtAllocateVirtualMemory")),
            0,
        )],
    );
    builder.emit(
        "test",
        RM32_R32,
        Access::Read,
        vec![reg(gpr(Gpr::Rax)), reg(gpr(Gpr::Rax))],
    );
    builder.emit("jne", REL32, Access::Read, vec![Operand::Label(done)]);
    builder.emit(
        "mov",
        RM64_R64,
        Access::Read,
        vec![mem_base(gpr(Gpr::Rsp), 0x30), reg(gpr(Gpr::Rax))],
    );
    builder.emit(
        "add",
        RM64_IMM32,
        Access::ReadWrite,
        vec![reg(gpr(Gpr::Rsp)), imm(0x48)],
    );
    builder.emit("ret", &[], Access::Read, Vec::new());
    builder.define(done);
    builder.emit(
        "add",
        RM64_IMM32,
        Access::ReadWrite,
        vec![reg(gpr(Gpr::Rsp)), imm(0x48)],
    );
    builder.define(failed);
    builder.emit(
        "xor",
        RM32_R32,
        Access::ReadWrite,
        vec![reg(gpr(Gpr::Rax)), reg(gpr(Gpr::Rax))],
    );
    builder.emit("ret", &[], Access::Read, Vec::new());
}

/// Windows `NtAllocateVirtualMemory`：描述符即 in/out base，`[desc + 8]` 是 in/out size。
fn windows_allocate(builder: &mut Builder, allocation: u64) {
    builder.emit(
        "sub",
        RM64_IMM32,
        Access::ReadWrite,
        vec![reg(gpr(Gpr::Rsp)), imm(0x38)],
    );
    builder.emit(
        "mov",
        R64_IMM32,
        Access::Write,
        vec![reg(gpr(Gpr::Rcx)), imm(u64::MAX)],
    );
    builder.emit(
        "mov",
        RM64_R64,
        Access::Write,
        vec![reg(gpr(Gpr::Rdx)), reg(gpr(Gpr::Rax))],
    );
    builder.emit(
        "xor",
        RM32_R32,
        Access::ReadWrite,
        vec![reg(gpr(Gpr::R8)), reg(gpr(Gpr::R8))],
    );
    builder.emit(
        "lea",
        MEM_R64,
        Access::Address,
        vec![mem_base(gpr(Gpr::Rax), 8), reg(gpr(Gpr::R9))],
    );
    builder.emit(
        "mov",
        RM64_IMM32,
        Access::Write,
        vec![mem_base(gpr(Gpr::Rsp), 0x20), imm(allocation)],
    );
    builder.emit(
        "mov",
        RM64_IMM32,
        Access::Write,
        vec![mem_base(gpr(Gpr::Rsp), 0x28), imm(4)],
    );
    builder.emit(
        "call",
        &[OperandKind::Mem],
        Access::Read,
        vec![Operand::Rip(
            RelocTarget::Lir(iat_symbol("NtAllocateVirtualMemory")),
            0,
        )],
    );
    builder.emit(
        "add",
        RM64_IMM32,
        Access::ReadWrite,
        vec![reg(gpr(Gpr::Rsp)), imm(0x38)],
    );
    builder.emit("ret", &[], Access::Read, Vec::new());
}

/// Windows `NtFreeVirtualMemory`：描述符即 in/out base，`[desc + 8]` 是 in/out size。
fn windows_free(builder: &mut Builder, free: u64) {
    builder.emit(
        "sub",
        RM64_IMM32,
        Access::ReadWrite,
        vec![reg(gpr(Gpr::Rsp)), imm(0x28)],
    );
    builder.emit(
        "mov",
        R64_IMM32,
        Access::Write,
        vec![reg(gpr(Gpr::Rcx)), imm(u64::MAX)],
    );
    builder.emit(
        "mov",
        RM64_R64,
        Access::Write,
        vec![reg(gpr(Gpr::Rdx)), reg(gpr(Gpr::Rax))],
    );
    builder.emit(
        "lea",
        MEM_R64,
        Access::Address,
        vec![mem_base(gpr(Gpr::Rax), 8), reg(gpr(Gpr::R8))],
    );
    builder.emit(
        "mov",
        R32_IMM32,
        Access::Write,
        vec![reg(gpr(Gpr::R9)), imm(free)],
    );
    builder.emit(
        "call",
        &[OperandKind::Mem],
        Access::Read,
        vec![Operand::Rip(
            RelocTarget::Lir(iat_symbol("NtFreeVirtualMemory")),
            0,
        )],
    );
    builder.emit(
        "add",
        RM64_IMM32,
        Access::ReadWrite,
        vec![reg(gpr(Gpr::Rsp)), imm(0x28)],
    );
    builder.emit("ret", &[], Access::Read, Vec::new());
}

/// 内存操作 glue 的种类。名字即 `mangle_glue` 的输入，与 lowering 里写死的调用名一一对应。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum MemoryGlueKind {
    /// `Op::Memcpy`：非重叠拷贝。
    Memcpy,
    /// `Op::Memmove`：允许重叠拷贝。
    Memmove,
    /// `Op::Memset`：填充。
    Memset,
}

impl MemoryGlueKind {
    /// lowering 写进 LIR 的 glue 名。
    pub(crate) fn glue(self) -> &'static str {
        match self {
            Self::Memcpy => "memcpy",
            Self::Memmove => "memmove",
            Self::Memset => "memset",
        }
    }
}

/// 编译器生成体的机器片段：`Op::Memcpy`/`Op::Memmove`/`Op::Memset` 的 lowering 直接按 glue 域
/// 符号调用，缺定义就会落到 stub，因此这三个符号必须由镜像自己提供。
///
/// 调用约定来自 lowering（`backend/x64/lower/memory.rs`）：`rax = 目的`、`rbx = 源或填充字节`、
/// `rcx = 字节数`。指令用了 `rep movsb`/`rep stosb` 这种前缀 + 隐式操作数的形式，内部 encoder 的
/// 助记符表表达不了，所以按字节给出并逐条注释。方向标志由调用约定保证为 0（SysV/Windows 都要求），
/// 只有 `memmove` 的向后路径临时置位并在返回前清回。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct MemoryGlueCode {
    /// 目标平台内部边界的版本。
    pub(crate) revision: u32,
    /// 指令字节。
    pub(crate) bytes: Vec<u8>,
}

impl MemoryGlueCode {
    /// 三个内存 glue 的固定字节串。
    pub(crate) fn fixed(kind: MemoryGlueKind) -> Self {
        let bytes = match kind {
            MemoryGlueKind::Memcpy => vec![
                0x48, 0x89, 0xde, // mov rsi, rbx
                0x48, 0x89, 0xc7, // mov rdi, rax
                0xf3, 0xa4, // rep movsb
                0xc3, // ret
            ],
            MemoryGlueKind::Memmove => vec![
                0x48, 0x89, 0xde, // mov rsi, rbx
                0x48, 0x89, 0xc7, // mov rdi, rax
                0x48, 0x39, 0xf7, // cmp rdi, rsi
                0x76, 0x0f, // jbe forward（目的 <= 源时向前拷）
                0x48, 0x8d, 0x7c, 0x0f, 0xff, // lea rdi, [rdi + rcx - 1]
                0x48, 0x8d, 0x74, 0x0e, 0xff, // lea rsi, [rsi + rcx - 1]
                0xfd, // std
                0xf3, 0xa4, // rep movsb
                0xfc, // cld
                0xc3, // ret
                0xf3, 0xa4, // forward: rep movsb
                0xc3, // ret
            ],
            MemoryGlueKind::Memset => vec![
                0x48, 0x89, 0xc7, // mov rdi, rax
                0x48, 0x89, 0xd8, // mov rax, rbx（rep stosb 取 al 作填充字节）
                0xf3, 0xaa, // rep stosb
                0xc3, // ret
            ],
        };
        Self {
            revision: MEMORY_GLUE_REVISION,
            bytes,
        }
    }
}

/// 内存 glue 字节串的版本。
pub(crate) const MEMORY_GLUE_REVISION: u32 = 1;

/// `current_slot()` 片段：返回当前协程句柄，入口 `r14` 保持为 `CoroutineHot*`。
fn current_slot_bytes() -> Vec<u8> {
    vec![0x4c, 0x89, 0xf0, 0xc3]
}

/// panic 入口传递原调用返回 PC 与 runtime 状态；dispatcher 返回栈图 landing 的机器地址。
fn panic_entry_fragment() -> FragmentBlob {
    let mut builder = Builder::new();
    builder.emit(
        "sub",
        RM64_IMM32,
        Access::ReadWrite,
        vec![reg(gpr(Gpr::Rsp)), imm(8)],
    );
    builder.emit(
        "mov",
        RM64_R64,
        Access::Read,
        vec![mem_base(gpr(Gpr::Rsp), 8), reg(gpr(Gpr::R8))],
    );
    builder.emit(
        "lea",
        MEM_R64,
        Access::Address,
        vec![
            Operand::Rip(RelocTarget::Lir(glue_symbol("morestack_mailbox")), 0),
            reg(gpr(Gpr::R9)),
        ],
    );
    builder.emit(
        "call",
        REL32,
        Access::Read,
        vec![Operand::Reloc(
            RelocTarget::Lir(glue_symbol("panic_dispatch")),
            RelocKind::PcRel32,
        )],
    );
    builder.emit(
        "add",
        RM64_IMM32,
        Access::ReadWrite,
        vec![reg(gpr(Gpr::Rsp)), imm(16)],
    );
    builder.emit("jmp", RM64, Access::Read, vec![reg(gpr(Gpr::Rax))]);
    let encoded = finish(builder);
    FragmentBlob {
        alignment: FRAGMENT_ALIGNMENT,
        domain: FragmentDomain::Runtime,
        bytes: encoded.bytes,
        entries: vec![FragmentEntry {
            glue: "panic",
            offset: 0,
        }],
        relocations: encoded.relocations,
    }
}
/// PollResume 的同步慢路只消费 PREEMPT 位，保留 GC_STOP 交给 collector；它不分配、不切换栈。
fn safepoint_slow_fragment() -> FragmentBlob {
    let mut builder = Builder::new();
    let offset = i32::try_from(processor::poll_flags_offset()).expect("poll flags 偏移适配 i32");
    let poll = mem_base(gpr(Gpr::R15), offset);
    builder.emit(
        "mov",
        RM32_R32,
        Access::Read,
        vec![poll.clone(), reg(gpr(Gpr::Rax))],
    );
    builder.emit(
        "and",
        RM32_IMM8,
        Access::ReadWrite,
        vec![reg(gpr(Gpr::Rax)), imm(2)],
    );
    builder.emit(
        "mov",
        RM32_R32,
        Access::Write,
        vec![poll, reg(gpr(Gpr::Rax))],
    );
    builder.emit("ret", &[], Access::Read, Vec::new());
    let encoded = finish(builder);
    FragmentBlob {
        alignment: FRAGMENT_ALIGNMENT,
        domain: FragmentDomain::Runtime,
        bytes: encoded.bytes,
        entries: vec![FragmentEntry {
            glue: "safepoint_slow",
            offset: 0,
        }],
        relocations: encoded.relocations,
    }
}

/// GC 入口：发布发起者 RSP、切到 runtime 专用 system stack、调用 dispatcher 后恢复。
///
/// 收集器必须在与协程栈分离的边界上运行：它在 system stack 上执行，因此自身帧既不会触达
/// mutator 的栈上限，也不会被发起者的根扫描当成 mutator 帧。`rax` 是请求指针、`rbx` 是发起者
/// RSP，两者都是登记片段 ABI 的一部分。
fn gc_enter_fragment() -> FragmentBlob {
    let mut builder = Builder::new();
    let state = i32::try_from(processor::PROCESSOR_STATE_OFFSET).expect("状态区偏移适配 i32");
    let coordinator = i32::try_from(crate::runtime::entry::state::COORDINATOR)
        .expect("coordinator 槽偏移适配 i32");
    let entry_rsp = crate::runtime::collect_layout::COORDINATOR_ENTRY_RSP_OFFSET;
    let saved_rsp = crate::runtime::collect_layout::COORDINATOR_SAVED_RSP_OFFSET;
    let system_top = crate::runtime::collect_layout::COORDINATOR_SYSTEM_TOP_OFFSET;
    // r11 = coordinator（processor 在 r15）。
    builder.emit(
        "mov",
        RM64_R64,
        Access::Read,
        vec![mem_base(gpr(Gpr::R15), state), reg(gpr(Gpr::R11))],
    );
    builder.emit(
        "mov",
        RM64_R64,
        Access::Read,
        vec![mem_base(gpr(Gpr::R11), coordinator), reg(gpr(Gpr::R11))],
    );
    // system 模式下 r14 指向 collector 自己的栈描述符：否则收集器在 system stack 上执行时，
    // 栈检查仍拿 mutator 协程的 stack_check 比较，会误报 StackOverflow。
    let system_descriptor = crate::runtime::collect_layout::COORDINATOR_SYSTEM_DESCRIPTOR_OFFSET;
    let saved_r14 = crate::runtime::collect_layout::COORDINATOR_SAVED_R14_OFFSET;
    builder.emit(
        "mov",
        RM64_R64,
        Access::Write,
        vec![mem_base(gpr(Gpr::R11), saved_r14), reg(gpr(Gpr::R14))],
    );
    builder.emit(
        "mov",
        RM64_R64,
        Access::Read,
        vec![
            mem_base(gpr(Gpr::R11), system_descriptor),
            reg(gpr(Gpr::R14)),
        ],
    );
    // 发布发起者 RSP，再切到 system stack；`rbx` 把同一个 RSP 交给 dispatcher。
    builder.emit(
        "mov",
        RM64_R64,
        Access::Write,
        vec![mem_base(gpr(Gpr::R11), entry_rsp), reg(gpr(Gpr::Rsp))],
    );
    builder.emit(
        "mov",
        RM64_R64,
        Access::Write,
        vec![mem_base(gpr(Gpr::R11), saved_rsp), reg(gpr(Gpr::Rsp))],
    );
    builder.emit(
        "mov",
        RM64_R64,
        Access::Read,
        vec![mem_base(gpr(Gpr::R11), system_top), reg(gpr(Gpr::Rsp))],
    );
    // Gugu 函数调用 ABI 是 rdi/rsi；机器原语的 rax 车道在这里换成 dispatcher 的第一实参。
    builder.emit(
        "mov",
        RM64_R64,
        Access::Write,
        vec![reg(gpr(Gpr::Rdi)), reg(gpr(Gpr::Rax))],
    );
    builder.emit(
        "mov",
        RM64_R64,
        Access::Read,
        vec![mem_base(gpr(Gpr::R11), entry_rsp), reg(gpr(Gpr::Rsi))],
    );
    builder.emit(
        "mov",
        RM64_R64,
        Access::Read,
        vec![mem_base(gpr(Gpr::R11), entry_rsp), reg(gpr(Gpr::Rbx))],
    );
    builder.emit(
        "call",
        REL32,
        Access::Read,
        vec![Operand::Reloc(
            RelocTarget::Lir(glue_symbol("gc_dispatch")),
            RelocKind::PcRel32,
        )],
    );
    builder.emit(
        "mov",
        RM64_R64,
        Access::Read,
        vec![mem_base(gpr(Gpr::R15), state), reg(gpr(Gpr::R11))],
    );
    builder.emit(
        "mov",
        RM64_R64,
        Access::Read,
        vec![mem_base(gpr(Gpr::R11), coordinator), reg(gpr(Gpr::R11))],
    );
    builder.emit(
        "mov",
        RM64_R64,
        Access::Read,
        vec![mem_base(gpr(Gpr::R11), saved_r14), reg(gpr(Gpr::R14))],
    );
    builder.emit(
        "mov",
        RM64_R64,
        Access::Read,
        vec![mem_base(gpr(Gpr::R11), saved_rsp), reg(gpr(Gpr::Rsp))],
    );
    builder.emit("ret", &[], Access::Read, Vec::new());
    let encoded = finish(builder);
    FragmentBlob {
        alignment: FRAGMENT_ALIGNMENT,
        domain: FragmentDomain::Runtime,
        bytes: encoded.bytes,
        entries: vec![FragmentEntry {
            glue: "gc_enter",
            offset: 0,
        }],
        relocations: encoded.relocations,
    }
}

/// landing 的 `ResumePanic` 终结符跳到这里：从状态区恢复原报告参数，退出路径不返回。
fn panic_resume_fragment() -> FragmentBlob {
    let mut builder = Builder::new();
    builder.emit(
        "lea",
        MEM_R64,
        Access::Address,
        vec![
            Operand::Rip(RelocTarget::Lir(glue_symbol("morestack_mailbox")), 0),
            reg(gpr(Gpr::R11)),
        ],
    );
    builder.emit(
        "cmp",
        RM64_IMM32,
        Access::Read,
        vec![mem_base(gpr(Gpr::R11), 104), imm(1)],
    );
    let invalid = builder.label();
    builder.emit("jne", REL32, Access::Read, vec![Operand::Label(invalid)]);
    for (register, offset) in [
        (Gpr::Rax, 640),
        (Gpr::Rbx, 648),
        (Gpr::Rcx, 656),
        (Gpr::Rdx, 664),
        (Gpr::Rdi, 672),
        (Gpr::Rsi, 680),
    ] {
        builder.emit(
            "mov",
            RM64_R64,
            Access::Read,
            vec![mem_base(gpr(Gpr::R11), offset), reg(gpr(register))],
        );
    }
    builder.emit(
        "call",
        REL32,
        Access::Read,
        vec![Operand::Reloc(
            RelocTarget::Lir(glue_symbol("panic_unwind")),
            RelocKind::PcRel32,
        )],
    );
    builder.define(invalid);
    builder.emit(
        "mov",
        R32_IMM32,
        Access::Write,
        vec![reg(gpr(Gpr::Rax)), imm(2)],
    );
    builder.emit(
        "jmp",
        REL32,
        Access::Read,
        vec![Operand::Reloc(
            RelocTarget::Lir(glue_symbol("process_exit")),
            RelocKind::PcRel32,
        )],
    );
    let encoded = finish(builder);
    FragmentBlob {
        alignment: FRAGMENT_ALIGNMENT,
        domain: FragmentDomain::Runtime,
        bytes: encoded.bytes,
        entries: vec![FragmentEntry {
            glue: "panic_resume",
            offset: 0,
        }],
        relocations: encoded.relocations,
    }
}

/// 子协程入口：从栈上的 `(code, env)` 装入内部 ABI 参数，调用用户函数，再切回系统栈。
fn coroutine_enter_fragment() -> FragmentBlob {
    let mut builder = Builder::new();
    let mut planned: Vec<(&'static str, i32)> = Vec::new();
    // 整数车道：lane0 落在 Rax（入口先把它当包指针，随后换成车道值），lane1..lane8 依次装入
    // 其余内部整数寄存器；偏移全部来自参数包布局契约。
    let integer = [
        Gpr::Rbx,
        Gpr::Rcx,
        Gpr::Rdx,
        Gpr::Rdi,
        Gpr::Rsi,
        Gpr::R8,
        Gpr::R9,
        Gpr::R10,
    ];
    // `[rsp]` 的零位移编码会多写出一字节；先把 rsp 放到 r11 再间接读取。
    builder.emit(
        "mov",
        RM64_R64,
        Access::Read,
        vec![reg(gpr(Gpr::Rsp)), reg(gpr(Gpr::R11))],
    );
    builder.emit(
        "mov",
        RM64_R64,
        Access::Read,
        vec![mem_base(gpr(Gpr::R11), 0), reg(gpr(Gpr::R11))],
    );
    builder.emit(
        "mov",
        RM64_R64,
        Access::Read,
        vec![mem_base(gpr(Gpr::Rsp), 8), reg(gpr(Gpr::Rax))],
    );
    for (lane, register) in integer.iter().enumerate() {
        let offset = i32::try_from(coroutine_pack_integer_offset(lane + 1)).expect("车道偏移");
        builder.emit(
            "mov",
            RM64_R64,
            Access::Read,
            vec![mem_base(gpr(Gpr::Rax), offset), reg(gpr(*register))],
        );
    }
    for index in 0..COROUTINE_PACK_FLOAT_LANES {
        let offset = i32::try_from(coroutine_pack_float_offset(index)).expect("车道偏移");
        builder.emit(
            "movups",
            XMMRM_XMM,
            Access::Read,
            vec![
                mem_base(gpr(Gpr::Rax), offset),
                xmm(crate::backend::x64::abi::INTERNAL_FLOAT[index]),
            ],
        );
    }
    builder.emit(
        "mov",
        RM64_R64,
        Access::Read,
        vec![mem_base(gpr(Gpr::Rax), 0), reg(gpr(Gpr::Rax))],
    );
    builder.emit("call", RM64, Access::Read, vec![reg(gpr(Gpr::R11))]);
    let return_value = field_offset(offset_of!(MorestackMailbox, return_value));
    let candidate = field_offset(offset_of!(MorestackMailbox, candidate));
    let system_context = field_offset(offset_of!(MorestackMailbox, system_context));
    let hot = field_offset(offset_of!(MorestackMailbox, hot));
    let processor = field_offset(offset_of!(MorestackMailbox, processor));
    builder.emit(
        "mov",
        RM64_R64,
        Access::Write,
        vec![
            mailbox_operand(&mut planned, return_value),
            reg(gpr(Gpr::Rax)),
        ],
    );
    builder.emit(
        "mov",
        RM64_R64,
        Access::Write,
        vec![mailbox_operand(&mut planned, candidate), reg(gpr(Gpr::Rbx))],
    );
    builder.emit(
        "mov",
        R32_IMM32,
        Access::Write,
        vec![reg(gpr(Gpr::Rdx)), imm(2)],
    );
    builder.emit(
        "mov",
        RM64_R64,
        Access::Write,
        vec![
            mailbox_operand(&mut planned, SCHEDULE_REASON),
            reg(gpr(Gpr::Rdx)),
        ],
    );
    builder.emit(
        "lea",
        MEM_R64,
        Access::Address,
        vec![
            mailbox_operand(&mut planned, system_context),
            reg(gpr(Gpr::Rax)),
        ],
    );
    builder.emit(
        "mov",
        RM64_R64,
        Access::Read,
        vec![mailbox_operand(&mut planned, hot), reg(gpr(Gpr::Rbx))],
    );
    builder.emit(
        "mov",
        RM64_R64,
        Access::Read,
        vec![mailbox_operand(&mut planned, processor), reg(gpr(Gpr::Rcx))],
    );
    planned.push(("context_restore", 0));
    builder.emit(
        "jmp",
        REL32,
        Access::Read,
        vec![Operand::Reloc(
            RelocTarget::Lir(glue_symbol("context_restore")),
            RelocKind::PcRel32,
        )],
    );
    let encoded = finish(builder);
    FragmentBlob {
        alignment: FRAGMENT_ALIGNMENT,
        domain: FragmentDomain::Runtime,
        bytes: encoded.bytes,
        entries: vec![FragmentEntry {
            glue: "coroutine_enter",
            offset: 0,
        }],
        relocations: relocations_of(&patch_list(&encoded.relocations, &planned)),
    }
}

/// 已有机器片段实现、因而登记为 runtime glue 的平台范围原语。
///
/// 其余平台操作（guard/Unprotect/Wait/Wake/Entropy/Zero/SetDumpPolicy/两个 hint）在镜像里还
/// 没有执行体，属于后续步骤；发射门禁必须拒绝它们，绝不静默落桩。
pub(crate) const PLATFORM_OPS_WITH_CODE: [PlatformOp; 4] = [
    PlatformOp::ReserveAligned,
    PlatformOp::Commit,
    PlatformOp::Decommit,
    PlatformOp::Release,
];

/// 目标平台的全部登记片段；一个 blob 可带多个入口（context 片段）。
pub(crate) fn registered_fragments(target: TargetName) -> Vec<FragmentBlob> {
    let context = ContextSwitchCode::fixed();
    let morestack = morestack_entry_code();
    let exit = ProcessExitCode::fixed(target);
    let main_return = main_return_code();
    let report = ReportWriteCode::fixed(target);
    let mut blobs = vec![
        FragmentBlob {
            alignment: FRAGMENT_ALIGNMENT,
            domain: FragmentDomain::Runtime,
            bytes: context.bytes,
            entries: vec![
                FragmentEntry {
                    glue: "context_switch",
                    offset: 0,
                },
                FragmentEntry {
                    glue: "context_restore",
                    offset: context.restore_offset,
                },
            ],
            relocations: Vec::new(),
        },
        FragmentBlob {
            alignment: FRAGMENT_ALIGNMENT,
            domain: FragmentDomain::Runtime,
            bytes: current_slot_bytes(),
            entries: vec![FragmentEntry {
                glue: "current_slot",
                offset: 0,
            }],
            relocations: Vec::new(),
        },
        FragmentBlob {
            alignment: FRAGMENT_ALIGNMENT,
            domain: FragmentDomain::Runtime,
            bytes: safepoint_slow_fragment().bytes,
            entries: vec![FragmentEntry {
                glue: "safepoint_slow",
                offset: 0,
            }],
            relocations: Vec::new(),
        },
        FragmentBlob {
            alignment: FRAGMENT_ALIGNMENT,
            domain: FragmentDomain::Runtime,
            bytes: morestack.bytes,
            entries: vec![FragmentEntry {
                glue: "morestack_or_poll",
                offset: 0,
            }],
            relocations: relocations_of(&morestack.patches),
        },
        FragmentBlob {
            alignment: FRAGMENT_ALIGNMENT,
            domain: FragmentDomain::Runtime,
            bytes: exit.bytes,
            entries: vec![FragmentEntry {
                glue: "process_exit",
                offset: 0,
            }],
            relocations: exit.relocations,
        },
        FragmentBlob {
            alignment: FRAGMENT_ALIGNMENT,
            domain: FragmentDomain::Runtime,
            bytes: main_return.bytes,
            entries: vec![FragmentEntry {
                glue: "main_return",
                offset: 0,
            }],
            relocations: relocations_of(&main_return.patches),
        },
        FragmentBlob {
            alignment: FRAGMENT_ALIGNMENT,
            domain: FragmentDomain::Runtime,
            bytes: report.bytes,
            entries: vec![FragmentEntry {
                glue: "report_write",
                offset: 0,
            }],
            relocations: report.relocations,
        },
    ];
    blobs.push(gc_enter_fragment());
    blobs.push(coroutine_enter_fragment());
    blobs.push(panic_entry_fragment());
    blobs.push(panic_resume_fragment());
    // GC 配置记录：数据片段承载编译器生成的只读 `GcImageConfig`，其地址由同名 glue 符号
    // 暴露；访问器片段用 PC 相对 `lea` 把它取进 rax 返回（机器片段只允许 PC 相对修补点）。
    blobs.push(FragmentBlob {
        alignment: 8,
        domain: FragmentDomain::Runtime,
        bytes: crate::runtime::GcImageConfig::build().canonical_bytes(),
        entries: vec![FragmentEntry {
            glue: "gc_config_record",
            offset: 0,
        }],
        relocations: Vec::new(),
    });
    let mut accessor = vec![0x48, 0x8D, 0x05];
    accessor.extend_from_slice(&[0_u8; 4]);
    accessor.push(0xC3);
    blobs.push(FragmentBlob {
        alignment: FRAGMENT_ALIGNMENT,
        domain: FragmentDomain::Runtime,
        bytes: accessor,
        entries: vec![FragmentEntry {
            glue: "gc_config",
            offset: 0,
        }],
        relocations: vec![Relocation {
            offset: 3,
            kind: RelocKind::PcRel32,
            target: RelocTarget::Lir(glue_symbol("gc_config_record")),
            addend: 0,
        }],
    });
    for op in PLATFORM_OPS_WITH_CODE {
        let encoded = PlatformRangeCode::fixed(op, target);
        blobs.push(FragmentBlob {
            alignment: FRAGMENT_ALIGNMENT,
            domain: FragmentDomain::Runtime,
            bytes: encoded.bytes,
            entries: vec![FragmentEntry {
                glue: match op {
                    PlatformOp::ReserveAligned => "platform_reserve_aligned",
                    PlatformOp::Commit => "platform_commit",
                    PlatformOp::Decommit => "platform_decommit",
                    PlatformOp::Release => "platform_release",
                    _ => unreachable!("登记片段只覆盖四个范围原语"),
                },
                offset: 0,
            }],
            relocations: encoded.relocations,
        });
    }
    for kind in [
        MemoryGlueKind::Memcpy,
        MemoryGlueKind::Memset,
        MemoryGlueKind::Memmove,
    ] {
        let encoded = MemoryGlueCode::fixed(kind);
        blobs.push(FragmentBlob {
            alignment: FRAGMENT_ALIGNMENT,
            domain: FragmentDomain::Glue,
            bytes: encoded.bytes,
            entries: vec![FragmentEntry {
                glue: kind.glue(),
                offset: 0,
            }],
            relocations: Vec::new(),
        });
    }
    blobs
}

#[cfg(test)]
mod tests {
    use super::{current_slot_bytes, registered_fragments};
    use crate::target::TargetName;

    #[test]
    fn current_slot_returns_r14_in_rax() {
        assert_eq!(current_slot_bytes(), [0x4c, 0x89, 0xf0, 0xc3]);
        let fragment = registered_fragments(TargetName::X86_64Linux)
            .into_iter()
            .find(|fragment| {
                fragment
                    .entries
                    .iter()
                    .any(|entry| entry.glue == "current_slot")
            })
            .expect("current_slot 片段已登记");
        assert_eq!(fragment.bytes, [0x4c, 0x89, 0xf0, 0xc3]);
    }

    /// 注入的 morestack/main 返回片段逐字节对照编码器：字节、长度、续点、版本与修补点
    /// 任何一处被改都必须在 backend 消费契约前被拒。
    #[test]
    fn coroutine_code_corruption_is_rejected_before_consumption() {
        use super::{main_return_code, morestack_entry_code, verify_coroutine_code};
        use crate::runtime::{CoroutineDemand, CoroutineRuntimeContract};
        let contract = CoroutineRuntimeContract::build(CoroutineDemand::default())
            .unwrap()
            .with_code(morestack_entry_code(), main_return_code());
        verify_coroutine_code(&contract).expect("编码器产物自洽");
        let corruptions: [(&str, fn(&mut CoroutineRuntimeContract)); 7] = [
            ("改片段字节", |c| c.morestack.bytes[0] ^= 1),
            ("截断片段", |c| {
                let len = c.morestack.bytes.len();
                c.morestack.bytes.truncate(len - 1);
            }),
            ("改续点偏移", |c| c.morestack.resume_offset += 1),
            ("改片段版本", |c| c.morestack.revision += 1),
            ("改修补点", |c| c.morestack.patches[0].field += 1),
            ("改修补目标", |c| {
                c.morestack.patches[0].glue = "context_restore".to_owned()
            }),
            ("改 main 返回字节", |c| c.main_return.bytes[0] ^= 1),
        ];
        for (label, corrupt) in corruptions {
            let mut bad = contract.clone();
            corrupt(&mut bad);
            assert!(verify_coroutine_code(&bad).is_err(), "{label}必须被拒");
        }
    }

    use crate::backend::x64::fragments::RelocKind;

    /// GC 配置由数据片段承载、访问器片段用 PC 相对 `lea` 取它的地址：记录字节与
    /// `GcImageConfig::canonical_bytes` 同源，访问器只有一个 PC 相对修补点。
    #[test]
    fn gc_config_fragments_carry_the_record_and_its_address() {
        let target = crate::TargetName::X86_64Linux;
        let blobs = registered_fragments(target);
        let by_glue = |glue: &str| {
            blobs
                .iter()
                .find(|blob| blob.entries.iter().any(|entry| entry.glue == glue))
                .expect("glue 片段已登记")
        };
        let config = crate::runtime::GcImageConfig::build();
        let record = by_glue("gc_config_record");
        assert_eq!(record.bytes, config.canonical_bytes());
        assert_eq!(
            u64::try_from(record.bytes.len()).expect("记录长度适配 u64"),
            config.byte_len
        );
        let accessor = by_glue("gc_config");
        assert_eq!(accessor.bytes, [0x48, 0x8D, 0x05, 0, 0, 0, 0, 0xC3]);
        assert_eq!(accessor.relocations.len(), 1);
        assert_eq!(accessor.relocations[0].offset, 3);
        assert_eq!(accessor.relocations[0].kind, RelocKind::PcRel32);
    }
}
