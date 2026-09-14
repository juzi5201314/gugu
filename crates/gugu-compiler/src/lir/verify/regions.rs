use super::{Graph, edges, invalid};
use crate::Diagnostic;
use crate::frontend::gir::body::{CallKind, ViewMode};
use crate::lir::body::{BlockId, Body, Op, Origin, Terminator, ValueId, id, range};
use crate::runtime::barrier_schema::CARD_MARK_BUFFER_ENTRIES;
use std::collections::{BTreeMap, VecDeque};

#[derive(Clone, Copy, Eq, PartialEq)]
pub(super) enum Mode {
    Structure,
    Complete,
}

/// region 的 block 归属；raw publish verifier 用它定位 region 的指令窗口。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct Layout {
    /// region 编号 → block 是否属于该 region。
    pub(super) memberships: Vec<Vec<bool>>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct State {
    regions: Vec<u32>,
    views: BTreeMap<u32, (ValueId, ViewMode)>,
    shared: Vec<u32>,
    permits: Vec<Option<PermitState>>,
}

/// 一条路径上某个 permit 的剩余额度与已消费的 card 键。
///
/// `seen` 按值编号升序且去重：同一地址在同一 region 内只需一个 card-mark slot，因此只有
/// 首次出现的地址才扣除 card 额度。合流边取 `min` 剩余额度、取 `seen` 的交集，保证任何
/// 执行路径都不会超出 compile-time 额度。
#[derive(Clone, Debug, Eq, PartialEq)]
struct PermitState {
    shades: u32,
    cards: u32,
    seen: Vec<ValueId>,
}

impl PermitState {
    fn new(permit: &crate::lir::body::BarrierPermit) -> Self {
        Self {
            shades: permit.max_shades,
            cards: permit.max_card_marks,
            seen: Vec::new(),
        }
    }

    /// 消费一次 hybrid barrier 写入；返回 `false` 表示额度不足。
    fn consume(&mut self, address: ValueId) -> bool {
        let Some(shades) = self.shades.checked_sub(2) else {
            return false;
        };
        self.shades = shades;
        match self.seen.binary_search(&address) {
            Ok(_) => true,
            Err(position) => {
                let Some(cards) = self.cards.checked_sub(1) else {
                    return false;
                };
                self.cards = cards;
                self.seen.insert(position, address);
                true
            }
        }
    }

    /// 合流：剩余额度取逐项最小值，已消费键取交集。
    fn join(previous: &Self, next: &Self) -> Self {
        let mut seen = Vec::with_capacity(previous.seen.len().min(next.seen.len()));
        let (mut left, mut right) = (0, 0);
        while left < previous.seen.len() && right < next.seen.len() {
            match previous.seen[left].cmp(&next.seen[right]) {
                std::cmp::Ordering::Less => left += 1,
                std::cmp::Ordering::Greater => right += 1,
                std::cmp::Ordering::Equal => {
                    seen.push(previous.seen[left]);
                    left += 1;
                    right += 1;
                }
            }
        }
        Self {
            shades: previous.shades.min(next.shades),
            cards: previous.cards.min(next.cards),
            seen,
        }
    }
}

/// region 内屏障的静态额度：shade 按写数、card-mark 按 distinct 写入地址数。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct StaticQuota {
    shades: u32,
    cards: u32,
}

/// 复算一个 region 的静态额度；region 必须落在单个 block 内。
fn static_quota(body: &Body, region: u32) -> Result<StaticQuota, Diagnostic> {
    let mut window = None;
    for (block_index, block) in body.blocks.iter().enumerate() {
        let mut begin = None;
        for (offset, instruction) in body.instructions[range(&block.instructions)]
            .iter()
            .enumerate()
        {
            match instruction.op {
                Op::NoSafepointBegin(id) if id == region => begin = Some(offset),
                Op::NoSafepointEnd(id) if id == region => {
                    let Some(begin) = begin else {
                        return Err(invalid("NoSafepointRegion end 没有匹配 begin"));
                    };
                    if window.is_some() {
                        return Err(invalid("NoSafepointRegion 必须具有唯一 begin/end"));
                    }
                    window = Some((block_index, begin, offset));
                }
                _ => {}
            }
        }
    }
    let (block_index, begin, end) =
        window.ok_or_else(|| invalid("barrier permit 引用的 region 没有唯一 begin/end"))?;
    let mut barriers = 0_u32;
    let mut addresses: Vec<ValueId> = Vec::new();
    let instructions = &body.instructions[range(&body.blocks[block_index].instructions)];
    for instruction in &instructions[begin..end] {
        // 构建结果里 region 内只会剩下已预留屏障；结构 verifier 模式允许裸屏障，
        // 两种 opcode 都是同一个写入地址的 hybrid barrier。
        if matches!(
            instruction.op,
            Op::GcWriteBarrier { .. } | Op::GcWriteBarrierReserved { .. }
        ) {
            barriers += 1;
            let address = body.args(&instruction.arguments)[0];
            if let Err(position) = addresses.binary_search(&address) {
                addresses.insert(position, address);
            }
        }
    }
    Ok(StaticQuota {
        shades: barriers.saturating_mul(2),
        cards: u32::try_from(addresses.len()).expect("屏障写入地址数量适配 u32"),
    })
}

pub(super) fn verify(body: &Body, graph: &Graph, mode: Mode) -> Result<Layout, Diagnostic> {
    let mut begins = vec![0; body.no_safepoint_regions.len()];
    let mut ends = begins.clone();
    let mut reserves = if mode == Mode::Complete {
        vec![0; body.barrier_permits.len()]
    } else {
        Vec::new()
    };
    for instruction in &body.instructions {
        match instruction.op {
            Op::NoSafepointBegin(region) => {
                begins[usize::try_from(region).expect("region 编号")] += 1
            }
            Op::NoSafepointEnd(region) => ends[usize::try_from(region).expect("region 编号")] += 1,
            Op::BarrierReserve(permit) if mode == Mode::Complete => reserves[permit.index()] += 1,
            _ => {}
        }
    }
    if begins.iter().chain(&ends).any(|count| *count != 1) {
        return Err(invalid("NoSafepointRegion 必须具有唯一 begin/end"));
    }
    if mode == Mode::Complete
        && (reserves.iter().any(|count| *count != 1)
            || body.barrier_permits.iter().any(|permit| {
                permit.max_shades == 0
                    || permit.max_card_marks == 0
                    || !usize::try_from(permit.region).is_ok_and(|index| index < begins.len())
            }))
    {
        return Err(invalid("barrier permit 缺少唯一 reserve 或引用非法 region"));
    }
    // permit 是 compile-time 容量证明：card-mark 额度不得超过 processor 的
    // `CardMarkBuffer` 容量，否则 region 内必然需要补容量，而补容量只能发生在 region 外。
    // 这条检查只看 permit 自身字段，因此先于额度一致性检查生效。
    if mode == Mode::Complete
        && body
            .barrier_permits
            .iter()
            .any(|permit| permit.max_card_marks > CARD_MARK_BUFFER_ENTRIES)
    {
        return Err(invalid(
            "barrier permit 的 card-mark 额度超过 CardMarkBuffer 容量",
        ));
    }
    if mode == Mode::Complete {
        // permit 的额度必须等于 region 的静态复算值：额度既不能偏小（region 内不得补容量），
        // 也不能偏大（禁止用宽松 permit 掩盖其它 region 的消费）。
        for permit in &body.barrier_permits {
            let quota = static_quota(body, permit.region)?;
            if permit.max_shades != quota.shades || permit.max_card_marks != quota.cards {
                return Err(invalid(
                    "barrier permit 额度与该 region 的静态消费上界不一致",
                ));
            }
        }
    }
    let empty = State {
        regions: Vec::new(),
        views: BTreeMap::new(),
        shared: Vec::new(),
        permits: vec![None; reserves.len()],
    };
    let mut incoming = vec![None; body.blocks.len()];
    incoming[body.entry.index()] = Some(empty);
    let mut work = VecDeque::from([body.entry]);
    let mut memberships = vec![vec![false; body.blocks.len()]; begins.len()];
    while let Some(block_id) = work.pop_front() {
        let mut state = incoming[block_id.index()].clone().expect("已登记输入状态");
        let block = &body.blocks[block_id.index()];
        for region in &state.regions {
            memberships[usize::try_from(*region).expect("region 编号")][block_id.index()] = true;
        }
        for instruction in &body.instructions[range(&block.instructions)] {
            if !state.regions.is_empty() && forbidden(&instruction.op, mode) {
                return Err(invalid(
                    "NoSafepointRegion 含有调用、panic、分配或 slow edge",
                ));
            }
            match &instruction.op {
                Op::NoSafepointBegin(region) => {
                    if state.regions.contains(region) {
                        return Err(invalid("同一 NoSafepointRegion 重入"));
                    }
                    state.regions.push(*region);
                    memberships[usize::try_from(*region).expect("region 编号")][block_id.index()] =
                        true;
                }
                Op::NoSafepointEnd(region) => {
                    if state.regions.pop() != Some(*region) {
                        return Err(invalid("NoSafepointRegion end 没有匹配 begin"));
                    }
                }
                Op::BarrierReserve(permit) if mode == Mode::Complete => {
                    if !state.regions.is_empty() {
                        return Err(invalid("barrier reserve 必须在 region 外"));
                    }
                    state.permits[permit.index()] =
                        Some(PermitState::new(&body.barrier_permits[permit.index()]));
                }
                Op::GcWriteBarrierReserved { permit, .. } if mode == Mode::Complete => {
                    let record = &body.barrier_permits[permit.index()];
                    if state.regions.last() != Some(&record.region) {
                        return Err(invalid("预留屏障不在对应 region 内"));
                    }
                    let address = body.args(&instruction.arguments)[0];
                    let remaining = state.permits[permit.index()]
                        .as_mut()
                        .ok_or_else(|| invalid("屏障没有被 reserve 支配"))?;
                    if !remaining.consume(address) {
                        return Err(invalid("hybrid barrier 超过预留 shade 或 card-mark 额度"));
                    }
                }
                Op::ScopedViewBegin { token, mode } => {
                    let source = body.args(&instruction.arguments)[0];
                    if state.views.insert(*token, (source, *mode)).is_some() {
                        return Err(invalid("scoped view token 重复 begin"));
                    }
                }
                Op::ScopedViewEnd { token } => {
                    if state.views.remove(token).is_none() {
                        return Err(invalid("scoped view token 重复或未匹配 end"));
                    }
                }
                Op::SharedAccessBegin { token } => {
                    if state.shared.contains(token) {
                        return Err(invalid("shared access token 重复 begin"));
                    }
                    state.shared.push(*token);
                }
                Op::SharedAccessEnd { token } => {
                    if state.shared.pop() != Some(*token) {
                        return Err(invalid("shared access guard 没有正确闭合"));
                    }
                }
                Op::Store(_) => {
                    let args = body.args(&instruction.arguments);
                    if state.views.values().any(|(source, mode)| {
                        *mode == ViewMode::ScopedRead && derived_from(body, args[0], *source)
                    }) {
                        return Err(invalid("ScopedRead 投影被写入"));
                    }
                    if state
                        .views
                        .values()
                        .any(|(source, _)| derived_from(body, args[1], *source))
                    {
                        return Err(invalid("scoped view 引用逃逸到存储"));
                    }
                }
                Op::Call(call) | Op::ForeignCall(call) => {
                    view_call(body, &state, call, body.args(&instruction.arguments))?
                }
                Op::Park | Op::CoroutineSwitch
                    if !state.views.is_empty() || !state.shared.is_empty() =>
                {
                    return Err(invalid("borrowed view 或 shared access guard 跨 suspend"));
                }
                _ => {}
            }
        }
        if let Terminator::Invoke {
            call, arguments, ..
        }
        | Terminator::TailCall {
            call, arguments, ..
        } = &block.terminator
        {
            if !state.regions.is_empty() {
                return Err(invalid("NoSafepointRegion 跨越 Invoke/TailCall"));
            }
            view_call(body, &state, call, body.args(arguments))?;
        }
        let mut successors = Vec::new();
        edges(body, &block.terminator, |edge| {
            successors.push(body.edges[edge.index()].to)
        });
        if successors.is_empty()
            && (!state.regions.is_empty() || !state.views.is_empty() || !state.shared.is_empty())
        {
            return Err(invalid("控制流出口留下未闭合的 region/view/access guard"));
        }
        for successor in successors {
            if !state.regions.is_empty() && graph.dominates(successor, block_id) {
                return Err(invalid("NoSafepointRegion 包含回边"));
            }
            match &mut incoming[successor.index()] {
                None => {
                    incoming[successor.index()] = Some(state.clone());
                    work.push_back(successor);
                }
                Some(previous) => {
                    if previous.regions != state.regions
                        || previous.views != state.views
                        || previous.shared != state.shared
                    {
                        return Err(invalid("合流边的 effect region 栈不一致"));
                    }
                    let mut changed = false;
                    for (previous, next) in previous.permits.iter_mut().zip(&state.permits) {
                        let joined = match (previous.as_ref(), next.as_ref()) {
                            (Some(left), Some(right)) => Some(PermitState::join(left, right)),
                            _ => None,
                        };
                        if previous.as_ref() != joined.as_ref() {
                            *previous = joined;
                            changed = true;
                        }
                    }
                    if changed {
                        work.push_back(successor);
                    }
                }
            }
        }
    }
    for members in &memberships {
        acyclic(body, members)?;
    }
    Ok(Layout { memberships })
}

fn forbidden(op: &Op, mode: Mode) -> bool {
    match op {
        Op::GcWriteBarrierReserved { .. } => false,
        Op::GcWriteBarrier { .. } => mode == Mode::Complete,
        Op::Call(call) | Op::ForeignCall(call) if call.poll_free_leaf => false,
        _ => {
            op.safepoint_kind().is_some()
                || matches!(
                    op,
                    Op::Call(_)
                        | Op::ForeignCall(_)
                        | Op::TrapIf
                        | Op::InlineAsm(_)
                        | Op::BarrierReserve(_)
                        | Op::RegionPublish
                        | Op::RegionReset
                        | Op::ForwardSharedHandle
                        | Op::PlatformCall(_)
                )
        }
    }
}

fn view_call(
    body: &Body,
    state: &State,
    call: &crate::lir::body::Call,
    args: &[ValueId],
) -> Result<(), Diagnostic> {
    if (!state.views.is_empty() || !state.shared.is_empty())
        && (call.may_suspend || call.kind != CallKind::Managed)
    {
        return Err(invalid(
            "scoped/shared borrowed view 不能跨 suspend 或 foreign frame",
        ));
    }
    if call.captures_arguments
        && state
            .views
            .values()
            .any(|(source, _)| args.iter().any(|value| derived_from(body, *value, *source)))
    {
        return Err(invalid("scoped view 传入会保存参数的调用"));
    }
    Ok(())
}
fn derived_from(body: &Body, mut value: ValueId, source: ValueId) -> bool {
    for _ in 0..body.values.len() {
        if value == source {
            return true;
        }
        if let Origin::Derived(base) = body.values[value.index()].origin {
            value = base;
        } else {
            return false;
        }
    }
    false
}
fn acyclic(body: &Body, members: &[bool]) -> Result<(), Diagnostic> {
    let mut incoming = vec![0u32; body.blocks.len()];
    for edge in &body.edges {
        if members[edge.from.index()] && members[edge.to.index()] {
            incoming[edge.to.index()] += 1;
        }
    }
    let mut ready: Vec<_> = members
        .iter()
        .enumerate()
        .filter(|(index, member)| **member && incoming[*index] == 0)
        .map(|(index, _)| BlockId(id(index)))
        .collect();
    let mut visited = 0;
    while let Some(block) = ready.pop() {
        visited += 1;
        edges(body, &body.blocks[block.index()].terminator, |edge| {
            let target = body.edges[edge.index()].to;
            if members[target.index()] {
                incoming[target.index()] -= 1;
                if incoming[target.index()] == 0 {
                    ready.push(target);
                }
            }
        });
    }
    if visited != members.iter().filter(|member| **member).count() {
        Err(invalid("NoSafepointRegion 含有不可约环"))
    } else {
        Ok(())
    }
}
