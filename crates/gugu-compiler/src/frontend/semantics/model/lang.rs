//! 标准原语按规范路径识别，导入别名和再导出保留同一身份。
use super::{Model, ResolvedTarget};

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) enum MemoryIntrinsic {
    AddrOf,
    PtrRead,
    PtrWrite,
    ReadUnaligned,
    WriteUnaligned,
    VolatileLoad,
    VolatileStore,
    Transmute,
    Unreachable,
    Uninit,
    UninitNew,
    UninitAsPtr,
    UninitWrite,
    AssumeInit,
    PointerCast,
    ScalarCast,
}

impl MemoryIntrinsic {
    pub(in super::super) fn from_path(path: &str) -> Option<Self> {
        Some(match path {
            "std.ptr.addr_of" => Self::AddrOf,
            "std.ptr.ptr_read" => Self::PtrRead,
            "std.ptr.ptr_write" => Self::PtrWrite,
            "std.ptr.read_unaligned" => Self::ReadUnaligned,
            "std.ptr.write_unaligned" => Self::WriteUnaligned,
            "std.ptr.volatile_load" => Self::VolatileLoad,
            "std.ptr.volatile_store" => Self::VolatileStore,
            "std.mem.transmute" => Self::Transmute,
            "std.hint.unreachable" => Self::Unreachable,
            "std.mem.MaybeUninit.uninit" => Self::Uninit,
            "std.mem.MaybeUninit.new" => Self::UninitNew,
            "std.mem.MaybeUninit.as_ptr" => Self::UninitAsPtr,
            "std.mem.MaybeUninit.write" => Self::UninitWrite,
            "std.mem.MaybeUninit.assume_init" => Self::AssumeInit,
            _ => return None,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) enum RuntimeIntrinsic {
    OwnershipPublish,
    RootPublish,
}

impl RuntimeIntrinsic {
    pub(in super::super) fn from_path(path: &str) -> Option<Self> {
        Some(match path {
            "std.runtime.ownership_publish" => Self::OwnershipPublish,
            "std.runtime.root_publish" => Self::RootPublish,
            _ => return None,
        })
    }
}

/// 平台范围原语的固定操作集合；与 `PlatformRangeSchemaV1` 的操作目录一一对应。
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) enum PlatformIntrinsic {
    ReserveAligned,
    Commit,
    Decommit,
    Release,
    ProtectGuard,
    Unprotect,
    Wait,
    Wake,
    Entropy,
    Zero,
    SetDumpPolicy,
    LowMemoryHint,
    HugePageHint,
}

impl PlatformIntrinsic {
    /// 全部操作的稠密登记顺序；它同时是路径识别的唯一目录，`from_path` 按 `name()` 匹配。
    pub(crate) const ALL: [Self; 13] = [
        Self::ReserveAligned,
        Self::Commit,
        Self::Decommit,
        Self::Release,
        Self::ProtectGuard,
        Self::Unprotect,
        Self::Wait,
        Self::Wake,
        Self::Entropy,
        Self::Zero,
        Self::SetDumpPolicy,
        Self::LowMemoryHint,
        Self::HugePageHint,
    ];

    /// 按规范路径识别平台原语；导入别名与再导出保留同一身份。
    ///
    /// 路径表由 `ALL` 与 `name()` 唯一派生：新增操作只需要登记一处，识别、诊断名与契约目录
    /// 必然同步，不会出现只在某一侧认识的半登记操作。
    pub(in super::super) fn from_path(path: &str) -> Option<Self> {
        let name = path.strip_prefix("std.platform.")?;
        Self::ALL.into_iter().find(|kind| kind.name() == name)
    }

    /// 返回契约中的操作名；诊断与 dump 使用它。
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::ReserveAligned => "reserve_aligned",
            Self::Commit => "commit",
            Self::Decommit => "decommit",
            Self::Release => "release",
            Self::ProtectGuard => "protect_guard",
            Self::Unprotect => "unprotect",
            Self::Wait => "wait",
            Self::Wake => "wake",
            Self::Entropy => "entropy",
            Self::Zero => "zero",
            Self::SetDumpPolicy => "set_dump_policy",
            Self::LowMemoryHint => "low_memory_hint",
            Self::HugePageHint => "huge_page_hint",
        }
    }

    /// 返回该原语的值实参数量。
    pub(crate) const fn value_arguments(self) -> usize {
        match self {
            Self::ReserveAligned | Self::Wait | Self::Wake | Self::SetDumpPolicy => 2,
            Self::Commit
            | Self::Decommit
            | Self::Release
            | Self::ProtectGuard
            | Self::Unprotect
            | Self::Zero
            | Self::HugePageHint
            | Self::Entropy => 1,
            Self::LowMemoryHint => 0,
        }
    }

    /// 判断该原语的返回类型是否是平台 range 编号。
    pub(crate) const fn yields_range(self) -> bool {
        matches!(self, Self::ReserveAligned)
    }

    /// 判断该原语的返回类型是否是唤醒的等待者数量。
    pub(crate) const fn yields_count(self) -> bool {
        matches!(self, Self::Wake)
    }

    /// 判断该原语的返回类型是否是 `bool`。
    pub(crate) const fn yields_bool(self) -> bool {
        matches!(self, Self::LowMemoryHint | Self::Wait)
    }

    /// 判断该原语是否可能阻塞当前协程；只有 `wait` 会睡眠。
    ///
    /// 该判定同时是 HIR 效果位与 LIR safepoint 类别的唯一来源：阻塞原语是挂起点，其余只跨越
    /// syscall/CRT 边界。
    pub(crate) const fn blocking(self) -> bool {
        matches!(self, Self::Wait)
    }

    /// 判断该原语是否改变 range 或字状态；只读查询不推进生命周期。
    pub(crate) const fn mutating(self) -> bool {
        matches!(
            self,
            Self::ReserveAligned
                | Self::Commit
                | Self::Decommit
                | Self::Release
                | Self::ProtectGuard
                | Self::Unprotect
                | Self::Zero
                | Self::SetDumpPolicy
                | Self::HugePageHint
                | Self::LowMemoryHint
        )
    }

    /// 判断该原语是否取得平台 entropy；它返回平台所有的原始字节指针。
    ///
    /// 返回裸指针而不是切片：entropy 缓冲区由平台持有，不是受管对象，调用方也不得释放它。
    pub(crate) const fn yields_raw_bytes(self) -> bool {
        matches!(self, Self::Entropy)
    }
}

impl std::fmt::Display for PlatformIntrinsic {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.name())
    }
}

impl Model<'_> {
    pub(in super::super) fn external_path(&self, module: usize, path: &[&str]) -> Option<String> {
        self.external_path_inner(module, path, false)
    }

    fn external_path_inner(&self, module: usize, path: &[&str], exported: bool) -> Option<String> {
        if path.first() == Some(&"std") {
            return Some(path.join("."));
        }
        let first = path.first()?;
        for import in &self.names.imports {
            if import.module.index() != module
                || import.alias != *first
                || exported && !import.public
            {
                continue;
            }
            match &import.target {
                ResolvedTarget::External(prefix) => {
                    let mut result = prefix.clone();
                    for segment in &path[1..] {
                        result.push('.');
                        result.push_str(segment);
                    }
                    return Some(result);
                }
                ResolvedTarget::Module(target) if path.len() > 1 => {
                    return self.external_path_inner(target.index(), &path[1..], true);
                }
                _ => {}
            }
        }
        None
    }
}
