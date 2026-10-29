#![forbid(unsafe_code)]

//! Gugu 编译器最底层：源码快照与位置、诊断、版本化 query、规范编码、目标描述与调优 profile。
//!
//! 本 crate 不依赖任何编译阶段；上层 crate 通过模块路径直接使用这里的类型。

pub mod action;
pub mod canonical;
pub mod diagnostics;
pub mod hash;
pub mod query;
pub mod source;
pub mod target;
#[cfg(test)]
mod target_tests;
pub mod tuning;
