//! 覆盖矩阵按构造器和不相交标量区间分割，不枚举标量值。
use super::super::{ast::*, intern::Symbol};
use super::model::{Model, Ty};
use super::numeric::{domain, literal_ordinal, ordinal};
use crate::{Diagnostic, DiagnosticCode, Span};
use std::collections::{BTreeMap, BTreeSet};
#[derive(Clone)]
pub(crate) struct Binding {
    pub(crate) name: Symbol,
    pub(crate) ty: Ty,
    pub(crate) span: Span,
}
pub(crate) struct CheckedPattern {
    pub(crate) bindings: Vec<Binding>,
    pub(crate) irrefutable: bool,
}
#[derive(Clone, Debug)]
enum P {
    Any,
    Scalar(u128, u128),
    Ctor(String, Vec<P>),
    Or(Vec<P>),
    Seq(Vec<P>, Option<Vec<P>>),
}
struct C<'m, 'a> {
    model: &'m Model<'a>,
    early: &'m super::comptime::EarlyConstTable,
    module: usize,
    bindings: BTreeMap<Symbol, Binding>,
    errors: Vec<Diagnostic>,
}
pub(crate) fn check(
    model: &Model<'_>,
    early: &super::comptime::EarlyConstTable,
    module: usize,
    pat: PatId,
    ty: &Ty,
) -> Result<CheckedPattern, Vec<Diagnostic>> {
    let mut c = C {
        model,
        early,
        module,
        bindings: BTreeMap::new(),
        errors: Vec::new(),
    };
    let p = c.pattern(pat, ty);
    if c.errors.is_empty() {
        let mut bindings: Vec<_> = c.bindings.into_values().collect();
        bindings.sort_by_key(|binding| binding.span.start());
        Ok(CheckedPattern {
            bindings,
            irrefutable: covered(model, &[ty.clone()], &[vec![p]]),
        })
    } else {
        Err(c.errors)
    }
}
pub(crate) fn exhaustive(
    model: &Model<'_>,
    early: &super::comptime::EarlyConstTable,
    module: usize,
    ty: &Ty,
    arms: &[(PatId, bool)],
) -> Result<bool, Vec<Diagnostic>> {
    let mut rows = Vec::new();
    let mut errors = Vec::new();
    for &(pat, guarded) in arms {
        let mut c = C {
            model,
            early,
            module,
            bindings: BTreeMap::new(),
            errors: Vec::new(),
        };
        let p = c.pattern(pat, ty);
        errors.extend(c.errors);
        if !guarded {
            rows.push(vec![p]);
        }
    }
    if errors.is_empty() {
        Ok(covered(model, &[ty.clone()], &rows))
    } else {
        Err(errors)
    }
}
impl C<'_, '_> {
    /// 优先消费 EarlyConstTable 中已求值的整数常量。
    fn evaluated_int(&self, expression: ExprId) -> Option<i128> {
        match self.early.expression_value(self.module, expression.0) {
            Some(super::comptime::eval::ConstantValue::Int(value)) => Some(*value),
            _ => self.model.constant_int(self.module, expression).ok(),
        }
    }
    fn error(&mut self, message: &str, span: &Span) {
        self.errors.push(Diagnostic::error(
            DiagnosticCode::InvalidPattern,
            message,
            Some(span.clone()),
        ));
    }
    fn bind(&mut self, name: Symbol, ty: &Ty, span: &Span) {
        if self
            .bindings
            .insert(
                name,
                Binding {
                    name,
                    ty: ty.clone(),
                    span: span.clone(),
                },
            )
            .is_some()
        {
            self.error("模式重复绑定同一个名字", span);
        }
    }
    fn pattern(&mut self, id: PatId, ty: &Ty) -> P {
        let a = &self.model.modules[self.module].arena;
        let pat = &a.pats[id.0 as usize];
        if let PatKind::Ref(inner) = pat.kind {
            return if let Ty::Ref(t) = ty {
                self.pattern(inner, t)
            } else {
                self.error("引用模式要求引用类型", &pat.span);
                P::Any
            };
        }
        if let PatKind::Ident(name) = pat.kind {
            let text = self.model.name(self.module, name);
            if let Ok(Some((_, ctor))) = self.model.constructor(self.module, &[text], Some(ty))
                && ctor.fields.is_empty()
            {
                return P::Ctor(ctor.name, vec![]);
            }
            self.bind(name, ty, &pat.span);
            return P::Any;
        }
        if let PatKind::At {
            name, pat: inner, ..
        } = pat.kind
        {
            self.bind(name, ty, &pat.span);
            return self.pattern(inner, ty);
        }
        let original_ty = ty;
        let ty = ty.deref();
        match &pat.kind {
            PatKind::Wildcard => P::Any,
            PatKind::Ident(_) | PatKind::At { .. } => unreachable!("绑定保留原始引用类型"),
            PatKind::Or(alts) => {
                let baseline = self.bindings.clone();
                let mut arms = Vec::new();
                let mut common = None;
                for &alt in alts.as_slice(&a.pat_ids) {
                    self.bindings = baseline.clone();
                    arms.push(self.pattern(alt, original_ty));
                    if let Some(previous) = &common {
                        let previous: &BTreeMap<Symbol, Binding> = previous;
                        if previous.len() != self.bindings.len()
                            || previous.iter().any(|(k, v)| {
                                self.bindings.get(k).is_none_or(|other| other.ty != v.ty)
                            })
                        {
                            self.error("or 模式必须绑定相同名字及类型", &pat.span);
                        }
                    } else {
                        common = Some(self.bindings.clone());
                    }
                }
                self.bindings = common.unwrap_or(baseline);
                P::Or(arms)
            }
            PatKind::Literal(lit) | PatKind::NegativeLiteral(lit) => {
                match literal_ordinal(a, *lit, matches!(pat.kind, PatKind::NegativeLiteral(_)), ty)
                {
                    Some(value) => P::Scalar(value, value),
                    None => {
                        self.error("模式字面量与目标类型不兼容或超出范围", &pat.span);
                        P::Any
                    }
                }
            }
            PatKind::Range { start, end } => {
                if !matches!(
                    ty,
                    Ty::Char
                        | Ty::Int {
                            signed: true,
                            bits: 64
                        }
                ) {
                    self.error("范围模式只支持 int 或 char", &pat.span);
                }
                if self.model.constant_type(self.module, *start).as_ref() != Ok(ty)
                    || self.model.constant_type(self.module, *end).as_ref() != Ok(ty)
                {
                    self.error("范围端点类型必须与被匹配类型相同", &pat.span);
                }
                let x = self.evaluated_int(*start);
                let y = self.evaluated_int(*end);
                match (x, y) {
                    (Some(x), Some(y))
                        if x < y && ordinal(x, ty).is_some() && ordinal(y, ty).is_some() =>
                    {
                        P::Scalar(
                            ordinal(x, ty).expect("已验证端点"),
                            ordinal(y, ty).expect("已验证端点") - 1,
                        )
                    }
                    _ => {
                        self.error("范围端点必须是同类型常量且下界小于上界", &pat.span);
                        P::Any
                    }
                }
            }
            PatKind::Tuple(ps) => {
                let fields = match ty {
                    Ty::Tuple(ts) => ts.clone(),
                    Ty::Unit => vec![],
                    _ => {
                        self.error("元组模式要求元组类型", &pat.span);
                        vec![]
                    }
                };
                let ps = ps.as_slice(&a.pat_ids);
                if ps.len() != fields.len() {
                    self.error("元组模式长度不符", &pat.span);
                }
                P::Ctor(
                    "tuple".into(),
                    ps.iter()
                        .zip(fields)
                        .map(|(&p, t)| self.pattern(p, &t))
                        .collect(),
                )
            }
            PatKind::Array {
                prefix,
                rest,
                suffix,
            } => {
                let (elem, len) = match ty {
                    Ty::Array(t, n) => (t.as_ref(), Some(*n)),
                    Ty::Slice(t) => (t.as_ref(), None),
                    _ => {
                        self.error("数组模式要求数组或切片", &pat.span);
                        return P::Any;
                    }
                };
                let n = u64::from(prefix.len) + u64::from(suffix.len);
                if let Some(len) = len {
                    if n > len || (rest.is_none() && n != len) {
                        self.error("数组模式长度不符", &pat.span);
                    }
                }
                if let Some(rest) = rest {
                    if let Some(name) = rest.name {
                        let rest_ty = match len {
                            Some(len) => Ty::Array(Box::new(elem.clone()), len.saturating_sub(n)),
                            None => Ty::Ref(Box::new(Ty::Slice(Box::new(elem.clone())))),
                        };
                        self.bind(name, &rest_ty, &rest.span);
                    }
                }
                let pre = prefix
                    .as_slice(&a.pat_ids)
                    .iter()
                    .map(|&p| self.pattern(p, elem))
                    .collect();
                let suf = suffix
                    .as_slice(&a.pat_ids)
                    .iter()
                    .map(|&p| self.pattern(p, elem))
                    .collect::<Vec<_>>();
                if rest.is_some() {
                    P::Seq(pre, Some(suf))
                } else {
                    let mut ps: Vec<P> = pre;
                    ps.extend(suf);
                    P::Seq(ps, None)
                }
            }
            PatKind::Constructor { path, fields } => {
                let parts = self.model.path(self.module, *path);
                match self.model.constructor(self.module, &parts, Some(ty)) {
                    Ok(Some((actual, ctor))) => {
                        if &actual != ty || ctor.record {
                            self.error("构造器模式类型或形状不符", &pat.span);
                        }
                        let ps = fields.as_slice(&a.pat_ids);
                        if ps.len() != ctor.fields.len() {
                            self.error("构造器模式字段数不符", &pat.span);
                        }
                        let fields = ps
                            .iter()
                            .zip(&ctor.fields)
                            .map(|(&p, f)| {
                                if !f.public && self.model.def_module(ty) != Some(self.module) {
                                    self.error("不能匹配私有字段", &pat.span);
                                }
                                self.pattern(p, &f.ty)
                            })
                            .collect();
                        P::Ctor(ctor.name, fields)
                    }
                    _ => {
                        self.error("未知模式构造器", &pat.span);
                        P::Any
                    }
                }
            }
            PatKind::Struct { path, fields, rest } => {
                let parts = self.model.path(self.module, *path);
                match self.model.constructor(self.module, &parts, Some(ty)) {
                    Ok(Some((actual, ctor))) => {
                        if &actual != ty || !ctor.record {
                            self.error("结构体模式类型或形状不符", &pat.span);
                        }
                        let mut ps = vec![P::Any; ctor.fields.len()];
                        let mut seen = BTreeSet::new();
                        for f in fields.as_slice(&a.field_pats) {
                            let name = self.model.name(self.module, f.name);
                            if !seen.insert(name) {
                                self.error("模式重复字段", &f.span);
                            }
                            if let Some((i, field)) =
                                ctor.fields.iter().enumerate().find(|(_, f)| f.name == name)
                            {
                                if !field.public && self.model.def_module(ty) != Some(self.module) {
                                    self.error("模式不能点名私有字段", &f.span);
                                }
                                if let Some(p) = f.pat {
                                    ps[i] = self.pattern(p, &field.ty);
                                } else {
                                    self.bind(f.name, &field.ty, &f.span);
                                }
                            } else {
                                self.error("模式字段不存在", &f.span);
                            }
                        }
                        if !rest && seen.len() != ctor.fields.len() {
                            self.error("省略字段必须使用 ..", &pat.span);
                        }
                        P::Ctor(ctor.name, ps)
                    }
                    _ => {
                        self.error("未知结构体模式", &pat.span);
                        P::Any
                    }
                }
            }
            PatKind::Ref(_) => unreachable!(),
            PatKind::SourceMacro { .. } | PatKind::Error => {
                self.error("模式尚未完成展开或解析", &pat.span);
                P::Any
            }
        }
    }
}
fn expand(rows: &[Vec<P>]) -> Vec<Vec<P>> {
    let mut out = Vec::new();
    for row in rows {
        match row.first() {
            Some(P::Or(alts)) => {
                for p in alts {
                    let mut next = row.clone();
                    next[0] = p.clone();
                    out.extend(expand(&[next]));
                }
            }
            _ => out.push(row.clone()),
        }
    }
    out
}
fn covered(model: &Model<'_>, types: &[Ty], rows: &[Vec<P>]) -> bool {
    if types.is_empty() {
        return !rows.is_empty();
    }
    if rows.iter().any(|r| r.iter().all(|p| matches!(p, P::Any))) {
        return true;
    }
    if rows.is_empty() {
        return types
            .iter()
            .any(|ty| uninhabited(model, ty, &mut Vec::new()));
    }
    let rows = expand(rows);
    if rows.iter().all(|row| matches!(row[0], P::Any)) {
        let remaining: Vec<_> = rows.iter().map(|row| row[1..].to_vec()).collect();
        return covered(model, &types[1..], &remaining);
    }
    let ty = types[0].deref();
    if *ty == Ty::Never {
        return true;
    }
    let specialize = |name: &str, fields: &[Ty]| {
        let mut specialized = Vec::new();
        for row in &rows {
            let mut prefix = match &row[0] {
                P::Any => vec![P::Any; fields.len()],
                P::Ctor(n, ps) if n == name => ps.clone(),
                _ => continue,
            };
            prefix.extend_from_slice(&row[1..]);
            specialized.push(prefix);
        }
        let mut ts = fields.to_vec();
        ts.extend_from_slice(&types[1..]);
        covered(model, &ts, &specialized)
    };
    if let Some(variants) = model.variants(ty) {
        return variants.iter().all(|v| {
            specialize(
                &v.name,
                &v.fields.iter().map(|f| f.ty.clone()).collect::<Vec<_>>(),
            )
        });
    }
    match ty {
        Ty::Tuple(ts) => return specialize("tuple", ts),
        Ty::Unit => return specialize("tuple", &[]),
        Ty::Array(elem, _) | Ty::Slice(elem) => {
            let fixed = if let Ty::Array(_, n) = ty {
                Some(*n)
            } else {
                None
            };
            let max = rows
                .iter()
                .filter_map(|r| match &r[0] {
                    P::Seq(pre, suf) => Some(pre.len() + suf.as_ref().map_or(0, Vec::len)),
                    _ => None,
                })
                .max()
                .unwrap_or(0);
            // 模式触及的前缀和后缀各自至多 max；长度到 2*max 后不再相交。
            let lengths: Vec<u64> =
                fixed.map_or_else(|| (0..=(max as u64 * 2).max(1)).collect(), |n| vec![n]);
            return lengths.into_iter().all(|n| {
                let indices: Vec<u64> = (0..(max as u64).min(n))
                    .chain(n.saturating_sub(max as u64)..n)
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .collect();
                let mut specialized = Vec::new();
                for row in &rows {
                    let mut ps = match &row[0] {
                        P::Any => vec![P::Any; indices.len()],
                        P::Seq(pre, suf)
                            if suf.as_ref().map_or(n == pre.len() as u64, |s| {
                                n >= (pre.len() + s.len()) as u64
                            }) =>
                        {
                            indices
                                .iter()
                                .map(|&i| {
                                    if i < pre.len() as u64 {
                                        pre[i as usize].clone()
                                    } else if let Some(suf) = suf {
                                        if i >= n - suf.len() as u64 {
                                            suf[(i - (n - suf.len() as u64)) as usize].clone()
                                        } else {
                                            P::Any
                                        }
                                    } else {
                                        P::Any
                                    }
                                })
                                .collect()
                        }
                        _ => continue,
                    };
                    ps.extend_from_slice(&row[1..]);
                    specialized.push(ps);
                }
                let mut ts = vec![(**elem).clone(); indices.len()];
                ts.extend_from_slice(&types[1..]);
                covered(model, &ts, &specialized)
            });
        }
        _ => {}
    }
    if let Some((lo, hi)) = domain(ty) {
        let mut points = BTreeSet::from([lo]);
        if *ty == Ty::Char {
            points.extend([0xD800, 0xE000]);
        }
        for row in &rows {
            if let P::Scalar(a, b) = row[0] {
                points.insert(a);
                if b < hi {
                    points.insert(b + 1);
                }
            }
        }
        let points: Vec<_> = points.into_iter().collect();
        return points.iter().enumerate().all(|(i, &start)| {
            let end = points.get(i + 1).map_or(hi, |end| end - 1);
            if *ty == Ty::Char && start >= 0xD800 && end < 0xE000 {
                return true;
            }
            let rows: Vec<_> = rows
                .iter()
                .filter(|r| {
                    matches!(r[0], P::Any) || matches!(r[0],P::Scalar(a,b) if a<=start && b>=end)
                })
                .map(|r| r[1..].to_vec())
                .collect();
            covered(model, &types[1..], &rows)
        });
    }
    let rows: Vec<_> = rows
        .iter()
        .filter(|r| matches!(r[0], P::Any))
        .map(|r| r[1..].to_vec())
        .collect();
    covered(model, &types[1..], &rows)
}

fn uninhabited(model: &Model<'_>, ty: &Ty, active: &mut Vec<Ty>) -> bool {
    if active.contains(ty) {
        return false;
    }
    active.push(ty.clone());
    let result = match ty {
        Ty::Never => true,
        Ty::Ref(t) => uninhabited(model, t, active),
        Ty::Tuple(ts) => ts.iter().any(|t| uninhabited(model, t, active)),
        Ty::Array(t, n) => *n != 0 && uninhabited(model, t, active),
        _ => model.variants(ty).is_some_and(|vs| {
            vs.iter()
                .all(|v| v.fields.iter().any(|f| uninhabited(model, &f.ty, active)))
        }),
    };
    active.pop();
    result
}
