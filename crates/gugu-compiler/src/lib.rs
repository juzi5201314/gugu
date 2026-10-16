//! Gugu compiler 的 façade：前端加上 build-time 固化的 runtime artifact。
//!
//! `build.rs` 把 `gugu-runtime-builder` 的 source 表与 runtime 契约固化成 target 专属
//! `GUGURT01` 字节；本 crate 负责把它们解码成静态 artifact，并作为
//! [`gugu_frontend::RuntimeArtifactProvider`] 注入前端 session。前端不嵌入字节，也不从
//! 目标名推断字节来源，取不到即失败——没有源码回退。

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod artifact;

pub use artifact::{EMBEDDED_ARTIFACTS, EmbeddedRuntimeArtifacts, embedded_artifact};
pub use gugu_frontend::*;
