//! 从冻结 HIR owner 构造 generic GIR body。
mod calls;
mod checks;
mod cleanup;
mod concurrency;
mod control;
mod copy;
mod expr;
mod lang;
mod patterns;
mod place;

use super::body::*;
use super::passing::PassingTable;
use super::{Primitives, body_kind, gir_error, primitive_types};
use crate::frontend::hir::{self, ExprId, TypeId};
const ADDRESS_TAKEN: u8 = 1;
const CAPTURED: u8 = 2;
const CROSS_COROUTINE: u8 = 4;
use crate::Diagnostic;
use std::collections::BTreeMap;
use std::ops::Range;

pub(super) fn lower(module: &hir::Module, owner: &hir::Owner) -> Result<GirBody, Diagnostic> {
    let mut builder = Builder::new(module, owner)?;
    builder.emit_body()?;
    let body = builder.finish()?;
    super::verify(module, &body)?;
    Ok(body)
}

struct PendingBlock {
    statements: Vec<Statement>,
    terminator: Option<Terminator>,
    source: SourceInfo,
    cleanup: bool,
}

struct LoopFrame {
    scope: hir::ScopeId,
    header: BlockId,
    exit: BlockId,
    value: Option<LocalId>,
}

struct TryFrame {
    scope: hir::ScopeId,
    exit: BlockId,
    value: Option<LocalId>,
}

enum SavedCleanup {
    Call {
        callee: Callee,
        args: Vec<Operand>,
        destination: Place,
        call_kind: CallKind,
        site: crate::frontend::mono::instantiate::CallSite,
    },
    Body(hir::ExprId),
}

struct Builder<'a> {
    module: &'a hir::Module,
    owner: &'a hir::Owner,
    primitives: Primitives,
    locals: Vec<GirLocal>,
    blocks: Vec<PendingBlock>,
    current: BlockId,
    projections: Vec<Projection>,
    constants: Vec<Constant>,
    source_scopes: Vec<SourceScope>,
    cleanup_regions: Vec<CleanupRegion>,
    exit_records: Vec<ExitRecord>,
    safepoints: Vec<Safepoint>,
    no_safepoint_regions: Vec<NoSafepointReason>,
    select_cases: Vec<SelectCase>,
    expression_locals: Vec<Option<LocalId>>,
    expression_places: Vec<Option<Place>>,
    match_leaves: Vec<(BlockId, u32)>,
    flags: u32,
    hir_to_gir: Vec<LocalId>,
    return_local: LocalId,
    chain_head: Option<LocalId>,
    cleanup_flags: Vec<Option<LocalId>>,
    saved: Vec<SavedCleanup>,
    intern: BTreeMap<(CleanupChain, Vec<hir::CleanupAction>), BlockId>,
    intern_order: u32,
    loops: Vec<LoopFrame>,
    tries: Vec<TryFrame>,
    live: Vec<bool>,
    written: Vec<bool>,
    large_copies: Vec<LargeCopySite>,
    passing: PassingTable,
    return_block: BlockId,
}

impl<'a> Builder<'a> {
    fn new(module: &'a hir::Module, owner: &'a hir::Owner) -> Result<Self, Diagnostic> {
        let primitives = primitive_types(module)?;
        let source_scopes = owner
            .scopes
            .iter()
            .map(|scope| SourceScope {
                parent: scope.parent.map(|id| ScopeId(id.0)),
                location: scope.location.clone(),
                hir_scope: hir::ScopeId(0),
            })
            .collect();
        let mut builder = Self {
            module,
            owner,
            primitives,
            locals: Vec::new(),
            blocks: Vec::new(),
            current: BlockId(0),
            projections: Vec::new(),
            constants: Vec::new(),
            source_scopes,
            cleanup_regions: Vec::new(),
            exit_records: Vec::new(),
            safepoints: Vec::new(),
            no_safepoint_regions: Vec::new(),
            select_cases: Vec::new(),
            expression_locals: vec![None; owner.expressions.len()],
            expression_places: vec![None; owner.expressions.len()],
            match_leaves: Vec::new(),
            flags: 0,
            hir_to_gir: Vec::new(),
            return_local: LocalId(0),
            chain_head: None,
            cleanup_flags: vec![None; owner.cleanup.len()],
            saved: Vec::new(),
            intern: BTreeMap::new(),
            intern_order: 0,
            loops: Vec::new(),
            tries: Vec::new(),
            live: Vec::new(),
            written: Vec::new(),
            large_copies: Vec::new(),
            passing: PassingTable::new(module),
            return_block: BlockId(0),
        };
        for (index, scope) in builder.source_scopes.iter_mut().enumerate() {
            scope.hir_scope = hir::ScopeId(index as u32);
        }
        builder.allocate_locals()?;
        let entry = builder.fresh(false);
        builder.current = entry;
        builder.return_block = builder.fresh(false);
        builder.live_entry_locals();
        Ok(builder)
    }

    fn emit_body(&mut self) -> Result<(), Diagnostic> {
        let value = self.emit_expr(self.owner.body)?;
        if !self.terminated() {
            if let Some(value) = value {
                self.assign_copy(Place::local(self.return_local), value);
            } else {
                self.assign_unit(self.return_local);
            }
            let cleanup = self.intern_plan(self.owner.return_plan, CleanupChain::Normal)?;
            self.goto(cleanup);
        }
        self.fill_return_block();
        Ok(())
    }

    fn finish(mut self) -> Result<GirBody, Diagnostic> {
        self.seal_open_blocks()?;
        let (blocks, statements, predecessors) = self.materialize_blocks()?;
        let definition = &self.module.definitions[self.owner.definition.index()];
        if self
            .owner
            .expressions
            .iter()
            .any(|expression| expression.effects.0 & hir::Effects::UNSAFE != 0)
        {
            self.flags |= BodyFlags::UNSAFE;
        }
        debug_assert_eq!(self.flags & !BodyFlags::KNOWN, 0);
        Ok(GirBody {
            owner: self.owner.definition,
            owner_key: definition.key,
            kind: body_kind(definition.kind),
            signature: self.signature(),
            generic_params: definition.parameters.len() as u32,
            locals: self.locals,
            blocks,
            statements,
            predecessors,
            projections: self.projections,
            constants: self.constants,
            source_scopes: self.source_scopes,
            cleanup_regions: self.cleanup_regions,
            exit_records: self.exit_records,
            safepoints: self.safepoints,
            no_safepoint_regions: self.no_safepoint_regions,
            select_cases: self.select_cases,
            expression_locals: self.expression_locals,
            match_leaves: self.match_leaves,
            large_copies: self.large_copies,
            flags: self.flags,
            revision: GIR_REVISION,
            entry: BlockId(0),
        })
    }

    fn signature(&self) -> Signature {
        let result = self.result_ty();
        let args = self
            .owner
            .parameters
            .iter()
            .flat_map(|&pattern| binds(self.owner, pattern))
            .map(|local| self.owner.locals[local.index()].ty)
            .collect();
        let effects = self
            .owner
            .expressions
            .iter()
            .fold(0, |bits, expression| bits | expression.effects.0);
        Signature {
            parameters: args,
            result,
            effects,
        }
    }

    fn result_ty(&self) -> TypeId {
        let Some(signature) = self.module.definitions[self.owner.definition.index()].signature
        else {
            return self.owner.expression_types[self.owner.body.index()];
        };
        match &self.module.types[signature.index()] {
            hir::Type::Function { result, .. } => *result,
            hir::Type::Callable { signature, .. } => match &self.module.types[signature.index()] {
                hir::Type::Function { result, .. } => *result,
                _ => *signature,
            },
            _ => signature,
        }
    }

    fn allocate_locals(&mut self) -> Result<(), Diagnostic> {
        let result = self.result_ty();
        self.return_local = self.push_local(result, LocalKind::Return, false, None, false);
        let args = self
            .owner
            .parameters
            .iter()
            .flat_map(|&pattern| binds(self.owner, pattern))
            .collect::<Vec<_>>();
        self.hir_to_gir = vec![LocalId(0); self.owner.locals.len()];
        for local in &args {
            self.hir_to_gir[local.index()] = self.user_local(*local, LocalKind::Argument);
        }
        for index in 0..self.owner.locals.len() {
            if args.iter().any(|arg| arg.index() == index) {
                continue;
            }
            self.hir_to_gir[index] = self.user_local(hir::LocalId(index as u32), LocalKind::User);
        }
        for (index, cleanup) in self.owner.cleanup.iter().enumerate() {
            if cleanup.registration == hir::Registration::Flag {
                let flag = self.temp(self.primitives.bool_ty);
                self.cleanup_flags[index] = Some(flag);
            }
        }
        if self
            .owner
            .cleanup
            .iter()
            .any(|cleanup| cleanup.registration == hir::Registration::Chain)
        {
            self.chain_head = Some(self.temp(self.primitives.ptr_unit));
        }
        self.saved = (0..self.owner.cleanup.len())
            .map(|_| SavedCleanup::Body(hir::ExprId(0)))
            .collect();
        Ok(())
    }

    fn user_local(&mut self, hir_local: hir::LocalId, kind: LocalKind) -> LocalId {
        let local = &self.owner.locals[hir_local.index()];
        let pinned = local.storage & (CAPTURED | CROSS_COROUTINE) != 0;
        self.push_local(local.ty, kind, true, Some(hir_local), pinned)
    }

    fn push_local(
        &mut self,
        ty: TypeId,
        kind: LocalKind,
        mutable: bool,
        hir_local: Option<hir::LocalId>,
        pinned_storage: bool,
    ) -> LocalId {
        let id = LocalId(self.locals.len() as u32);
        let address_taken = hir_local
            .is_some_and(|local| self.owner.locals[local.index()].storage & ADDRESS_TAKEN != 0);
        self.locals.push(GirLocal {
            ty,
            kind,
            mutable,
            address_taken,
            source_scope: ScopeId(0),
            hir_local,
            pinned_storage,
        });
        self.live.push(false);
        self.written.push(matches!(kind, LocalKind::Argument));
        id
    }

    fn temp(&mut self, ty: TypeId) -> LocalId {
        let id = self.push_local(ty, LocalKind::Temporary, true, None, false);
        if !self.blocks.is_empty() && !self.terminated() {
            self.live_local(id);
        }
        id
    }

    fn live_entry_locals(&mut self) {
        let live_ids: Vec<LocalId> = (0..self.locals.len())
            .filter(|&index| {
                matches!(
                    self.locals[index].kind,
                    LocalKind::Return | LocalKind::Argument | LocalKind::User
                ) || self
                    .cleanup_flags
                    .iter()
                    .any(|flag| flag == &Some(LocalId(index as u32)))
                    || self.chain_head == Some(LocalId(index as u32))
            })
            .map(|index| LocalId(index as u32))
            .collect();
        for id in live_ids {
            self.live_local(id);
        }
        if let Some(head) = self.chain_head {
            let empty = self.intrinsic_temp(
                IntrinsicOp::DeferChainEmpty,
                Vec::new(),
                Vec::new(),
                self.primitives.ptr_unit,
            );
            self.assign(
                Place::local(head),
                Rvalue::Use(Operand::Copy(Place::local(empty))),
            );
        }
        for flag in self
            .cleanup_flags
            .iter()
            .flatten()
            .copied()
            .collect::<Vec<_>>()
        {
            self.assign_bool(flag, false);
        }
    }

    fn live_local(&mut self, local: LocalId) {
        if self.live[local.index()] {
            return;
        }
        self.push_stmt(StatementKind::StorageLive(local));
        self.live[local.index()] = true;
    }

    fn dead_local(&mut self, local: LocalId) {
        if !self.live[local.index()] || self.locals[local.index()].pinned_storage {
            return;
        }
        self.release_local(local);
        self.push_stmt(StatementKind::StorageDead(local));
        self.live[local.index()] = false;
    }

    fn dead_unpinned(&mut self) {
        let ids: Vec<_> = self
            .locals
            .iter()
            .enumerate()
            .filter(|(_, local)| {
                matches!(
                    local.kind,
                    LocalKind::Return | LocalKind::Argument | LocalKind::User
                )
            })
            .map(|(index, _)| LocalId(index as u32))
            .collect();
        for id in ids {
            self.dead_local(id);
        }
    }

    fn fill_return_block(&mut self) {
        let current = self.current;
        self.current = self.return_block;
        self.dead_unpinned();
        self.terminate(Terminator::Return);
        self.current = current;
    }

    fn fresh(&mut self, cleanup: bool) -> BlockId {
        let id = BlockId(self.blocks.len() as u32);
        self.blocks.push(PendingBlock {
            statements: Vec::new(),
            terminator: None,
            source: self.source_of_scope(hir::ScopeId(0)),
            cleanup,
        });
        id
    }

    fn terminated(&self) -> bool {
        self.blocks
            .get(self.current.index())
            .is_some_and(|block| block.terminator.is_some())
    }

    fn switch_to(&mut self, block: BlockId) {
        self.current = block;
    }

    fn goto(&mut self, target: BlockId) {
        self.terminate(Terminator::Goto { target });
    }

    fn terminate(&mut self, terminator: Terminator) {
        if self.terminated() {
            return;
        }
        self.record_effects(&terminator);
        self.blocks[self.current.index()].terminator = Some(terminator);
    }

    fn record_effects(&mut self, terminator: &Terminator) {
        match terminator {
            Terminator::Panic { .. } => self.flags |= BodyFlags::PANIC,
            Terminator::Suspend { .. } | Terminator::SelectCommit { .. } => {
                self.flags |= BodyFlags::SUSPEND;
            }
            Terminator::Call {
                call_kind:
                    CallKind::ForeignBridge
                    | CallKind::ForeignBridgeDirtyCpu
                    | CallKind::ForeignLeaf { .. },
                ..
            } => self.flags |= BodyFlags::FOREIGN,
            _ => {}
        }
    }

    fn push_stmt(&mut self, kind: StatementKind) {
        if self.terminated() {
            return;
        }
        if matches!(
            kind,
            StatementKind::NoSafepointBegin(_) | StatementKind::NoSafepointEnd(_)
        ) {
            self.flags |= BodyFlags::RUNTIME_GLUE;
        }
        let source = self.blocks[self.current.index()].source.clone();
        self.blocks[self.current.index()]
            .statements
            .push(Statement { kind, source });
    }

    fn assign(&mut self, place: Place, rvalue: Rvalue) {
        if matches!(
            rvalue,
            Rvalue::AllocObject { .. } | Rvalue::AllocArray { .. }
        ) {
            self.flags |= BodyFlags::ALLOCATE;
        }
        self.push_stmt(StatementKind::Assign(place, rvalue));
    }

    fn assign_copy(&mut self, dest: Place, src: LocalId) {
        self.copy_value(dest, Place::local(src), self.locals[src.index()].ty);
    }

    fn assign_unit(&mut self, local: LocalId) {
        let constant = self.intern_const(self.primitives.unit, ConstValue::Unit);
        self.assign(
            Place::local(local),
            Rvalue::Use(Operand::Constant(constant)),
        );
    }

    fn assign_bool(&mut self, local: LocalId, value: bool) {
        let constant = self.intern_const(self.primitives.bool_ty, ConstValue::Bool(value));
        self.assign(
            Place::local(local),
            Rvalue::Use(Operand::Constant(constant)),
        );
    }

    fn intern_const(&mut self, ty: TypeId, value: ConstValue) -> ConstId {
        if let Some((index, _)) = self
            .constants
            .iter()
            .enumerate()
            .find(|(_, constant)| constant.ty == ty && constant.value == value)
        {
            return ConstId(index as u32);
        }
        let id = ConstId(self.constants.len() as u32);
        self.constants.push(Constant { ty, value });
        id
    }

    fn const_operand(&mut self, ty: TypeId, value: ConstValue) -> Operand {
        Operand::Constant(self.intern_const(ty, value))
    }

    fn expr_ty(&self, id: ExprId) -> TypeId {
        self.owner.expression_types[id.index()]
    }

    fn expr_scope(&self, id: ExprId) -> hir::ScopeId {
        self.owner.expressions[id.index()].scope
    }

    fn source_of(&self, id: ExprId) -> SourceInfo {
        let expression = &self.owner.expressions[id.index()];
        SourceInfo {
            location: expression.location.clone(),
            scope: ScopeId(expression.scope.0),
        }
    }

    fn source_of_scope(&self, scope: hir::ScopeId) -> SourceInfo {
        let location = self.owner.scopes[scope.index()].location.clone();
        SourceInfo {
            location,
            scope: ScopeId(scope.0),
        }
    }

    fn set_value(&mut self, id: ExprId, local: LocalId) {
        self.expression_locals[id.index()] = Some(local);
        self.blocks[self.current.index()].source = self.source_of(id);
    }

    fn set_place(&mut self, id: ExprId, place: Place) {
        self.expression_places[id.index()] = Some(place);
        if place.is_local() {
            self.expression_locals[id.index()] = Some(place.local);
        }
        self.blocks[self.current.index()].source = self.source_of(id);
    }

    fn value_of(&mut self, id: ExprId) -> Result<Option<LocalId>, Diagnostic> {
        if let Some(local) = self.expression_locals[id.index()] {
            return Ok(Some(local));
        }
        if let Some(place) = self.expression_places[id.index()] {
            let local = self.temp(self.expr_ty(id));
            self.copy_value(Place::local(local), place, self.expr_ty(id));
            self.expression_locals[id.index()] = Some(local);
            return Ok(Some(local));
        }
        Ok(None)
    }

    fn require_value(&mut self, id: ExprId) -> Result<LocalId, Diagnostic> {
        self.value_of(id)?
            .ok_or_else(|| gir_error("表达式没有可复制的值", Some(&self.source_of(id).location)))
    }

    fn project(&mut self, base: Place, projection: Projection) -> Place {
        let start = self.projections.len() as u32;
        let existing = self.projections[base.range()].to_vec();
        self.projections.extend(existing);
        self.projections.push(projection);
        Place {
            local: base.local,
            projections: (start, self.projections.len() as u32),
        }
    }

    fn intrinsic_temp(
        &mut self,
        op: IntrinsicOp,
        operands: Vec<Operand>,
        types: Vec<TypeId>,
        ty: TypeId,
    ) -> LocalId {
        let local = self.temp(ty);
        self.assign(
            Place::local(local),
            Rvalue::Intrinsic {
                op,
                operands,
                types,
            },
        );
        local
    }

    fn unwind_of(&self, scope: hir::ScopeId) -> u32 {
        self.owner.scopes[scope.index()].unwind_plan
    }

    fn current_unwind(&self, id: ExprId) -> u32 {
        self.unwind_of(self.expr_scope(id))
    }

    fn panic_string(&mut self, text: &str) -> Operand {
        self.const_operand(self.string_ty(), ConstValue::String(text.to_owned()))
    }

    fn string_ty(&self) -> TypeId {
        self.module
            .types
            .iter()
            .position(|ty| matches!(ty, hir::Type::String))
            .map(|index| TypeId(index as u32))
            .unwrap_or(self.primitives.unit)
    }

    fn int_ty(&self) -> TypeId {
        self.module
            .types
            .iter()
            .find_map(|ty| match ty {
                hir::Type::Int {
                    signed: true,
                    bits: 64,
                } => Some(TypeId(
                    self.module
                        .types
                        .iter()
                        .position(|item| item == ty)
                        .unwrap() as u32,
                )),
                _ => None,
            })
            .or_else(|| {
                self.module
                    .types
                    .iter()
                    .enumerate()
                    .find_map(|(index, ty)| {
                        matches!(ty, hir::Type::Int { .. }).then_some(TypeId(index as u32))
                    })
            })
            .unwrap_or(self.primitives.unit)
    }

    fn seal_open_blocks(&mut self) -> Result<(), Diagnostic> {
        for block in &mut self.blocks {
            if block.terminator.is_none() {
                block.terminator = Some(Terminator::Unreachable);
            }
        }
        Ok(())
    }

    fn materialize_blocks(
        &self,
    ) -> Result<(Vec<GirBlock>, Vec<Statement>, Vec<BlockId>), Diagnostic> {
        let mut incoming = vec![Vec::new(); self.blocks.len()];
        for (index, block) in self.blocks.iter().enumerate() {
            let Some(terminator) = &block.terminator else {
                return Err(gir_error("GIR block 缺少终结符", None));
            };
            for successor in terminator.successors() {
                incoming[successor.index()].push(BlockId(index as u32));
            }
        }
        let mut statements = Vec::new();
        let mut predecessors = Vec::new();
        let mut blocks = Vec::with_capacity(self.blocks.len());
        for (index, block) in self.blocks.iter().enumerate() {
            let start = statements.len() as u32;
            statements.extend(block.statements.iter().cloned());
            let pred_start = predecessors.len() as u32;
            predecessors.extend(incoming[index].iter().copied());
            blocks.push(GirBlock {
                statements: start..statements.len() as u32,
                terminator: block.terminator.clone().expect("已补终结符"),
                source: block.source.clone(),
                predecessors: pred_start..predecessors.len() as u32,
                cleanup: block.cleanup,
            });
        }
        Ok((blocks, statements, predecessors))
    }
}

fn copy_of(local: LocalId) -> Operand {
    Operand::Copy(Place::local(local))
}

fn binds(owner: &hir::Owner, pattern: hir::PatternId) -> Vec<hir::LocalId> {
    let mut out = Vec::new();
    collect_binds(owner, pattern, &mut out);
    out
}

fn collect_binds(owner: &hir::Owner, pattern: hir::PatternId, out: &mut Vec<hir::LocalId>) {
    match &owner.patterns[pattern.index()].kind {
        hir::PatternKind::Bind(local) | hir::PatternKind::At { local, .. } => out.push(*local),
        hir::PatternKind::Ref(inner) => collect_binds(owner, *inner, out),
        hir::PatternKind::Tuple(range) | hir::PatternKind::Or(range) => {
            for id in &owner.pattern_ids[range.start as usize..range.end as usize] {
                collect_binds(owner, *id, out);
            }
        }
        hir::PatternKind::Array {
            prefix,
            rest,
            suffix,
            ..
        } => {
            for id in &owner.pattern_ids[prefix.start as usize..prefix.end as usize] {
                collect_binds(owner, *id, out);
            }
            if let Some(local) = rest {
                out.push(*local);
            }
            for id in &owner.pattern_ids[suffix.start as usize..suffix.end as usize] {
                collect_binds(owner, *id, out);
            }
        }
        hir::PatternKind::Construct { fields, .. } => {
            for field in &owner.pattern_fields[fields.start as usize..fields.end as usize] {
                collect_binds(owner, field.pattern, out);
            }
        }
        hir::PatternKind::Wildcard
        | hir::PatternKind::Literal(_)
        | hir::PatternKind::Range { .. } => {}
    }
    if let hir::PatternKind::At { pattern, .. } = &owner.patterns[pattern.index()].kind {
        collect_binds(owner, *pattern, out);
    }
}

fn expr_range(owner: &hir::Owner, range: &Range<u32>) -> Vec<ExprId> {
    owner.expression_ids[range.start as usize..range.end as usize].to_vec()
}
