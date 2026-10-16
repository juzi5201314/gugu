//! `morestack_or_poll` 片段的真实 CPU 往返：保存/恢复参数寄存器、required 计算与系统侧交接。
//!
//! 宿主 VM 与裸汇编仅存在于此验收二进制；compiler 保持 `forbid(unsafe_code)`。场景按契约
//! 修补点自行给片段打补丁后执行，`context_switch` 位置用宿主 stub 顶替：stub 记录四个参数
//! 并故意破坏九个整数参数寄存器与八个浮点参数寄存器，续点必须从 cold scratch 完整恢复。

use gugu_compiler::{CompileRequest, Compiler, EMBEDDED_ARTIFACTS, TargetName};
use std::arch::naked_asm;
use std::mem::offset_of;

/// 邮箱 104 字节，观测区紧跟其后；stub 的 rbx 指向 mailbox + 40，因此相对偏移是 64。
const LOG_OFFSET: usize = 104;
/// 观测区字数：交接次数与四个交接参数。
const LOG_WORDS: usize = 5;
/// 冷记录区字数（含 `morestack_scratch`），远大于契约声明字节数。
const COLD_WORDS: usize = 192;
/// 九个整数参数寄存器的哨兵，顺序与 `INTERNAL_INTEGER` 一致：`rax rbx rcx rdx rdi rsi r8 r9 r10`。
const SENTINELS: [u64; 9] = [
    0x1111_1111_1111_1111,
    0x2222_2222_2222_2222,
    0x3333_3333_3333_3333,
    0x4444_4444_4444_4444,
    0x5555_5555_5555_5555,
    0x6666_6666_6666_6666,
    0x8888_8888_8888_8888,
    0x9999_9999_9999_9999,
    0xaaaa_aaaa_aaaa_aaaa,
];

#[repr(C, align(16))]
struct State {
    /// 片段入口地址（`call [r12 + entry]`）。
    entry: usize,
    /// 入口 `rsp`（`call` 前一刻）。
    entry_rsp: u64,
    /// `r11` 传入的候选低水位。
    candidate: u64,
    /// `call` 之后的首地址；片段即把同一值写进 scratch。
    expected_return: u64,
    /// 传入的 `r14`/`r15` 哨兵。
    slot: u64,
    processor: u64,
    /// 浮点哨兵与回读。
    xmm_sentinel: [u128; 8],
    xmm_after: [u128; 8],
    /// 九个整数参数寄存器回读，顺序与 `INTERNAL_INTEGER` 一致。
    gpr_after: [u64; 9],
}

/// 片段、mailbox、冷记录、`context_switch` 替身都放进同一映射：RIP 相对修补只有 ±2GiB 窗口。
const CODE_PAGE: usize = 0x0000;
const MAILBOX_PAGE: usize = 0x2000;
const STUB_PAGE: usize = 0x3000;
const COLD_PAGE: usize = 0x4000;
/// 替身被整体搬进映射，按上限复制字节（其后紧跟 `ret`，多余字节不会执行）。
const STUB_COPY_BYTES: usize = 256;

/// 裸适配器：设置 sentinel/`r14`/`r15`/`r11`，调用片段，再把九个整数参数寄存器与八个浮点
/// 参数寄存器写回观测区。全程以 `r12` 为观测基址，与片段保存的寄存器互不干扰。
///
/// # Safety
/// `state` 必须是按本文件修补后、当前契约的可执行片段所在的独占 State；宿主栈充足
/// （片段只在 `rsp` 以下 8 字节写 return PC，由 stub 的 `ret` 回收）。
#[unsafe(naked)]
unsafe extern "sysv64" fn run_morestack(_state: *mut State) {
    naked_asm!(
        "push rbx",
        "push rbp",
        "push r12",
        "push r13",
        "push r14",
        "push r15",
        "sub rsp, 8",
        "mov r12, rdi",
        "mov [r12 + {entry_rsp}], rsp",
        "mov [r12 + {candidate}], rdx",
        "lea rax, [rip + 2f]",
        "mov [r12 + {expected_return}], rax",
        "mov rax, {s0}", "mov rbx, {s1}", "mov rcx, {s2}", "mov rdx, {s3}",
        "mov rdi, {s4}", "mov rsi, {s5}", "mov r8, {s6}", "mov r9, {s7}", "mov r10, {s8}",
        "movups xmm0, [r12 + {xmm_sentinel} + 0]",
        "movups xmm1, [r12 + {xmm_sentinel} + 16]",
        "movups xmm2, [r12 + {xmm_sentinel} + 32]",
        "movups xmm3, [r12 + {xmm_sentinel} + 48]",
        "movups xmm4, [r12 + {xmm_sentinel} + 64]",
        "movups xmm5, [r12 + {xmm_sentinel} + 80]",
        "movups xmm6, [r12 + {xmm_sentinel} + 96]",
        "movups xmm7, [r12 + {xmm_sentinel} + 112]",
        "mov r14, [r12 + {slot}]",
        "mov r15, [r12 + {processor}]",
        "mov r11, [r12 + {candidate}]",
        "call qword ptr [r12 + {entry}]",
        "2:",
        "mov [r12 + {gpr} + 0], rax",
        "mov [r12 + {gpr} + 8], rbx",
        "mov [r12 + {gpr} + 16], rcx",
        "mov [r12 + {gpr} + 24], rdx",
        "mov [r12 + {gpr} + 32], rdi",
        "mov [r12 + {gpr} + 40], rsi",
        "mov [r12 + {gpr} + 48], r8",
        "mov [r12 + {gpr} + 56], r9",
        "mov [r12 + {gpr} + 64], r10",
        "movups [r12 + {xmm_after} + 0], xmm0",
        "movups [r12 + {xmm_after} + 16], xmm1",
        "movups [r12 + {xmm_after} + 32], xmm2",
        "movups [r12 + {xmm_after} + 48], xmm3",
        "movups [r12 + {xmm_after} + 64], xmm4",
        "movups [r12 + {xmm_after} + 80], xmm5",
        "movups [r12 + {xmm_after} + 96], xmm6",
        "movups [r12 + {xmm_after} + 112], xmm7",
        "add rsp, 8",
        "pop r15",
        "pop r14",
        "pop r13",
        "pop r12",
        "pop rbp",
        "pop rbx",
        "ret",
        entry = const offset_of!(State, entry),
        entry_rsp = const offset_of!(State, entry_rsp),
        candidate = const offset_of!(State, candidate),
        expected_return = const offset_of!(State, expected_return),
        slot = const offset_of!(State, slot),
        processor = const offset_of!(State, processor),
        gpr = const offset_of!(State, gpr_after),
        xmm_sentinel = const offset_of!(State, xmm_sentinel),
        xmm_after = const offset_of!(State, xmm_after),
        s0 = const SENTINELS[0], s1 = const SENTINELS[1], s2 = const SENTINELS[2],
        s3 = const SENTINELS[3], s4 = const SENTINELS[4], s5 = const SENTINELS[5],
        s6 = const SENTINELS[6], s7 = const SENTINELS[7], s8 = const SENTINELS[8],
    );
}

/// 顶替 `context_switch`：记录四个内部 ABI 参数（`rax`/`rbx`/`rcx`/`rdx`）到邮箱之后，
/// 破坏九个整数参数寄存器与八个浮点参数寄存器，然后把控制权交回续点。
///
/// # Safety
/// 只由片段在 `rbx = mailbox + 40` 时调用；观测区已映射。
#[unsafe(naked)]
unsafe extern "sysv64" fn switch_stub() {
    naked_asm!(
        "inc qword ptr [rbx + 64]",
        "mov [rbx + 72], rax",
        "mov [rbx + 80], rbx",
        "mov [rbx + 88], rcx",
        "mov [rbx + 96], rdx",
        "mov rax, 0xdead",
        "mov rbx, 0xdead",
        "mov rcx, 0xdead",
        "mov rdx, 0xdead",
        "mov rdi, 0xdead",
        "mov r8, 0xdead",
        "mov r9, 0xdead",
        "mov r10, 0xdead",
        "mov rsi, 0xdead",
        "pcmpeqd xmm0, xmm0",
        "pcmpeqd xmm1, xmm1",
        "pcmpeqd xmm2, xmm2",
        "pcmpeqd xmm3, xmm3",
        "pcmpeqd xmm4, xmm4",
        "pcmpeqd xmm5, xmm5",
        "pcmpeqd xmm6, xmm6",
        "pcmpeqd xmm7, xmm7",
        "ret",
    );
}

/// 宿主 VM 映射与裸适配器；与 x64_encoding bench 共用同一份实现。
#[path = "support/vm.rs"]
mod vm;

fn main() {
    let target = if cfg!(target_os = "windows") {
        TargetName::X86_64Windows
    } else {
        TargetName::X86_64Linux
    };
    let compilation =
        Compiler::with_artifacts(&EMBEDDED_ARTIFACTS).compile(CompileRequest::single_file(
            "main.gg",
            "fn main() { let child = async { 42 }\n _ = child }",
            target,
        ));
    assert!(
        compilation.is_success(),
        "{:?}",
        compilation.diagnostics().items()
    );
    let contract = compilation
        .image_plan()
        .expect("镜像计划")
        .coroutine_runtime();
    let morestack = &contract.morestack;
    let mailbox_record = contract
        .records
        .iter()
        .find(|record| record.name == "MorestackMailbox")
        .expect("mailbox 记录布局");
    assert!(
        usize::try_from(mailbox_record.bytes).expect("mailbox 字节") <= LOG_OFFSET,
        "观测区紧跟 mailbox，不与契约字段重叠"
    );
    assert!(
        LOG_OFFSET + LOG_WORDS * 8 <= 0x1000,
        "bench mailbox 页容得下契约 mailbox 与观测区"
    );
    let cold_record = contract
        .records
        .iter()
        .find(|record| record.name == "CoroutineCold")
        .expect("冷记录布局");
    assert!(
        usize::try_from(cold_record.bytes).expect("冷记录字节") <= COLD_WORDS * 8,
        "bench 冷记录区容得下契约字节数"
    );
    let field = |name: &str| {
        usize::try_from(
            cold_record
                .fields
                .iter()
                .find(|field| field.name == name)
                .unwrap_or_else(|| panic!("冷记录字段 {name}"))
                .offset,
        )
        .expect("字段偏移非负")
    };
    let scratch_offset = field("morestack_scratch");
    let context_offset = field("context");
    let mut state = Box::new(State {
        entry: 0,
        entry_rsp: 0,
        candidate: 0,
        expected_return: 0,
        slot: 0x1234_5678_9abc,
        processor: 0x5678_9abc_def0,
        xmm_sentinel: [
            0x0102_0304_0506_0708_090a_0b0c_0d0e_0f10,
            0x1112_1314_1516_1718_191a_1b1c_1d1e_1f20,
            0x2122_2324_2526_2728_292a_2b2c_2d2e_2f30,
            0x3132_3334_3536_3738_393a_3b3c_3d3e_3f40,
            0x4142_4344_4546_4748_494a_4b4c_4d4e_4f50,
            0x5152_5354_5556_5758_595a_5b5c_5d5e_5f60,
            0x6162_6364_6566_6768_696a_6b6c_6d6e_6f70,
            0x7172_7374_7576_7778_797a_7b7c_7d7e_7f80,
        ],
        xmm_after: [0; 8],
        gpr_after: [0; 9],
    });
    // 片段、mailbox、冷记录、替身同处一个映射：RIP 相对修补只有 ±2GiB 窗口。
    let region = vm::reserve(COLD_PAGE + 0x1000);
    // SAFETY: 四个页偏移都落在刚预留的映射内。
    let (code_base, mailbox_base, stub_base, cold_base) = unsafe {
        (
            region.base.add(CODE_PAGE),
            region.base.add(MAILBOX_PAGE),
            region.base.add(STUB_PAGE),
            region.base.add(COLD_PAGE),
        )
    };
    for page in [code_base, mailbox_base, stub_base, cold_base] {
        vm::protect(page, 0x1000, false);
    }
    state.entry = code_base.addr();
    // SAFETY: 各页独占且尚未转 RX；替身是无 RIP 相对引用的位置无关代码，整段搬运后原地有效。
    unsafe {
        std::ptr::copy_nonoverlapping(morestack.bytes.as_ptr(), code_base, morestack.bytes.len());
        std::ptr::copy_nonoverlapping(switch_stub as *const u8, stub_base, STUB_COPY_BYTES);
    }
    for patch in &morestack.patches {
        let at = usize::try_from(patch.field).expect("修补字段偏移");
        let (target, addend) = match patch.glue.as_str() {
            "morestack_mailbox" => (mailbox_base.addr(), i64::from(patch.addend)),
            "context_switch" => (stub_base.addr(), 0),
            other => panic!("未登记的修补目标 {other}"),
        };
        let disp = i64::try_from(target).expect("目标地址") + addend
            - i64::try_from(code_base.addr() + at + 4).expect("字段末尾");
        let disp = i32::try_from(disp).expect("PC 相对位移").to_le_bytes();
        // SAFETY: 修补字段落在已复制的片段字节内，地址按 4 字节对齐写入。
        unsafe { std::ptr::copy_nonoverlapping(disp.as_ptr(), code_base.add(at), disp.len()) };
    }
    for page in [code_base, stub_base] {
        vm::protect(page, 0x1000, true);
    }
    // mailbox：cold 指针与系统侧 context 装载区由宿主发布，其余字段由片段填写。
    let mailbox_word = |index: usize| {
        // SAFETY: mailbox 页已映射可写，索引落在契约声明的字节数内。
        unsafe { mailbox_base.cast::<u64>().add(index).read_unaligned() }
    };
    let log_word = |index: usize| {
        // SAFETY: 观测区在 mailbox 页内且紧随契约声明字节数之后。
        unsafe {
            mailbox_base
                .add(LOG_OFFSET + index * 8)
                .cast::<u64>()
                .read_unaligned()
        }
    };
    // SAFETY: mailbox 页已映射可写，字段索引落在契约声明范围内。
    unsafe {
        mailbox_base
            .cast::<u64>()
            .add(4)
            .write_unaligned(cold_base.addr() as u64);
        mailbox_base
            .cast::<u64>()
            .add(5)
            .write_unaligned(mailbox_base.addr() as u64 + 40);
    }
    state.candidate = cold_base.addr() as u64 + 0x2000;
    // SAFETY: 片段已修补为可执行；State 独占且地址稳定（Box）；片段不改宿主栈布局。
    unsafe { run_morestack(state.as_mut()) };
    assert_eq!(log_word(0), 1, "系统侧交接恰好一次");
    assert_eq!(
        log_word(1),
        cold_base.addr() as u64 + context_offset as u64,
        "交接参数指向冷记录内 context 字段"
    );
    assert_eq!(
        log_word(2),
        mailbox_base.addr() as u64 + 40,
        "系统侧 context 装载区"
    );
    assert_eq!(log_word(3), state.slot, "现场切换到 slot");
    assert_eq!(log_word(4), state.processor, "现场切换到 processor");
    assert_eq!(mailbox_word(0), state.slot, "mailbox 记录 hot");
    assert_eq!(
        mailbox_word(1),
        state.entry_rsp - 8,
        "mailbox 记录片段入口 rsp（call 已压入 return PC）"
    );
    assert_eq!(mailbox_word(2), state.candidate, "mailbox 记录候选低水位");
    assert_eq!(
        mailbox_word(3),
        state.entry_rsp - state.candidate,
        "required = entry_rsp + 8 - candidate"
    );
    let scratch = cold_base.addr() + scratch_offset;
    // SAFETY: 冷记录区在 State 内且容得下契约声明字节数，scratch 已被片段写入。
    let return_pc = unsafe { (scratch as *const u64).read_unaligned() };
    assert_eq!(return_pc, state.expected_return, "scratch 里的 return PC");
    assert_eq!(
        state.gpr_after, SENTINELS,
        "续点从 scratch 恢复整数参数寄存器"
    );
    assert_eq!(
        state.xmm_after, state.xmm_sentinel,
        "续点从 scratch 恢复八个浮点参数寄存器"
    );
    println!(
        "morestack-entry: 片段{}字节；续点偏移{}；required={}；往返恢复通过",
        morestack.bytes.len(),
        morestack.resume_offset,
        mailbox_word(3)
    );
}
