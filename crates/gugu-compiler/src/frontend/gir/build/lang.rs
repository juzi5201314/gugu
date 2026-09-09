//! lang item 钩子：scoped view 与 NoSafepoint 只能由登记路径产生。
use super::*;

impl Builder<'_> {
    /// 没有函数定义的内建 trait 成员名；其它情况返回 None。
    pub(super) fn builtin_member(&self, dispatch: u32) -> Option<String> {
        let plan = &self.owner.dispatches[dispatch as usize];
        if plan.dynamic || plan.function.is_some() {
            return None;
        }
        plan.member_name.clone()
    }

    /// 内建运算符方法对应的语言运算；接收者与 Rhs 各求值一次。
    pub(super) fn builtin_binary(&self, dispatch: u32) -> Option<BinaryOp> {
        Some(match self.builtin_member(dispatch)?.as_str() {
            "add" | "add_assign" => BinaryOp::Add,
            "sub" | "sub_assign" => BinaryOp::Sub,
            "mul" | "mul_assign" => BinaryOp::Mul,
            "div" | "div_assign" => BinaryOp::Div,
            "rem" | "rem_assign" => BinaryOp::Rem,
            "bitand" | "bitand_assign" => BinaryOp::BitAnd,
            "bitor" | "bitor_assign" => BinaryOp::BitOr,
            "bitxor" | "bitxor_assign" => BinaryOp::BitXor,
            "shl" | "shl_assign" => BinaryOp::Shl,
            "shr" | "shr_assign" => BinaryOp::Shr,
            _ => return None,
        })
    }

    /// 内建 trait 方法没有函数定义；在 GIR 中按成员名展开为封闭操作。
    pub(super) fn emit_builtin_dispatch(
        &mut self,
        id: ExprId,
        target: &hir::CallTarget,
        receiver: Option<ExprId>,
        arguments: &Range<u32>,
    ) -> Result<Option<LocalId>, Diagnostic> {
        let hir::CallTarget::Dispatch(dispatch) = target else {
            return Ok(None);
        };
        let dispatch = *dispatch;
        let plan = &self.owner.dispatches[dispatch as usize];
        if plan.dynamic || plan.function.is_some() {
            return Ok(None);
        }
        let Some(interface) = plan.interface.as_ref() else {
            return Ok(None);
        };
        let interface = self.module.definitions[interface.definition.index()]
            .name
            .clone();
        let Some(member) = plan.member_name.clone() else {
            return Ok(None);
        };
        let explicit = expr_range(self.owner, arguments);
        // `Clone::clone` 就是语言规定的语义拷贝：位值浅拷贝、COW 封存、resource lease。
        if let ("Clone", "clone") = (interface.as_str(), member.as_str()) {
            let receiver = match receiver {
                Some(receiver) => Some(receiver),
                None => match explicit.as_slice() {
                    [only] => Some(*only),
                    _ => None,
                },
            };
            let Some(receiver) = receiver else {
                return Ok(None);
            };
            let Some(local) = self.emit_expr(receiver)? else {
                return Ok(None);
            };
            let ty = self.owner.expression_types[id.index()];
            let dest = self.temp(ty);
            self.copy_value(Place::local(dest), Place::local(local), ty);
            self.set_value(id, dest);
            return Ok(Some(dest));
        }
        // 内建运算符 impl 的语义就是对应语言运算。
        let Some(op) = self.builtin_binary(dispatch) else {
            return Ok(None);
        };
        let (left, right) = match receiver {
            Some(receiver) => match explicit.as_slice() {
                [right] => (receiver, *right),
                _ => return Ok(None),
            },
            None => match explicit.as_slice() {
                [left, right] => (*left, *right),
                _ => return Ok(None),
            },
        };
        let Some(left) = self.emit_expr(left)? else {
            return Ok(None);
        };
        let Some(right) = self.emit_expr(right)? else {
            return Ok(None);
        };
        let ty = self.owner.expression_types[id.index()];
        let dest = self.temp(ty);
        self.assign(
            Place::local(dest),
            Rvalue::BinaryOp {
                op,
                left: copy_of(left),
                right: copy_of(right),
            },
        );
        self.set_value(id, dest);
        Ok(Some(dest))
    }

    pub(super) fn emit_lang_call(
        &mut self,
        id: ExprId,
        callee: &Callee,
        args: &[Operand],
    ) -> Result<Option<LocalId>, Diagnostic> {
        let Some(name) = lang_name(self.module, callee) else {
            return Ok(None);
        };
        if matches!(
            name.as_str(),
            "std.mem.with_ref" | "std.mem.with_read_ref" | "std.mem.for_each_ref"
        ) {
            return self.emit_scoped_view(id, name.as_str(), args).map(Some);
        }
        if matches!(
            name.as_str(),
            "std.runtime.no_safepoint_lock"
                | "std.runtime.ownership_publish"
                | "std.runtime.root_publish"
        ) {
            return self.emit_no_safepoint(id, name.as_str(), args).map(Some);
        }
        Ok(None)
    }

    fn emit_scoped_view(
        &mut self,
        id: ExprId,
        name: &str,
        args: &[Operand],
    ) -> Result<LocalId, Diagnostic> {
        let source = match args.first() {
            Some(Operand::Copy(place) | Operand::MoveInternal(place)) => *place,
            _ => Place::local(self.temp(self.primitives.unit)),
        };
        let token = self.temp(self.primitives.unit);
        let mode = if name.ends_with("with_ref") {
            ViewMode::ScopedWrite
        } else {
            ViewMode::ScopedRead
        };
        self.push_stmt(StatementKind::ScopedViewBegin {
            source,
            mode,
            token,
        });
        let dest = self.temp(self.expr_ty(id));
        if let Some(callback) = args.get(1).cloned() {
            let normal = self.fresh(false);
            let unwind = self.intern_plan(self.current_unwind(id), CleanupChain::Unwind)?;
            self.terminate(Terminator::Call {
                callee: Callee::Value(callback),
                args: vec![Operand::Copy(source)],
                destination: Place::local(dest),
                normal,
                unwind: Some(unwind),
                call_kind: CallKind::Managed,
                site: crate::frontend::mono::instantiate::CallSite::Expression(id.0),
            });
            self.switch_to(normal);
        } else {
            self.assign_unit(dest);
        }
        self.push_stmt(StatementKind::ScopedViewEnd { token });
        self.set_value(id, dest);
        Ok(dest)
    }

    fn emit_no_safepoint(
        &mut self,
        id: ExprId,
        name: &str,
        args: &[Operand],
    ) -> Result<LocalId, Diagnostic> {
        let reason = match name {
            "std.runtime.no_safepoint_lock" => NoSafepointReason::RuntimeLock,
            "std.runtime.ownership_publish" => NoSafepointReason::OwnershipPublish,
            _ => NoSafepointReason::RootPublish,
        };
        let region = NoSafepointRegionId(self.no_safepoint_regions.len() as u32);
        self.no_safepoint_regions.push(reason);
        self.push_stmt(StatementKind::NoSafepointBegin(region));
        let dest = self.temp(self.expr_ty(id));
        if let Some(callback) = args.first().cloned() {
            let normal = self.fresh(false);
            let unwind = self.intern_plan(self.current_unwind(id), CleanupChain::Unwind)?;
            self.terminate(Terminator::Call {
                callee: Callee::Value(callback),
                args: Vec::new(),
                destination: Place::local(dest),
                normal,
                unwind: Some(unwind),
                call_kind: CallKind::Managed,
                site: crate::frontend::mono::instantiate::CallSite::Expression(id.0),
            });
            self.switch_to(normal);
        } else {
            self.assign_unit(dest);
        }
        self.push_stmt(StatementKind::NoSafepointEnd(region));
        self.set_value(id, dest);
        Ok(dest)
    }
}

fn lang_name(module: &hir::Module, callee: &Callee) -> Option<String> {
    let definition = match callee {
        Callee::Dispatch(index) => module
            .owners
            .iter()
            .find_map(|owner| owner.dispatches.get(*index as usize))
            .and_then(|dispatch| dispatch.function)?,
        Callee::Value(Operand::Function(candidate)) => candidate.definition,
        _ => return None,
    };
    Some(module.definitions[definition.index()].name.clone())
}
