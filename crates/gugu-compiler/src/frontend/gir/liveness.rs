//! generic GIR 上的 turn 边界、局部活跃性与 may-suspend 不动点。
//!
//! 三个判定的输入都是 `BuildGenericGir` 的 generic body，输出只服务
//! `EscapeAndPlacement`：TurnRegion 候选必须证明「没有任何 region 派生值在 turn 边界上活跃」。
//! HIR 的 `Effects::SUSPEND` 不能用于该判定——`call_plans` 对任何普通调用都保守地置位
//! SUSPEND，因此这里在 generic 调用图上做结构化不动点。
//!
//! 活跃位集按 block 展开为 `u64` 词数组：单个 body 的 local 数量很小，访问模式是逐 block
//! 的全量迭代与位运算，位集比 `BTreeSet` 少掉全部堆分配与元素比较。

use std::collections::BTreeSet;

use super::body::{BlockId, Callee, GirBody, LocalId, Operand, Place, SelectOperation, Terminator};
use super::pass::{statement_defined, statement_reads};

/// 一个 body 的局部活跃位集，按 block 稠密编号索引。
///
/// 位宽是 `locals`，词数 `ceil(locals / 64)`；`live_in`/`live_out` 覆盖全部 block，
/// `terminator_use` 记录 terminator 自身在出口处读取的局部。
pub(crate) struct Liveness {
    locals: usize,
    words: usize,
    live_in: Vec<u64>,
    live_out: Vec<u64>,
}

impl Liveness {
    /// 在 generic body 上求后向活跃性固定点。
    pub(crate) fn analyze(body: &GirBody) -> Self {
        let locals = body.locals.len();
        let words = locals.div_ceil(64);
        let blocks = body.blocks.len();
        let mut analysis = Self {
            locals,
            words,
            live_in: vec![0; blocks * words],
            live_out: vec![0; blocks * words],
        };
        let mut reads = BTreeSet::new();
        let mut use_set = vec![0_u64; blocks * words];
        let mut def_set = vec![0_u64; blocks * words];
        for (index, block) in body.blocks.iter().enumerate() {
            for statement in body.block_statements(BlockId(index as u32)) {
                reads.clear();
                statement_reads(statement, &mut reads);
                if let Some(defined) = statement_defined(statement) {
                    // 直接写整个 local 时，写入之前的读取不计入 use；带投影的写入仍然读取基址。
                    for &read in &reads {
                        if read != defined {
                            set_bit(
                                &mut use_set,
                                index * words + read.index() / 64,
                                read.index(),
                            );
                        }
                    }
                    set_bit(
                        &mut def_set,
                        index * words + defined.0 as usize / 64,
                        defined.0 as usize,
                    );
                } else {
                    for &read in &reads {
                        set_bit(
                            &mut use_set,
                            index * words + read.index() / 64,
                            read.index(),
                        );
                    }
                    // 带投影的写入读取基址 local，基址在语句之前必须活跃。
                    for place in statement_places(&statement.kind) {
                        if !place.is_local() {
                            set_bit(
                                &mut use_set,
                                index * words + place.local.index() / 64,
                                place.local.index(),
                            );
                        }
                    }
                }
            }
            let mut terminator_reads = BTreeSet::new();
            terminator_use_at(body, &block.terminator, &mut terminator_reads);
            for &read in &terminator_reads {
                set_bit(
                    &mut use_set,
                    index * words + read.index() / 64,
                    read.index(),
                );
            }
            for defined in terminator_defs(body, &block.terminator) {
                set_bit(
                    &mut def_set,
                    index * words + defined.index() / 64,
                    defined.index(),
                );
            }
        }
        let mut changed = true;
        while changed {
            changed = false;
            for (index, block) in body.blocks.iter().enumerate() {
                for successor in block.terminator.successors() {
                    let target = successor.index();
                    let mut merged = false;
                    for word in 0..words {
                        let incoming = analysis.live_in[target * words + word];
                        let slot = &mut analysis.live_out[index * words + word];
                        let next = *slot | incoming;
                        if next != *slot {
                            *slot = next;
                            merged = true;
                        }
                    }
                    if merged {
                        changed = true;
                    }
                }
                for word in 0..words {
                    let live_out = analysis.live_out[index * words + word];
                    let defined = def_set[index * words + word];
                    let next = use_set[index * words + word] | (live_out & !defined);
                    let slot = &mut analysis.live_in[index * words + word];
                    if next != *slot {
                        *slot = next;
                        changed = true;
                    }
                }
            }
        }
        analysis
    }

    /// 局部在 block 出口（terminator 之前）是否仍然活跃。
    pub(crate) fn live_out(&self, block: BlockId, local: LocalId) -> bool {
        self.bit(&self.live_out, block, local)
    }

    fn bit(&self, table: &[u64], block: BlockId, local: LocalId) -> bool {
        let index = local.index();
        if index >= self.locals {
            return false;
        }
        let word = block.index() * self.words + index / 64;
        table
            .get(word)
            .is_some_and(|slot| slot >> (index % 64) & 1 == 1)
    }
}

/// 一个 turn 边界：region 的存活区间不允许跨过它。
///
/// 可能挂起的调用不是 region 的 reset 点：region 对象在调用期间继续存活是安全的，安全边界由
/// LIR 在「可能挂起的调用收到 region 派生实参」处用 `PromoteManaged` 保证。因此这里只登记
/// 直接挂起点与本 body 出口。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TurnBoundaryKind {
    /// 本 body 的直接挂起点。
    DirectSuspend,
    /// 本 body 的出口（正常返回、展开、终止或 panic）。
    Exit,
}

/// 一个 turn 边界所在的 block。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct TurnBoundary {
    pub(crate) block: BlockId,
    pub(crate) kind: TurnBoundaryKind,
}

/// 返回 body 内的直接挂起点与出口边界。
pub(crate) fn turn_boundaries(body: &GirBody) -> Vec<TurnBoundary> {
    let mut out = Vec::new();
    for (index, block) in body.blocks.iter().enumerate() {
        let kind = match &block.terminator {
            Terminator::Suspend { .. } => TurnBoundaryKind::DirectSuspend,
            Terminator::SelectCommit { suspend, .. } if suspend.is_some() => {
                TurnBoundaryKind::DirectSuspend
            }
            Terminator::Panic { .. }
            | Terminator::Return
            | Terminator::ResumePanic
            | Terminator::Abort
            | Terminator::Unreachable => TurnBoundaryKind::Exit,
            _ => continue,
        };
        out.push(TurnBoundary {
            block: BlockId(index as u32),
            kind,
        });
    }
    out
}

/// 一条语句读写的局部 place（用于补齐带投影写入的基址读取）。
fn statement_places(kind: &super::body::StatementKind) -> Vec<Place> {
    use super::body::StatementKind;
    match kind {
        StatementKind::Assign(place, _) | StatementKind::SetDiscriminant { place, .. } => {
            vec![*place]
        }
        StatementKind::ValueAction { place, .. } | StatementKind::ResourceAction { place, .. } => {
            vec![*place]
        }
        _ => Vec::new(),
    }
}

/// terminator 读取的局部。
fn terminator_use_at(body: &GirBody, terminator: &Terminator, out: &mut BTreeSet<LocalId>) {
    let operand = |operand: &Operand, out: &mut BTreeSet<LocalId>| {
        if let Operand::Copy(place) | Operand::MoveInternal(place) = operand {
            out.insert(place.local);
        }
    };
    match terminator {
        Terminator::SwitchInt { value, .. } => operand(value, out),
        Terminator::Call { callee, args, .. } => {
            if let Callee::Value(value) = callee {
                operand(value, out);
            }
            for argument in args {
                operand(argument, out);
            }
        }
        Terminator::Panic { payload, .. } => operand(payload, out),
        Terminator::Suspend { reason, .. } => match reason {
            super::body::SuspendReason::ChanSend { channel, value } => {
                operand(channel, out);
                operand(value, out);
            }
            super::body::SuspendReason::ChanRecv { channel } => operand(channel, out),
            super::body::SuspendReason::JoinWait { join } => operand(join, out),
            super::body::SuspendReason::Yield => {}
        },
        Terminator::SelectCommit { cases, index, .. } => {
            out.insert(*index);
            for case in &body.select_cases[cases.start as usize..cases.end as usize] {
                match &case.operation {
                    SelectOperation::Send { channel, value } => {
                        operand(channel, out);
                        operand(value, out);
                    }
                    SelectOperation::Recv { channel } => operand(channel, out),
                    SelectOperation::Wait { join } => operand(join, out),
                }
            }
        }
        Terminator::Goto { .. }
        | Terminator::Return
        | Terminator::ResumePanic
        | Terminator::Abort
        | Terminator::Unreachable => {}
    }
}

/// terminator 定义的局部。
fn terminator_defs(body: &GirBody, terminator: &Terminator) -> Vec<LocalId> {
    let mut out = Vec::new();
    match terminator {
        Terminator::Call { destination, .. } if destination.is_local() => {
            out.push(destination.local);
        }
        Terminator::Suspend { destination, .. } => {
            if let Some(place) = destination
                && place.is_local()
            {
                out.push(place.local);
            }
        }
        Terminator::SelectCommit { cases, .. } => {
            for case in &body.select_cases[cases.start as usize..cases.end as usize] {
                if let Some(place) = case.destination
                    && place.is_local()
                {
                    out.push(place.local);
                }
            }
        }
        _ => {}
    }
    out
}

/// 置位一个位集中的位；越界（`callsite` 新 local）不影响分析结果。
fn set_bit(table: &mut [u64], word: usize, index: usize) {
    if let Some(slot) = table.get_mut(word) {
        *slot |= 1 << (index % 64);
    }
}
