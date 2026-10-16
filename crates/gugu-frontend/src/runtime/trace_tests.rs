//! 精确追踪游标的确定性回归：一次性扫描与逐单位预算扫描必须访问完全相同的真实偏移。
//!
//! 测试直接构造已验证形态的 descriptor 与类型表，因此断言的是**访问到的 payload 偏移与值**，
//! 不是实现细节；Bitmap 两张位图的同位号语义与 SWITCH 分支返回位置都在这里被钉住。

use super::gc_metadata_schema::{TraceKind, TraceOp, encode_uleb};
use super::gc_metadata_section::{GcRuntimeMetadata, GcRuntimeType};
use super::gc_trace::{
    ARENA_HEADER_BYTES, ARENA_SLOT_RECORD_BYTES, ObjectTraceCursor, TRACE_MAX_FRAMES, TraceCursor,
    TraceProgress, WorkBudget, walk_descriptor,
};

/// 把一个 descriptor 包成只有一条类型的类型表；顺序即 `TypeId`。
fn table_of(descriptor: Vec<u8>) -> GcRuntimeMetadata {
    GcRuntimeMetadata {
        types: vec![GcRuntimeType {
            name: "edge-node".to_owned(),
            size: 64,
            align: 8,
            flags: 1,
            trace: descriptor,
        }],
    }
}

/// 构造一个 bitmap descriptor：`direct` 与 `interior` 给出被标记的 word 编号。
fn bitmap_descriptor(word_count: u32, direct: &[u32], interior: &[u32]) -> Vec<u8> {
    let bytes = usize::try_from(word_count.div_ceil(8)).expect("bitmap 字节数");
    let mut out = vec![0u8; (8 + bytes * 2).next_multiple_of(4)];
    out[0] = TraceKind::Bitmap as u8;
    out[4..8].copy_from_slice(&word_count.to_le_bytes());
    for word in direct {
        out[8 + (word / 8) as usize] |= 1 << (word % 8);
    }
    for word in interior {
        out[8 + bytes + (word / 8) as usize] |= 1 << (word % 8);
    }
    out
}

/// 构造一个 program descriptor：`body` 是 opcode 序列（不需要自带长度前缀）。
fn program_descriptor(body: &[u8]) -> Vec<u8> {
    let mut out = vec![TraceKind::Program as u8];
    out.extend_from_slice(
        &u32::try_from(body.len())
            .expect("长度适配 u32")
            .to_le_bytes(),
    );
    out.extend_from_slice(body);
    out
}

/// 用一次性入口扫描并收集 `(word 偏移, 值)`。
fn collect_once(types: &GcRuntimeMetadata, payload: &mut [u8]) -> Vec<(u64, u64)> {
    let cap = payload.len() / 8 + 1;
    let mut visited = Vec::new();
    walk_descriptor(0, types, payload, 0x1000, &mut |word, address| {
        assert!(
            visited.len() < cap,
            "一次扫描访问的 word 数必须不超过 payload 的 word 数"
        );
        visited.push((address - 0x1000, u64::from_le_bytes(*word)));
        Ok(())
    })
    .expect("一次性扫描成功");
    visited
}

/// 用预算 1 的游标扫描并收集 `(word 偏移, 值)`，同时记录每个切片的工作量。
///
/// 切片数有明确上限：合法 descriptor 的游标必须在有限步内完成，超出即视为不收敛，测试因此
/// 以断言失败而不是无限循环结束。
fn collect_batched(types: &GcRuntimeMetadata, payload: &mut [u8]) -> (Vec<(u64, u64)>, Vec<u32>) {
    let cap = payload.len() / 8 + 1;
    let mut cursor = TraceCursor::new();
    let mut visited = Vec::new();
    let mut slices = Vec::new();
    loop {
        assert!(slices.len() < cap * 8 + 64, "游标必须在有限切片内完成");
        let mut budget = WorkBudget::new(1);
        let progress = cursor
            .step(
                0,
                types,
                payload,
                0x1000,
                &mut budget,
                &mut |word, address| {
                    assert!(
                        visited.len() < cap,
                        "分批扫描不得重复访问超过 payload 的 word 数"
                    );
                    visited.push((address - 0x1000, u64::from_le_bytes(*word)));
                    Ok(())
                },
            )
            .expect("分批扫描成功");
        slices.push(budget.spent());
        if progress == TraceProgress::Complete {
            return (visited, slices);
        }
    }
}

#[test]
fn bitmap_direct_and_interior_bits_share_one_word_numbering() {
    // word 3 有 direct 位、word 4 有 interior 位：两者必须是相邻 word，而不是相隔八个。
    let types = table_of(bitmap_descriptor(16, &[3], &[4]));
    let mut payload = vec![0u8; 16 * 8];
    payload[3 * 8] = 0xAA;
    payload[4 * 8] = 0xBB;
    let visited = collect_once(&types, &mut payload);
    assert_eq!(
        visited,
        vec![(3 * 8, 0xAA), (4 * 8, 0xBB)],
        "interior 位必须映射到同一个 word 编号，不能左移八位"
    );
    // 分批扫描访问完全相同的偏移。
    let (batched, _) = collect_batched(&types, &mut payload);
    assert_eq!(batched, visited);
}

#[test]
fn switch_selected_case_returns_after_the_whole_encoding() {
    // SWITCH 的前两个 case 都命中同一个 tag；尾部是 default，它在分支结束后必须继续执行。
    let mut body = vec![TraceOp::Switch as u8];
    encode_uleb(&mut body, 0); // tag 位于 payload 字节 0
    encode_uleb(&mut body, 1); // tag 宽度 1
    encode_uleb(&mut body, 2); // 两个 case
    // case 0：命中，body 访问 word 1。
    body.extend_from_slice(&1u64.to_le_bytes());
    let mut case0 = vec![TraceOp::Direct as u8];
    encode_uleb(&mut case0, 1);
    encode_uleb(&mut case0, 1);
    case0.push(TraceOp::End as u8);
    body.extend_from_slice(&u32::try_from(case0.len()).expect("长度").to_le_bytes());
    body.extend_from_slice(&case0);
    // case 1：tag 不匹配，body 访问 word 5（不得被执行）。
    body.extend_from_slice(&9u64.to_le_bytes());
    let mut case1 = vec![TraceOp::Direct as u8];
    encode_uleb(&mut case1, 5);
    encode_uleb(&mut case1, 1);
    case1.push(TraceOp::End as u8);
    body.extend_from_slice(&u32::try_from(case1.len()).expect("长度").to_le_bytes());
    body.extend_from_slice(&case1);
    // default：访问 word 7（同样不得被执行）。
    let mut default = vec![TraceOp::Direct as u8];
    encode_uleb(&mut default, 7);
    encode_uleb(&mut default, 1);
    default.push(TraceOp::End as u8);
    body.extend_from_slice(&u32::try_from(default.len()).expect("长度").to_le_bytes());
    body.extend_from_slice(&default);
    // SWITCH 之后还有一个字段：它必须被执行，证明分支结束回到整个 SWITCH 之后。
    body.push(TraceOp::Direct as u8);
    encode_uleb(&mut body, 2);
    encode_uleb(&mut body, 1);
    body.push(TraceOp::End as u8);
    let types = table_of(program_descriptor(&body));

    let mut payload = vec![0u8; 8 * 8];
    payload[0] = 1; // tag
    payload[8] = 0x11;
    payload[5 * 8] = 0x55;
    payload[7 * 8] = 0x77;
    payload[2 * 8] = 0x22;
    let visited = collect_once(&types, &mut payload);
    assert_eq!(
        visited,
        vec![(8, 0x11), (2 * 8, 0x22)],
        "只执行命中的分支，并在整个 SWITCH 之后继续"
    );
    let (batched, _) = collect_batched(&types, &mut payload);
    assert_eq!(batched, visited);
}

/// 构造 arena backing 的 payload：32 字节头、紧随其后的记录区、再后面的 data 区。
///
/// 槽数由 `records` 决定，记录区与 data 区按规范布局自动推导，因此调用点不会把区偏移写反。
fn arena_payload(records: &[(u64, u32, u32)], capacity: u64) -> Vec<u8> {
    arena_payload_with_slot_count(
        u64::try_from(records.len()).expect("槽数适配 u64"),
        records,
        capacity,
    )
}

/// 构造 arena backing 的 payload，并允许显式给出与记录数不同的槽数（用于损坏的头）。
fn arena_payload_with_slot_count(
    slot_count: u64,
    records: &[(u64, u32, u32)],
    capacity: u64,
) -> Vec<u8> {
    let records_offset = ARENA_HEADER_BYTES;
    let data_offset = records_offset
        + u64::try_from(records.len()).expect("记录数适配 u64") * ARENA_SLOT_RECORD_BYTES;
    let mut out = vec![0u8; usize::try_from(data_offset + capacity).expect("payload 长度")];
    out[0..8].copy_from_slice(&slot_count.to_le_bytes());
    out[8..16].copy_from_slice(&records_offset.to_le_bytes());
    out[16..24].copy_from_slice(&data_offset.to_le_bytes());
    out[24..32].copy_from_slice(&capacity.to_le_bytes());
    for (index, (value_offset, type_id, flags)) in records.iter().enumerate() {
        let at = usize::try_from(records_offset).expect("记录偏移") + index * 16;
        out[at..at + 8].copy_from_slice(&value_offset.to_le_bytes());
        out[at + 8..at + 12].copy_from_slice(&type_id.to_le_bytes());
        out[at + 12..at + 16].copy_from_slice(&flags.to_le_bytes());
    }
    out
}

/// 构造 backing + inline 两个类型的类型表，并返回 backing 的 payload。
fn arena_types() -> GcRuntimeMetadata {
    let backing = vec![TraceOp::ArenaSlots as u8, TraceOp::End as u8];
    let inline = vec![TraceOp::Direct as u8, 0, 1, TraceOp::End as u8];
    GcRuntimeMetadata {
        types: vec![
            GcRuntimeType {
                name: "backing".to_owned(),
                size: 96,
                align: 8,
                flags: 0,
                trace: program_descriptor(&backing),
            },
            GcRuntimeType {
                name: "inline-node".to_owned(),
                size: 8,
                align: 8,
                flags: 0,
                trace: program_descriptor(&inline),
            },
        ],
    }
}

/// 用一次性入口扫描 arena backing，收集 `(word 偏移, 值)`。
fn collect_arena_once(types: &GcRuntimeMetadata, payload: &mut [u8]) -> Vec<(u64, u64)> {
    let mut visited = Vec::new();
    let scan = walk_descriptor(0, types, payload, 0x1000, &mut |word, address| {
        visited.push((address - 0x1000, u64::from_le_bytes(*word)));
        Ok(())
    })
    .expect("arena backing 扫描成功");
    assert!(scan.arena_slots, "backing 必须报告 ARENA_SLOTS");
    visited
}

/// 用预算 1 的对象游标扫描 arena backing，返回与一次性路径同形的访问序列。
fn collect_arena_batched(types: &GcRuntimeMetadata, payload: &mut [u8]) -> Vec<(u64, u64)> {
    let mut cursor = ObjectTraceCursor::new();
    let mut visited = Vec::new();
    let mut slices = 0usize;
    loop {
        assert!(slices < 4096, "slot 展开必须在有限切片内完成");
        let mut budget = WorkBudget::new(1);
        let progress = cursor
            .step(
                0,
                types,
                payload,
                0x1000,
                &mut budget,
                &mut |word, address| {
                    visited.push((address - 0x1000, u64::from_le_bytes(*word)));
                    Ok(())
                },
            )
            .expect("arena backing 分批扫描成功");
        slices += 1;
        if progress == TraceProgress::Complete {
            return visited;
        }
    }
}

#[test]
fn arena_slots_expand_only_initialized_slots_at_absolute_offsets() {
    let types = arena_types();
    // 记录 0 已初始化，指向 data 区偏移 16 的 inline-node；记录 1 未初始化，必须被跳过。
    let mut payload = arena_payload(&[(16, 1, 1), (0, 1, 0)], 32);
    let marker = 0xabcdu64;
    payload[80..88].copy_from_slice(&marker.to_le_bytes());
    let visited = collect_arena_once(&types, &mut payload);
    assert_eq!(
        visited,
        vec![(80, marker)],
        "只有 initialized slot 的 inline 值可达，偏移按 data 区绝对值解释"
    );
    let mut batched = arena_payload(&[(16, 1, 1), (0, 1, 0)], 32);
    batched[80..88].copy_from_slice(&marker.to_le_bytes());
    assert_eq!(
        collect_arena_batched(&types, &mut batched),
        visited,
        "单元预算的 slot 展开必须访问完全相同的偏移"
    );
}

#[test]
fn arena_slots_reject_damaged_backing_records() {
    let types = arena_types();
    // 未知 flags：只有 bit 0 允许，其余位出现即损坏。
    let mut unknown_flags = arena_payload(&[(0, 1, 0b10)], 32);
    assert!(
        walk_descriptor(0, &types, &mut unknown_flags, 0, &mut |_, _| Ok(())).is_err(),
        "未知 flags 必须失败"
    );
    // 值偏移越过容量：容量 32、偏移 32 已经在区外。
    let mut out_of_capacity = arena_payload(&[(32, 1, 1)], 32);
    assert!(
        walk_descriptor(0, &types, &mut out_of_capacity, 0, &mut |_, _| Ok(())).is_err(),
        "越过容量的值偏移必须失败"
    );
    // 不存在的 TypeId：不能当成“没有指针”跳过。
    let mut missing_type = arena_payload(&[(0, 9, 1)], 32);
    assert!(
        walk_descriptor(0, &types, &mut missing_type, 0, &mut |_, _| Ok(())).is_err(),
        "不存在的 TypeId 必须失败"
    );
    // 记录区越过 payload：巨大的 slot_count 不能靠“读不到就跳过”掩盖。
    let mut truncated = arena_payload_with_slot_count(1024, &[], 32);
    assert!(
        walk_descriptor(0, &types, &mut truncated, 0, &mut |_, _| Ok(())).is_err(),
        "越过 payload 的记录区必须失败"
    );
    // 值没有满足类型对齐：inline-node 的对齐是 8。
    let mut misaligned = arena_payload(&[(4, 1, 1)], 32);
    assert!(
        walk_descriptor(0, &types, &mut misaligned, 0, &mut |_, _| Ok(())).is_err(),
        "未对齐的 slot 值必须失败"
    );
}

#[test]
fn arena_slot_with_resource_type_is_rejected() {
    let mut types = arena_types();
    types.types[1].flags = 0b1000;
    let mut payload = arena_payload(&[(0, 1, 1)], 32);
    assert!(
        walk_descriptor(0, &types, &mut payload, 0, &mut |_, _| Ok(())).is_err(),
        "含 resource 的 slot 类型必须失败"
    );
}

#[test]
fn nested_repeat_and_bitmap_are_resumable_with_unit_budgets() {
    // REPEAT 2 次，每次 body 访问两个 word；每次嵌套都用 unit budget 逐单位推进。
    let mut body = vec![TraceOp::Repeat as u8];
    encode_uleb(&mut body, 0); // base
    encode_uleb(&mut body, 2); // count
    encode_uleb(&mut body, 4); // stride
    let mut inner = vec![TraceOp::Direct as u8];
    encode_uleb(&mut inner, 0);
    encode_uleb(&mut inner, 2);
    inner.push(TraceOp::End as u8);
    body.extend_from_slice(&u32::try_from(inner.len()).expect("长度").to_le_bytes());
    body.extend_from_slice(&inner);
    body.push(TraceOp::End as u8);
    let types = table_of(program_descriptor(&body));
    let mut payload = vec![0u8; 8 * 16];
    for word in 0..16 {
        payload[word * 8] = u8::try_from(word).expect("word 值");
    }
    let visited = collect_once(&types, &mut payload);
    assert_eq!(
        visited
            .iter()
            .map(|(offset, _)| *offset)
            .collect::<Vec<_>>(),
        vec![0, 8, 32, 40],
        "REPEAT 的 stride 按 word 累加"
    );
    let (batched, slices) = collect_batched(&types, &mut payload);
    assert_eq!(batched, visited);
    assert!(
        slices.iter().all(|spent| *spent == 1),
        "每次 step 只消费一个工作单位"
    );
    assert!(slices.len() > 1, "unit budget 必须产生多个切片");
}

#[test]
fn arena_slots_flag_is_reported_without_extra_opcodes() {
    let body = vec![TraceOp::ArenaSlots as u8, TraceOp::End as u8];
    let types = table_of(program_descriptor(&body));
    // 空 backing：slot_count = 0，记录区与 data 区都为空，因此没有可达 slot。
    let mut payload = arena_payload(&[], 0);
    let scan = walk_descriptor(0, &types, &mut payload, 0, &mut |_, _| Ok(())).expect("扫描成功");
    assert!(scan.arena_slots, "ARENA_SLOTS 必须被标记");
    assert_eq!(scan.pointers, 0, "标记本身不访问 word");

    // 嵌套深度上限：超过 TRACE_MAX_FRAMES 的嵌套必须失败，而不是栈溢出或无限重扫。
    // 每个 REPEAT 的 body 都写成**完整的 body 编码**（含 4 字节长度），因此解析路径不会走
    // 进兄弟 opcode 的字节里；构造出的嵌套深度是 TRACE_MAX_FRAMES + 4。
    let mut deep = Vec::new();
    for _ in 0..TRACE_MAX_FRAMES + 4 {
        let mut body = vec![TraceOp::Repeat as u8];
        encode_uleb(&mut body, 0);
        encode_uleb(&mut body, 1);
        encode_uleb(&mut body, 0);
        let inner = [TraceOp::End as u8];
        body.extend_from_slice(&u32::try_from(inner.len()).expect("长度").to_le_bytes());
        body.extend_from_slice(&inner);
        deep.push(TraceOp::Repeat as u8);
        encode_uleb(&mut deep, 0);
        encode_uleb(&mut deep, 1);
        encode_uleb(&mut deep, 0);
        deep.extend_from_slice(&u32::try_from(body.len()).expect("长度").to_le_bytes());
        deep.extend_from_slice(&body);
    }
    let deep_types = table_of(program_descriptor(&deep));
    let mut deep_payload = vec![0u8; 8];
    assert!(
        walk_descriptor(0, &deep_types, &mut deep_payload, 0, &mut |_, _| Ok(())).is_err(),
        "超过 TRACE_MAX_FRAMES 的嵌套必须失败"
    );
}
