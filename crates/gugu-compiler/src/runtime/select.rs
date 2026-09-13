//! select 提交：xoshiro256++、两条路径、Building/Armed 与 winner CAS。
//!
//! ≤8 且 `try_lock` 全成功：按 `WaitSourceId` 排序加锁，按 permutation 提交第一个 ready。
//! 任一 `try_lock` 失败则逆序释放，不得持锁等下一把，转入一次一把锁的扫描路径。
//! Building 期 waker 可 CAS winner，但不得 ready 仍在登记的协程。

use super::channel::{ChannelHandle, ChannelTable, TryRecvErr, TrySendErr};
use super::coroutine::{CoroutineHandle, CoroutineState, CoroutineTable};
use super::slab::RawInvariant;
use super::wait::{
    SelectTxn, WAIT_NODE_BUILDING, WAIT_NODE_SELECT, WAIT_NOTIFIED, WaitNodeHandle, WaitPlane,
    WaitResult, WaitSourceId, phase_armed, phase_building, winner_default, winner_unset,
};
use super::wait_schema::INLINE_SELECT_CASES;

/// select 操作。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SelectOp {
    Send {
        channel: ChannelHandle,
        payload: u64,
    },
    Recv {
        channel: ChannelHandle,
    },
    Wait {
        join: CoroutineHandle,
    },
}

/// 一个 select case 的登记。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SelectCase {
    pub(crate) op: SelectOp,
    pub(crate) index: u32,
}

/// select 提交结果。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SelectOutcome {
    Case(u32),
    Default,
    Parked,
    Never,
}

/// 确定性 xoshiro256++；测试注入状态，生产语义由契约注释的 BLAKE3 种子说明。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SelectRng {
    pub(crate) s: [u64; 4],
}

impl SelectRng {
    pub(crate) fn new(seed: [u64; 4]) -> Self {
        let mut rng = Self { s: seed };
        if rng.s.iter().all(|&word| word == 0) {
            rng.s = [0x9E37_79B9_7F4A_7C15, 1, 2, 3];
        }
        rng
    }

    pub(crate) fn from_cold(words: [u64; 4]) -> Self {
        Self::new(words)
    }

    pub(crate) fn next_u64(&mut self) -> u64 {
        let result = self.s[1].wrapping_mul(5).rotate_left(7).wrapping_mul(9);
        let t = self.s[1] << 17;
        self.s[2] ^= self.s[0];
        self.s[3] ^= self.s[1];
        self.s[1] ^= self.s[2];
        self.s[0] ^= self.s[3];
        self.s[2] ^= t;
        self.s[3] = self.s[3].rotate_left(45);
        result
    }

    /// threshold rejection，无偏 `uniform(n)`。
    pub(crate) fn uniform(&mut self, n: u32) -> u32 {
        debug_assert!(n > 0);
        let n = u64::from(n);
        let threshold = n.wrapping_neg() % n;
        loop {
            let sample = self.next_u64();
            if sample >= threshold {
                return u32::try_from(sample % n).expect("uniform 的模数来自 u32");
            }
        }
    }

    pub(crate) fn fisher_yates(&mut self, n: u32) -> Vec<u32> {
        let mut perm: Vec<u32> = (0..n).collect();
        for i in (1..n).rev() {
            let j = self.uniform(i + 1);
            perm.swap(
                usize::try_from(i).expect("下标"),
                usize::try_from(j).expect("下标"),
            );
        }
        perm
    }
}

fn source_of(
    channels: &ChannelTable,
    wait: &WaitPlane,
    case: &SelectCase,
) -> Result<WaitSourceId, RawInvariant> {
    match case.op {
        SelectOp::Send { channel, .. } | SelectOp::Recv { channel } => channels.source(channel),
        SelectOp::Wait { join } => wait.join_source(join),
    }
}

fn source_order(
    channels: &ChannelTable,
    wait: &WaitPlane,
    controls: &CoroutineTable,
    cases: &[SelectCase],
) -> Result<Vec<(WaitSourceId, usize)>, RawInvariant> {
    let mut sources = Vec::with_capacity(cases.len());
    for (index, case) in cases.iter().enumerate() {
        if let SelectOp::Wait { join } = case.op {
            controls.get(join)?;
        }
        sources.push((source_of(channels, wait, case)?, index));
    }
    sources.sort_unstable();
    Ok(sources)
}

fn case_ready(
    channels: &ChannelTable,
    controls: &CoroutineTable,
    case: &SelectCase,
) -> Result<bool, RawInvariant> {
    match case.op {
        SelectOp::Send { channel, .. } => channels.is_ready_send(channel),
        SelectOp::Recv { channel } => channels.is_ready_recv(channel),
        SelectOp::Wait { join } => {
            Ok(controls.get(join)?.0.hot.lifecycle()? == CoroutineState::Dead)
        }
    }
}

fn try_lock_all(
    wait: &mut WaitPlane,
    sources: &[(WaitSourceId, usize)],
) -> Result<bool, RawInvariant> {
    for (index, &(source, _)) in sources.iter().enumerate() {
        if index > 0 && sources[index - 1].0 == source {
            continue;
        }
        match wait.try_lock(source) {
            Ok(true) => {}
            result => {
                unlock_all(wait, &sources[..index])?;
                return result;
            }
        }
    }
    Ok(true)
}

fn unlock_all(wait: &mut WaitPlane, sources: &[(WaitSourceId, usize)]) -> Result<(), RawInvariant> {
    let mut previous = None;
    for &(source, _) in sources.iter().rev() {
        if previous != Some(source) {
            wait.unlock(source)?;
        }
        previous = Some(source);
    }
    Ok(())
}

/// 提交 select：求值已在锁外完成，这里只做提交。
pub(crate) fn select_commit(
    wait: &mut WaitPlane,
    channels: &mut ChannelTable,
    controls: &mut CoroutineTable,
    cases: &[SelectCase],
    has_default: bool,
    rng: &mut SelectRng,
    coroutine: CoroutineHandle,
) -> Result<(SelectOutcome, Option<WaitNodeHandle>), RawInvariant> {
    let count = u32::try_from(cases.len())
        .map_err(|_| RawInvariant::new("select case 数量超出编码范围"))?;
    if cases.iter().any(|case| case.index > u32::MAX - 2) {
        return Err(RawInvariant::new("select case index 超出 winner 编码范围"));
    }
    controls.get(coroutine)?;
    let sources = source_order(channels, wait, controls, cases)?;
    let generation = wait.begin_wait(coroutine)?;
    let (slot, cold) = controls.get_mut(coroutine)?;
    slot.hot
        .wait_word
        .store(0, std::sync::atomic::Ordering::Relaxed);
    cold.select_scratch = SelectTxn {
        phase_winner: phase_building() << 32,
        case_count: u64::from(count),
        scratch_handle: 0,
        wait_block: 0,
    }
    .to_cold();
    if cases.is_empty() && !has_default {
        let never = wait.never_source();
        let node = wait.alloc_node(coroutine, never, 0, 0, 0, 0, generation)?;
        wait.arm_nodes(coroutine, vec![node]);
        wait.lock(never)?;
        let result = wait.enqueue_source(never, node);
        wait.unlock(never)?;
        result?;
        arm_select(wait, controls, coroutine)?;
        return Ok((SelectOutcome::Never, None));
    }
    let perm = rng.fisher_yates(count);
    let committed = if count <= INLINE_SELECT_CASES && try_lock_all(wait, &sources)? {
        let result = commit_locked(wait, channels, controls, cases, &perm, coroutine);
        unlock_all(wait, &sources)?;
        result?
    } else {
        scan_path(wait, channels, controls, cases, &sources, &perm, coroutine)?
    };
    if let Some(wake) = committed {
        return Ok((arm_select(wait, controls, coroutine)?, wake));
    }
    if has_default {
        let (_, cold) = controls.get_mut(coroutine)?;
        let mut txn = SelectTxn::from_cold(cold.select_scratch);
        let won = txn.cas_winner(winner_unset(), winner_default());
        debug_assert!(won);
        txn.set_phase(phase_armed());
        cold.select_scratch = txn.to_cold();
        return Ok((SelectOutcome::Default, None));
    }
    let wake = register_waiters(
        wait, channels, controls, cases, &sources, coroutine, generation,
    )?;
    Ok((arm_select(wait, controls, coroutine)?, wake))
}

fn commit_locked(
    wait: &mut WaitPlane,
    channels: &mut ChannelTable,
    controls: &mut CoroutineTable,
    cases: &[SelectCase],
    perm: &[u32],
    coroutine: CoroutineHandle,
) -> Result<Option<Option<WaitNodeHandle>>, RawInvariant> {
    for &index in perm {
        if let Some(wake) = try_case_locked(
            wait,
            channels,
            controls,
            &cases[usize::try_from(index).expect("case 下标")],
            coroutine,
        )? {
            return Ok(Some(wake));
        }
    }
    Ok(None)
}

fn selected(
    controls: &CoroutineTable,
    coroutine: CoroutineHandle,
) -> Result<Option<SelectOutcome>, RawInvariant> {
    let txn = SelectTxn::from_cold(controls.get(coroutine)?.1.select_scratch);
    Ok(match txn.winner() {
        value if value == winner_unset() => None,
        value if value == winner_default() => Some(SelectOutcome::Default),
        value => Some(SelectOutcome::Case(
            u32::try_from(value - 2).expect("winner 保存在低 32 位"),
        )),
    })
}

fn try_case_locked(
    wait: &mut WaitPlane,
    channels: &mut ChannelTable,
    controls: &mut CoroutineTable,
    case: &SelectCase,
    coroutine: CoroutineHandle,
) -> Result<Option<Option<WaitNodeHandle>>, RawInvariant> {
    if selected(controls, coroutine)?.is_some() {
        return Ok(Some(None));
    }
    let selection = Some((coroutine, case.index));
    match case.op {
        SelectOp::Send { channel, payload } => {
            match channels.try_send_locked(wait, controls, channel, payload, selection)? {
                Ok(wake) => Ok(Some(wake)),
                Err(TrySendErr::Full) => Ok(None),
                Err(TrySendErr::Closed) => Err(RawInvariant::new("send on closed channel")),
            }
        }
        SelectOp::Recv { channel } => {
            match channels.try_recv_locked(wait, controls, channel, selection)? {
                Ok((_, wake)) => Ok(Some(wake)),
                Err(TryRecvErr::Empty) => Ok(None),
                Err(TryRecvErr::Closed) => Ok(Some(None)),
            }
        }
        SelectOp::Wait { join } => {
            let (slot, cold) = controls.get(join)?;
            if slot.hot.lifecycle()? != CoroutineState::Dead {
                return Ok(None);
            }
            if cold
                .join_state
                .status
                .load(std::sync::atomic::Ordering::Acquire)
                & 3
                == 0
            {
                return Err(RawInvariant::new("协程尚未发布完成记录"));
            }
            if !wait.try_claim(controls, wait.join_source(join)?, selection, None)? {
                return Ok(None);
            }
            let value = controls
                .get(join)?
                .1
                .join_state
                .read()
                .expect("Dead 完成记录已预检且独占 controls");
            wait.publish_result(coroutine, WaitResult::Join(value));
            Ok(Some(None))
        }
    }
}

fn scan_path(
    wait: &mut WaitPlane,
    channels: &mut ChannelTable,
    controls: &mut CoroutineTable,
    cases: &[SelectCase],
    sources: &[(WaitSourceId, usize)],
    perm: &[u32],
    coroutine: CoroutineHandle,
) -> Result<Option<Option<WaitNodeHandle>>, RawInvariant> {
    let mut ready = vec![0_u64; cases.len().div_ceil(64)];
    for group in sources.chunk_by(|left, right| left.0 == right.0) {
        let source = group[0].0;
        wait.lock(source)?;
        let probed: Result<(), RawInvariant> = (|| {
            for &(_, bit) in group {
                if case_ready(channels, controls, &cases[bit])? {
                    ready[bit / 64] |= 1_u64 << (bit % 64);
                }
            }
            Ok(())
        })();
        wait.unlock(source)?;
        probed?;
    }
    for &index in perm {
        let bit = usize::try_from(index).expect("case 下标");
        if ready[bit / 64] & (1_u64 << (bit % 64)) == 0 {
            continue;
        }
        let case = &cases[bit];
        let source = source_of(channels, wait, case)?;
        wait.lock(source)?;
        let result = try_case_locked(wait, channels, controls, case, coroutine);
        wait.unlock(source)?;
        if let Some(wake) = result? {
            return Ok(Some(wake));
        }
    }
    Ok(None)
}

fn register_waiters(
    wait: &mut WaitPlane,
    channels: &mut ChannelTable,
    controls: &mut CoroutineTable,
    cases: &[SelectCase],
    sources: &[(WaitSourceId, usize)],
    coroutine: CoroutineHandle,
    generation: u64,
) -> Result<Option<WaitNodeHandle>, RawInvariant> {
    allocate_waiters(wait, channels, cases, coroutine, generation)?;
    for group in sources.chunk_by(|left, right| left.0 == right.0) {
        let source = group[0].0;
        wait.lock(source)?;
        let result: Result<Option<Option<WaitNodeHandle>>, RawInvariant> = (|| {
            for &(_, index) in group {
                let case = &cases[index];
                if let Some(wake) = try_case_locked(wait, channels, controls, case, coroutine)? {
                    return Ok(Some(wake));
                }
                let node = wait.armed_nodes(coroutine)[index];
                match case.op {
                    SelectOp::Send { channel, .. } => {
                        wait.enqueue(channels.send_queue_mut(channel)?, node)?;
                        channels.sync_heads(channel)?;
                    }
                    SelectOp::Recv { channel } => {
                        wait.enqueue(channels.recv_queue_mut(channel)?, node)?;
                        channels.sync_heads(channel)?;
                    }
                    SelectOp::Wait { .. } => wait.enqueue_source(source, node)?,
                }
            }
            Ok(None)
        })();
        wait.unlock(source)?;
        if let Some(wake) = result? {
            return Ok(wake);
        }
    }
    Ok(None)
}

fn allocate_waiters(
    wait: &mut WaitPlane,
    channels: &ChannelTable,
    cases: &[SelectCase],
    coroutine: CoroutineHandle,
    generation: u64,
) -> Result<(), RawInvariant> {
    let mut nodes = Vec::with_capacity(cases.len());
    for case in cases {
        let source = source_of(channels, wait, case).expect("所有 case 的源已经预检");
        let payload = match case.op {
            SelectOp::Send { payload, .. } => payload,
            _ => 0,
        };
        match wait.alloc_node(
            coroutine,
            source,
            case.index,
            payload,
            0,
            WAIT_NODE_SELECT | WAIT_NODE_BUILDING,
            generation,
        ) {
            Ok(node) => nodes.push(node),
            Err(error) => {
                for node in nodes {
                    wait.release_node(node).expect("预备节点尚未入队且仍有效");
                }
                return Err(error);
            }
        }
    }
    wait.arm_nodes(coroutine, nodes);
    Ok(())
}

/// 先进入 Parking 再公开 Armed，避免 winner 到达后仍被挂起。
pub(crate) fn arm_select(
    wait: &mut WaitPlane,
    controls: &mut CoroutineTable,
    coroutine: CoroutineHandle,
) -> Result<SelectOutcome, RawInvariant> {
    if selected(controls, coroutine)?.is_none() {
        controls
            .get(coroutine)?
            .0
            .hot
            .transition(CoroutineState::Running, CoroutineState::Parking)?;
    }
    let (_, cold) = controls.get_mut(coroutine)?;
    let mut txn = SelectTxn::from_cold(cold.select_scratch);
    txn.set_phase(phase_armed());
    cold.select_scratch = txn.to_cold();
    for index in 0..wait.armed_nodes(coroutine).len() {
        wait.arm_building(wait.armed_nodes(coroutine)[index], false)?;
    }
    if let Some(outcome) = selected(controls, coroutine)? {
        let hot = &controls.get(coroutine)?.0.hot;
        if hot.lifecycle()? == CoroutineState::Parking {
            hot.wait_word
                .fetch_and(!WAIT_NOTIFIED, std::sync::atomic::Ordering::Release);
            hot.transition(CoroutineState::Parking, CoroutineState::Running)?;
        }
        Ok(outcome)
    } else {
        Ok(SelectOutcome::Parked)
    }
}
