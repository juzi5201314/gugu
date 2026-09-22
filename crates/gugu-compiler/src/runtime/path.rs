//! `std.path` 的确定性参照模型。
//!
//! Linux 只保存原始字节，Windows 只保存 UTF-16 code unit。非法 UTF-8 与 lone
//! surrogate 都不会被提前替换。所有操作返回新值，不读取目录、不解析 `.` / `..`。

use super::hash::Hasher;
use super::io::{self, IoError};
use super::text::{utf8_decode, utf16_decode};

/// 路径比较与分隔符规则所属的目标。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PathTarget {
    Linux,
    Windows,
}

/// 一次 Unicode 失败。`offset` 是原生 code unit 序号。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct UnicodeError {
    offset: usize,
}

/// 无损的原生字符串。Linux 填 `linux_bytes`，Windows 填 `units`。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct OsString {
    units: Vec<u16>,
    linux_bytes: Option<Vec<u8>>,
    target: PathTarget,
}

/// 不可变路径值。变换只产生新值。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PathValue {
    text: OsString,
}

const LINUX_SEPARATOR: u8 = b'/';
const WINDOWS_SEPARATORS: [u16; 2] = [b'\\' as u16, b'/' as u16];

impl UnicodeError {
    pub(crate) const fn offset(self) -> usize {
        self.offset
    }

    pub(crate) const fn message(self) -> &'static str {
        "invalid unicode"
    }

    pub(crate) const fn source(self) -> Option<&'static IoError> {
        None
    }
}

impl OsString {
    pub(crate) fn from_string(value: &str, target: PathTarget) -> Result<Self, IoError> {
        if value.contains('\0') {
            return Err(IoError::bare(io::INVALID_INPUT));
        }
        match target {
            PathTarget::Linux => Ok(Self::linux(value.as_bytes().to_vec())),
            PathTarget::Windows => Ok(Self::windows(value.encode_utf16().collect())),
        }
    }

    pub(crate) fn from_bytes(value: &[u8], target: PathTarget) -> Result<Self, UnicodeError> {
        match target {
            PathTarget::Linux => Ok(Self::linux(value.to_vec())),
            PathTarget::Windows => {
                let text = utf8_decode(value).map_err(UnicodeError::new)?;
                Ok(Self::windows(text.encode_utf16().collect()))
            }
        }
    }

    pub(crate) fn from_units(value: &[u16], target: PathTarget) -> Result<Self, PathError> {
        match target {
            PathTarget::Linux => Err(PathError::Io(IoError::bare(io::UNSUPPORTED))),
            PathTarget::Windows => {
                validate_units(value)?;
                Ok(Self::windows(value.to_vec()))
            }
        }
    }

    pub(crate) fn to_bytes(&self) -> Vec<u8> {
        match self.target {
            PathTarget::Linux => self.linux_bytes.clone().unwrap_or_default(),
            PathTarget::Windows => String::from_utf16_lossy(&self.units).into_bytes(),
        }
    }

    pub(crate) fn to_units(&self) -> Vec<u16> {
        match self.target {
            PathTarget::Linux => self
                .linux_bytes
                .as_deref()
                .unwrap_or_default()
                .iter()
                .map(|byte| u16::from(*byte))
                .collect(),
            PathTarget::Windows => self.units.clone(),
        }
    }

    pub(crate) fn to_string(&self) -> Result<String, UnicodeError> {
        match self.target {
            PathTarget::Linux => utf8_decode(self.linux_bytes.as_deref().unwrap_or_default())
                .map_err(UnicodeError::new),
            PathTarget::Windows => utf16_decode(&self.units).map_err(UnicodeError::new),
        }
    }

    pub(crate) fn to_string_lossy(&self) -> String {
        match self.target {
            PathTarget::Linux => {
                String::from_utf8_lossy(self.linux_bytes.as_deref().unwrap_or_default())
                    .into_owned()
            }
            PathTarget::Windows => String::from_utf16_lossy(&self.units),
        }
    }

    pub(crate) fn len(&self) -> usize {
        match self.target {
            PathTarget::Linux => self.linux_bytes.as_deref().unwrap_or_default().len(),
            PathTarget::Windows => self.units.len(),
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub(crate) const fn target(&self) -> PathTarget {
        self.target
    }

    /// 按原生序列做长度前缀馈送。Windows 喂 UTF-16LE 字节。
    pub(crate) fn feed(&self, hasher: &mut Hasher) {
        match self.target {
            PathTarget::Linux => {
                hasher.write_bytes(self.linux_bytes.as_deref().unwrap_or_default())
            }
            PathTarget::Windows => {
                let mut bytes = Vec::with_capacity(self.units.len().saturating_mul(2));
                for unit in &self.units {
                    bytes.extend_from_slice(&unit.to_le_bytes());
                }
                hasher.write_bytes(&bytes);
            }
        }
    }

    fn linux(bytes: Vec<u8>) -> Self {
        Self {
            units: Vec::new(),
            linux_bytes: Some(bytes),
            target: PathTarget::Linux,
        }
    }

    fn windows(units: Vec<u16>) -> Self {
        Self {
            units,
            linux_bytes: None,
            target: PathTarget::Windows,
        }
    }
}

impl PartialOrd for OsString {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for OsString {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        debug_assert_eq!(self.target, other.target, "只比较同一目标的原生序列");
        match self.target {
            PathTarget::Linux => self
                .linux_bytes
                .as_deref()
                .cmp(&other.linux_bytes.as_deref()),
            PathTarget::Windows => folded_units(&self.units).cmp(&folded_units(&other.units)),
        }
    }
}

/// 构造或比较路径时的失败。Windows 字节入口失败是 Unicode，Linux UTF-16 入口是 I/O。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PathError {
    Io(IoError),
    Unicode(UnicodeError),
}

impl PathValue {
    pub(crate) fn from_os(value: OsString) -> Self {
        Self { text: value }
    }

    pub(crate) fn from_string(value: &str, target: PathTarget) -> Result<Self, IoError> {
        Ok(Self::from_os(OsString::from_string(value, target)?))
    }

    pub(crate) fn as_os(&self) -> OsString {
        self.text.clone()
    }

    pub(crate) fn to_string(&self) -> Result<String, UnicodeError> {
        self.text.to_string()
    }

    pub(crate) fn to_string_lossy(&self) -> String {
        self.text.to_string_lossy()
    }

    pub(crate) fn is_absolute(&self) -> bool {
        match self.text.target {
            PathTarget::Linux => self
                .text
                .linux_bytes
                .as_deref()
                .unwrap_or_default()
                .first()
                .is_some_and(|byte| *byte == LINUX_SEPARATOR),
            PathTarget::Windows => windows_absolute(&self.text.units),
        }
    }

    pub(crate) fn is_relative(&self) -> bool {
        !self.is_absolute()
    }

    pub(crate) fn join(&self, child: &Self) -> Self {
        if child.is_absolute() || self.text.is_empty() {
            return child.clone();
        }
        if self.ends_with_separator() {
            return self.concat(child);
        }
        self.concat_with_separator(child)
    }

    pub(crate) fn components(&self) -> Vec<OsString> {
        split_components(&self.text)
    }

    pub(crate) fn parent(&self) -> Option<Self> {
        if self.is_root() || !self.contains_separator() {
            return None;
        }
        let parts = self.components();
        let kept = parts.len().saturating_sub(1);
        if kept == 0 {
            return None;
        }
        Some(join_components(&parts[..kept], self.text.target))
    }

    pub(crate) fn file_name(&self) -> Option<OsString> {
        self.components()
            .into_iter()
            .rev()
            .find(|part| !part.is_empty())
    }

    pub(crate) fn extension(&self) -> Option<OsString> {
        let name = self.file_name()?;
        extension_of(&name).map(|(_, extension)| extension)
    }

    pub(crate) fn file_stem(&self) -> Option<OsString> {
        let name = self.file_name()?;
        Some(match extension_of(&name) {
            Some((stem, _)) => stem,
            None => name,
        })
    }

    pub(crate) fn with_file_name(&self, name: OsString) -> Self {
        let Some(file_name) = self.file_name() else {
            return self.join(&Self::from_os(name));
        };
        let parts = self.components();
        let mut replaced = Vec::with_capacity(parts.len());
        let mut seen = false;
        for part in parts.into_iter().rev() {
            if !seen && part == file_name {
                replaced.push(name.clone());
                seen = true;
            } else {
                replaced.push(part);
            }
        }
        replaced.reverse();
        join_components(&replaced, self.text.target)
    }

    pub(crate) fn with_extension(&self, extension: OsString) -> Self {
        let Some(name) = self.file_name() else {
            return self.clone();
        };
        let replacement = match extension_of(&name) {
            Some((stem, _)) if extension.is_empty() => stem,
            Some((stem, _)) => concat_name(&stem, &extension),
            None if extension.is_empty() => name,
            None => concat_name(&name, &extension),
        };
        self.with_file_name(replacement)
    }

    fn ends_with_separator(&self) -> bool {
        match self.text.target {
            PathTarget::Linux => self
                .text
                .linux_bytes
                .as_deref()
                .unwrap_or_default()
                .last()
                .is_some_and(|byte| *byte == LINUX_SEPARATOR),
            PathTarget::Windows => self
                .text
                .units
                .last()
                .is_some_and(|unit| WINDOWS_SEPARATORS.contains(unit)),
        }
    }

    fn contains_separator(&self) -> bool {
        match self.text.target {
            PathTarget::Linux => self
                .text
                .linux_bytes
                .as_deref()
                .unwrap_or_default()
                .contains(&LINUX_SEPARATOR),
            PathTarget::Windows => self
                .text
                .units
                .iter()
                .any(|unit| WINDOWS_SEPARATORS.contains(unit)),
        }
    }

    fn is_root(&self) -> bool {
        match self.text.target {
            PathTarget::Linux => self.text.linux_bytes.as_deref() == Some(b"/"),
            PathTarget::Windows => {
                let units = &self.text.units;
                windows_drive_root(units) || windows_unc_root(units)
            }
        }
    }

    fn concat(&self, child: &Self) -> Self {
        match self.text.target {
            PathTarget::Linux => Self::from_os(OsString::linux(concat_bytes(
                self.text.linux_bytes.as_deref().unwrap_or_default(),
                child.text.linux_bytes.as_deref().unwrap_or_default(),
            ))),
            PathTarget::Windows => Self::from_os(OsString::windows(concat_units(
                &self.text.units,
                &child.text.units,
            ))),
        }
    }

    fn concat_with_separator(&self, child: &Self) -> Self {
        match self.text.target {
            PathTarget::Linux => {
                let mut bytes = self.text.linux_bytes.clone().unwrap_or_default();
                bytes.push(LINUX_SEPARATOR);
                bytes.extend_from_slice(child.text.linux_bytes.as_deref().unwrap_or_default());
                Self::from_os(OsString::linux(bytes))
            }
            PathTarget::Windows => {
                let mut units = self.text.units.clone();
                units.push(u16::from(b'\\'));
                units.extend_from_slice(&child.text.units);
                Self::from_os(OsString::windows(units))
            }
        }
    }
}

impl UnicodeError {
    const fn new(offset: usize) -> Self {
        Self { offset }
    }
}

fn validate_units(value: &[u16]) -> Result<(), PathError> {
    if let Some(offset) = value.iter().position(|unit| *unit == 0) {
        return Err(PathError::Unicode(UnicodeError::new(offset)));
    }
    let mut index = 0;
    while index < value.len() {
        let unit = value[index];
        if is_high_surrogate(unit) {
            let next = value.get(index + 1).copied();
            if !next.is_some_and(is_low_surrogate) {
                return Err(PathError::Unicode(UnicodeError::new(index)));
            }
            index += 2;
            continue;
        }
        if is_low_surrogate(unit) {
            return Err(PathError::Unicode(UnicodeError::new(index)));
        }
        index += 1;
    }
    Ok(())
}

fn is_high_surrogate(unit: u16) -> bool {
    (0xD800..=0xDBFF).contains(&unit)
}

fn is_low_surrogate(unit: u16) -> bool {
    (0xDC00..=0xDFFF).contains(&unit)
}

fn windows_absolute(units: &[u16]) -> bool {
    windows_drive_root(units)
        || units.len() >= 2
            && WINDOWS_SEPARATORS.contains(&units[0])
            && WINDOWS_SEPARATORS.contains(&units[1])
}

fn windows_drive_root(units: &[u16]) -> bool {
    units.len() >= 3
        && (units[0] as u8).is_ascii_alphabetic()
        && units[1] == u16::from(b':')
        && WINDOWS_SEPARATORS.contains(&units[2])
}

fn windows_unc_root(units: &[u16]) -> bool {
    units.len() > 2
        && WINDOWS_SEPARATORS.contains(&units[0])
        && WINDOWS_SEPARATORS.contains(&units[1])
        && !units[2..]
            .iter()
            .any(|unit| WINDOWS_SEPARATORS.contains(unit))
}

fn split_components(text: &OsString) -> Vec<OsString> {
    match text.target {
        PathTarget::Linux => text
            .linux_bytes
            .as_deref()
            .unwrap_or_default()
            .split(|byte| *byte == LINUX_SEPARATOR)
            .map(|part| OsString::linux(part.to_vec()))
            .collect(),
        PathTarget::Windows => split_windows(&text.units),
    }
}

fn split_windows(units: &[u16]) -> Vec<OsString> {
    let mut parts = Vec::new();
    let mut start = 0;
    for (index, unit) in units.iter().enumerate() {
        if WINDOWS_SEPARATORS.contains(unit) {
            parts.push(OsString::windows(units[start..index].to_vec()));
            start = index + 1;
        }
    }
    parts.push(OsString::windows(units[start..].to_vec()));
    parts
}

fn join_components(parts: &[OsString], target: PathTarget) -> PathValue {
    match target {
        PathTarget::Linux => {
            let mut bytes = Vec::new();
            for (index, part) in parts.iter().enumerate() {
                if index > 0 {
                    bytes.push(LINUX_SEPARATOR);
                }
                bytes.extend_from_slice(part.linux_bytes.as_deref().unwrap_or_default());
            }
            PathValue::from_os(OsString::linux(bytes))
        }
        PathTarget::Windows => {
            let mut units = Vec::new();
            for (index, part) in parts.iter().enumerate() {
                if index > 0 {
                    units.push(u16::from(b'\\'));
                }
                units.extend_from_slice(&part.units);
            }
            PathValue::from_os(OsString::windows(units))
        }
    }
}

fn extension_of(name: &OsString) -> Option<(OsString, OsString)> {
    match name.target {
        PathTarget::Linux => {
            let bytes = name.linux_bytes.as_deref().unwrap_or_default();
            let dot = bytes.iter().rposition(|byte| *byte == b'.')?;
            if dot == 0 || dot + 1 == bytes.len() {
                return None;
            }
            Some((
                OsString::linux(bytes[..dot].to_vec()),
                OsString::linux(bytes[dot + 1..].to_vec()),
            ))
        }
        PathTarget::Windows => {
            let dot = name
                .units
                .iter()
                .rposition(|unit| *unit == u16::from(b'.'))?;
            if dot == 0 || dot + 1 == name.units.len() {
                return None;
            }
            Some((
                OsString::windows(name.units[..dot].to_vec()),
                OsString::windows(name.units[dot + 1..].to_vec()),
            ))
        }
    }
}

fn concat_name(stem: &OsString, extension: &OsString) -> OsString {
    match stem.target {
        PathTarget::Linux => {
            let mut bytes = stem.linux_bytes.clone().unwrap_or_default();
            bytes.push(b'.');
            bytes.extend_from_slice(extension.linux_bytes.as_deref().unwrap_or_default());
            OsString::linux(bytes)
        }
        PathTarget::Windows => {
            let mut units = stem.units.clone();
            units.push(u16::from(b'.'));
            units.extend_from_slice(&extension.units);
            OsString::windows(units)
        }
    }
}

fn concat_bytes(left: &[u8], right: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(left.len().saturating_add(right.len()));
    bytes.extend_from_slice(left);
    bytes.extend_from_slice(right);
    bytes
}

fn concat_units(left: &[u16], right: &[u16]) -> Vec<u16> {
    let mut units = Vec::with_capacity(left.len().saturating_add(right.len()));
    units.extend_from_slice(left);
    units.extend_from_slice(right);
    units
}

fn folded_units(units: &[u16]) -> Vec<u16> {
    units
        .iter()
        .map(|unit| {
            if WINDOWS_SEPARATORS.contains(unit) {
                u16::from(b'\\')
            } else if (0x20..0x7F).contains(unit) {
                u16::from((*unit as u8).to_ascii_lowercase())
            } else {
                *unit
            }
        })
        .collect()
}
