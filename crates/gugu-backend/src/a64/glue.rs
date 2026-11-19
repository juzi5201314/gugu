//! Explicit bindings to the independently emitted A64 runtime fragments.
use super::Error;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeBindings {
    pub morestack_or_poll: String,
    pub safepoint_slow: String,
    pub foreign: Option<ForeignBindings>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ForeignBindings {
    pub ordinary_dispatch: String,
    pub dirty_dispatch: String,
}

impl RuntimeBindings {
    pub(crate) fn validate(&self) -> Result<(), Error> {
        if self.morestack_or_poll.is_empty()
            || self.safepoint_slow.is_empty()
            || self.morestack_or_poll == self.safepoint_slow
        {
            return Err(super::error(
                "runtime fragment bindings must be distinct nonempty symbols",
            ));
        }
        if let Some(foreign) = &self.foreign {
            let symbols = [
                &self.morestack_or_poll,
                &self.safepoint_slow,
                &foreign.ordinary_dispatch,
                &foreign.dirty_dispatch,
            ];
            for (index, symbol) in symbols.iter().enumerate() {
                if symbol.is_empty() || symbols[..index].contains(symbol) {
                    return Err(super::error(
                        "foreign bindings must be distinct nonempty runtime symbols",
                    ));
                }
            }
        }
        Ok(())
    }
}
