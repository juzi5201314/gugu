use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub enum ArrayLength<P = String> {
    Known(u64),
    Param(P),
}

impl<P> ArrayLength<P> {
    pub fn known(&self) -> Option<u64> {
        match self {
            Self::Known(value) => Some(*value),
            Self::Param(_) => None,
        }
    }
}

impl<P: std::fmt::Display> std::fmt::Display for ArrayLength<P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Known(value) => value.fmt(f),
            Self::Param(name) => name.fmt(f),
        }
    }
}
