//! Physical moves, rematerialization and scratch selection.
use super::*;

pub(super) fn emit_moves(
    emit: &Emit<'_>,
    builder: &mut Builder,
    moves: &[EmittedMove],
) -> Result<(), AllocError> {
    for mov in moves {
        match *mov {
            EmittedMove::Move { src, dst, ty } => {
                emit_loc_move(emit, builder, src, dst, ty)?;
            }
            EmittedMove::Rematerialize { value, dest } => {
                if dest.register().is_some() {
                    emit.rematerialize(builder, value, dest)?;
                } else {
                    let ty = emit.body.values[value as usize].kind.ty;
                    let scratch = match RegClass::of(ty) {
                        RegClass::Gpr => Loc::Gpr(Gpr::R11),
                        RegClass::Xmm => Loc::Xmm(Xmm::Xmm15),
                    };
                    emit.rematerialize(builder, value, scratch)?;
                    emit_loc_move(emit, builder, scratch, dest, ty)?;
                }
            }
        }
    }
    Ok(())
}

/// 一条 `Loc → Loc` 搬运。
fn emit_loc_move(
    emit: &Emit<'_>,
    builder: &mut Builder,
    src: Loc,
    dst: Loc,
    ty: Type,
) -> Result<(), AllocError> {
    if src == dst {
        return Ok(());
    }
    let float = matches!(ty, Type::F32 | Type::F64 | Type::V128(_));
    match (src, dst) {
        (Loc::Gpr(left), Loc::Gpr(right)) => {
            if float {
                return Err(AllocError::new(format!(
                    "整数寄存器搬运使用了浮点类型 {ty:?}：{left:?} → {right:?}"
                )));
            }
            lower::value_move(builder, Reg::Gpr(left), Reg::Gpr(right), ty).map_err(lowering_error)
        }
        (Loc::Xmm(left), Loc::Xmm(right)) => {
            if !float {
                return Err(AllocError::new(format!(
                    "XMM 寄存器搬运使用了整数类型 {ty:?}：{left:?} → {right:?}"
                )));
            }
            lower::value_move(builder, Reg::Xmm(left), Reg::Xmm(right), ty).map_err(lowering_error)
        }
        (Loc::Gpr(gpr), Loc::Slot(slot)) => {
            let offset = emit.frame.spill_offset(slot)?;
            emit.count_stores(1);
            lower::store_stack(builder, Reg::Gpr(gpr), offset, ty).map_err(lowering_error)
        }
        (Loc::Xmm(xmm), Loc::Slot(slot)) => {
            let offset = emit.frame.spill_offset(slot)?;
            emit.count_stores(1);
            lower::store_stack(builder, Reg::Xmm(xmm), offset, ty).map_err(lowering_error)
        }
        (Loc::Slot(slot), Loc::Gpr(gpr)) => {
            let offset = emit.frame.spill_offset(slot)?;
            emit.count_reloads(1);
            lower::load_stack(builder, offset, Reg::Gpr(gpr), ty).map_err(lowering_error)
        }
        (Loc::Slot(slot), Loc::Xmm(xmm)) => {
            let offset = emit.frame.spill_offset(slot)?;
            emit.count_reloads(1);
            lower::load_stack(builder, offset, Reg::Xmm(xmm), ty).map_err(lowering_error)
        }
        (Loc::Gpr(gpr), Loc::Scratch) => {
            let offset = emit.scratch_offset()?;
            lower::store_stack(builder, Reg::Gpr(gpr), offset, ty).map_err(lowering_error)
        }
        (Loc::Xmm(xmm), Loc::Scratch) => {
            let offset = emit.scratch_offset()?;
            lower::store_stack(builder, Reg::Xmm(xmm), offset, ty).map_err(lowering_error)
        }
        (Loc::Scratch, Loc::Gpr(gpr)) => {
            let offset = emit.scratch_offset()?;
            lower::load_stack(builder, offset, Reg::Gpr(gpr), ty).map_err(lowering_error)
        }
        (Loc::Scratch, Loc::Xmm(xmm)) => {
            let offset = emit.scratch_offset()?;
            lower::load_stack(builder, offset, Reg::Xmm(xmm), ty).map_err(lowering_error)
        }
        (Loc::Slot(_) | Loc::Scratch, Loc::Slot(_) | Loc::Scratch) => {
            // 内存到内存：经 `r11` 搬机器字（16 字节值搬两个字）。
            let bytes = if matches!(ty, Type::V128(_)) { 16 } else { 8 };
            let from = emit.loc_offset(src)?;
            let to = emit.loc_offset(dst)?;
            let words = bytes / 8;
            emit.count_reloads(words);
            emit.count_stores(words);
            let mut at = 0_u32;
            while at < bytes {
                lower::load_stack(builder, from + at, Reg::Gpr(Gpr::R11), Type::I64)
                    .map_err(lowering_error)?;
                lower::store_stack(builder, Reg::Gpr(Gpr::R11), to + at, Type::I64)
                    .map_err(lowering_error)?;
                at += 8;
            }
            Ok(())
        }
        (Loc::Gpr(_) | Loc::Xmm(_), Loc::Gpr(_) | Loc::Xmm(_)) => {
            Err(AllocError::new("寄存器搬运两侧 bank 不一致"))
        }
    }
}

impl Emit<'_> {
    /// 值的位置。
    pub fn value_location(&self, index: usize) -> Result<Loc, AllocError> {
        let value = self
            .values
            .get(index)
            .ok_or_else(|| AllocError::new("值编号越过分配结果"))?;
        value
            .location()
            .map(frame::loc_of)
            .ok_or_else(|| AllocError::new(format!("值 v{index} 没有机器位置")))
    }

    /// 发射重发单元（frame 阶段合成 prologue 时复用）。
    pub fn emit_moves(
        &self,
        builder: &mut Builder,
        moves: &[EmittedMove],
    ) -> Result<(), AllocError> {
        emit_moves(self, builder, moves)
    }

    /// 内存位置偏移。
    pub(super) fn loc_offset(&self, loc: Loc) -> Result<u32, AllocError> {
        match loc {
            Loc::Slot(slot) => self.frame.spill_offset(slot),
            Loc::Scratch => self.scratch_offset(),
            Loc::Gpr(_) | Loc::Xmm(_) => Err(AllocError::new("寄存器位置没有内存偏移")),
        }
    }

    /// 记 `count` 条 spill 重载。
    pub(super) fn count_reloads(&self, count: u32) {
        let traffic = self.traffic.get();
        self.traffic.set(SpillTraffic {
            reloads: traffic.reloads.saturating_add(count),
            ..traffic
        });
    }

    /// 记 `count` 条 spill 写回。
    pub(super) fn count_stores(&self, count: u32) {
        let traffic = self.traffic.get();
        self.traffic.set(SpillTraffic {
            stores: traffic.stores.saturating_add(count),
            ..traffic
        });
    }

    /// 记一次常量重建。
    pub(super) fn count_rematerialization(&self) {
        let traffic = self.traffic.get();
        self.traffic.set(SpillTraffic {
            rematerializations: traffic.rematerializations.saturating_add(1),
            ..traffic
        });
    }

    /// copy scratch 偏移。
    pub(super) fn scratch_offset(&self) -> Result<u32, AllocError> {
        self.frame
            .scratch_offset()
            .ok_or_else(|| AllocError::new("并行拷贝环需要 copy scratch，但 frame 未预留"))
    }

    /// 按值装载：源是 spill slot 或 scratch。
    pub(super) fn emit_load_place(
        &self,
        builder: &mut Builder,
        offset: u32,
        dest: Loc,
        class: RegClass,
    ) {
        match (class, dest) {
            (_, Loc::Gpr(gpr)) => {
                lower::load_stack(builder, offset, Reg::Gpr(gpr), Type::I64).expect("载入可编码");
            }
            (_, Loc::Xmm(xmm)) => {
                lower::load_stack(builder, offset, Reg::Xmm(xmm), Type::F64).expect("载入可编码");
            }
            (_, Loc::Slot(_) | Loc::Scratch) => {}
        }
    }

    /// 按值存储：目标是 spill slot 或 scratch。
    pub(super) fn emit_store_place(
        &self,
        builder: &mut Builder,
        src: Loc,
        offset: u32,
        class: RegClass,
    ) {
        match (class, src) {
            (_, Loc::Gpr(gpr)) => {
                lower::store_stack(builder, Reg::Gpr(gpr), offset, Type::I64).expect("可编码");
            }
            (_, Loc::Xmm(xmm)) => {
                lower::store_stack(builder, Reg::Xmm(xmm), offset, Type::F64).expect("可编码");
            }
            (_, Loc::Slot(_) | Loc::Scratch) => {}
        }
    }

    /// 通用载入：把 `[rsp + offset]` 读进位置（目标不是寄存器时经 `r11`）。
    pub fn emit_load(
        &self,
        builder: &mut Builder,
        offset: u32,
        dest: Loc,
        ty: Type,
    ) -> Result<(), AllocError> {
        match dest {
            Loc::Gpr(gpr) => {
                lower::load_stack(builder, offset, Reg::Gpr(gpr), ty).map_err(lowering_error)
            }
            Loc::Xmm(xmm) => {
                lower::load_stack(builder, offset, Reg::Xmm(xmm), ty).map_err(lowering_error)
            }
            Loc::Slot(_) | Loc::Scratch => {
                let from = offset;
                let to = self.loc_offset(dest)?;
                let bytes = if matches!(ty, Type::V128(_)) { 16 } else { 8 };
                let mut at = 0_u32;
                while at < bytes {
                    lower::load_stack(builder, from + at, Reg::Gpr(Gpr::R11), Type::I64)
                        .map_err(lowering_error)?;
                    lower::store_stack(builder, Reg::Gpr(Gpr::R11), to + at, Type::I64)
                        .map_err(lowering_error)?;
                    at += 8;
                }
                Ok(())
            }
        }
    }

    /// 重建可重建的值。
    pub(super) fn rematerialize(
        &self,
        builder: &mut Builder,
        value: u32,
        dest: Loc,
    ) -> Result<(), AllocError> {
        let definition = self
            .body
            .values
            .get(value as usize)
            .ok_or_else(|| AllocError::new("重建引用未登记的值"))?;
        let Definition::Instruction { instruction, .. } = definition.definition else {
            return Err(AllocError::new("只有指令定义的值可以重建"));
        };
        let site = u32::try_from(instruction.index()).unwrap_or(u32::MAX);
        let instruction = &self.body.instructions[instruction.index()];
        let op = instruction.op.clone();
        if !matches!(
            op,
            Op::IConst(_) | Op::FConst(_) | Op::SymbolAddr(_) | Op::StackAddr(_)
        ) {
            return Err(AllocError::new("只有常量/symbol/stack 地址可以重建"));
        }
        let operands: Vec<SiteValue> = self
            .body
            .args(&instruction.arguments)
            .iter()
            .map(|value| SiteValue {
                ty: self.body.values[value.index()].kind,
                reg: Reg::Virtual(value.0),
            })
            .collect();
        let results = vec![SiteValue {
            ty: definition.kind,
            reg: reg_of(dest)?,
        }];
        let ctx = LowerCtx {
            target: self.target,
            body: Some(self.body),
            universe: None,
            raw: None,
            site,
        };
        let lowered = lower::lower_with(&op, &operands, &results, &instruction.source, ctx)
            .map_err(|error| AllocError::new(format!("重建常量失败：{error}")))?;
        if !lowered.copy_groups.is_empty() {
            return Err(AllocError::new("重建序列不应包含并行拷贝组"));
        }
        // `StackAddr` 的重建序列同样带 frame 占位基址，必须当场换算成 `[rsp + offset]`。
        let mut sequence = lowered.sequence;
        for inst in &mut sequence.instructions {
            for operand in &mut inst.operands {
                if let Operand::Mem(mem) = operand
                    && let Some(Reg::Virtual(id)) = mem.base
                    && id >= FRAME_SLOT_BASE
                {
                    let slot =
                        usize::try_from(id - FRAME_SLOT_BASE).expect("frame 槽下标适配 usize");
                    let local = self
                        .frame
                        .local(slot)
                        .ok_or_else(|| AllocError::new(format!("frame 局部槽 {slot} 不存在")))?;
                    mem.base = Some(Reg::Gpr(Gpr::Rsp));
                    mem.disp = i32::try_from(
                        i64::from(local.offset)
                            .checked_add(i64::from(mem.disp))
                            .ok_or_else(|| AllocError::new("局部槽位移溢出"))?,
                    )
                    .map_err(|_| AllocError::new("局部槽位移超出 i32"))?;
                }
            }
        }
        // 重建只允许写 `r11`（lowering 的固定 scratch）与目标位置本身。
        let allowed = loc_register(dest);
        for inst in &sequence.instructions {
            let form = table::form(inst.form);
            for (position, operand) in inst.operands.iter().enumerate() {
                if !matches!(
                    form.access.get(position),
                    Some(Access::Write | Access::ReadWrite)
                ) {
                    continue;
                }
                if let Operand::Reg(register) = operand
                    && *register != Reg::Gpr(Gpr::R11)
                    && Some(*register) != allowed
                {
                    return Err(AllocError::new(format!(
                        "重建序列写入了非 scratch 寄存器 {register:?}"
                    )));
                }
            }
        }
        builder.extend(sequence);
        self.count_rematerialization();
        Ok(())
    }

    /// 该点位空闲且可用的 scratch。
    pub(super) fn choose_scratch(
        &self,
        point: u32,
        class: RegClass,
        kind: OperandKind,
        forbidden: &Clobbers,
    ) -> Result<Loc, AllocError> {
        let byte_register = matches!(kind, OperandKind::Rm8 | OperandKind::R8);
        let occupied = self.occupied(point);
        let mask = self
            .points
            .get(point as usize)
            .map_or(Clobbers::NONE, |point| point.mask);
        match class {
            RegClass::Gpr => {
                let candidates = std::iter::once(Gpr::R11).chain(super::super::scan::GPR_POOL);
                candidates
                    .into_iter()
                    .find(|gpr| {
                        self.scratch_gpr.gpr & gpr.bit() != 0
                            && mask.gpr & gpr.bit() == 0
                            && occupied.gpr & gpr.bit() == 0
                            && forbidden.gpr & gpr.bit() == 0
                            && (!byte_register || gpr.is_byte_encodable())
                    })
                    .map(Loc::Gpr)
                    .ok_or_else(|| {
                        AllocError::new(format!(
                            "指令需要 scratch 寄存器，但该点位没有空闲寄存器（pool={:#06x} mask={:#06x} occupied={:#06x} forbidden={:#06x}）",
                            self.scratch_gpr.gpr, mask.gpr, occupied.gpr, forbidden.gpr,
                        ))
                    })
            }
            RegClass::Xmm => super::super::scan::XMM_POOL
                .into_iter()
                .find(|xmm| {
                    mask.xmm & xmm.bit() == 0
                        && occupied.xmm & xmm.bit() == 0
                        && forbidden.xmm & xmm.bit() == 0
                })
                .map(Loc::Xmm)
                .ok_or_else(|| AllocError::new("指令需要 XMM scratch，但该点位没有空闲寄存器")),
        }
    }

    /// 该点位被活跃值占用的寄存器。
    pub(super) fn occupied(&self, point: u32) -> Clobbers {
        let mut occupied = Clobbers::NONE;
        for value in self.values {
            if !value.is_live() || !super::super::live::range_covers_point(value.range, point) {
                continue;
            }
            match value.location() {
                Some(Location::Gpr(gpr)) => occupied = occupied.union(Clobbers::gpr(gpr)),
                Some(Location::Xmm(xmm)) => occupied = occupied.union(Clobbers::xmm(xmm)),
                Some(Location::Slot(_)) | None => {}
            }
        }
        occupied
    }
}
