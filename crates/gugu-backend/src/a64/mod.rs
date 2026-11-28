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
mod foreign_leaf;
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
pub struct Error(pub String, pub Option<crate::hir::Location>, Option<String>);

impl Error {
    pub fn new(message: String) -> Self {
        Self(message, None, None)
    }

    pub(crate) fn undefined_symbol(name: &str) -> Self {
        Self(
            format!("undefined A64 symbol {name}"),
            None,
            Some(name.to_owned()),
        )
    }

    pub fn with_location(mut self, location: Option<&crate::hir::Location>) -> Self {
        if self.1.is_none() {
            self.1 = location.cloned();
        }
        self
    }

    pub fn context(mut self, context: impl std::fmt::Display) -> Self {
        self.0 = format!("{context}: {}", self.0);
        self
    }

    pub(crate) fn locate_symbol(
        self,
        bodies: &[crate::lir::body::Body],
        platform: foreign::Platform,
    ) -> Self {
        let location = self.2.as_deref().and_then(|name| {
            bodies
                .iter()
                .find_map(|body| world::symbol_location(body, name, platform))
        });
        self.with_location(location)
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

pub(crate) fn error(message: &str) -> Error {
    Error::new(message.to_owned())
}

pub(crate) fn body_location(body: &crate::lir::body::Body) -> Option<&crate::hir::Location> {
    body.source_scopes
        .first()
        .map(|scope| &scope.location)
        .or_else(|| {
            body.blocks
                .get(body.entry.index())
                .map(|block| &block.source.location)
        })
}
