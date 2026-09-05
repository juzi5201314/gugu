use crate::diagnostics::{Diagnostic, DiagnosticCode};
use crate::source::{ExpansionId, SourceMap};

use super::ast::{AstArena, AstFile, AstRange, AttrKind, Attribute, ExprId, ItemId, StmtId};
use super::cfg::ConfiguredAst;
use super::token::{Token, TokenBuffer, TokenKind};
use crate::Span;
// 语言当前有 11 个 lint，编译期固定表按位编码，不分配名称集合。
const NAMES: [&str; 11] = [
    "large_copy",
    "unused_must_use",
    "unused",
    "dead_code",
    "non_snake_case",
    "non_upper_camel_case",
    "non_screaming_case",
    "bad_initialism",
    "missing_docs",
    "long_line",
    "use_order",
];

pub(super) fn validate_attributes(
    source: &str,
    source_map: &SourceMap,
    buffer: &mut TokenBuffer,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let file = buffer.file.expect("属性校验需要源文件");
    let tokens = &mut buffer.tokens;
    let mut index = 0;
    while index < tokens.len() {
        if tokens[index].kind != TokenKind::Hash {
            index += 1;
            continue;
        }
        let Some(open) = attribute_open(tokens, index) else {
            index += 1;
            continue;
        };
        let Some(close) = matching_rbracket(tokens, open) else {
            emit(
                diagnostics,
                source_map,
                file,
                tokens[index].start as usize,
                tokens
                    .last()
                    .map(|token| token.end as usize)
                    .unwrap_or(source.len()),
                DiagnosticCode::LexUnterminated,
                "属性括号未闭合",
            );
            tokens[index].kind = TokenKind::Error;
            break;
        };
        let body = &tokens[open + 1..close];
        if body.is_empty() {
            emit(
                diagnostics,
                source_map,
                file,
                tokens[open].start as usize,
                tokens[close].end as usize,
                DiagnosticCode::LexUnknownAttribute,
                "属性不能为空",
            );
            tokens[open].kind = TokenKind::Error;
        } else if let Err(error) = validate_attr_body(source, body) {
            emit(
                diagnostics,
                source_map,
                file,
                error.start,
                error.end,
                error.code,
                error.message,
            );
            if let Some(token) = tokens
                .iter_mut()
                .find(|token| token.start as usize == error.start)
            {
                token.kind = TokenKind::Error;
            }
        }
        index = close + 1;
    }
}

pub(super) fn validate_lint_levels(
    source: &str,
    file: &AstFile,
    arena: &AstArena,
    tokens: &TokenBuffer,
    configured: &ConfiguredAst,
    diagnostics: &mut Vec<Diagnostic>,
) {
    struct Scope {
        start: u32,
        end: u32,
        forbid: u16,
        lower: Vec<(u16, Span)>,
    }
    let mut scopes = Vec::new();
    let mut add = |start, end, attributes: AstRange<Attribute>| {
        let mut scope = Scope {
            start,
            end,
            forbid: 0,
            lower: Vec::new(),
        };
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
                _ => continue,
            };
            let body = &tokens.tokens[open + 1..close];
            let Some(level) = body.first().map(|token| token.text(source)) else {
                continue;
            };
            if !matches!(level, "forbid" | "allow" | "warn") {
                continue;
            }
            debug_assert!(NAMES.len() <= u16::BITS as usize);
            let mut mask = 0;
            for token in &body[2..body.len() - 1] {
                if let Some(index) = NAMES.iter().position(|name| *name == token.text(source)) {
                    mask |= 1 << index;
                }
            }
            if level == "forbid" {
                scope.forbid |= mask;
            } else {
                scope.lower.push((mask, attribute.span.clone()));
            }
        }
        if scope.forbid != 0 || !scope.lower.is_empty() {
            scopes.push(scope);
        }
    };
    add(
        0,
        u32::try_from(source.len()).expect("源码长度受 SourceMap 限制"),
        file.inner_attributes,
    );
    for (index, item) in arena.items.iter().enumerate() {
        if configured.item_active(ItemId(index as u32)) {
            add(item.span.start(), item.span.end(), item.attributes);
        }
    }
    for (index, expression) in arena.exprs.iter().enumerate() {
        if configured.expr_active(ExprId(index as u32)) {
            add(
                expression.span.start(),
                expression.span.end(),
                expression.attributes,
            );
        }
    }
    for (index, statement) in arena.stmts.iter().enumerate() {
        if configured.stmt_active(StmtId(index as u32)) {
            add(
                statement.span.start(),
                statement.span.end(),
                statement.attributes,
            );
        }
    }
    scopes.sort_by_key(|scope| (scope.start, std::cmp::Reverse(scope.end)));
    let mut stack: Vec<(u32, u16)> = Vec::new();
    for scope in scopes {
        while stack.last().is_some_and(|(end, _)| *end <= scope.start) {
            stack.pop();
        }
        let mask = stack.last().map_or(0, |(_, mask)| *mask) | scope.forbid;
        for (lower, span) in scope.lower {
            if lower & mask != 0 {
                diagnostics.push(Diagnostic::error(
                    DiagnosticCode::InvalidDeclaration,
                    "内层 allow 或 warn 不能降低外层 forbid",
                    Some(span),
                ));
            }
        }
        stack.push((scope.end, mask));
    }
}

fn attribute_open(tokens: &[Token], hash: usize) -> Option<usize> {
    let next = hash + 1;
    if tokens
        .get(next)
        .is_some_and(|token| token.kind == TokenKind::LBracket)
    {
        return Some(next);
    }
    if tokens
        .get(next)
        .is_some_and(|token| token.kind == TokenKind::Not)
        && tokens
            .get(next + 1)
            .is_some_and(|token| token.kind == TokenKind::LBracket)
    {
        return Some(next + 1);
    }
    None
}

fn matching_rbracket(tokens: &[Token], open: usize) -> Option<usize> {
    let mut depth = 0_u32;
    for (offset, token) in tokens[open..].iter().enumerate() {
        match token.kind {
            TokenKind::LBracket => depth += 1,
            TokenKind::RBracket => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(open + offset);
                }
            }
            _ => {}
        }
    }
    None
}

struct AttrError {
    code: DiagnosticCode,
    start: usize,
    end: usize,
    message: String,
}

fn validate_attr_body(source: &str, body: &[Token]) -> Result<(), AttrError> {
    let Some(name_token) = body.first() else {
        return Err(AttrError {
            code: DiagnosticCode::LexUnknownAttribute,
            start: 0,
            end: 0,
            message: "属性不能为空".to_owned(),
        });
    };
    if name_token.kind != TokenKind::Ident
        && super::token::keyword_kind(name_token.text(source)).is_none()
    {
        return err(
            name_token,
            DiagnosticCode::LexUnknownAttribute,
            "属性名必须是标识符",
        );
    }
    let name = name_token.text(source);
    match name {
        "inline" | "cold" | "must_use" | "test" | "ignore" | "bench" | "coroutine_local"
        | "os_thread_local" | "track_caller" | "used" | "naked" => expect_bare(source, body),
        "should_panic" => validate_should_panic(source, body),
        "repr" => validate_repr(source, body),
        "derive" => validate_derive(source, body),
        "cfg" => validate_cfg(source, body),
        "allow" | "warn" | "deny" | "forbid" => validate_lint(source, body),
        "comptime" => validate_comptime(source, body),
        "ffi" => validate_ffi(source, body),
        "export_name" | "link_name" | "link_section" => validate_link_name(source, body),
        _ => err(
            name_token,
            DiagnosticCode::LexUnknownAttribute,
            format!("未知属性 `{name}`"),
        ),
    }
}

fn expect_bare(source: &str, body: &[Token]) -> Result<(), AttrError> {
    if body.len() == 1 {
        Ok(())
    } else {
        err(
            &body[1],
            DiagnosticCode::LexInvalidAttributeArg,
            format!("属性 `{}` 不能带参数", body[0].text(source)),
        )
    }
}

fn validate_should_panic(source: &str, body: &[Token]) -> Result<(), AttrError> {
    if body.len() == 1 {
        return Ok(());
    }
    expect_paren_args(body)?;
    let args = inner_parens(body);
    match args {
        [eq, assign, value]
            if eq.kind == TokenKind::Ident
                && eq.text(source) == "eq"
                && assign.kind == TokenKind::Eq
                && value.kind == TokenKind::String =>
        {
            Ok(())
        }
        _ => err(
            args.first().unwrap_or(&body[1]),
            DiagnosticCode::LexInvalidAttributeArg,
            "should_panic 参数必须是 eq = \"...\"",
        ),
    }
}

fn validate_repr(source: &str, body: &[Token]) -> Result<(), AttrError> {
    expect_paren_args(body)?;
    let args = inner_parens(body);
    if args.is_empty() {
        return err(
            &body[1],
            DiagnosticCode::LexInvalidAttributeArg,
            "repr 缺少参数",
        );
    }
    let mut index = 0;
    while index < args.len() {
        index = validate_repr_item(source, args, index)?;
        if index < args.len() {
            if args[index].kind != TokenKind::Comma {
                return err(
                    &args[index],
                    DiagnosticCode::LexInvalidAttributeArg,
                    "repr 参数必须用逗号分隔",
                );
            }
            index += 1;
        }
    }
    Ok(())
}

fn validate_repr_item(source: &str, args: &[Token], index: usize) -> Result<usize, AttrError> {
    let token = &args[index];
    if token.kind != TokenKind::Ident {
        return err(
            token,
            DiagnosticCode::LexInvalidAttributeArg,
            "未知 repr 参数",
        );
    }
    match token.text(source) {
        "C" | "u8" | "u16" | "u32" | "u64" | "packed" | "transparent" => Ok(index + 1),
        "align" => validate_align(args, index),
        name => err(
            token,
            DiagnosticCode::LexInvalidAttributeArg,
            format!("未知 repr 参数 `{name}`"),
        ),
    }
}

fn validate_align(args: &[Token], index: usize) -> Result<usize, AttrError> {
    match args.get(index + 1..index + 4) {
        Some([open, value, close])
            if open.kind == TokenKind::LParen
                && value.kind == TokenKind::Int
                && close.kind == TokenKind::RParen =>
        {
            Ok(index + 4)
        }
        _ => err(
            args.get(index + 1).unwrap_or(&args[index]),
            DiagnosticCode::LexInvalidAttributeArg,
            "repr(align(N)) 的 N 必须是整数",
        ),
    }
}

fn validate_derive(source: &str, body: &[Token]) -> Result<(), AttrError> {
    expect_paren_args(body)?;
    let args = inner_parens(body);
    if args.is_empty() {
        return err(
            &body[1],
            DiagnosticCode::LexInvalidAttributeArg,
            "derive 缺少参数",
        );
    }
    for (offset, token) in args.iter().enumerate() {
        if offset % 2 == 1 {
            if token.kind != TokenKind::Comma {
                return err(
                    token,
                    DiagnosticCode::LexInvalidAttributeArg,
                    "derive 参数必须用逗号分隔",
                );
            }
            continue;
        }
        if token.kind != TokenKind::Ident || !is_derive_trait(token.text(source)) {
            return err(
                token,
                DiagnosticCode::LexInvalidAttributeArg,
                format!("derive 不允许 `{}`", token.text(source)),
            );
        }
    }
    if args.len() % 2 == 0
        && args
            .last()
            .is_some_and(|token| token.kind == TokenKind::Comma)
    {
        return Ok(());
    }
    if args.len() % 2 == 1 {
        Ok(())
    } else {
        err(
            args.last().unwrap(),
            DiagnosticCode::LexInvalidAttributeArg,
            "derive 参数非法",
        )
    }
}

fn is_derive_trait(name: &str) -> bool {
    matches!(
        name,
        "Clone" | "Eq" | "Ord" | "Hash" | "StableHash" | "StableOrd" | "Print"
    )
}

fn validate_cfg(source: &str, body: &[Token]) -> Result<(), AttrError> {
    expect_paren_args(body)?;
    let args = inner_parens(body);
    if args.is_empty() {
        return err(
            &body[1],
            DiagnosticCode::LexInvalidAttributeArg,
            "cfg 谓词不能为空",
        );
    }
    validate_cfg_pred(source, args)
}

fn validate_cfg_pred(source: &str, tokens: &[Token]) -> Result<(), AttrError> {
    if tokens.is_empty() {
        return Err(AttrError {
            code: DiagnosticCode::LexInvalidAttributeArg,
            start: 0,
            end: 0,
            message: "cfg 谓词不能为空".to_owned(),
        });
    }
    let mut index = 0;
    while index < tokens.len() {
        index = validate_cfg_atom(source, tokens, index)?;
        if index < tokens.len() {
            if tokens[index].kind != TokenKind::Comma {
                return err(
                    &tokens[index],
                    DiagnosticCode::LexInvalidAttributeArg,
                    "cfg 谓词中出现非法记号组合",
                );
            }
            index += 1;
        }
    }
    Ok(())
}

fn validate_cfg_atom(source: &str, tokens: &[Token], index: usize) -> Result<usize, AttrError> {
    let token = &tokens[index];
    if !matches!(
        token.kind,
        TokenKind::Ident | TokenKind::KwTrue | TokenKind::KwFalse
    ) {
        return err(
            token,
            DiagnosticCode::LexInvalidAttributeArg,
            "cfg 谓词中出现非法记号组合",
        );
    }
    let name = token.text(source);
    if matches!(name, "not" | "all" | "any") {
        return validate_cfg_group(source, tokens, index);
    }
    if tokens
        .get(index + 1)
        .is_some_and(|next| next.kind == TokenKind::Eq)
    {
        let Some(value) = tokens.get(index + 2) else {
            return err(
                &tokens[index + 1],
                DiagnosticCode::LexInvalidAttributeArg,
                "cfg 赋值缺少字符串",
            );
        };
        if value.kind != TokenKind::String {
            return err(
                value,
                DiagnosticCode::LexInvalidAttributeArg,
                "cfg 赋值右侧必须是字符串",
            );
        }
        return Ok(index + 3);
    }
    Ok(index + 1)
}

fn validate_cfg_group(source: &str, tokens: &[Token], index: usize) -> Result<usize, AttrError> {
    let Some(open) = tokens.get(index + 1) else {
        return err(
            &tokens[index],
            DiagnosticCode::LexInvalidAttributeArg,
            "cfg 组合谓词必须带括号",
        );
    };
    if open.kind != TokenKind::LParen {
        return err(
            open,
            DiagnosticCode::LexInvalidAttributeArg,
            "cfg 组合谓词必须带括号",
        );
    }
    let close_offset = matching_paren(tokens, index + 1).ok_or_else(|| AttrError {
        code: DiagnosticCode::LexInvalidAttributeArg,
        start: open.start as usize,
        end: tokens
            .last()
            .map(|token| token.end as usize)
            .unwrap_or(open.end as usize),
        message: "cfg 括号未闭合".to_owned(),
    })?;
    validate_cfg_pred(source, &tokens[index + 2..close_offset])?;
    Ok(close_offset + 1)
}

fn matching_paren(tokens: &[Token], open: usize) -> Option<usize> {
    let mut depth = 0_u32;
    for (offset, token) in tokens[open..].iter().enumerate() {
        match token.kind {
            TokenKind::LParen => depth += 1,
            TokenKind::RParen => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(open + offset);
                }
            }
            _ => {}
        }
    }
    None
}

fn validate_lint(source: &str, body: &[Token]) -> Result<(), AttrError> {
    expect_paren_args(body)?;
    let args = inner_parens(body);
    if args.is_empty() {
        return err(
            &body[1],
            DiagnosticCode::LexInvalidAttributeArg,
            "lint 属性缺少名字",
        );
    }
    for (offset, token) in args.iter().enumerate() {
        if offset % 2 == 1 {
            if token.kind != TokenKind::Comma {
                return err(
                    token,
                    DiagnosticCode::LexInvalidAttributeArg,
                    "lint 名必须用逗号分隔",
                );
            }
            continue;
        }
        if token.kind != TokenKind::Ident || !is_lint_name(token.text(source)) {
            return err(
                token,
                DiagnosticCode::LexInvalidAttributeArg,
                format!("未知 lint `{}`", token.text(source)),
            );
        }
    }
    Ok(())
}

fn is_lint_name(name: &str) -> bool {
    NAMES.contains(&name)
}

fn validate_comptime(source: &str, body: &[Token]) -> Result<(), AttrError> {
    expect_paren_args(body)?;
    let args = inner_parens(body);
    match args {
        [name, eq, value]
            if name.kind == TokenKind::Ident
                && name.text(source) == "expansion_limit"
                && eq.kind == TokenKind::Eq
                && value.kind == TokenKind::Int =>
        {
            Ok(())
        }
        _ => err(
            args.first().unwrap_or(&body[1]),
            DiagnosticCode::LexInvalidAttributeArg,
            "comptime 属性必须是 expansion_limit = N",
        ),
    }
}

fn validate_ffi(source: &str, body: &[Token]) -> Result<(), AttrError> {
    expect_paren_args(body)?;
    let args = inner_parens(body);
    if args.is_empty() {
        return err(
            &body[1],
            DiagnosticCode::LexInvalidAttributeArg,
            "ffi 缺少参数",
        );
    }
    let first = &args[0];
    if first.kind != TokenKind::Ident {
        return err(
            first,
            DiagnosticCode::LexInvalidAttributeArg,
            "未知 ffi 参数",
        );
    }
    match first.text(source) {
        "bridge" | "dirty_cpu" if args.len() == 1 => Ok(()),
        "leaf" => validate_leaf_args(source, args),
        name => err(
            first,
            DiagnosticCode::LexInvalidAttributeArg,
            format!("未知 ffi 参数 `{name}`"),
        ),
    }
}

fn validate_leaf_args(source: &str, args: &[Token]) -> Result<(), AttrError> {
    if args.len() == 1 {
        return Ok(());
    }
    if args.len() == 6
        && args[1].kind == TokenKind::LParen
        && args[2].kind == TokenKind::Ident
        && args[2].text(source) == "stack"
        && args[3].kind == TokenKind::Eq
        && args[4].kind == TokenKind::Int
        && args[5].kind == TokenKind::RParen
    {
        return Ok(());
    }
    if args.len() == 5
        && args[1].kind == TokenKind::Comma
        && args[2].kind == TokenKind::Ident
        && args[2].text(source) == "stack"
        && args[3].kind == TokenKind::Eq
        && args[4].kind == TokenKind::Int
    {
        return Ok(());
    }
    err(
        &args[1],
        DiagnosticCode::LexInvalidAttributeArg,
        "ffi(leaf) 参数必须是 stack = N",
    )
}

fn validate_link_name(source: &str, body: &[Token]) -> Result<(), AttrError> {
    match body {
        [name, eq, value] if eq.kind == TokenKind::Eq && value.kind == TokenKind::String => Ok(()),
        _ => err(
            body.get(1).unwrap_or(&body[0]),
            DiagnosticCode::LexInvalidAttributeArg,
            format!("属性 `{}` 必须是 = \"...\"", body[0].text(source)),
        ),
    }
}

fn expect_paren_args(body: &[Token]) -> Result<(), AttrError> {
    if body.len() < 3
        || body[1].kind != TokenKind::LParen
        || body
            .last()
            .is_none_or(|token| token.kind != TokenKind::RParen)
    {
        return err(
            body.get(1).unwrap_or(&body[0]),
            DiagnosticCode::LexInvalidAttributeArg,
            "属性参数必须写在括号里",
        );
    }
    let mut depth = 0_u32;
    for token in &body[1..] {
        match token.kind {
            TokenKind::LParen => depth += 1,
            TokenKind::RParen => depth -= 1,
            _ => {}
        }
    }
    if depth == 0 {
        Ok(())
    } else {
        err(
            &body[1],
            DiagnosticCode::LexInvalidAttributeArg,
            "属性括号未闭合",
        )
    }
}

fn inner_parens(body: &[Token]) -> &[Token] {
    &body[2..body.len() - 1]
}

fn attr_err(token: &Token, code: DiagnosticCode, message: impl Into<String>) -> AttrError {
    AttrError {
        code,
        start: token.start as usize,
        end: token.end as usize,
        message: message.into(),
    }
}

fn err<T>(token: &Token, code: DiagnosticCode, message: impl Into<String>) -> Result<T, AttrError> {
    Err(attr_err(token, code, message))
}

fn emit(
    diagnostics: &mut Vec<Diagnostic>,
    source_map: &SourceMap,
    file: crate::source::SourceFileId,
    start: usize,
    end: usize,
    code: DiagnosticCode,
    message: impl Into<String>,
) {
    let span = source_map
        .span(file, start, end.max(start), ExpansionId::ROOT)
        .ok();
    diagnostics.push(Diagnostic::error(code, message, span));
}
