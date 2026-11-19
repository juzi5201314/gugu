//! Mach-O 64 可执行镜像 writer（arm64 / x86_64）。
//!
//! 纯库：调用方给出节内容、dyld 导入、指针 fixup 与入口，writer 写出带
//! `__PAGEZERO`/`__TEXT`/`__DATA_CONST`/`__DATA`/`__LINKEDIT` 段、chained fixups、
//! `LC_MAIN` 与 linker-signed ad-hoc `LC_CODE_SIGNATURE` 的 `MH_EXECUTE`。布局只取决于
//! 节大小与请求形状：调用方先用 [`layout`] 取得地址修补机器码，再用 [`link`] 写出。

mod commands;
mod fixups;
mod layout;
mod sign;
#[cfg(test)]
mod tests;

pub use layout::{Layout, PlacedSection};

/// 目标架构；决定 CPU 类型与段页大小。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Arch {
    Arm64,
    X86_64,
}

impl Arch {
    /// 段对齐与 chained fixup 的页大小：arm64 内核按 16 KiB 页映射，x86_64 为 4 KiB。
    pub fn page_size(self) -> u64 {
        match self {
            Arch::Arm64 => 0x4000,
            Arch::X86_64 => 0x1000,
        }
    }

    fn cpu(self) -> (u32, u32) {
        match self {
            Arch::Arm64 => (0x0100_000c, 0),
            Arch::X86_64 => (0x0100_0007, 3),
        }
    }
}

/// 节所在段。段集合与权限固定：`__TEXT` 为 r-x，`__DATA_CONST` 在 dyld 修完 fixup
/// 后变为只读，`__DATA` 为 rw-；不存在可写可执行段。
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum Segment {
    Text,
    DataConst,
    Data,
}

impl Segment {
    pub const ALL: [Segment; 3] = [Segment::Text, Segment::DataConst, Segment::Data];

    pub fn name(self) -> &'static str {
        match self {
            Segment::Text => "__TEXT",
            Segment::DataConst => "__DATA_CONST",
            Segment::Data => "__DATA",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SectionContent {
    /// 机器码；只允许在 `__TEXT`。
    Code(Vec<u8>),
    Data(Vec<u8>),
    /// 只占虚拟内存的零初始化区；只允许在 `__DATA`，且排在该段全部有文件内容的节之后。
    ZeroFill(u64),
}

impl SectionContent {
    pub fn size(&self) -> u64 {
        match self {
            SectionContent::Code(bytes) | SectionContent::Data(bytes) => bytes.len() as u64,
            SectionContent::ZeroFill(size) => *size,
        }
    }

    fn bytes(&self) -> Option<&[u8]> {
        match self {
            SectionContent::Code(bytes) | SectionContent::Data(bytes) => Some(bytes),
            SectionContent::ZeroFill(_) => None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Section {
    pub segment: Segment,
    /// 不超过 16 字节，如 `__text`、`__const`、`__got`。
    pub name: String,
    /// 对齐字节数，必须是 2 的幂且不超过段页大小。
    pub align: u64,
    pub content: SectionContent,
}

/// 节内位置：`section` 是 [`MachORequest::sections`] 的下标。
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct Location {
    pub section: usize,
    pub offset: u64,
}

/// 由 dyld 绑定的外部符号。`name` 是 dylib 导出表中的 Mach-O 名字（C 符号带前导 `_`）。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Import {
    pub dylib: String,
    pub name: String,
    pub weak: bool,
}

/// 8 字节指针槽的加载期修正；槽必须位于 `__DATA_CONST` 或 `__DATA` 的有文件内容节内并
/// 8 字节对齐。槽内原有字节被 chained fixup 编码覆盖。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Fixup {
    /// 槽在加载后保存 `target` 的运行时地址。
    Rebase { at: Location, target: Location },
    /// 槽在加载后保存 `imports[import]` 的地址加 `addend`（0..=255）。
    Bind {
        at: Location,
        import: usize,
        addend: u8,
    },
}

impl Fixup {
    pub fn at(&self) -> Location {
        match self {
            Fixup::Rebase { at, .. } | Fixup::Bind { at, .. } => *at,
        }
    }
}

/// 写入 `LC_SYMTAB` 的本地调试符号；不参与 dyld 绑定。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Symbol {
    pub name: String,
    pub at: Location,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MachORequest {
    pub arch: Arch,
    /// CodeDirectory identifier，惯例为输出文件名。
    pub identifier: String,
    pub sections: Vec<Section>,
    pub imports: Vec<Import>,
    pub fixups: Vec<Fixup>,
    pub symbols: Vec<Symbol>,
    /// `LC_MAIN` 入口；必须落在代码节内。
    pub entry: Location,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MachImage {
    pub bytes: Vec<u8>,
    pub fingerprint: [u8; 32],
}

/// main executable 必须链接 libSystem；它固定为 dylib 序号 1。
pub const LIB_SYSTEM: &str = "/usr/lib/libSystem.B.dylib";
/// 镜像首选基址：`__PAGEZERO` 占满低 4 GiB。
pub const IMAGE_BASE: u64 = 0x1_0000_0000;
/// 最低系统版本 12.0：两个架构的 dyld 都支持 chained fixups。
pub const MIN_MACOS: u32 = 12 << 16;

/// 计算全部节与段的地址；同一请求形状（节大小、导入、fixup、符号数）给出同一布局。
pub fn layout(request: &MachORequest) -> Result<Layout, String> {
    validate(request)?;
    Layout::compute(request)
}

/// 写出签名完成的镜像。
pub fn link(request: &MachORequest) -> Result<MachImage, String> {
    let layout = layout(request)?;
    let mut file = vec![0; layout.file_size as usize];
    commands::write_header(&mut file, request, &layout)?;
    for (section, placed) in request.sections.iter().zip(&layout.sections) {
        if let Some(bytes) = section.content.bytes() {
            let at = placed.fileoff as usize;
            file[at..at + bytes.len()].copy_from_slice(bytes);
        }
    }
    fixups::apply(&mut file, request, &layout)?;
    commands::write_linkedit(&mut file, request, &layout)?;
    commands::write_uuid(&mut file, &layout);
    sign::write_signature(&mut file, request, &layout);
    let fingerprint = *blake3::hash(&file).as_bytes();
    Ok(MachImage {
        bytes: file,
        fingerprint,
    })
}

fn validate(request: &MachORequest) -> Result<(), String> {
    check_name("identifier", &request.identifier)?;
    for section in &request.sections {
        validate_section(request.arch, section)?;
    }
    for segment in Segment::ALL {
        let mut zerofill = false;
        for section in request.sections.iter().filter(|s| s.segment == segment) {
            let is_zero = matches!(section.content, SectionContent::ZeroFill(_));
            if zerofill && !is_zero {
                return Err(format!(
                    "Mach-O section {} follows a zero-fill section in {}",
                    section.name,
                    segment.name()
                ));
            }
            zerofill |= is_zero;
        }
    }
    let entry = section_at(request, request.entry, false)?;
    if !matches!(entry.content, SectionContent::Code(_)) {
        return Err("Mach-O entry must be inside a code section".to_owned());
    }
    for import in &request.imports {
        check_name("dylib path", &import.dylib)?;
        check_name("import name", &import.name)?;
    }
    for symbol in &request.symbols {
        check_name("symbol name", &symbol.name)?;
        section_at(request, symbol.at, true)?;
    }
    fixups::validate(request)
}

fn validate_section(arch: Arch, section: &Section) -> Result<(), String> {
    if section.name.is_empty() || section.name.len() > 16 {
        return Err(format!(
            "Mach-O section name {:?} must be 1..=16 bytes",
            section.name
        ));
    }
    if !section.align.is_power_of_two() || section.align > arch.page_size() {
        return Err(format!(
            "Mach-O section {} alignment {} is not a power of two within the page",
            section.name, section.align
        ));
    }
    match (&section.content, section.segment) {
        (SectionContent::Code(_), Segment::Text)
        | (SectionContent::Data(_), _)
        | (SectionContent::ZeroFill(_), Segment::Data) => Ok(()),
        (SectionContent::Code(_), segment) => Err(format!(
            "Mach-O code section {} must live in __TEXT, not {}",
            section.name,
            segment.name()
        )),
        (SectionContent::ZeroFill(_), segment) => Err(format!(
            "Mach-O zero-fill section {} must live in __DATA, not {}",
            section.name,
            segment.name()
        )),
    }
}

/// 位置所在节；`end_inclusive` 允许指向节尾（符号或指针可以指向区间末端）。
fn section_at(
    request: &MachORequest,
    at: Location,
    end_inclusive: bool,
) -> Result<&Section, String> {
    let section = request
        .sections
        .get(at.section)
        .ok_or_else(|| format!("Mach-O location names missing section {}", at.section))?;
    let size = section.content.size();
    let inside = if end_inclusive {
        at.offset <= size
    } else {
        at.offset < size
    };
    if !inside {
        return Err(format!(
            "Mach-O location {:#x} is outside section {} ({size:#x} bytes)",
            at.offset, section.name
        ));
    }
    Ok(section)
}

fn check_name(what: &str, name: &str) -> Result<(), String> {
    if name.is_empty() || name.contains('\0') {
        return Err(format!(
            "Mach-O {what} {name:?} must be non-empty without NUL"
        ));
    }
    Ok(())
}

/// libSystem 在前，其余 dylib 按导入首次出现的顺序；返回值下标加 1 即 dylib 序号。
pub(crate) fn dylibs(request: &MachORequest) -> Vec<&str> {
    let mut out = vec![LIB_SYSTEM];
    for import in &request.imports {
        if !out.contains(&import.dylib.as_str()) {
            out.push(&import.dylib);
        }
    }
    out
}
