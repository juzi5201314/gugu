//! 测试/bench harness：扫描 `#[test]`/`#[bench]` 项、校验签名与身份、收集 doctest、
//! 生成 `__gugu_harness` 入口模块并把入口改写到生成的 `fn main`。

use std::collections::BTreeMap;

use gugu_base::{
    diagnostics::{Diagnostic, DiagnosticCode},
    source::{
        ExpansionId, ExpansionInput, SourceFileId, SourceMap, SourceSlot, SourceSnapshot, Span,
    },
};
use gugu_sema::{
    ParsedModule,
    ast::{AstRange, AttrKind, Attribute, FnDecl, ItemKind, TyId, TyKind, Visibility},
    cfg::CfgContext,
    modules::{parse_module, parse_module_in_expansion},
};

/// 生成的 runner 模块逻辑文件名（模块路径即去掉扩展名）。
const MANIFEST_PATH: &str = "__gugu_harness.gg";

const FLAG_IGNORE: u64 = 1;
const FLAG_SHOULD_PANIC: u64 = 2;
const FLAG_EXPECT: u64 = 4;

struct Case {
    module: String,
    name: String,
    identity: String,
    flags: u64,
    expect: String,
    returns_result: bool,
}

struct Doctest {
    module: usize,
    /// 1-based 围栏起始行。
    line: u32,
    index: u32,
    code: String,
    /// 围栏起始 ` ``` ` 文档行 span；诊断重锚定的调用点。
    fence: Span,
    /// `fn main` 独立程序：Some(主体源码) 时生成 `fn __gugu_dt_prog`。
    program: bool,
}

/// 收集标记项与 doctest，生成 manifest 与 doctest 模块；返回入口应指向的文件 ID。
///
/// 只在 `cfg.harness()` 域调用。域外 `#[test]`/`#[bench]` 项已被 cfg 裁除，不会到达这里。
pub fn apply(
    modules: &mut Vec<ParsedModule>,
    source_map: &mut SourceMap,
    cfg: &CfgContext,
) -> Result<Option<SourceFileId>, Vec<Diagnostic>> {
    if !cfg.harness() {
        return Ok(None);
    }
    let mut cases = Vec::new();
    let mut benches = Vec::new();
    let mut doctests = Vec::new();
    let mut diagnostics = Vec::new();
    for (index, module) in modules.iter_mut().enumerate() {
        if module.builtin
            || module
                .package
                .as_ref()
                .is_some_and(|package| !package.namespace.is_empty())
        {
            continue;
        }
        let Some(source) = source_map
            .snapshot(module.file.source)
            .map(|snapshot| snapshot.content().to_owned())
        else {
            continue;
        };
        collect_module(
            module,
            index,
            &source,
            &mut cases,
            &mut benches,
            &mut doctests,
            &mut diagnostics,
        );
    }
    let mut doctest_sources = Vec::new();
    // doctest 只属于测试域：bench 构建中 `#[test]` 项被 harness_domain 裁除，
    // 生成的 doctest 模块若继续生成，manifest 会引用不存在的函数。
    if cfg.test() {
        for (index, doctest) in doctests.iter().enumerate() {
            doctest_sources.push((
                doctest_source(&modules[doctest.module], doctest),
                format!("__gugu_doctest_{index}"),
            ));
            cases.push(Case {
                module: format!("__gugu_doctest_{index}"),
                name: "__gugu_dt_case".to_owned(),
                identity: format!(
                    "doctest:{}:{}:{}",
                    modules[doctest.module].path, doctest.line, doctest.index
                ),
                flags: 0,
                expect: String::new(),
                returns_result: false,
            });
        }
    }
    if !diagnostics.is_empty() {
        return Err(diagnostics);
    }
    for ((source, path), doctest) in doctest_sources.into_iter().zip(&doctests) {
        let module = push_doctest(source_map, &path, &source, cfg, doctest)?;
        modules.push(module);
    }
    // 收集顺序确定：按身份排序后生成表。
    cases.sort_by(|left, right| left.identity.cmp(&right.identity));
    benches.sort_by(|left, right| left.identity.cmp(&right.identity));
    let manifest = manifest_source(&cases, &benches, cfg.test());
    let module = push_generated(source_map, MANIFEST_PATH, &manifest, cfg)?;
    let entry = module.file.source;
    modules.push(module);
    Ok(Some(entry))
}

/// doctest 模块按展开注册：span 携带展开记录，诊断经 `reanchor_errors`
/// 回锚到文档围栏所在行，附注给出展开链。
fn push_doctest(
    source_map: &mut SourceMap,
    path: &str,
    source: &str,
    cfg: &CfgContext,
    doctest: &Doctest,
) -> Result<ParsedModule, Vec<Diagnostic>> {
    let snapshot = SourceSnapshot::from_str(path, source).map_err(|error| {
        vec![Diagnostic::error(
            DiagnosticCode::InvalidDeclaration,
            format!("harness 生成模块 `{path}` 无效：{error}"),
            None,
        )]
    })?;
    let file = source_map.push_snapshot(snapshot).map_err(|error| {
        vec![Diagnostic::error(
            DiagnosticCode::InvalidDeclaration,
            format!("harness 生成模块 `{path}` 登记失败：{error}"),
            None,
        )]
    })?;
    let expansion = source_map
        .register_expansion(ExpansionInput {
            parent: ExpansionId::ROOT,
            macro_call: doctest.fence.clone(),
            macro_definition: doctest.fence.clone(),
            generated_source: file,
            fragment_kind: SourceSlot::Item,
            round: 0,
            fragment_order: doctest.index,
        })
        .map_err(|error| {
            vec![Diagnostic::error(
                DiagnosticCode::InvalidDeclaration,
                format!("harness 生成模块 `{path}` 展开登记失败：{error}"),
                Some(doctest.fence.clone()),
            )]
        })?;
    parse_module_in_expansion(
        source_map.snapshot(file).expect("生成模块快照已登记"),
        source_map,
        file,
        expansion,
        path.strip_suffix(".gg").unwrap_or(path).to_owned(),
        cfg,
        false,
    )
}

fn push_generated(
    source_map: &mut SourceMap,
    path: &str,
    source: &str,
    cfg: &CfgContext,
) -> Result<ParsedModule, Vec<Diagnostic>> {
    let snapshot = SourceSnapshot::from_str(path, source).map_err(|error| {
        vec![Diagnostic::error(
            DiagnosticCode::InvalidDeclaration,
            format!("harness 生成模块 `{path}` 无效：{error}"),
            None,
        )]
    })?;
    let file = source_map.push_snapshot(snapshot).map_err(|error| {
        vec![Diagnostic::error(
            DiagnosticCode::InvalidDeclaration,
            format!("harness 生成模块 `{path}` 登记失败：{error}"),
            None,
        )]
    })?;
    parse_module(
        source_map.snapshot(file).expect("生成模块快照已登记"),
        source_map,
        file,
        path.strip_suffix(".gg").unwrap_or(path).to_owned(),
        cfg,
        false,
    )
}

/// 扫描一个模块：标记项收集+校验+提权，doc 围栏收集。
fn collect_module(
    module: &mut ParsedModule,
    module_index: usize,
    source: &str,
    cases: &mut Vec<Case>,
    benches: &mut Vec<Case>,
    doctests: &mut Vec<Doctest>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let items = module.file.items.as_slice(&module.arena.item_ids).to_vec();
    let mut identities = BTreeMap::new();
    let mut doc_index = 0_u32;
    for item_id in items {
        if !module.configured.item_active(item_id) {
            continue;
        }
        let item = &module.arena.items[item_id.0 as usize];
        collect_doctests(
            module,
            module_index,
            source,
            item.attributes,
            &mut doc_index,
            doctests,
        );
        let marks = marks(module, source, item.attributes);
        if marks.test.is_none() && marks.bench.is_none() {
            continue;
        }
        let item = &mut module.arena.items[item_id.0 as usize];
        item.visibility = Visibility::Pub;
        let ItemKind::Function(function) = item.kind else {
            diagnostics.push(Diagnostic::error(
                DiagnosticCode::InvalidDeclaration,
                "#[test]/#[bench] 只能标在函数项上",
                Some(item.span.clone()),
            ));
            continue;
        };
        let declaration = &module.arena.fns[function.0 as usize];
        let name = declaration
            .name
            .map(|symbol| module.tokens.intern.get_str(symbol).to_owned())
            .unwrap_or_default();
        let identity = format!("{}.{}", module.path, name);
        if identities.insert(identity.clone(), ()).is_some() {
            diagnostics.push(Diagnostic::error(
                DiagnosticCode::InvalidDeclaration,
                format!("重复的测试/benchmark 身份 `{identity}`"),
                Some(item.span.clone()),
            ));
            continue;
        }
        if marks.test.is_some() && marks.bench.is_some() {
            diagnostics.push(invalid("#[test] 与 #[bench] 不能同标一项", &item.span));
            continue;
        }
        if marks.test.is_some()
            && let Some((flags, expect, returns_result)) =
                validate_test(module, declaration, &marks, source, &name, diagnostics)
        {
            cases.push(Case {
                module: module.path.clone(),
                name: name.clone(),
                identity: identity.clone(),
                flags,
                expect,
                returns_result,
            });
        }
        if marks.bench.is_some()
            && validate_bench(module, declaration, &marks, source, diagnostics).is_some()
        {
            benches.push(Case {
                module: module.path.clone(),
                name,
                identity,
                flags: 0,
                expect: String::new(),
                returns_result: false,
            });
        }
    }
}

#[derive(Default)]
struct Marks {
    test: Option<usize>,
    bench: Option<usize>,
    ignore: bool,
    should_panic: Option<Option<String>>,
    naked: bool,
}

/// 读 item 上的 harness 相关属性名；`should_panic` 带 eq 载荷。
fn marks(module: &ParsedModule, source: &str, attributes: AstRange<Attribute>) -> Marks {
    let mut marks = Marks::default();
    for attribute in attributes.as_slice(&module.arena.attrs) {
        let (open, close) = match attribute.kind {
            AttrKind::Outer {
                token_open,
                token_close,
            }
            | AttrKind::Inner {
                token_open,
                token_close,
            } => (token_open as usize + 1, token_close as usize),
            AttrKind::Doc { .. } | AttrKind::InnerDoc { .. } => continue,
        };
        let body = &module.tokens.tokens[open..close];
        let Some(first) = body.first() else { continue };
        let name = first.text(source);
        match name {
            "test" => marks.test = Some(open),
            "bench" => marks.bench = Some(open),
            "ignore" => marks.ignore = true,
            "naked" => marks.naked = true,
            "should_panic" => {
                let eq = body.iter().find_map(|token| {
                    let text = token.text(source);
                    text.strip_prefix('"')
                        .and_then(|rest| rest.strip_suffix('"'))
                        .map(str::to_owned)
                });
                marks.should_panic = Some(eq);
            }
            _ => {}
        }
    }
    marks
}

/// `#[test]` 签名校验：无参、无泛型、非 unsafe/naked/extern，返回 `()` 或 `Result[_, E]`。
/// 返回（flags, expect, returns_result）。
fn validate_test(
    module: &ParsedModule,
    declaration: &FnDecl,
    marks: &Marks,
    source: &str,
    name: &str,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<(u64, String, bool)> {
    if name.is_empty() {
        diagnostics.push(invalid("测试函数必须有名字", &declaration.span));
        return None;
    }
    if declaration.generics.len != 0
        || declaration.unsafety
        || declaration.extern_abi.is_some()
        || declaration.extern_import
        || marks.naked
    {
        diagnostics.push(invalid(
            "#[test] 函数不能是泛型、unsafe、extern 或 naked",
            &declaration.span,
        ));
        return None;
    }
    if active_params(module, declaration) != 0 {
        diagnostics.push(invalid("#[test] 函数必须无参数", &declaration.span));
        return None;
    }
    let returns_result = match declaration.return_ty {
        None => false,
        Some(ty) => {
            let ty = &module.arena.tys[ty.0 as usize];
            match &ty.kind {
                TyKind::Tuple(range) if range.len == 0 => false,
                TyKind::Path(path) => {
                    let last = module.arena.paths[path.0 as usize]
                        .segments
                        .as_slice(&module.arena.segments)
                        .last()
                        .map(|segment| span_text(source, &segment.span));
                    if last == Some("Result") {
                        true
                    } else {
                        diagnostics.push(invalid("#[test] 只能返回 () 或 Result[(), E]", &ty.span));
                        return None;
                    }
                }
                _ => {
                    diagnostics.push(invalid("#[test] 只能返回 () 或 Result[(), E]", &ty.span));
                    return None;
                }
            }
        }
    };
    let mut flags = 0_u64;
    if marks.ignore {
        flags |= FLAG_IGNORE;
    }
    let mut expect = String::new();
    if let Some(eq) = &marks.should_panic {
        flags |= FLAG_SHOULD_PANIC;
        if let Some(eq) = eq {
            flags |= FLAG_EXPECT;
            expect = eq.clone();
        }
    }
    Some((flags, expect, returns_result))
}

/// `#[bench]` 签名校验：恰好 `fn name(b: &std.test.Bencher)`，无 test/should_panic/ignore。
fn validate_bench(
    module: &ParsedModule,
    declaration: &FnDecl,
    marks: &Marks,
    source: &str,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<()> {
    if declaration.generics.len != 0
        || declaration.unsafety
        || declaration.extern_abi.is_some()
        || declaration.extern_import
        || marks.naked
    {
        diagnostics.push(invalid(
            "#[bench] 函数不能是泛型、unsafe、extern 或 naked",
            &declaration.span,
        ));
        return None;
    }
    if marks.test.is_some() || marks.ignore || marks.should_panic.is_some() {
        diagnostics.push(invalid(
            "#[bench] 不能与 test/should_panic/ignore 同标",
            &declaration.span,
        ));
        return None;
    }
    if matches!(declaration.return_ty, Some(ty) if !unit_ty(module, source, ty)) {
        diagnostics.push(invalid("#[bench] 函数不能带返回值", &declaration.span));
        return None;
    }
    let params = active_params(module, declaration);
    if params != 1 {
        diagnostics.push(invalid(
            "#[bench] 签名必须是 fn name(b: &std.test.Bencher)",
            &declaration.span,
        ));
        return None;
    }
    let bencher = declaration
        .params
        .as_slice(&module.arena.params)
        .iter()
        .filter(|param| param.ty.is_some())
        .any(
            |param| match module.arena.tys[param.ty.unwrap().0 as usize].kind {
                TyKind::Ref(inner) => match &module.arena.tys[inner.0 as usize].kind {
                    TyKind::Path(path) => module.arena.paths[path.0 as usize]
                        .segments
                        .as_slice(&module.arena.segments)
                        .last()
                        .is_some_and(|segment| span_text(source, &segment.span) == "Bencher"),
                    _ => false,
                },
                _ => false,
            },
        );
    if !bencher {
        diagnostics.push(invalid(
            "#[bench] 签名必须是 fn name(b: &std.test.Bencher)",
            &declaration.span,
        ));
        return None;
    }
    Some(())
}

fn unit_ty(module: &ParsedModule, _source: &str, ty: TyId) -> bool {
    matches!(
        module.arena.tys[ty.0 as usize].kind,
        TyKind::Tuple(range) if range.len == 0
    )
}

fn active_params(module: &ParsedModule, declaration: &FnDecl) -> usize {
    (0..declaration.params.len as usize)
        .filter(|index| {
            module
                .configured
                .param_active(declaration.params.start as usize + index)
        })
        .count()
}

fn invalid(message: &str, span: &Span) -> Diagnostic {
    Diagnostic::error(
        DiagnosticCode::InvalidDeclaration,
        message.to_owned(),
        Some(span.clone()),
    )
}

fn span_text<'a>(source: &'a str, span: &Span) -> &'a str {
    source
        .get(span.start() as usize..span.end() as usize)
        .unwrap_or_default()
}

/// 抽取一个 item 上的 `///` doc 注释，收集 ` ``` ` 围栏里的可编译块。
fn collect_doctests(
    module: &ParsedModule,
    module_index: usize,
    source: &str,
    attributes: AstRange<Attribute>,
    doc_index: &mut u32,
    doctests: &mut Vec<Doctest>,
) {
    let mut in_fence = false;
    let mut fence_line = 0_u32;
    let mut fence_span: Option<Span> = None;
    let mut code = String::new();
    for attribute in attributes.as_slice(&module.arena.attrs) {
        let (AttrKind::Doc { .. } | AttrKind::InnerDoc { .. }) = attribute.kind else {
            continue;
        };
        let raw = span_text(source, &attribute.span);
        let line_text = doc_line(raw);
        if line_text.trim_start().starts_with("```") {
            if in_fence {
                finish_fence(
                    module_index,
                    fence_line,
                    fence_span.take(),
                    doc_index,
                    &code,
                    doctests,
                );
                code.clear();
                in_fence = false;
                continue;
            }
            let info = line_text.trim_start()[3..].trim();
            in_fence = info.is_empty() || info == "gugu";
            fence_line = attribute.span.line();
            fence_span = Some(attribute.span.clone());
            continue;
        }
        if in_fence {
            code.push_str(line_text);
            code.push('\n');
        }
    }
    if in_fence {
        finish_fence(
            module_index,
            fence_line,
            fence_span.take(),
            doc_index,
            &code,
            doctests,
        );
    }
}

fn doc_line(raw: &str) -> &str {
    let trimmed = raw.trim_start();
    trimmed
        .strip_prefix("///")
        .or_else(|| trimmed.strip_prefix("//!"))
        .map(|rest| rest.strip_prefix(' ').unwrap_or(rest))
        .unwrap_or(trimmed)
}

fn finish_fence(
    module_index: usize,
    line: u32,
    fence: Option<Span>,
    doc_index: &mut u32,
    code: &str,
    doctests: &mut Vec<Doctest>,
) {
    let index = *doc_index;
    *doc_index += 1;
    let Some(fence) = fence else { return };
    doctests.push(Doctest {
        module: module_index,
        line,
        index,
        code: code.to_owned(),
        fence,
        program: code.contains("fn main"),
    });
}

/// doctest 模块源码：pub 项入作用域，`fn main` 围栏改独立程序名 + `#[test]` 适配器。
fn doctest_source(module: &ParsedModule, doctest: &Doctest) -> String {
    let mut out = String::from("//! doctest 生成模块\n#![allow(missing_docs)]\n");
    let pubs: Vec<String> = module
        .file
        .items
        .as_slice(&module.arena.item_ids)
        .iter()
        .filter(|item| module.configured.item_active(**item))
        .filter_map(|item| {
            let item = &module.arena.items[item.0 as usize];
            (item.visibility == Visibility::Pub)
                .then_some(())
                .and(item.name)
                .map(|name| module.tokens.intern.get_str(name).to_owned())
        })
        .collect();
    if !pubs.is_empty() {
        out.push_str(&format!("use {}.{{{}}}\n", module.path, pubs.join(", ")));
    }
    if doctest.program {
        let renamed = doctest.code.replacen("fn main", "fn __gugu_dt_prog", 1);
        let result_main = renamed
            .find("fn __gugu_dt_prog")
            .map(|at| {
                let tail = &renamed[at..];
                let end = tail
                    .find('{')
                    .or_else(|| tail.find('='))
                    .unwrap_or(tail.len());
                tail[..end].contains("Result")
            })
            .unwrap_or(false);
        out.push_str(&renamed);
        if !renamed.ends_with('\n') {
            out.push('\n');
        }
        if result_main {
            out.push_str(
                "#[test]\nfn __gugu_dt_case() {\n    match __gugu_dt_prog() {\n        Ok(()) => (),\n        Err(_) => panic(\"doctest main 返回 Err\"),\n    }\n}\n",
            );
        } else {
            out.push_str("#[test]\npub fn __gugu_dt_case() = __gugu_dt_prog()\n");
        }
    } else {
        out.push_str("#[test]\npub fn __gugu_dt_case() {\n");
        out.push_str(&doctest.code);
        out.push_str("}\n");
    }
    out
}

fn gugu_string(text: &str) -> String {
    let mut out = String::from("\"");
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            _ => out.push(ch),
        }
    }
    out.push('"');
    out
}

/// `__gugu_harness` 模块：use 别名 + per-case 包装 + 调度表 + `fn main`。
fn manifest_source(cases: &[Case], benches: &[Case], test: bool) -> String {
    let mut out = String::from(
        "//! 测试/bench harness 入口（生成）。\n#![allow(missing_docs)]\nuse std.test.{__run_tests, __run_bench, __synthetic_panic, __bench_run, __report_test_err, BenchSample}\n",
    );
    for (index, case) in cases.iter().enumerate() {
        out.push_str(&format!(
            "use {}.{{{} as __gugu_t_{index}}}\n",
            case.module, case.name
        ));
    }
    for (index, case) in benches.iter().enumerate() {
        out.push_str(&format!(
            "use {}.{{{} as __gugu_b_{index}}}\n",
            case.module, case.name
        ));
    }
    for (index, case) in cases.iter().enumerate() {
        if case.returns_result {
            out.push_str(&format!(
                "fn __gugu_wrap_{index}() Result[(), Panic] {{\n    match __gugu_t_{index}() {{\n        Ok(()) => Ok(()),\n        Err(e) => {{\n            __report_test_err(f\"{{e}}\")\n            Err(__synthetic_panic())\n        }},\n    }}\n}}\n"
            ));
        } else {
            out.push_str(&format!(
                "fn __gugu_wrap_{index}() Result[(), Panic] {{\n    __gugu_t_{index}()\n    Ok(())\n}}\n"
            ));
        }
    }
    for index in 0..benches.len() {
        out.push_str(&format!(
            "fn __gugu_bwrap_{index}() &BenchSample {{\n    let sample = __bench_run(__gugu_b_{index})\n    &sample\n}}\n"
        ));
    }
    out.push_str(&format!(
        "pub fn __gugu_test_count() uint = {}\n",
        cases.len()
    ));
    out.push_str(
        "pub fn __gugu_test_spawn(index: uint) Join[Result[(), Panic]] {\n    match index {\n",
    );
    for index in 0..cases.len() {
        out.push_str(&format!(
            "        {index} => async {{ __gugu_wrap_{index}() }},\n"
        ));
    }
    out.push_str("        _ => async { Err(__synthetic_panic()) },\n    }\n}\n");
    emit_table(
        &mut out,
        "__gugu_test_name",
        cases.iter().map(|case| case.identity.clone()),
    );
    emit_table(
        &mut out,
        "__gugu_test_flags",
        cases.iter().map(|case| format!("uint({})", case.flags)),
    );
    emit_table(
        &mut out,
        "__gugu_test_expect",
        cases.iter().map(|case| gugu_string(&case.expect)),
    );
    out.push_str(&format!(
        "pub fn __gugu_bench_count() uint = {}\n",
        benches.len()
    ));
    out.push_str(
        "pub fn __gugu_bench_spawn(index: uint) Join[&BenchSample] {\n    match index {\n",
    );
    for index in 0..benches.len() {
        out.push_str(&format!(
            "        {index} => async {{ __gugu_bwrap_{index}() }},\n"
        ));
    }
    out.push_str("        _ => panic(\"不存在的 benchmark 索引\"),\n    }\n}\n");
    emit_table(
        &mut out,
        "__gugu_bench_name",
        benches.iter().map(|case| case.identity.clone()),
    );
    out.push_str("fn main() Result[(), uint] {\n    let failed = ");
    out.push_str(if test {
        "__run_tests(__gugu_test_count(), __gugu_test_spawn, __gugu_test_name, __gugu_test_flags, __gugu_test_expect)"
    } else {
        "__run_bench(__gugu_bench_count(), __gugu_bench_spawn, __gugu_bench_name)"
    });
    out.push_str("\n    if failed == 0 { Ok(()) } else { Err(failed) }\n}\n");
    out
}

/// `pub fn name(index: uint) string/uint { match ... }` 查表。
fn emit_table(out: &mut String, name: &str, values: impl Iterator<Item = String>) {
    let values: Vec<String> = values.collect();
    let ty = if name.contains("flags") {
        "uint"
    } else {
        "string"
    };
    let fallback = if ty == "uint" { "uint(0)" } else { "\"\"" };
    out.push_str(&format!(
        "pub fn {name}(index: uint) {ty} {{\n    match index {{\n"
    ));
    for (index, value) in values.iter().enumerate() {
        let value = if ty == "string" && !value.starts_with('"') {
            gugu_string(value)
        } else {
            value.clone()
        };
        out.push_str(&format!("        {index} => {value},\n"));
    }
    out.push_str(&format!("        _ => {fallback},\n    }}\n}}\n"));
}
