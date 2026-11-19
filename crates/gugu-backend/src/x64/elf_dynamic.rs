//! ELF 动态导入：loader 解析 undefined dynsym，并以 GLOB_DAT 写入 GOT。
use super::{Blob, LinkRequest, put_u32, put_u64};
use std::collections::BTreeMap;

const DT_NULL: u64 = 0;
const DT_NEEDED: u64 = 1;
const DT_HASH: u64 = 4;
const DT_STRTAB: u64 = 5;
const DT_SYMTAB: u64 = 6;
const DT_RELA: u64 = 7;
const DT_RELASZ: u64 = 8;
const DT_RELAENT: u64 = 9;
const DT_STRSZ: u64 = 10;
const DT_SYMENT: u64 = 11;
const DT_BIND_NOW: u64 = 24;
const SYMBOL_BYTES: usize = 24;

pub(super) fn blobs(request: &LinkRequest) -> Result<Vec<Blob>, String> {
    let mut strings = vec![0];
    let mut offsets = BTreeMap::new();
    let mut symbols = vec![0; SYMBOL_BYTES];
    let mut needed = Vec::new();
    let mut names = std::collections::BTreeSet::new();
    // Library 导入只占一条 DT_NEEDED，不进 dynsym/rela。
    let relocations = request
        .imports
        .iter()
        .filter(|import| import.kind != crate::NativeImportKind::Library)
        .count();
    for crate::NativeImport {
        library,
        name,
        kind,
    } in &request.imports
    {
        if library.is_empty() || name.is_empty() || library.contains('\0') || name.contains('\0') {
            return Err("动态导入名为空或包含 NUL".into());
        }
        if *kind != crate::NativeImportKind::Library && !names.insert(name.as_str()) {
            return Err(format!("动态符号 {name} 重复导入"));
        }
        if !offsets.contains_key(library) {
            let offset = strings.len() as u64;
            offsets.insert(library, offset);
            needed.push((DT_NEEDED, offset));
            strings.extend_from_slice(library.as_bytes());
            strings.push(0);
        }
        if *kind == crate::NativeImportKind::Library {
            continue;
        }
        let name_offset = u32::try_from(strings.len()).map_err(|_| "动态字符串表超过 u32")?;
        strings.extend_from_slice(name.as_bytes());
        strings.push(0);
        let at = symbols.len();
        symbols.resize(at + SYMBOL_BYTES, 0);
        symbols[at..at + 4].copy_from_slice(&name_offset.to_le_bytes());
        symbols[at + 4] = match kind {
            crate::NativeImportKind::Function => 0x12, // STB_GLOBAL | STT_FUNC。
            crate::NativeImportKind::Data => 0x11,     // STB_GLOBAL | STT_OBJECT。
            crate::NativeImportKind::Library => 0x10,
        };
    }
    for symbol in &request.symbols {
        let Some(name) = symbol.name.strip_prefix("~gugu_export_") else {
            continue;
        };
        if !names.insert(name) {
            return Err(format!("动态符号 {name} 重复定义"));
        }
        let offset = u32::try_from(strings.len()).map_err(|_| "动态字符串表超过 u32")?;
        strings.extend_from_slice(name.as_bytes());
        strings.push(0);
        let at = symbols.len();
        symbols.resize(at + SYMBOL_BYTES, 0);
        symbols[at..at + 4].copy_from_slice(&offset.to_le_bytes());
        symbols[at + 4] = 0x12;
        // 镜像不依赖 section header 查找，保留非零普通 section 身份，st_value 使用装载相对地址。
        symbols[at + 6..at + 8].copy_from_slice(&1u16.to_le_bytes());
        symbols[at + 16..at + 24].copy_from_slice(&(symbol.bytes.len() as u64).to_le_bytes());
    }
    let count = u32::try_from(symbols.len() / SYMBOL_BYTES).map_err(|_| "动态符号数超过 u32")?;
    // 单 bucket 的 SysV hash 链包含导入与导出；loader 使用名字完成最终比较。
    let mut hash = Vec::new();
    for value in [1, count, u32::from(count > 1)]
        .into_iter()
        .chain((0..count).map(|index| {
            if index != 0 && index + 1 < count {
                index + 1
            } else {
                0
            }
        }))
    {
        hash.extend_from_slice(&value.to_le_bytes());
    }
    needed.extend([
        (DT_HASH, 0),
        (DT_STRTAB, 0),
        (DT_SYMTAB, 0),
        (DT_RELA, 0),
        (DT_RELASZ, relocations as u64 * 24),
        (DT_RELAENT, 24),
        (DT_STRSZ, strings.len() as u64),
        (DT_SYMENT, 24),
        (DT_BIND_NOW, 0),
        (DT_NULL, 0),
    ]);
    let dynamic = needed
        .into_iter()
        .flat_map(|(tag, value)| tag.to_le_bytes().into_iter().chain(value.to_le_bytes()))
        .collect();
    Ok(vec![
        Blob {
            name: ".dynstr".into(),
            bytes: strings,
        },
        Blob {
            name: ".dynsym".into(),
            bytes: symbols,
        },
        Blob {
            name: ".hash".into(),
            bytes: hash,
        },
        Blob {
            name: ".rela.dyn".into(),
            bytes: vec![0; relocations * 24],
        },
        Blob {
            name: ".dynamic".into(),
            bytes: dynamic,
        },
    ])
}

pub(super) fn patch(
    file: &mut [u8],
    defined: &BTreeMap<String, u64>,
    request: &LinkRequest,
) -> Result<(), String> {
    // Library 导入不产符号/重定位：dynsym 序数与 GOT 槽分别按各自口径枚举。
    let imports = request
        .imports
        .iter()
        .filter(|import| import.kind != crate::NativeImportKind::Library)
        .count();
    let Some(&dynamic) = defined.get(".dynamic") else {
        return Ok(());
    };
    let mut at = dynamic as usize;
    loop {
        let tag = file.get(at..at + 8).ok_or("动态节越界")?;
        let tag = u64::from_le_bytes(tag.try_into().unwrap());
        let symbol = match tag {
            DT_NULL => break,
            DT_HASH => Some(".hash"),
            DT_STRTAB => Some(".dynstr"),
            DT_SYMTAB => Some(".dynsym"),
            DT_RELA => Some(".rela.dyn"),
            _ => None,
        };
        if let Some(symbol) = symbol {
            put_u64(file, at + 8, defined[symbol])?;
        }
        at += 16;
    }
    for (index, symbol) in request
        .symbols
        .iter()
        .filter(|symbol| symbol.name.starts_with("~gugu_export_"))
        .enumerate()
    {
        let at = defined[".dynsym"] as usize + (imports + 1 + index) * SYMBOL_BYTES;
        put_u64(file, at + 8, defined[&symbol.name])?;
    }
    let relocations = defined[".rela.dyn"] as usize;
    let mut ordinal = 0_usize;
    for (slot, import) in request.imports.iter().enumerate() {
        if import.kind == crate::NativeImportKind::Library {
            continue;
        }
        let at = relocations + ordinal * 24;
        put_u64(
            file,
            at,
            defined["__gugu_stack_limit"] + 128 + slot as u64 * 8,
        )?;
        put_u32(file, at + 8, 6)?; // R_X86_64_GLOB_DAT。
        put_u32(file, at + 12, (ordinal + 1) as u32)?;
        ordinal += 1;
    }
    Ok(())
}
