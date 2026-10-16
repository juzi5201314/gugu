//! typed combining 契约段与冷操作平面的确定性测试。
//!
//! 覆盖契约目录与指纹、漂移拒绝、fast path 的单原子认领、争用挂链与下一轮服务、
//! 有限同类合并、轮次条目/字节预算、取消与超时、response 恰好一次、槽位 generation、
//! 记录池的非移动补充与资源耗尽。全部进程内运行。

use super::EXTENT_CLASS_LADDER;
use super::combining::{
    CombiningLimits, CombiningPlane, MergeGroup, OpTicket, OperationOutcome, OperationRequest,
    OperationState, OperationTag, PublishOutcome,
};
use super::combining_schema::{
    COMBINING_ALLOWED_USES, COMBINING_FAST_PATHS, COMBINING_FORBIDDEN_USES,
    COMBINING_MAX_OPERATION_CHUNKS, COMBINING_MERGE_LIMIT, COMBINING_MODES, COMBINING_PROFILE_NAME,
    COMBINING_PROFILE_REVISION, COMBINING_RECORD_BYTES, COMBINING_RECORD_CHUNK_ITEMS,
    COMBINING_REFILL_RESERVE_SLOTS, COMBINING_ROUND_BYTE_BUDGET, COMBINING_ROUND_ITEM_BUDGET,
    COMBINING_SCHEMA, COMBINING_STATISTICS, COMBINING_TIMEOUT_ROUNDS, COMBINING_WAIT_PATHS,
    COMBINING_WAIT_WAKE, CombiningDemand, CombiningMode, CombiningPolicyV1,
    CombiningRuntimeContract, OPERATION_OUTCOMES, OPERATION_STATES, OPERATION_TAGS,
};
use super::model::{FieldKind, MessageFieldSchema};

/// 世界用的 combiner 队列数：两条 raw、两条 resource 加一条 domain 队列。
const QUEUES: u32 = 5;

/// 由推导公式构造需求视图；与 `RuntimeRawContractV1` 的装配点同源。
fn demand() -> CombiningDemand {
    CombiningDemand::derive(
        2,
        12,
        4,
        u32::try_from(EXTENT_CLASS_LADDER.len()).expect("extent class 数量适配 u32"),
    )
}

/// 构建一份已验证的契约。
fn contract(mode: CombiningMode) -> CombiningRuntimeContract {
    CombiningRuntimeContract::build(demand(), CombiningPolicyV1 { mode }).expect("契约可构建")
}

/// 按契约参数构建平面。
fn plane(mode: CombiningMode) -> CombiningPlane {
    CombiningPlane::new(
        mode,
        QUEUES,
        CombiningLimits::from_contract(&contract(mode)),
    )
}

/// 构造一条平面请求；descriptor 与 scalar 只在 handler 里被解释，这里固定取值。
fn request(tag: OperationTag, owner: u32, merge_key: u64, bytes: u64) -> OperationRequest {
    OperationRequest {
        tag,
        owner,
        merge_key,
        descriptor: 7,
        scalar: 3,
        bytes,
    }
}

/// 提交一条走 fast path 的请求并返回句柄。
fn fast_path(plane: &mut CombiningPlane, bytes: u64) -> OpTicket {
    match plane
        .publish(request(OperationTag::PlatformTrim, 0, 1, bytes))
        .expect("publish")
    {
        PublishOutcome::FastPath(ticket) => ticket,
        other => panic!("期望无争用 fast path，实际 {other:?}"),
    }
}

/// 认领结果按「组内成员数」展开成应用结局。
fn executions(
    plane: &CombiningPlane,
    groups: &[MergeGroup],
    bytes: u64,
) -> Vec<(MergeGroup, Vec<OperationOutcome>)> {
    groups
        .iter()
        .map(|group| {
            let members = plane.groups_members(group);
            (
                group.clone(),
                vec![
                    OperationOutcome::Applied { bytes };
                    usize::try_from(members).expect("成员数")
                ],
            )
        })
        .collect()
}

/// 把静态目录复制成 `Vec<String>`。
fn catalog(names: &[&str]) -> Vec<String> {
    names.iter().map(|name| (*name).to_owned()).collect()
}

/// 契约目录闭合、需求推导一致、指纹稳定且随 mode 变化。
#[test]
fn contract_catalogs_stay_closed_and_versioned() {
    let direct = contract(CombiningMode::Direct);
    assert_eq!(COMBINING_SCHEMA, 1);
    assert_eq!(direct.schema, COMBINING_SCHEMA);
    assert_eq!(direct.profile(), COMBINING_PROFILE_NAME);
    assert_eq!(direct.profile_revision(), COMBINING_PROFILE_REVISION);
    assert_eq!(direct.mode(), CombiningMode::Direct);
    assert_eq!(direct.merge_limit(), COMBINING_MERGE_LIMIT);
    assert_eq!(direct.round_item_budget(), COMBINING_ROUND_ITEM_BUDGET);
    assert_eq!(direct.round_byte_budget(), COMBINING_ROUND_BYTE_BUDGET);
    assert_eq!(direct.timeout_rounds(), COMBINING_TIMEOUT_ROUNDS);
    assert_eq!(direct.record_bytes(), COMBINING_RECORD_BYTES);
    assert_eq!(direct.record_chunk_items(), COMBINING_RECORD_CHUNK_ITEMS);
    assert_eq!(
        direct.refill_reserve_slots(),
        COMBINING_REFILL_RESERVE_SLOTS
    );
    assert_eq!(
        direct.max_operation_chunks(),
        COMBINING_MAX_OPERATION_CHUNKS
    );
    assert_eq!(direct.operation_tag_count(), 4);
    // 目录逐项闭合：允许用途就是 tag 目录，禁止用途与同步路径都不能留空。
    assert_eq!(direct.operation_tags, catalog(&OPERATION_TAGS));
    assert_eq!(direct.allowed_uses, catalog(&COMBINING_ALLOWED_USES));
    assert_eq!(direct.allowed_uses, direct.operation_tags);
    assert_eq!(direct.forbidden_uses, catalog(&COMBINING_FORBIDDEN_USES));
    assert_eq!(direct.states, catalog(&OPERATION_STATES));
    assert_eq!(direct.outcomes, catalog(&OPERATION_OUTCOMES));
    assert_eq!(direct.fast_paths, catalog(&COMBINING_FAST_PATHS));
    assert_eq!(direct.wait_paths, catalog(&COMBINING_WAIT_PATHS));
    assert_eq!(direct.wait_wake, catalog(&COMBINING_WAIT_WAKE));
    assert_eq!(direct.statistics, catalog(&COMBINING_STATISTICS));
    assert_eq!(direct.transitions.len(), 7);
    let triggers: Vec<&str> = direct
        .transitions
        .iter()
        .map(|entry| entry.trigger.as_str())
        .collect();
    assert_eq!(
        triggers,
        vec![
            "publish",
            "claim",
            "cancel-before-claim",
            "timeout",
            "apply",
            "recycle",
            "recycle",
        ]
    );
    assert_eq!(direct.transitions[0].from, "free");
    assert_eq!(direct.transitions[0].to, "published");
    // record 字段只有标量、stable descriptor id 与 response slot，没有任何地址。
    let fields: Vec<(String, &str)> = direct
        .record_fields
        .iter()
        .map(|field| (field.name.clone(), field.kind.name()))
        .collect();
    assert_eq!(
        fields,
        vec![
            ("tag".to_owned(), "kind-tag"),
            ("state".to_owned(), "message-state"),
            ("generation".to_owned(), "generation"),
            ("owner-domain".to_owned(), "owner-domain"),
            ("owner-id".to_owned(), "owner-id"),
            ("merge-key".to_owned(), "merge-key"),
            ("descriptor".to_owned(), "descriptor-index"),
            ("scalar".to_owned(), "scalar"),
            ("bytes".to_owned(), "bytes"),
            ("response-slot".to_owned(), "response-slot"),
        ]
    );
    assert!(
        direct
            .record_fields
            .iter()
            .all(|field| !field.kind.carries_address())
    );
    // 需求由 raw 平面需求推导，记录数与 chunk 数都必须是同一个公式的输出。
    assert_eq!(direct.demand(), demand());
    assert_eq!(
        direct.demand().records,
        direct.demand().owners.max(1) * COMBINING_ROUND_ITEM_BUDGET
            + COMBINING_REFILL_RESERVE_SLOTS
    );
    assert_eq!(
        direct.demand().chunks,
        direct
            .demand()
            .records
            .div_ceil(COMBINING_RECORD_CHUNK_ITEMS)
    );
    // 指纹稳定；mode 是契约内容的一部分，因此 combined 的指纹必须不同。
    assert_eq!(
        direct.fingerprint(),
        contract(CombiningMode::Direct).fingerprint()
    );
    let combined = contract(CombiningMode::Combined);
    assert_ne!(direct.fingerprint(), combined.fingerprint());
    assert_eq!(combined.mode(), CombiningMode::Combined);
    assert!(COMBINING_MODES.contains(&CombiningMode::Combined.name()));
    let dump = combined.dump();
    assert!(
        dump.contains(
            "combining schema=1 profile=mosaic-combining revision=1 mode=combined tags=4"
        )
    );
    assert!(dump.contains(&format!("combining-tags {}", OPERATION_TAGS.join(","))));
    // 平面枚举的名字与判别值顺序必须与契约目录逐项一致。
    for (index, tag) in OperationTag::ALL.iter().enumerate() {
        assert_eq!(tag.index(), index);
        assert_eq!(tag.name(), OPERATION_TAGS[index]);
    }
    assert_eq!(OperationState::Free.name(), OPERATION_STATES[0]);
    assert_eq!(OperationState::Cancelled.name(), OPERATION_STATES[4]);
    assert_eq!(
        OperationOutcome::Applied { bytes: 0 }.name(),
        OPERATION_OUTCOMES[0]
    );
    assert_eq!(OperationOutcome::TimedOut.name(), OPERATION_OUTCOMES[2]);
}

/// 契约漂移必须被 verifier 拒绝：每一处改动都点名自己那一项。
#[test]
fn contract_rejects_drifted_catalogs() {
    let mut drifted = contract(CombiningMode::Direct);
    drifted.operation_tags.pop();
    assert!(
        drifted
            .verify()
            .unwrap_err()
            .message()
            .contains("冷操作 tag")
    );

    let mut drifted = contract(CombiningMode::Direct);
    drifted.allowed_uses.push("tlab-allocation".to_owned());
    assert!(drifted.verify().is_err());

    let mut drifted = contract(CombiningMode::Direct);
    drifted.merge_limit = 1;
    assert!(drifted.verify().is_err());

    let mut drifted = contract(CombiningMode::Direct);
    drifted.round_byte_budget = u64::from(COMBINING_RECORD_BYTES) * 2;
    assert!(drifted.verify().is_err());

    let mut drifted = contract(CombiningMode::Direct);
    drifted.record_fields[6] = MessageFieldSchema::new("descriptor", FieldKind::ManagedAddress);
    assert!(drifted.verify().unwrap_err().message().contains("携带地址"));

    let mut drifted = contract(CombiningMode::Direct);
    drifted.transitions[1].to = "retired".to_owned();
    assert!(
        drifted
            .verify()
            .unwrap_err()
            .message()
            .contains("未登记状态")
    );

    let mut drifted = contract(CombiningMode::Direct);
    drifted.demand.records += 1;
    assert!(drifted.verify().unwrap_err().message().contains("需求"));

    // 合法契约本身必须通过校验。
    contract(CombiningMode::Combined)
        .verify()
        .expect("合法契约");
}

/// 无争用路径占用单原子 claim 字，完成前后都不留挂链记录。
#[test]
fn fast_path_claim_is_single_atomic_and_leaves_no_pending() {
    let mut plane = plane(CombiningMode::Combined);
    let ticket = fast_path(&mut plane, 4096);
    let stats = plane.stats();
    assert_eq!(stats.requests, 1);
    assert_eq!(stats.fast_path_claims, 1);
    assert_eq!(stats.contended_parkings, 0);
    assert!(plane.claim_held());
    assert!(!plane.queue_open(0));
    assert_eq!(plane.pending_records(), 1);
    assert_eq!(plane.pending_bytes(), 4096);
    assert_eq!(plane.response(ticket).expect("response"), None);
    assert_eq!(plane.state_name(ticket).expect("状态"), "claimed");
    plane
        .complete(ticket, OperationOutcome::Applied { bytes: 4096 })
        .expect("complete");
    assert_eq!(
        plane.response(ticket).expect("response"),
        Some(OperationOutcome::Applied { bytes: 4096 })
    );
    assert!(!plane.claim_held());
    assert_eq!(plane.stats().executions, 1);
    plane.release(ticket).expect("release");
    assert_eq!(plane.pending_records(), 0);
    assert_eq!(plane.pending_bytes(), 0);
    plane.verify_pool().expect("记录池分区");
}

/// 一轮打开时的发布必须挂链，并由下一轮认领服务。
#[test]
fn publish_during_round_parks_and_next_round_serves_it() {
    let mut plane = plane(CombiningMode::Combined);
    let before = plane.stats();
    let first = plane
        .enqueue(request(OperationTag::PlatformTrim, 0, 1, 4096))
        .expect("enqueue");
    let groups = plane.begin_round(0).expect("开轮");
    assert_eq!(plane.groups_members(&groups[0]), 1);
    let parked = plane
        .publish(request(OperationTag::PlatformTrim, 0, 9, 4096))
        .expect("publish");
    let PublishOutcome::Parked(second) = parked else {
        panic!("期望争用挂链，实际 {parked:?}");
    };
    // 批量入链与争用发布各记一次挂链；fast path 一次都没有被占用。
    assert_eq!(
        plane.stats().contended_parkings - before.contended_parkings,
        2
    );
    assert_eq!(plane.stats().fast_path_claims, before.fast_path_claims);
    assert_eq!(plane.state_name(second).expect("状态"), "published");
    let planned = executions(&plane, &groups, 4096);
    let report = plane.end_round(0, &planned).expect("结束一轮");
    assert_eq!(report.executions, 1);
    assert_eq!(report.items, 1);
    assert_eq!(report.woken, 1);
    assert_eq!(
        plane.response(first).expect("response"),
        Some(OperationOutcome::Applied { bytes: 4096 })
    );
    // 第二轮认领挂链的请求；两条记录各自恰好执行一次。
    let groups = plane.begin_round(0).expect("第二轮");
    assert_eq!(plane.groups_members(&groups[0]), 1);
    let planned = executions(&plane, &groups, 4096);
    plane.end_round(0, &planned).expect("结束第二轮");
    assert_eq!(plane.stats().rounds, 2);
    assert_eq!(plane.stats().executions, 2);
    assert_eq!(
        plane.response(second).expect("response"),
        Some(OperationOutcome::Applied { bytes: 4096 })
    );
    plane.release(first).expect("release");
    plane.release(second).expect("release");
    assert_eq!(plane.pending_records(), 0);
    plane.verify_pool().expect("记录池分区");
}

/// 同 tag 的连续同类记录按 merge_limit 合并，组大小有限且不会跨类合并。
#[test]
fn merge_groups_are_finite_and_same_class() {
    let mut plane = plane(CombiningMode::Combined);
    for _ in 0..6 {
        plane
            .enqueue(request(OperationTag::PlatformTrim, 0, 1, 4096))
            .expect("enqueue");
    }
    for _ in 0..2 {
        plane
            .enqueue(request(OperationTag::PlatformTrim, 0, 2, 4096))
            .expect("enqueue");
    }
    let groups = plane.begin_round(0).expect("开轮");
    let sizes: Vec<u32> = groups
        .iter()
        .map(|group| plane.groups_members(group))
        .collect();
    assert_eq!(sizes, vec![4, 2, 2]);
    for group in &groups {
        assert_eq!(plane.groups_tag(group), OperationTag::PlatformTrim);
    }
    let planned = executions(&plane, &groups, 4096);
    let report = plane.end_round(0, &planned).expect("结束一轮");
    assert_eq!(report.executions, 3);
    // 合并省下的执行次数是「成员数减一」之和：8 条记录 3 次执行省下 5 次。
    assert_eq!(report.merged, 5);
    assert_eq!(plane.stats().merged_requests, 5);
    assert_eq!(plane.stats().executions + plane.stats().merged_requests, 8);
    plane.verify_pool().expect("记录池分区");
}

/// 轮次预算同时约束条目数与请求字节数。
#[test]
fn round_budget_bounds_items_and_bytes() {
    let mut item_budget_plane = plane(CombiningMode::Combined);
    item_budget_plane.grow_operation_pool().expect("补充 chunk");
    for _ in 0..20 {
        item_budget_plane
            .enqueue(request(OperationTag::PlatformTrim, 0, 1, 4096))
            .expect("enqueue");
    }
    let groups = item_budget_plane.begin_round(0).expect("开轮");
    let items: u32 = groups
        .iter()
        .map(|group| item_budget_plane.groups_members(group))
        .sum();
    assert_eq!(items, COMBINING_ROUND_ITEM_BUDGET);
    let planned = executions(&item_budget_plane, &groups, 4096);
    item_budget_plane.end_round(0, &planned).expect("结束一轮");
    // 未认领的记录仍在链上等待；已完成的记录仍占用槽位直到回收。
    assert_eq!(item_budget_plane.pending_records(), 20);

    // 每条请求占半个字节预算时，首轮恰好消费满预算：两条。
    let mut byte_budget_plane = plane(CombiningMode::Combined);
    byte_budget_plane.grow_operation_pool().expect("补充 chunk");
    for _ in 0..20 {
        byte_budget_plane
            .enqueue(request(
                OperationTag::PlatformTrim,
                0,
                1,
                COMBINING_ROUND_BYTE_BUDGET / 2,
            ))
            .expect("enqueue");
    }
    let groups = byte_budget_plane.begin_round(0).expect("开轮");
    let items: u32 = groups
        .iter()
        .map(|group| byte_budget_plane.groups_members(group))
        .sum();
    assert_eq!(items, 2);
    let planned = executions(&byte_budget_plane, &groups, 0);
    let report = byte_budget_plane.end_round(0, &planned).expect("结束一轮");
    assert_eq!(report.items, 2);
    // 报告的字节是本轮认领记录携带的请求字节，不含记录规范槽。
    assert_eq!(report.bytes, COMBINING_ROUND_BYTE_BUDGET);
    byte_budget_plane.verify_pool().expect("记录池分区");
}

/// 取消只允许发生在认领之前；已取消的记录由下一轮回收且没有任何 handler。
#[test]
fn cancel_before_claim_and_refuse_after_claim() {
    let mut plane = plane(CombiningMode::Combined);
    let ticket = plane
        .enqueue(request(OperationTag::PlatformTrim, 0, 1, 4096))
        .expect("enqueue");
    assert_eq!(
        plane.cancel(ticket).expect("取消"),
        OperationOutcome::Cancelled
    );
    assert_eq!(plane.stats().cancellations, 1);
    assert_eq!(
        plane.response(ticket).expect("response"),
        Some(OperationOutcome::Cancelled)
    );
    // 已取消的记录仍在链上，不能由请求者回收：回收会让链指向被复用的槽位。
    assert!(plane.release(ticket).is_err());
    let groups = plane.begin_round(0).expect("开轮");
    assert!(groups.is_empty());
    let report = plane.end_round(0, &[]).expect("结束一轮");
    assert_eq!(report.cancellations, 1);
    assert_eq!(report.items, 0);
    assert_eq!(plane.pending_records(), 0);
    // 槽位已经回收：旧句柄按 generation 失效。
    assert!(plane.response(ticket).is_err());
    plane.verify_pool().expect("记录池分区");

    // 已认领的记录不能被取消。
    let claimed = fast_path(&mut plane, 4096);
    assert!(plane.cancel(claimed).is_err());
    plane
        .complete(claimed, OperationOutcome::Applied { bytes: 4096 })
        .expect("complete");
    plane.release(claimed).expect("release");
}

/// 在链上等待到超时轮数的记录以 `TimedOut` 收尾，并且只超时一次。
#[test]
fn timeout_after_fixed_rounds() {
    let mut plane = plane(CombiningMode::Combined);
    let total = COMBINING_TIMEOUT_ROUNDS * COMBINING_ROUND_ITEM_BUDGET + 1;
    while plane.pool_slots() < total {
        plane.grow_operation_pool().expect("补充 chunk");
    }
    let mut tickets = Vec::new();
    for _ in 0..total {
        tickets.push(
            plane
                .enqueue(request(OperationTag::PlatformTrim, 0, 1, 0))
                .expect("enqueue"),
        );
    }
    let mut timeouts = 0;
    for _ in 0..COMBINING_TIMEOUT_ROUNDS {
        let groups = plane.begin_round(0).expect("开轮");
        let planned = executions(&plane, &groups, 0);
        let report = plane.end_round(0, &planned).expect("结束一轮");
        timeouts += report.timeouts;
    }
    assert_eq!(timeouts, 1);
    assert_eq!(plane.stats().timeouts, 1);
    // 前 8 轮各认领一轮预算的记录，最后一条在链上等满超时轮数后被收尾。
    let last = tickets[tickets.len() - 1];
    assert_eq!(
        plane
            .response(tickets[tickets.len() - 2])
            .expect("response"),
        Some(OperationOutcome::Applied { bytes: 0 })
    );
    assert_eq!(plane.state_name(last).expect("状态"), "completed");
    assert_eq!(
        plane.response(last).expect("response"),
        Some(OperationOutcome::TimedOut)
    );
    plane.release(last).expect("release");
    plane.verify_pool().expect("记录池分区");
}

/// response 恰好发布一次；重复完成、重复取消与已回收句柄都按不变量失败。
#[test]
fn response_publication_is_ordered_and_exactly_once() {
    let mut plane = plane(CombiningMode::Combined);
    let ticket = fast_path(&mut plane, 128);
    assert_eq!(plane.response(ticket).expect("response"), None);
    plane
        .complete(ticket, OperationOutcome::Applied { bytes: 128 })
        .expect("complete");
    assert_eq!(
        plane.response(ticket).expect("response"),
        Some(OperationOutcome::Applied { bytes: 128 })
    );
    // 同一条记录不能被完成两次：第二次调用时认领字已经释放。
    assert!(
        plane
            .complete(ticket, OperationOutcome::Applied { bytes: 128 })
            .is_err()
    );

    // 轮次路径同样只发布一次：结束后再完成或取消都失败。
    let parked = plane
        .enqueue(request(OperationTag::PlatformTrim, 0, 1, 0))
        .expect("enqueue");
    let groups = plane.begin_round(0).expect("开轮");
    let planned = executions(&plane, &groups, 0);
    plane.end_round(0, &planned).expect("结束一轮");
    assert_eq!(
        plane.response(parked).expect("response"),
        Some(OperationOutcome::Applied { bytes: 0 })
    );
    assert!(
        plane
            .complete(parked, OperationOutcome::Applied { bytes: 0 })
            .is_err()
    );
    assert!(plane.cancel(parked).is_err());

    plane.release(parked).expect("release");
    plane.release(ticket).expect("release");
    assert_eq!(
        plane.response(ticket).expect_err("已回收句柄").message(),
        "combining ticket 属于已被回收的记录槽"
    );
    assert!(plane.release(ticket).is_err());
}

/// 槽位回收后 generation 推进，同一槽位可以复用而不影响新句柄。
#[test]
fn recycled_slots_advance_generation() {
    let mut plane = plane(CombiningMode::Combined);
    let first = fast_path(&mut plane, 64);
    plane
        .complete(first, OperationOutcome::Applied { bytes: 64 })
        .expect("complete");
    plane.release(first).expect("release");
    let second = fast_path(&mut plane, 64);
    assert!(second.generation() > first.generation());
    assert!(plane.response(first).is_err());
    assert_eq!(plane.state_name(second).expect("状态"), "claimed");
    assert_eq!(plane.pending_records(), 1);
    plane
        .complete(second, OperationOutcome::Applied { bytes: 64 })
        .expect("complete");
    plane.release(second).expect("release");
    assert_eq!(plane.pending_records(), 0);
    plane.verify_pool().expect("记录池分区");
}

/// 记录池按 chunk 整块补充，已发放句柄的槽位不移动；chunk 上限是硬约束。
#[test]
fn operation_pool_is_non_moving_and_refills_in_chunks() {
    let limits = CombiningLimits {
        max_operation_chunks: 2,
        ..CombiningLimits::from_contract(&contract(CombiningMode::Combined))
    };
    let mut plane = CombiningPlane::new(CombiningMode::Combined, QUEUES, limits);
    assert_eq!(plane.chunk_count(), 1);
    assert_eq!(plane.pool_slots(), COMBINING_RECORD_CHUNK_ITEMS);
    assert!(!plane.needs_refill());
    let ticket = fast_path(&mut plane, 256);
    let before = plane.operation(ticket).expect("操作视图");
    let added = plane.grow_operation_pool().expect("补充 chunk");
    assert_eq!(added, COMBINING_RECORD_CHUNK_ITEMS);
    assert_eq!(plane.chunk_count(), 2);
    assert_eq!(plane.pool_slots(), COMBINING_RECORD_CHUNK_ITEMS * 2);
    // 旧句柄仍指向同一条记录：chunk 是整块 push 的，池内记录永不移动。
    assert_eq!(plane.operation(ticket).expect("操作视图"), before);
    assert!(plane.grow_operation_pool().is_err());
    plane
        .complete(ticket, OperationOutcome::Applied { bytes: 256 })
        .expect("complete");
    plane.release(ticket).expect("release");
    plane.verify_pool().expect("记录池分区");
}

/// 记录池耗尽是不变量失败而不是静默丢弃；refill 预留槽位是唯一例外。
#[test]
fn resource_exhaustion_is_an_invariant_not_a_silent_drop() {
    let limits = CombiningLimits {
        record_chunk_items: 4,
        refill_reserve_slots: 1,
        ..CombiningLimits::from_contract(&contract(CombiningMode::Combined))
    };
    let mut plane = CombiningPlane::new(CombiningMode::Combined, QUEUES, limits);
    assert_eq!(plane.pool_slots(), 4);
    for _ in 0..3 {
        plane
            .enqueue(request(OperationTag::PlatformTrim, 0, 1, 0))
            .expect("enqueue");
    }
    assert!(plane.needs_refill());
    let error = plane
        .enqueue(request(OperationTag::ExtentCoalesce, 0, 1, 0))
        .expect_err("普通槽位用尽");
    assert_eq!(
        error.message(),
        "operation 记录池已满；调用方必须先执行 global-range-refill"
    );
    assert!(
        plane
            .publish(request(OperationTag::PlatformTrim, 0, 1, 0))
            .is_err()
    );
    // refill 自己可以使用 chunk 尾部的预留槽位，因此补充记录池这条出路不会被卡住。
    let refill = plane
        .enqueue(request(OperationTag::GlobalRangeRefill, 0, 0, 0))
        .expect("refill 使用预留槽位");
    assert_eq!(
        plane.state_name(refill).expect("状态"),
        OperationState::Published.name()
    );
    assert!(
        plane
            .enqueue(request(OperationTag::GlobalRangeRefill, 0, 0, 0))
            .is_err()
    );
    // 已入链的四条记录仍能在一轮里被认领并结清。
    let groups = plane.begin_round(0).expect("开轮");
    let planned = executions(&plane, &groups, 0);
    let report = plane.end_round(0, &planned).expect("结束一轮");
    assert_eq!(report.items, 4);
    plane.verify_pool().expect("记录池分区");
}
