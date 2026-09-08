//! 早期 comptime evaluator 与受限脚本解释器。
//!
//! 每次求值持有独立的 `EvalState`：fuel、确定性 comptime heap 字节账本、调用链、
//! 深度上限与常量循环栈。能力调用在求值前按封闭 registry 校验执行域；未登记或
//! 域不符直接产生 `comptime-capability` 编译错误，不得用空结果继续。

use std::collections::BTreeMap;

use super::super::super::ast::{
    AssignOp, AstRange, BinOp, ExprId, ExprKind, FStringPart, FnBody, IndexKind, ItemKind, LitKind,
    PatId, PatKind, StmtId, StmtKind, UnOp,
};
use super::super::model::{DefRef, Model, Ty};
use super::super::traits::MemberKind;
use super::registry::{self, Domain};
use crate::frontend::string;
use crate::query::{QueryEngine, QueryKey, QueryKind};
use crate::source::{ExpansionId, SourceMap, SourceSlot, SourceSnapshot};
use crate::{Diagnostic, DiagnosticCode};

/// ParseSource query 的 schema 版本。
pub(crate) const PARSE_SOURCE_SCHEMA_VERSION: u32 = 1;

/// 单次求值的确定性资源边界，由 compiler profile 固定并进入编译输入。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct EvalProfile {
    /// 求值步数上限。
    pub(crate) fuel: u64,
    /// comptime heap 字节上限。
    pub(crate) heap_bytes: u64,
    /// 用户函数调用与常量展开的深度上限。
    pub(crate) depth: u32,
}

impl Default for EvalProfile {
    fn default() -> Self {
        Self {
            fuel: 1_000_000,
            heap_bytes: 4 * 1024 * 1024,
            depth: 128,
        }
    }
}

/// 规范的早期常量值；离开 evaluator 前必须归一化为该表示。
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, serde::Serialize, serde::Deserialize)]
pub(crate) enum ConstantValue {
    Unit,
    Int(i128),
    Float(u64),
    Bool(bool),
    String(String),
    Array(Vec<ConstantValue>),
    Tuple(Vec<ConstantValue>),
    Struct(BTreeMap<String, ConstantValue>),
    /// 早期类型身份，不包含当前镜像的数字编号。
    Type(Ty),
    /// `std.syntax.parse_*` 产生的已解析片段；只在 SourceExpand 域出现。
    ParsedSource(ParsedFragment),
    /// `Ok(...)` 构造值；只在 SourceExpand 域出现。
    ResultOk(Box<ConstantValue>),
    /// `Err(...)` 构造值；只在 SourceExpand 域出现。
    ResultErr(Box<ConstantValue>),
}

impl ConstantValue {
    // 规范账本为每个聚合槽固定计 64 字节，并递归计算拥有的动态负载；不依赖宿主布局。
    fn heap_bytes(&self) -> u64 {
        match self {
            Self::String(text) => u64::try_from(text.len()).expect("字符串长度可编码"),
            Self::ParsedSource(fragment) => {
                u64::try_from(fragment.text.len()).expect("片段长度可编码")
            }
            Self::Array(values) | Self::Tuple(values) => values.iter().fold(0u64, |size, value| {
                size.saturating_add(64).saturating_add(value.heap_bytes())
            }),
            Self::Struct(fields) => fields.iter().fold(0u64, |size, (name, value)| {
                size.saturating_add(64)
                    .saturating_add(u64::try_from(name.len()).expect("字段名长度可编码"))
                    .saturating_add(value.heap_bytes())
            }),
            Self::ResultOk(value) | Self::ResultErr(value) => {
                64u64.saturating_add(value.heap_bytes())
            }
            _ => 0,
        }
    }
}

/// 不透明 `ParsedSource` 的内部表示：生成文本与片段类别。
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, serde::Serialize, serde::Deserialize)]
pub(crate) struct ParsedFragment {
    /// 片段类别（与解析入口一致）。
    pub(crate) slot: SourceSlot,
    /// 通过语法闸门的生成文本。
    pub(crate) text: String,
}

/// SourceExpand 域求值的外部上下文。
pub(in crate::frontend) struct ExpandHost<'a> {
    /// query 引擎，用于 `ParseSource` 解析闸门。
    pub(in crate::frontend) queries: &'a QueryEngine,
}

/// 一次源码宏脚本求值的结果与资源用量。
pub(in crate::frontend) struct MacroEval {
    /// 脚本终值。
    pub(in crate::frontend) value: ConstantValue,
    /// 消耗的 fuel 步数。
    pub(in crate::frontend) fuel_used: u64,
    /// 记账的 comptime heap 字节。
    pub(in crate::frontend) heap_used: u64,
}

/// 求值中未完成的控制流出口。
#[derive(Debug)]
enum Unwind {
    Break(ConstantValue),
    Continue,
    Return(ConstantValue),
}

#[derive(Debug)]
enum EvalError {
    Diagnostic(Diagnostic),
    Unwind(Unwind),
}

type EvalResult<T> = Result<T, EvalError>;

impl From<Diagnostic> for EvalError {
    fn from(error: Diagnostic) -> Self {
        Self::Diagnostic(error)
    }
}

impl EvalError {
    fn diagnostic(self, span: &crate::Span) -> Diagnostic {
        match self {
            Self::Diagnostic(error) => error,
            Self::Unwind(_) => Diagnostic::error(
                DiagnosticCode::InvalidExpression,
                "comptime 控制流出口超出所属函数或循环",
                Some(span.clone()),
            ),
        }
    }
}

/// 一次 comptime 求值的独立状态。
pub(super) struct EvalState<'a> {
    domain: Domain,
    profile: EvalProfile,
    fuel: u64,
    heap: u64,
    depth: u32,
    calls: Vec<String>,
    const_stack: Vec<DefRef>,
    frames: Vec<Vec<(String, ConstantValue)>>,
    /// 函数与常量初始化只访问自身词法帧，不可读取调用者局部槽。
    frame_bases: Vec<usize>,
    /// SourceExpand 域的插入上下文与 query 访问；其它域为空。
    expand: Option<ExpandState<'a>>,
}

/// SourceExpand 域的求值上下文。
struct ExpandState<'a> {
    /// 当前源码宏的插入 source slot。
    slot: SourceSlot,
    /// query 引擎。
    queries: &'a QueryEngine,
}

impl<'a> EvalState<'a> {
    fn new(domain: Domain, profile: EvalProfile) -> Self {
        Self {
            domain,
            profile,
            fuel: profile.fuel,
            heap: 0,
            depth: 0,
            calls: Vec::new(),
            const_stack: Vec::new(),
            frames: Vec::new(),
            frame_bases: Vec::new(),
            expand: None,
        }
    }

    fn source_expand(
        domain: Domain,
        profile: EvalProfile,
        slot: SourceSlot,
        host: &ExpandHost<'a>,
    ) -> Self {
        let queries = host.queries;
        let mut state = Self::new(domain, profile);
        state.expand = Some(ExpandState { slot, queries });
        state
    }

    fn step(&mut self, span: &crate::Span) -> Result<(), Diagnostic> {
        self.fuel = self
            .fuel
            .checked_sub(1)
            .ok_or_else(|| budget_error(span, &self.calls, "comptime 求值步数超过 fuel 上限"))?;
        Ok(())
    }

    fn alloc(&mut self, bytes: u64, span: &crate::Span) -> Result<(), Diagnostic> {
        self.heap = self
            .heap
            .checked_add(bytes)
            .ok_or_else(|| budget_error(span, &self.calls, "comptime heap 账本溢出"))?;
        if self.heap > self.profile.heap_bytes {
            return Err(budget_error(
                span,
                &self.calls,
                "comptime heap 分配超过字节上限",
            ));
        }
        Ok(())
    }

    fn enter(&mut self, name: &str, span: &crate::Span) -> Result<(), Diagnostic> {
        self.depth += 1;
        if self.depth > self.profile.depth {
            return Err(budget_error(span, &self.calls, "comptime 求值深度超过上限"));
        }
        self.calls.push(name.to_owned());
        self.frame_bases.push(self.frames.len());
        self.frames.push(Vec::new());
        Ok(())
    }

    fn leave(&mut self) {
        self.depth -= 1;
        self.calls.pop();
        self.frames.pop();
        self.frame_bases.pop();
    }

    fn bind(&mut self, name: String, value: ConstantValue) {
        self.frames
            .last_mut()
            .expect("局部帧随 enter/leave 配对")
            .push((name, value));
    }

    fn lookup(
        &mut self,
        name: &str,
        span: &crate::Span,
    ) -> Result<Option<ConstantValue>, Diagnostic> {
        let base = self.frame_bases.last().copied().unwrap_or(0);
        let slot = self.frames[base..]
            .iter()
            .enumerate()
            .rev()
            .find_map(|(frame, values)| {
                values
                    .iter()
                    .rposition(|(bound, _)| bound == name)
                    .map(|slot| (base + frame, slot))
            });
        let Some((frame, slot)) = slot else {
            return Ok(None);
        };
        self.alloc(self.frames[frame][slot].1.heap_bytes(), span)?;
        Ok(Some(self.frames[frame][slot].1.clone()))
    }

    fn slot_mut(&mut self, name: &str) -> Option<&mut ConstantValue> {
        let base = self.frame_bases.last().copied().unwrap_or(0);
        self.frames[base..]
            .iter_mut()
            .rev()
            .flat_map(|frame| frame.iter_mut().rev())
            .find(|(bound, _)| bound == name)
            .map(|(_, value)| value)
    }

    fn with_scope<T, E>(&mut self, run: impl FnOnce(&mut Self) -> Result<T, E>) -> Result<T, E> {
        self.frames.push(Vec::new());
        let result = run(self);
        self.frames.pop();
        result
    }
}

fn budget_error(span: &crate::Span, calls: &[String], reason: &str) -> Diagnostic {
    let chain = if calls.is_empty() {
        String::new()
    } else {
        format!("，调用链：{}", calls.join(" -> "))
    };
    Diagnostic::error(
        DiagnosticCode::ComptimeBudget,
        format!("{reason}{chain}"),
        Some(span.clone()),
    )
}

fn capability_error(span: &crate::Span, message: String) -> Diagnostic {
    Diagnostic::error(
        DiagnosticCode::ComptimeCapability,
        message,
        Some(span.clone()),
    )
}

impl Model<'_> {
    pub(super) fn eval_profile(&self) -> EvalProfile {
        self.eval_profile
    }

    #[cfg(test)]
    pub(crate) fn set_eval_profile(&mut self, profile: EvalProfile) {
        self.eval_profile = profile;
    }

    pub(crate) fn constant_type(&self, module: usize, expr: ExprId) -> Result<Ty, Diagnostic> {
        self.constant_type_inner(module, expr, &mut Vec::new())
    }

    fn constant_type_inner(
        &self,
        module: usize,
        expr: ExprId,
        stack: &mut Vec<DefRef>,
    ) -> Result<Ty, Diagnostic> {
        match self.modules[module].arena.exprs[usize::try_from(expr.0).expect("表达式下标")].kind
        {
            ExprKind::Literal(LitKind::Int { .. }) => Ok(Ty::int()),
            ExprKind::Literal(LitKind::Char { .. }) => Ok(Ty::Char),
            ExprKind::Literal(LitKind::ByteChar { .. }) => Ok(Ty::Int {
                signed: false,
                bits: 8,
            }),
            ExprKind::Literal(LitKind::Bool(_)) => Ok(Ty::Bool),
            ExprKind::Literal(LitKind::Float { .. }) => Ok(Ty::Float(64)),
            ExprKind::Literal(LitKind::String { .. } | LitKind::RawString { .. }) => Ok(Ty::String),
            ExprKind::Paren(inner) | ExprKind::Unary { expr: inner, .. } => {
                self.constant_type_inner(module, inner, stack)
            }
            ExprKind::Binary { lhs, rhs, op } => {
                let left = self.constant_type_inner(module, lhs, stack)?;
                let right = self.constant_type_inner(module, rhs, stack)?;
                if left != right {
                    return Err(self.error(module, "常量操作数类型不一致"));
                }
                Ok(
                    if matches!(
                        op,
                        BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge
                    ) {
                        Ty::Bool
                    } else {
                        left
                    },
                )
            }
            ExprKind::Path(path) => {
                let def = match self
                    .constant_member(module, path)?
                    .and_then(|member| member.definition)
                {
                    Some(def) => def,
                    None => self.resolve(module, &self.path(module, path))?,
                };
                if stack.contains(&def) {
                    return Err(self.error(module, "常量类型推断形成循环"));
                }
                match self.modules[def.module].arena.items
                    [usize::try_from(def.item.0).expect("项下标")]
                .kind
                {
                    ItemKind::Const { ty: Some(ty), .. } | ItemKind::Static { ty, .. } => {
                        self.form(def.module, ty)
                    }
                    ItemKind::Const {
                        value: Some(value), ..
                    } => {
                        stack.push(def);
                        let ty = self.constant_type_inner(def.module, value, stack);
                        stack.pop();
                        ty
                    }
                    _ => Err(self.error(module, "端点不是常量")),
                }
            }
            ExprKind::Intrinsic {
                kind: crate::frontend::ast::IntrinsicKind::TypeId,
                ..
            } => Ok(Ty::TypeId),
            ExprKind::Intrinsic { .. } => Ok(Ty::int()),
            ExprKind::Comptime(inner) => self.constant_type_inner(module, inner, stack),
            ExprKind::Call { callee, .. } => {
                match self.modules[module].arena.exprs[callee.0 as usize].kind {
                    ExprKind::Field { base, name }
                        if self.constant_type_inner(module, base, stack)? == Ty::TypeId =>
                    {
                        match self.name(module, name) {
                            "name" => Ok(Ty::String),
                            "as_int" => Ok(Ty::int()),
                            _ => Err(self.error(module, "未知 TypeId 方法")),
                        }
                    }
                    ExprKind::Path(path) => {
                        let def = self.resolve(module, &self.path(module, path))?;
                        let signature = self.value_type(def)?;
                        Ok(signature
                            .signature()
                            .ok_or_else(|| self.error(module, "常量函数没有签名"))?
                            .1
                            .clone())
                    }
                    _ => Err(self.error(module, "无法形成常量调用类型")),
                }
            }
            _ => Err(self.error(module, "无法形成常量类型")),
        }
    }

    pub(crate) fn constant_int(
        &self,
        module: usize,
        expression: ExprId,
    ) -> Result<i128, Diagnostic> {
        match self.constant_value(module, expression, &Ty::int())? {
            ConstantValue::Int(value) => Ok(value),
            _ => Err(self.error(module, "需要整数常量")),
        }
    }

    pub(in super::super) fn constant_value(
        &self,
        module: usize,
        expression: ExprId,
        ty: &Ty,
    ) -> Result<ConstantValue, Diagnostic> {
        self.eval_early_const(module, expression, ty)
    }

    /// 以独立的 `EvalState` 在早期域求值一个常量表达式。
    pub(in crate::frontend) fn eval_early_const(
        &self,
        module: usize,
        expression: ExprId,
        ty: &Ty,
    ) -> Result<ConstantValue, Diagnostic> {
        if self.depends_on_late(module, expression) {
            return Err(Diagnostic::error(
                DiagnosticCode::LateComptime,
                "late 值不能用于早期类型、布局、泛型实参或源码宏",
                Some(
                    self.modules[module].arena.exprs[expression.0 as usize]
                        .span
                        .clone(),
                ),
            ));
        }
        let mut state = EvalState::new(Domain::EARLY_CONST, self.eval_profile());
        let value = self
            .value(module, expression, &mut state)
            .map_err(|error| {
                error.diagnostic(&self.modules[module].arena.exprs[expression.0 as usize].span)
            })?;
        Ok(in_type(value, ty))
    }

    /// 在 SourceExpand 域执行一个源码宏脚本，返回脚本终值与资源用量。
    ///
    /// 顶层 `?` 与 `return` 通过 `Unwind::Return` 传出；`break`/`continue` 属于
    /// 脚本错误。终值可以是 `ParsedSource`、`Ok(...)`/`Err(...)` 或其它
    /// 编译期值，由宏边界进一步判定。
    pub(in crate::frontend) fn eval_source_macro(
        &self,
        module: usize,
        body: ExprId,
        slot: SourceSlot,
        host: &ExpandHost<'_>,
    ) -> Result<MacroEval, Diagnostic> {
        let profile = self.eval_profile();
        let mut state = EvalState::source_expand(Domain::SOURCE_EXPAND, profile, slot, host);
        let result = self.value(module, body, &mut state);
        let fuel_used = profile.fuel - state.fuel;
        let heap_used = state.heap;
        match result {
            Ok(value) | Err(EvalError::Unwind(Unwind::Return(value))) => Ok(value),
            Err(error) => {
                Err(error.diagnostic(&self.modules[module].arena.exprs[body.0 as usize].span))
            }
        }
        .map(|value| MacroEval {
            value,
            fuel_used,
            heap_used,
        })
    }

    fn fail(&self, module: usize, expression: ExprId, message: &str) -> EvalError {
        Diagnostic::error(
            DiagnosticCode::InvalidExpression,
            message.to_owned(),
            Some(
                self.modules[module].arena.exprs[expression.0 as usize]
                    .span
                    .clone(),
            ),
        )
        .into()
    }

    fn value(
        &self,
        module: usize,
        expression: ExprId,
        state: &mut EvalState,
    ) -> EvalResult<ConstantValue> {
        let arena = &self.modules[module].arena;
        let span = arena.exprs[expression.0 as usize].span.clone();
        state.step(&span)?;
        let fail = || self.fail(module, expression, "需要可求值的编译期表达式");
        match arena.exprs[expression.0 as usize].kind {
            ExprKind::Literal(lit) => {
                let value = self.literal_value(module, lit).ok_or_else(fail)?;
                if let ConstantValue::String(text) = &value {
                    state.alloc(text.len() as u64, &span)?;
                }
                Ok(value)
            }
            ExprKind::Paren(inner)
            | ExprKind::Comptime(inner)
            | ExprKind::Unsafe(inner)
            | ExprKind::TypeApp { base: inner, .. } => self.value(module, inner, state),
            ExprKind::Unary { op, expr } => {
                let inner = self.value(module, expr, state)?;
                match (op, inner) {
                    (UnOp::Not, ConstantValue::Bool(value)) => Ok(ConstantValue::Bool(!value)),
                    (UnOp::Neg, ConstantValue::Int(value)) => {
                        value.checked_neg().map(ConstantValue::Int).ok_or_else(fail)
                    }
                    (UnOp::BitNot, ConstantValue::Int(value)) => Ok(ConstantValue::Int(!value)),
                    (UnOp::Neg, ConstantValue::Float(bits)) => {
                        Ok(ConstantValue::Float((-f64::from_bits(bits)).to_bits()))
                    }
                    _ => Err(fail()),
                }
            }
            ExprKind::Binary { lhs, rhs, op } => {
                let left = self.value(module, lhs, state)?;
                if matches!(
                    (&left, op),
                    (ConstantValue::Bool(false), BinOp::And)
                        | (ConstantValue::Bool(true), BinOp::Or)
                ) {
                    return Ok(left);
                }
                let right = self.value(module, rhs, state)?;
                charge_concat(op, &left, &right, state, &span)?;
                evaluate(op, left, right).ok_or_else(fail)
            }
            ExprKind::Path(path) => self.path_value(module, expression, path, state),
            ExprKind::Call { callee, args, .. } => {
                self.call_value(module, expression, callee, args, state)
            }
            ExprKind::Tuple(elements) => {
                let elements = self.value_list(module, elements, state)?;
                Ok(ConstantValue::Tuple(elements))
            }
            ExprKind::Array(elements) => {
                let elements = self.value_list(module, elements, state)?;
                Ok(ConstantValue::Array(elements))
            }
            ExprKind::Repeat { elem, count } => {
                let element = self.value(module, elem, state)?;
                let count = self.int_operand(module, count, state)?;
                let count = usize::try_from(count).map_err(|_| {
                    budget_error(&span, &state.calls, "comptime 数组长度超出宿主可表示范围")
                })?;
                let bytes = u64::try_from(count)
                    .expect("数组长度可编码")
                    .saturating_mul(64u64.saturating_add(element.heap_bytes()));
                state.alloc(bytes, &span)?;
                Ok(ConstantValue::Array(vec![element; count]))
            }
            ExprKind::Struct { fields, .. } => {
                let mut value = BTreeMap::new();
                for field in fields.as_slice(&arena.field_exprs) {
                    let name = self.name(module, field.name).to_owned();
                    let field_value = match field.value {
                        Some(value) => self.value(module, value, state)?,
                        None => match state.lookup(&name, &span)? {
                            Some(value) => value,
                            None => self.module_const(module, &[&name], state)?,
                        },
                    };
                    state.alloc(
                        64 + u64::try_from(name.len()).expect("字段名长度可编码"),
                        &span,
                    )?;
                    value.insert(name, field_value);
                }
                Ok(ConstantValue::Struct(value))
            }
            ExprKind::Field { base, name } => {
                let base = self.value(module, base, state)?;
                match base {
                    ConstantValue::Struct(mut fields) => {
                        fields.remove(self.name(module, name)).ok_or_else(fail)
                    }
                    _ => Err(fail()),
                }
            }
            ExprKind::TupleField { base, index } => {
                let base = self.value(module, base, state)?;
                match base {
                    ConstantValue::Tuple(elements) => {
                        elements.into_iter().nth(index as usize).ok_or_else(fail)
                    }
                    _ => Err(fail()),
                }
            }
            ExprKind::Index { base, index } => {
                let base = self.value(module, base, state)?;
                let IndexKind::Expr(index) = index else {
                    return Err(fail());
                };
                let index = self.int_operand(module, index, state)?;
                let index = usize::try_from(index).map_err(|_| fail())?;
                match base {
                    ConstantValue::Array(elements) | ConstantValue::Tuple(elements) => {
                        elements.into_iter().nth(index).ok_or_else(fail)
                    }
                    _ => Err(fail()),
                }
            }
            ExprKind::If {
                cond,
                then_block,
                else_branch,
            } => {
                if self.condition(module, cond, state)? {
                    self.value(module, then_block, state)
                } else if let Some(else_branch) = else_branch {
                    self.value(module, else_branch, state)
                } else {
                    Ok(ConstantValue::Unit)
                }
            }
            ExprKind::While { cond, body } => {
                while self.condition(module, cond, state)? {
                    state.step(&span)?;
                    match self.value(module, body, state) {
                        Ok(_) | Err(EvalError::Unwind(Unwind::Continue)) => {}
                        Err(EvalError::Unwind(Unwind::Break(_))) => break,
                        Err(error) => return Err(error),
                    }
                }
                Ok(ConstantValue::Unit)
            }
            ExprKind::Loop(body) => loop {
                state.step(&span)?;
                match self.value(module, body, state) {
                    Ok(_) | Err(EvalError::Unwind(Unwind::Continue)) => {}
                    Err(EvalError::Unwind(Unwind::Break(value))) => return Ok(value),
                    Err(error) => return Err(error),
                }
            },
            ExprKind::For { pat, iter, body } => {
                let ExprKind::Range { start, end } = arena.exprs[iter.0 as usize].kind else {
                    return Err(fail());
                };
                let start = self.int_operand(module, start, state)?;
                let end = self.int_operand(module, end, state)?;
                for index in start..end {
                    state.step(&span)?;
                    let result = state.with_scope(|state| {
                        self.bind_pattern(module, pat, &ConstantValue::Int(index), state)?;
                        self.value(module, body, state)
                    });
                    match result {
                        Ok(_) | Err(EvalError::Unwind(Unwind::Continue)) => {}
                        Err(EvalError::Unwind(Unwind::Break(_))) => break,
                        Err(error) => return Err(error),
                    }
                }
                Ok(ConstantValue::Unit)
            }
            ExprKind::Block { stmts, tail } => {
                state.with_scope(|state| self.block(module, stmts, tail, state))
            }
            ExprKind::Match { scrutinee, arms } => {
                self.match_value(module, expression, scrutinee, arms, state)
            }
            ExprKind::TryOp(inner) => match self.value(module, inner, state)? {
                ConstantValue::ResultOk(value) => Ok(*value),
                ConstantValue::ResultErr(error) => Err(EvalError::Unwind(Unwind::Return(
                    ConstantValue::ResultErr(error),
                ))),
                _ => Err(fail()),
            },
            ExprKind::FString { parts } => self.fstring(module, parts, state),
            ExprKind::Return(value) => {
                let value = match value {
                    Some(value) => self.value(module, value, state)?,
                    None => ConstantValue::Unit,
                };
                Err(EvalError::Unwind(Unwind::Return(value)))
            }
            ExprKind::Break(value) => {
                let value = match value {
                    Some(value) => self.value(module, value, state)?,
                    None => ConstantValue::Unit,
                };
                Err(EvalError::Unwind(Unwind::Break(value)))
            }
            ExprKind::Continue => Err(EvalError::Unwind(Unwind::Continue)),
            ExprKind::Intrinsic { kind, tys, .. } => {
                use crate::frontend::ast::IntrinsicKind;
                match kind {
                    IntrinsicKind::TypeId => {
                        let [argument] = tys.as_slice(&arena.generic_args) else {
                            return Err(fail());
                        };
                        let ty = self.form_argument(module, *argument)?;
                        if matches!(ty, Ty::Never | Ty::MaybeUninit(_)) {
                            return Err(self.fail(module, expression, "该类型没有 TypeId"));
                        }
                        Ok(ConstantValue::Type(ty))
                    }
                    IntrinsicKind::TypeIdCount => Err(Diagnostic::error(
                        DiagnosticCode::LateComptime,
                        "type_id_count 只能在类型集合冻结后求值",
                        Some(span),
                    )
                    .into()),
                    _ => Err(capability_error(
                        &span,
                        "该 intrinsic 尚无可用的早期求值结果".to_owned(),
                    )
                    .into()),
                }
            }
            ExprKind::Asm { .. }
            | ExprKind::Select { .. }
            | ExprKind::Async(_)
            | ExprKind::Closure(_)
            | ExprKind::SourceMacro { .. }
            | ExprKind::Try(_)
            | ExprKind::TypeCallee(_) => Err(fail()),
            _ => Err(fail()),
        }
    }

    fn value_list(
        &self,
        module: usize,
        elements: AstRange<ExprId>,
        state: &mut EvalState,
    ) -> EvalResult<Vec<ConstantValue>> {
        state.alloc(
            u64::from(elements.len).saturating_mul(64),
            &self.modules[module].file.eof_span,
        )?;
        elements
            .as_slice(&self.modules[module].arena.expr_ids)
            .iter()
            .map(|&element| self.value(module, element, state))
            .collect()
    }

    /// 复用当前求值状态求整数操作数；局部绑定与 fuel 账本保持共享。
    fn int_operand(
        &self,
        module: usize,
        expression: ExprId,
        state: &mut EvalState,
    ) -> EvalResult<i128> {
        match self.value(module, expression, state)? {
            ConstantValue::Int(value) => Ok(value),
            _ => Err(self.fail(module, expression, "需要整数常量")),
        }
    }

    fn condition(
        &self,
        module: usize,
        expression: ExprId,
        state: &mut EvalState,
    ) -> EvalResult<bool> {
        match self.value(module, expression, state)? {
            ConstantValue::Bool(value) => Ok(value),
            _ => Err(self.fail(module, expression, "comptime 条件必须是 bool")),
        }
    }

    fn match_value(
        &self,
        module: usize,
        expression: ExprId,
        scrutinee: ExprId,
        arms: AstRange<super::super::super::ast::MatchArm>,
        state: &mut EvalState,
    ) -> EvalResult<ConstantValue> {
        let arena = &self.modules[module].arena;
        let scrutinee = self.value(module, scrutinee, state)?;
        for arm in arms.as_slice(&arena.match_arms) {
            if arm.guard.is_some() {
                return Err(self.fail(module, expression, "comptime match 不支持守卫"));
            }
            state.frames.push(Vec::new());
            match self.match_pattern(module, arm.pat, &scrutinee, state) {
                Ok(true) => {
                    let result = self.value(module, arm.body, state);
                    state.frames.pop();
                    return result;
                }
                Ok(false) => {
                    state.frames.pop();
                }
                Err(error) => {
                    state.frames.pop();
                    return Err(error);
                }
            }
        }
        Err(self.fail(module, expression, "comptime match 没有匹配分支"))
    }

    fn match_pattern(
        &self,
        module: usize,
        pat: PatId,
        value: &ConstantValue,
        state: &mut EvalState,
    ) -> EvalResult<bool> {
        let arena = &self.modules[module].arena;
        let span = arena.pats[pat.0 as usize].span.clone();
        let fail = |message: &str| {
            Diagnostic::error(
                DiagnosticCode::InvalidPattern,
                message.to_owned(),
                Some(span.clone()),
            )
            .into()
        };
        Ok(match arena.pats[pat.0 as usize].kind {
            PatKind::Wildcard => true,
            PatKind::Ident(name) => {
                state.alloc(value.heap_bytes(), &span)?;
                state.bind(self.name(module, name).to_owned(), value.clone());
                true
            }
            PatKind::Literal(lit) => self.literal_value(module, lit).as_ref() == Some(value),
            PatKind::NegativeLiteral(lit) => match (self.literal_value(module, lit), value) {
                (Some(ConstantValue::Int(inner)), ConstantValue::Int(value)) => -inner == *value,
                (Some(ConstantValue::Float(bits)), ConstantValue::Float(value)) => {
                    f64::from_bits(*value) == -f64::from_bits(bits)
                }
                _ => false,
            },
            PatKind::Ref(inner) => self.match_pattern(module, inner, value, state)?,
            PatKind::Range { start, end } => {
                let ConstantValue::Int(value) = value else {
                    return Err(fail("范围模式要求整数标量"));
                };
                let ConstantValue::Int(start) = self.value(module, start, state)? else {
                    return Err(fail("范围模式端点必须是整数"));
                };
                let ConstantValue::Int(end) = self.value(module, end, state)? else {
                    return Err(fail("范围模式端点必须是整数"));
                };
                start <= *value && *value <= end
            }
            PatKind::Tuple(patterns) => {
                let ConstantValue::Tuple(elements) = value else {
                    return Err(fail("元组模式要求元组值"));
                };
                let patterns = patterns.as_slice(&arena.pat_ids);
                if patterns.len() != elements.len() {
                    return Ok(false);
                }
                for (pattern, element) in patterns.iter().zip(elements) {
                    if !self.match_pattern(module, *pattern, element, state)? {
                        return Ok(false);
                    }
                }
                true
            }
            PatKind::Or(patterns) => {
                let saved = state.frames.last().expect("match 分支帧").len();
                for pattern in patterns.as_slice(&arena.pat_ids) {
                    if self.match_pattern(module, *pattern, value, state)? {
                        return Ok(true);
                    }
                    state
                        .frames
                        .last_mut()
                        .expect("match 分支帧")
                        .truncate(saved);
                }
                false
            }
            PatKind::At { name, pat, .. } => {
                state.alloc(value.heap_bytes(), &span)?;
                state.bind(self.name(module, name).to_owned(), value.clone());
                self.match_pattern(module, pat, value, state)?
            }
            PatKind::Constructor { path, fields } => {
                let segments = self.path(module, path);
                let fields = fields.as_slice(&arena.pat_ids);
                match (segments.as_slice(), value) {
                    (["Ok"], ConstantValue::ResultOk(inner)) => {
                        if fields.len() != 1 {
                            return Err(fail("Ok 模式需要恰好一个绑定"));
                        }
                        self.match_pattern(module, fields[0], inner, state)?
                    }
                    (["Err"], ConstantValue::ResultErr(inner)) => {
                        if fields.len() != 1 {
                            return Err(fail("Err 模式需要恰好一个绑定"));
                        }
                        self.match_pattern(module, fields[0], inner, state)?
                    }
                    (_, ConstantValue::ResultOk(_)) | (_, ConstantValue::ResultErr(_)) => false,
                    _ => return Err(fail("构造器模式只能匹配 Result 值")),
                }
            }
            _ => return Err(fail("comptime match 只支持标量与元组模式")),
        })
    }

    fn literal_value(&self, module: usize, lit: LitKind) -> Option<ConstantValue> {
        match lit {
            LitKind::Bool(value) => Some(ConstantValue::Bool(value)),
            LitKind::Char { value, .. } => Some(ConstantValue::Int(i128::from(value as u32))),
            LitKind::ByteChar { value, .. } => Some(ConstantValue::Int(i128::from(value))),
            LitKind::Int { limbs, .. } => {
                let mut value = 0i128;
                for &limb in limbs
                    .as_slice(&self.modules[module].arena.int_limbs)
                    .iter()
                    .rev()
                {
                    value = value
                        .checked_mul(1i128 << 32)?
                        .checked_add(i128::from(limb))?;
                }
                Some(ConstantValue::Int(value))
            }
            LitKind::Float { digits, exp10 } => {
                let value = format!("{}e{exp10}", self.name(module, digits))
                    .parse::<f64>()
                    .ok()?;
                Some(ConstantValue::Float(value.to_bits()))
            }
            LitKind::String { text } | LitKind::RawString { text } => Some(ConstantValue::String(
                string::decode_string(self.name(module, text)).into_owned(),
            )),
            _ => None,
        }
    }

    fn fstring(
        &self,
        module: usize,
        parts: AstRange<FStringPart>,
        state: &mut EvalState,
    ) -> EvalResult<ConstantValue> {
        let arena = &self.modules[module].arena;
        let span = self.modules[module].file.eof_span.clone();
        let mut output = String::new();
        for part in parts.as_slice(&arena.fstring_parts) {
            match *part {
                FStringPart::Text { text, .. } => {
                    let text = self.name(module, text);
                    state.alloc(u64::try_from(text.len()).expect("文本长度可编码"), &span)?;
                    output.push_str(text);
                }
                FStringPart::Interp { expr, spec, .. } => {
                    if spec.is_some() {
                        return Err(self.fail(module, expr, "comptime f-string 不支持格式码"));
                    }
                    let value = self.value(module, expr, state)?;
                    if matches!(value, ConstantValue::ParsedSource(_)) {
                        return Err(self.fail(module, expr, "f-string 不能插值 ParsedSource"));
                    }
                    let text = match value {
                        ConstantValue::String(text) => text,
                        value => display_value(&value),
                    };
                    state.alloc(u64::try_from(text.len()).expect("插值长度可编码"), &span)?;
                    output.push_str(&text);
                }
            }
        }
        Ok(ConstantValue::String(output))
    }

    fn module_const(
        &self,
        module: usize,
        segments: &[&str],
        state: &mut EvalState,
    ) -> EvalResult<ConstantValue> {
        let def = self
            .resolve(module, segments)
            .map_err(|_| self.error(module, "未解析的编译期名称"))?;
        self.item_const_value(def, state)
    }

    fn item_const_value(&self, def: DefRef, state: &mut EvalState) -> EvalResult<ConstantValue> {
        let span = self.modules[def.module].file.eof_span.clone();
        if state.const_stack.contains(&def) {
            return Err(Diagnostic::error(
                DiagnosticCode::InvalidDeclaration,
                "常量初始化形成循环",
                Some(span),
            )
            .into());
        }
        let ItemKind::Const {
            ty: declared,
            value: Some(value),
            ..
        } = self.modules[def.module].arena.items[def.item.0 as usize].kind
        else {
            return Err(Diagnostic::error(
                DiagnosticCode::InvalidExpression,
                "端点不是可求值的常量",
                Some(span),
            )
            .into());
        };
        state.enter("<常量初始化>", &span)?;
        state.const_stack.push(def);
        let declared = match declared {
            Some(ty) => self.form(def.module, ty),
            None => self.constant_type(def.module, value),
        };
        let result = match declared {
            Ok(ty) => self
                .value(def.module, value, state)
                .map(|value| in_type(value, &ty)),
            Err(error) => Err(error.into()),
        };
        state.const_stack.pop();
        state.leave();
        result
    }

    fn path_value(
        &self,
        module: usize,
        expression: ExprId,
        path: super::super::super::ast::PathId,
        state: &mut EvalState,
    ) -> EvalResult<ConstantValue> {
        let segments = self.path(module, path);
        let span = self.modules[module].arena.exprs[expression.0 as usize]
            .span
            .clone();
        self.check_capability(module, &segments, &span, state)?;
        if segments.len() == 1
            && let Some(value) = state.lookup(segments[0], &span)?
        {
            return Ok(value);
        }
        if let Some(member) = self.constant_member(module, path)? {
            if let MemberKind::Const {
                value: Some(value), ..
            } = &member.kind
            {
                state.alloc(value.heap_bytes(), &span)?;
                return Ok(value.clone());
            }
            if let Some(def) = member.definition {
                return self.item_const_value(def, state);
            }
        }
        let def = self
            .resolve(module, &segments)
            .map_err(|_| self.fail(module, expression, "未解析的编译期名称"))?;
        if matches!(
            self.modules[def.module].arena.items[def.item.0 as usize].kind,
            ItemKind::Function(_)
        ) {
            return Err(self.fail(module, expression, "函数值不是可物化的编译期常量"));
        }
        self.item_const_value(def, state)
    }

    /// 求值前按封闭 registry 校验路径身份；未登记或执行域不符立即失败。
    fn check_capability(
        &self,
        module: usize,
        segments: &[&str],
        span: &crate::Span,
        state: &EvalState,
    ) -> Result<(), Diagnostic> {
        let canonical =
            lang_item_identity(segments).or_else(|| self.external_path(module, segments));
        let Some(canonical) = canonical else {
            return Ok(());
        };
        let Some(entry) = registry::lookup(&canonical) else {
            return Err(capability_error(
                span,
                format!("标准库能力 `{canonical}` 未在 comptime capability registry 登记"),
            ));
        };
        if !entry.domains.allows(state.domain) {
            return Err(capability_error(
                span,
                format!(
                    "能力 `{canonical}` 不允许在 {} 执行域调用",
                    state.domain.name()
                ),
            ));
        }
        Ok(())
    }

    fn call_value(
        &self,
        module: usize,
        expression: ExprId,
        callee: ExprId,
        args: AstRange<ExprId>,
        state: &mut EvalState,
    ) -> EvalResult<ConstantValue> {
        let arena = &self.modules[module].arena;
        let span = arena.exprs[expression.0 as usize].span.clone();
        if let ExprKind::Field { base, name } = arena.exprs[callee.0 as usize].kind {
            if args.len != 0 {
                return Err(self.fail(module, expression, "TypeId 方法不接收参数"));
            }
            let ConstantValue::Type(ty) = self.value(module, base, state)? else {
                return Err(self.fail(module, expression, "comptime 方法要求 TypeId"));
            };
            return match self.name(module, name) {
                "name" => Ok(ConstantValue::String(self.describe(&ty))),
                "as_int" => Err(Diagnostic::error(
                    DiagnosticCode::LateComptime,
                    "TypeId.as_int 只能在类型集合冻结后求值",
                    Some(span),
                )
                .into()),
                _ => Err(self.fail(module, expression, "未知 TypeId 方法")),
            };
        }
        let ExprKind::Path(path) = arena.exprs[callee.0 as usize].kind else {
            return Err(self.fail(module, expression, "comptime 只支持直接路径调用"));
        };
        let segments = self.path(module, path);
        if segments == ["panic"] {
            return self.comptime_panic(module, args, &span, state);
        }
        self.check_capability(module, &segments, &span, state)?;
        let arguments = self.value_list(module, args, state)?;
        if state.domain == Domain::SOURCE_EXPAND
            && let Some(value) =
                self.expand_builtin(module, expression, &segments, &arguments, &span, state)?
        {
            return Ok(value);
        }
        let def = self
            .resolve(module, &segments)
            .map_err(|_| self.fail(module, expression, "无法在编译期解析被调函数"))?;
        if !matches!(
            self.modules[def.module].arena.items[def.item.0 as usize].kind,
            ItemKind::Function(_)
        ) {
            return Err(self.fail(module, expression, "被调端点不是函数"));
        }
        self.interpret_function(def, &arguments, &span, state)
    }

    /// SourceExpand 域的内建调用：`std.syntax.parse_*` 解析闸门与 `Ok`/`Err` 构造。
    ///
    /// 返回 `None` 表示不是内建，沿普通调用路径继续。解析闸门把生成文本经
    /// `ParseSource` query 送入主 lexer/parser；失败返回 `Err(SyntaxError)`
    /// 值，由脚本自行捕获或传播到宏边界。
    fn expand_builtin(
        &self,
        module: usize,
        expression: ExprId,
        segments: &[&str],
        arguments: &[ConstantValue],
        span: &crate::Span,
        state: &mut EvalState,
    ) -> EvalResult<Option<ConstantValue>> {
        match segments {
            ["Ok"] | ["Err"] if arguments.len() == 1 => {
                state.alloc(64u64.saturating_add(arguments[0].heap_bytes()), span)?;
                let payload = Box::new(arguments[0].clone());
                return Ok(Some(if segments == ["Ok"] {
                    ConstantValue::ResultOk(payload)
                } else {
                    ConstantValue::ResultErr(payload)
                }));
            }
            _ => {}
        }
        let canonical = self.external_path(module, segments);
        let Some(canonical) = canonical else {
            return Ok(None);
        };
        let slot = match canonical.as_str() {
            "std.syntax.parse_source" => state
                .expand
                .as_ref()
                .map(|expand| expand.slot)
                .ok_or_else(|| {
                    capability_error(
                        span,
                        "parse_source 需要源码宏插入上下文，请使用 parse_expr 等明确入口"
                            .to_owned(),
                    )
                })?,
            "std.syntax.parse_items" => SourceSlot::Item,
            "std.syntax.parse_expr" => SourceSlot::Expression,
            "std.syntax.parse_type" => SourceSlot::Type,
            "std.syntax.parse_pattern" => SourceSlot::Pattern,
            _ => return Ok(None),
        };
        let fail = |message: &str| self.fail(module, expression, message);
        let [ConstantValue::String(text)] = arguments else {
            return Err(fail("parse_* 需要恰好一个 string 参数"));
        };
        let queries = state
            .expand
            .as_ref()
            .map(|expand| expand.queries)
            .ok_or_else(|| capability_error(span, "parse_* 只能在源码宏脚本中调用".to_owned()))?;
        let mut key = blake3::Hasher::new_derive_key("gugu-parse-source-input-v1");
        key.update(&[slot_byte(slot)]);
        key.update(&(text.len() as u64).to_le_bytes());
        key.update(text.as_bytes());
        let input_fingerprint = *key.finalize().as_bytes();
        let query = QueryKey::new(
            QueryKind::ParseSource,
            PARSE_SOURCE_SCHEMA_VERSION,
            input_fingerprint,
        );
        let result = queries.compute(query, |_| match validate_source_fragment(text, slot) {
            Ok(()) => Ok((
                serde_json::to_vec(&ParseSourceOutcome {
                    ok: true,
                    message: String::new(),
                    offset: 0,
                })
                .expect("ParseSource 结果 schema 序列化"),
                Vec::new(),
            )),
            Err(error) => Ok((
                serde_json::to_vec(&ParseSourceOutcome {
                    ok: false,
                    message: error.0,
                    offset: error.1,
                })
                .expect("ParseSource 结果 schema 序列化"),
                Vec::new(),
            )),
        });
        let payload = result
            .map_err(|error| {
                Diagnostic::error(
                    DiagnosticCode::InvalidExpression,
                    format!("解析闸门 query 失败：{error}"),
                    Some(span.clone()),
                )
            })?
            .payload()
            .to_vec();
        let outcome: ParseSourceOutcome = serde_json::from_slice(&payload).map_err(|_| {
            Diagnostic::error(
                DiagnosticCode::InvalidExpression,
                "ParseSource query 缓存 schema 不合法",
                Some(span.clone()),
            )
        })?;
        if outcome.ok {
            state.alloc(
                64 + u64::try_from(text.len()).expect("源码长度可编码"),
                span,
            )?;
            return Ok(Some(ConstantValue::ResultOk(Box::new(
                ConstantValue::ParsedSource(ParsedFragment {
                    slot,
                    text: text.clone(),
                }),
            ))));
        }
        state.alloc(
            192 + 13 + u64::try_from(outcome.message.len()).expect("错误长度可编码"),
            span,
        )?;
        let mut fields = BTreeMap::new();
        fields.insert("message".to_owned(), ConstantValue::String(outcome.message));
        fields.insert(
            "offset".to_owned(),
            ConstantValue::Int(i128::from(outcome.offset)),
        );
        Ok(Some(ConstantValue::ResultErr(Box::new(
            ConstantValue::Struct(fields),
        ))))
    }

    fn comptime_panic(
        &self,
        module: usize,
        args: AstRange<ExprId>,
        span: &crate::Span,
        state: &mut EvalState,
    ) -> EvalResult<ConstantValue> {
        let arguments = self.value_list(module, args, state)?;
        let message = match arguments.as_slice() {
            [ConstantValue::String(message)] => message.as_str(),
            _ => "comptime panic",
        };
        Err(Diagnostic::error(
            DiagnosticCode::ComptimePanic,
            format!("编译期求值 panic：{message}"),
            Some(span.clone()),
        )
        .into())
    }

    fn interpret_function(
        &self,
        def: DefRef,
        arguments: &[ConstantValue],
        span: &crate::Span,
        state: &mut EvalState,
    ) -> EvalResult<ConstantValue> {
        let parsed = &self.modules[def.module];
        let ItemKind::Function(function) = parsed.arena.items[def.item.0 as usize].kind else {
            unreachable!("interpret_function 只处理函数项");
        };
        let declaration = &parsed.arena.fns[function.0 as usize];
        let name = declaration
            .name
            .map(|symbol| self.name(def.module, symbol))
            .unwrap_or("<匿名函数>")
            .to_owned();
        let body = match declaration.body {
            FnBody::None => {
                return Err(capability_error(
                    span,
                    format!("外部函数 `{name}` 不能在 comptime 求值"),
                )
                .into());
            }
            FnBody::Eq(body) | FnBody::Block(body) => body,
        };
        state.enter(&name, span)?;
        let result = self
            .bind_parameters(def.module, function, arguments, state)
            .map_err(EvalError::from)
            .and_then(|()| self.value(def.module, body, state));
        state.leave();
        match result {
            Ok(value) | Err(EvalError::Unwind(Unwind::Return(value))) => Ok(value),
            Err(error) => Err(error.diagnostic(span).into()),
        }
    }

    fn bind_parameters(
        &self,
        module: usize,
        function: super::super::super::ast::FnId,
        arguments: &[ConstantValue],
        state: &mut EvalState,
    ) -> Result<(), Diagnostic> {
        let parsed = &self.modules[module];
        let declaration = &parsed.arena.fns[function.0 as usize];
        let mut bound = 0;
        for (index, param) in declaration
            .params
            .as_slice(&parsed.arena.params)
            .iter()
            .enumerate()
        {
            if !parsed
                .configured
                .param_active(declaration.params.start as usize + index)
            {
                continue;
            }
            let Some(value) = arguments.get(bound) else {
                return Err(self.error(module, "comptime 调用实参数量不足"));
            };
            bound += 1;
            let Some(pat) = param.pat else {
                return Err(self.error(module, "comptime 调用的参数缺少绑定模式"));
            };
            self.bind_pattern(module, pat, value, state)?;
        }
        if bound != arguments.len() {
            return Err(self.error(module, "comptime 调用实参数量不符"));
        }
        Ok(())
    }

    fn bind_pattern(
        &self,
        module: usize,
        pat: PatId,
        value: &ConstantValue,
        state: &mut EvalState,
    ) -> Result<(), Diagnostic> {
        let arena = &self.modules[module].arena;
        let span = arena.pats[pat.0 as usize].span.clone();
        let fail = |message: &str| {
            Diagnostic::error(
                DiagnosticCode::InvalidPattern,
                message.to_owned(),
                Some(span.clone()),
            )
        };
        match arena.pats[pat.0 as usize].kind {
            PatKind::Wildcard => Ok(()),
            PatKind::Ident(name) => {
                state.alloc(value.heap_bytes(), &span)?;
                state.bind(self.name(module, name).to_owned(), value.clone());
                Ok(())
            }
            PatKind::Tuple(patterns) => {
                let ConstantValue::Tuple(elements) = value else {
                    return Err(fail("comptime 解构要求元组值"));
                };
                let patterns = patterns.as_slice(&arena.pat_ids);
                if patterns.len() != elements.len() {
                    return Err(fail("comptime 解构的元组长度不一致"));
                }
                for (pattern, element) in patterns.iter().zip(elements) {
                    self.bind_pattern(module, *pattern, element, state)?;
                }
                Ok(())
            }
            _ => Err(fail("comptime 求值只支持标识符、通配与元组绑定模式")),
        }
    }

    fn block(
        &self,
        module: usize,
        stmts: AstRange<StmtId>,
        tail: Option<ExprId>,
        state: &mut EvalState,
    ) -> EvalResult<ConstantValue> {
        let arena = &self.modules[module].arena;
        for &statement in stmts.as_slice(&arena.stmt_ids) {
            state.step(&self.modules[module].file.eof_span)?;
            self.statement(module, statement, state)?;
        }
        match tail {
            Some(tail) => self.value(module, tail, state),
            _ => Ok(ConstantValue::Unit),
        }
    }

    fn statement(&self, module: usize, statement: StmtId, state: &mut EvalState) -> EvalResult<()> {
        let arena = &self.modules[module].arena;
        let span = arena.stmts[statement.0 as usize].span.clone();
        let unsupported = |message: &str| {
            Diagnostic::error(
                DiagnosticCode::InvalidExpression,
                message.to_owned(),
                Some(span.clone()),
            )
            .into()
        };
        match arena.stmts[statement.0 as usize].kind {
            StmtKind::Let { pat, init, .. } => {
                let value = match init {
                    Some(init) => self.value(module, init, state)?,
                    None => ConstantValue::Unit,
                };
                self.bind_pattern(module, pat, &value, state)
                    .map_err(EvalError::from)
            }
            StmtKind::Assign { op, place, value } => {
                let value = self.value(module, value, state)?;
                self.assign(module, place, op, value, state)
                    .map_err(EvalError::from)
            }
            StmtKind::Expr { expr, .. } => {
                self.value(module, expr, state)?;
                Ok(())
            }
            StmtKind::Static { .. } | StmtKind::Defer { .. } | StmtKind::Yield => {
                Err(unsupported("comptime 求值不支持该语句"))
            }
            StmtKind::SourceMacro { .. } => Err(unsupported("comptime source 属于源码宏展开域")),
        }
    }

    fn assign(
        &self,
        module: usize,
        place: ExprId,
        op: AssignOp,
        value: ConstantValue,
        state: &mut EvalState,
    ) -> Result<(), Diagnostic> {
        let arena = &self.modules[module].arena;
        let span = arena.exprs[place.0 as usize].span.clone();
        let deny = |message: &str| {
            Diagnostic::error(
                DiagnosticCode::InvalidExpression,
                message.to_owned(),
                Some(span.clone()),
            )
        };
        let ExprKind::Path(path) = arena.exprs[place.0 as usize].kind else {
            return Err(deny("comptime 赋值目标必须是局部绑定"));
        };
        let segments = self.path(module, path);
        let [name] = &segments[..] else {
            return Err(deny("comptime 赋值目标必须是局部绑定"));
        };
        let Some(slot) = state.slot_mut(name) else {
            return Err(deny("comptime 赋值只能写入局部绑定"));
        };
        let previous = std::mem::replace(slot, ConstantValue::Unit);
        if op == AssignOp::Add {
            charge_concat(BinOp::Add, &previous, &value, state, &span)?;
        }
        let combined = match op {
            AssignOp::Assign => value,
            _ => evaluate(
                match op {
                    AssignOp::Add => BinOp::Add,
                    AssignOp::Sub => BinOp::Sub,
                    AssignOp::Mul => BinOp::Mul,
                    AssignOp::Div => BinOp::Div,
                    AssignOp::Rem => BinOp::Rem,
                    AssignOp::BitAnd => BinOp::BitAnd,
                    AssignOp::BitOr => BinOp::BitOr,
                    AssignOp::BitXor => BinOp::BitXor,
                    AssignOp::Shl => BinOp::Shl,
                    AssignOp::Shr => BinOp::Shr,
                    AssignOp::Assign => unreachable!("赋值分支已处理"),
                },
                previous,
                value,
            )
            .ok_or_else(|| deny("comptime 复合赋值操作数类型不符"))?,
        };
        *state.slot_mut(name).expect("赋值期间局部槽不改变") = combined;
        Ok(())
    }
}

fn charge_concat(
    op: BinOp,
    left: &ConstantValue,
    right: &ConstantValue,
    state: &mut EvalState,
    span: &crate::Span,
) -> Result<(), Diagnostic> {
    if op == BinOp::Add
        && matches!(
            (left, right),
            (ConstantValue::String(_), ConstantValue::String(_))
        )
    {
        state.alloc(left.heap_bytes().saturating_add(right.heap_bytes()), span)?;
    }
    Ok(())
}

fn display_value(value: &ConstantValue) -> String {
    match value {
        ConstantValue::Unit => "()".to_owned(),
        ConstantValue::Int(value) => value.to_string(),
        ConstantValue::Float(bits) => f64::from_bits(*bits).to_string(),
        ConstantValue::Bool(value) => value.to_string(),
        ConstantValue::String(value) => value.clone(),
        ConstantValue::Type(_) => "TypeId".to_owned(),
        ConstantValue::Array(_) | ConstantValue::Tuple(_) | ConstantValue::Struct(_) => {
            "…".to_owned()
        }
        ConstantValue::ParsedSource(_) => "<parsed source>".to_owned(),
        ConstantValue::ResultOk(_) => "Ok(…)".to_owned(),
        ConstantValue::ResultErr(_) => "Err(…)".to_owned(),
    }
}

/// `ParseSource` query 的确定性结果。
#[derive(serde::Serialize, serde::Deserialize)]
struct ParseSourceOutcome {
    ok: bool,
    message: String,
    offset: u32,
}

/// source slot 的稳定字节编码。
pub(in crate::frontend) fn slot_byte(slot: SourceSlot) -> u8 {
    match slot {
        SourceSlot::Item => 1,
        SourceSlot::Statement => 2,
        SourceSlot::Expression => 3,
        SourceSlot::Type => 4,
        SourceSlot::Pattern => 5,
    }
}

/// 把生成文本送入主 lexer/parser 的解析闸门；失败返回首个语法错误与字节偏移。
///
/// 使用一次性源码表独立解析，成功只代表语法有效；语义正确性由拼接后的
/// 主前端链保证。
fn validate_source_fragment(text: &str, slot: SourceSlot) -> Result<(), (String, u32)> {
    let kind = match slot {
        SourceSlot::Item => crate::frontend::parse::FragmentKind::Items,
        SourceSlot::Statement => crate::frontend::parse::FragmentKind::Statements,
        SourceSlot::Expression => crate::frontend::parse::FragmentKind::Expression,
        SourceSlot::Type => crate::frontend::parse::FragmentKind::Type,
        SourceSlot::Pattern => crate::frontend::parse::FragmentKind::Pattern,
    };
    let snapshot = SourceSnapshot::from_str(std::path::Path::new("<parsed-source>"), text)
        .map_err(|_| ("生成文本不是合法 UTF-8 源码".to_owned(), 0_u32))?;
    let mut sources = SourceMap::empty();
    let file = sources
        .push_snapshot(snapshot)
        .map_err(|_| ("生成文本无法注册源码快照".to_owned(), 0_u32))?;
    let snapshot = sources.snapshot(file).expect("快照已注册");
    let lexed = crate::frontend::lex::lex_in_expansion(snapshot, &sources, file, ExpansionId::ROOT);
    if let Some(first) = lexed.diagnostics.first() {
        return Err((
            first.message().to_owned(),
            first.span().map_or(0, |s| s.start()),
        ));
    }
    let mut buffer = lexed.buffer;
    if buffer.has_error_tokens() {
        return Err(("生成文本包含词法错误记号".to_owned(), 0));
    }
    let (_, _, diagnostics) = crate::frontend::parse::parse_fragment(
        text,
        &sources,
        file,
        ExpansionId::ROOT,
        kind,
        Default::default(),
        &mut buffer,
    );
    match diagnostics.first() {
        Some(first) => Err((
            first.message().to_owned(),
            first.span().map_or(0, |s| s.start()),
        )),
        None => Ok(()),
    }
}

/// 裸 lang item 身份；其余 std 身份由导入解析给出。
fn lang_item_identity(segments: &[&str]) -> Option<String> {
    match segments {
        ["panic"] => Some("panic".to_owned()),
        ["size_of"] => Some("size_of".to_owned()),
        ["align_of"] => Some("align_of".to_owned()),
        ["offset_of"] => Some("offset_of".to_owned()),
        ["type_id"] => Some("type_id".to_owned()),
        ["type_id_count"] => Some("type_id_count".to_owned()),
        _ => None,
    }
}

fn in_type(value: ConstantValue, ty: &Ty) -> ConstantValue {
    match (value, ty) {
        (ConstantValue::Float(bits), Ty::Float(32)) => {
            ConstantValue::Float(f64::from(f64::from_bits(bits) as f32).to_bits())
        }
        (ConstantValue::Int(value), Ty::Int { signed, bits }) if *bits < 128 => {
            debug_assert!(*bits > 0, "整数类型至少占一位");
            let value = if *signed {
                (value << (128 - bits)) >> (128 - bits)
            } else {
                value & ((1i128 << bits) - 1)
            };
            ConstantValue::Int(value)
        }
        (value, _) => value,
    }
}

fn evaluate(op: BinOp, a: ConstantValue, b: ConstantValue) -> Option<ConstantValue> {
    Some(match (a, b) {
        (ConstantValue::Type(a), ConstantValue::Type(b)) => match op {
            BinOp::Eq => ConstantValue::Bool(a == b),
            BinOp::Ne => ConstantValue::Bool(a != b),
            _ => return None,
        },
        (ConstantValue::Int(a), ConstantValue::Int(b)) => match op {
            BinOp::Add => ConstantValue::Int(a.checked_add(b)?),
            BinOp::Sub => ConstantValue::Int(a.checked_sub(b)?),
            BinOp::Mul => ConstantValue::Int(a.checked_mul(b)?),
            BinOp::Div => ConstantValue::Int(a.checked_div(b)?),
            BinOp::Rem => ConstantValue::Int(a.checked_rem(b)?),
            BinOp::BitAnd => ConstantValue::Int(a & b),
            BinOp::BitOr => ConstantValue::Int(a | b),
            BinOp::BitXor => ConstantValue::Int(a ^ b),
            BinOp::Shl => ConstantValue::Int(a.checked_shl(u32::try_from(b).ok()?)?),
            BinOp::Shr => ConstantValue::Int(a.checked_shr(u32::try_from(b).ok()?)?),
            BinOp::Eq => ConstantValue::Bool(a == b),
            BinOp::Ne => ConstantValue::Bool(a != b),
            BinOp::Lt => ConstantValue::Bool(a < b),
            BinOp::Le => ConstantValue::Bool(a <= b),
            BinOp::Gt => ConstantValue::Bool(a > b),
            BinOp::Ge => ConstantValue::Bool(a >= b),
            _ => return None,
        },
        (ConstantValue::Bool(a), ConstantValue::Bool(b)) => match op {
            BinOp::And => ConstantValue::Bool(a && b),
            BinOp::Or => ConstantValue::Bool(a || b),
            BinOp::Eq => ConstantValue::Bool(a == b),
            BinOp::Ne => ConstantValue::Bool(a != b),
            _ => return None,
        },
        (ConstantValue::String(mut a), ConstantValue::String(b)) => match op {
            BinOp::Add => {
                a.push_str(&b);
                ConstantValue::String(a)
            }
            BinOp::Eq => ConstantValue::Bool(a == b),
            BinOp::Ne => ConstantValue::Bool(a != b),
            _ => return None,
        },
        (ConstantValue::Float(a), ConstantValue::Float(b)) => {
            let (a, b) = (f64::from_bits(a), f64::from_bits(b));
            match op {
                BinOp::Add => ConstantValue::Float((a + b).to_bits()),
                BinOp::Sub => ConstantValue::Float((a - b).to_bits()),
                BinOp::Mul => ConstantValue::Float((a * b).to_bits()),
                BinOp::Div => ConstantValue::Float((a / b).to_bits()),
                BinOp::Rem => ConstantValue::Float((a % b).to_bits()),
                BinOp::Eq => ConstantValue::Bool(a == b),
                BinOp::Ne => ConstantValue::Bool(a != b),
                BinOp::Lt => ConstantValue::Bool(a < b),
                BinOp::Le => ConstantValue::Bool(a <= b),
                BinOp::Gt => ConstantValue::Bool(a > b),
                BinOp::Ge => ConstantValue::Bool(a >= b),
                _ => return None,
            }
        }
        _ => return None,
    })
}
