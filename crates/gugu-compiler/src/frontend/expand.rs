//! 源码宏展开驱动器：轮次闭包、`ExpandSourceMacro` query、AST 拼接与展开预算。
//!
//! 每一轮先冻结输入（名称解析 + 语义模型），在 SourceExpand 域求值本轮全部宏
//! 脚本；随后把通过解析闸门的生成文本注册为生成快照与展开记录，复用主
//! lexer/parser 解析进宿主 arena，执行片段 cfg 裁项后拼接回外层 AST。拼接完成
//! 后重新收集，直到没有未展开的 `comptime source` 节点。任何宏失败都在写出
//! 下游产物之前终止前端 action。

use std::collections::{BTreeMap, BTreeSet};

use crate::diagnostics::{Diagnostic, DiagnosticCode};
use crate::query::{QueryEngine, QueryKey, QueryKind};
use crate::source::{
    ExpansionId, ExpansionInput, SourceFileId, SourceMap, SourceSlot, SourceSnapshot, Span,
};

use super::ast::{
    ArenaLens, AstArena, AstFile, AstRange, AttrKind, Attribute, ExprId, ExprKind, ItemId,
    ItemKind, PatId, PatKind, StmtId, StmtKind, TyId, TyKind,
};
use super::cfg::{CfgContext, CfgRoots};
use super::parse::{FragmentAst, FragmentKind, parse_fragment};
use super::semantics::comptime::eval::{ConstantValue, ExpandHost};
use super::{ParsedModule, cfg, names, semantics};

/// `ExpandSourceMacro` query 的 schema 版本。
const EXPAND_SCHEMA_VERSION: u32 = 2;

/// 展开树深度默认上限与全局硬上限。
const DEFAULT_DEPTH_LIMIT: u32 = 16;
const HARD_DEPTH_LIMIT: u32 = 256;
/// 单 action 总展开次数默认上限。
const DEFAULT_EXPANSION_LIMIT: u64 = 4096;
/// 生成源码总字节默认上限。
const DEFAULT_BYTE_LIMIT: u64 = 4 * 1024 * 1024;
/// 生成 AST 总节点默认上限。
const DEFAULT_NODE_LIMIT: u64 = 1_000_000;
/// 宏脚本 comptime fuel 总池默认上限。
const DEFAULT_FUEL_LIMIT: u64 = 10_000_000;
/// 宏脚本 comptime heap 总池默认上限。
const DEFAULT_HEAP_LIMIT: u64 = 16 * 1024 * 1024;

/// 前端 action 消费的宏输入与预算摘要。
#[derive(Clone, Debug, Default)]
pub(crate) struct ExpansionInputs {
    /// 有效预算的规范编码。
    pub(crate) budget: Vec<u8>,
    /// 生成文本摘要，key 为生成快照逻辑路径。
    pub(crate) macros: BTreeMap<String, [u8; 32]>,
}

/// 六项独立预算的运行账本。
#[derive(Clone, Debug)]
struct ExpansionBudget {
    depth_limit: u32,
    expansion_limit: u64,
    byte_limit: u64,
    node_limit: u64,
    fuel_limit: u64,
    heap_limit: u64,
    expansions_used: u64,
    bytes_used: u64,
    nodes_used: u64,
    fuel_used: u64,
    heap_used: u64,
}

impl Default for ExpansionBudget {
    fn default() -> Self {
        Self {
            depth_limit: DEFAULT_DEPTH_LIMIT,
            expansion_limit: DEFAULT_EXPANSION_LIMIT,
            byte_limit: DEFAULT_BYTE_LIMIT,
            node_limit: DEFAULT_NODE_LIMIT,
            fuel_limit: DEFAULT_FUEL_LIMIT,
            heap_limit: DEFAULT_HEAP_LIMIT,
            expansions_used: 0,
            bytes_used: 0,
            nodes_used: 0,
            fuel_used: 0,
            heap_used: 0,
        }
    }
}

impl ExpansionBudget {
    fn limit_error(&self, chain: &str, reason: &str) -> Diagnostic {
        Diagnostic::error(
            DiagnosticCode::ExpansionLimit,
            format!("源码宏展开预算超限：{reason}{chain}"),
            None,
        )
    }
}

/// 本轮收集到的一个源码宏调用。
struct MacroCall {
    module: usize,
    slot: SourceSlot,
    script: ExprId,
    /// 宏调用点（宏节点 span）。
    call: Span,
    /// 宏定义点（脚本体 span）。
    definition: Span,
    position: MacroPosition,
}

#[derive(Clone, Copy)]
enum MacroPosition {
    Item(ItemId),
    Stmt(StmtId),
    Expr(ExprId),
    Ty(TyId),
    Pat(PatId),
}

/// 宏脚本求值成功后待拼接的片段。
struct Fragment {
    slot: SourceSlot,
    text: String,
}

/// 一个模块合并 token 流中各文件的区域表。
///
/// 合并后的 `TokenBuffer` 混有宿主与多个生成文件的 token；token 偏移只在所属
/// 文件内有效，按 token 下标分区解析文本来源。
struct TokenFileTable {
    /// 按 token 下标上界（排他）划分的区域；首个区域是宿主文件。
    boundaries: Vec<(usize, SourceFileId, Option<u32>)>,
}

impl TokenFileTable {
    fn new(module: &ParsedModule) -> Self {
        Self {
            boundaries: vec![(module.tokens.tokens.len(), module.file.source, None)],
        }
    }

    fn push_fragment(&mut self, token_end: usize, file: SourceFileId, depth_limit: u32) {
        self.boundaries.push((token_end, file, Some(depth_limit)));
    }

    fn file_of(&self, token_index: usize) -> SourceFileId {
        for (end, file, _) in &self.boundaries {
            if token_index < *end {
                return *file;
            }
        }
        self.boundaries
            .last()
            .map(|(_, file, _)| *file)
            .unwrap_or(SourceFileId::new(0))
    }
}

/// 展开主入口：驱动轮次闭包直至没有未展开宏。
pub(crate) fn run(
    modules: &mut [ParsedModule],
    sources: &mut SourceMap,
    cfg: &CfgContext,
    queries: &QueryEngine,
    package_identity: &str,
    external_packages: &BTreeSet<String>,
) -> Result<ExpansionInputs, Vec<Diagnostic>> {
    let mut budget = ExpansionBudget::default();
    let mut token_files: Vec<_> = modules.iter().map(TokenFileTable::new).collect();
    let mut inputs = ExpansionInputs::default();
    let mut round = 0_u32;
    let mut generated = 0_u32;
    // 模块级 expansion_limit 属性无论是否存在宏都必须通过校验。
    for (module, parsed) in modules.iter().enumerate() {
        expansion_limit_attribute(
            parsed,
            &token_files[module],
            sources,
            parsed.file.inner_attributes,
        )?;
    }
    loop {
        let calls = collect(modules);
        if calls.is_empty() {
            break;
        }
        round += 1;
        // 阶段 A：冻结输入，在同一语义模型上求值本轮全部宏脚本。
        let names = names::analyze(package_identity, external_packages, modules)?;
        let model = semantics::model::Model::new(modules, &names)?;
        let mut round_hash = blake3::Hasher::new_derive_key("gugu-expand-round-input-v1");
        for source in sources.snapshots() {
            round_hash.update(
                &u64::try_from(source.logical_path().len())
                    .expect("路径长度可编码")
                    .to_le_bytes(),
            );
            round_hash.update(source.logical_path().as_bytes());
            round_hash.update(&source.content_hash());
        }
        let round_input = *round_hash.finalize().as_bytes();
        let host = ExpandHost { queries };
        let mut fragments = Vec::with_capacity(calls.len());
        for call in &calls {
            charge_expansion(&mut budget, &token_files, sources, modules, call)?;
            let fragment = evaluate_macro(
                &model,
                sources,
                call,
                &host,
                round,
                &round_input,
                cfg,
                queries,
                &mut budget,
            )?;
            fragments.push(fragment);
        }
        // 阶段 B：注册展开并拼接生成片段。
        for (call, fragment) in calls.into_iter().zip(fragments) {
            splice_one(
                modules,
                sources,
                &mut token_files,
                &mut budget,
                &mut inputs,
                &mut generated,
                call,
                fragment,
                round,
                cfg,
            )?;
        }
    }
    inputs.budget = encode_budget(modules, &token_files, sources, &budget);
    Ok(inputs)
}

/// 收集本轮全部 active 源码宏，按调用位置稳定排序。
fn collect(modules: &[ParsedModule]) -> Vec<MacroCall> {
    let mut calls = Vec::new();
    for (module, parsed) in modules.iter().enumerate() {
        let arena = &parsed.arena;
        for (index, item) in arena.items.iter().enumerate() {
            let id = ItemId(index as u32);
            if let ItemKind::SourceMacro { body } = item.kind {
                if parsed.configured.item_active(id) && item_active_in_containers(parsed, id) {
                    calls.push(MacroCall {
                        module,
                        slot: SourceSlot::Item,
                        script: body,
                        call: item.span.clone(),
                        definition: arena.exprs[body.0 as usize].span.clone(),
                        position: MacroPosition::Item(id),
                    });
                }
            }
        }
        for (index, stmt) in arena.stmts.iter().enumerate() {
            let id = StmtId(index as u32);
            if let StmtKind::SourceMacro { body } = stmt.kind {
                if parsed.configured.stmt_active(id) {
                    calls.push(MacroCall {
                        module,
                        slot: SourceSlot::Statement,
                        script: body,
                        call: stmt.span.clone(),
                        definition: arena.exprs[body.0 as usize].span.clone(),
                        position: MacroPosition::Stmt(id),
                    });
                }
            }
        }
        for (index, expr) in arena.exprs.iter().enumerate() {
            if let ExprKind::SourceMacro { body } = expr.kind {
                if !parsed.configured.expr_active(body) {
                    continue;
                }
                calls.push(MacroCall {
                    module,
                    slot: SourceSlot::Expression,
                    script: body,
                    call: expr.span.clone(),
                    definition: arena.exprs[body.0 as usize].span.clone(),
                    position: MacroPosition::Expr(ExprId(index as u32)),
                });
            }
        }
        for (index, ty) in arena.tys.iter().enumerate() {
            if let TyKind::SourceMacro { body } = ty.kind {
                if !parsed.configured.expr_active(body) {
                    continue;
                }
                calls.push(MacroCall {
                    module,
                    slot: SourceSlot::Type,
                    script: body,
                    call: ty.span.clone(),
                    definition: arena.exprs[body.0 as usize].span.clone(),
                    position: MacroPosition::Ty(TyId(index as u32)),
                });
            }
        }
        for (index, pat) in arena.pats.iter().enumerate() {
            if let PatKind::SourceMacro { body } = pat.kind {
                if !parsed.configured.expr_active(body) {
                    continue;
                }
                calls.push(MacroCall {
                    module,
                    slot: SourceSlot::Pattern,
                    script: body,
                    call: pat.span.clone(),
                    definition: arena.exprs[body.0 as usize].span.clone(),
                    position: MacroPosition::Pat(PatId(index as u32)),
                });
            }
        }
    }
    calls.sort_by(|left, right| {
        (
            left.call.file(),
            left.call.start(),
            left.call.end(),
            left.slot,
        )
            .cmp(&(
                right.call.file(),
                right.call.start(),
                right.call.end(),
                right.slot,
            ))
    });
    calls
}

/// 判定 item slot 宏是否出现在某个 item 列表容器中（item 宏必须可被拼接定位）。
fn item_active_in_containers(parsed: &ParsedModule, target: ItemId) -> bool {
    let arena = &parsed.arena;
    if parsed
        .file
        .items
        .as_slice(&arena.item_ids)
        .contains(&target)
    {
        return true;
    }
    for item in &arena.items {
        let range = match item.kind {
            ItemKind::Trait { items, .. }
            | ItemKind::Impl { items, .. }
            | ItemKind::ExternBlock { items, .. } => items,
            _ => continue,
        };
        if range.as_slice(&arena.item_ids).contains(&target) {
            return true;
        }
    }
    false
}

/// 展开前的预算与 cycle 检查。
fn charge_expansion(
    budget: &mut ExpansionBudget,
    token_files: &[TokenFileTable],
    sources: &SourceMap,
    modules: &[ParsedModule],
    call: &MacroCall,
) -> Result<(), Vec<Diagnostic>> {
    if budget.expansions_used >= budget.expansion_limit {
        return Err(vec![
            budget.limit_error(&chain_text(sources, call), "展开次数达到上限"),
        ]);
    }
    budget.expansions_used += 1;
    let depth = expansion_depth(sources, call);
    let effective = effective_depth_limit(token_files, sources, modules, call)?;
    if depth > effective {
        return Err(vec![budget.limit_error(
            &chain_text(sources, call),
            &format!("展开树深度 {depth} 超过有效上限 {effective}"),
        )]);
    }
    // cycle 检测：祖先链上出现完全相同的 (脚本文本, source slot)。
    let script = script_text(sources, call);
    let mut parent = call.call.expansion();
    while parent != ExpansionId::ROOT {
        let record = sources
            .expansions()
            .get(parent.index() - 1)
            .expect("父展开已注册");
        let definition = record.macro_definition();
        let ancestor_text = sources
            .snapshot(definition.file())
            .and_then(|snapshot| {
                snapshot
                    .content()
                    .get(definition.start() as usize..definition.end() as usize)
            })
            .unwrap_or_default();
        if ancestor_text == script && record.fragment_kind() == call.slot {
            return Err(vec![Diagnostic::error(
                DiagnosticCode::ExpansionCycle,
                format!(
                    "源码宏展开形成循环：相同脚本在当前展开栈再次出现{}",
                    chain_text(sources, call),
                ),
                Some(call.call.clone()),
            )]);
        }
        parent = record.parent();
    }
    Ok(())
}

/// 宏调用点的展开树深度：父链长度加一。
fn expansion_depth(sources: &SourceMap, call: &MacroCall) -> u32 {
    let mut depth = 1;
    let mut parent = call.call.expansion();
    while parent != ExpansionId::ROOT {
        depth += 1;
        parent = sources
            .expansions()
            .get(parent.index() - 1)
            .expect("父展开已注册")
            .parent();
    }
    depth
}

/// 调用点属性 > 模块内属性 > 默认值；属性值不能超过全局硬上限。
fn effective_depth_limit(
    token_files: &[TokenFileTable],
    sources: &SourceMap,
    modules: &[ParsedModule],
    call: &MacroCall,
) -> Result<u32, Vec<Diagnostic>> {
    let module = &modules[call.module];
    let table = &token_files[call.module];
    let inherited = table
        .boundaries
        .iter()
        .find_map(|(_, file, limit)| (*file == call.call.file()).then_some(*limit).flatten());
    let position_attributes = match call.position {
        MacroPosition::Item(item) => module.arena.items[item.0 as usize].attributes,
        MacroPosition::Stmt(stmt) => module.arena.stmts[stmt.0 as usize].attributes,
        MacroPosition::Expr(expr) => module.arena.exprs[expr.0 as usize].attributes,
        _ => AstRange::empty(),
    };
    if let Some(limit) = expansion_limit_attribute(module, table, sources, position_attributes)? {
        return Ok(inherited.map_or(limit, |parent| parent.min(limit)));
    }
    if let Some(limit) = inherited {
        return Ok(limit);
    }
    Ok(
        expansion_limit_attribute(module, table, sources, module.file.inner_attributes)?
            .unwrap_or(DEFAULT_DEPTH_LIMIT),
    )
}

/// 读取一个属性集合中的 `comptime(expansion_limit = N)`；重复不同值报错。
fn expansion_limit_attribute(
    module: &ParsedModule,
    table: &TokenFileTable,
    sources: &SourceMap,
    attributes: AstRange<Attribute>,
) -> Result<Option<u32>, Vec<Diagnostic>> {
    let arena = &module.arena;
    let tokens = &module.tokens.tokens;
    let mut found: Option<u32> = None;
    for attribute in attributes.as_slice(&arena.attrs) {
        let (open, close) = match attribute.kind {
            AttrKind::Outer {
                token_open,
                token_close,
            }
            | AttrKind::Inner {
                token_open,
                token_close,
            } => (token_open as usize, token_close as usize),
            AttrKind::Doc { .. } | AttrKind::InnerDoc { .. } => continue,
        };
        let Some(body) = tokens.get(open + 1..close) else {
            continue;
        };
        let name = match body.first() {
            Some(_) => token_text(module, table, sources, open + 1),
            None => continue,
        };
        if name != "comptime" || body.len() < 6 || body[1].kind != super::token::TokenKind::LParen {
            continue;
        }
        let args = &body[2..body.len() - 1];
        if args.len() != 3
            || token_text(module, table, sources, open + 3) != "expansion_limit"
            || args[1].kind != super::token::TokenKind::Eq
            || args[2].kind != super::token::TokenKind::Int
        {
            return Err(vec![Diagnostic::error(
                DiagnosticCode::ExpansionLimit,
                "comptime 属性必须是 expansion_limit = N",
                Some(attribute.span.clone()),
            )]);
        }
        let value_text = token_text(module, table, sources, open + 5);
        let value = value_text.parse::<u32>().map_err(|_| {
            vec![Diagnostic::error(
                DiagnosticCode::ExpansionLimit,
                format!("expansion_limit 必须是正整数，收到 `{value_text}`"),
                Some(attribute.span.clone()),
            )]
        })?;
        if value == 0 || value > HARD_DEPTH_LIMIT {
            return Err(vec![Diagnostic::error(
                DiagnosticCode::ExpansionLimit,
                format!("expansion_limit 必须在 1..={HARD_DEPTH_LIMIT} 内，收到 {value}"),
                Some(attribute.span.clone()),
            )]);
        }
        match found {
            Some(previous) if previous != value => {
                return Err(vec![Diagnostic::error(
                    DiagnosticCode::ExpansionLimit,
                    format!("同一作用域重复指定不同的 expansion_limit：{previous} 与 {value}"),
                    Some(attribute.span.clone()),
                )]);
            }
            _ => found = Some(value),
        }
    }
    Ok(found)
}

/// 按所属文件读取 token 文本。
fn token_text<'a>(
    module: &'a ParsedModule,
    table: &'a TokenFileTable,
    sources: &'a SourceMap,
    token_index: usize,
) -> &'a str {
    let Some(token) = module.tokens.tokens.get(token_index) else {
        return "";
    };
    let file = table.file_of(token_index);
    let content = sources
        .snapshot(file)
        .map(|snapshot| snapshot.content())
        .unwrap_or_default();
    &content[token.start as usize..token.end as usize]
}

/// 展开链的确定性描述：从外层宏到当前宏的调用位置序列。
fn chain_text(sources: &SourceMap, call: &MacroCall) -> String {
    let mut chain = Vec::new();
    let mut current = call.call.clone();
    let mut parent = current.expansion();
    loop {
        chain.push(format!(
            "{}:{}:{}",
            current.path().display(),
            current.line(),
            current.column()
        ));
        if parent == ExpansionId::ROOT {
            break;
        }
        let record = sources
            .expansions()
            .get(parent.index() - 1)
            .expect("父展开已注册");
        current = record.macro_call().clone();
        parent = record.parent();
    }
    format!("，展开链：{}", chain.join(" -> "))
}

/// 宏脚本文本：按脚本体 span 从其所属文件切取。
fn script_text(sources: &SourceMap, call: &MacroCall) -> String {
    let span = &call.definition;
    sources
        .snapshot(span.file())
        .and_then(|snapshot| {
            snapshot
                .content()
                .get(span.start() as usize..span.end() as usize)
        })
        .unwrap_or_default()
        .to_owned()
}

/// `ExpandSourceMacro` query 包装的宏求值。
fn evaluate_macro(
    model: &semantics::model::Model<'_>,
    sources: &SourceMap,
    call: &MacroCall,
    host: &ExpandHost<'_>,
    round: u32,
    round_input: &[u8; 32],
    cfg: &CfgContext,
    queries: &QueryEngine,
    budget: &mut ExpansionBudget,
) -> Result<Fragment, Vec<Diagnostic>> {
    let text = script_text(sources, call);
    let configuration = format!("{cfg:?}");
    let mut hash = blake3::Hasher::new_derive_key("gugu-expand-macro-input-v1");
    hash.update(&[super::semantics::comptime::eval::slot_byte(call.slot)]);
    hash.update(&round.to_le_bytes());
    hash.update(round_input);
    hash.update(
        &u64::try_from(call.module)
            .expect("模块编号可编码")
            .to_le_bytes(),
    );
    hash.update(&call.call.start().to_le_bytes());
    hash.update(&call.call.end().to_le_bytes());
    hash.update(
        call.call
            .path()
            .to_str()
            .expect("源码逻辑路径为 UTF-8")
            .as_bytes(),
    );
    hash.update(&(text.len() as u64).to_le_bytes());
    hash.update(text.as_bytes());
    hash.update(&model.name_fingerprint());
    hash.update(configuration.as_bytes());
    hash.update(&semantics::comptime::registry::summary());
    let input_fingerprint = *hash.finalize().as_bytes();
    let key = QueryKey::new(
        QueryKind::ExpandSourceMacro,
        EXPAND_SCHEMA_VERSION,
        input_fingerprint,
    );
    let boundary_span = call.call.clone();
    let result = queries.compute(key, |context| {
        context.record_dependency(
            QueryKey::new(QueryKind::Configure, 1, b"cfg"),
            *blake3::hash(configuration.as_bytes()).as_bytes(),
        );
        context.record_dependency(
            QueryKey::new(QueryKind::ResolveImports, 1, b"names"),
            model.name_fingerprint(),
        );
        let evaluated = match model.eval_source_macro(call.module, call.script, call.slot, host) {
            Ok(evaluated) => evaluated,
            Err(error) => return Err(semantics::query::store_errors(&[error])),
        };
        match boundary_fragment(evaluated.value, boundary_span.clone()) {
            Ok(fragment) => Ok((
                serde_json::to_vec(&FragmentData {
                    slot: fragment.slot,
                    text: fragment.text,
                    fuel_used: evaluated.fuel_used,
                    heap_used: evaluated.heap_used,
                })
                .expect("宏展开结果 schema 序列化"),
                Vec::new(),
            )),
            Err(error) => Err(semantics::query::store_errors(&[error])),
        }
    });
    match result {
        Ok(result) => {
            let data: FragmentData = serde_json::from_slice(result.payload()).map_err(|_| {
                vec![Diagnostic::error(
                    DiagnosticCode::InvalidExpression,
                    "宏展开 query 缓存 schema 不合法",
                    None,
                )]
            })?;
            budget.fuel_used = budget.fuel_used.saturating_add(data.fuel_used);
            budget.heap_used = budget.heap_used.saturating_add(data.heap_used);
            if budget.fuel_used > budget.fuel_limit || budget.heap_used > budget.heap_limit {
                return Err(vec![budget.limit_error(
                    &chain_text(sources, call),
                    "宏脚本 fuel 或 heap 总量达到上限",
                )]);
            }
            Ok(Fragment {
                slot: data.slot,
                text: data.text,
            })
        }
        Err(error) => Err(semantics::query::restore_errors(error, sources)),
    }
}

/// 宏边界：终值必须是 `ParsedSource` 或 `Ok(ParsedSource)`。
fn boundary_fragment(value: ConstantValue, span: Span) -> Result<Fragment, Diagnostic> {
    match value {
        ConstantValue::ParsedSource(fragment) => Ok(Fragment {
            slot: fragment.slot,
            text: fragment.text,
        }),
        ConstantValue::ResultOk(inner) => match *inner {
            ConstantValue::ParsedSource(fragment) => Ok(Fragment {
                slot: fragment.slot,
                text: fragment.text,
            }),
            _ => Err(Diagnostic::error(
                DiagnosticCode::ExpansionFragmentMismatch,
                "宏脚本的 Ok 值不是 ParsedSource",
                Some(span),
            )),
        },
        ConstantValue::ResultErr(payload) => Err(Diagnostic::error(
            DiagnosticCode::MacroBoundaryError,
            format!("源码宏脚本返回 Err：{}", boundary_error_text(&payload)),
            Some(span),
        )),
        _ => Err(Diagnostic::error(
            DiagnosticCode::ExpansionFragmentMismatch,
            "宏脚本必须返回 ParsedSource 或 Result[ParsedSource, E]",
            Some(span),
        )),
    }
}

/// `Err` 负载的可读文本：字符串或携带 `message` 字段的结构值。
fn boundary_error_text(payload: &ConstantValue) -> String {
    match payload {
        ConstantValue::String(message) => message.clone(),
        ConstantValue::Struct(fields) => match fields.get("message") {
            Some(ConstantValue::String(message)) => message.clone(),
            _ => format!("{payload:?}"),
        },
        other => format!("{other:?}"),
    }
}

/// 宏展开 query 的确定性结果。
#[derive(serde::Serialize, serde::Deserialize)]
struct FragmentData {
    slot: SourceSlot,
    text: String,
    fuel_used: u64,
    heap_used: u64,
}

/// 拼接一个宏的生成片段。
#[allow(clippy::too_many_arguments)]
fn splice_one(
    modules: &mut [ParsedModule],
    sources: &mut SourceMap,
    token_files: &mut [TokenFileTable],
    budget: &mut ExpansionBudget,
    inputs: &mut ExpansionInputs,
    generated: &mut u32,
    call: MacroCall,
    fragment: Fragment,
    round: u32,
    cfg: &CfgContext,
) -> Result<(), Vec<Diagnostic>> {
    if fragment.slot != call.slot {
        return Err(vec![Diagnostic::error(
            DiagnosticCode::ExpansionFragmentMismatch,
            format!(
                "生成片段类别 {} 与插入位置 {} 不符",
                fragment.slot, call.slot
            ),
            Some(call.call.clone()),
        )]);
    }
    budget.bytes_used += fragment.text.len() as u64;
    if budget.bytes_used > budget.byte_limit {
        return Err(vec![budget.limit_error(
            &chain_text(sources, &call),
            "生成源码总字节达到上限",
        )]);
    }
    let inherited_limit = effective_depth_limit(token_files, sources, modules, &call)?;
    let host_path = sources
        .snapshot(call.call.file())
        .map(|snapshot| snapshot.logical_path().to_owned())
        .unwrap_or_else(|| "<unknown>".to_owned());
    let logical = format!("{host_path}::macro{generated}");
    *generated += 1;
    let snapshot = SourceSnapshot::from_str(std::path::Path::new(&logical), &fragment.text)
        .map_err(|error| {
            vec![Diagnostic::error(
                DiagnosticCode::InvalidSourcePath,
                format!("生成文本无法注册源码快照：{error}"),
                Some(call.call.clone()),
            )]
        })?;
    let file = sources.push_snapshot(snapshot).map_err(|error| {
        vec![Diagnostic::error(
            DiagnosticCode::InvalidSourcePath,
            format!("生成文本无法注册源码快照：{error}"),
            Some(call.call.clone()),
        )]
    })?;
    let expansion = sources
        .register_expansion(ExpansionInput {
            parent: call.call.expansion(),
            macro_call: call.call.clone(),
            macro_definition: call.definition.clone(),
            generated_source: file,
            fragment_kind: fragment.slot,
            round,
            fragment_order: 0,
        })
        .map_err(|error| {
            vec![Diagnostic::error(
                DiagnosticCode::InvalidType,
                format!("展开记录注册失败：{error}"),
                None,
            )]
        })?;
    let module = &mut modules[call.module];
    let pre: ArenaLens = module.arena.lens();
    let kind = fragment_kind_of(call.slot);
    let arena = std::mem::take(&mut module.arena);
    let (fragment_ast, arena, parse_errors) = parse_fragment(
        &fragment.text,
        sources,
        file,
        expansion,
        kind,
        arena,
        &mut module.tokens,
    );
    module.arena = arena;
    token_files[call.module].push_fragment(module.tokens.tokens.len(), file, inherited_limit);
    if !parse_errors.is_empty() {
        return Err(parse_errors);
    }
    if let FragmentAst::Items {
        inner_attributes, ..
    } = &fragment_ast
        && let Some(limit) = expansion_limit_attribute(
            module,
            &token_files[call.module],
            sources,
            *inner_attributes,
        )?
    {
        token_files[call.module]
            .boundaries
            .last_mut()
            .expect("已注册生成文件区域")
            .2 = Some(inherited_limit.min(limit));
    }
    let fragment_snapshot = sources.snapshot(file).expect("生成快照已注册");
    let roots = match &fragment_ast {
        FragmentAst::Items {
            items,
            inner_attributes,
        } => CfgRoots::Items {
            inner_attributes: *inner_attributes,
            items,
        },
        FragmentAst::Statements(stmts, tail) => CfgRoots::Statements { stmts, tail: *tail },
        FragmentAst::Expression(expr) => CfgRoots::Expression(*expr),
        FragmentAst::Type(ty) => CfgRoots::Type(*ty),
        FragmentAst::Pattern(pat) => CfgRoots::Pattern(*pat),
    };
    let fragment_configured = cfg::configure_fragment(
        fragment_snapshot,
        sources,
        file,
        &module.arena,
        &module.tokens,
        cfg,
        expansion,
        roots,
    )?;
    module.configured.merge_fragment(&fragment_configured, pre);
    budget.nodes_used += u64::from(module.arena.next_node - pre.next_node);
    if budget.nodes_used > budget.node_limit {
        return Err(vec![budget.limit_error(
            &chain_text(sources, &call),
            "生成 AST 总节点达到上限",
        )]);
    }
    let content_hash = *blake3::hash(fragment.text.as_bytes()).as_bytes();
    inputs.macros.insert(logical, content_hash);
    splice_ast(module, call.position, fragment_ast)?;
    // 孤儿宏节点（被替换的 item/stmt 条目）必须去激活，避免下一轮重收集。
    deactivate_orphan(module, call.position);
    Ok(())
}

/// 把已拼接宏位置的 cfg 位清零：该节点不再作为 SourceMacro 存在。
fn deactivate_orphan(module: &mut ParsedModule, position: MacroPosition) {
    match position {
        MacroPosition::Item(item) => {
            if let Some(slot) = module.configured.items_mut().get_mut(item.0 as usize) {
                *slot = false;
            }
        }
        MacroPosition::Stmt(stmt) => {
            if let Some(slot) = module.configured.stmts_mut().get_mut(stmt.0 as usize) {
                *slot = false;
            }
        }
        _ => {}
    }
}

/// 片段类别到解析类别的映射。
fn fragment_kind_of(slot: SourceSlot) -> FragmentKind {
    super::parse::fragment_kind_of(slot)
}

/// 在宿主 AST 中拼接片段：列表手术或根节点覆写。
///
/// item/stmt 拼接采用「原位替换 + 插入」：位置 `at` 的宏元素被片段首元素替换，
/// 其余片段元素紧随插入。列表中 `at` 之后的全部元素（含片段解析期间追加的
/// 嵌套范围）统一平移 `n-1`，所有范围修正遵循同一规则。
fn splice_ast(
    module: &mut ParsedModule,
    position: MacroPosition,
    fragment: FragmentAst,
) -> Result<(), Vec<Diagnostic>> {
    match (position, fragment) {
        (MacroPosition::Item(macro_item), FragmentAst::Items { items, .. }) => {
            let Some(at) = find_item_index(module, macro_item) else {
                return Err(vec![unlocatable("item", macro_item.0)]);
            };
            let n = items.len();
            if module.arena.item_ids.len() + n > u32::MAX as usize {
                return Err(vec![Diagnostic::error(
                    DiagnosticCode::ParseImplementationLimit,
                    "实现限制：AST 规模超过上限",
                    None,
                )]);
            }
            let mut items = items;
            match items.len() {
                0 => {
                    module.arena.item_ids.remove(at);
                }
                _ => {
                    let first = items.remove(0);
                    module.arena.item_ids[at] = first;
                    module.arena.item_ids.splice(at + 1..at + 1, items);
                }
            }
            fix_item_ranges(&mut module.arena, &mut module.file, at, n);
            Ok(())
        }
        (MacroPosition::Stmt(macro_stmt), FragmentAst::Statements(stmts, tail)) => {
            let Some((block, at, range_start, range_len)) = find_stmt_position(module, macro_stmt)
            else {
                return Err(vec![unlocatable("stmt", macro_stmt.0)]);
            };
            let n = stmts.len();
            let is_last = at + 1 == range_start + range_len;
            if let Some(tail) = tail {
                if is_last && block_tail_is_none(module, block) {
                    set_block_tail(module, block, tail);
                } else {
                    return Err(vec![Diagnostic::error(
                        DiagnosticCode::ExpansionFragmentMismatch,
                        "语句片段的尾表达式只能作为所在块的块尾",
                        Some(module.arena.stmts[macro_stmt.0 as usize].span.clone()),
                    )]);
                }
            }
            let mut stmts = stmts;
            match stmts.len() {
                0 => {
                    module.arena.stmt_ids.remove(at);
                }
                _ => {
                    let first = stmts.remove(0);
                    module.arena.stmt_ids[at] = first;
                    module.arena.stmt_ids.splice(at + 1..at + 1, stmts);
                }
            }
            fix_stmt_ranges(&mut module.arena, at, n);
            Ok(())
        }
        (MacroPosition::Expr(macro_expr), FragmentAst::Expression(root)) => {
            let node = module.arena.exprs[root.0 as usize].clone();
            let host = &mut module.arena.exprs[macro_expr.0 as usize];
            host.span = node.span;
            host.kind = node.kind;
            Ok(())
        }
        (MacroPosition::Ty(macro_ty), FragmentAst::Type(root)) => {
            let node = module.arena.tys[root.0 as usize].clone();
            let host = &mut module.arena.tys[macro_ty.0 as usize];
            host.span = node.span;
            host.kind = node.kind;
            Ok(())
        }
        (MacroPosition::Pat(macro_pat), FragmentAst::Pattern(root)) => {
            let node = module.arena.pats[root.0 as usize].clone();
            let host = &mut module.arena.pats[macro_pat.0 as usize];
            host.span = node.span;
            host.kind = node.kind;
            Ok(())
        }
        (position, fragment) => Err(vec![Diagnostic::error(
            DiagnosticCode::ExpansionFragmentMismatch,
            format!(
                "生成片段类别与插入位置不符：{:?} 位置收到 {:?} 片段",
                slot_of(position),
                kind_of(&fragment)
            ),
            None,
        )]),
    }
}

fn slot_of(position: MacroPosition) -> SourceSlot {
    match position {
        MacroPosition::Item(_) => SourceSlot::Item,
        MacroPosition::Stmt(_) => SourceSlot::Statement,
        MacroPosition::Expr(_) => SourceSlot::Expression,
        MacroPosition::Ty(_) => SourceSlot::Type,
        MacroPosition::Pat(_) => SourceSlot::Pattern,
    }
}

fn kind_of(fragment: &FragmentAst) -> &'static str {
    match fragment {
        FragmentAst::Items { .. } => "items",
        FragmentAst::Statements(..) => "statements",
        FragmentAst::Expression(_) => "expression",
        FragmentAst::Type(_) => "type",
        FragmentAst::Pattern(_) => "pattern",
    }
}

fn unlocatable(kind: &str, index: u32) -> Diagnostic {
    Diagnostic::error(
        DiagnosticCode::InvalidType,
        format!("源码宏 {kind} 节点 {index} 无法定位插入容器"),
        None,
    )
}

/// 宏 item 在 `item_ids` 列表中的绝对下标。
fn find_item_index(module: &ParsedModule, target: ItemId) -> Option<usize> {
    let arena = &module.arena;
    let file_items = module.file.items.as_slice(&arena.item_ids);
    if let Some(offset) = file_items.iter().position(|&item| item == target) {
        return Some(module.file.items.start as usize + offset);
    }
    for item in &arena.items {
        let range = match item.kind {
            ItemKind::Trait { items, .. }
            | ItemKind::Impl { items, .. }
            | ItemKind::ExternBlock { items, .. } => items,
            _ => continue,
        };
        if let Some(offset) = range
            .as_slice(&arena.item_ids)
            .iter()
            .position(|&item| item == target)
        {
            return Some(range.start as usize + offset);
        }
    }
    None
}

/// 宏语句所在的 Block 表达式与其在 `stmt_ids` 中的绝对下标和原区间。
fn find_stmt_position(
    module: &ParsedModule,
    target: StmtId,
) -> Option<(ExprId, usize, usize, usize)> {
    let arena = &module.arena;
    for (index, expr) in arena.exprs.iter().enumerate() {
        if let ExprKind::Block { stmts, .. } = expr.kind {
            if let Some(offset) = stmts
                .as_slice(&arena.stmt_ids)
                .iter()
                .position(|&stmt| stmt == target)
            {
                return Some((
                    ExprId(index as u32),
                    stmts.start as usize + offset,
                    stmts.start as usize,
                    stmts.len as usize,
                ));
            }
        }
    }
    None
}

fn block_tail_is_none(module: &ParsedModule, block: ExprId) -> bool {
    matches!(
        module.arena.exprs[block.0 as usize].kind,
        ExprKind::Block { tail: None, .. }
    )
}

fn set_block_tail(module: &mut ParsedModule, block: ExprId, tail: ExprId) {
    if let ExprKind::Block { tail: slot, .. } = &mut module.arena.exprs[block.0 as usize].kind {
        *slot = Some(tail);
    }
}

/// item 列表范围修正：包含插入点的区间长度加 `n-1`，之后的区间平移。
fn fix_item_ranges(arena: &mut AstArena, file: &mut AstFile, at: usize, n: usize) {
    let delta = n as i64 - 1;
    shift_range(&mut file.items.start, &mut file.items.len, at, delta);
    for item in &mut arena.items {
        match &mut item.kind {
            ItemKind::Trait { items, .. }
            | ItemKind::Impl { items, .. }
            | ItemKind::ExternBlock { items, .. } => {
                shift_range(&mut items.start, &mut items.len, at, delta);
            }
            _ => {}
        }
    }
}

/// stmt 列表范围修正：同 `fix_item_ranges`，作用在块语句区间上。
fn fix_stmt_ranges(arena: &mut AstArena, at: usize, n: usize) {
    let delta = n as i64 - 1;
    for expr in &mut arena.exprs {
        if let ExprKind::Block { stmts, .. } = &mut expr.kind {
            shift_range(&mut stmts.start, &mut stmts.len, at, delta);
        }
    }
}

fn shift_range(start: &mut u32, len: &mut u32, at: usize, delta: i64) {
    let s = *start as usize;
    let l = *len as usize;
    if s <= at && at < s + l {
        // 包含宏的容器：长度随替换增长（可能为负，如空片段删除宏）。
        *len = (l as i64 + delta) as u32;
    } else if s > at {
        *start = (s as i64 + delta) as u32;
    }
}

/// 预算的规范编码：默认上限与各模块生效的深度上限。
fn encode_budget(
    modules: &[ParsedModule],
    token_files: &[TokenFileTable],
    sources: &SourceMap,
    budget: &ExpansionBudget,
) -> Vec<u8> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&budget.depth_limit.to_le_bytes());
    bytes.extend_from_slice(&budget.expansion_limit.to_le_bytes());
    bytes.extend_from_slice(&budget.byte_limit.to_le_bytes());
    bytes.extend_from_slice(&budget.node_limit.to_le_bytes());
    bytes.extend_from_slice(&budget.fuel_limit.to_le_bytes());
    bytes.extend_from_slice(&budget.heap_limit.to_le_bytes());
    for (module, parsed) in modules.iter().enumerate() {
        let table = &token_files[module];
        let limit = expansion_limit_attribute(parsed, table, sources, parsed.file.inner_attributes)
            .ok()
            .flatten()
            .unwrap_or(budget.depth_limit);
        let path = parsed.path.as_bytes();
        bytes.extend_from_slice(&(path.len() as u32).to_le_bytes());
        bytes.extend_from_slice(path);
        bytes.extend_from_slice(&limit.to_le_bytes());
    }
    bytes
}

/// 生成源码诊断重锚定：主位置换成最外层宏调用点，附注携带完整展开链。
///
/// 规范要求生成源码的诊断以宏调用点为主位置，生成文本偏移与宏定义位置作为
/// 附注；该步在 query 之外执行，冷/热缓存命中路径产生一致结果。
pub(crate) fn reanchor_errors(errors: Vec<Diagnostic>, sources: &SourceMap) -> Vec<Diagnostic> {
    if sources.expansions().is_empty() {
        return errors;
    }
    let mut output = Vec::with_capacity(errors.len());
    for error in errors {
        let Some(span) = error.span().cloned() else {
            output.push(error);
            continue;
        };
        let expansion = span.expansion();
        if expansion == ExpansionId::ROOT {
            output.push(error);
            continue;
        }
        let mut chain = Vec::new();
        let mut current = expansion;
        while current != ExpansionId::ROOT {
            let Some(record) = sources.expansions().get(current.index() - 1) else {
                break;
            };
            chain.push((
                record.macro_call().clone(),
                record.macro_definition().clone(),
            ));
            current = record.parent();
        }
        let Some((outermost, _)) = chain.last() else {
            output.push(error);
            continue;
        };
        let severity = error.severity();
        let code = error.code();
        let seq = error.sequence();
        let message = error.message().to_owned();
        output.push(Diagnostic::new(
            severity,
            code,
            message,
            Some(outermost.clone()),
            seq,
        ));
        for (call, definition) in chain.iter().rev() {
            output.push(Diagnostic::note(code, "宏调用位置", Some(call.clone())));
            output.push(Diagnostic::note(
                code,
                "宏定义位置",
                Some(definition.clone()),
            ));
        }
        output.push(Diagnostic::note(code, "生成文本位置", Some(span)));
    }
    output
}

#[cfg(test)]
mod tests;
