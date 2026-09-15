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
                        | Op::RegionPublish { .. }
                        | Op::RegionReset { .. }
                        | Op::PromoteManaged { .. }
                        | Op::RegionTransfer { .. }
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

/// region 生命周期阶段；顺序即强度。
const STAGE_NONE: u8 = 0;
const STAGE_ALLOC: u8 = 1;
const STAGE_PUBLISHED: u8 = 2;
const STAGE_ENDED: u8 = 3;

/// 一个 region 在某个程序点上的 must/may 阶段。
///
/// must 是全部前驱阶段的最小值，may 是最大值：前者的 `Alloc` 表示「每条路径都分配过」，
/// 后者的 `Published` 表示「每条路径都恰好停在这一阶段」。两者相等时该阶段在所有路径上都
/// 成立，这正是发布与结束动作要求的条件。
#[derive(Clone, Copy, Eq, PartialEq)]
struct Life {
    must: u8,
    must_export: u8,
    may: u8,
    may_export: u8,
}

impl Life {
    const NONE: Self = Self {
        must: STAGE_NONE,
        must_export: 0,
        may: STAGE_NONE,
        may_export: 0,
    };

    fn join(&mut self, other: &Self) {
        self.must = self.must.min(other.must);
        self.must_export |= other.must_export;
        self.may = self.may.max(other.may);
        self.may_export |= other.may_export;
    }
}

/// 一条 region 生命周期指令。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RegionOp {
    Alloc { region: u32 },
    Publish { region: u32, export: u8 },
    Reset { region: u32 },
    Promote { region: u32 },
    Transfer { region: u32 },
}

impl RegionOp {
    fn region(self) -> u32 {
        match self {
            Self::Alloc { region }
            | Self::Publish { region, .. }
            | Self::Reset { region }
            | Self::Promote { region }
            | Self::Transfer { region } => region,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Alloc { .. } => "分配",
            Self::Publish { .. } => "发布",
            Self::Reset { .. } => "重置",
            Self::Promote { .. } => "保留",
            Self::Transfer { .. } => "移交",
        }
    }
}

fn region_op(op: &Op) -> Option<RegionOp> {
    match op {
        Op::RegionAlloc { region, .. } => Some(RegionOp::Alloc { region: *region }),
        Op::RegionPublish { region, export } => Some(RegionOp::Publish {
            region: *region,
            export: *export,
        }),
        Op::RegionReset { region } => Some(RegionOp::Reset { region: *region }),
        Op::PromoteManaged { region } => Some(RegionOp::Promote { region: *region }),
        Op::RegionTransfer { region } => Some(RegionOp::Transfer { region: *region }),
        _ => None,
    }
}

/// 校验 region 生命周期：分配 → 发布 →（重置 | 保留 | 移交）。
///
/// must 分析保证「每条路径都先分配」「没有重复发布也没有结束后再分配」；may 分析保证结束动作
/// 之前全部路径都恰好停在 `Published`。重置只允许发生在 export summary 闭合（`export == 0`）
/// 的 region 上；summary 未闭合时只能保留或移交。
pub(super) fn verify_lifecycle(body: &Body) -> Result<(), Diagnostic> {
    let reachable = reachable(body);
    let mut ops: Vec<Vec<RegionOp>> = vec![Vec::new(); body.blocks.len()];
    let mut regions = 0_u32;
    let mut allocated = Vec::new();
    for (index, block) in body.blocks.iter().enumerate() {
        if !reachable[index] {
            continue;
        }
        for offset in block.instructions.start..block.instructions.end {
            let instruction = &body.instructions[offset as usize];
            let Some(op) = region_op(&instruction.op) else {
                continue;
            };
            let region = op.region();
            regions = regions.max(region + 1);
            if allocated.len() < regions as usize {
                allocated.resize(regions as usize, false);
            }
            if matches!(op, RegionOp::Alloc { .. }) {
                allocated[region as usize] = true;
            }
            ops[index].push(op);
        }
    }
    if regions == 0 {
        return Ok(());
    }
    for block_ops in &ops {
        for op in block_ops {
            if !allocated
                .get(op.region() as usize)
                .copied()
                .unwrap_or(false)
            {
                return Err(invalid(&format!(
                    "LIR region {} 没有分配点（{} 中 {}）",
                    op.region(),
                    body.name,
                    op.name()
                )));
            }
        }
    }
    let count = regions as usize;
    let mut state_in = vec![vec![Life::NONE; count]; body.blocks.len()];
    let mut state_out = state_in.clone();
    let entry = body.entry.index();
    let mut changed = true;
    while changed {
        changed = false;
        for (index, block_ops) in ops.iter().enumerate() {
            if !reachable[index] {
                continue;
            }
            let mut entry_state = vec![Life::NONE; count];
            if index != entry {
                let mut first = true;
                for predecessor in body.predecessors[body.blocks[index].predecessors.start as usize
                    ..body.blocks[index].predecessors.end as usize]
                    .iter()
                {
                    let source = body.edges[predecessor.index()].from;
                    if !reachable[source.index()] {
                        continue;
                    }
                    let out = &state_out[source.index()];
                    if first {
                        entry_state.clone_from(out);
                        first = false;
                    } else {
                        for (slot, other) in entry_state.iter_mut().zip(out) {
                            slot.join(other);
                        }
                    }
                }
            }
            let mut out = entry_state.clone();
            for op in block_ops {
                transition(&mut out, *op);
            }
            if entry_state != state_in[index] {
                state_in[index] = entry_state;
                changed = true;
            }
            if out != state_out[index] {
                state_out[index] = out;
                changed = true;
            }
        }
    }
    for (index, block_ops) in ops.iter().enumerate() {
        if !reachable[index] {
            continue;
        }
        let mut state = state_in[index].clone();
        for op in block_ops {
            check(&mut state, *op)?;
        }
    }
    verify_channel_transfer(body, &reachable)
}

/// 无校验的阶段迁移；只用于求不动点，必须保持单调。
fn transition(state: &mut [Life], op: RegionOp) {
    let slot = &mut state[op.region() as usize];
    match op {
        RegionOp::Alloc { .. } => {
            slot.must = STAGE_ALLOC;
            slot.may = slot.may.max(STAGE_ALLOC);
        }
        RegionOp::Publish { export, .. } => {
            slot.must = STAGE_PUBLISHED;
            slot.must_export |= export;
            slot.may = slot.may.max(STAGE_PUBLISHED);
            slot.may_export |= export;
        }
        RegionOp::Reset { .. } | RegionOp::Promote { .. } | RegionOp::Transfer { .. } => {
            slot.must = STAGE_ENDED;
            slot.may = STAGE_ENDED;
        }
    }
}

/// 在收敛后的输入状态上校验一条 region 指令。
fn check(state: &mut [Life], op: RegionOp) -> Result<(), Diagnostic> {
    let slot = state[op.region() as usize];
    match op {
        RegionOp::Alloc { .. } => {
            if slot.may == STAGE_PUBLISHED {
                return Err(invalid("region 结束后才能重新分配"));
            }
        }
        RegionOp::Publish { .. } => {
            if slot.must != STAGE_ALLOC || slot.may != STAGE_ALLOC {
                return Err(invalid("region 发布前必须在每条路径上恰好分配一次"));
            }
        }
        RegionOp::Reset { .. } => {
            if slot.must != STAGE_PUBLISHED || slot.may != STAGE_PUBLISHED {
                return Err(invalid("region 重置前必须在每条路径上恰好发布一次"));
            }
            if slot.must_export | slot.may_export != 0 {
                return Err(invalid("只有 export summary 闭合的 region 才能重置"));
            }
        }
        RegionOp::Promote { .. } | RegionOp::Transfer { .. } => {
            if slot.must != STAGE_PUBLISHED || slot.may != STAGE_PUBLISHED {
                return Err(invalid("region 结束前必须在每条路径上恰好发布一次"));
            }
        }
    }
    let _ = op.name();
    transition(state, op);
    Ok(())
}

/// region 移交必须落在 channel send 边界上，且 send 携带 region 派生值时必须已经移交。
///
/// 两条规则一起使「普通 channel 不获得 transfer 语义」成为结构不变量：`RegionTransfer` 只能
/// 紧邻一个真实的 `ChannelSend`/`SelectCommit` 出现，普通出口上出现移交就是错误；反过来，
/// 能追溯到 region 分配点的 send 实参必须先由显式移交放行。
fn verify_channel_transfer(body: &Body, reachable: &[bool]) -> Result<(), Diagnostic> {
    for (index, block) in body.blocks.iter().enumerate() {
        if !reachable[index] {
            continue;
        }
        let sends: Vec<usize> = (block.instructions.start..block.instructions.end)
            .filter(|offset| {
                let op = &body.instructions[*offset as usize].op;
                matches!(op, Op::Call(call) if sends_region(call))
            })
            .map(|offset| offset as usize)
            .collect();
        let mut transferred: Vec<u32> = Vec::new();
        for offset in block.instructions.start..block.instructions.end {
            let instruction = &body.instructions[offset as usize];
            match &instruction.op {
                Op::RegionTransfer { region } => {
                    if !sends.iter().any(|send| *send > offset as usize) {
                        return Err(invalid("region 移交只能发生在 channel send 边界"));
                    }
                    transferred.push(*region);
                }
                Op::Call(call) => {
                    if !sends_region(call) {
                        continue;
                    }
                    for argument in body.args(&instruction.arguments) {
                        let Some(region) = super::operations::region_of(body, *argument) else {
                            continue;
                        };
                        if !transferred.contains(&region) {
                            return Err(invalid(
                                "channel send 携带 region 派生值前必须先移交该 region",
                            ));
                        }
                    }
                }
                _ => {}
            }
        }
    }
    Ok(())
}

/// 该调用是否把 region 的所有权交给另一个 owner。
fn sends_region(call: &crate::lir::body::Call) -> bool {
    matches!(
        call.target,
        crate::lir::body::CallTarget::Runtime(
            crate::lir::body::RuntimeCall::ChannelSend
                | crate::lir::body::RuntimeCall::SelectCommit { .. }
        )
    )
}

/// 从入口可达的 block。
fn reachable(body: &Body) -> Vec<bool> {
    let mut seen = vec![false; body.blocks.len()];
    let mut stack = vec![body.entry];
    while let Some(block) = stack.pop() {
        if std::mem::replace(&mut seen[block.index()], true) {
            continue;
        }
        edges(body, &body.blocks[block.index()].terminator, |edge| {
            stack.push(body.edges[edge.index()].to)
        });
    }
    seen
}
