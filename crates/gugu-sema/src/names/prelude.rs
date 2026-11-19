use std::collections::BTreeMap;

use super::{ModuleBindings, ModuleId, ModuleTarget, ParsedModule, PendingImport};

/// 由 std 源码声明的预导入名与其声明模块。语言 trait 等编译器挂钩的预导入名按名字
/// 直接解析，不经导入表。
const SOURCE_PRELUDE: [(&str, &str); 8] = [
    ("Vec", "std.vec"),
    ("HashMap", "std.collections"),
    ("HashSet", "std.collections"),
    ("Hasher", "std.hash"),
    ("Path", "std.path"),
    ("Duration", "std.time"),
    ("Read", "std.io"),
    ("Write", "std.io"),
];

/// 给每个用户模块补上源码预导入名的隐式导入。
///
/// 模块里已有同名显式导入时以显式导入为准；声明模块未进入本次编译时不补。
/// 内建源单元自行显式导入，不参与隐式预导入。
pub(super) fn implicit_imports(
    modules: &[ParsedModule],
    pending: &[PendingImport],
    bindings: &ModuleBindings,
) -> Vec<PendingImport> {
    let declaring = modules
        .iter()
        .enumerate()
        .filter(|(_, module)| module.builtin && module.configured.module_active())
        .map(|(index, module)| (module.path.as_str(), ModuleId(index as u32)))
        .collect::<BTreeMap<_, _>>();
    let mut implicit = Vec::new();
    for (index, module) in modules.iter().enumerate() {
        if module.builtin || !module.configured.module_active() {
            continue;
        }
        let owner = ModuleId(index as u32);
        for (name, path) in SOURCE_PRELUDE {
            let Some(&target) = declaring.get(path) else {
                continue;
            };
            let declared = bindings
                .get(&(target, name.to_owned()))
                .is_some_and(|bindings| !bindings.is_empty());
            let shadowed = pending
                .iter()
                .any(|import| import.module == owner && import.alias == name);
            if declared && !shadowed {
                implicit.push(PendingImport {
                    module: owner,
                    target: ModuleTarget::Local(target),
                    item: Some(name.to_owned()),
                    alias: name.to_owned(),
                    public: false,
                    span: module.file.eof_span.clone(),
                });
            }
        }
    }
    implicit
}
