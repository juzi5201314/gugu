//! 公共效果只保留参数序号与隐藏状态标志，不外泄局部变量、捕获或私有定义身份。

use super::types::FunctionSummary;
use crate::frontend::ast::UnOp;
use crate::frontend::hir::{self, ExprId, ExprKind, Module, Owner, PatternKind, Res};
use crate::frontend::mono::instantiate::CallSite;

pub(crate) fn summarize(
    module: &Module,
    owner: &Owner,
    callees: &dyn Fn(CallSite) -> Option<FunctionSummary>,
    summary: &mut FunctionSummary,
) {
    // LocalId 连续；参数序号无 64 个的上界，使用稠密局部索引与排序后的参数序列。
    let mut parameters = vec![None; owner.locals.len()];
    for (index, &pattern) in owner.parameters.iter().enumerate() {
        bind_parameters(
            owner,
            pattern,
            u32::try_from(index).expect("参数数量不超过 u32"),
            &mut parameters,
        );
    }
    for capture in &owner.captures {
        summary.reads_hidden_state |= capture.read_before_write;
        summary.writes_hidden_state |= capture.written;
    }
    for (index, expression) in owner.expressions.iter().enumerate() {
        if let ExprKind::Resolved(Res::Local(local)) = expression.kind
            && let Some(parameter) = parameters[local.index()]
        {
            summary.read_params.push(parameter);
        }
        if let ExprKind::Call {
            target,
            receiver,
            arguments,
        }
        | ExprKind::SpawnCall {
            target,
            receiver,
            arguments,
        } = &expression.kind
        {
            let callee = callees(CallSite::Expression(
                u32::try_from(index).expect("ExprId 不超过 u32"),
            ))
            .unwrap_or_else(|| {
                if matches!(
                    target,
                    hir::CallTarget::Builtin(_) | hir::CallTarget::Constructor { .. }
                ) {
                    FunctionSummary::default()
                } else {
                    FunctionSummary::conservative()
                }
            });
            let arguments = &owner.expression_ids[range(arguments)];
            let supplied = receiver.iter().copied().chain(arguments.iter().copied());
            for (index, argument) in supplied.enumerate() {
                let index = u32::try_from(index).expect("实参数量不超过 u32");
                let read =
                    callee.unknown_param_access || callee.read_params.binary_search(&index).is_ok();
                let write = callee.unknown_param_access
                    || callee.write_params.binary_search(&index).is_ok();
                access_argument(owner, &parameters, argument, read, write, summary);
            }
        }
        if let ExprKind::Closure { definition } | ExprKind::Spawn { definition } = expression.kind
            && let Some(nested) = module
                .owners
                .iter()
                .find(|nested| nested.definition == definition)
        {
            for capture in &nested.captures {
                if capture.owner == owner.definition
                    && capture.read_before_write
                    && let Some(parameter) = parameters[capture.source.index()]
                {
                    summary.read_params.push(parameter);
                }
            }
        }
    }
    for statement in &owner.statements {
        if let hir::StatementKind::Assign { place, .. } = statement.kind
            && !matches!(
                owner.expressions[place.index()].kind,
                ExprKind::Resolved(Res::Local(_))
            )
        {
            access_argument(owner, &parameters, place, false, true, summary);
        }
    }
    // 协议操作的接收者可能是编译器生成的迭代器或 try 载荷；无法投影到单个参数时取并集。
    for index in 0..owner.dispatches.len() {
        if let Some(callee) = callees(CallSite::Dispatch(
            u32::try_from(index).expect("dispatch 数量不超过 u32"),
        )) {
            if callee.unknown_param_access || !callee.read_params.is_empty() {
                summary
                    .read_params
                    .extend(0..u32::try_from(owner.parameters.len()).expect("参数数量不超过 u32"));
            }
            if callee.unknown_param_access || !callee.write_params.is_empty() {
                summary
                    .write_params
                    .extend(0..u32::try_from(owner.parameters.len()).expect("参数数量不超过 u32"));
            }
        }
    }
    summary.read_params.sort_unstable();
    summary.read_params.dedup();
    summary.write_params.sort_unstable();
    summary.write_params.dedup();
}

fn access_argument(
    owner: &Owner,
    parameters: &[Option<u32>],
    argument: ExprId,
    read: bool,
    write: bool,
    summary: &mut FunctionSummary,
) {
    if !read && !write {
        return;
    }
    match root(owner, argument) {
        Res::Local(local) if parameters[local.index()].is_some() => {
            let parameter = parameters[local.index()].expect("匹配分支已检查参数身份");
            if read {
                summary.read_params.push(parameter);
            }
            if write {
                summary.write_params.push(parameter);
            }
        }
        Res::Local(_) => {
            // 局部别名可能指向参数或隐藏堆对象，不能把未知别名解释为无访问。
            let count = u32::try_from(owner.parameters.len()).expect("参数数量不超过 u32");
            if read {
                summary.read_params.extend(0..count);
            }
            if write {
                summary.write_params.extend(0..count);
            }
            summary.reads_hidden_state |= read;
            summary.writes_hidden_state |= write;
        }
        _ => {
            summary.reads_hidden_state |= read;
            summary.writes_hidden_state |= write;
        }
    }
    if write {
        summary.alias_heap = true;
        summary.may_mutate_len = true;
    }
}

fn root(owner: &Owner, expression: ExprId) -> Res {
    match &owner.expressions[expression.index()].kind {
        ExprKind::Resolved(resolution) => resolution.clone(),
        ExprKind::Field { base, .. }
        | ExprKind::Index { base, .. }
        | ExprKind::Slice { base, .. }
        | ExprKind::Unary {
            operation: UnOp::Deref | UnOp::Ref,
            value: base,
        } => root(owner, *base),
        _ => Res::Primitive(owner.expression_types[expression.index()]),
    }
}

fn bind_parameters(
    owner: &Owner,
    pattern: hir::PatternId,
    index: u32,
    parameters: &mut [Option<u32>],
) {
    let bind_range = |ids: &std::ops::Range<u32>, parameters: &mut [Option<u32>]| {
        for &pattern in &owner.pattern_ids[range(ids)] {
            bind_parameters(owner, pattern, index, parameters);
        }
    };
    match &owner.patterns[pattern.index()].kind {
        PatternKind::Bind(local) => parameters[local.index()] = Some(index),
        PatternKind::At { local, pattern } => {
            parameters[local.index()] = Some(index);
            bind_parameters(owner, *pattern, index, parameters);
        }
        PatternKind::Ref(pattern) => bind_parameters(owner, *pattern, index, parameters),
        PatternKind::Tuple(ids) | PatternKind::Or(ids) => bind_range(ids, parameters),
        PatternKind::Array {
            prefix,
            rest,
            suffix,
            ..
        } => {
            bind_range(prefix, parameters);
            if let Some(local) = rest {
                parameters[local.index()] = Some(index);
            }
            bind_range(suffix, parameters);
        }
        PatternKind::Construct { fields, .. } => {
            for field in &owner.pattern_fields[range(fields)] {
                bind_parameters(owner, field.pattern, index, parameters);
            }
        }
        PatternKind::Wildcard | PatternKind::Literal(_) | PatternKind::Range { .. } => {}
    }
}

fn range(range: &std::ops::Range<u32>) -> std::ops::Range<usize> {
    usize::try_from(range.start).expect("HIR range 适配宿主")
        ..usize::try_from(range.end).expect("HIR range 适配宿主")
}
