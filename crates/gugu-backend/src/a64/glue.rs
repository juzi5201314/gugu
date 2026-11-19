//! Explicit bindings to the independently emitted A64 runtime fragments.
use super::Error;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeBindings {
    pub morestack_or_poll: String,
    pub safepoint_slow: String,
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
        Ok(())
    }
}
