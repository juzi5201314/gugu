//! C NUL 字符串的唯一字节规则。
//!
//! 字面量、`CString.from_*` 与 `CStr.from_ptr` 都走这里：内部 NUL 被拒绝，终止 NUL
//! 只追加一次，视图不包含终止字节。

/// 构造或扫描 C 字符串失败。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CStringError {
    /// 负载里已经有 NUL。
    InteriorNul,
    /// 窗口为空、指针为空，或窗口内没有终止 NUL。
    MissingNul,
}

impl CStringError {
    pub(crate) const fn message(self) -> &'static str {
        match self {
            Self::InteriorNul => "CString 拒绝内部 NUL",
            Self::MissingNul => "CStr 缺少终止 NUL",
        }
    }
}

/// 拒绝内部 NUL，并在末尾追加恰好一个终止字节。
pub(crate) fn terminate(bytes: &[u8]) -> Result<Vec<u8>, CStringError> {
    if bytes.contains(&0) {
        return Err(CStringError::InteriorNul);
    }
    let mut owned = bytes.to_vec();
    owned.push(0);
    if scan(&owned)? != bytes {
        return Err(CStringError::InteriorNul);
    }
    Ok(owned)
}

/// 要求字节已经以单个终止 NUL 结束，且终止符之前没有 NUL。
pub(crate) fn require_terminated(bytes: &[u8]) -> Result<(), CStringError> {
    payload(bytes).map(|_| ())
}

/// 返回不含终止 NUL 的负载。
pub(crate) fn payload(bytes: &[u8]) -> Result<&[u8], CStringError> {
    let viewed = scan(bytes)?;
    if bytes.len() != viewed.len() + 1 {
        return Err(CStringError::InteriorNul);
    }
    Ok(viewed)
}

/// 在调用方提供的可读窗口里扫描第一个 NUL。空窗口视为无效指针。
pub(crate) fn scan(window: &[u8]) -> Result<&[u8], CStringError> {
    let Some(end) = window.iter().position(|byte| *byte == 0) else {
        return Err(CStringError::MissingNul);
    };
    Ok(&window[..end])
}
