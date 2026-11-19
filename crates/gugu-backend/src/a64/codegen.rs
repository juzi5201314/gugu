//! Internal leaf-only entry point. No world/image/TargetName integration.
use super::{
    Error, abi,
    alloc::{self, Allocation},
    frame::{self, Frame},
    layout,
    machine::Function,
};
use crate::{lir::body::Body, regalloc::RegisterFile};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LeafFragment {
    pub bytes: Vec<u8>,
    pub layout: layout::Plan,
    pub frame: Frame,
    pub allocation: Allocation,
    pub selected: Function,
}

pub fn compile_leaf(body: &Body) -> Result<LeafFragment, Error> {
    compile_with_registers(body, &abi::A64Registers)
}

pub(crate) fn compile_with_registers(
    body: &Body,
    registers: &impl RegisterFile,
) -> Result<LeafFragment, Error> {
    let compile = || {
        for reg in registers.pool(abi::GPR, false) {
            if !abi::POOL.contains(reg) {
                return Err(super::error(
                    "register file exposes a reserved A64 register",
                ));
            }
        }
        let selected = super::select::select(body)?;
        let allocation = alloc::allocate(&selected, registers)?;
        let frame = frame::layout(body, &allocation)?;
        let rewritten = super::rewrite::rewrite(&selected, &allocation, &frame)?;
        let (bytes, layout) = layout::assemble(&rewritten)?;
        Ok(LeafFragment {
            bytes,
            layout,
            frame,
            allocation,
            selected,
        })
    };
    compile().map_err(|error: Error| Error(format!("A64 leaf {}: {error}", body.name)))
}
