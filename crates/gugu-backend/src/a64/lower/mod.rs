//! Integer-only leaf instruction selection.
pub(super) mod call;
mod integer;
mod memory;
use super::{
    Error,
    machine::{self, Access, Function, Item, MInst, MOperand, MReg, Site},
    reg::Width,
};
use crate::lir::body::{Body, Instruction, Type, ValueId};

pub struct Builder<'a> {
    pub function: &'a mut Function,
    pub items: Vec<Site>,
    pub context: String,
}
impl Builder<'_> {
    pub fn push(&mut self, item: Item) {
        self.items.push(Site {
            context: self.context.clone(),
            item,
        });
    }
    pub fn emit(&mut self, key: &str, operands: Vec<MOperand>) -> Result<(), Error> {
        self.push(Item::Inst(MInst {
            form: machine::form(key)?,
            operands,
        }));
        Ok(())
    }
    pub fn temp(&mut self) -> Result<u32, Error> {
        self.function.temp(Type::I64)
    }
    pub fn constant(&mut self, bits: u64) -> Result<u32, Error> {
        let dest = self.temp()?;
        self.push(Item::Constant { dest, bits });
        Ok(dest)
    }
    pub fn normalize(
        &mut self,
        source: u32,
        dest: u32,
        bits: u32,
        signed: bool,
    ) -> Result<(), Error> {
        if bits == 64 {
            self.push(Item::Copies(vec![(
                MReg::Virtual(source),
                MReg::Virtual(dest),
            )]));
            Ok(())
        } else {
            self.emit(
                if signed { "sbfm.X" } else { "ubfm.X" },
                vec![
                    machine::write(dest),
                    machine::read(source),
                    machine::fixed(super::inst::Operand::Imm(0)),
                    machine::fixed(super::inst::Operand::Imm(i64::from(bits - 1))),
                ],
            )
        }
    }
}

pub fn bits(ty: Type) -> Result<u32, Error> {
    match ty {
        Type::I8 => Ok(8),
        Type::I16 => Ok(16),
        Type::I32 => Ok(32),
        Type::I64 | Type::Ptr => Ok(64),
        _ => Err(super::error("unsupported integer leaf type")),
    }
}

pub fn lower(body: &Body, instruction: &Instruction, b: &mut Builder<'_>) -> Result<(), Error> {
    let args = body.args(&instruction.arguments);
    let results = body.args(&instruction.results);
    if let crate::lir::body::Op::Call(call) = &instruction.op {
        return call::lower(body, call, args, results, false, b);
    }
    memory::lower(body, &instruction.op, args, results, b).or_else(|error| {
        if matches!(
            instruction.op,
            crate::lir::body::Op::IConst(_)
                | crate::lir::body::Op::Integer(_)
                | crate::lir::body::Op::Compare { .. }
                | crate::lir::body::Op::Convert(_)
                | crate::lir::body::Op::Select
        ) {
            integer::lower(body, &instruction.op, args, results, b)
        } else {
            Err(error)
        }
    })
}

pub fn signature(
    body: &Body,
    args: &[ValueId],
    results: &[ValueId],
    expected_args: &[Type],
    expected_results: &[Type],
) -> Result<(), Error> {
    if args
        .iter()
        .map(|id| body.values[id.index()].kind.ty)
        .collect::<Vec<_>>()
        != expected_args
        || results
            .iter()
            .map(|id| body.values[id.index()].kind.ty)
            .collect::<Vec<_>>()
            != expected_results
    {
        return Err(super::error("invalid leaf operation signature"));
    }
    Ok(())
}

pub fn reg(id: u32, width: Width, access: Access) -> MOperand {
    MOperand::Reg(MReg::Virtual(id), width, access)
}
