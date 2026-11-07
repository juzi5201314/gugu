//! 普通 C 调用包的稳定车道；请求只持有 raw 分配中的副本。
use std::mem::{offset_of, size_of};

/// 与文件请求共享八个额度槽，负 opcode 不与平台文件操作重叠。
pub const FOREIGN_CALL_OPERATION: u64 = u64::MAX;

/// 32字节封送记录末字：栈副本输入、结果回写或裸地址租约校验。
pub const FIXUP_INPUT: u64 = 0;
pub const FIXUP_OUTPUT: u64 = 1;
pub const FIXUP_POINTER: u64 = 2;

#[repr(C, align(16))]
pub struct ForeignCallPacket {
    pub target: u64,
    /// 从 header 之后开始的栈参数字节数，包含 Win64 shadow space，16 字节取整。
    pub stack_bytes: u64,
    pub integers: [u64; 6],
    pub floats: [u64; 8],
    /// rax、rdx、xmm0、xmm1 的原始位。
    pub results: [u64; 4],
    pub error: u64,
    pub reserved: u64,
}

pub const FOREIGN_PACKET_BYTES: usize = size_of::<ForeignCallPacket>();
pub const FOREIGN_PACKET_TARGET: usize = offset_of!(ForeignCallPacket, target);
pub const FOREIGN_PACKET_STACK_BYTES: usize = offset_of!(ForeignCallPacket, stack_bytes);
pub const FOREIGN_PACKET_INTEGERS: usize = offset_of!(ForeignCallPacket, integers);
pub const FOREIGN_PACKET_FLOATS: usize = offset_of!(ForeignCallPacket, floats);
pub const FOREIGN_PACKET_RESULTS: usize = offset_of!(ForeignCallPacket, results);
pub const FOREIGN_PACKET_ERROR: usize = offset_of!(ForeignCallPacket, error);

pub fn layout() -> [u32; 7] {
    [
        FOREIGN_PACKET_BYTES,
        FOREIGN_PACKET_TARGET,
        FOREIGN_PACKET_STACK_BYTES,
        FOREIGN_PACKET_INTEGERS,
        FOREIGN_PACKET_FLOATS,
        FOREIGN_PACKET_RESULTS,
        FOREIGN_PACKET_ERROR,
    ]
    .map(|offset| u32::try_from(offset).expect("C 调用包布局适配 u32"))
}

const _: () = {
    assert!(FOREIGN_PACKET_BYTES == 176);
    assert!(FOREIGN_PACKET_INTEGERS == 16);
    assert!(FOREIGN_PACKET_FLOATS == 64);
    assert!(FOREIGN_PACKET_RESULTS == 128);
    assert!(FOREIGN_PACKET_ERROR == 160);
};
