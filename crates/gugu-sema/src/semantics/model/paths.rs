use super::{DefRef, Model, Visibility};
use crate::{Diagnostic, DiagnosticCode};

impl Model<'_> {
    pub(super) fn resolve_std_path(
        &self,
        module: usize,
        path: &[&str],
    ) -> Result<DefRef, Diagnostic> {
        let module_path = path[..path.len() - 1].join(".");
        if crate::names::private_builtin_module(&module_path) && !self.modules[module].builtin {
            return Err(Diagnostic::error(
                DiagnosticCode::ReservedName,
                format!("`{module_path}` 是 std 的私有实现模块，不是 package API"),
                Some(self.modules[module].file.eof_span.clone()),
            ));
        }
        let target = self
            .modules
            .iter()
            .position(|candidate| candidate.builtin && candidate.path == module_path)
            .ok_or_else(|| self.error(module, format!("未解析名称 `{}`", path.join("."))))?;
        let name = path[path.len() - 1];
        let definition = self.resolve(target, &[name])?;
        let item = &self.modules[definition.module].arena.items[definition.item.0 as usize];
        let exported = definition.module == target
            || self.names.imports.iter().any(|import| {
                import.module.index() == target && import.alias == name && import.public
            });
        if item.visibility != Visibility::Pub || !exported {
            return Err(self.error(module, "不能访问私有声明"));
        }
        Ok(definition)
    }
}
