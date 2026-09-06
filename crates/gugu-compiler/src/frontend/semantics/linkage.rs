//! 链接名称与节归属属于声明，不与 C 调用效应混合；函数、static 和全局汇编共用此表。
use super::super::ast::{FnBody, ItemKind, Visibility};
use super::model::{DefRef, Model};
use crate::{Diagnostic, TargetName};

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) struct Linkage {
    pub(crate) definition: DefRef,
    pub(crate) export_name: Option<String>,
    pub(crate) import_name: Option<String>,
    pub(crate) section: Option<String>,
    pub(crate) used: bool,
}

impl Model<'_> {
    pub(super) fn item_linkage(&self, definition: DefRef) -> Result<Option<Linkage>, Diagnostic> {
        let module = definition.module;
        let parsed = &self.modules[module];
        let item = &parsed.arena.items[definition.item.0 as usize];
        let function = match item.kind {
            ItemKind::Function(id) => Some(&parsed.arena.fns[id.0 as usize]),
            _ => None,
        };
        let imported = function
            .is_some_and(|function| function.extern_abi.is_some() && function.body == FnBody::None);
        let body = function.is_some_and(|function| function.body != FnBody::None);
        let mut linkage = Linkage {
            definition,
            export_name: None,
            import_name: None,
            section: None,
            used: false,
        };
        for (name, tokens) in self.attributes(module, item.attributes) {
            let permitted = match name {
                "export_name" => body,
                "link_name" => imported,
                "link_section" => {
                    body || matches!(
                        item.kind,
                        ItemKind::Static { .. } | ItemKind::GlobalAsm { .. }
                    )
                }
                "used" => body || matches!(item.kind, ItemKind::Static { .. }),
                _ => continue,
            };
            if !permitted {
                return Err(self.trait_error(definition, "链接属性不适用于该声明种类或无体函数"));
            }
            if name == "used" {
                if linkage.used {
                    return Err(self.trait_error(definition, "used 属性不能重复指定"));
                }
                linkage.used = true;
                continue;
            }
            let value = super::super::string::decode_string(
                self.name(module, tokens[2].symbol.expect("链接字符串经过词法检查")),
            )
            .into_owned();
            if value.is_empty() || value.contains('\0') {
                return Err(self.trait_error(definition, "链接名称必须非空且不能包含 NUL"));
            }
            let destination = match name {
                "export_name" => &mut linkage.export_name,
                "link_name" => &mut linkage.import_name,
                _ => &mut linkage.section,
            };
            if destination.replace(value).is_some() {
                return Err(self.trait_error(definition, "同一链接属性不能重复指定"));
            }
        }
        if let Some(function) = function.filter(|function| function.extern_abi.is_some()) {
            let name = || {
                self.name(module, function.name.expect("extern 函数具有名称"))
                    .to_owned()
            };
            if imported && linkage.import_name.is_none() {
                linkage.import_name = Some(name());
            }
            if body && item.visibility == Visibility::Pub && linkage.export_name.is_none() {
                linkage.export_name = Some(name());
            }
        }
        Ok((linkage.used
            || linkage.export_name.is_some()
            || linkage.import_name.is_some()
            || linkage.section.is_some())
        .then_some(linkage))
    }

    pub(crate) fn validate_linkage(
        &self,
        linkage: &[Linkage],
        target: TargetName,
    ) -> Result<(), Diagnostic> {
        for entry in linkage {
            let Some(section) = &entry.section else {
                continue;
            };
            if target == TargetName::X86_64Windows && section.len() > 8 {
                return Err(self.trait_error(entry.definition, "PE 镜像节名不能超过 8 字节"));
            }
            let reserved = section.starts_with(".gugu")
                || match target {
                    TargetName::X86_64Linux => {
                        matches!(
                            section.as_str(),
                            ".eh_frame"
                                | ".eh_frame_hdr"
                                | ".dynamic"
                                | ".dynsym"
                                | ".dynstr"
                                | ".symtab"
                                | ".strtab"
                                | ".shstrtab"
                                | ".hash"
                                | ".gnu.hash"
                                | ".got"
                                | ".got.plt"
                                | ".plt"
                                | ".interp"
                        ) || section.starts_with(".rela.")
                            || section.starts_with(".rel.")
                            || section.starts_with(".gnu.version")
                    }
                    TargetName::X86_64Windows => matches!(
                        section.as_str(),
                        ".pdata" | ".xdata" | ".idata" | ".edata" | ".reloc" | ".tls"
                    ),
                };
            if reserved {
                return Err(self.trait_error(
                    entry.definition,
                    "link_section 不能覆盖编译器及 runtime 保留节",
                ));
            }
        }
        Ok(())
    }
}
