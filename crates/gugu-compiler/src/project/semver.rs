use std::{cmp::Ordering, collections::BTreeSet, fmt};

/// SemVer 2.0.0 版本。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Version {
    major: u64,
    minor: u64,
    patch: u64,
    pre: Vec<String>,
    build: Vec<String>,
}

impl Version {
    /// 解析规范 SemVer 文本。
    pub fn parse(input: &str) -> Result<Self, String> {
        let (without_build, build) = input
            .split_once('+')
            .map_or((input, None), |(value, build)| (value, Some(build)));
        let (core, pre) = without_build
            .split_once('-')
            .map_or((without_build, None), |(value, pre)| (value, Some(pre)));
        let mut components = core.split('.');
        let major = parse_number(components.next(), "major")?;
        let minor = parse_number(components.next(), "minor")?;
        let patch = parse_number(components.next(), "patch")?;
        if components.next().is_some() {
            return Err("版本必须恰好包含 major.minor.patch".to_owned());
        }
        Ok(Self {
            major,
            minor,
            patch,
            pre: parse_identifiers(pre, true)?,
            build: parse_identifiers(build, false)?,
        })
    }

    /// 创建不含预发布和 build metadata 的版本。
    pub fn new(major: u64, minor: u64, patch: u64) -> Self {
        Self {
            major,
            minor,
            patch,
            pre: Vec::new(),
            build: Vec::new(),
        }
    }

    /// 返回 major 分量。
    pub fn major(&self) -> u64 {
        self.major
    }

    /// 返回 minor 分量。
    pub fn minor(&self) -> u64 {
        self.minor
    }

    /// 返回 patch 分量。
    pub fn patch(&self) -> u64 {
        self.patch
    }

    /// 判断版本是否为预发布版本。
    pub fn is_prerelease(&self) -> bool {
        !self.pre.is_empty()
    }

    fn precedence_cmp(&self, other: &Self) -> Ordering {
        (self.major, self.minor, self.patch)
            .cmp(&(other.major, other.minor, other.patch))
            .then_with(|| compare_prerelease(&self.pre, &other.pre))
    }

    fn prerelease_base(&self) -> (u64, u64, u64) {
        (self.major, self.minor, self.patch)
    }
}

impl fmt::Display for Version {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}.{}.{}", self.major, self.minor, self.patch)?;
        if !self.pre.is_empty() {
            write!(formatter, "-{}", self.pre.join("."))?;
        }
        if !self.build.is_empty() {
            write!(formatter, "+{}", self.build.join("."))?;
        }
        Ok(())
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        self.precedence_cmp(other)
            .then_with(|| self.to_string().cmp(&other.to_string()))
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

fn parse_number(value: Option<&str>, field: &str) -> Result<u64, String> {
    let value = value.ok_or_else(|| format!("缺少 {field} 版本分量"))?;
    if value.is_empty() || (value.len() > 1 && value.starts_with('0')) {
        return Err(format!("{field} 不是规范十进制整数"));
    }
    value
        .parse()
        .map_err(|_| format!("{field} 超出无符号整数范围"))
}

fn parse_identifiers(
    value: Option<&str>,
    reject_numeric_leading_zero: bool,
) -> Result<Vec<String>, String> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    if value.is_empty() {
        return Err("版本标识符不能为空".to_owned());
    }
    value
        .split('.')
        .map(|identifier| {
            if identifier.is_empty()
                || !identifier
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
            {
                return Err(format!("非法版本标识符 `{identifier}`"));
            }
            if reject_numeric_leading_zero
                && identifier.len() > 1
                && identifier.bytes().all(|byte| byte.is_ascii_digit())
                && identifier.starts_with('0')
            {
                return Err(format!("数字版本标识符 `{identifier}` 不能有前导零"));
            }
            Ok(identifier.to_owned())
        })
        .collect()
}

fn compare_prerelease(left: &[String], right: &[String]) -> Ordering {
    match (left.is_empty(), right.is_empty()) {
        (true, true) => Ordering::Equal,
        (true, false) => Ordering::Greater,
        (false, true) => Ordering::Less,
        (false, false) => left
            .iter()
            .zip(right)
            .map(|(left, right)| compare_identifier(left, right))
            .find(|ordering| *ordering != Ordering::Equal)
            .unwrap_or_else(|| left.len().cmp(&right.len())),
    }
}

fn compare_identifier(left: &str, right: &str) -> Ordering {
    match (left.parse::<u64>(), right.parse::<u64>()) {
        (Ok(left), Ok(right)) => left.cmp(&right),
        (Ok(_), Err(_)) => Ordering::Less,
        (Err(_), Ok(_)) => Ordering::Greater,
        (Err(_), Err(_)) => left.cmp(right),
    }
}

/// 一个可组合的 SemVer 版本约束。
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct VersionReq {
    comparators: Vec<Comparator>,
    prerelease_bases: BTreeSet<(u64, u64, u64)>,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum Comparator {
    Any,
    Greater(Version),
    GreaterEqual(Version),
    Less(Version),
    LessEqual(Version),
    Exact(Version),
}

impl VersionReq {
    /// 解析 Cargo 风格的 SemVer 约束。
    pub fn parse(input: &str) -> Result<Self, String> {
        let input = input.trim();
        if input.is_empty() {
            return Err("版本约束不能为空".to_owned());
        }
        let mut comparators = Vec::new();
        let mut prerelease_bases = BTreeSet::new();
        for part in input.split(',') {
            let part = part.trim();
            if part.is_empty() {
                return Err("版本约束不能包含空交集项".to_owned());
            }
            for comparator in parse_comparator(part)? {
                if let Some(version) = comparator_version(&comparator) {
                    if version.is_prerelease() {
                        prerelease_bases.insert(version.prerelease_base());
                    }
                }
                comparators.push(comparator);
            }
        }
        Ok(Self {
            comparators,
            prerelease_bases,
        })
    }

    /// 判断一个版本是否满足约束。
    pub fn matches(&self, version: &Version) -> bool {
        (!version.is_prerelease() || self.prerelease_bases.contains(&version.prerelease_base()))
            && self.comparators.iter().all(|comparator| match comparator {
                Comparator::Any => true,
                Comparator::Greater(bound) => version.precedence_cmp(bound) == Ordering::Greater,
                Comparator::GreaterEqual(bound) => version.precedence_cmp(bound) != Ordering::Less,
                Comparator::Less(bound) => version.precedence_cmp(bound) == Ordering::Less,
                Comparator::LessEqual(bound) => version.precedence_cmp(bound) != Ordering::Greater,
                Comparator::Exact(bound) => version.precedence_cmp(bound) == Ordering::Equal,
            })
    }
}

impl Default for VersionReq {
    fn default() -> Self {
        Self::parse("*").expect("通配版本约束恒合法")
    }
}

impl fmt::Display for VersionReq {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, comparator) in self.comparators.iter().enumerate() {
            if index > 0 {
                formatter.write_str(", ")?;
            }
            comparator.fmt(formatter)?;
        }
        Ok(())
    }
}

impl fmt::Display for Comparator {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Any => formatter.write_str("*"),
            Self::Greater(value) => write!(formatter, ">{value}"),
            Self::GreaterEqual(value) => write!(formatter, ">={value}"),
            Self::Less(value) => write!(formatter, "<{value}"),
            Self::LessEqual(value) => write!(formatter, "<={value}"),
            Self::Exact(value) => write!(formatter, "={value}"),
        }
    }
}

fn comparator_version(comparator: &Comparator) -> Option<&Version> {
    match comparator {
        Comparator::Any => None,
        Comparator::Greater(value)
        | Comparator::GreaterEqual(value)
        | Comparator::Less(value)
        | Comparator::LessEqual(value)
        | Comparator::Exact(value) => Some(value),
    }
}

fn parse_comparator(input: &str) -> Result<Vec<Comparator>, String> {
    let (operator, value) = [">=", "<=", "^", "~", ">", "<", "="]
        .iter()
        .find_map(|operator| input.strip_prefix(operator).map(|value| (*operator, value)))
        .unwrap_or(("", input));
    let value = value.trim();
    let wildcard = value == "*" || value.eq_ignore_ascii_case("x");
    if wildcard {
        return if operator.is_empty() {
            Ok(vec![Comparator::Any])
        } else {
            Err(format!("版本约束 `{input}` 不能对通配符使用 `{operator}`"))
        };
    }
    let (components, pre, build, wildcard) = parse_partial_version(value)?;
    if components.is_empty() {
        return Ok(vec![Comparator::Any]);
    }
    if wildcard {
        return Ok(wildcard_bounds(components));
    }
    if operator == "^" || (operator.is_empty() && components.len() < 3) {
        return caret_bounds(components, pre, build);
    }
    if operator == "~" {
        return tilde_bounds(components, pre, build);
    }
    let version = version_from_components(&components, pre, build);
    if components.len() < 3 {
        return match operator {
            ">" => Ok(vec![Comparator::Greater(version)]),
            ">=" => Ok(vec![Comparator::GreaterEqual(version)]),
            "<" => Ok(vec![Comparator::Less(version)]),
            "<=" => Ok(vec![Comparator::LessEqual(version)]),
            "=" => Ok(wildcard_bounds(components)),
            _ => Err(format!("未知版本运算符 `{operator}`")),
        };
    }
    Ok(vec![match operator {
        ">" => Comparator::Greater(version),
        ">=" => Comparator::GreaterEqual(version),
        "<" => Comparator::Less(version),
        "<=" => Comparator::LessEqual(version),
        "=" | "" => Comparator::Exact(version),
        _ => return Err(format!("未知版本运算符 `{operator}`")),
    }])
}

fn wildcard_bounds(components: Vec<u64>) -> Vec<Comparator> {
    let lower = version_from_components(&components, None, None);
    let upper = if components.len() == 1 {
        Version::new(components[0] + 1, 0, 0)
    } else {
        Version::new(components[0], components[1] + 1, 0)
    };
    vec![Comparator::GreaterEqual(lower), Comparator::Less(upper)]
}

fn parse_partial_version(
    input: &str,
) -> Result<(Vec<u64>, Option<&str>, Option<&str>, bool), String> {
    let (without_build, build) = input
        .split_once('+')
        .map_or((input, None), |(value, build)| (value, Some(build)));
    let (without_pre, pre) = without_build
        .split_once('-')
        .map_or((without_build, None), |(value, pre)| (value, Some(pre)));
    let raw_components = without_pre.split('.').collect::<Vec<_>>();
    let mut components = Vec::new();
    let mut wildcard = false;
    for (index, component) in raw_components.iter().enumerate() {
        if *component == "*" || component.eq_ignore_ascii_case("x") {
            if index + 1 != raw_components.len() {
                return Err(format!("非法版本约束 `{input}`"));
            }
            wildcard = true;
            break;
        }
        if component.is_empty() {
            return Err(format!("非法版本约束 `{input}`"));
        }
        components.push(parse_number(Some(component), "版本")?);
    }
    if components.len() > 3 || (pre.is_some() && components.len() != 3) {
        return Err(format!("非法版本约束 `{input}`"));
    }
    parse_identifiers(pre, true)?;
    parse_identifiers(build, false)?;
    Ok((components, pre, build, wildcard))
}

fn version_from_components(components: &[u64], pre: Option<&str>, build: Option<&str>) -> Version {
    let mut version = Version::new(
        components.first().copied().unwrap_or(0),
        components.get(1).copied().unwrap_or(0),
        components.get(2).copied().unwrap_or(0),
    );
    version.pre = parse_identifiers(pre, true).expect("partial version 已验证预发布标识");
    version.build = parse_identifiers(build, false).expect("partial version 已验证 build 标识");
    version
}

fn caret_bounds(
    components: Vec<u64>,
    pre: Option<&str>,
    build: Option<&str>,
) -> Result<Vec<Comparator>, String> {
    let lower = version_from_components(&components, pre, build);
    let upper = if lower.major != 0 {
        Version::new(lower.major + 1, 0, 0)
    } else if lower.minor != 0 {
        Version::new(0, lower.minor + 1, 0)
    } else if components.len() >= 3 {
        Version::new(0, 0, lower.patch + 1)
    } else if components.len() == 1 {
        Version::new(1, 0, 0)
    } else {
        Version::new(0, lower.minor + 1, 0)
    };
    Ok(vec![
        Comparator::GreaterEqual(lower),
        Comparator::Less(upper),
    ])
}

fn tilde_bounds(
    components: Vec<u64>,
    pre: Option<&str>,
    build: Option<&str>,
) -> Result<Vec<Comparator>, String> {
    let lower = version_from_components(&components, pre, build);
    let upper = if components.len() <= 1 {
        Version::new(lower.major + 1, 0, 0)
    } else {
        Version::new(lower.major, lower.minor + 1, 0)
    };
    Ok(vec![
        Comparator::GreaterEqual(lower),
        Comparator::Less(upper),
    ])
}
