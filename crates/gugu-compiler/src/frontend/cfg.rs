use std::collections::{BTreeMap, BTreeSet};

use crate::{
    diagnostics::{Diagnostic, DiagnosticCode},
    source::{ExpansionId, SourceMap, SourceSnapshot, Span},
    target::{Architecture, OperatingSystem, TargetName},
};

use super::{
    ast::{
        AsmOperandKind, AstArena, AstFile, AstRange, AttrKind, Attribute, BoundKind, ExprId,
        ExprKind, FStringPart, FnBody, GenericArg, GenericParamKind, IndexKind, ItemId, ItemKind,
        MatchArm, PatId, PatKind, SelectArm, SelectArmKind, StmtId, StmtKind, StructBody, TyId,
        TyKind, VariantKind,
    },
    token::{Token, TokenBuffer, TokenKind},
};

/// cfg 裁项遍历的入口根集合。
pub(crate) enum CfgRoots<'a> {
    /// 完整文件：内属性加顶层 item 列表。
    File(&'a AstFile),
    /// 生成片段：模块 item 列表。
    Items {
        inner_attributes: AstRange<Attribute>,
        items: &'a [ItemId],
    },
    /// 生成片段：块语句序列与可选尾表达式。
    Statements {
        stmts: &'a [StmtId],
        tail: Option<ExprId>,
    },
    /// 生成片段：语法必需的单一表达式。
    Expression(ExprId),
    /// 生成片段：语法必需的单一类型。
    Type(TyId),
    /// 生成片段：单一模式。
    Pattern(PatId),
}

#[derive(Clone, Debug)]
pub(crate) struct CfgContext {
    target: TargetName,
    declared_features: BTreeSet<String>,
    enabled_features: BTreeSet<String>,
    test: bool,
    bench: bool,
    custom: BTreeMap<String, Option<String>>,
}

impl CfgContext {
    pub(crate) fn new(
        target: TargetName,
        declared_features: impl IntoIterator<Item = String>,
        enabled_features: impl IntoIterator<Item = String>,
        test: bool,
        bench: bool,
        custom: BTreeMap<String, Option<String>>,
    ) -> Self {
        Self {
            target,
            declared_features: declared_features.into_iter().collect(),
            enabled_features: enabled_features.into_iter().collect(),
            test,
            bench,
            custom,
        }
    }

    pub(crate) fn target_only(target: TargetName) -> Self {
        Self::new(target, [], [], false, false, BTreeMap::new())
    }

    /// 是否处于 test/bench harness 编译域。
    pub(crate) fn harness(&self) -> bool {
        self.test || self.bench
    }
    pub(crate) fn target(&self) -> TargetName {
        self.target
    }

    /// 产出进入 action key 的 cfg 键值集合；顺序由 BTreeMap 固定。
    pub(crate) fn action_inputs(&self) -> BTreeMap<String, String> {
        let mut map = BTreeMap::new();
        map.insert("target".to_owned(), self.target.to_string());
        for feature in &self.enabled_features {
            map.insert(format!("feature:{feature}"), "1".to_owned());
        }
        if self.test {
            map.insert("test".to_owned(), "1".to_owned());
        }
        if self.bench {
            map.insert("bench".to_owned(), "1".to_owned());
        }
        for (key, value) in &self.custom {
            map.insert(format!("cfg:{key}"), value.clone().unwrap_or_default());
        }
        map
    }

    fn atom(&self, name: &str) -> Option<bool> {
        match name {
            "true" => Some(true),
            "false" => Some(false),
            "test" => Some(self.test),
            "bench" => Some(self.bench),
            "os" | "arch" | "feature" => None,
            _ => self.custom.get(name).map(|value| value.is_none()),
        }
    }

    fn key_value(&self, name: &str, value: &str) -> Option<bool> {
        match name {
            "os" => Some(match self.target.descriptor().os {
                OperatingSystem::Linux => value == "linux",
                OperatingSystem::Windows => value == "windows",
            }),
            "arch" => Some(match self.target.descriptor().arch {
                Architecture::X86_64 => value == "x86_64",
            }),
            "feature" if self.declared_features.contains(value) => {
                Some(self.enabled_features.contains(value))
            }
            "feature" => None,
            "test" | "bench" | "true" | "false" => None,
            _ => self
                .custom
                .get(name)
                .map(|configured| configured.as_deref() == Some(value)),
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ConfiguredAst {
    module_active: bool,
    items: Vec<bool>,
    fields: Vec<bool>,
    variants: Vec<bool>,
    use_items: Vec<bool>,
    field_exprs: Vec<bool>,
    match_arms: Vec<bool>,
    select_arms: Vec<bool>,
    params: Vec<bool>,
    stmts: Vec<bool>,
    exprs: Vec<bool>,
}

impl ConfiguredAst {
    pub(crate) fn module_active(&self) -> bool {
        self.module_active
    }

    /// item 活动位图的只读视图。
    pub(crate) fn items_mut(&mut self) -> &mut [bool] {
        &mut self.items
    }

    /// stmt 活动位图的可变视图。
    pub(crate) fn stmts_mut(&mut self) -> &mut [bool] {
        &mut self.stmts
    }

    /// 把生成片段的裁项位合并进宿主位图；`pre` 是片段解析前的 arena 长度。
    ///
    /// 片段位图按合并后的 arena 计算，宿主前缀全为 false；宿主位图只追加
    /// `pre` 之后的新节点位。
    pub(crate) fn merge_fragment(&mut self, fragment: &ConfiguredAst, pre: super::ast::ArenaLens) {
        self.items.extend_from_slice(&fragment.items[pre.items..]);
        self.fields
            .extend_from_slice(&fragment.fields[pre.fields..]);
        self.variants
            .extend_from_slice(&fragment.variants[pre.variants..]);
        self.use_items
            .extend_from_slice(&fragment.use_items[pre.use_items..]);
        self.field_exprs
            .extend_from_slice(&fragment.field_exprs[pre.field_exprs..]);
        self.match_arms
            .extend_from_slice(&fragment.match_arms[pre.match_arms..]);
        self.select_arms
            .extend_from_slice(&fragment.select_arms[pre.select_arms..]);
        self.params
            .extend_from_slice(&fragment.params[pre.params..]);
        self.stmts.extend_from_slice(&fragment.stmts[pre.stmts..]);
        self.exprs.extend_from_slice(&fragment.exprs[pre.exprs..]);
    }

    pub(crate) fn item_active(&self, item: ItemId) -> bool {
        self.items.get(item.0 as usize).copied().unwrap_or(false)
    }

    pub(crate) fn field_active(&self, index: usize) -> bool {
        self.fields.get(index).copied().unwrap_or(false)
    }

    pub(crate) fn variant_active(&self, index: usize) -> bool {
        self.variants.get(index).copied().unwrap_or(false)
    }

    pub(crate) fn use_item_active(&self, index: usize) -> bool {
        self.use_items.get(index).copied().unwrap_or(false)
    }

    pub(crate) fn param_active(&self, index: usize) -> bool {
        self.params[index]
    }
    pub(crate) fn stmt_active(&self, id: StmtId) -> bool {
        self.stmts[id.0 as usize]
    }
    pub(crate) fn expr_active(&self, id: ExprId) -> bool {
        self.exprs[id.0 as usize]
    }
    pub(crate) fn match_arm_active(&self, index: usize) -> bool {
        self.match_arms[index]
    }
    pub(crate) fn field_expr_active(&self, index: usize) -> bool {
        self.field_exprs[index]
    }
    pub(crate) fn select_arm_active(&self, index: usize) -> bool {
        self.select_arms[index]
    }
}

pub(crate) fn configure(
    snapshot: &SourceSnapshot,
    source_map: &SourceMap,
    file: &AstFile,
    arena: &AstArena,
    tokens: &TokenBuffer,
    context: &CfgContext,
) -> Result<ConfiguredAst, Vec<Diagnostic>> {
    configure_roots(
        snapshot,
        source_map,
        file.source,
        arena,
        tokens,
        context,
        ExpansionId::ROOT,
        CfgRoots::File(file),
    )
}

/// 对生成片段执行 cfg 裁项；`snapshot` 是生成文本快照，`expansion` 是其展开记录。
pub(crate) fn configure_fragment(
    snapshot: &SourceSnapshot,
    source_map: &SourceMap,
    file: crate::source::SourceFileId,
    arena: &AstArena,
    tokens: &TokenBuffer,
    context: &CfgContext,
    expansion: ExpansionId,
    roots: CfgRoots<'_>,
) -> Result<ConfiguredAst, Vec<Diagnostic>> {
    configure_roots(
        snapshot, source_map, file, arena, tokens, context, expansion, roots,
    )
}

fn configure_roots(
    snapshot: &SourceSnapshot,
    source_map: &SourceMap,
    file: crate::source::SourceFileId,
    arena: &AstArena,
    tokens: &TokenBuffer,
    context: &CfgContext,
    expansion: ExpansionId,
    roots: CfgRoots<'_>,
) -> Result<ConfiguredAst, Vec<Diagnostic>> {
    let mut diagnostics = Vec::new();
    let (inner_attributes, walk) = match &roots {
        CfgRoots::File(file) => (file.inner_attributes, Roots::File(file)),
        CfgRoots::Items {
            inner_attributes,
            items,
        } => (*inner_attributes, Roots::Items(items)),
        CfgRoots::Statements { stmts, tail } => {
            (AstRange::empty(), Roots::Statements((stmts, *tail)))
        }
        CfgRoots::Expression(expr) => (AstRange::empty(), Roots::Expression(*expr)),
        CfgRoots::Type(ty) => (AstRange::empty(), Roots::Type(*ty)),
        CfgRoots::Pattern(pat) => (AstRange::empty(), Roots::Pattern(*pat)),
    };
    let module_active = attributes_match(
        inner_attributes.as_slice(&arena.attrs),
        snapshot,
        source_map,
        tokens,
        context,
        file,
        expansion,
        &mut diagnostics,
    );
    let mut configured = ConfiguredAst {
        module_active,
        items: vec![false; arena.items.len()],
        fields: vec![false; arena.fields.len()],
        variants: vec![false; arena.variants.len()],
        use_items: vec![false; arena.use_items.len()],
        field_exprs: vec![false; arena.field_exprs.len()],
        match_arms: vec![false; arena.match_arms.len()],
        select_arms: vec![false; arena.select_arms.len()],
        params: vec![false; arena.params.len()],
        stmts: vec![false; arena.stmts.len()],
        exprs: vec![false; arena.exprs.len()],
    };
    if module_active {
        let mut configurator = Configurator {
            snapshot,
            source_map,
            arena,
            tokens,
            context,
            configured: &mut configured,
            diagnostics: &mut diagnostics,
            file,
            expansion,
        };
        match walk {
            Roots::File(file) => {
                for &item in file.items.as_slice(&arena.item_ids) {
                    configurator.item(item);
                }
            }
            Roots::Items(items) => {
                for &item in items {
                    configurator.item(item);
                }
            }
            Roots::Statements((stmts, tail)) => {
                for &stmt in stmts {
                    configurator.stmt(stmt);
                }
                if let Some(tail) = tail {
                    configurator.expr(tail, true);
                }
            }
            Roots::Expression(expr) => {
                configurator.expr(expr, false);
            }
            Roots::Type(ty) => {
                configurator.ty(ty);
            }
            Roots::Pattern(pat) => {
                configurator.pat(pat);
            }
        }
        super::attr::validate_lint_levels(
            snapshot.content(),
            inner_attributes,
            arena,
            tokens,
            &configured,
            &mut diagnostics,
        );
    }
    if diagnostics.is_empty() {
        Ok(configured)
    } else {
        Err(diagnostics)
    }
}

enum Roots<'a> {
    File(&'a AstFile),
    Items(&'a [ItemId]),
    Statements((&'a [StmtId], Option<ExprId>)),
    Expression(ExprId),
    Type(TyId),
    Pattern(PatId),
}

struct Configurator<'a> {
    snapshot: &'a SourceSnapshot,
    source_map: &'a SourceMap,
    arena: &'a AstArena,
    tokens: &'a TokenBuffer,
    context: &'a CfgContext,
    configured: &'a mut ConfiguredAst,
    diagnostics: &'a mut Vec<Diagnostic>,
    file: crate::source::SourceFileId,
    expansion: ExpansionId,
}

impl Configurator<'_> {
    fn item(&mut self, item_id: ItemId) {
        let item = &self.arena.items[item_id.0 as usize];
        let active = self.attributes(item.attributes);
        self.configured.items[item_id.0 as usize] = active;
        if !active {
            return;
        }
        match item.kind.clone() {
            ItemKind::Use(super::ast::UseTreeKind::Brace { items, .. }) => self.use_items(items),
            ItemKind::Function(function) => self.function(function),
            ItemKind::Struct { generics, body } => {
                self.generic_params(generics);
                match body {
                    StructBody::Newtype(field) => {
                        if self.attributes(field.attributes) {
                            self.ty(field.ty);
                        } else {
                            self.diagnostics.push(Diagnostic::error(
                                DiagnosticCode::CfgInvalidPredicate,
                                "cfg(false) 不能删除 newtype 的唯一字段",
                                Some(field.span.clone()),
                            ));
                        }
                    }
                    StructBody::Record(fields) => self.fields(fields),
                }
            }
            ItemKind::Enum { generics, variants } => {
                self.generic_params(generics);
                self.variants(variants);
            }
            ItemKind::Union { generics, fields } => {
                self.generic_params(generics);
                self.fields(fields);
            }
            ItemKind::TypeAlias { generics, ty } => {
                self.generic_params(generics);
                if let Some(ty) = ty {
                    self.ty(ty);
                }
            }
            ItemKind::Const { ty, value } => {
                if let Some(ty) = ty {
                    self.ty(ty);
                }
                if let Some(value) = value {
                    self.expr(value, false);
                }
            }
            ItemKind::Static { ty, value } => {
                self.ty(ty);
                self.expr(value, false);
            }
            ItemKind::Trait {
                generics, items, ..
            } => {
                self.generic_params(generics);
                self.items(items);
            }
            ItemKind::Impl {
                generics,
                self_ty,
                trait_ty,
                items,
                ..
            } => {
                self.generic_params(generics);
                self.ty(self_ty);
                if let Some(trait_ty) = trait_ty {
                    self.ty(trait_ty);
                }
                self.items(items);
            }
            ItemKind::ExternBlock { items, .. } => self.items(items),
            ItemKind::SourceMacro { body } => {
                self.expr(body, false);
            }
            ItemKind::GlobalAsm { template } => {
                self.expr(template, false);
            }
            ItemKind::Use(_) | ItemKind::Error => {}
        }
    }

    fn function(&mut self, function: super::ast::FnId) {
        let declaration = &self.arena.fns[function.0 as usize];
        self.generic_params(declaration.generics);
        for index in declaration.params.start as usize
            ..(declaration.params.start + declaration.params.len) as usize
        {
            let param = &self.arena.params[index];
            let active = self.attributes(param.attributes);
            self.configured.params[index] = active;
            if !active {
                continue;
            }
            if let Some(pat) = param.pat {
                self.pat(pat);
            }
            if let Some(ty) = param.ty {
                self.ty(ty);
            }
        }
        if let Some(ty) = declaration.return_ty {
            self.ty(ty);
        }
        match declaration.body {
            FnBody::Block(body) | FnBody::Eq(body) => {
                self.expr(body, false);
            }
            FnBody::None => {}
        }
    }

    fn fields(&mut self, fields: AstRange<super::ast::Field>) {
        for index in fields.start as usize..(fields.start + fields.len) as usize {
            let field = &self.arena.fields[index];
            let active = self.attributes(field.attributes);
            self.configured.fields[index] = active;
            if active {
                self.ty(field.ty);
            }
        }
    }

    fn variants(&mut self, variants: AstRange<super::ast::Variant>) {
        for index in variants.start as usize..(variants.start + variants.len) as usize {
            let variant = &self.arena.variants[index];
            let active = self.attributes(variant.attributes);
            self.configured.variants[index] = active;
            if !active {
                continue;
            }
            match variant.kind {
                VariantKind::Unit => {}
                VariantKind::Tuple(fields) | VariantKind::Struct(fields) => self.fields(fields),
            }
        }
    }

    fn use_items(&mut self, items: AstRange<super::ast::UseItem>) {
        for index in items.start as usize..(items.start + items.len) as usize {
            let attributes = self.arena.use_items[index].attributes;
            self.configured.use_items[index] = self.attributes(attributes);
        }
    }

    fn stmt(&mut self, stmt_id: StmtId) {
        let index = stmt_id.0 as usize;
        let stmt = &self.arena.stmts[index];
        let active = self.attributes(stmt.attributes);
        self.configured.stmts[index] = active;
        if !active {
            return;
        }
        match stmt.kind {
            StmtKind::Static { ty, value, .. } => {
                self.ty(ty);
                self.expr(value, false);
            }
            StmtKind::Let {
                pat,
                ty,
                init,
                else_block,
            } => {
                self.pat(pat);
                if let Some(ty) = ty {
                    self.ty(ty);
                }
                if let Some(init) = init {
                    self.expr(init, false);
                }
                if let Some(else_block) = else_block {
                    self.expr(else_block, false);
                }
            }
            StmtKind::Assign { place, value, .. } => {
                self.expr(place, false);
                self.expr(value, false);
            }
            StmtKind::Defer { body, .. } | StmtKind::SourceMacro { body } => {
                self.expr(body, false);
            }
            StmtKind::Expr { expr, .. } => {
                if !self.expr(expr, true) {
                    self.configured.stmts[index] = false;
                }
            }
            StmtKind::Yield => {}
        }
    }

    fn expr(&mut self, expr_id: ExprId, deletable: bool) -> bool {
        let index = expr_id.0 as usize;
        let expr = &self.arena.exprs[index];
        let active = self.attributes(expr.attributes);
        self.configured.exprs[index] = active;
        if !active {
            if !deletable {
                self.diagnostics.push(Diagnostic::error(
                    DiagnosticCode::CfgInvalidPredicate,
                    "cfg(false) 不能删除语法必需的单一表达式",
                    Some(expr.span.clone()),
                ));
            }
            return false;
        }
        self.expr_children(expr.kind);
        true
    }

    fn expr_children(&mut self, kind: ExprKind) {
        match kind {
            ExprKind::Path(path) => self.path(path),
            ExprKind::Paren(expr)
            | ExprKind::Loop(expr)
            | ExprKind::Try(expr)
            | ExprKind::Async(expr)
            | ExprKind::TryOp(expr)
            | ExprKind::Unsafe(expr)
            | ExprKind::Comptime(expr) => {
                self.expr(expr, false);
            }
            ExprKind::Tuple(exprs) | ExprKind::Array(exprs) => self.exprs(exprs, true),
            ExprKind::Repeat { elem, count } => {
                self.expr(elem, false);
                self.expr(count, false);
            }
            ExprKind::Struct { path, fields } => {
                self.path(path);
                self.field_exprs(fields);
            }
            ExprKind::Block { stmts, tail } => {
                for &stmt in stmts.as_slice(&self.arena.stmt_ids) {
                    self.stmt(stmt);
                }
                if let Some(tail) = tail {
                    self.expr(tail, true);
                }
            }
            ExprKind::If {
                cond,
                then_block,
                else_branch,
            } => {
                self.expr(cond, false);
                self.expr(then_block, false);
                if let Some(branch) = else_branch {
                    self.expr(branch, false);
                }
            }
            ExprKind::Match { scrutinee, arms } => {
                self.expr(scrutinee, false);
                self.match_arms(arms);
            }
            ExprKind::While { cond, body } => {
                self.expr(cond, false);
                self.expr(body, false);
            }
            ExprKind::For { pat, iter, body } => {
                self.pat(pat);
                self.expr(iter, false);
                self.expr(body, false);
            }
            ExprKind::Select { arms } => self.select_arms(arms),
            ExprKind::Closure(function) => self.function(function),
            ExprKind::TypeCallee(ty) => self.ty(ty),
            ExprKind::Call {
                callee,
                type_args,
                args,
            } => {
                self.expr(callee, false);
                self.generic_args(type_args);
                self.exprs(args, true);
            }
            ExprKind::TypeApp { base, args } => {
                self.expr(base, false);
                self.generic_args(args);
            }
            ExprKind::Field { base, .. } | ExprKind::TupleField { base, .. } => {
                self.expr(base, false);
            }
            ExprKind::Index { base, index } => {
                self.expr(base, false);
                match index {
                    IndexKind::Expr(index) => {
                        self.expr(index, false);
                    }
                    IndexKind::Range { start, end } => {
                        if let Some(start) = start {
                            self.expr(start, false);
                        }
                        if let Some(end) = end {
                            self.expr(end, false);
                        }
                    }
                }
            }
            ExprKind::Unary { expr, .. } => {
                self.expr(expr, false);
            }
            ExprKind::Binary { lhs, rhs, .. }
            | ExprKind::Range {
                start: lhs,
                end: rhs,
            } => {
                self.expr(lhs, false);
                self.expr(rhs, false);
            }
            ExprKind::SourceMacro { body } => {
                self.expr(body, false);
            }
            ExprKind::Intrinsic { tys, args, .. } => {
                self.generic_args(tys);
                self.exprs(args, true);
            }
            ExprKind::Asm { template, operands } => {
                self.expr(template, false);
                for operand in operands.as_slice(&self.arena.asm_operands) {
                    match operand.kind {
                        AsmOperandKind::In { expr, .. }
                        | AsmOperandKind::Out { place: expr, .. }
                        | AsmOperandKind::Lateout { place: expr, .. } => {
                            self.expr(expr, false);
                        }
                        AsmOperandKind::Clobber { .. } => {}
                    }
                }
            }
            ExprKind::Return(value) | ExprKind::Break(value) => {
                if let Some(value) = value {
                    self.expr(value, false);
                }
            }
            ExprKind::FString { parts } => {
                for part in parts.as_slice(&self.arena.fstring_parts) {
                    if let FStringPart::Interp { expr, .. } = part {
                        self.expr(*expr, true);
                    }
                }
            }
            ExprKind::Literal(_) | ExprKind::Continue | ExprKind::Error => {}
        }
    }

    fn exprs(&mut self, exprs: AstRange<ExprId>, deletable: bool) {
        for &expr in exprs.as_slice(&self.arena.expr_ids) {
            self.expr(expr, deletable);
        }
    }

    fn field_exprs(&mut self, fields: AstRange<super::ast::FieldExpr>) {
        for index in fields.start as usize..(fields.start + fields.len) as usize {
            let field = &self.arena.field_exprs[index];
            let active = self.attributes(field.attributes);
            self.configured.field_exprs[index] = active;
            if active && let Some(value) = field.value {
                self.expr(value, false);
            }
        }
    }

    fn match_arms(&mut self, arms: AstRange<MatchArm>) {
        for index in arms.start as usize..(arms.start + arms.len) as usize {
            let arm = &self.arena.match_arms[index];
            let active = self.attributes(arm.attributes);
            self.configured.match_arms[index] = active;
            if active {
                self.pat(arm.pat);
                if let Some(guard) = arm.guard {
                    self.expr(guard, false);
                }
                self.expr(arm.body, false);
            }
        }
    }

    fn select_arms(&mut self, arms: AstRange<SelectArm>) {
        for index in arms.start as usize..(arms.start + arms.len) as usize {
            let arm = &self.arena.select_arms[index];
            let active = self.attributes(arm.attributes);
            self.configured.select_arms[index] = active;
            if !active {
                continue;
            }
            match arm.kind {
                SelectArmKind::Send {
                    chan,
                    payload,
                    body,
                } => {
                    self.expr(chan, false);
                    self.expr(payload, false);
                    self.expr(body, false);
                }
                SelectArmKind::Recv { pat, chan, body } => {
                    self.pat(pat);
                    self.expr(chan, false);
                    self.expr(body, false);
                }
                SelectArmKind::Wait { pat, join, body } => {
                    self.pat(pat);
                    self.expr(join, false);
                    self.expr(body, false);
                }
                SelectArmKind::Default { body } => {
                    self.expr(body, false);
                }
                SelectArmKind::Error => {}
            }
        }
    }

    fn pat(&mut self, pat_id: PatId) {
        match self.arena.pats[pat_id.0 as usize].kind {
            PatKind::Range { start, end } => {
                self.expr(start, false);
                self.expr(end, false);
            }
            PatKind::Tuple(patterns) | PatKind::Or(patterns) => {
                for &pattern in patterns.as_slice(&self.arena.pat_ids) {
                    self.pat(pattern);
                }
            }
            PatKind::Array { prefix, suffix, .. } => {
                for &pattern in prefix
                    .as_slice(&self.arena.pat_ids)
                    .iter()
                    .chain(suffix.as_slice(&self.arena.pat_ids))
                {
                    self.pat(pattern);
                }
            }
            PatKind::Struct { path, fields, .. } => {
                self.path(path);
                for field in fields.as_slice(&self.arena.field_pats) {
                    if let Some(pattern) = field.pat {
                        self.pat(pattern);
                    }
                }
            }
            PatKind::Constructor { path, fields } => {
                self.path(path);
                for &pattern in fields.as_slice(&self.arena.pat_ids) {
                    self.pat(pattern);
                }
            }
            PatKind::Ref(pattern) | PatKind::At { pat: pattern, .. } => self.pat(pattern),
            PatKind::SourceMacro { body } => {
                self.expr(body, false);
            }
            PatKind::Wildcard
            | PatKind::Ident(_)
            | PatKind::Literal(_)
            | PatKind::NegativeLiteral(_)
            | PatKind::Error => {}
        }
    }

    fn items(&mut self, items: AstRange<ItemId>) {
        for &item in items.as_slice(&self.arena.item_ids) {
            self.item(item);
        }
    }

    fn generic_params(&mut self, params: AstRange<super::ast::GenericParam>) {
        for param in params.as_slice(&self.arena.generic_params) {
            match param.kind {
                GenericParamKind::Type { bounds, .. } => self.bounds(bounds),
                GenericParamKind::Comptime { ty, .. } => self.ty(ty),
            }
        }
    }

    fn bounds(&mut self, bounds: AstRange<super::ast::Bound>) {
        for bound in bounds.as_slice(&self.arena.bounds) {
            match bound.kind {
                BoundKind::Path(path) => self.path(path),
                BoundKind::Fn { params, ret } => {
                    for &ty in params.as_slice(&self.arena.ty_ids) {
                        self.ty(ty);
                    }
                    if let Some(ret) = ret {
                        self.ty(ret);
                    }
                }
            }
        }
    }

    fn ty(&mut self, ty_id: TyId) {
        match self.arena.tys[ty_id.0 as usize].kind {
            TyKind::Path(path) => self.path(path),
            TyKind::Tuple(types) => {
                for &ty in types.as_slice(&self.arena.ty_ids) {
                    self.ty(ty);
                }
            }
            TyKind::Array { elem, len } => {
                self.ty(elem);
                self.expr(len, false);
            }
            TyKind::Slice(ty) | TyKind::Ref(ty) | TyKind::Ptr(ty) => self.ty(ty),
            TyKind::Fn { params, ret } => {
                for &ty in params.as_slice(&self.arena.ty_ids) {
                    self.ty(ty);
                }
                if let Some(ret) = ret {
                    self.ty(ret);
                }
            }
            TyKind::Dyn(paths) => {
                for &path in paths.as_slice(&self.arena.path_ids) {
                    self.path(path);
                }
            }
            TyKind::Chan(args) => self.generic_args(args),
            TyKind::SourceMacro { body } => {
                self.expr(body, false);
            }
            TyKind::Impl(bounds) => self.bounds(bounds),
            TyKind::Never | TyKind::Infer | TyKind::Error => {}
        }
    }

    fn path(&mut self, path: super::ast::PathId) {
        for segment in self.arena.paths[path.0 as usize]
            .segments
            .as_slice(&self.arena.segments)
        {
            self.generic_args(segment.args);
        }
    }

    fn generic_args(&mut self, args: AstRange<GenericArg>) {
        for arg in args.as_slice(&self.arena.generic_args) {
            match arg {
                GenericArg::Type(ty) => self.ty(*ty),
                GenericArg::Expr(expr) => {
                    self.expr(*expr, true);
                }
            }
        }
    }

    fn attributes(&mut self, attributes: AstRange<Attribute>) -> bool {
        attributes_match(
            attributes.as_slice(&self.arena.attrs),
            self.snapshot,
            self.source_map,
            self.tokens,
            self.context,
            self.file,
            self.expansion,
            self.diagnostics,
        )
    }
}

fn attributes_match(
    attributes: &[Attribute],
    snapshot: &SourceSnapshot,
    source_map: &SourceMap,
    tokens: &TokenBuffer,
    context: &CfgContext,
    file: crate::source::SourceFileId,
    expansion: ExpansionId,
    diagnostics: &mut Vec<Diagnostic>,
) -> bool {
    let mut active = true;
    for attribute in attributes {
        let Some(predicate) = cfg_tokens(attribute, &tokens.tokens, snapshot.content()) else {
            continue;
        };
        match PredicateParser::new(
            predicate,
            snapshot.content(),
            source_map,
            file,
            expansion,
            context,
        )
        .parse()
        {
            Ok(matches) => active &= matches,
            Err(diagnostic) => diagnostics.push(diagnostic),
        }
    }
    active
}

fn cfg_tokens<'tokens>(
    attribute: &Attribute,
    tokens: &'tokens [Token],
    source: &str,
) -> Option<&'tokens [Token]> {
    let (open, close) = match attribute.kind {
        AttrKind::Outer {
            token_open,
            token_close,
        }
        | AttrKind::Inner {
            token_open,
            token_close,
        } => (token_open as usize, token_close as usize),
        AttrKind::Doc { .. } | AttrKind::InnerDoc { .. } => return None,
    };
    let body = tokens.get(open + 1..close)?;
    if body.first()?.text(source) != "cfg"
        || body.get(1)?.kind != TokenKind::LParen
        || body.last()?.kind != TokenKind::RParen
    {
        return None;
    }
    body.get(2..body.len() - 1)
}

struct PredicateParser<'a> {
    tokens: &'a [Token],
    source: &'a str,
    source_map: &'a SourceMap,
    file: crate::source::SourceFileId,
    expansion: ExpansionId,
    context: &'a CfgContext,
    cursor: usize,
}

impl<'a> PredicateParser<'a> {
    fn new(
        tokens: &'a [Token],
        source: &'a str,
        source_map: &'a SourceMap,
        file: crate::source::SourceFileId,
        expansion: ExpansionId,
        context: &'a CfgContext,
    ) -> Self {
        Self {
            tokens,
            source,
            source_map,
            file,
            expansion,
            context,
            cursor: 0,
        }
    }

    fn parse(mut self) -> Result<bool, Diagnostic> {
        let mut value = self.parse_predicate()?;
        while self.cursor < self.tokens.len() {
            self.expect(TokenKind::Comma, "cfg 谓词之间需要逗号")?;
            value &= self.parse_predicate()?;
        }
        Ok(value)
    }

    fn parse_predicate(&mut self) -> Result<bool, Diagnostic> {
        let name_token = self
            .next()
            .ok_or_else(|| self.error_at_end("cfg 谓词不能为空"))?;
        let name = name_token.text(self.source);
        if self.eat(TokenKind::Eq) {
            let value = self
                .next()
                .filter(|token| token.kind == TokenKind::String)
                .ok_or_else(|| self.error(name_token, "cfg 键值谓词需要字符串值"))?;
            let value = decode_string(value.text(self.source));
            return self.context.key_value(name, &value).ok_or_else(|| {
                self.error(
                    name_token,
                    format!("未知或未声明的 cfg 键值 `{name} = {:?}`", value),
                )
            });
        }
        if self.eat(TokenKind::LParen) {
            return self.parse_group(name_token, name);
        }
        self.context.atom(name).ok_or_else(|| {
            self.error(
                name_token,
                format!("未知 cfg 谓词 `{name}` 或该键需要字符串值"),
            )
        })
    }

    fn parse_group(&mut self, name_token: Token, name: &str) -> Result<bool, Diagnostic> {
        let mut values = Vec::new();
        while self.peek_kind() != Some(TokenKind::RParen) {
            values.push(self.parse_predicate()?);
            if self.peek_kind() != Some(TokenKind::RParen) {
                self.expect(TokenKind::Comma, "cfg 组合谓词参数之间需要逗号")?;
            }
        }
        self.expect(TokenKind::RParen, "cfg 组合谓词缺少右括号")?;
        match name {
            "all" if !values.is_empty() => Ok(values.into_iter().all(|value| value)),
            "any" if !values.is_empty() => Ok(values.into_iter().any(|value| value)),
            "not" if values.len() == 1 => Ok(!values[0]),
            "not" => Err(self.error(name_token, "cfg not 恰好需要一个谓词")),
            "all" | "any" => Err(self.error(name_token, "cfg 组合谓词不能为空")),
            _ => Err(self.error(name_token, format!("未知 cfg 组合谓词 `{name}`"))),
        }
    }

    fn expect(&mut self, kind: TokenKind, message: &str) -> Result<Token, Diagnostic> {
        let Some(token) = self.next() else {
            return Err(self.error_at_end(message));
        };
        if token.kind == kind {
            Ok(token)
        } else {
            Err(self.error(token, message))
        }
    }

    fn eat(&mut self, kind: TokenKind) -> bool {
        if self.peek_kind() == Some(kind) {
            self.cursor += 1;
            true
        } else {
            false
        }
    }

    fn next(&mut self) -> Option<Token> {
        let token = self.tokens.get(self.cursor).copied()?;
        self.cursor += 1;
        Some(token)
    }

    fn peek_kind(&self) -> Option<TokenKind> {
        self.tokens.get(self.cursor).map(|token| token.kind)
    }

    fn error(&self, token: Token, message: impl Into<String>) -> Diagnostic {
        let span = self
            .source_map
            .span(
                self.file,
                token.start as usize,
                token.end as usize,
                self.expansion,
            )
            .unwrap_or_else(|_| {
                Span::detached(
                    std::path::Path::new("<cfg>"),
                    token.start as usize,
                    token.end as usize,
                )
            });
        Diagnostic::error(DiagnosticCode::CfgInvalidPredicate, message, Some(span))
    }

    fn error_at_end(&self, message: impl Into<String>) -> Diagnostic {
        let token = self.tokens.last().copied().unwrap_or(Token {
            kind: TokenKind::Eof,
            start: 0,
            end: 0,
            trivia_start: 0,
            trivia_len: 0,
            symbol: None,
        });
        self.error(token, message)
    }
}

fn decode_string(text: &str) -> String {
    let mut decoded = String::with_capacity(text.len().saturating_sub(2));
    let mut chars = text[1..text.len() - 1].chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            decoded.push(ch);
            continue;
        }
        match chars.next().expect("词法器已校验字符串转义") {
            '\\' => decoded.push('\\'),
            '"' => decoded.push('"'),
            'n' => decoded.push('\n'),
            'r' => decoded.push('\r'),
            't' => decoded.push('\t'),
            '0' => decoded.push('\0'),
            '\'' => decoded.push('\''),
            'u' => {
                assert_eq!(chars.next(), Some('{'));
                let digits = chars
                    .by_ref()
                    .take_while(|ch| *ch != '}')
                    .collect::<String>();
                let value = u32::from_str_radix(&digits, 16).expect("词法器已校验 Unicode 转义");
                decoded.push(char::from_u32(value).expect("词法器已校验 Unicode scalar"));
            }
            _ => unreachable!("词法器已拒绝未知转义"),
        }
    }
    decoded
}
