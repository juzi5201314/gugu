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

/// A64 retains the shared result/error prefix and admission/fixup protocol.
/// Native stack arguments follow this entire header, not the common prefix.
#[repr(C, align(16))]
pub struct A64ForeignCallPacket {
    pub common: ForeignCallPacket,
    pub integers_x6_x7: [u64; 2],
    pub sret_x8: u64,
    /// FIXUP_OUTPUT destination for d2/d3, when returning a three/four-member HFA.
    pub extra_results: u64,
}

pub const A64_PACKET_BYTES: usize = size_of::<A64ForeignCallPacket>();
pub const A64_PACKET_X6: usize = offset_of!(A64ForeignCallPacket, integers_x6_x7);
pub const A64_PACKET_SRET: usize = offset_of!(A64ForeignCallPacket, sret_x8);
pub const A64_PACKET_EXTRA_RESULTS: usize = offset_of!(A64ForeignCallPacket, extra_results);
pub const A64_EXTRA_RESULT_BYTES: u32 = 16;

pub fn a64_integer_lane(index: u8) -> Option<u32> {
    match index {
        0..=5 => Some((FOREIGN_PACKET_INTEGERS + usize::from(index) * 8) as u32),
        6..=7 => Some((A64_PACKET_X6 + usize::from(index - 6) * 8) as u32),
        _ => None,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct A64Layout {
    pub stack_bytes: u32,
    pub fixups: u32,
    pub bytes: u32,
}

impl A64Layout {
    pub fn new(stack_bytes: u32, fixup_count: u32) -> Option<Self> {
        let stack_bytes = stack_bytes.checked_add(15)? & !15;
        let fixups = (A64_PACKET_BYTES as u32).checked_add(stack_bytes)?;
        let bytes = fixups.checked_add(fixup_count.checked_mul(32)?)?;
        Some(Self {
            stack_bytes,
            fixups,
            bytes,
        })
    }
}

const _: () = {
    assert!(A64_PACKET_BYTES == 208);
    assert!(A64_PACKET_X6 == 176);
    assert!(A64_PACKET_SRET == 192);
    assert!(A64_PACKET_EXTRA_RESULTS == 200);
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a64_header_preserves_the_shared_copy_back_window_and_all_integer_lanes() {
        assert_eq!(offset_of!(A64ForeignCallPacket, common), 0);
        assert_eq!(layout(), [176, 0, 8, 16, 64, 128, 160]);
        assert_eq!(
            (FOREIGN_PACKET_RESULTS, FOREIGN_PACKET_ERROR + 8),
            (128, 168)
        );
        assert_eq!(
            (0..8).map(a64_integer_lane).collect::<Option<Vec<_>>>(),
            Some(vec![16, 24, 32, 40, 48, 56, 176, 184])
        );
        assert_eq!(a64_integer_lane(8), None);
        assert_eq!(A64_PACKET_SRET, 192);
        assert_eq!(A64_PACKET_EXTRA_RESULTS, 200);
        assert_eq!(A64_EXTRA_RESULT_BYTES, 16);
    }

    #[test]
    fn a64_packets_align_stack_arguments_and_check_empty_and_overflow_layouts() {
        assert_eq!(
            A64Layout::new(0, 0),
            Some(A64Layout {
                stack_bytes: 0,
                fixups: 208,
                bytes: 208
            })
        );
        assert_eq!(
            A64Layout::new(33, 3),
            Some(A64Layout {
                stack_bytes: 48,
                fixups: 256,
                bytes: 352
            })
        );
        assert_eq!(A64Layout::new(u32::MAX, 0), None);
        assert_eq!(A64Layout::new(u32::MAX - 15, 0), None);
        assert_eq!(A64Layout::new(0, u32::MAX), None);
    }
}
