//! x86_64 context switch machine intrinsic。直接编码，不调用系统assembler/linker。

use serde::{Deserialize, Serialize};
use std::mem::offset_of;

use super::coroutine::CoroutineContext;

/// 固定内部边界：rdi=保存位置，rsi=恢复位置，rdx=CoroutineHot*，rcx=LogicalProcessor*。
/// 调用前必须完成root spill；r14/r15由恢复方重建。宿主C调用必须另行保持其nonvolatile寄存器。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct ContextSwitchCode {
    /// 内部调用约定的版本。
    pub revision: u32,
    /// switch入口位于0；完成协程从restore入口单向离开，不保存可恢复PC。
    pub restore_offset: u32,
    /// 可直接放入只执行代码区的x86_64指令。
    pub bytes: Vec<u8>,
}

impl ContextSwitchCode {
    pub(crate) fn fixed() -> Self {
        let offset = |value| u8::try_from(value).expect("context的6个机器字均在disp8内");
        let rip = offset(offset_of!(CoroutineContext, rip));
        let rbx = offset(offset_of!(CoroutineContext, rbx));
        let rbp = offset(offset_of!(CoroutineContext, rbp));
        let r12 = offset(offset_of!(CoroutineContext, r12));
        let r13 = offset(offset_of!(CoroutineContext, r13));
        // `rsp` 是 context 首字段：`[rdi]`/`[rsi]` 形式不带位移字节，与 x64 编码器的
        // 「零位移省略」策略一致，因此这片段可与编码器产物逐字节比对。
        let rsp = offset(offset_of!(CoroutineContext, rsp));
        debug_assert_eq!(rsp, 0, "rsp 字段在 context 首位，零位移因此可以省略");
        let mut bytes = Vec::with_capacity(64);
        // LIR 按内部 ABI 传参（rax=save, rbx=restore, rcx=hot, rdx=processor），片段内部沿用
        // 既有约定（rdi=save, rsi=restore, rdx=hot, rcx=processor）：入口先换位。
        bytes.extend_from_slice(&[0x48, 0x89, 0xc7]);
        bytes.extend_from_slice(&[0x48, 0x89, 0xde]);
        bytes.extend_from_slice(&[0x49, 0x89, 0xc8, 0x48, 0x89, 0xd1, 0x4c, 0x89, 0xc2]);
        // lea rax,[rsp+8]；mov [rdi],rax；取调用者return PC。
        bytes.extend_from_slice(&[0x48, 0x8d, 0x44, 0x24, 8, 0x48, 0x89, 0x07]);
        bytes.extend_from_slice(&[0x48, 0x8b, 0x04, 0x24, 0x48, 0x89, 0x47, rip]);
        bytes.extend_from_slice(&[0x48, 0x89, 0x5f, rbx, 0x48, 0x89, 0x6f, rbp]);
        bytes.extend_from_slice(&[0x4c, 0x89, 0x67, r12, 0x4c, 0x89, 0x6f, r13]);
        let restore_body_offset = bytes.len();
        // mov r14,rdx；mov r15,rcx。两目标共用Gugu内部ABI，而非宿主C ABI。
        bytes.extend_from_slice(&[0x49, 0x89, 0xd6, 0x49, 0x89, 0xcf]);
        bytes.extend_from_slice(&[0x48, 0x8b, 0x5e, rbx, 0x48, 0x8b, 0x6e, rbp]);
        bytes.extend_from_slice(&[0x4c, 0x8b, 0x66, r12, 0x4c, 0x8b, 0x6e, r13]);
        bytes.extend_from_slice(&[0x48, 0x8b, 0x26, 0xff, 0x66, rip]);
        // context_switch 保存上下文后直接落入恢复体；独立 context_restore 入口使用
        // 相同的内部 ABI（rax=restore、rbx=hot、rcx=processor），先搬入共享恢复体的寄存器。
        let restore_offset = u32::try_from(bytes.len()).expect("固定片段长度适配u32");
        bytes.extend_from_slice(&[0x48, 0x89, 0xc6, 0x48, 0x89, 0xda, 0xeb]);
        let jump = i8::try_from(
            isize::try_from(restore_body_offset).expect("固定片段长度适配isize")
                - isize::try_from(bytes.len() + 1).expect("固定片段长度适配isize"),
        )
        .expect("恢复体位于短跳范围");
        bytes.extend_from_slice(&jump.to_ne_bytes());
        Self {
            revision: 2,
            restore_offset,
            bytes,
        }
    }
}
