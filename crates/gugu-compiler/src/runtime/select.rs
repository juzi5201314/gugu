//! select 提交：xoshiro256++、两条路径、Building/Armed 与 winner CAS。
//!
//! ≤8 且 `try_lock` 全成功：按 `WaitSourceId` 排序加锁，按 permutation 提交第一个 ready。
//! 任一 `try_lock` 失败则逆序释放，不得持锁等下一把，转入一次一把锁的扫描路径。
//! Building 期 waker 可 CAS winner，但不得 ready 仍在登记的协程。

use super::channel::{ChannelHandle, ChannelTable};
use super::coroutine::CoroutineHandle;
use super::slab::RawInvariant;
use super::wait::{
    SelectTxn, WAIT_NODE_BUILDING, WaitNodeHandle, WaitPlane, WaitSourceId, encode_winner_case,
    phase_armed, phase_building, winner_default, winner_unset,
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
                return (sample % n) as u32;
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

fn unique_sorted_sources(
    channels: &ChannelTable,
    wait: &WaitPlane,
    cases: &[SelectCase],
) -> Result<Vec<WaitSourceId>, RawInvariant> {
    let mut sources = Vec::with_capacity(cases.len());
    for case in cases {
        let source = source_of(channels, wait, case)?;
        if !sources.contains(&source) {
            sources.push(source);
        }
    }
    sources.sort_unstable();
    Ok(sources)
}

fn case_ready(
    channels: &ChannelTable,
    _wait: &WaitPlane,
    completed: &[(CoroutineHandle, bool)],
    case: &SelectCase,
) -> Result<bool, RawInvariant> {
    match case.op {
        SelectOp::Send { channel, .. } => channels.is_ready_send(channel),
        SelectOp::Recv { channel } => channels.is_ready_recv(channel),
        SelectOp::Wait { join } => Ok(completed
            .iter()
            .any(|(handle, done)| *handle == join && *done)),
    }
}

fn try_lock_all(
    wait: &mut WaitPlane,
    sources: &[WaitSourceId],
) -> Result<Option<usize>, RawInvariant> {
    for (index, source) in sources.iter().enumerate() {
        if !wait.try_lock(*source)? {
            for held in sources[..index].iter().rev() {
                wait.unlock(*held)?;
            }
            return Ok(None);
        }
    }
    Ok(Some(sources.len()))
}

fn unlock_all(wait: &mut WaitPlane, sources: &[WaitSourceId]) -> Result<(), RawInvariant> {
    for source in sources.iter().rev() {
        wait.unlock(*source)?;
    }
    Ok(())
}

/// 提交 select：求值已在锁外完成，这里只做提交。
pub(crate) fn select_commit(
    wait: &mut WaitPlane,
    channels: &mut ChannelTable,
    completed_joins: &[(CoroutineHandle, bool)],
    cases: &[SelectCase],
    has_default: bool,
    rng: &mut SelectRng,
    coroutine: CoroutineHandle,
) -> Result<(SelectOutcome, SelectTxn), RawInvariant> {
    let mut txn = SelectTxn {
        phase_winner: phase_building() << 32,
        case_count: u64::from(u32::try_from(cases.len()).expect("case 数量")),
        scratch_handle: 0,
        wait_block: 0,
    };
    if cases.is_empty() && !has_default {
        let generation = wait.begin_wait(coroutine)?;
        let never = wait.never_source();
        let node = wait.alloc_node(coroutine, never, 0, 0, 0, 0, generation)?;
        wait.enqueue_source(never, node)?;
        wait.arm_nodes(coroutine, vec![node]);
        txn.set_phase(phase_armed());
        return Ok((SelectOutcome::Never, txn));
    }
    let sources = unique_sorted_sources(channels, wait, cases)?;
    let n = u32::try_from(cases.len()).expect("case 数量");
    let perm = if n == 0 {
        Vec::new()
    } else {
        rng.fisher_yates(n)
    };
    let inline = cases.len() as u32 <= INLINE_SELECT_CASES;
    let outcome = if inline {
        match try_lock_all(wait, &sources)? {
            Some(_) => {
                let outcome = commit_locked(
                    wait,
                    channels,
                    completed_joins,
                    cases,
                    &perm,
                    has_default,
                    &mut txn,
                    coroutine,
                )?;
                unlock_all(wait, &sources)?;
                outcome
            }
            None => scan_path(
                wait,
                channels,
                completed_joins,
                cases,
                &sources,
                &perm,
                has_default,
                rng,
                &mut txn,
                coroutine,
            )?,
        }
    } else {
        scan_path(
            wait,
            channels,
            completed_joins,
            cases,
            &sources,
            &perm,
            has_default,
            rng,
            &mut txn,
            coroutine,
        )?
    };
    Ok((outcome, txn))
}

fn commit_locked(
    wait: &mut WaitPlane,
    channels: &mut ChannelTable,
    completed: &[(CoroutineHandle, bool)],
    cases: &[SelectCase],
    perm: &[u32],
    has_default: bool,
    txn: &mut SelectTxn,
    coroutine: CoroutineHandle,
) -> Result<SelectOutcome, RawInvariant> {
    for &index in perm {
        let case = &cases[usize::try_from(index).expect("case 下标")];
        if case_ready(channels, wait, completed, case)? {
            txn.cas_winner(winner_unset(), encode_winner_case(case.index));
            txn.set_phase(phase_armed());
            return Ok(SelectOutcome::Case(case.index));
        }
    }
    if has_default && txn.cas_winner(winner_unset(), winner_default()) {
        txn.set_phase(phase_armed());
        return Ok(SelectOutcome::Default);
    }
    register_waiters(wait, channels, cases, coroutine)?;
    txn.set_phase(phase_armed());
    Ok(SelectOutcome::Parked)
}

fn scan_path(
    wait: &mut WaitPlane,
    channels: &mut ChannelTable,
    completed: &[(CoroutineHandle, bool)],
    cases: &[SelectCase],
    sources: &[WaitSourceId],
    perm: &[u32],
    has_default: bool,
    rng: &mut SelectRng,
    txn: &mut SelectTxn,
    coroutine: CoroutineHandle,
) -> Result<SelectOutcome, RawInvariant> {
    let mut ready = 0_u64;
    for source in sources {
        wait.lock(*source)?;
        for (bit, case) in cases.iter().enumerate() {
            if source_of(channels, wait, case)? == *source
                && case_ready(channels, wait, completed, case)?
            {
                ready |= 1 << bit;
            }
        }
        wait.unlock(*source)?;
    }
    let mut ready_cases = Vec::new();
    for &index in perm {
        let bit = usize::try_from(index).expect("case 下标");
        if ready & (1 << bit) != 0 {
            ready_cases.push(index);
        }
    }
    if let Some(&winner) = ready_cases.first() {
        let case = &cases[usize::try_from(winner).expect("case 下标")];
        let source = source_of(channels, wait, case)?;
        wait.lock(source)?;
        let still = case_ready(channels, wait, completed, case)?;
        if still {
            txn.cas_winner(winner_unset(), encode_winner_case(case.index));
            txn.set_phase(phase_armed());
            wait.unlock(source)?;
            return Ok(SelectOutcome::Case(case.index));
        }
        wait.unlock(source)?;
        let _ = rng;
    }
    if has_default && txn.cas_winner(winner_unset(), winner_default()) {
        txn.set_phase(phase_armed());
        return Ok(SelectOutcome::Default);
    }
    for source in sources {
        wait.lock(*source)?;
    }
    register_waiters(wait, channels, cases, coroutine)?;
    for source in sources.iter().rev() {
        wait.unlock(*source)?;
    }
    txn.set_phase(phase_armed());
    Ok(SelectOutcome::Parked)
}

fn register_waiters(
    wait: &mut WaitPlane,
    channels: &mut ChannelTable,
    cases: &[SelectCase],
    coroutine: CoroutineHandle,
) -> Result<Vec<WaitNodeHandle>, RawInvariant> {
    let generation = wait.begin_wait(coroutine)?;
    let mut nodes = Vec::with_capacity(cases.len());
    for case in cases {
        let source = source_of(channels, wait, case)?;
        let payload = match case.op {
            SelectOp::Send { payload, .. } => payload,
            SelectOp::Recv { .. } | SelectOp::Wait { .. } => 0,
        };
        let node = wait.alloc_node(
            coroutine,
            source,
            case.index,
            payload,
            0,
            WAIT_NODE_BUILDING,
            generation,
        )?;
        match case.op {
            SelectOp::Send { channel, .. } => {
                wait.enqueue(channels.send_queue_mut(channel)?, node)?;
                channels.sync_heads(channel)?;
            }
            SelectOp::Recv { channel } => {
                wait.enqueue(channels.recv_queue_mut(channel)?, node)?;
                channels.sync_heads(channel)?;
            }
            SelectOp::Wait { join } => {
                wait.enqueue_source(wait.join_source(join)?, node)?;
            }
        }
        wait.arm_building(node, false)?;
        nodes.push(node);
    }
    wait.arm_nodes(coroutine, nodes.clone());
    Ok(nodes)
}

/// Building 期 CAS winner；不得 ready 仍在登记的协程。
pub(crate) fn building_cas_winner(txn: &mut SelectTxn, case: u32) -> bool {
    if txn.phase() != phase_building() {
        return false;
    }
    txn.cas_winner(winner_unset(), encode_winner_case(case))
}
