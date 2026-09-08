//! 从冻结前 HIR 构造过程内显式 CFG。

use crate::frontend::ast::{AssignOp, BinOp};
use crate::frontend::hir::{
    self, ExprId, ExprKind, LocalId, Owner, PatternKind, ScopeId, StatementKind, StmtId,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct BlockId(pub u32);

impl BlockId {
    pub fn index(self) -> usize {
        self.0 as usize
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Inst {
    Eval(ExprId),
    Bind {
        local: LocalId,
        value: ExprId,
    },
    Assign {
        place: ExprId,
        value: ExprId,
        operation: AssignOp,
    },
    Increment(LocalId),
    Dispatch(u32),
    Initialize(u32),
    Yield,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Terminator {
    Goto {
        target: BlockId,
        backedge: bool,
    },
    If {
        cond: ExprId,
        then_block: BlockId,
        else_block: BlockId,
    },
    Iv {
        local: LocalId,
        end: ExprId,
        body: BlockId,
        exit: BlockId,
    },
    Switch {
        value: ExprId,
        arms: Vec<BlockId>,
    },
    Return(Option<ExprId>),
    Unreachable,
}

#[derive(Clone, Debug)]
pub(crate) struct Block {
    pub instructions: Vec<Inst>,
    pub terminator: Option<Terminator>,
    pub predecessors: Vec<BlockId>,
}

#[derive(Clone, Debug)]
pub(crate) struct Cfg {
    pub blocks: Vec<Block>,
    pub entry: BlockId,
}

struct LoopFrame {
    scope: ScopeId,
    latch: BlockId,
    exit: BlockId,
}

struct Builder<'a> {
    owner: &'a Owner,
    blocks: Vec<Block>,
    current: BlockId,
    loops: Vec<LoopFrame>,
    tries: Vec<(ScopeId, BlockId)>,
    emitted: Vec<bool>,
}

pub(crate) fn build(owner: &Owner) -> Cfg {
    let mut builder = Builder {
        owner,
        blocks: Vec::new(),
        current: BlockId(0),
        loops: Vec::new(),
        tries: Vec::new(),
        emitted: vec![false; owner.expressions.len()],
    };
    let entry = builder.fresh();
    builder.current = entry;
    builder.emit_expr(owner.body);
    if builder.block_mut(builder.current).terminator.is_none() {
        builder.terminate(Terminator::Return(Some(owner.body)));
    }
    Cfg {
        blocks: builder.blocks,
        entry,
    }
}

impl<'a> Builder<'a> {
    fn fresh(&mut self) -> BlockId {
        let id = BlockId(self.blocks.len() as u32);
        self.blocks.push(Block {
            instructions: Vec::new(),
            terminator: None,
            predecessors: Vec::new(),
        });
        id
    }

    fn block_mut(&mut self, id: BlockId) -> &mut Block {
        &mut self.blocks[id.index()]
    }

    fn push(&mut self, inst: Inst) {
        if self.block_mut(self.current).terminator.is_none() {
            self.block_mut(self.current).instructions.push(inst);
        }
    }

    fn terminate(&mut self, terminator: Terminator) {
        if self.block_mut(self.current).terminator.is_some() {
            return;
        }
        match &terminator {
            Terminator::Goto { target, .. } => self.add_pred(*target, self.current),
            Terminator::If {
                then_block,
                else_block,
                ..
            } => {
                self.add_pred(*then_block, self.current);
                self.add_pred(*else_block, self.current);
            }
            Terminator::Iv { body, exit, .. } => {
                self.add_pred(*body, self.current);
                self.add_pred(*exit, self.current);
            }
            Terminator::Switch { arms, .. } => {
                for &arm in arms {
                    self.add_pred(arm, self.current);
                }
            }
            Terminator::Return(_) | Terminator::Unreachable => {}
        }
        self.block_mut(self.current).terminator = Some(terminator);
    }

    fn add_pred(&mut self, target: BlockId, pred: BlockId) {
        if !self.blocks[target.index()].predecessors.contains(&pred) {
            self.blocks[target.index()].predecessors.push(pred);
        }
    }

    fn goto(&mut self, target: BlockId, backedge: bool) {
        self.terminate(Terminator::Goto { target, backedge });
    }

    fn switch_to(&mut self, id: BlockId) {
        self.current = id;
    }

    fn emit_expr(&mut self, id: ExprId) {
        if self.emitted[id.index()] {
            return;
        }
        let kind = &self.owner.expressions[id.index()].kind;
        match kind {
            ExprKind::If {
                condition,
                then_value,
                else_value,
            } => {
                let condition = *condition;
                let then_value = *then_value;
                let else_value = *else_value;
                self.emit_if(id, condition, then_value, else_value);
            }
            ExprKind::Loop { body } => {
                let body = *body;
                self.emit_loop(id, None, body);
            }
            ExprKind::While { condition, body } => {
                let condition = *condition;
                let body = *body;
                self.emit_loop(id, Some(condition), body);
            }
            ExprKind::For {
                pattern,
                value,
                body,
                ..
            } => {
                let pattern = *pattern;
                let value = *value;
                let body = *body;
                self.emit_for(id, pattern, value, body);
            }
            ExprKind::Match { value, arms } => {
                let value = *value;
                let arms = arms.clone();
                self.emit_match(id, value, arms);
            }
            ExprKind::Block { statements, tail } => {
                let statements = statements.clone();
                let tail = *tail;
                self.emit_block(id, statements, tail);
            }
            ExprKind::Exit { target, value, .. } => {
                let target = *target;
                let value = *value;
                self.emit_exit(id, target, value);
            }
            ExprKind::Try { body, .. } => self.emit_try(id, *body),
            ExprKind::TryExit {
                value,
                target,
                branch,
                from_error,
                ..
            } => {
                self.emit_try_exit(id, *value, *target, *branch, *from_error);
            }
            ExprKind::Binary {
                operation,
                left,
                right,
                ..
            } if matches!(*operation, BinOp::And | BinOp::Or) => {
                let left = *left;
                let right = *right;
                let is_and = *operation == BinOp::And;
                self.emit_short_circuit(id, left, right, is_and);
            }
            _ => self.emit_simple(id),
        }
    }

    fn emit_simple(&mut self, id: ExprId) {
        for child in children(self.owner, id) {
            self.emit_expr(child);
        }
        self.emitted[id.index()] = true;
        self.push(Inst::Eval(id));
        if matches!(
            &self.owner.expressions[id.index()].kind,
            ExprKind::Intrinsic {
                operation: hir::Builtin::Panic,
                ..
            }
        ) {
            self.terminate(Terminator::Unreachable);
        }
    }

    fn emit_block(&mut self, id: ExprId, statements: std::ops::Range<u32>, tail: Option<ExprId>) {
        for index in statements.start as usize..statements.end as usize {
            self.emit_stmt(self.owner.statement_ids[index]);
        }
        if let Some(tail) = tail {
            self.emit_expr(tail);
        }
        self.emitted[id.index()] = true;
        self.push(Inst::Eval(id));
    }

    fn emit_stmt(&mut self, id: StmtId) {
        let kind = self.owner.statements[id.index()].kind.clone();
        match kind {
            StatementKind::Let {
                pattern,
                value,
                otherwise,
            } => {
                if let Some(value) = value {
                    self.emit_expr(value);
                    self.bind_pattern(pattern, value);
                }
                if let Some(otherwise) = otherwise {
                    let join = self.fresh();
                    self.goto(join, false);
                    let fail = self.fresh();
                    self.switch_to(fail);
                    self.emit_expr(otherwise);
                    if self.block_mut(self.current).terminator.is_none() {
                        self.goto(join, false);
                    }
                    self.switch_to(join);
                }
            }
            StatementKind::Assign {
                place,
                value,
                dispatch,
                operation,
            } => {
                self.emit_expr(place);
                self.emit_expr(value);
                if let Some(dispatch) = dispatch {
                    self.push(Inst::Dispatch(dispatch));
                }
                self.push(Inst::Assign {
                    place,
                    value,
                    operation,
                });
            }
            StatementKind::Expression(value) => self.emit_expr(value),
            StatementKind::Defer(action) => {
                let body = self.owner.cleanup[action as usize].body;
                self.emit_expr(body);
            }
            StatementKind::Static { .. } => self.push(Inst::Initialize(id.0)),
            StatementKind::Yield => self.push(Inst::Yield),
        }
    }

    fn emit_if(
        &mut self,
        id: ExprId,
        condition: ExprId,
        then_value: ExprId,
        else_value: Option<ExprId>,
    ) {
        self.emit_expr(condition);
        let then_block = self.fresh();
        let else_block = self.fresh();
        let join = self.fresh();
        self.terminate(Terminator::If {
            cond: condition,
            then_block,
            else_block,
        });
        self.switch_to(then_block);
        self.emit_expr(then_value);
        self.goto(join, false);
        self.switch_to(else_block);
        if let Some(else_value) = else_value {
            self.emit_expr(else_value);
        }
        self.goto(join, false);
        self.switch_to(join);
        self.emitted[id.index()] = true;
        self.push(Inst::Eval(id));
    }

    fn emit_short_circuit(&mut self, id: ExprId, left: ExprId, right: ExprId, is_and: bool) {
        self.emit_expr(left);
        let rhs = self.fresh();
        let join = self.fresh();
        if is_and {
            self.terminate(Terminator::If {
                cond: left,
                then_block: rhs,
                else_block: join,
            });
        } else {
            self.terminate(Terminator::If {
                cond: left,
                then_block: join,
                else_block: rhs,
            });
        }
        self.switch_to(rhs);
        self.emit_expr(right);
        self.goto(join, false);
        self.switch_to(join);
        self.emitted[id.index()] = true;
        self.push(Inst::Eval(id));
    }

    fn emit_loop(&mut self, id: ExprId, condition: Option<ExprId>, body: ExprId) {
        let header = self.fresh();
        let body_block = self.fresh();
        let exit = self.fresh();
        self.goto(header, false);
        self.loops.push(LoopFrame {
            scope: self.owner.expressions[id.index()].scope,
            latch: header,
            exit,
        });
        self.switch_to(header);
        if let Some(condition) = condition {
            self.emit_expr(condition);
            self.terminate(Terminator::If {
                cond: condition,
                then_block: body_block,
                else_block: exit,
            });
            self.switch_to(body_block);
        }
        self.emit_expr(body);
        self.goto(header, true);
        self.loops.pop();
        self.switch_to(exit);
        self.emitted[id.index()] = true;
        self.push(Inst::Eval(id));
    }

    fn emit_try(&mut self, id: ExprId, body: ExprId) {
        let exit = self.fresh();
        self.tries
            .push((self.owner.expressions[id.index()].scope, exit));
        self.emit_expr(body);
        self.goto(exit, false);
        self.tries.pop();
        self.switch_to(exit);
        self.emitted[id.index()] = true;
        self.push(Inst::Eval(id));
    }

    fn emit_try_exit(
        &mut self,
        id: ExprId,
        value: ExprId,
        target: hir::ExitTarget,
        branch: Option<u32>,
        from_error: Option<u32>,
    ) {
        self.emit_expr(value);
        if let Some(dispatch) = branch {
            self.push(Inst::Dispatch(dispatch));
        }
        let success = self.fresh();
        let failure = self.fresh();
        self.terminate(Terminator::Switch {
            value,
            arms: vec![success, failure],
        });
        self.switch_to(failure);
        if let Some(dispatch) = from_error {
            self.push(Inst::Dispatch(dispatch));
        }
        self.emit_exit(id, target, None);
        self.switch_to(success);
        self.emitted[id.index()] = true;
        self.push(Inst::Eval(id));
    }

    fn emit_for(&mut self, id: ExprId, pattern: hir::PatternId, value: ExprId, body: ExprId) {
        self.emit_expr(value);
        let ExprKind::For {
            into_iter, next, ..
        } = self.owner.expressions[id.index()].kind
        else {
            unreachable!("emit_for 只接收 for 表达式");
        };
        if let Some(dispatch) = into_iter {
            self.push(Inst::Dispatch(dispatch));
        }
        let iv = match self.owner.patterns[pattern.index()].kind {
            PatternKind::Bind(local) => Some(local),
            _ => None,
        };
        let end = match &self.owner.expressions[value.index()].kind {
            ExprKind::Range { end, .. } => Some(*end),
            _ => None,
        };
        let start = match &self.owner.expressions[value.index()].kind {
            ExprKind::Range { start, .. } => Some(*start),
            _ => None,
        };
        if let (Some(iv), Some(start), Some(end)) = (iv, start, end) {
            self.push(Inst::Bind {
                local: iv,
                value: start,
            });
            let header = self.fresh();
            let body_block = self.fresh();
            let latch = self.fresh();
            let exit = self.fresh();
            self.goto(header, false);
            self.loops.push(LoopFrame {
                scope: self.owner.expressions[id.index()].scope,
                latch,
                exit,
            });
            self.switch_to(header);
            self.terminate(Terminator::Iv {
                local: iv,
                end,
                body: body_block,
                exit,
            });
            self.switch_to(body_block);
            self.emit_expr(body);
            self.goto(latch, false);
            self.switch_to(latch);
            self.push(Inst::Increment(iv));
            self.goto(header, true);
            self.loops.pop();
            self.switch_to(exit);
        } else {
            self.emit_iterator_loop(id, value, body, next);
            return;
        }
        self.emitted[id.index()] = true;
        self.push(Inst::Eval(id));
    }

    fn emit_iterator_loop(&mut self, id: ExprId, value: ExprId, body: ExprId, next: Option<u32>) {
        let header = self.fresh();
        let loop_body = self.fresh();
        let exit = self.fresh();
        self.goto(header, false);
        self.switch_to(header);
        if let Some(dispatch) = next {
            self.push(Inst::Dispatch(dispatch));
        }
        // 非 range 迭代器允许零次迭代，后继必须保留为可达路径。
        self.terminate(Terminator::Switch {
            value,
            arms: vec![loop_body, exit],
        });
        self.loops.push(LoopFrame {
            scope: self.owner.expressions[id.index()].scope,
            latch: header,
            exit,
        });
        self.switch_to(loop_body);
        self.emit_expr(body);
        self.goto(header, true);
        self.loops.pop();
        self.switch_to(exit);
        self.emitted[id.index()] = true;
        self.push(Inst::Eval(id));
    }

    fn emit_match(&mut self, id: ExprId, value: ExprId, arms: std::ops::Range<u32>) {
        self.emit_expr(value);
        let join = self.fresh();
        let mut blocks = Vec::new();
        for _ in arms.start..arms.end {
            blocks.push(self.fresh());
        }
        if blocks.is_empty() {
            self.emitted[id.index()] = true;
            return;
        }
        self.terminate(Terminator::Switch {
            value,
            arms: blocks.clone(),
        });
        for (offset, &block) in blocks.iter().enumerate() {
            self.switch_to(block);
            let arm = &self.owner.arms[(arms.start as usize) + offset];
            if let Some(guard) = arm.guard {
                self.emit_expr(guard);
            }
            self.emit_expr(arm.body);
            self.goto(join, false);
        }
        self.switch_to(join);
        self.emitted[id.index()] = true;
        self.push(Inst::Eval(id));
    }

    fn emit_exit(&mut self, id: ExprId, target: hir::ExitTarget, value: Option<ExprId>) {
        if let Some(value) = value {
            self.emit_expr(value);
        }
        self.emitted[id.index()] = true;
        self.push(Inst::Eval(id));
        match target {
            hir::ExitTarget::Return => {
                self.terminate(Terminator::Return(value));
            }
            hir::ExitTarget::Try(scope) => {
                let exit = self
                    .tries
                    .iter()
                    .rev()
                    .find(|(target, _)| *target == scope)
                    .map(|(_, exit)| *exit)
                    .expect("try 退出目标属于包围的 try scope");
                self.goto(exit, false);
            }
            hir::ExitTarget::Break(scope) => {
                if let Some(frame) = self.loops.iter().rev().find(|frame| frame.scope == scope) {
                    let exit = frame.exit;
                    self.goto(exit, false);
                } else {
                    self.terminate(Terminator::Return(value));
                }
            }
            hir::ExitTarget::Continue(scope) => {
                if let Some(frame) = self.loops.iter().rev().find(|frame| frame.scope == scope) {
                    let latch = frame.latch;
                    self.goto(latch, false);
                } else {
                    self.terminate(Terminator::Return(None));
                }
            }
        }
    }

    fn bind_pattern(&mut self, pattern: hir::PatternId, value: ExprId) {
        match self.owner.patterns[pattern.index()].kind {
            PatternKind::Bind(local) | PatternKind::At { local, .. } => {
                self.push(Inst::Bind { local, value });
            }
            PatternKind::Ref(inner) => self.bind_pattern(inner, value),
            _ => {}
        }
    }
}

fn children(owner: &Owner, id: ExprId) -> Vec<ExprId> {
    match &owner.expressions[id.index()].kind {
        ExprKind::Literal(_)
        | ExprKind::Resolved(_)
        | ExprKind::Closure { .. }
        | ExprKind::Spawn { .. }
        | ExprKind::Assembly(_) => Vec::new(),
        ExprKind::Unary { value, .. }
        | ExprKind::Field { base: value, .. }
        | ExprKind::Comptime { value }
        | ExprKind::Repeat { value, .. }
        | ExprKind::Try { body: value, .. } => vec![*value],
        ExprKind::Binary { left, right, .. } => vec![*left, *right],
        ExprKind::Index { base, index, .. } => vec![*base, *index],
        ExprKind::Range { start, end } => vec![*start, *end],
        ExprKind::Slice { base, start, end } => {
            let mut values = vec![*base];
            values.extend(*start);
            values.extend(*end);
            values
        }
        ExprKind::Call {
            target,
            receiver,
            arguments,
        }
        | ExprKind::SpawnCall {
            target,
            receiver,
            arguments,
        } => {
            let mut values = Vec::new();
            if let hir::CallTarget::Value(value) = target {
                values.push(*value);
            }
            values.extend(*receiver);
            values.extend(expr_list(owner, arguments));
            values
        }
        ExprKind::Intrinsic { arguments, .. } => expr_list(owner, arguments),
        ExprKind::Tuple(items) | ExprKind::Array(items) => expr_list(owner, items),
        ExprKind::Construct { fields, .. } => owner.fields
            [fields.start as usize..fields.end as usize]
            .iter()
            .map(|field| field.value)
            .collect(),
        ExprKind::Select { arms } => select_children(owner, arms.clone()),
        ExprKind::String { parts } => owner.string_parts[parts.start as usize..parts.end as usize]
            .iter()
            .filter_map(|part| match part {
                hir::StringPart::Value { expression, .. } => Some(*expression),
                hir::StringPart::Text(_) => None,
            })
            .collect(),
        ExprKind::LetCondition { value, .. } => vec![*value],
        _ => Vec::new(),
    }
}

fn expr_list(owner: &Owner, list: &std::ops::Range<u32>) -> Vec<ExprId> {
    owner.expression_ids[list.start as usize..list.end as usize].to_vec()
}

fn select_children(owner: &Owner, arms: std::ops::Range<u32>) -> Vec<ExprId> {
    let mut values = Vec::new();
    for arm in &owner.select_arms[arms.start as usize..arms.end as usize] {
        match arm {
            hir::SelectArm::Send {
                channel,
                value,
                body,
            } => values.extend([*channel, *value, *body]),
            hir::SelectArm::Recv { channel, body, .. }
            | hir::SelectArm::Wait {
                join: channel,
                body,
                ..
            } => values.extend([*channel, *body]),
            hir::SelectArm::Default { body } => values.push(*body),
        }
    }
    values
}
