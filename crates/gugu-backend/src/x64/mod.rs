//! x86_64 指令表示、描述符表、编码器与 instruction verifier。
//!
//! 表是单一事实来源：编码器只写表内形式，verifier 按目标 `cpu_baseline` 拒绝超出
//! 可接受面的形式；lowering 与 codegen 在后续模块接入。

pub mod abi;
pub mod alloc;
pub mod codegen;
pub mod contract;
pub mod copies;
pub mod elf;
pub mod encode;
pub mod fragments;
pub(crate) mod fs_native;
pub mod glue;
pub mod inst;
pub mod installed;
pub mod layout;
pub mod lower;
pub mod mangle;
pub mod metadata;
pub mod oracle;
pub mod pe;
pub mod reg;
mod rows;
pub mod select;
#[cfg(test)]
mod select_tests;
pub mod table;
pub mod unwind;
pub mod verify;
