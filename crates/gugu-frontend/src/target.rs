use serde::{Deserialize, Serialize};
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

    /// 返回规范目标名。
    pub fn name(self) -> &'static str {
        match self {
            Self::X86_64Linux => "x86_64-linux",
            Self::X86_64Windows => "x86_64-windows",
        }
    }

    /// 返回当前编译器宿主对应的已登记目标。
    pub fn host() -> Option<Self> {
        host_target()
    }

    /// 返回目标的不可变 descriptor。
    pub fn descriptor(self) -> TargetDescriptor {
        let cost_profile = baseline_cost_profile();
        let sysroot = sysroot_entry(self);
        TargetDescriptor {
            name: self,
            arch: Architecture::X86_64,
            os: match self {
                Self::X86_64Linux => OperatingSystem::Linux,
                Self::X86_64Windows => OperatingSystem::Windows,
            },
            object_format: match self {
                Self::X86_64Linux => ObjectFormat::Elf64,
                Self::X86_64Windows => ObjectFormat::Pe32Plus,
            },
            pointer_width: 64,
            rt0: match self {
                Self::X86_64Linux => Rt0Kind::LinuxSyscall,
                Self::X86_64Windows => Rt0Kind::WindowsThinImport,
            },
            page_size: TARGET_PAGE_SIZE,
            cpu_baseline: CpuBaseline::X86_64V1,
            linux_interpreter: sysroot.interpreter,
            sysroot_digest: target_sysroot_digest(self),
            import_policy_revision: IMPORT_POLICY_REVISION,
            runtime_tuning_profile_digest: crate::runtime::scheduler_schema::RUNTIME_TUNING_PROFILE
                .digest(),
            backend_cost_profile_digest: cost_profile.digest(),
            cost_profile,
            pointer_compression: PointerCompression::for_target(self),
        }
    }
}

/// 平台页大小；当前两个目标均为 4 KiB。
pub const TARGET_PAGE_SIZE: u32 = 4096;

/// 导入策略 revision；IAT 只含登记导入库。
pub const IMPORT_POLICY_REVISION: u32 = 1;

/// 后端接受的 CPU 指令集特性；`CpuBaseline` 用它表达目标可接受面。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum CpuFeature {
    /// x86-64 基础指令集。
    X86_64,
    /// SSE2；x86-64-v1 基线包含。
    Sse2,
    /// SSSE3；超出 x86-64-v1。
    Ssse3,
    /// SSE4.1；超出 x86-64-v1。
    Sse41,
    /// AVX；超出 x86-64-v1。
    Avx,
}

/// 登记的全部 CPU 特性；[`CpuFeature::name`] 的取值集合，契约按此校验特性名。
pub const CPU_FEATURES: [CpuFeature; 5] = [
    CpuFeature::X86_64,
    CpuFeature::Sse2,
    CpuFeature::Ssse3,
    CpuFeature::Sse41,
    CpuFeature::Avx,
];

impl CpuFeature {
    /// 返回规范特性名。
    pub fn name(self) -> &'static str {
        match self {
            Self::X86_64 => "x86_64",
            Self::Sse2 => "sse2",
            Self::Ssse3 => "ssse3",
            Self::Sse41 => "sse4.1",
            Self::Avx => "avx",
        }
    }

    /// 按规范特性名反查；未登记的名字返回 `None`。
    pub fn from_name(name: &str) -> Option<Self> {
        CPU_FEATURES
            .into_iter()
            .find(|feature| feature.name() == name)
    }
}

/// 平台登记的 CPU 基线。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum CpuBaseline {
    /// x86-64-v1：接受 `x86_64` 与 `sse2`，拒绝 SSSE3/SSE4.1/AVX。
    X86_64V1,
}

impl CpuBaseline {
    /// 返回规范基线名。
    pub fn name(self) -> &'static str {
        match self {
            Self::X86_64V1 => "x86_64-v1",
        }
    }

    /// 判定基线是否接受该特性。
    pub fn allows(self, feature: CpuFeature) -> bool {
        match self {
            Self::X86_64V1 => matches!(feature, CpuFeature::X86_64 | CpuFeature::Sse2),
        }
    }
}

/// 目标 sysroot 的登记描述；FFI 阶段只扩展这张表，digest 绑定登记顺序。
struct SysrootEntry {
    name: TargetName,
    interpreter: Option<&'static str>,
    import_libraries: &'static [&'static str],
}

const TARGET_SYSROOTS: &[SysrootEntry] = &[
    SysrootEntry {
        name: TargetName::X86_64Linux,
        interpreter: Some("/lib64/ld-linux-x86-64.so.2"),
        import_libraries: &[],
    },
    SysrootEntry {
        name: TargetName::X86_64Windows,
        interpreter: None,
        import_libraries: &["ntdll.dll", "kernel32.dll"],
    },
];

fn sysroot_entry(name: TargetName) -> &'static SysrootEntry {
    TARGET_SYSROOTS
        .iter()
        .find(|entry| entry.name == name)
        .expect("登记目标都有 sysroot 描述")
}

/// 返回目标 sysroot 描述的域隔离内容身份。
pub fn target_sysroot_digest(name: TargetName) -> [u8; 32] {
    let entry = sysroot_entry(name);
    let mut hasher = blake3::Hasher::new_derive_key("gugu-target-sysroot-v1");
    encode_field(&mut hasher, name.name().as_bytes());
    match entry.interpreter {
        Some(interpreter) => {
            hasher.update(&[1]);
            encode_field(&mut hasher, interpreter.as_bytes());
        }
        None => {
            hasher.update(&[0]);
        }
    }
    for library in entry.import_libraries {
        hasher.update(&[2]);
        encode_field(&mut hasher, library.as_bytes());
    }
    hasher.update(&IMPORT_POLICY_REVISION.to_le_bytes());
    *hasher.finalize().as_bytes()
}

/// 追加长度前缀字段；不同描述串接不会产生同一编码。
fn encode_field(hasher: &mut blake3::Hasher, bytes: &[u8]) {
    hasher.update(
        &u64::try_from(bytes.len())
            .expect("字段长度适配 u64")
            .to_le_bytes(),
    );
    hasher.update(bytes);
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
        formatter.write_str(self.name())
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

impl Rt0Kind {
    /// 返回规范边界名。
    pub fn name(self) -> &'static str {
        match self {
            Self::LinuxSyscall => "linux-syscall",
            Self::WindowsThinImport => "windows-thin-import",
        }
    }
}

impl fmt::Display for Rt0Kind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.name())
    }
}

/// 目标对 checked pointer compression 的能力声明。
///
/// 能力是目标属性，不随 profile 变化：`supported` 为 `false` 时任何 cage 契约都被拒绝；
/// 上限与对齐来自目标的地址空间与 GC arena 布局，`canonical_bits` 是解码结果的合法
/// canonical 位宽。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PointerCompression {
    /// 目标是否支持 checked pointer compression。
    pub supported: bool,
    /// 单个 cage 的字节上界。
    pub max_cage_bytes: u64,
    /// cage 基址与尺寸必须满足的最小对齐。
    pub min_alignment: u64,
    /// 可用地址的 canonical 位宽；解码地址必须落在该位宽的正半区。
    pub canonical_bits: u8,
}

impl PointerCompression {
    /// 未登记能力：不支持任何 cage；测试用它驱动能力拒绝路径。
    pub const fn unsupported() -> Self {
        Self {
            supported: false,
            max_cage_bytes: 0,
            min_alignment: 1,
            canonical_bits: 0,
        }
    }

    /// x86_64 双目标共用能力：≤4 GiB cage、2 MiB 粒度、48 位 canonical。
    pub const fn x86_64() -> Self {
        Self {
            supported: true,
            max_cage_bytes: 1 << 32,
            min_alignment: 2 * 1024 * 1024,
            canonical_bits: 48,
        }
    }

    /// 按目标名返回能力。
    pub const fn for_target(name: TargetName) -> Self {
        match name {
            TargetName::X86_64Linux | TargetName::X86_64Windows => Self::x86_64(),
        }
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
    /// 平台页大小。
    pub page_size: u32,
    /// CPU 基线：instruction verifier 的可接受面。
    pub cpu_baseline: CpuBaseline,
    /// 动态链接解释器；静态目标为 `None`。
    pub linux_interpreter: Option<&'static str>,
    /// 目标 sysroot 描述的内容身份。
    pub sysroot_digest: [u8; 32],
    /// 导入策略 revision。
    pub import_policy_revision: u32,
    /// 调度调优 profile 的内容身份。
    pub runtime_tuning_profile_digest: [u8; 32],
    /// 后端成本 profile 的内容身份。
    pub backend_cost_profile_digest: [u8; 32],
    /// 后端成本基线，供内联、循环与向量化策略消费。
    pub cost_profile: BackendCostProfile,
    /// checked pointer compression 能力。
    pub pointer_compression: PointerCompression,
}

impl TargetDescriptor {
    /// 返回 descriptor 的域隔离内容身份。
    ///
    /// 编码顺序固定为字段声明顺序；成本 profile 直接编码其内容身份，
    /// `pointer_compression` 作为后端可接受面的一部分直接编码。
    pub fn digest(&self) -> [u8; 32] {
        let mut hasher = blake3::Hasher::new_derive_key("gugu-target-descriptor-v1");
        encode_field(&mut hasher, self.name.name().as_bytes());
        encode_field(&mut hasher, arch_name(self.arch).as_bytes());
        encode_field(&mut hasher, os_name(self.os).as_bytes());
        encode_field(&mut hasher, format_name(self.object_format).as_bytes());
        hasher.update(&[self.pointer_width]);
        encode_field(&mut hasher, self.rt0.name().as_bytes());
        hasher.update(&self.page_size.to_le_bytes());
        encode_field(&mut hasher, self.cpu_baseline.name().as_bytes());
        match self.linux_interpreter {
            Some(interpreter) => {
                hasher.update(&[1]);
                encode_field(&mut hasher, interpreter.as_bytes());
            }
            None => {
                hasher.update(&[0]);
            }
        }
        hasher.update(&self.sysroot_digest);
        hasher.update(&self.import_policy_revision.to_le_bytes());
        hasher.update(&self.runtime_tuning_profile_digest);
        hasher.update(&self.cost_profile.digest());
        hasher.update(&[u8::from(self.pointer_compression.supported)]);
        hasher.update(&self.pointer_compression.max_cage_bytes.to_le_bytes());
        hasher.update(&self.pointer_compression.min_alignment.to_le_bytes());
        hasher.update(&[self.pointer_compression.canonical_bits]);
        *hasher.finalize().as_bytes()
    }
}

fn arch_name(arch: Architecture) -> &'static str {
    match arch {
        Architecture::X86_64 => "x86_64",
    }
}

fn os_name(os: OperatingSystem) -> &'static str {
    match os {
        OperatingSystem::Linux => "linux",
        OperatingSystem::Windows => "windows",
    }
}

fn format_name(format: ObjectFormat) -> &'static str {
    match format {
        ObjectFormat::Elf64 => "elf64",
        ObjectFormat::Pe32Plus => "pe32+",
    }
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

impl BackendCostProfile {
    /// 返回成本 profile 的域隔离内容身份。
    pub fn digest(&self) -> [u8; 32] {
        let mut hasher = blake3::Hasher::new_derive_key("gugu-backend-cost-profile-v1");
        hasher.update(&self.baseline_digest);
        hasher.update(&self.inline_hot_bytes.to_le_bytes());
        hasher.update(&self.inline_cold_bytes.to_le_bytes());
        hasher.update(&self.code_size_budget.to_le_bytes());
        hasher.update(&self.max_spill_slots.to_le_bytes());
        hasher.update(&self.max_spill_bytes_per_call.to_le_bytes());
        hasher.update(&self.max_reload_stores.to_le_bytes());
        hasher.update(&self.regression_percent.to_le_bytes());
        hasher.update(&[u8::from(self.vector_lowering)]);
        *hasher.finalize().as_bytes()
    }
}
