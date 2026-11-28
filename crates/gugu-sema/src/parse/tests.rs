use super::{FragmentAst, FragmentKind, ParseOutput, parse, parse_fragment};
use crate::ast::{
    AssignOp, AstArena, AstRange, ExprId, ExprKind, FnBody, ItemKind, StmtId, StmtKind, StructBody,
};
use crate::diagnostics::DiagnosticCode;
use crate::source::{ExpansionId, SourceMap, SourceSnapshot};
use crate::token::TokenBuffer;

const SEMICOLONS: &str = include_str!("fixtures/semicolons.gg");

fn parse_source(source: &str) -> (ParseOutput, TokenBuffer) {
    let snapshot = SourceSnapshot::from_str("semicolons.gg", source).expect("合法源码快照");
    let sources = SourceMap::new(vec![snapshot.clone()]).expect("唯一源码路径");
    let file = sources.file_id("semicolons.gg").expect("已登记源码");
    let lexed = crate::lex(&snapshot, &sources, file);
    assert!(lexed.diagnostics.is_empty(), "{:?}", lexed.diagnostics);
    let mut buffer = lexed.buffer;
    let parsed = parse(source, &sources, file, &mut buffer);
    assert!(parsed.diagnostics.is_empty(), "{:?}", parsed.diagnostics);
    (parsed, buffer)
}

fn function_block(
    parsed: &ParseOutput,
    buffer: &TokenBuffer,
    name: &str,
) -> (AstRange<StmtId>, Option<ExprId>) {
    let function = parsed
        .arena
        .fns
        .iter()
        .find(|function| {
            function
                .name
                .is_some_and(|symbol| buffer.intern.get_str(symbol) == name)
        })
        .expect("fixture 中存在指定函数");
    let FnBody::Block(body) = function.body else {
        panic!("fixture 函数必须使用块体");
    };
    let ExprKind::Block { stmts, tail } = parsed.arena.exprs[body.0 as usize].kind else {
        panic!("函数体必须是块表达式");
    };
    (stmts, tail)
}

#[test]
fn semicolons_terminate_let_and_calls_on_separate_or_shared_lines() {
    let (parsed, buffer) = parse_source(SEMICOLONS);
    for name in ["main", "same_line"] {
        let (stmts, tail) = function_block(&parsed, &buffer, name);
        let ids = stmts.as_slice(&parsed.arena.stmt_ids);
        assert_eq!(ids.len(), 2, "{name} 应包含声明与调用");
        assert!(matches!(
            parsed.arena.stmts[ids[0].0 as usize].kind,
            StmtKind::Let { .. }
        ));
        assert!(matches!(
            parsed.arena.stmts[ids[1].0 as usize].kind,
            StmtKind::Expr {
                discarded: true,
                ..
            }
        ));
        assert!(tail.is_none(), "带分号的调用不得成为块值");
    }
}

#[test]
fn semicolons_terminate_special_statements_and_every_assignment_operator() {
    let (parsed, buffer) = parse_source(SEMICOLONS);
    let (stmts, tail) = function_block(&parsed, &buffer, "statements");
    let kinds: Vec<_> = stmts
        .as_slice(&parsed.arena.stmt_ids)
        .iter()
        .map(|id| &parsed.arena.stmts[id.0 as usize].kind)
        .collect();
    assert!(matches!(
        kinds[..7],
        [
            StmtKind::Let { init: None, .. },
            StmtKind::Let {
                else_block: Some(_),
                ..
            },
            StmtKind::Static { .. },
            StmtKind::Defer { ret: false, .. },
            StmtKind::Defer { ret: true, .. },
            StmtKind::Yield,
            StmtKind::SourceMacro { .. },
        ]
    ));
    let operators: Vec<_> = kinds[7..]
        .iter()
        .map(|kind| {
            let StmtKind::Assign { op, .. } = kind else {
                panic!("此段必须全是赋值语句")
            };
            *op
        })
        .collect();
    assert_eq!(
        operators,
        [
            AssignOp::Assign,
            AssignOp::Add,
            AssignOp::Sub,
            AssignOp::Mul,
            AssignOp::Div,
            AssignOp::Rem,
            AssignOp::BitAnd,
            AssignOp::BitOr,
            AssignOp::BitXor,
            AssignOp::Shl,
            AssignOp::Shr
        ]
    );
    assert!(tail.is_some(), "赋值后的裸表达式必须成为块值");
}

#[test]
fn semicolons_terminate_declarations_including_newtypes() {
    let source = include_str!("fixtures/semicolon_declarations.gg").trim_end();
    let (parsed, _) = parse_source(source);
    let newtypes = parsed
        .arena
        .items
        .iter()
        .filter(|item| {
            matches!(
                item.kind,
                ItemKind::Struct {
                    body: StructBody::Newtype(_),
                    ..
                }
            )
        })
        .count();
    assert_eq!(newtypes, 2, "同一行后接声明及文件末尾的 newtype 均应保留");
}

#[test]
fn semicolons_discard_expression_values_without_discarding_bare_tails() {
    let (parsed, buffer) = parse_source(SEMICOLONS);
    let (stmts, tail) = function_block(&parsed, &buffer, "value");
    assert_eq!(stmts.len, 2);
    assert!(matches!(
        parsed.arena.exprs[tail.expect("保留块值").0 as usize].kind,
        ExprKind::Binary { .. }
    ));
    let (stmts, tail) = function_block(&parsed, &buffer, "discarded");
    assert!(tail.is_none(), "分号必须丢弃末尾表达式的值");
    let ids = stmts.as_slice(&parsed.arena.stmt_ids);
    assert_eq!(ids.len(), 1);
    assert!(matches!(
        parsed.arena.stmts[ids[0].0 as usize].kind,
        StmtKind::Expr {
            discarded: true,
            ..
        }
    ));
}

#[test]
fn semicolons_preserve_statement_fragment_eof_and_tail_semantics() {
    let source = include_str!("fixtures/semicolon_fragment.gg").trim_end();
    for (source, discarded) in [(source.to_owned(), false), (format!("{source};"), true)] {
        let snapshot = SourceSnapshot::from_str("fragment.gg", &source).expect("合法源码快照");
        let sources = SourceMap::new(vec![snapshot]).expect("唯一源码路径");
        let file = sources.file_id("fragment.gg").expect("已登记源码");
        let (fragment, arena, diagnostics) = parse_fragment(
            &source,
            &sources,
            file,
            ExpansionId::ROOT,
            FragmentKind::Statements,
            AstArena::default(),
            &mut TokenBuffer::default(),
        );
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let FragmentAst::Statements(stmts, tail) = fragment else {
            panic!("必须得到语句片段");
        };
        assert_eq!(stmts.len(), if discarded { 3 } else { 2 });
        assert_eq!(tail.is_none(), discarded);
        assert!(matches!(
            arena.stmts[stmts[1].0 as usize].kind,
            StmtKind::Assign {
                op: AssignOp::Add,
                ..
            }
        ));
        if discarded {
            assert!(matches!(
                arena.stmts[stmts[2].0 as usize].kind,
                StmtKind::Expr {
                    discarded: true,
                    ..
                }
            ));
        }
    }
}

#[test]
fn semicolon_formatting_preserves_ast_and_is_idempotent() {
    let (before, before_tokens) = parse_source(SEMICOLONS);
    let formatted = crate::format::format_source("semicolons.gg", SEMICOLONS).expect("格式化成功");
    let (after, after_tokens) = parse_source(&formatted);
    assert_eq!(
        super::dump_ast(&before.file, &before.arena, &before_tokens.intern),
        super::dump_ast(&after.file, &after.arena, &after_tokens.intern),
        "格式化必须保留语句与块值结构",
    );
    assert_eq!(
        crate::format::format_source("semicolons.gg", &formatted).expect("再次格式化成功"),
        formatted
    );
}

#[test]
fn missing_struct_bodies_preserve_following_declarations() {
    for (source, missing) in [
        (include_str!("fixtures/missing_struct_body.gg"), 1),
        (include_str!("fixtures/missing_struct_bodies.gg"), 2),
    ] {
        let snapshot = SourceSnapshot::from_str("missing_struct.gg", source).expect("合法源码快照");
        let sources = SourceMap::new(vec![snapshot.clone()]).expect("唯一源码路径");
        let file = sources.file_id("missing_struct.gg").expect("已登记源码");
        let mut lexed = crate::lex(&snapshot, &sources, file);
        assert!(lexed.diagnostics.is_empty(), "{:?}", lexed.diagnostics);
        let parsed = parse(source, &sources, file, &mut lexed.buffer);
        assert_eq!(parsed.diagnostics.len(), missing);
        for (index, diagnostic) in parsed.diagnostics.iter().enumerate() {
            assert_eq!(diagnostic.code(), DiagnosticCode::ParseExpected);
            assert_eq!(diagnostic.message(), "结构体需要 `{`");
            assert_eq!(
                diagnostic.span().expect("诊断必须定位缺失体").line(),
                index as u32 + 2
            );
        }
        assert_eq!(parsed.file.items.len as usize, missing + 1);
        let (stmts, tail) = function_block(&parsed, &lexed.buffer, "main");
        assert_eq!(stmts.len, 0);
        assert!(tail.is_none());
    }
}

#[test]
fn malformed_struct_fields_stop_at_unconsumed_tokens() {
    let source = include_str!("fixtures/malformed_struct_fields.gg");
    let snapshot = SourceSnapshot::from_str("malformed_struct.gg", source).expect("合法源码快照");
    let sources = SourceMap::new(vec![snapshot.clone()]).expect("唯一源码路径");
    let file = sources.file_id("malformed_struct.gg").expect("已登记源码");
    let mut lexed = crate::lex(&snapshot, &sources, file);
    assert!(lexed.diagnostics.is_empty(), "{:?}", lexed.diagnostics);
    let parsed = parse(source, &sources, file, &mut lexed.buffer);
    assert!(!parsed.diagnostics.is_empty());
    assert_eq!(parsed.arena.fields.len(), 2, "错误字段只能登记一次");
    let (stmts, tail) = function_block(&parsed, &lexed.buffer, "main");
    assert_eq!(stmts.len, 0);
    assert!(tail.is_none());
}
