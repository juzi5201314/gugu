use crate::client::{Method, Request, Response, Transport};
use crate::error::{RegistryError, protocol};
use crate::identity::{is_loopback_host, normalize_index_url, origin};

/// 基于 ureq 的 HTTPS 传输：证书验证使用宿主信任库；手动跟随重定向，
/// 跨 origin 不转发 `Authorization`；GET 幂等重试外不做自动重发。
pub struct UreqTransport {
    agent: ureq::Agent,
}

impl UreqTransport {
    /// 默认传输：最多跟随 10 次重定向，4xx/5xx 交给协议层判定。
    pub fn new() -> Self {
        let config = ureq::Agent::config_builder()
            .max_redirects(0)
            .http_status_as_error(false)
            .build();
        Self {
            agent: ureq::Agent::new_with_config(config),
        }
    }
}

impl Default for UreqTransport {
    fn default() -> Self {
        Self::new()
    }
}

const MAX_REDIRECTS: usize = 10;

impl Transport for UreqTransport {
    fn request(&self, request: &Request) -> Result<Response, RegistryError> {
        let mut url = request.url.clone();
        let method = request.method;
        let mut body = request.body.clone();
        let request_origin = origin(&request.url).to_owned();
        for _ in 0..MAX_REDIRECTS {
            validate_url(&url)?;
            // 跨 origin 重定向不得转发 Authorization。
            let cross_origin = origin(&url) != request_origin;
            let response = match method {
                Method::Get => {
                    let mut call = self.agent.get(&url);
                    for (name, value) in &request.headers {
                        if cross_origin && name.eq_ignore_ascii_case("authorization") {
                            continue;
                        }
                        call = call.header(name.as_str(), value.as_str());
                    }
                    call.call()
                }
                Method::Post => {
                    let mut call = self.agent.post(&url);
                    for (name, value) in &request.headers {
                        if cross_origin && name.eq_ignore_ascii_case("authorization") {
                            continue;
                        }
                        call = call.header(name.as_str(), value.as_str());
                    }
                    match body.take() {
                        Some(bytes) => call.send(&*bytes),
                        None => call.send_empty(),
                    }
                }
            };
            let response = match response {
                Ok(response) => response,
                Err(ureq::Error::Io(error)) => {
                    return Err(protocol(format!("请求 `{url}` 传输失败：{error}")));
                }
                Err(error) => {
                    return Err(protocol(format!("请求 `{url}` 失败：{error}")));
                }
            };
            let status = response.status().as_u16();
            if !matches!(status, 301 | 302 | 303 | 307 | 308) {
                return read_response(response, request.max_body);
            }
            let location = response
                .headers()
                .get("location")
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned);
            let Some(location) = location else {
                return Err(protocol(format!("重定向响应缺少 Location：`{url}`")));
            };
            let next = join_redirect(&url, &location)?;
            if method == Method::Post && matches!(status, 301..=303) {
                return Err(protocol("POST 不能跟随会改变方法的重定向"));
            }
            if origin(&next) != origin(&url) {
                validate_url(&next)?;
            }
            url = next;
            body = request.body.clone();
        }
        Err(protocol(format!("`{url}` 重定向次数超限")))
    }
}

/// 只允许 HTTPS，或 loopback HTTP（测试配置）。
fn validate_url(url: &str) -> Result<(), RegistryError> {
    let rest = url
        .split_once("://")
        .ok_or_else(|| protocol(format!("URL `{url}` 缺少 scheme")))?;
    if rest.0 == "https" {
        return Ok(());
    }
    if rest.0 == "http" {
        let authority_end = rest.1.find('/').unwrap_or(rest.1.len());
        let authority = &rest.1[..authority_end];
        let host = authority.split('@').next_back().unwrap_or_default();
        let host = host.split(':').next().unwrap_or_default();
        if is_loopback_host(host) {
            return Ok(());
        }
    }
    Err(protocol(format!("URL `{url}` 必须使用 HTTPS")))
}

/// 解析 Location：绝对 URL 原样使用；`/` 开头拼到原 origin；相对路径
/// 拼到当前路径目录；其余非法。
fn join_redirect(base: &str, location: &str) -> Result<String, RegistryError> {
    if location.contains("://") {
        return normalize_index_url_like(location)
            .map_err(|_| protocol(format!("重定向目标 `{location}` 非法")));
    }
    if let Some(path) = location.strip_prefix('/') {
        return Ok(format!("{}/{path}", origin(base)));
    }
    if location.starts_with('?') || location.starts_with('#') || location.is_empty() {
        return Err(protocol(format!("重定向目标 `{location}` 非法")));
    }
    let path_start = base.find("://").map(|index| {
        base[index + 3..]
            .find('/')
            .map(|offset| index + 3 + offset)
            .unwrap_or(base.len())
    });
    let Some(start) = path_start else {
        return Err(protocol(format!("重定向目标 `{location}` 非法")));
    };
    // `start` 是当前 URL 路径的起始位置；目录为路径起点到最后一个 `/`。
    let path = &base[start..];
    let dir_end = start + path.rfind('/').map(|index| index + 1).unwrap_or(0);
    let joined = format!("{}{}", &base[..dir_end], location);
    normalize_index_url_like(&joined).map_err(|_| protocol(format!("重定向目标 `{location}` 非法")))
}

/// 重定向目标允许不带结尾 `/` 的任意路径——只做 scheme/host 归一化，
/// 不强制目录式路径。与 `normalize_index_url` 共享点段与用户信息规则。
fn normalize_index_url_like(url: &str) -> Result<String, RegistryError> {
    let mut normalized = normalize_index_url(url)?;
    if !url.ends_with('/') && normalized.ends_with('/') {
        normalized.pop();
    }
    Ok(normalized)
}

fn read_response(
    mut response: ureq::http::Response<ureq::Body>,
    max_body: usize,
) -> Result<Response, RegistryError> {
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .map(str::trim)
        .map(str::to_owned);
    let content_encoding = response
        .headers()
        .get("content-encoding")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let body = response
        .body_mut()
        .with_config()
        .limit(max_body as u64)
        .read_to_vec()
        .map_err(|error| protocol(format!("读取响应体失败或超出大小上限：{error}")))?;
    Ok(Response {
        status: response.status().as_u16(),
        content_type,
        content_encoding,
        body,
    })
}
