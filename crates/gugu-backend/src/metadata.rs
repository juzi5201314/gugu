//! Architecture-neutral root bitmap construction.
pub(crate) fn empty_bitmaps(slot_count: u32) -> [Vec<u64>; 5] {
    let bytes = slot_count.div_ceil(8) as usize;
    let lanes = bytes.div_ceil(8).max(1);
    std::array::from_fn(|_| vec![0; lanes])
}

pub(crate) fn assign_init_bytes(
    roots: &[usize],
    offset: &mut u32,
) -> Result<Vec<Option<u32>>, &'static str> {
    let mut bases = Vec::with_capacity(roots.len());
    for count in roots {
        if *count == 0 {
            bases.push(None);
            continue;
        }
        let bytes = u32::try_from(*count).map_err(|_| "逐根初始化字节数超出 u32")?;
        bases.push(Some(*offset));
        *offset = offset.checked_add(bytes).ok_or("frame 负载溢出")?;
    }
    Ok(bases)
}

pub(crate) fn init_events_until<'a>(
    events: &'a [(u32, u32, u32, bool)],
    cursor: &mut usize,
    upto: u32,
) -> &'a [(u32, u32, u32, bool)] {
    let start = *cursor;
    while events.get(*cursor).is_some_and(|event| event.0 <= upto) {
        *cursor += 1;
    }
    &events[start..*cursor]
}

#[expect(
    clippy::too_many_arguments,
    reason = "local root decision combines LIR proof and physical frame layout"
)]
pub(crate) fn local_root(
    body: &crate::lir::body::Body,
    anchor: &crate::lir::stackmap::LogicalSafepoint,
    class: usize,
    slot: u32,
    offset: u64,
    local: u32,
    base: Option<u32>,
    init: Option<&crate::lir::root_init::InitSlots>,
) -> Result<(u32, Option<u32>), &'static str> {
    let byte = local
        .checked_add(u32::try_from(offset).map_err(|_| "栈槽根偏移溢出")?)
        .ok_or("栈槽根偏移溢出")?;
    if byte % 8 != 0 {
        return Err("栈槽根没有对齐到机器字");
    }
    let roots = &body
        .stack_slots
        .get(slot as usize)
        .ok_or("栈槽根缺少槽表")?
        .roots;
    let bit = roots
        .iter()
        .position(|(root, p)| *root == offset && p.root_kind() == Some(class as u32))
        .or_else(|| roots.iter().position(|(root, _)| *root == offset))
        .ok_or("栈槽根不在槽 root 表里")?;
    let ambiguous = roots.iter().filter(|(root, _)| *root == offset).count() > 1;
    let position = anchor
        .instruction
        .checked_sub(body.blocks[anchor.block as usize].instructions.start)
        .ok_or("初始化查询不在安全点所在块内")?;
    let gate = if ambiguous
        || init.is_none_or(|init| {
            !init.is_initialized(
                crate::lir::body::BlockId(anchor.block),
                position,
                slot as usize,
                bit,
            )
        }) {
        Some(
            base.ok_or("栈槽根缺少帧内逐根初始化字节")?
                .checked_add(u32::try_from(bit).map_err(|_| "帧内逐根初始化字节偏移溢出")?)
                .ok_or("帧内逐根初始化字节偏移溢出")?,
        )
    } else {
        None
    };
    Ok((byte / 8, gate))
}
