use std::fmt;

/// Gugu 当前登记的目标名称。
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum TargetName {
    /// x86_64 Linux，使用 ELF64 与 System V AMD64 ABI。
    X86_64Linux,
    /// x86_64 Windows，使用 PE32+ 与 Microsoft x64 ABI。
    X86_64Windows,
}

impl TargetName {
    /// 从规范目标名解析目标。
    pub fn parse(value: &str) -> Result<Self, TargetParseError> {
        match value {
            "x86_64-linux" => Ok(Self::X86_64Linux),
            "x86_64-windows" => Ok(Self::X86_64Windows),
            _ => Err(TargetParseError {
                requested: value.to_owned(),
            }),
        }
    }

    /// 返回当前编译器宿主对应的已登记目标。
    pub fn host() -> Option<Self> {
        host_target()
    }

    /// 返回目标的不可变 descriptor。
    pub fn descriptor(self) -> TargetDescriptor {
        let cost_profile = baseline_cost_profile();
        match self {
            Self::X86_64Linux => TargetDescriptor {
                name: self,
                arch: Architecture::X86_64,
                os: OperatingSystem::Linux,
                object_format: ObjectFormat::Elf64,
                pointer_width: 64,
                rt0: Rt0Kind::LinuxSyscall,
                cost_profile,
            },
            Self::X86_64Windows => TargetDescriptor {
                name: self,
                arch: Architecture::X86_64,
                os: OperatingSystem::Windows,
                object_format: ObjectFormat::Pe32Plus,
                pointer_width: 64,
                rt0: Rt0Kind::WindowsThinImport,
                cost_profile,
            },
        }
    }
}

#[cfg(all(target_arch = "x86_64", target_os = "linux"))]
fn host_target() -> Option<TargetName> {
    Some(TargetName::X86_64Linux)
}

#[cfg(all(target_arch = "x86_64", target_os = "windows"))]
fn host_target() -> Option<TargetName> {
    Some(TargetName::X86_64Windows)
}

#[cfg(not(any(
    all(target_arch = "x86_64", target_os = "linux"),
    all(target_arch = "x86_64", target_os = "windows"),
)))]
fn host_target() -> Option<TargetName> {
    None
}

impl fmt::Display for TargetName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::X86_64Linux => "x86_64-linux",
            Self::X86_64Windows => "x86_64-windows",
        })
    }
}

/// 目标解析失败。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TargetParseError {
    requested: String,
}

impl TargetParseError {
    /// 返回未登记的原始目标名。
    pub fn requested(&self) -> &str {
        &self.requested
    }
}

impl fmt::Display for TargetParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "未登记的目标 `{}`", self.requested)
    }
}

impl std::error::Error for TargetParseError {}

/// 已登记目标的架构。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Architecture {
    /// x86-64 指令集架构。
    X86_64,
}

/// 已登记目标的操作系统。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OperatingSystem {
    /// Linux。
    Linux,
    /// Windows。
    Windows,
}

/// 目标镜像格式。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ObjectFormat {
    /// ELF64。
    Elf64,
    /// PE32+。
    Pe32Plus,
}

/// 平台 rt0 的边界类型。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Rt0Kind {
    /// 通过 Linux syscall 启动。
    LinuxSyscall,
    /// 通过 ntdll/kernel32 薄导入启动。
    WindowsThinImport,
}

impl fmt::Display for Rt0Kind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::LinuxSyscall => "linux-syscall",
            Self::WindowsThinImport => "windows-thin-import",
        })
    }
}

/// 编译 action 使用的不可变目标描述。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TargetDescriptor {
    /// 规范目标名。
    pub name: TargetName,
    /// 指令集架构。
    pub arch: Architecture,
    /// 操作系统。
    pub os: OperatingSystem,
    /// 目标镜像格式。
    pub object_format: ObjectFormat,
    /// 指针宽度。
    pub pointer_width: u8,
    /// rt0 边界。
    pub rt0: Rt0Kind,
    /// 后端成本基线，供内联、循环与向量化策略消费。
    pub cost_profile: BackendCostProfile,
}

/// 后端成本基线：内联与向量化策略共用的校准输入。
///
/// `baseline_digest` 绑定成本模型的来源；未校准的目标使用
/// [`baseline_cost_profile`] 给出的保守取值。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BackendCostProfile {
    /// 成本模型基线的内容身份。
    pub baseline_digest: [u8; 32],
    /// 热路径内联的字节上界。
    pub inline_hot_bytes: u32,
    /// 冷路径内联的字节上界。
    pub inline_cold_bytes: u32,
    /// 单个函数体的代码尺寸预算。
    pub code_size_budget: u32,
    /// 一次调用允许的溢出槽数量。
    pub max_spill_slots: u32,
    /// 一次调用的溢出字节上界。
    pub max_spill_bytes_per_call: u32,
    /// 允许的重载存储数量。
    pub max_reload_stores: u32,
    /// 允许的吞吐回退百分比。
    pub regression_percent: u16,
    /// 后端是否已提供向量 lowering；未校准时为 `false`。
    pub vector_lowering: bool,
}

/// 返回未校准目标共用的保守成本基线。
pub fn baseline_cost_profile() -> BackendCostProfile {
    let mut hasher = blake3::Hasher::new_derive_key("gugu-backend-cost-baseline-v1");
    hasher.update(b"x86_64-v1");
    BackendCostProfile {
        baseline_digest: *hasher.finalize().as_bytes(),
        inline_hot_bytes: 256,
        inline_cold_bytes: 128,
        code_size_budget: 4096,
        max_spill_slots: 64,
        max_spill_bytes_per_call: 512,
        max_reload_stores: 256,
        regression_percent: 5,
        vector_lowering: false,
    }
}
