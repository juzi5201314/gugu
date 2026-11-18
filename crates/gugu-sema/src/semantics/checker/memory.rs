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
            MemoryIntrinsic::Unreachable | MemoryIntrinsic::ArenaNew { .. } => 0,
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
        if let MemoryIntrinsic::ArenaNew { sync } = kind {
            return Some(self.arena_new_call(callee, sync, args, expected, span));
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
            MemoryIntrinsic::ArenaNew { .. } | MemoryIntrinsic::ArenaAlloc { .. } => {
                unreachable!("arena 原语有专属通道")
            }
        };
        if !matches!(
            kind,
            MemoryIntrinsic::Uninit
                | MemoryIntrinsic::UninitNew
                | MemoryIntrinsic::UninitAsPtr
                | MemoryIntrinsic::UninitWrite
                | MemoryIntrinsic::ArenaNew { .. }
                | MemoryIntrinsic::ArenaAlloc { .. }
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
        if let Some(result) = self.arena_method(callee, receiver, name, types, args, expected) {
            return Some(result);
        }
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
        Some(self.memory_method_tail(
            callee,
            kind,
            parameters,
            result,
            (**value).clone(),
            args,
            expected,
            span,
        ))
    }

    /// arena `alloc[T]`：receiver 是 `std.mem.LocalArena`/`SyncArena` 及其引用，结果 `&T`。
    /// 返回 Some 表示已接管；返回 None 交给 MaybeUninit 检查。
    fn arena_method(
        &mut self,
        callee: ExprId,
        receiver: &Ty,
        name: &str,
        types: AstRange<GenericArg>,
        args: &[ExprId],
        expected: Option<&Ty>,
    ) -> Option<Ty> {
        let Ty::Named(index, _) = receiver.deref() else {
            return None;
        };
        let nominal = &self.model.nominal[*index];
        let sync = match (
            nominal.name.as_str(),
            self.model.modules[nominal.definition.module].path.as_str(),
        ) {
            ("LocalArena", "std.mem") => false,
            ("SyncArena", "std.mem") => true,
            _ => return None,
        };
        if name != "alloc" {
            return None;
        }
        let span = &self.arena().exprs[callee.0 as usize].span;
        let slice = types.as_slice(&self.arena().generic_args);
        let value = match slice.len() {
            // 缺省类型实参走常规推断：元素类型由 `v` 反解。
            0 => self.fresh(),
            1 => match self.model.form_argument(self.module, slice[0]) {
                Ok(ty) => ty,
                Err(error) => {
                    self.errors.push(error);
                    return Some(Ty::Error);
                }
            },
            _ => {
                self.error(
                    DiagnosticCode::InvalidExpression,
                    "arena 的 alloc 至多接受一个类型实参",
                    span.clone(),
                );
                return Some(Ty::Error);
            }
        };
        let result = Ty::Ref(Box::new(value.clone()));
        Some(self.memory_method_tail(
            callee,
            MemoryIntrinsic::ArenaAlloc { sync },
            vec![value.clone()],
            result,
            value,
            args,
            expected,
            span,
        ))
    }

    /// `alloc`/`write`/`as_ptr`/`assume_init` 共用的调用构造与操作登记。
    fn memory_method_tail(
        &mut self,
        callee: ExprId,
        kind: MemoryIntrinsic,
        parameters: Vec<Ty>,
        result: Ty,
        value: Ty,
        args: &[ExprId],
        expected: Option<&Ty>,
        span: &Span,
    ) -> Ty {
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
            value,
            result,
            arguments: args.to_vec(),
        });
        ty
    }

    /// `std.mem.LocalArena::new`/`SyncArena::new`/`with_capacity`：构造由 LIR 降级完成，
    /// 这里只建立签名与登记操作；`with_capacity` 的字面量负值在编译期拒绝。
    fn arena_new_call(
        &mut self,
        callee: ExprId,
        sync: bool,
        args: &[ExprId],
        expected: Option<&Ty>,
        span: &Span,
    ) -> Ty {
        let wanted = if sync { "SyncArena" } else { "LocalArena" };
        let Some(index) = self.model.nominal.iter().position(|nominal| {
            nominal.name == wanted
                && self.model.modules[nominal.definition.module].path == "std.mem"
        }) else {
            self.error(
                DiagnosticCode::InvalidType,
                "std.mem 缺少 arena 句柄类型",
                span.clone(),
            );
            return Ty::Error;
        };
        if args.len() > 1 {
            self.error(
                DiagnosticCode::InvalidExpression,
                "with_capacity 只接受一个容量实参",
                span.clone(),
            );
            return Ty::Error;
        }
        if let Some(&argument) = args.first() {
            let exprs = &self.arena().exprs;
            let ExprKind::Unary {
                op: UnOp::Neg,
                expr,
            } = exprs[argument.0 as usize].kind
            else {
                return self.arena_new_call_tail(callee, index, sync, args, expected);
            };
            if matches!(
                exprs[expr.0 as usize].kind,
                ExprKind::Literal(LitKind::Int { .. })
            ) {
                self.error(
                    DiagnosticCode::InvalidExpression,
                    "with_capacity 的容量不能为负",
                    span.clone(),
                );
                return Ty::Error;
            }
        }
        self.arena_new_call_tail(callee, index, sync, args, expected)
    }

    fn arena_new_call_tail(
        &mut self,
        callee: ExprId,
        index: usize,
        sync: bool,
        args: &[ExprId],
        expected: Option<&Ty>,
    ) -> Ty {
        let parameters = if args.is_empty() {
            Vec::new()
        } else {
            vec![Ty::Int {
                signed: true,
                bits: 64,
            }]
        };
        let result = Ty::Named(index, Vec::new());
        let ty = self.invoke(
            callee,
            Ty::Function(parameters, Box::new(result.clone())),
            args.to_vec(),
            None,
            expected,
        );
        self.memory_operations.push(MemoryOperation {
            expression: callee,
            kind: MemoryIntrinsic::ArenaNew { sync },
            value: result.clone(),
            result,
            arguments: args.to_vec(),
        });
        ty
    }
}
