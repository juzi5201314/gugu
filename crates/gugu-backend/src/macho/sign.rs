//! linker-signed ad-hoc 代码签名：SuperBlob 内只有一个 CodeDirectory（版本 0x20400，
//! SHA-256），覆盖签名之前的全部文件字节。签名页粒度取目标 Mach-O 页大小
//! （arm64 16 KiB、x86_64 4 KiB）；arm64 内核拒绝执行未签名镜像。

use super::MachORequest;
use super::layout::Layout;
use sha2::{Digest, Sha256};

const CSMAGIC_EMBEDDED_SIGNATURE: u32 = 0xfade_0cc0;
const CSMAGIC_CODEDIRECTORY: u32 = 0xfade_0c02;
const CSSLOT_CODEDIRECTORY: u32 = 0;
const CD_VERSION: u32 = 0x2_0400;
/// `CS_ADHOC | CS_LINKER_SIGNED`
const CD_FLAGS: u32 = 0x2_0002;
const CS_HASHTYPE_SHA256: u8 = 2;
const CS_EXECSEG_MAIN_BINARY: u64 = 1;
const HASH_SIZE: u64 = 32;
const SUPERBLOB_HEADER: u64 = 12 + 8;
const CD_HEADER: u64 = 88;

fn page_shift(request: &MachORequest) -> u8 {
    request.arch.page_size().trailing_zeros() as u8
}

fn slots(code_limit: u64, page_shift: u8) -> u64 {
    code_limit.div_ceil(1 << page_shift)
}

fn directory_size(request: &MachORequest, code_limit: u64) -> u64 {
    CD_HEADER
        + request.identifier.len() as u64
        + 1
        + HASH_SIZE * slots(code_limit, page_shift(request))
}

pub(super) fn signature_size(request: &MachORequest, code_limit: u64) -> u64 {
    SUPERBLOB_HEADER + directory_size(request, code_limit)
}

/// 签名区已在布局中预留；页哈希覆盖 `[0, signature_off)`，因此必须最后调用。
pub(super) fn write_signature(file: &mut [u8], request: &MachORequest, layout: &Layout) {
    let code_limit = layout.linkedit.signature_off;
    let page_shift = page_shift(request);
    let directory = directory_size(request, code_limit);
    let mut out = Vec::with_capacity(layout.linkedit.signature_size as usize);
    let total = SUPERBLOB_HEADER + directory;
    for value in [CSMAGIC_EMBEDDED_SIGNATURE, total as u32, 1] {
        out.extend_from_slice(&value.to_be_bytes());
    }
    for value in [CSSLOT_CODEDIRECTORY, SUPERBLOB_HEADER as u32] {
        out.extend_from_slice(&value.to_be_bytes());
    }
    let ident_len = request.identifier.len() as u64 + 1;
    let header = [
        CSMAGIC_CODEDIRECTORY,
        directory as u32,
        CD_VERSION,
        CD_FLAGS,
        (CD_HEADER + ident_len) as u32,
        CD_HEADER as u32,
        0,
        slots(code_limit, page_shift) as u32,
        code_limit as u32,
    ];
    for value in header {
        out.extend_from_slice(&value.to_be_bytes());
    }
    out.extend_from_slice(&[HASH_SIZE as u8, CS_HASHTYPE_SHA256, 0, page_shift]);
    // spare2、scatterOffset、teamOffset、spare3 与 codeLimit64 全为 0。
    out.extend_from_slice(&[0; 4 * 4 + 8]);
    let text = layout.text();
    for value in [text.fileoff, text.filesize, CS_EXECSEG_MAIN_BINARY] {
        out.extend_from_slice(&value.to_be_bytes());
    }
    out.extend_from_slice(request.identifier.as_bytes());
    out.push(0);
    for page in file[..code_limit as usize].chunks(1 << page_shift) {
        out.extend_from_slice(&Sha256::digest(page));
    }
    debug_assert_eq!(out.len() as u64, layout.linkedit.signature_size);
    let at = code_limit as usize;
    file[at..at + out.len()].copy_from_slice(&out);
}
