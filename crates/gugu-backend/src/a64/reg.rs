//! 编码号 31 的含义由操作数槽决定，不能把 SP 当成零寄存器。

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Width {
    B,
    H,
    W,
    X,
    S,
    D,
    Q,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RegKind {
    Gpr(u8),
    Sp,
    Zero,
    Vector(u8),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Reg {
    pub kind: RegKind,
    pub width: Width,
}

impl Reg {
    pub const fn x(number: u8) -> Self {
        Self {
            kind: RegKind::Gpr(number),
            width: Width::X,
        }
    }
    pub const fn w(number: u8) -> Self {
        Self {
            kind: RegKind::Gpr(number),
            width: Width::W,
        }
    }
    pub const fn v(number: u8, width: Width) -> Self {
        Self {
            kind: RegKind::Vector(number),
            width,
        }
    }
    pub const fn sp(width: Width) -> Self {
        Self {
            kind: RegKind::Sp,
            width,
        }
    }
    pub const fn zr(width: Width) -> Self {
        Self {
            kind: RegKind::Zero,
            width,
        }
    }
}

impl std::fmt::Display for Reg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let prefix = match self.width {
            Width::B => "b",
            Width::H => "h",
            Width::W => "w",
            Width::X => "x",
            Width::S => "s",
            Width::D => "d",
            Width::Q => "q",
        };
        match self.kind {
            RegKind::Gpr(n) | RegKind::Vector(n) => write!(f, "{prefix}{n}"),
            RegKind::Sp => f.write_str(if self.width == Width::W { "wsp" } else { "sp" }),
            RegKind::Zero => write!(f, "{prefix}zr"),
        }
    }
}
