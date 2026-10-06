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

/// runtime 机器 intrinsic：镜像内 Gugu runtime 与登记机器片段之间的固定机器边界。
///
/// 与 publish 原语同一 `std.runtime.` 命名空间，但形状完全不同：每个操作的实参与结果都由
/// 登记机器片段的固定内部边界决定，操作名即 runtime glue 名（`mangle::mangle_runtime` 的
/// 输入）。按规范路径识别，导入别名和再导出保留同一身份，也不能取地址。全部调用都必须在
/// `unsafe` 块内：寄存器边界、栈布局与控制转移的前置条件由调用方维护，编译器不能证明它们。
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) enum RuntimeMachineIntrinsic {
    ContextSwitch,
    ContextRestore,
    CurrentSlot,
    ProcessExit,
    ReportWrite,
    /// GC 控制原语：除规范路径外还要求调用方来自 runtime 注入源。
    GcConfig,
    /// GC 入口：只由 runtime 注入源调用；保存发起者上下文、切 system stack、
    /// 调用 dispatcher 后再恢复，返回值是 dispatcher 的状态字。
    GcEnter,
    AtomicLoadAcquire,
    AtomicStoreRelease,
    AtomicFetchOrAcqRel,
    AtomicFetchAndAcqRel,
}

impl RuntimeMachineIntrinsic {
    /// 全部操作的稠密登记顺序；`from_path` 按 `name()` 匹配（与 `PlatformIntrinsic` 同法）。
    pub(crate) const ALL: [Self; 11] = [
        Self::ContextSwitch,
        Self::ContextRestore,
        Self::CurrentSlot,
        Self::ProcessExit,
        Self::ReportWrite,
        Self::GcConfig,
        Self::GcEnter,
        Self::AtomicLoadAcquire,
        Self::AtomicStoreRelease,
        Self::AtomicFetchOrAcqRel,
        Self::AtomicFetchAndAcqRel,
    ];

    /// 按规范路径识别 runtime 机器原语；导入别名与再导出保留同一身份。
    pub(in super::super) fn from_path(path: &str) -> Option<Self> {
        let name = path.strip_prefix("std.runtime.")?;
        Self::ALL.into_iter().find(|kind| kind.name() == name)
    }

    /// 操作名（即 runtime glue 名）；诊断与 dump 使用它。
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::ContextSwitch => "context_switch",
            Self::ContextRestore => "context_restore",
            Self::CurrentSlot => "current_slot",
            Self::ProcessExit => "process_exit",
            Self::ReportWrite => "report_write",
            Self::GcConfig => "gc_config",
            Self::GcEnter => "gc_enter",
            Self::AtomicLoadAcquire => "atomic_load_acquire",
            Self::AtomicStoreRelease => "atomic_store_release",
            Self::AtomicFetchOrAcqRel => "atomic_fetch_or_acq_rel",
            Self::AtomicFetchAndAcqRel => "atomic_fetch_and_acq_rel",
        }
    }

    /// 返回该原语的值实参数量。
    pub(crate) const fn value_arguments(self) -> usize {
        match self {
            Self::ContextSwitch => 4,
            Self::ContextRestore => 3,
            Self::CurrentSlot => 0,
            Self::ProcessExit => 1,
            Self::ReportWrite => 3,
            Self::GcConfig => 0,
            Self::GcEnter => 1,
            Self::AtomicLoadAcquire => 1,
            Self::AtomicStoreRelease | Self::AtomicFetchOrAcqRel | Self::AtomicFetchAndAcqRel => 2,
        }
    }

    /// 判断该原语的返回类型是否是机器字 `uint`；其余不返回值。
    pub(crate) const fn yields_word(self) -> bool {
        matches!(
            self,
            Self::CurrentSlot | Self::ReportWrite | Self::GcConfig | Self::GcEnter
        )
    }

    /// 该原语是否只接受由 `RuntimeResources` 以 `Runtime` 角色注入的源身份。
    ///
    /// GC 控制原语按注入身份授权，而不只是规范路径：普通源码即使写 `unsafe` 也不能调用；
    /// 其余机器原语保持既有契约（按规范路径识别）。
    pub(crate) const fn requires_runtime_source(self) -> bool {
        matches!(self, Self::GcConfig | Self::GcEnter) || self.is_atomic()
    }

    /// 这四个原语直接降到 Atomic，不生成 runtime 调用或 safepoint。
    pub(crate) const fn is_atomic(self) -> bool {
        matches!(
            self,
            Self::AtomicLoadAcquire
                | Self::AtomicStoreRelease
                | Self::AtomicFetchOrAcqRel
                | Self::AtomicFetchAndAcqRel
        )
    }
}

impl std::fmt::Display for RuntimeMachineIntrinsic {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.name())
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

    /// 需要向 runtime 返回的机器字：reserve 返回基址、commit 返回平台状态，wake 返回计数。
    pub(crate) const fn yields_range(self) -> bool {
        matches!(self, Self::ReserveAligned)
    }

    pub(crate) const fn yields_count(self) -> bool {
        matches!(self, Self::Commit | Self::Wake)
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
