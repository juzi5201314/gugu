//! Windows PE32+。入口经 ntdll / kernel32 的 IAT 启动，不写 syscall 号。

mod algo;
mod coff;
mod link;
mod platform;
mod runtime;

use crate::lir::body::Body;

use super::codegen::X64World;
use super::elf::{self, BootOffsets};
use super::metadata::{SOURCE_SECTION, UNWIND_SECTION};

pub(crate) const PE_SCHEMA: u32 = 1;
pub(crate) const PE_DOMAIN: &str = "gugu-pe32-plus-v1";
pub(crate) const IMAGE_BASE: u64 = 0x0000_0001_4000_0000;

/// 可执行文件或 DLL。导出目录只在显式 C 导出非空时出现，当前主路径没有导出。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ImageKind {
    /// 控制台子系统可执行文件。
    Exe,
    /// 带 `IMAGE_FILE_DLL` 的 PE32+，不生成导入库。
    Dll,
}

/// 一次成功写出的 PE32+。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PeImage {
    pub(crate) bytes: Vec<u8>,
    pub(crate) schema: u32,
    pub(crate) entry: u64,
    pub(crate) sections: u32,
    pub(crate) imports: u32,
    pub(crate) relocs: u32,
    pub(crate) fingerprint: [u8; 32],
    pub(crate) archive: Vec<u8>,
}

impl PeImage {
    pub(crate) fn absent() -> Self {
        Self {
            bytes: Vec::new(),
            schema: 0,
            entry: 0,
            sections: 0,
            imports: 0,
            relocs: 0,
            fingerprint: [0; 32],
            archive: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PeError {
    message: String,
}

impl PeError {
    pub(crate) fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    pub(crate) fn message(&self) -> &str {
        &self.message
    }
}

/// 把机器片段链接成 Windows 可执行文件。
pub(crate) fn link_world(
    world: &X64World,
    bodies: &[Body],
    boot: BootOffsets,
) -> Result<PeImage, PeError> {
    link_kind(world, bodies, boot, ImageKind::Exe)
}

/// 按 exe 或 cdylib 写出。staticlib 走 [`staticlib`]。
pub(crate) fn link_kind(
    world: &X64World,
    bodies: &[Body],
    boot: BootOffsets,
    kind: ImageKind,
) -> Result<PeImage, PeError> {
    let (codes, consts) = elf::prepare(world, bodies).map_err(error_from_elf)?;
    let mut frames = std::collections::BTreeMap::new();
    for fragment in &world.fragments {
        frames.insert(fragment.symbol.clone(), fragment.metadata.frame_size);
    }
    let sections = [
        (
            world.metadata.section_name.as_str(),
            world.metadata.stackmap.as_slice(),
        ),
        (UNWIND_SECTION, world.metadata.unwind.as_slice()),
        (SOURCE_SECTION, world.metadata.source.as_slice()),
    ];
    link::link(
        &codes,
        &consts,
        &sections,
        &world.entry_symbol,
        boot,
        &frames,
        kind,
    )
}

/// 把一段文本收成确定性 COFF 静态库。时间戳为 0，不生成 `.lib` 导入库。
pub(crate) fn staticlib(text: &[u8]) -> Vec<u8> {
    coff::staticlib(text)
}

fn error_from_elf(error: elf::ElfError) -> PeError {
    PeError::new(error.message())
}

#[cfg(test)]
mod tests;
