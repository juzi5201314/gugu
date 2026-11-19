//! Architecture-neutral allocation over normalized machine events.

pub mod live;
pub mod parallel;
pub mod scan;
pub mod slots;

#[cfg(test)]
mod tests;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Class(pub u8);

/// Architecture-local identity, independent of serialized register codes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PhysReg(pub u8);

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RegSet(pub u64);

impl RegSet {
    pub fn single(reg: PhysReg) -> Result<Self, Error> {
        1_u64
            .checked_shl(u32::from(reg.0))
            .map(Self)
            .ok_or_else(|| Error::new("register identity exceeds mask capacity"))
    }

    pub fn contains(self, reg: PhysReg) -> bool {
        Self::single(reg).is_ok_and(|bit| self.0 & bit.0 != 0)
    }

    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum RootClass {
    NonPointer = 0,
    HeapPointer = 1,
    StackPointer = 2,
}

impl RootClass {
    pub const fn code(self) -> u8 {
        self as u8
    }
    pub const fn must_spill(self) -> bool {
        !matches!(self, Self::NonPointer)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Location {
    Reg(PhysReg),
    Slot(u32),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Placement {
    Location(Location),
    Rematerialize,
    Dead,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Hint {
    Register(PhysReg),
    SameAs(u32),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Value {
    pub class: Class,
    pub root: RootClass,
    pub allowed: RegSet,
    pub rematerializable: bool,
    pub range: (u32, u32),
    pub forced: bool,
    pub weight: u64,
    pub hints: Vec<Hint>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Point {
    pub use_slot: u32,
    pub def_slot: u32,
    pub mask: RegSet,
    pub pointer_spill: bool,
    pub call: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LiveInfo {
    pub values: Vec<Value>,
    pub points: Vec<Point>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ValueAllocation {
    pub value: u32,
    pub class: Class,
    pub root: RootClass,
    pub range: (u32, u32),
    pub placement: Placement,
}

impl ValueAllocation {
    pub const fn location(&self) -> Option<Location> {
        match self.placement {
            Placement::Location(location) => Some(location),
            Placement::Rematerialize | Placement::Dead => None,
        }
    }
}

pub trait RegisterFile {
    fn class(&self, reg: PhysReg) -> Class;
    fn pool(&self, class: Class, crosses_call: bool) -> &[PhysReg];
    fn spill_layout(&self, class: Class) -> (u32, u32);
    fn copy_scratch(&self, class: Class) -> Option<PhysReg>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Error(String);

impl Error {
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}
