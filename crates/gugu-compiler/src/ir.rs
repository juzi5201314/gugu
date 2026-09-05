use crate::frontend::FrontendOutput;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct FunctionId(u32);
impl FunctionId {
    pub(crate) fn index(self) -> usize {
        self.0 as usize
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum IrOperation {
    ReturnUnit,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct IrFunction {
    pub(crate) name: &'static str,
    pub(crate) operations: Vec<IrOperation>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct IrModule {
    pub(crate) functions: Vec<IrFunction>,
    pub(crate) entry: Option<FunctionId>,
    pub(crate) semantics: crate::frontend::CheckedSemantics,
}

pub(crate) fn lower(frontend: FrontendOutput) -> IrModule {
    if !frontend.has_main {
        return IrModule {
            semantics: frontend.semantics,
            ..IrModule::default()
        };
    }
    IrModule {
        functions: vec![IrFunction {
            name: "main",
            operations: vec![IrOperation::ReturnUnit],
        }],
        entry: Some(FunctionId(0)),
        semantics: frontend.semantics,
    }
}
