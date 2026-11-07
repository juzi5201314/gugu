//! PE 导出目录与 C 入口异常展开表；名称和地址取自同一已链接符号表。
use super::*;

fn exports(request: &PeRequest) -> Vec<(&str, &ObjectSymbol)> {
    let mut exports: Vec<_> = request
        .symbols
        .iter()
        .filter_map(|symbol| {
            symbol
                .name
                .strip_prefix("~gugu_export_")
                .map(|name| (name, symbol))
        })
        .collect();
    exports.sort_unstable_by_key(|(name, _)| *name);
    exports
}

pub(super) fn directory(
    request: &PeRequest,
    defined: &BTreeMap<String, u64>,
    base: u64,
    text_rva: u64,
) -> Result<Vec<u8>, String> {
    let exports = exports(request);
    if exports.is_empty() {
        return Ok(Vec::new());
    }
    let count = exports.len();
    let mut bytes = vec![0; 40 + count * 10];
    let dll = bytes.len();
    bytes.extend_from_slice(b"gugu\0");
    let rva = |offset: usize| {
        u32::try_from(base + offset as u64).map_err(|_| "导出目录 RVA 越界".to_owned())
    };
    put_u32_at(&mut bytes, 12, rva(dll)?)?;
    put_u32_at(&mut bytes, 16, 1)?;
    put_u32_at(&mut bytes, 20, count as u32)?;
    put_u32_at(&mut bytes, 24, count as u32)?;
    put_u32_at(&mut bytes, 28, rva(40)?)?;
    put_u32_at(&mut bytes, 32, rva(40 + count * 4)?)?;
    put_u32_at(&mut bytes, 36, rva(40 + count * 8)?)?;
    for (index, (name, _)) in exports.into_iter().enumerate() {
        let address = u32::try_from(text_rva + defined[name]).map_err(|_| "导出函数 RVA 越界")?;
        put_u32_at(&mut bytes, 40 + index * 4, address)?;
        let offset = bytes.len();
        put_u32_at(&mut bytes, 40 + count * 4 + index * 4, rva(offset)?)?;
        bytes[40 + count * 8 + index * 2..42 + count * 8 + index * 2]
            .copy_from_slice(&(index as u16).to_le_bytes());
        bytes.extend_from_slice(name.as_bytes());
        bytes.push(0);
    }
    Ok(bytes)
}

pub(super) fn unwind(
    request: &PeRequest,
    sections: &mut [Section],
    defined: &BTreeMap<String, u64>,
    text_rva: u64,
) -> Result<(), String> {
    let pdata = section_bytes(sections, ".pdata")?;
    for (name, symbol) in exports(request) {
        let begin = text_rva + defined[name];
        let info = defined[&format!("~gugu_unwind_{name}")];
        for address in [begin, begin + symbol.bytes.len() as u64, info] {
            pdata.extend_from_slice(
                &u32::try_from(address)
                    .map_err(|_| "原生展开 RVA 越界")?
                    .to_le_bytes(),
            );
        }
    }
    let mut records: Vec<_> = pdata
        .chunks_exact(12)
        .map(|chunk| <[u8; 12]>::try_from(chunk).unwrap())
        .collect();
    records.sort_unstable_by_key(|record| u32::from_le_bytes(record[..4].try_into().unwrap()));
    *pdata = records.into_iter().flatten().collect();
    Ok(())
}
