//! ARMv8.0-A 编码库、独立运行时片段与内部整数叶 lowering；尚未接入目标。

pub mod abi;
pub mod alloc;
mod atomic;
mod bits;
pub mod codegen;
pub mod contract;
pub mod decode;
pub mod definitions;
pub mod elf;
pub mod encode;
pub mod foreign;
mod foreign_emit;
mod foreign_packet;
mod fp;
pub mod fragments;
pub mod frame;
pub mod glue;
pub mod image;
pub mod inst;
pub mod layout;
mod locals;
mod lower;
pub mod machine;
pub mod macho;
pub mod memory;
pub mod metadata;
pub mod oracle;
mod print;
pub mod program;
pub mod reg;
pub(crate) mod relocation;
pub mod rewrite;
mod rows;
pub mod runtime;
pub mod select;
pub mod sites;
pub mod table;
pub mod verify;
pub mod world;

#[cfg(test)]
mod tests;

#[cfg(test)]
mod leaf_tests;

#[cfg(test)]
mod program_tests;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Error(pub String);

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

pub(crate) fn error(message: &str) -> Error {
    Error(message.to_owned())
}
