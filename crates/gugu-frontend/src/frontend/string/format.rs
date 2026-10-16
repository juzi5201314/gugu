use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) enum Alignment {
    Left,
    Center,
    Right,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) enum Sign {
    Plus,
    Minus,
    Space,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) enum FormatKind {
    #[default]
    Print,
    Debug,
    Binary,
    Octal,
    LowerHex,
    UpperHex,
    LowerExponent,
    UpperExponent,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct FormatSpec<C> {
    pub(crate) fill: char,
    pub(crate) alignment: Option<Alignment>,
    pub(crate) sign: Option<Sign>,
    pub(crate) alternate: bool,
    pub(crate) zero: bool,
    pub(crate) width: Option<C>,
    pub(crate) precision: Option<C>,
    pub(crate) kind: FormatKind,
}
#[derive(Clone, Copy, Debug)]
pub(crate) enum ParsedCount<'a> {
    Fixed(u64),
    Name(&'a str),
}

impl<C> Default for FormatSpec<C> {
    fn default() -> Self {
        Self {
            fill: ' ',
            alignment: None,
            sign: None,
            alternate: false,
            zero: false,
            width: None,
            precision: None,
            kind: FormatKind::Print,
        }
    }
}
/// 被格式化值在标志兼容规则里的分类。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ValueClass {
    Int,
    Float,
    Str,
    Other,
}

/// 标志与值类型不兼容时返回诊断文本；兼容返回 `None`。
///
/// 符号与零填充只给整数和浮点；精度只给浮点，以及 `Print` 下按标量截断的 string；
/// `#` 只对 `Debug` 与进制格式有意义。
pub(crate) fn flag_conflict<C>(spec: &FormatSpec<C>, class: ValueClass) -> Option<&'static str> {
    let numeric = matches!(class, ValueClass::Int | ValueClass::Float);
    if (spec.sign.is_some() || spec.zero) && !numeric {
        return Some("符号与零填充标志只适用于整数和浮点");
    }
    if spec.precision.is_some()
        && !(class == ValueClass::Float
            || (class == ValueClass::Str && spec.kind == FormatKind::Print))
    {
        return Some("精度只适用于浮点和 Print 格式的 string");
    }
    if spec.alternate
        && !matches!(
            spec.kind,
            FormatKind::Debug
                | FormatKind::Binary
                | FormatKind::Octal
                | FormatKind::LowerHex
                | FormatKind::UpperHex
        )
    {
        return Some("`#` 只适用于 Debug 与进制格式");
    }
    None
}

impl FormatKind {
    /// 格式码对应的语言 trait 与方法名。
    pub(crate) const fn trait_method(self) -> (&'static str, &'static str) {
        match self {
            Self::Print => ("Print", "print"),
            Self::Debug => ("Debug", "debug"),
            Self::Binary => ("Binary", "binary"),
            Self::Octal => ("Octal", "octal"),
            Self::LowerHex => ("LowerHex", "lower_hex"),
            Self::UpperHex => ("UpperHex", "upper_hex"),
            Self::LowerExponent => ("LowerExp", "lower_exp"),
            Self::UpperExponent => ("UpperExp", "upper_exp"),
        }
    }
}

impl<C> FormatSpec<C> {
    /// 去掉计数值、只保留标志与格式码的说明；用于兼容检查。
    pub(crate) fn flags(&self) -> FormatSpec<()> {
        FormatSpec {
            fill: self.fill,
            alignment: self.alignment,
            sign: self.sign,
            alternate: self.alternate,
            zero: self.zero,
            width: self.width.as_ref().map(|_| ()),
            precision: self.precision.as_ref().map(|_| ()),
            kind: self.kind,
        }
    }

    pub(crate) fn try_map<D, E>(
        self,
        mut map: impl FnMut(C) -> Result<D, E>,
    ) -> Result<FormatSpec<D>, E> {
        Ok(FormatSpec {
            fill: self.fill,
            alignment: self.alignment,
            sign: self.sign,
            alternate: self.alternate,
            zero: self.zero,
            width: self.width.map(&mut map).transpose()?,
            precision: self.precision.map(map).transpose()?,
            kind: self.kind,
        })
    }
}

pub(crate) fn format_spec_error(spec: &str) -> Option<&'static str> {
    parse_format(spec).err()
}

pub(crate) fn parse_format(spec: &str) -> Result<FormatSpec<ParsedCount<'_>>, &'static str> {
    let mut result = FormatSpec::default();
    let mut chars = spec.char_indices();
    let first = chars.next();
    let second = chars.next();
    let mut offset = if let Some((index, character)) = second
        && let Some(alignment) = alignment(character)
    {
        result.fill = first.expect("第二个字符存在时必有首字符").1;
        result.alignment = Some(alignment);
        index + character.len_utf8()
    } else if let Some((_, character)) = first
        && let Some(alignment) = alignment(character)
    {
        result.alignment = Some(alignment);
        character.len_utf8()
    } else {
        0
    };
    let bytes = spec.as_bytes();
    if let Some(&byte) = bytes.get(offset) {
        result.sign = match byte {
            b'+' => Some(Sign::Plus),
            b'-' => Some(Sign::Minus),
            b' ' => Some(Sign::Space),
            _ => None,
        };
        offset += usize::from(result.sign.is_some());
    }
    if bytes.get(offset) == Some(&b'#') {
        result.alternate = true;
        offset += 1;
    }
    if bytes.get(offset) == Some(&b'0') {
        result.zero = true;
        offset += 1;
    }
    result.width = count(spec, &mut offset)?;
    if bytes.get(offset) == Some(&b'.') {
        offset += 1;
        result.precision = count(spec, &mut offset)?;
        if result.precision.is_none() {
            return Err("格式精度必须是整数或 name$");
        }
    }
    if let Some(&byte) = bytes.get(offset) {
        result.kind = match byte {
            b'?' => FormatKind::Debug,
            b'b' => FormatKind::Binary,
            b'o' => FormatKind::Octal,
            b'x' => FormatKind::LowerHex,
            b'X' => FormatKind::UpperHex,
            b'e' => FormatKind::LowerExponent,
            b'E' => FormatKind::UpperExponent,
            _ => return Err("未知格式码"),
        };
        offset += 1;
    }
    if offset != spec.len() {
        return Err("未知格式码");
    }
    Ok(result)
}

fn alignment(character: char) -> Option<Alignment> {
    match character {
        '<' => Some(Alignment::Left),
        '^' => Some(Alignment::Center),
        '>' => Some(Alignment::Right),
        _ => None,
    }
}

fn count<'a>(spec: &'a str, offset: &mut usize) -> Result<Option<ParsedCount<'a>>, &'static str> {
    let bytes = spec.as_bytes();
    let start = *offset;
    if bytes.get(start).is_some_and(u8::is_ascii_digit) {
        while bytes.get(*offset).is_some_and(u8::is_ascii_digit) {
            *offset += 1;
        }
        let value = spec[start..*offset]
            .parse::<u64>()
            .map_err(|_| "格式宽度或精度超出 int 范围")?;
        if value > i64::MAX as u64 {
            return Err("格式宽度或精度超出 int 范围");
        }
        return Ok(Some(ParsedCount::Fixed(value)));
    }
    if bytes.get(start).copied().is_some_and(super::is_ident_start) {
        let mut end = start + 1;
        while bytes
            .get(end)
            .copied()
            .is_some_and(super::is_ident_continue)
        {
            end += 1;
        }
        if bytes.get(end) == Some(&b'$') {
            *offset = end + 1;
            return Ok(Some(ParsedCount::Name(&spec[start..end])));
        }
    }
    Ok(None)
}
