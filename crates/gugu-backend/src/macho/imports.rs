use std::collections::{BTreeMap, BTreeSet};

use crate::{NativeImport, NativeImportKind};

use super::{Fixup, Import, Location, MachORequest, Section, SectionContent, Segment, check_name};

/// 供机器后端解析的 dyld bind 槽；只有 Function 种类可用作调用入口。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ImportSlot {
    pub at: Location,
    pub kind: NativeImportKind,
}

/// 追加 C 导入与架构无关的指针槽，按原声明名返回槽位置。
pub fn append_native_imports(
    request: &mut MachORequest,
    imports: &[NativeImport],
) -> Result<BTreeMap<String, ImportSlot>, String> {
    let mut names: BTreeSet<_> = request
        .imports
        .iter()
        .map(|import| import.name.clone())
        .collect();
    for import in imports {
        check_name("dylib path", &import.library)?;
        check_name("C import name", &import.name)?;
        if import.kind != NativeImportKind::Library && !names.insert(format!("_{}", import.name)) {
            return Err(format!("native 符号 {} 重复导入", import.name));
        }
    }
    let mut slots = BTreeMap::new();
    let section = request.sections.len();
    for import in imports {
        request.libraries.push(import.library.clone());
        if import.kind == NativeImportKind::Library {
            continue;
        }
        let at = Location {
            section,
            offset: slots.len() as u64 * 8,
        };
        request.fixups.push(Fixup::Bind {
            at,
            import: request.imports.len(),
            addend: 0,
        });
        request.imports.push(Import {
            dylib: import.library.clone(),
            name: format!("_{}", import.name),
            weak: false,
        });
        slots.insert(
            import.name.clone(),
            ImportSlot {
                at,
                kind: import.kind,
            },
        );
    }
    if !slots.is_empty() {
        request.sections.push(Section {
            segment: Segment::DataConst,
            name: "__got".to_owned(),
            align: 8,
            content: SectionContent::Data(vec![0; slots.len() * 8]),
        });
    }
    Ok(slots)
}
