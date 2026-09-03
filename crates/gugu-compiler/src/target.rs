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
        match self {
            Self::X86_64Linux => TargetDescriptor {
                name: self,
                arch: Architecture::X86_64,
                os: OperatingSystem::Linux,
                object_format: ObjectFormat::Elf64,
                pointer_width: 64,
                rt0: Rt0Kind::LinuxSyscall,
            },
            Self::X86_64Windows => TargetDescriptor {
                name: self,
                arch: Architecture::X86_64,
                os: OperatingSystem::Windows,
                object_format: ObjectFormat::Pe32Plus,
                pointer_width: 64,
                rt0: Rt0Kind::WindowsThinImport,
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
}
