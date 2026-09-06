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
impl<C> FormatSpec<C> {
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
