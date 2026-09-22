//! lang item 钩子：scoped view 只能由登记路径产生。
use super::*;
use crate::frontend::semantics::model::RuntimeIntrinsic;

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
        if let Some(mode) = collection_view(self.module, callee) {
            // 标准集合的 `with_ref` / `for_each_ref`：整个调用在接收者的 view 动态 extent 内。
            let source = operand_place(self, args.first());
            let dest = self.emit_view_call(id, source, mode, callee.clone(), args.to_vec())?;
            return Ok(Some(dest));
        }
        let Some(name) = lang_name(self.module, callee) else {
            return Ok(None);
        };
        if matches!(
            name.as_str(),
            "std.mem.with_ref" | "std.mem.with_read_ref" | "std.mem.for_each_ref"
        ) {
            return self.emit_scoped_view(id, name.as_str(), args).map(Some);
        }
        Ok(None)
    }

    fn emit_scoped_view(
        &mut self,
        id: ExprId,
        name: &str,
        args: &[Operand],
    ) -> Result<LocalId, Diagnostic> {
        let source = operand_place(self, args.first());
        let mode = if name.ends_with("with_ref") {
            ViewMode::ScopedWrite
        } else {
            ViewMode::ScopedRead
        };
        let Some(callback) = args.get(1).cloned() else {
            let token = self.temp(self.primitives.unit);
            self.push_stmt(StatementKind::ScopedViewBegin {
                source,
                mode,
                token,
            });
            let dest = self.temp(self.expr_ty(id));
            self.assign_unit(dest);
            self.push_stmt(StatementKind::ScopedViewEnd { token });
            self.set_value(id, dest);
            return Ok(dest);
        };
        self.emit_view_call(
            id,
            source,
            mode,
            Callee::Value(callback),
            vec![Operand::Copy(source)],
        )
    }

    /// 在 `source` 的 scoped view 内执行一次调用；token 在正常返回与 panic 展开两条路径都闭合。
    fn emit_view_call(
        &mut self,
        id: ExprId,
        source: Place,
        mode: ViewMode,
        callee: Callee,
        args: Vec<Operand>,
    ) -> Result<LocalId, Diagnostic> {
        let token = self.temp(self.primitives.unit);
        self.push_stmt(StatementKind::ScopedViewBegin {
            source,
            mode,
            token,
        });
        let dest = self.temp(self.expr_ty(id));
        let normal = self.fresh(false);
        let unwind = self.intern_plan(self.current_unwind(id), CleanupChain::Unwind)?;
        // 展开路径先闭合 view，再进入外围 cleanup 链。
        let landing = self.fresh(true);
        self.terminate(Terminator::Call {
            callee,
            args,
            destination: Place::local(dest),
            normal,
            unwind: Some(landing),
            call_kind: CallKind::Managed,
            site: crate::frontend::mono::instantiate::CallSite::Expression(id.0),
        });
        self.switch_to(landing);
        self.push_stmt(StatementKind::ScopedViewEnd { token });
        self.terminate(Terminator::Goto { target: unwind });
        self.switch_to(normal);
        self.push_stmt(StatementKind::ScopedViewEnd { token });
        self.set_value(id, dest);
        Ok(dest)
    }

    pub(super) fn emit_runtime_intrinsic(
        &mut self,
        id: ExprId,
        kind: RuntimeIntrinsic,
        arguments: Range<u32>,
    ) -> Result<Option<LocalId>, Diagnostic> {
        let start = usize::try_from(arguments.start).expect("表达式范围起点");
        let end = usize::try_from(arguments.end).expect("表达式范围终点");
        let [destination, value] = self.owner.expression_ids[start..end] else {
            return Err(gir_error(
                "运行时 publish 原语缺少目标位置或值",
                Some(&self.source_of(id).location),
            ));
        };
        let place = self.emit_place(destination)?;
        let Some(value) = self.emit_expr(value)? else {
            return Ok(None);
        };
        let reason = match kind {
            RuntimeIntrinsic::OwnershipPublish => NoSafepointReason::OwnershipPublish,
            RuntimeIntrinsic::RootPublish => NoSafepointReason::RootPublish,
        };
        let region = NoSafepointRegionId(
            u32::try_from(self.no_safepoint_regions.len()).expect("NoSafepointRegion 编号"),
        );
        self.no_safepoint_regions.push(reason);
        self.push_stmt(StatementKind::NoSafepointBegin(region));
        self.assign(place, Rvalue::Use(copy_of(value)));
        self.push_stmt(StatementKind::NoSafepointEnd(region));
        let dest = self.temp(self.expr_ty(id));
        self.assign_unit(dest);
        self.set_value(id, dest);
        Ok(Some(dest))
    }
}

/// 把平台原语降级为 `StatementKind::PlatformCall`。
///
/// 实参按源码顺序求值并保留为 operands；返回值按 `types` 中的结果类型建立临时位置。平台调用
/// 一律禁止进入 `NoSafepointRegion`，因此这里不生成任何无 safepoint 区域。
impl Builder<'_> {
    pub(super) fn emit_platform_call(
        &mut self,
        id: ExprId,
        op: crate::runtime::PlatformOp,
        arguments: Range<u32>,
        types: Vec<TypeId>,
    ) -> Result<Option<LocalId>, Diagnostic> {
        let mut operands = Vec::new();
        for argument in expr_range(self.owner, &arguments) {
            let Some(local) = self.emit_expr(argument)? else {
                return Ok(None);
            };
            operands.push(copy_of(local));
        }
        self.emit_check_ops(id)?;
        // 状态操作返回 `()`，查询返回 `bool`，`entropy` 返回字节切片；结果类型由 HIR 给出。
        let destination = match types.first() {
            Some(&ty) if !matches!(self.module.types[ty.index()], hir::Type::Unit) => {
                let dest = self.temp(ty);
                Some(Place::local(dest))
            }
            _ => None,
        };
        self.push_stmt(StatementKind::PlatformCall {
            op,
            operands,
            destination,
        });
        match destination {
            Some(place) => {
                let dest = place.local;
                self.set_value(id, dest);
                Ok(Some(dest))
            }
            None => {
                let dest = self.temp(self.expr_ty(id));
                self.assign_unit(dest);
                self.set_value(id, dest);
                Ok(Some(dest))
            }
        }
    }
}

fn operand_place(builder: &mut Builder<'_>, operand: Option<&Operand>) -> Place {
    match operand {
        Some(Operand::Copy(place) | Operand::MoveInternal(place)) => *place,
        _ => Place::local(builder.temp(builder.primitives.unit)),
    }
}

/// 标准集合上 `with_ref` / `for_each_ref` 的派发；接收者类型必须是登记的共享身份集合。
fn collection_view(module: &hir::Module, callee: &Callee) -> Option<ViewMode> {
    let Callee::Dispatch(index) = callee else {
        return None;
    };
    let dispatch = module
        .owners
        .iter()
        .find_map(|owner| owner.dispatches.get(*index as usize))?;
    let function = dispatch.function?;
    if !matches!(
        module.definitions[function.index()].name.as_str(),
        "with_ref" | "for_each_ref"
    ) {
        return None;
    }
    let mut ty = dispatch.self_ty;
    while let Some(hir::Type::Ref(inner)) = module.types.get(ty.index()) {
        ty = *inner;
    }
    let Some(hir::Type::Named { definition, .. }) = module.types.get(ty.index()) else {
        return None;
    };
    super::super::passing::collection_item(module, *definition).then_some(ViewMode::ScopedRead)
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
