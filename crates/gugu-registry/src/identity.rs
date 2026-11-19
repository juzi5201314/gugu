use crate::error::{RegistryError, config};

/// 规范化 registry index URL：小写主机、移除默认端口、消除未编码点段、
/// 强制结尾 `/`；拒绝用户信息、查询、片段与 `%2e` 编码点段。
/// 非 loopback 必须 HTTPS；`http://` 只允许 loopback 测试地址。
pub fn normalize_index_url(raw: &str) -> Result<String, RegistryError> {
    let (scheme, rest) = raw
        .split_once("://")
        .ok_or_else(|| config(format!("registry index `{raw}` 缺少 scheme")))?;
    let (authority, path) = match rest.find('/') {
        Some(index) => (&rest[..index], &rest[index..]),
        None => (rest, "/"),
    };
    if authority.contains('@') {
        return Err(config(format!("registry index `{raw}` 不允许用户信息")));
    }
    if path.contains('?') || path.contains('#') {
        return Err(config(format!("registry index `{raw}` 不允许查询或片段")));
    }
    let scheme = scheme.to_ascii_lowercase();
    let (host, port) = split_host_port(authority, raw)?;
    let host = host.to_ascii_lowercase();
    let loopback = is_loopback_host(&host);
    let default_port = match scheme.as_str() {
        "https" => 443,
        "http" => 80,
        other => {
            return Err(config(format!(
                "registry index `{raw}` 使用不支持的 scheme `{other}`"
            )));
        }
    };
    if scheme == "http" && !loopback {
        return Err(config(format!(
            "registry index `{raw}` 必须使用 HTTPS（仅 loopback 允许 HTTP）"
        )));
    }
    if host.is_empty() || host.contains('%') {
        return Err(config(format!("registry index `{raw}` 主机非法")));
    }
    // IPv6 主机在 authority 中必须保留方括号。
    let host_out = if host.contains(':') {
        format!("[{host}]")
    } else {
        host.clone()
    };
    let authority = match port {
        Some(port) if port == default_port => host_out,
        Some(port) => format!("{host_out}:{port}"),
        None => host_out,
    };
    let path = normalize_path(path, raw)?;
    Ok(format!("{scheme}://{authority}{path}"))
}

fn split_host_port<'a>(
    authority: &'a str,
    raw: &str,
) -> Result<(&'a str, Option<u16>), RegistryError> {
    if let Some(rest) = authority.strip_prefix('[') {
        let Some(end) = rest.find(']') else {
            return Err(config(format!("registry index `{raw}` IPv6 主机不闭合")));
        };
        let host = &rest[..end];
        let suffix = &rest[end + 1..];
        let port = match suffix.strip_prefix(':') {
            Some(text) => Some(
                text.parse::<u16>()
                    .map_err(|_| config(format!("registry index `{raw}` 端口非法")))?,
            ),
            None if suffix.is_empty() => None,
            None => return Err(config(format!("registry index `{raw}` authority 非法"))),
        };
        return Ok((host, port));
    }
    match authority.rsplit_once(':') {
        Some((host, text)) if !text.contains(':') => {
            if text.is_empty() {
                return Ok((host, None));
            }
            let port = text
                .parse::<u16>()
                .map_err(|_| config(format!("registry index `{raw}` 端口非法")))?;
            Ok((host, Some(port)))
        }
        _ => Ok((authority, None)),
    }
}

fn normalize_path(path: &str, raw: &str) -> Result<String, RegistryError> {
    let mut segments = Vec::new();
    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                if segments.pop().is_none() {
                    return Err(config(format!("registry index `{raw}` 路径越出根")));
                }
            }
            other => {
                if other.eq_ignore_ascii_case("%2e") || other.eq_ignore_ascii_case("%2e%2e") {
                    return Err(config(format!("registry index `{raw}` 含编码点段")));
                }
                segments.push(other);
            }
        }
    }
    let mut normalized = String::from("/");
    normalized.push_str(&segments.join("/"));
    if !normalized.ends_with('/') {
        normalized.push('/');
    }
    Ok(normalized)
}

/// 主机是否为 loopback：完整的 127.x.x.x IPv4、`localhost` 或 `::1`。
/// 仅前缀为 `127.` 的域名（如 `127.attacker.example`）不算 loopback。
pub fn is_loopback_host(host: &str) -> bool {
    let host = host.trim_matches(['[', ']']);
    if host == "localhost" || host == "::1" {
        return true;
    }
    let octets: Vec<&str> = host.split('.').collect();
    octets.len() == 4
        && octets[0] == "127"
        && octets.iter().all(|octet| {
            !octet.is_empty()
                && octet.chars().all(|c| c.is_ascii_digit())
                && octet.parse::<u8>().is_ok()
        })
}

/// 规范 URL 的 `scheme://authority` 前缀，用于判断跨 origin 重定向。
pub fn origin(url: &str) -> &str {
    match url.split_once("://") {
        Some((scheme, rest)) => {
            let end = rest.find('/').unwrap_or(rest.len());
            &url[..scheme.len() + 3 + end]
        }
        None => url,
    }
}

/// 构造 index 下的 package 路径：`<index>/<owner>/<name>`。
/// `owner`/`name` 已由清单规则限制为 `[a-z0-9-]+`，不会引入额外段。
pub fn index_package_url(identity: &str, name: &str) -> String {
    format!(
        "{}{}",
        identity.trim_end_matches('/'),
        format_args!("/{name}")
    )
}

/// 替换 download 模板中的 `{owner}`、`{name}`、`{version}`。
/// 各段按清单/SemVer 规则限制，不含需要转义的字符；仍拒绝 `/?#%`。
pub fn render_download_template(
    template: &str,
    name: &str,
    version: &str,
) -> Result<String, RegistryError> {
    let (owner, short) = name.split_once('/').unwrap_or(("", name));
    for segment in [owner, short, version] {
        if segment.is_empty()
            || segment
                .chars()
                .any(|c| matches!(c, '/' | '?' | '#' | '%' | '\\' | '\0'))
        {
            return Err(config(format!(
                "download 模板参数 `{segment}` 不是安全路径段"
            )));
        }
    }
    Ok(template
        .replace("{owner}", owner)
        .replace("{name}", short)
        .replace("{version}", version))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_https_index_url() {
        assert_eq!(
            normalize_index_url("HTTPS://Packages.Example:443/index/../idx//").expect("normalize"),
            "https://packages.example/idx/"
        );
        assert_eq!(
            normalize_index_url("https://packages.example/index").expect("trailing slash"),
            "https://packages.example/index/"
        );
    }

    #[test]
    fn rejects_insecure_and_malformed_urls() {
        for raw in [
            "http://packages.example/index/",
            "https://user@packages.example/index/",
            "https://packages.example/index/?x=1",
            "https://packages.example/index/#f",
            "https://packages.example/%2e%2e/x/",
            "ftp://packages.example/index/",
            "https://packages.example/../../x/",
        ] {
            assert!(normalize_index_url(raw).is_err(), "{raw} should fail");
        }
    }

    #[test]
    fn allows_loopback_http() {
        assert_eq!(
            normalize_index_url("http://127.0.0.1:8999/index").expect("loopback"),
            "http://127.0.0.1:8999/index/"
        );
        assert_eq!(
            normalize_index_url("http://localhost:8999/i/").expect("localhost"),
            "http://localhost:8999/i/"
        );
        assert_eq!(
            normalize_index_url("http://[::1]:8999/index").expect("ipv6"),
            "http://[::1]:8999/index/"
        );
        // 域名以 `127.` 开头仍不是 loopback。
        assert!(normalize_index_url("http://127.attacker.example/index/").is_err());
        assert!(!is_loopback_host("127.attacker.example"));
    }

    #[test]
    fn renders_download_template_safely() {
        let url = render_download_template(
            "https://api.example/dl/{owner}/{name}/{version}",
            "acme/json",
            "1.2.3",
        )
        .expect("render");
        assert_eq!(url, "https://api.example/dl/acme/json/1.2.3");
        assert!(render_download_template("https://x/{version}", "a/b", "../x").is_err());
    }
}
