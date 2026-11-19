//! Private A64 compilation and image-world entry points; no target registration.
pub use super::world::{BuildRequest, World, build};
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
    pub metadata: Option<super::metadata::Metadata>,
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
    compile(body, registers, false, None, None)
}

pub(crate) fn compile_with_registers(
    body: &Body,
    registers: &impl RegisterFile,
) -> Result<LeafFragment, Error> {
    compile(body, registers, true, None, None)
}

/// Checked entries, scalar roots, polls and mapped internal direct calls.
pub fn compile_runtime(
    body: &Body,
    bindings: &super::glue::RuntimeBindings,
) -> Result<LeafFragment, Error> {
    bindings.validate()?;
    compile(body, &abi::A64Registers, false, Some(bindings), None)
}

pub(crate) fn compile_image(
    body: &Body,
    bindings: &super::glue::RuntimeBindings,
    symbols: &[(crate::lir::body::Symbol, String)],
) -> Result<LeafFragment, Error> {
    bindings.validate()?;
    compile(
        body,
        &abi::A64Registers,
        false,
        Some(bindings),
        Some(symbols),
    )
}

fn compile(
    body: &Body,
    registers: &impl RegisterFile,
    leaf: bool,
    bindings: Option<&super::glue::RuntimeBindings>,
    symbols: Option<&[(crate::lir::body::Symbol, String)]>,
) -> Result<LeafFragment, Error> {
    let compile = || {
        for reg in registers.pool(abi::GPR, false) {
            if !abi::POOL.contains(reg) {
                return Err(super::error(
                    "register file exposes a reserved A64 register",
                ));
            }
        }
        let selected = if let Some(symbols) = symbols {
            super::select::select_image(body, symbols)?
        } else if bindings.is_some() {
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
        let logical = if bindings.is_some() {
            super::metadata::logical(body)?
        } else {
            vec![]
        };
        let allocation = if bindings.is_some() {
            alloc::allocate_roots(&selected, registers, Some(body))?
        } else {
            alloc::allocate(&selected, registers)?
        };
        let mut frame = frame::layout(body, &allocation)?;
        if bindings.is_some() {
            frame::runtime_policy(body, &mut frame)?;
        }
        let (rewritten, anchors) =
            super::rewrite::rewrite_runtime(&selected, &allocation, &frame, bindings)?;
        let (bytes, layout, relocations) = layout::assemble_function(&rewritten)?;
        let runtime_sites = super::sites::resolve(&anchors, &layout, &frame)?;
        let metadata = if bindings.is_some() {
            Some(super::metadata::build(
                body,
                &allocation,
                &frame,
                layout.size,
                &runtime_sites,
                &logical,
            )?)
        } else {
            None
        };
        Ok(LeafFragment {
            symbol: abi::symbol(&body.instance),
            bytes,
            relocations,
            layout,
            frame,
            allocation,
            selected,
            runtime_sites,
            metadata,
        })
    };
    compile().map_err(|error: Error| Error(format!("A64 {}: {error}", body.name)))
}
