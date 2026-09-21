//! x86_64 指令表示、描述符表、编码器与 instruction verifier。
//!
//! 表是单一事实来源：编码器只写表内形式，verifier 按目标 `cpu_baseline` 拒绝超出
//! 可接受面的形式；lowering 与 codegen 在后续模块接入。

pub(crate) mod abi;
pub(crate) mod alloc;
#[cfg(test)]
mod alloc_tests;
pub(crate) mod codegen;
pub(crate) mod contract;
pub(crate) mod copies;
pub(crate) mod encode;
pub(crate) mod harness;
mod harness_cases;
pub(crate) mod inst;
pub(crate) mod layout;
pub(crate) mod lower;
pub(crate) mod mangle;
pub(crate) mod metadata;
pub(crate) mod metadata_section;
#[cfg(test)]
mod metadata_tests;
pub(crate) mod reg;
mod rows;
pub(crate) mod select;
#[cfg(test)]
mod select_tests;
pub(crate) mod table;
#[cfg(test)]
mod tests;
pub(crate) mod verify;
