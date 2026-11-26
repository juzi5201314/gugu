//! `std.src.file`/`line`/`column` 的调用点折叠：位置在 checker 取定，
//! 下游只消费 `SourceLiteral` 快照，普通函数调用路径接触不到这些项。
use super::super::output::{SourceField, SourceLiteral};
use super::*;

impl Checker<'_, '_> {
    pub(super) fn src_call(&mut self, callee: ExprId, path: PathId, args: &[ExprId]) -> Option<Ty> {
        let segments = self.arena().paths[path.0 as usize]
            .segments
            .as_slice(&self.arena().segments);
        if segments
            .first()
            .is_some_and(|segment| self.state.names.contains_key(&segment.name))
        {
            return None;
        }
        let source_path = self.model.path(self.module, path);
        let path = self
            .model
            .external_path(self.module, &source_path)
            .or_else(|| self.definition_path(&source_path))?;
        let kind = match path.as_str() {
            "std.src.file" => SourceField::File,
            "std.src.line" => SourceField::Line,
            "std.src.column" => SourceField::Column,
            "std.src.caller" => SourceField::Caller,
            _ => return None,
        };
        let span = &self.arena().exprs[callee.0 as usize].span;
        if !args.is_empty() {
            self.error(
                DiagnosticCode::InvalidExpression,
                "std.src 源位置查询不接收参数",
                span.clone(),
            );
            return Some(Ty::Error);
        }
        let result = match kind {
            SourceField::File => Ty::String,
            SourceField::Line | SourceField::Column => Ty::int(),
            SourceField::Caller => {
                let Some(ty) = self.location_ty() else {
                    self.error(
                        DiagnosticCode::InvalidExpression,
                        "std.src.Location 未定义",
                        span.clone(),
                    );
                    return Some(Ty::Error);
                };
                ty
            }
        };
        self.source_literals.push(SourceLiteral {
            expression: callee,
            kind,
            file: span.path().to_string_lossy().into_owned(),
            line: span.line(),
            column: span.column(),
        });
        Some(result)
    }

    fn definition_path(&self, path: &[&str]) -> Option<String> {
        let definition = self.model.resolve(self.module, path).ok()?;
        let item = &self.model.modules[definition.module].arena.items[definition.item.0 as usize];
        let name = self.model.name(definition.module, item.name?);
        let module = &self.model.modules[definition.module].path;
        if module.is_empty() {
            Some(name.to_owned())
        } else {
            Some(format!("{module}.{name}"))
        }
    }

    /// `std.src.Location` 的命名类型；`caller()` 的返回类型由它决定。
    /// `resolve` 是相对模块作用域的，先定位 `std.src` 模块再在其中解析。
    fn location_ty(&self) -> Option<Ty> {
        let module = self
            .model
            .modules
            .iter()
            .position(|module| module.path == "std.src")?;
        let definition = self.model.resolve(module, &["Location"]).ok()?;
        let index = self
            .model
            .nominal
            .iter()
            .position(|nominal| nominal.definition == definition)?;
        Some(Ty::Named(index, Vec::new()))
    }

    /// `std.mem.embed_file` 只在早期常量位置（const/static 初始化器）合法；
    /// 定长类型由现场求值结果的字节数给出，函数体等运行期位置拒绝。
    pub(super) fn embed_call(
        &mut self,
        callee: ExprId,
        path: PathId,
        args: &[ExprId],
    ) -> Option<Ty> {
        let segments = self.arena().paths[path.0 as usize]
            .segments
            .as_slice(&self.arena().segments);
        if segments
            .first()
            .is_some_and(|segment| self.state.names.contains_key(&segment.name))
        {
            return None;
        }
        let canonical = self
            .model
            .external_path(self.module, &self.model.path(self.module, path))?;
        if canonical != "std.mem.embed_file" {
            return None;
        }
        let span = self.arena().exprs[callee.0 as usize].span.clone();
        for &arg in args {
            self.expression(arg, Some(&Ty::String));
        }
        if self.current_function.is_some() {
            self.error(
                DiagnosticCode::InvalidExpression,
                "embed_file 只能在 const/static 初始值等早期常量位置使用",
                span,
            );
            return Some(Ty::Error);
        }
        if args.len() != 1 {
            self.error(
                DiagnosticCode::InvalidExpression,
                "embed_file 需要恰好一个 string 参数",
                span,
            );
            return Some(Ty::Error);
        }
        let site = self.call_site.unwrap_or(callee);
        let byte = Ty::Int {
            signed: false,
            bits: 8,
        };
        match self
            .model
            .eval_early_const(self.module, site, &Ty::Slice(Box::new(byte.clone())))
        {
            Ok(super::super::comptime::eval::ConstantValue::Array(items)) => {
                let bytes = items
                    .iter()
                    .filter_map(|item| match item {
                        super::super::comptime::eval::ConstantValue::Int(value) => {
                            u8::try_from(*value).ok()
                        }
                        _ => None,
                    })
                    .collect::<Vec<u8>>();
                if bytes.len() != items.len() {
                    self.error(
                        DiagnosticCode::InvalidExpression,
                        "embed_file 求值结果不是 [byte]",
                        span,
                    );
                    return Some(Ty::Error);
                }
                let length = bytes.len() as u64;
                self.embed_literals
                    .push(super::super::output::EmbedLiteral {
                        expression: callee,
                        bytes,
                    });
                Some(Ty::Array(
                    Box::new(byte),
                    super::super::model::ArrayLength::Known(length),
                ))
            }
            Ok(_) => {
                self.error(
                    DiagnosticCode::InvalidExpression,
                    "embed_file 求值结果不是 [byte]",
                    span,
                );
                Some(Ty::Error)
            }
            Err(error) => {
                self.errors.push(error);
                Some(Ty::Error)
            }
        }
    }
}
