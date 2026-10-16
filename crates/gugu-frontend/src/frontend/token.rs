use crate::source::SourceFileId;

use super::intern::{Symbol, SymbolInterner};

/// 词法记号种类。空白、换行与注释不在此枚举中，见 [`TriviaKind`]。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TokenKind {
    Ident,
    Int,
    Float,
    Char,
    ByteChar,
    String,
    ByteString,
    CString,
    RawString,
    FStringStart,
    FStringText,
    FStringInterpOpen,
    FStringInterpClose,
    FStringEnd,
    FormatSpec,
    KwAs,
    KwAlignOf,
    KwAsm,
    KwAsync,
    KwBreak,
    KwChan,
    KwComptime,
    KwConst,
    KwContinue,
    KwDefer,
    KwDyn,
    KwElse,
    KwEnum,
    KwExtern,
    KwFalse,
    KwFn,
    KwFor,
    KwGlobalAsm,
    KwIf,
    KwImpl,
    KwIn,
    KwLet,
    KwLoop,
    KwMatch,
    KwOffsetOf,
    KwPub,
    KwReturn,
    KwSelect,
    KwSizeOf,
    KwStatic,
    KwStruct,
    KwTrait,
    KwTrue,
    KwTry,
    KwType,
    KwTypeId,
    KwTypeIdCount,
    KwUnion,
    KwUnsafe,
    KwUse,
    KwWhile,
    KwYield,
    LParen,
    RParen,
    LBrace,
    RBrace,
    LBracket,
    RBracket,
    Dot,
    Comma,
    Colon,
    Semi,
    Eq,
    EqEq,
    Ne,
    Lt,
    Gt,
    Le,
    Ge,
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    PlusEq,
    MinusEq,
    StarEq,
    SlashEq,
    PercentEq,
    AndAnd,
    OrOr,
    Not,
    Tilde,
    And,
    Or,
    Caret,
    Shl,
    Shr,
    AndEq,
    OrEq,
    CaretEq,
    ShlEq,
    ShrEq,
    Hash,
    DotDotDot,
    FatArrow,
    Question,
    PathSep,
    DotDot,
    At,
    Eof,
    Error,
}

impl TokenKind {
    /// 行末出现该记号时，换行必须续行，不能单独结束语句。
    pub(crate) const fn continues_line(self) -> bool {
        matches!(
            self,
            Self::Plus
                | Self::Minus
                | Self::Star
                | Self::Slash
                | Self::Percent
                | Self::EqEq
                | Self::Ne
                | Self::Lt
                | Self::Gt
                | Self::Le
                | Self::Ge
                | Self::AndAnd
                | Self::OrOr
                | Self::And
                | Self::Or
                | Self::Caret
                | Self::Shl
                | Self::Shr
                | Self::PlusEq
                | Self::MinusEq
                | Self::StarEq
                | Self::SlashEq
                | Self::PercentEq
                | Self::AndEq
                | Self::OrEq
                | Self::CaretEq
                | Self::ShlEq
                | Self::ShrEq
                | Self::LParen
                | Self::LBracket
                | Self::LBrace
                | Self::Comma
                | Self::Dot
                | Self::PathSep
                | Self::DotDot
                | Self::Colon
                | Self::Eq
                | Self::FatArrow
                | Self::Question
        )
    }

    pub(crate) const fn is_error(self) -> bool {
        matches!(self, Self::Error)
    }
}

/// Trivia 种类：空白、换行与注释连续保存在 token 之前的范围里。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TriviaKind {
    Whitespace,
    Newline,
    LineComment,
    DocComment,
    InnerDocComment,
    BlockComment,
}

/// 一个带半开字节区间的 trivia。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Trivia {
    pub(crate) kind: TriviaKind,
    pub(crate) start: u32,
    pub(crate) end: u32,
}

/// 一个带精确 span 的词法记号。
///
/// AoS：parser 顺序扫描 kind/区间；payload 只在 ident 与字面量上使用。
/// `trivia_start`/`trivia_len` 指向同一 [`TokenBuffer`] 的 trivia 切片。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Token {
    pub(crate) kind: TokenKind,
    pub(crate) start: u32,
    pub(crate) end: u32,
    pub(crate) trivia_start: u32,
    pub(crate) trivia_len: u32,
    pub(crate) symbol: Option<Symbol>,
}

impl Token {
    pub(crate) fn text<'a>(&self, source: &'a str) -> &'a str {
        &source[self.start as usize..self.end as usize]
    }
}

/// 一个源文件的 token 与 trivia 连续缓冲。
#[derive(Clone, Debug, Default)]
pub(crate) struct TokenBuffer {
    pub(crate) file: Option<SourceFileId>,
    pub(crate) tokens: Vec<Token>,
    pub(crate) trivia: Vec<Trivia>,
    pub(crate) intern: SymbolInterner,
}

impl TokenBuffer {
    pub(crate) fn has_error_tokens(&self) -> bool {
        self.tokens.iter().any(|token| token.kind.is_error())
    }

    #[cfg(test)]
    pub(crate) fn leading_trivia(&self, token: &Token) -> &[Trivia] {
        let start = token.trivia_start as usize;
        let end = start + token.trivia_len as usize;
        &self.trivia[start..end]
    }
}

pub(crate) fn keyword_kind(ident: &str) -> Option<TokenKind> {
    Some(match ident {
        "as" => TokenKind::KwAs,
        "align_of" => TokenKind::KwAlignOf,
        "asm" => TokenKind::KwAsm,
        "async" => TokenKind::KwAsync,
        "break" => TokenKind::KwBreak,
        "chan" => TokenKind::KwChan,
        "comptime" => TokenKind::KwComptime,
        "const" => TokenKind::KwConst,
        "continue" => TokenKind::KwContinue,
        "defer" => TokenKind::KwDefer,
        "dyn" => TokenKind::KwDyn,
        "else" => TokenKind::KwElse,
        "enum" => TokenKind::KwEnum,
        "extern" => TokenKind::KwExtern,
        "false" => TokenKind::KwFalse,
        "fn" => TokenKind::KwFn,
        "for" => TokenKind::KwFor,
        "global_asm" => TokenKind::KwGlobalAsm,
        "if" => TokenKind::KwIf,
        "impl" => TokenKind::KwImpl,
        "in" => TokenKind::KwIn,
        "let" => TokenKind::KwLet,
        "loop" => TokenKind::KwLoop,
        "match" => TokenKind::KwMatch,
        "offset_of" => TokenKind::KwOffsetOf,
        "pub" => TokenKind::KwPub,
        "return" => TokenKind::KwReturn,
        "select" => TokenKind::KwSelect,
        "size_of" => TokenKind::KwSizeOf,
        "static" => TokenKind::KwStatic,
        "struct" => TokenKind::KwStruct,
        "trait" => TokenKind::KwTrait,
        "true" => TokenKind::KwTrue,
        "try" => TokenKind::KwTry,
        "type" => TokenKind::KwType,
        "type_id" => TokenKind::KwTypeId,
        "type_id_count" => TokenKind::KwTypeIdCount,
        "union" => TokenKind::KwUnion,
        "unsafe" => TokenKind::KwUnsafe,
        "use" => TokenKind::KwUse,
        "while" => TokenKind::KwWhile,
        "yield" => TokenKind::KwYield,
        _ => return None,
    })
}

pub(crate) fn checked_u32(value: usize) -> u32 {
    debug_assert!(value <= u32::MAX as usize, "源偏移超过 u32");
    value as u32
}

const _: () = {
    assert!(TokenKind::Eq.continues_line());
    assert!(TokenKind::Question.continues_line());
    assert!(!TokenKind::Ident.continues_line());
    assert!(!TokenKind::RBrace.continues_line());
};
