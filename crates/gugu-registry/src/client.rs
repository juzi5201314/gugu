use crate::error::{RegistryError, auth, config, not_found, package_conflict, protocol};
use crate::identity::{index_package_url, normalize_index_url, render_download_template};
use crate::protocol::{ConfigDocument, IndexRecord, parse_config_document, parse_index};

/// HTTP 方法。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Method {
    Get,
    Post,
}

/// 协议层请求；`max_body` 限制响应体字节数。
#[derive(Clone, Debug)]
pub struct Request {
    pub method: Method,
    pub url: String,
    /// 请求头；token 仅以 `Authorization` 头传递。
    pub headers: Vec<(String, String)>,
    pub body: Option<Vec<u8>>,
    /// 响应体大小上限（字节）。
    pub max_body: usize,
}

/// 协议层响应。
#[derive(Clone, Debug)]
pub struct Response {
    pub status: u16,
    /// `Content-Type` 主类型（不带参数）。
    pub content_type: Option<String>,
    /// `Content-Encoding`，不支持时必须为空。
    pub content_encoding: Option<String>,
    pub body: Vec<u8>,
}

/// 传输抽象：协议测试使用确定性替身；生产实现见 `http`。
pub trait Transport {
    fn request(&self, request: &Request) -> Result<Response, RegistryError>;
}

/// 单个响应体上限：config/index 为 MiB 级，归档为 MiB 级。
pub const MAX_DOCUMENT_BODY: usize = 16 * 1024 * 1024;
pub const MAX_ARCHIVE_BODY: usize = 256 * 1024 * 1024;

/// Protocol v1 客户端：identity 是规范化 index URL。
pub struct Client<'a, T: Transport> {
    transport: &'a T,
    identity: String,
    /// config/index 的网络位置（镜像时与 identity 不同）。
    fetch_base: String,
    /// 下载请求的 origin 覆盖（镜像）。
    download_origin: Option<String>,
    config: ConfigDocument,
    token: Option<String>,
}

/// `gugu publish` 的结果。
#[derive(Clone, Debug)]
pub struct PublishOutcome {
    pub name: String,
    pub vers: String,
    pub cksum: String,
    /// 相同 package ID + checksum 的幂等重试。
    pub idempotent: bool,
}

/// `gugu yank` 的结果。
#[derive(Clone, Debug)]
pub struct YankOutcome {
    pub name: String,
    pub vers: String,
    pub yanked: bool,
}

impl<'a, T: Transport> Client<'a, T> {
    /// 抓取并验证 `config.json`；服务端身份必须与 `index_url` 规范化结果一致。
    pub fn connect(
        transport: &'a T,
        index_url: &str,
        token: Option<String>,
    ) -> Result<Self, RegistryError> {
        let identity = normalize_index_url(index_url)?;
        Self::connect_base(transport, &identity, &identity, None, token)
    }

    /// 镜像连接：`fetch_base` 是镜像的网络位置，config.json 中的 `index`
    /// 仍必须等于 `expected_identity`；下载与索引走镜像，package ID 不变。
    pub fn connect_mirror(
        transport: &'a T,
        mirror_url: &str,
        expected_identity: &str,
        token: Option<String>,
    ) -> Result<Self, RegistryError> {
        let fetch_base = normalize_index_url(mirror_url)?;
        let identity = normalize_index_url(expected_identity)?;
        let download_origin = crate::identity::origin(&fetch_base).to_owned();
        Self::connect_base(
            transport,
            &fetch_base,
            &identity,
            Some(download_origin),
            token,
        )
    }

    fn connect_base(
        transport: &'a T,
        fetch_base: &str,
        identity: &str,
        download_origin: Option<String>,
        token: Option<String>,
    ) -> Result<Self, RegistryError> {
        let config_url = format!("{fetch_base}config.json");
        let response = transport.request(&Request {
            method: Method::Get,
            url: config_url,
            headers: auth_headers(token.as_deref()),
            body: None,
            max_body: MAX_DOCUMENT_BODY,
        })?;
        check_status(&response, "config.json")?;
        require_content_type(&response, "application/json")?;
        let text = String::from_utf8(response.body.clone())
            .map_err(|_| config("config.json 不是 UTF-8"))?;
        let config = parse_config_document(&text, identity)?;
        Ok(Self {
            transport,
            identity: identity.to_owned(),
            fetch_base: fetch_base.to_owned(),
            download_origin,
            config,
            token,
        })
    }

    /// 规范化 index 身份。
    pub fn identity(&self) -> &str {
        &self.identity
    }

    /// registry 是否要求认证。
    pub fn auth_required(&self) -> bool {
        self.config.auth_required
    }

    /// API 根（发布/yank）。
    pub fn api(&self) -> Option<&str> {
        self.config.api.as_deref()
    }

    /// 稀疏索引：`GET <index>/<owner>/<name>` → 全部版本记录。
    /// 404/410 → `registry-not-found`；401/403 → `registry-auth`。
    pub fn index(&self, name: &str) -> Result<Vec<IndexRecord>, RegistryError> {
        let url = index_package_url(&self.fetch_base, name);
        let response = self.transport.request(&Request {
            method: Method::Get,
            url: url.clone(),
            headers: auth_headers(self.token.as_deref()),
            body: None,
            max_body: MAX_DOCUMENT_BODY,
        })?;
        match response.status {
            200 => {}
            404 | 410 => {
                return Err(not_found(format!("registry 没有 package `{name}`")));
            }
            401 | 403 => {
                return Err(auth(format!("registry 拒绝访问 `{name}` 的索引")));
            }
            status => {
                return Err(protocol(format!("索引请求 `{url}` 返回 HTTP {status}")));
            }
        }
        let content_type = response.content_type.as_deref().unwrap_or_default();
        if !matches!(
            content_type,
            "application/x-ndjson" | "application/json" | "text/plain"
        ) {
            return Err(protocol(format!(
                "索引响应媒体类型 `{content_type}` 不受支持"
            )));
        }
        let text = String::from_utf8(response.body.clone())
            .map_err(|_| protocol(format!("`{name}` 的索引不是 UTF-8")))?;
        parse_index(&text, name)
    }

    /// 按 download 模板下载归档字节；checksum 由调用方验证。
    pub fn download(&self, record: &IndexRecord) -> Result<Vec<u8>, RegistryError> {
        let mut url = render_download_template(&self.config.download, &record.name, &record.vers)?;
        if let Some(mirror) = &self.download_origin {
            url = format!("{mirror}{}", &url[crate::identity::origin(&url).len()..]);
        }
        // 跨 origin 的下载目标（如 CDN）不携带 registry 凭据。
        let headers = if crate::identity::origin(&url) == crate::identity::origin(&self.fetch_base)
        {
            auth_headers(self.token.as_deref())
        } else {
            Vec::new()
        };
        let response = self.transport.request(&Request {
            method: Method::Get,
            url: url.clone(),
            headers,
            body: None,
            max_body: MAX_ARCHIVE_BODY,
        })?;
        match response.status {
            200 => {}
            404 | 410 => {
                return Err(not_found(format!(
                    "registry 没有归档 `{}/{}`",
                    record.name, record.vers
                )));
            }
            401 | 403 => {
                return Err(auth(format!(
                    "registry 拒绝下载 `{}/{}`",
                    record.name, record.vers
                )));
            }
            status => {
                return Err(protocol(format!("下载 `{url}` 返回 HTTP {status}")));
            }
        }
        if let Some(encoding) = response.content_encoding.as_deref()
            && !encoding.is_empty()
        {
            return Err(protocol(format!(
                "下载响应声明不支持的内容编码 `{encoding}`"
            )));
        }
        Ok(response.body)
    }

    /// 上传归档：`POST <api>/publish`。相同 ID + checksum → 幂等成功。
    /// `signature` 为可选的发布者签名记录 JSON（`gugu publish --sign`）。
    pub fn publish(
        &self,
        archive: &[u8],
        checksum: &str,
        token: &str,
        signature: Option<&str>,
    ) -> Result<PublishOutcome, RegistryError> {
        let api = self
            .config
            .api
            .as_deref()
            .ok_or_else(|| config("registry 未声明 publish API"))?;
        let mut headers = auth_headers(Some(token));
        headers.push(("Content-Type".to_owned(), "application/gzip".to_owned()));
        headers.push(("X-Gugu-Package-SHA256".to_owned(), checksum.to_owned()));
        if let Some(signature) = signature {
            headers.push(("X-Gugu-Package-Signature".to_owned(), signature.to_owned()));
        }
        let response = self.transport.request(&Request {
            method: Method::Post,
            url: format!("{api}/publish"),
            headers,
            body: Some(archive.to_vec()),
            max_body: MAX_DOCUMENT_BODY,
        })?;
        match response.status {
            200 | 201 => {}
            401 | 403 => return Err(auth("registry 拒绝发布：认证或权限不足")),
            409 => {
                return Err(package_conflict(
                    "已存在 package ID 的 checksum 与待发布归档不同",
                ));
            }
            413 => return Err(protocol("发布归档超出 registry 大小限制")),
            422 => {
                let detail = String::from_utf8_lossy(&response.body);
                return Err(crate::error::package_invalid(format!(
                    "registry 拒绝归档：{detail}"
                )));
            }
            status => {
                return Err(protocol(format!("发布请求返回 HTTP {status}")));
            }
        }
        #[derive(serde::Deserialize)]
        struct PublishResponse {
            name: String,
            vers: String,
            cksum: String,
        }
        let parsed: PublishResponse = serde_json::from_slice(&response.body)
            .map_err(|error| protocol(format!("发布响应不是合法 JSON：{error}")))?;
        Ok(PublishOutcome {
            name: parsed.name,
            vers: parsed.vers,
            cksum: parsed.cksum,
            idempotent: response.status == 200,
        })
    }

    /// `POST <api>/yank`：设置或恢复版本的 yanked 状态。
    pub fn yank(
        &self,
        name: &str,
        vers: &str,
        yanked: bool,
        token: &str,
    ) -> Result<YankOutcome, RegistryError> {
        let api = self
            .config
            .api
            .as_deref()
            .ok_or_else(|| config("registry 未声明 yank API"))?;
        let mut headers = auth_headers(Some(token));
        headers.push(("Content-Type".to_owned(), "application/json".to_owned()));
        let body = serde_json::json!({
            "name": name,
            "vers": vers,
            "yanked": yanked,
        });
        let response = self.transport.request(&Request {
            method: Method::Post,
            url: format!("{api}/yank"),
            headers,
            body: Some(body.to_string().into_bytes()),
            max_body: MAX_DOCUMENT_BODY,
        })?;
        match response.status {
            200 => {}
            401 | 403 => return Err(auth("registry 拒绝 yank：认证或权限不足")),
            404 => {
                return Err(not_found(format!("registry 没有 `{name}@{vers}`")));
            }
            status => return Err(protocol(format!("yank 请求返回 HTTP {status}"))),
        }
        Ok(YankOutcome {
            name: name.to_owned(),
            vers: vers.to_owned(),
            yanked,
        })
    }
}

fn auth_headers(token: Option<&str>) -> Vec<(String, String)> {
    token
        .map(|token| ("Authorization".to_owned(), format!("Bearer {token}")))
        .into_iter()
        .collect()
}

fn check_status(response: &Response, resource: &str) -> Result<(), RegistryError> {
    match response.status {
        200 => Ok(()),
        401 | 403 => Err(auth(format!("registry 拒绝访问 {resource}"))),
        404 | 410 => Err(not_found(format!("registry 没有 {resource}"))),
        status => Err(protocol(format!("请求 {resource} 返回 HTTP {status}"))),
    }
}

fn require_content_type(response: &Response, expected: &str) -> Result<(), RegistryError> {
    let content_type = response.content_type.as_deref().unwrap_or_default();
    if content_type != expected {
        return Err(protocol(format!(
            "响应媒体类型 `{content_type}` 不是 `{expected}`"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    /// 确定性传输替身：按队列返回预录响应。
    struct Fake {
        requests: Mutex<Vec<Request>>,
        responses: Mutex<VecDeque<Response>>,
    }

    impl Fake {
        fn new(responses: Vec<Response>) -> Self {
            Self {
                requests: Mutex::new(Vec::new()),
                responses: Mutex::new(responses.into()),
            }
        }
    }

    fn ok(body: &str, content_type: &str) -> Response {
        Response {
            status: 200,
            content_type: Some(content_type.to_owned()),
            content_encoding: None,
            body: body.as_bytes().to_vec(),
        }
    }

    impl Transport for Fake {
        fn request(&self, request: &Request) -> Result<Response, RegistryError> {
            self.requests.lock().unwrap().push(Request {
                method: request.method,
                url: request.url.clone(),
                headers: request.headers.clone(),
                body: request.body.clone(),
                max_body: request.max_body,
            });
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| protocol("预录响应耗尽"))
        }
    }

    const IDENTITY: &str = "https://packages.example/index/";
    const CKSUM: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn config_body() -> Response {
        ok(
            r#"{"protocol":1,"index":"https://packages.example/index/","api":"https://packages.example/api/v1","download":"https://packages.example/dl/{owner}/{name}/{version}","auth_required":false}"#,
            "application/json",
        )
    }

    #[test]
    fn connect_then_index_maps_status_codes() {
        let record = format!(
            r#"{{"schema":1,"name":"pub/foo","vers":"1.0.0","cksum":"{CKSUM}","deps":[],"features":{{}}}}"#
        );
        let fake = Fake::new(vec![
            config_body(),
            ok(&format!("{record}\n"), "application/x-ndjson"),
            Response {
                status: 404,
                content_type: None,
                content_encoding: None,
                body: Vec::new(),
            },
        ]);
        let client = Client::connect(&fake, IDENTITY, None).unwrap();
        assert_eq!(client.identity(), IDENTITY);
        let records = client.index("pub/foo").unwrap();
        assert_eq!(records.len(), 1);
        let err = client.index("pub/missing").unwrap_err();
        assert_eq!(err.code(), "registry-not-found");
        let requests = fake.requests.lock().unwrap();
        assert!(requests[0].url.ends_with("config.json"));
        assert!(requests[1].url.ends_with("index/pub/foo"));
    }

    #[test]
    fn connect_rejects_server_identity_mismatch() {
        let body = r#"{"protocol":1,"index":"https://other.example/index/","api":"https://other.example/api","download":"https://other.example/dl/{owner}/{name}/{version}","auth_required":false}"#;
        let fake = Fake::new(vec![ok(body, "application/json")]);
        let err = Client::connect(&fake, IDENTITY, None)
            .err()
            .expect("connect 应失败");
        assert_eq!(err.code(), "registry-config");
    }

    #[test]
    fn publish_maps_created_idempotent_and_conflict() {
        let outcome = |name: &str, status: u16| {
            let mut response = ok(
                &format!(r#"{{"name":"{name}","vers":"1.0.0","cksum":"{CKSUM}"}}"#),
                "application/json",
            );
            response.status = status;
            response
        };
        let conflict = Response {
            status: 409,
            content_type: None,
            content_encoding: None,
            body: Vec::new(),
        };
        let fake = Fake::new(vec![
            config_body(),
            outcome("pub/foo", 201),
            outcome("pub/foo", 200),
            conflict,
        ]);
        let client = Client::connect(&fake, IDENTITY, Some("tok".to_owned())).unwrap();
        let first = client.publish(b"archive", CKSUM, "tok", None).unwrap();
        assert!(!first.idempotent);
        // 相同 ID + checksum 重试 → 200 幂等成功。
        let retry = client.publish(b"archive", CKSUM, "tok", None).unwrap();
        assert!(retry.idempotent);
        let err = client.publish(b"other", CKSUM, "tok", None).unwrap_err();
        assert_eq!(err.code(), "package-conflict");
        let requests = fake.requests.lock().unwrap();
        assert!(
            requests
                .iter()
                .any(|r| r.headers.iter().any(|(k, _)| k == "Authorization"))
        );
        assert!(requests.iter().any(|r| {
            r.headers
                .iter()
                .any(|(k, v)| k == "X-Gugu-Package-SHA256" && v == CKSUM)
        }));
    }

    #[test]
    fn publish_201_then_200_marks_idempotent() {
        let mut first = ok(
            r#"{"name":"pub/foo","vers":"1.0.0","cksum":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}"#,
            "application/json",
        );
        first.status = 201;
        let fake = Fake::new(vec![config_body(), first]);
        let client = Client::connect(&fake, IDENTITY, None).unwrap();
        let outcome = client.publish(b"a", CKSUM, "t", None).unwrap();
        assert!(!outcome.idempotent);
    }

    #[test]
    fn yank_posts_state_and_returns_outcome() {
        let fake = Fake::new(vec![
            config_body(),
            ok(
                r#"{"name":"pub/foo","vers":"1.0.0","yanked":true}"#,
                "application/json",
            ),
        ]);
        let client = Client::connect(&fake, IDENTITY, None).unwrap();
        let outcome = client.yank("pub/foo", "1.0.0", true, "tok").unwrap();
        assert!(outcome.yanked);
        let requests = fake.requests.lock().unwrap();
        assert!(requests[1].url.ends_with("/yank"));
    }
}
