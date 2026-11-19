//! ARMv8.0-A 编码库与独立运行时机器片段；尚未接入目标。

mod bits;
pub mod contract;
pub mod decode;
pub mod encode;
pub mod fragments;
pub mod inst;
mod print;
pub mod reg;
mod rows;
pub mod table;
pub mod verify;

#[cfg(test)]
mod tests;

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
