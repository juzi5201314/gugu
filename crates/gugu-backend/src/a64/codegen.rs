//! Internal isolated entry points. No world/image/TargetName integration.
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
    pub symbol: String,
    pub bytes: Vec<u8>,
    pub relocations: Vec<layout::Relocation>,
    pub layout: layout::Plan,
    pub frame: Frame,
    pub allocation: Allocation,
    pub selected: Function,
    pub runtime_sites: Vec<super::sites::RuntimeSite>,
}

pub fn compile_leaf(body: &Body) -> Result<LeafFragment, Error> {
    compile_with_registers(body, &abi::A64Registers)
}

/// The caller must reserve entry stack capacity; checked entries remain unsupported.
pub fn compile_function(body: &Body) -> Result<LeafFragment, Error> {
    compile_function_with_registers(body, &abi::A64Registers)
}

pub(crate) fn compile_function_with_registers(
    body: &Body,
    registers: &impl RegisterFile,
) -> Result<LeafFragment, Error> {
    compile(body, registers, false, None)
}

pub(crate) fn compile_with_registers(
    body: &Body,
    registers: &impl RegisterFile,
) -> Result<LeafFragment, Error> {
    compile(body, registers, true, None)
}

/// Root-free checked entries and polls; serialized GC/unwind integration is 4c.2.
pub fn compile_runtime(
    body: &Body,
    bindings: &super::glue::RuntimeBindings,
) -> Result<LeafFragment, Error> {
    bindings.validate()?;
    compile(body, &abi::A64Registers, false, Some(bindings))
}

fn compile(
    body: &Body,
    registers: &impl RegisterFile,
    leaf: bool,
    bindings: Option<&super::glue::RuntimeBindings>,
) -> Result<LeafFragment, Error> {
    let compile = || {
        for reg in registers.pool(abi::GPR, false) {
            if !abi::POOL.contains(reg) {
                return Err(super::error(
                    "register file exposes a reserved A64 register",
                ));
            }
        }
        let selected = if bindings.is_some() {
            super::select::select_runtime(body)?
        } else if leaf {
            super::select::select(body)?
        } else {
            super::select::select_function(body)?
        };
        if selected
            .blocks
            .iter()
            .flat_map(|block| &block.items)
            .any(|site| site.item.is_call())
            && registers
                .pool(abi::GPR, true)
                .iter()
                .any(|reg| !abi::CALLEE_SAVED.contains(&reg.0))
        {
            return Err(super::error(
                "cross-call register pool must contain only x19-x26",
            ));
        }
        let allocation = alloc::allocate(&selected, registers)?;
        let frame = frame::layout(body, &allocation)?;
        let (rewritten, anchors) =
            super::rewrite::rewrite_runtime(&selected, &allocation, &frame, bindings)?;
        let (bytes, layout, relocations) = layout::assemble_function(&rewritten)?;
        let runtime_sites = super::sites::resolve(&anchors, &layout, &frame)?;
        Ok(LeafFragment {
            symbol: abi::symbol(&body.instance),
            bytes,
            relocations,
            layout,
            frame,
            allocation,
            selected,
            runtime_sites,
        })
    };
    compile().map_err(|error: Error| Error(format!("A64 {}: {error}", body.name)))
}
