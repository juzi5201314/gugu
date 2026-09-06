//! 标准内存原语共享推断与位置检查，不通过普通函数调用伪造按位操作。
use super::super::model::MemoryIntrinsic;
use super::super::output::MemoryOperation;
use super::*;

impl Checker<'_, '_> {
    pub(super) fn memory_call(
        &mut self,
        callee: ExprId,
        path: PathId,
        type_args: AstRange<GenericArg>,
        args: &[ExprId],
        expected: Option<&Ty>,
    ) -> Option<Ty> {
        let segments = self.arena().paths[path.0 as usize]
            .segments
            .as_slice(&self.arena().segments);
        if segments
            .first()
            .is_some_and(|segment| self.state.names.contains_key(&segment.name))
        {
            return None;
        }
        let path = self
            .model
            .external_path(self.module, &self.model.path(self.module, path))?;
        let kind = MemoryIntrinsic::from_path(&path)?;
        let type_args = if type_args.len != 0 {
            type_args
        } else {
            segments
                .iter()
                .rev()
                .find(|segment| segment.args.len != 0)
                .map_or(AstRange::empty(), |segment| segment.args)
        };
        let span = &self.arena().exprs[callee.0 as usize].span;
        let mut types = Vec::new();
        for &argument in type_args.as_slice(&self.arena().generic_args) {
            match self.model.form_argument(self.module, argument) {
                Ok(ty) => types.push(ty),
                Err(error) => {
                    self.errors.push(error);
                    return Some(Ty::Error);
                }
            }
        }
        let count = match kind {
            MemoryIntrinsic::Transmute => 2,
            MemoryIntrinsic::Unreachable => 0,
            _ => 1,
        };
        if !types.is_empty() && types.len() != count {
            self.error(
                DiagnosticCode::InvalidExpression,
                "内存原语的类型实参数量不符",
                span.clone(),
            );
            return Some(Ty::Error);
        }
        while types.len() < count {
            types.push(self.fresh());
        }
        let value = types.first().cloned().unwrap_or(Ty::Unit);
        if kind == MemoryIntrinsic::AddrOf {
            if args.len() != 1 {
                self.error(
                    DiagnosticCode::InvalidExpression,
                    "addr_of 需要一个位置实参",
                    span.clone(),
                );
                return Some(Ty::Error);
            }
            let actual = self.place(args[0], false);
            self.unify(&actual, &value, span);
            self.address_taken(args[0]);
            let result = Ty::Ptr(Box::new(value.clone()));
            self.memory_operations.push(MemoryOperation {
                expression: callee,
                kind,
                value,
                result: result.clone(),
                arguments: args.to_vec(),
            });
            return Some(result);
        }
        let (parameters, result) = match kind {
            MemoryIntrinsic::PtrRead
            | MemoryIntrinsic::ReadUnaligned
            | MemoryIntrinsic::VolatileLoad => {
                (vec![Ty::Ptr(Box::new(value.clone()))], value.clone())
            }
            MemoryIntrinsic::PtrWrite
            | MemoryIntrinsic::WriteUnaligned
            | MemoryIntrinsic::VolatileStore => (
                vec![Ty::Ptr(Box::new(value.clone())), value.clone()],
                Ty::Unit,
            ),
            MemoryIntrinsic::Transmute => (vec![value.clone()], types[1].clone()),
            MemoryIntrinsic::Unreachable => (Vec::new(), Ty::Never),
            MemoryIntrinsic::Uninit => (Vec::new(), Ty::MaybeUninit(Box::new(value.clone()))),
            MemoryIntrinsic::UninitNew => (
                vec![value.clone()],
                Ty::MaybeUninit(Box::new(value.clone())),
            ),
            MemoryIntrinsic::UninitAsPtr => (
                vec![Ty::Ref(Box::new(Ty::MaybeUninit(Box::new(value.clone()))))],
                Ty::Ptr(Box::new(value.clone())),
            ),
            MemoryIntrinsic::UninitWrite => (
                vec![
                    Ty::Ref(Box::new(Ty::MaybeUninit(Box::new(value.clone())))),
                    value.clone(),
                ],
                Ty::Unit,
            ),
            MemoryIntrinsic::AssumeInit => (
                vec![Ty::MaybeUninit(Box::new(value.clone()))],
                value.clone(),
            ),
            MemoryIntrinsic::AddrOf => unreachable!("addr_of 已检查位置"),
            MemoryIntrinsic::PointerCast | MemoryIntrinsic::ScalarCast => {
                unreachable!("类型构造语法形成转换")
            }
        };
        if !matches!(
            kind,
            MemoryIntrinsic::Uninit
                | MemoryIntrinsic::UninitNew
                | MemoryIntrinsic::UninitAsPtr
                | MemoryIntrinsic::UninitWrite
        ) && self.unsafe_depth == 0
        {
            self.error(
                DiagnosticCode::InvalidExpression,
                "内存原语调用必须处于 unsafe 块中",
                span.clone(),
            );
        }
        let ty = self.invoke(
            callee,
            Ty::Function(parameters, Box::new(result.clone())),
            args.to_vec(),
            None,
            expected,
        );
        self.memory_operations.push(MemoryOperation {
            expression: callee,
            kind,
            value,
            result,
            arguments: args.to_vec(),
        });
        Some(ty)
    }

    pub(super) fn memory_method(
        &mut self,
        callee: ExprId,
        receiver: &Ty,
        name: &str,
        types: AstRange<GenericArg>,
        args: &[ExprId],
        expected: Option<&Ty>,
    ) -> Option<Ty> {
        let Ty::MaybeUninit(value) = receiver.deref() else {
            return None;
        };
        let (kind, parameters, result) = match name {
            "as_ptr" => (
                MemoryIntrinsic::UninitAsPtr,
                Vec::new(),
                Ty::Ptr(value.clone()),
            ),
            "write" => (
                MemoryIntrinsic::UninitWrite,
                vec![(**value).clone()],
                Ty::Unit,
            ),
            "assume_init" => (MemoryIntrinsic::AssumeInit, Vec::new(), (**value).clone()),
            _ => return None,
        };
        let span = &self.arena().exprs[callee.0 as usize].span;
        if types.len != 0 {
            self.error(
                DiagnosticCode::InvalidExpression,
                "MaybeUninit 实例方法不接收类型实参",
                span.clone(),
            );
        }
        if kind == MemoryIntrinsic::AssumeInit && self.unsafe_depth == 0 {
            self.error(
                DiagnosticCode::InvalidExpression,
                "assume_init 必须处于 unsafe 块中",
                span.clone(),
            );
        }
        self.address_taken(callee);
        let ty = self.invoke(
            callee,
            Ty::Function(parameters, Box::new(result.clone())),
            args.to_vec(),
            None,
            expected,
        );
        self.memory_operations.push(MemoryOperation {
            expression: callee,
            kind,
            value: (**value).clone(),
            result,
            arguments: args.to_vec(),
        });
        Some(ty)
    }
}
