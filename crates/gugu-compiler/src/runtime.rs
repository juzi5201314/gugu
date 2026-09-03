use crate::target::{Rt0Kind, TargetName};

const STD_PRELUDE_SOURCE: &str = include_str!("../resources/std/prelude.gg");
const RUNTIME_CORE_SOURCE: &str = include_str!("../resources/runtime/core.gg");

/// 登记的 runtime 源文件角色。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeSourceRole {
    /// Gugu 标准库源文件。
    StandardLibrary,
    /// Gugu runtime 源文件。
    Runtime,
}

/// 一个嵌入 compiler 的 Gugu 源文件单元。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RuntimeSource {
    logical_path: &'static str,
    source: &'static str,
    role: RuntimeSourceRole,
}

impl RuntimeSource {
    /// 返回 package 内的规范逻辑路径。
    pub fn logical_path(&self) -> &'static str {
        self.logical_path
    }

    /// 返回源文件内容。
    pub fn source(&self) -> &'static str {
        self.source
    }

    /// 返回源文件角色。
    pub fn role(&self) -> RuntimeSourceRole {
        self.role
    }
}

/// runtime 所需的 compiler-owned intrinsic 边界。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IntrinsicBoundary {
    /// 原子操作。
    Atomic,
    /// 平台内存映射。
    MemoryMapping,
    /// 协程上下文切换。
    StackSwitch,
    /// safepoint 轮询。
    SafepointPoll,
    /// GC 写屏障。
    GcWriteBarrier,
    /// 外部函数交接。
    ForeignBridge,
}

/// 平台入口 rt0 的边界说明。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Rt0Boundary {
    /// Linux syscall 入口。
    LinuxSyscall,
    /// Windows 薄 IAT 入口。
    WindowsThinImport,
}
impl std::fmt::Display for Rt0Boundary {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::LinuxSyscall => "linux-syscall",
            Self::WindowsThinImport => "windows-thin-import",
        })
    }
}

impl From<Rt0Kind> for Rt0Boundary {
    fn from(value: Rt0Kind) -> Self {
        match value {
            Rt0Kind::LinuxSyscall => Self::LinuxSyscall,
            Rt0Kind::WindowsThinImport => Self::WindowsThinImport,
        }
    }
}

/// compiler 使用的 Gugu 标准库/runtime 源树登记表。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RuntimeResources {
    sources: &'static [RuntimeSource],
    intrinsics: &'static [IntrinsicBoundary],
}

impl RuntimeResources {
    /// 返回当前 compiler 构建携带的源树登记。
    pub fn builtin() -> Self {
        Self {
            sources: &[
                RuntimeSource {
                    logical_path: "std/prelude.gg",
                    source: STD_PRELUDE_SOURCE,
                    role: RuntimeSourceRole::StandardLibrary,
                },
                RuntimeSource {
                    logical_path: "runtime/core.gg",
                    source: RUNTIME_CORE_SOURCE,
                    role: RuntimeSourceRole::Runtime,
                },
            ],
            intrinsics: &[
                IntrinsicBoundary::Atomic,
                IntrinsicBoundary::MemoryMapping,
                IntrinsicBoundary::StackSwitch,
                IntrinsicBoundary::SafepointPoll,
                IntrinsicBoundary::GcWriteBarrier,
                IntrinsicBoundary::ForeignBridge,
            ],
        }
    }

    /// 返回已登记的 Gugu 源文件。
    pub fn sources(&self) -> &[RuntimeSource] {
        self.sources
    }

    /// 返回 compiler/runtime 之间允许的 intrinsic 边界。
    pub fn intrinsic_boundaries(&self) -> &[IntrinsicBoundary] {
        self.intrinsics
    }

    pub(crate) fn attach(&self, target: TargetName) -> RuntimeAttachment {
        RuntimeAttachment {
            source_count: self.sources.len() as u32,
            rt0: target.descriptor().rt0.into(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RuntimeAttachment {
    pub(crate) source_count: u32,
    pub(crate) rt0: Rt0Boundary,
}
