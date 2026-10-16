//! 候选回收平面的确定性回归：相位顺序、试验删除、SCC 死亡组判定、验证 gate 与恰好一次确认。
//!
//! 全部在进程内运行：快照与边记录都是显式输入，因此断言的是**决议与动作序列**，不是实现细节。
//! 每个用例都用两个 quantum 驱动同一场景，钉住「任意预算下结论相同」这一可恢复性契约。

use super::candidate::{
    BlockPairCount, CandidateAction, CandidateInputs, CandidatePlane, CandidateStats,
};
use super::candidate_schema::{
    CANDIDATE_SCHEMA, CandidateAliveReason, CandidatePhase, CandidateSnapshot, CandidateVerdict,
};
use super::edge_schema::{EDGE_CANDIDATE_SCHEMA, EDGE_PHASES};
use super::local_heap::ManagedBlockId;

/// 一个 block 身份；descriptor 就是 arena 号。
fn block(descriptor: u32, index: u32) -> ManagedBlockId {
    ManagedBlockId::new(descriptor, index).expect("块身份可构造")
}

/// 一个全部 gate 都通过的快照。
fn clear_snapshot(block: ManagedBlockId) -> CandidateSnapshot {
    CandidateSnapshot {
        block,
        generation: 1,
        incoming: 0,
        incoming_leases: 0,
        allocator_leases: 0,
        scanner_leases: 0,
        evacuation_leases: 0,
        mutation_version: 1,
        pinned: 0,
        resources: 0,
        marked: 0,
    }
}

/// 一条 block 对计数。
fn edge(source: ManagedBlockId, target: ManagedBlockId, count: u32) -> BlockPairCount {
    BlockPairCount {
        source,
        target,
        count,
    }
}

/// 一次被驱动的结果。
struct Driven {
    actions: Vec<CandidateAction>,
    verdicts: Vec<(u32, CandidateVerdict)>,
    stats: CandidateStats,
}

/// 驱动平面处理动作并回填确认，直到没有 job 为止。
fn drive(plane: &mut CandidatePlane, quantum: u32, inputs: &CandidateInputs<'_>) -> Driven {
    let mut actions = Vec::new();
    let mut verdicts = Vec::new();
    for _ in 0..256 {
        let report = plane.advance(quantum, inputs).expect("候选推进成功");
        verdicts.extend(report.verdicts.iter().cloned());
        for action in &report.actions {
            match action {
                CandidateAction::CommitGroup { blocks } => {
                    for (block, generation) in blocks {
                        plane
                            .note_swept(*block, *generation)
                            .expect("sweep 恰好一次");
                    }
                }
                CandidateAction::ReleaseBlock { block, generation } => {
                    plane
                        .note_released(*block, *generation)
                        .expect("release 恰好一次");
                }
                _ => {}
            }
        }
        actions.extend(report.actions);
        if plane.job_count() == 0 && plane.dirty_count() == 0 {
            return Driven {
                actions,
                verdicts,
                stats: plane.stats(),
            };
        }
    }
    panic!("候选平面必须在有限批次内收敛");
}

#[test]
fn phase_catalog_matches_edge_contract() {
    // 相位目录只有一份：候选枚举必须与 edge 契约的目录逐项同名同序。
    for (index, phase) in CandidatePhase::ALL.iter().enumerate() {
        assert_eq!(phase.name(), EDGE_PHASES[index], "相位名与契约目录不一致");
        assert_eq!(CandidatePhase::from_index(index), Some(*phase));
    }
    assert_eq!(CandidatePhase::from_index(EDGE_PHASES.len()), None);
    assert_eq!(
        CANDIDATE_SCHEMA, EDGE_CANDIDATE_SCHEMA,
        "候选决议 schema 必须与契约登记值相等"
    );
}

#[test]
fn dead_cycle_group_commits_and_releases_exactly_once() {
    let first = block(1, 0);
    let second = block(1, 1);
    let snapshots = [clear_snapshot(first), clear_snapshot(second)];
    // 两个 block 互相引用：入边计数全部来自组内，组外没有任何引用。
    let edges = [edge(first, second, 1), edge(second, first, 1)];
    let inputs = CandidateInputs {
        snapshots: &snapshots,
        edges: &edges,
    };
    for quantum in [1_u32, 4096] {
        let mut plane = CandidatePlane::new();
        plane.note_dirty(first);
        plane.note_dirty(second);
        let driven = drive(&mut plane, quantum, &inputs);
        assert_eq!(
            driven.verdicts,
            vec![(0, CandidateVerdict::Dead(vec![(first, 1), (second, 1)]))],
            "整组必须一次判定死亡（quantum={quantum}）"
        );
        let commits = driven
            .actions
            .iter()
            .filter(|action| matches!(action, CandidateAction::CommitGroup { .. }))
            .count();
        assert_eq!(commits, 1, "整组只能有一个线性化提交点");
        assert_eq!(
            driven
                .actions
                .iter()
                .filter(|action| matches!(action, CandidateAction::BindBlock { .. }))
                .count(),
            2
        );
        assert!(
            driven
                .actions
                .iter()
                .all(|action| !matches!(action, CandidateAction::DropOutgoing { .. })),
            "没有组外引用就不该产生出边减量"
        );
        assert_eq!(driven.stats.jobs_completed, 1);
        assert_eq!(driven.stats.jobs_invalidated, 0);
        assert_eq!(driven.stats.blocks_swept, 2);
        assert_eq!(driven.stats.blocks_released, 2);
        assert_eq!(driven.stats.dead_groups, 1);
        assert_eq!(plane.bound_count(), 0, "job 结束后必须解绑");
    }
}

#[test]
fn external_incoming_keeps_the_group_alive_without_partial_commit() {
    let first = block(1, 0);
    let second = block(1, 1);
    let mut snapshot = clear_snapshot(first);
    // 组内互引只解释了其中一条入边；剩下一条来自组外，整组不能死。
    snapshot.incoming = 2;
    let snapshots = [snapshot, clear_snapshot(second)];
    let edges = [edge(first, second, 1), edge(second, first, 1)];
    let inputs = CandidateInputs {
        snapshots: &snapshots,
        edges: &edges,
    };
    for quantum in [1_u32, 4096] {
        let mut plane = CandidatePlane::new();
        plane.note_dirty(first);
        plane.note_dirty(second);
        let driven = drive(&mut plane, quantum, &inputs);
        assert_eq!(
            driven.verdicts,
            vec![(
                0,
                CandidateVerdict::Alive {
                    reason: CandidateAliveReason::ExternalIncoming,
                    evidence: first,
                }
            )],
            "组外引用必须让整组保持存活（quantum={quantum}）"
        );
        assert!(
            driven
                .actions
                .iter()
                .all(|action| !matches!(action, CandidateAction::CommitGroup { .. })),
            "存活组不得产生任何提交动作"
        );
        assert_eq!(driven.stats.blocks_swept, 0);
        assert_eq!(driven.stats.blocks_released, 0);
    }
}

#[test]
fn liveness_splits_dead_and_live_components_inside_one_group() {
    // 组 {A, B, C}：A↔B 互引，A→C 指向 C；C 有两条组外入边，减去 A→C 后仍为正。
    // 存活种子是 C；沿定向边传播不会回到 A/B，因此只有 A、B 是死亡组。
    let a = block(1, 0);
    let b = block(1, 1);
    let c = block(1, 2);
    let mut c_snapshot = clear_snapshot(c);
    c_snapshot.incoming = 2;
    let snapshots = [clear_snapshot(a), clear_snapshot(b), c_snapshot];
    let edges = [edge(a, b, 1), edge(b, a, 1), edge(a, c, 1)];
    let inputs = CandidateInputs {
        snapshots: &snapshots,
        edges: &edges,
    };
    let mut plane = CandidatePlane::new();
    for target in [a, b, c] {
        plane.note_dirty(target);
    }
    let driven = drive(&mut plane, 2, &inputs);
    assert_eq!(
        driven.verdicts,
        vec![(0, CandidateVerdict::Dead(vec![(a, 1), (b, 1)]))],
        "只有未被存活传播覆盖的 SCC 才是死亡组"
    );
    assert!(
        driven.actions.iter().any(|action| matches!(
            action,
            CandidateAction::InvalidateGroup { blocks } if blocks == &vec![c]
        )),
        "存活成员 C 必须退回 active"
    );
    assert!(
        driven.actions.iter().any(|action| matches!(
            action,
            CandidateAction::DropOutgoing { source, target, count }
                if *source == a && *target == c && *count == 1
        )),
        "死亡组的出边必须一起减掉"
    );
    assert_eq!(driven.stats.blocks_swept, 2, "只 sweep 死亡组");
    assert_eq!(driven.stats.blocks_released, 2);
}

#[test]
fn validation_gates_block_commit() {
    let first = block(1, 0);
    let second = block(1, 1);
    // 两个零入边 block 互不成环也会各自成为死亡组；gate 挂在第二个成员上，验证相位必须拦下它。
    let cases = [
        (
            "marked",
            CandidateAliveReason::Marked,
            CandidateSnapshot {
                marked: 1,
                ..clear_snapshot(second)
            },
        ),
        (
            "pinned",
            CandidateAliveReason::Pinned,
            CandidateSnapshot {
                pinned: 1,
                ..clear_snapshot(second)
            },
        ),
        (
            "resource",
            CandidateAliveReason::Resource,
            CandidateSnapshot {
                resources: 1,
                ..clear_snapshot(second)
            },
        ),
    ];
    for (name, reason, gated) in cases {
        let snapshots = [clear_snapshot(first), gated];
        let edges = [edge(first, second, 1)];
        let inputs = CandidateInputs {
            snapshots: &snapshots,
            edges: &edges,
        };
        for quantum in [1_u32, 4096] {
            let mut plane = CandidatePlane::new();
            plane.note_dirty(first);
            let driven = drive(&mut plane, quantum, &inputs);
            assert!(
                driven.actions.iter().all(|action| !matches!(
                    action,
                    CandidateAction::CommitGroup { .. } | CandidateAction::ReleaseBlock { .. }
                )),
                "{name}: 命中 gate 的组不得提交或释放（quantum={quantum}）"
            );
            assert!(
                driven.verdicts.iter().any(|(_, verdict)| matches!(
                    verdict,
                    CandidateVerdict::Alive { reason: hit, evidence } if *hit == reason && *evidence == second
                )),
                "{name}: 决议必须是存活并点名命中 gate 的成员（quantum={quantum}）"
            );
            assert_eq!(driven.stats.blocks_swept, 0, "{name}");
        }
    }
}

#[test]
fn lease_gauges_discovery_and_validation() {
    let first = block(1, 0);
    let second = block(1, 1);
    // 建组时 lease 归零；验证相位前的刷新快照把 lease 拉起来，必须拦下提交。
    let clean = [clear_snapshot(first), clear_snapshot(second)];
    let busy = [
        clear_snapshot(first),
        CandidateSnapshot {
            scanner_leases: 1,
            ..clear_snapshot(second)
        },
    ];
    let edges = [edge(first, second, 1)];
    let mut plane = CandidatePlane::new();
    plane.note_dirty(first);
    let discover = CandidateInputs {
        snapshots: &clean,
        edges: &edges,
    };
    let refreshed = CandidateInputs {
        snapshots: &busy,
        edges: &edges,
    };
    // 用干净快照推进到验证相位之前：lease 门禁必须在建组之后才生效。
    for _ in 0..64 {
        if plane.phase_of(0) == Some(CandidatePhase::Validate) {
            break;
        }
        plane.advance(1, &discover).expect("建组推进成功");
    }
    assert_eq!(
        plane.phase_of(0),
        Some(CandidatePhase::Validate),
        "干净快照必须能推进到验证相位"
    );
    let mut verdicts = Vec::new();
    for _ in 0..256 {
        let report = plane.advance(4, &refreshed).expect("推进成功");
        verdicts.extend(report.verdicts.iter().cloned());
        if plane.job_count() == 0 {
            break;
        }
    }
    assert!(
        verdicts.iter().any(|(_, verdict)| matches!(
            verdict,
            CandidateVerdict::Alive {
                reason: CandidateAliveReason::LeaseBusy,
                evidence
            } if *evidence == second
        )),
        "扫描 lease 未归零必须让组失效"
    );
    assert!(
        verdicts.iter().all(|(_, verdict)| !verdict.is_dead()),
        "lease 未归零时不得产生死亡决议"
    );

    // lease 未归零的 block 连组都建不起来：它保持 dirty，等下一次推进再判定。
    let mut plane = CandidatePlane::new();
    plane.note_dirty(second);
    let init = CandidateInputs {
        snapshots: &busy,
        edges: &[],
    };
    let report = plane.advance(64, &init).expect("推进成功");
    assert_eq!(plane.job_count(), 0, "lease 未归零不能建组");
    assert_eq!(plane.dirty_count(), 1, "它必须保持 dirty");
    assert!(report.actions.is_empty());
}

#[test]
fn mutator_invalidation_retreats_the_bound_group() {
    let first = block(1, 0);
    let second = block(1, 1);
    let snapshots = [clear_snapshot(first), clear_snapshot(second)];
    let edges = [edge(first, second, 1), edge(second, first, 1)];
    let inputs = CandidateInputs {
        snapshots: &snapshots,
        edges: &edges,
    };
    let mut plane = CandidatePlane::new();
    plane.note_dirty(first);
    plane.note_dirty(second);
    // 只消费一个单位：第一个 seed 建组，第二个还留在 dirty 集合里。
    let _ = plane.advance(1, &inputs).expect("首轮推进成功");
    assert_eq!(plane.job_count(), 1, "job 必须已建立");
    assert_eq!(plane.dirty_count(), 1, "第二个 seed 还没轮到");
    assert!(
        plane.job_of_block(first).is_some(),
        "第一个 block 必须已绑定"
    );
    // 已绑定 block 的重复改动只置失效位，不进 dirty 集合。
    plane.note_dirty(first);
    plane.note_dirty(first);
    assert_eq!(
        plane.dirty_count(),
        1,
        "已绑定 block 的改动不得进入 dirty 集合"
    );
    assert_eq!(
        plane.phase_of(0),
        Some(CandidatePhase::Invalidate),
        "本地失效必须立即落到 job 相位上"
    );
    // 失效批次：必须退回 active，且此时不可能已经 sweep 过任何 block。
    let mut retreated = Vec::new();
    for _ in 0..256 {
        let report = plane.advance(4, &inputs).expect("推进成功");
        for action in &report.actions {
            match action {
                CandidateAction::InvalidateGroup { blocks } => {
                    assert_eq!(
                        plane.stats().blocks_swept,
                        0,
                        "失效批次不得已经执行过 sweep"
                    );
                    retreated.push(blocks.clone());
                }
                CandidateAction::CommitGroup { blocks } => {
                    for (block, generation) in blocks {
                        plane
                            .note_swept(*block, *generation)
                            .expect("sweep 恰好一次");
                    }
                }
                CandidateAction::ReleaseBlock { block, generation } => {
                    plane
                        .note_released(*block, *generation)
                        .expect("release 恰好一次");
                }
                _ => {}
            }
        }
        if plane.job_count() == 0 && plane.dirty_count() == 0 {
            break;
        }
    }
    assert!(
        retreated.iter().any(|blocks| blocks.contains(&first)),
        "被 mutator 改动的成员必须随绑定组退回 active"
    );
    assert!(plane.stats().jobs_invalidated >= 1, "失效必须计入统计");
}

#[test]
fn sweep_and_release_confirmations_are_exactly_once() {
    let first = block(1, 0);
    let snapshots = [clear_snapshot(first)];
    let inputs = CandidateInputs {
        snapshots: &snapshots,
        edges: &[],
    };
    let mut plane = CandidatePlane::new();
    plane.note_dirty(first);
    // 单个零入边 block：组内没有任何入边，试验删除后仍为零，因此可以直接死亡。
    let mut committed = Vec::new();
    for _ in 0..64 {
        let report = plane.advance(4096, &inputs).expect("推进成功");
        for action in &report.actions {
            if let CandidateAction::CommitGroup { blocks } = action {
                committed.extend(blocks.iter().copied());
            }
        }
        if !committed.is_empty() {
            break;
        }
    }
    assert_eq!(
        committed,
        vec![(first, 1)],
        "单个零入边 block 必须被判定死亡"
    );
    plane.note_swept(first, 1).expect("首次 sweep 确认成功");
    assert!(
        plane.note_swept(first, 1).is_err(),
        "同一 block 不能被 sweep 两次"
    );
    assert!(
        plane.note_swept(first, 2).is_err(),
        "世代不符的确认必须失败"
    );
    assert!(
        plane.note_released(first, 1).is_err(),
        "还没到 Release 相位就不能确认释放"
    );
    // 推进到 Release 相位并确认恰好一次。
    let mut released = false;
    for _ in 0..64 {
        let report = plane.advance(4096, &inputs).expect("推进成功");
        for action in &report.actions {
            if let CandidateAction::ReleaseBlock { block, generation } = action {
                assert_eq!(*block, first);
                plane
                    .note_released(*block, *generation)
                    .expect("首次 release 确认");
                released = true;
            }
        }
        if released {
            break;
        }
    }
    assert!(released, "提交后必须进入 Release 相位");
    assert!(
        plane.note_released(first, 1).is_err(),
        "同一 block 不能被 release 两次"
    );
    let _ = plane.advance(4096, &inputs).expect("收尾推进成功");
    assert_eq!(plane.job_count(), 0, "确认齐全后 job 必须结束");
    assert_eq!(plane.stats().jobs_completed, 1);
}
