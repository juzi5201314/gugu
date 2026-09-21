//! 寄存器分配之后的栈图、展开记录与源码记录。
//!
//! 位置只消费分配结果与编码后的指令边界，不重新推导寄存器。section 字节进入镜像
//! 计划；`--strip` 不能删掉这些运行时必需的记录。

use std::collections::BTreeSet;

use crate::SourceMap;
use crate::diagnostics::Diagnostic;
use crate::lir::body::{Body, Terminator};
use crate::lir::stackmap::{self, LogicalRoot, LogicalSafepoint};
use crate::runtime::{self, CodeLayout, SafepointLayout, StackLanding};
use crate::source::SourceFileId;
use crate::target::TargetName;

use super::abi::{self, AbiSlot};
use super::codegen::{FragmentPayload, SitePayload};
use super::reg::Gpr;
use super::unwind::{self, Landing, UnwindFunction};

/// 分配后的机器元数据。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct MachineMetadata {
    pub(crate) stackmap_name: String,
    pub(crate) unwind_name: String,
    pub(crate) source_name: String,
    pub(crate) stackmap_section: Vec<u8>,
    pub(crate) unwind_section: Vec<u8>,
    pub(crate) source_section: Vec<u8>,
    pub(crate) function_count: u32,
    pub(crate) safepoint_count: u32,
    pub(crate) map_count: u32,
    pub(crate) unwind_function_count: u32,
    pub(crate) landing_count: u32,
    pub(crate) source_record_count: u32,
    pub(crate) fingerprint: [u8; 32],
}

struct Prepared<'a> {
    rva: u64,
    body: &'a Body,
    fragment: &'a FragmentPayload,
    safepoints: Vec<SafepointLayout>,
    landings: Vec<Landing>,
    unwind_index: u32,
}

/// 从已编码片段生成三个 section，并用 runtime walker 消费它们。
pub(crate) fn build(
    lir: &crate::lir::Validated,
    fragments: &[FragmentPayload],
    sources: &SourceMap,
    target: TargetName,
    raw: &crate::runtime::RuntimeRawContractV1,
) -> Result<MachineMetadata, Vec<Diagnostic>> {
    if lir.bodies().len() != fragments.len() {
        return Err(vec![invalid("机器片段数量与 LIR 函数不一致")]);
    }
    let rvas = assign_rvas(fragments);
    let mut prepared = Vec::with_capacity(fragments.len());
    for (index, body) in lir.bodies().iter().enumerate() {
        let fragment = &fragments[index];
        let anchors = stackmap::anchors(body).map_err(|error| vec![error])?;
        let safepoints = machine_safepoints(body, fragment, target, &anchors)?;
        let landings = landings_of(body, fragment)?;
        prepared.push(Prepared {
            rva: rvas[index],
            body,
            fragment,
            safepoints,
            landings,
            unwind_index: 0,
        });
    }
    prepared.sort_by_key(|item| (item.rva, item.fragment.symbol.clone()));
    for (index, item) in prepared.iter_mut().enumerate() {
        item.unwind_index = u32::try_from(index).expect("函数数量适配 u32");
    }
    let (unwind_functions, landings, saves) = unwind_tables(&prepared)?;
    let unwind_section = unwind::encode(target, &unwind_functions, &landings, &saves)
        .map_err(|message| vec![invalid(&message)])?;
    let (layouts, points, stack_index) = stackmap_inputs(&prepared);
    let stackmap_section = runtime::encode_stackmap(&layouts, &points)
        .map_err(|error| vec![invalid(error.message())])?;
    let source_section = source_section(&prepared, &stack_index, sources)?;
    let decoded = runtime::decode_tables(&stackmap_section)
        .map_err(|error| vec![invalid(error.message())])?;
    check_joint(&prepared, &decoded, &unwind_functions)?;
    let consumed = runtime::consume_metadata(&stackmap_section, &[], raw.compression())
        .map_err(|error| vec![invalid(error.message())])?;
    if consumed.functions != decoded.functions.len() as u32
        || consumed.safepoints != decoded.safepoints.len() as u32
    {
        return Err(vec![invalid("runtime 消费的栈图计数与 section 不一致")]);
    }
    let landing_count = consume_landings(&stackmap_section, &prepared, raw)?;
    let mut metadata = MachineMetadata {
        stackmap_name: stackmap_name(target).to_owned(),
        unwind_name: unwind_name(target).to_owned(),
        source_name: source_section_name(target).to_owned(),
        stackmap_section,
        unwind_section,
        source_section,
        function_count: consumed.functions,
        safepoint_count: consumed.safepoints,
        map_count: consumed.maps,
        unwind_function_count: unwind_functions.len() as u32,
        landing_count,
        source_record_count: 0,
        fingerprint: [0; 32],
    };
    metadata.source_record_count = source_count(&metadata.source_section)?;
    metadata.fingerprint = fingerprint(&metadata);
    if !strip_keeps(&metadata, target) {
        return Err(vec![invalid("strip 会删除运行时必需的栈图或展开记录")]);
    }
    Ok(metadata)
}

/// 当前目标必须保留的元数据节。
pub(crate) fn required_names(target: TargetName) -> &'static [&'static str] {
    match target {
        TargetName::X86_64Linux => &[".gugu.stackmap", ".eh_frame", ".gugu.src"],
        TargetName::X86_64Windows => &[".gugustk", ".xdata", ".gugusrc"],
    }
}

fn source_section_name(target: TargetName) -> &'static str {
    match target {
        TargetName::X86_64Linux => ".gugu.src",
        TargetName::X86_64Windows => ".gugusrc",
    }
}

/// 每个函数的落地链单独交给 walker：记录里的 PC 相对函数起点，不能跨函数混查。
fn consume_landings(
    section: &[u8],
    prepared: &[Prepared<'_>],
    raw: &crate::runtime::RuntimeRawContractV1,
) -> Result<u32, Vec<Diagnostic>> {
    let mut total = 0_u32;
    for item in prepared {
        if item.landings.is_empty() {
            continue;
        }
        let landings = item
            .landings
            .iter()
            .map(|landing| StackLanding {
                pc_start: landing.pc_start,
                pc_end: landing.pc_end,
                landing_pc: landing.landing_pc,
                cleanup: landing.cleanup,
            })
            .collect::<Vec<_>>();
        let consumed = runtime::consume_metadata(section, &landings, raw.compression())
            .map_err(|error| vec![invalid(error.message())])?;
        total = total.saturating_add(consumed.landings);
    }
    Ok(total)
}

fn stackmap_name(target: TargetName) -> &'static str {
    match target {
        TargetName::X86_64Linux => ".gugu.stackmap",
        TargetName::X86_64Windows => ".gugustk",
    }
}

fn unwind_name(target: TargetName) -> &'static str {
    match target {
        TargetName::X86_64Linux => ".eh_frame",
        TargetName::X86_64Windows => ".xdata",
    }
}

fn assign_rvas(fragments: &[FragmentPayload]) -> Vec<u64> {
    let mut order: Vec<usize> = (0..fragments.len()).collect();
    order.sort_by(|left, right| fragments[*left].symbol.cmp(&fragments[*right].symbol));
    let mut rvas = vec![0; fragments.len()];
    let mut cursor = 0_u64;
    for index in order {
        cursor = cursor.div_ceil(16) * 16;
        rvas[index] = cursor;
        let span = (fragments[index].bytes.len() as u64).max(1);
        cursor = cursor.saturating_add(span);
    }
    rvas
}

fn machine_safepoints(
    body: &Body,
    fragment: &FragmentPayload,
    target: TargetName,
    anchors: &[LogicalSafepoint],
) -> Result<Vec<SafepointLayout>, Vec<Diagnostic>> {
    let mut layouts = Vec::new();
    let boundaries = boundaries(fragment);
    let code_size = u32::try_from(fragment.bytes.len()).expect("代码长度适配 u32");
    for anchor in anchors {
        let pc = safepoint_pc(fragment, anchor)?;
        if pc >= code_size || !boundaries.contains(&pc) {
            return Err(vec![invalid(&format!(
                "{} 的安全点 PC 不在指令边界",
                body.name
            ))]);
        }
        layouts.push(layout_of(body, fragment, target, anchor, pc)?);
    }
    layouts.sort_by_key(|layout| layout.pc_offset);
    if layouts
        .windows(2)
        .any(|pair| pair[0].pc_offset >= pair[1].pc_offset)
    {
        return Err(vec![invalid(&format!("{} 的安全点 PC 重叠", body.name))]);
    }
    Ok(layouts)
}

fn safepoint_pc(
    fragment: &FragmentPayload,
    anchor: &LogicalSafepoint,
) -> Result<u32, Vec<Diagnostic>> {
    if anchor.kind == stackmap::KIND_MORESTACK_ENTRY {
        return fragment
            .sites
            .iter()
            .find_map(|site| site.morestack_pc)
            .ok_or_else(|| vec![invalid("入口检查没有 morestack 返回点")]);
    }
    fragment
        .sites
        .iter()
        .find(|site| site.block == anchor.block && site.instruction == anchor.instruction)
        .and_then(|site| site.call_return_pc)
        .ok_or_else(|| vec![invalid("安全点没有调用返回 PC")])
}

fn layout_of(
    body: &Body,
    fragment: &FragmentPayload,
    target: TargetName,
    anchor: &LogicalSafepoint,
    pc: u32,
) -> Result<SafepointLayout, Vec<Diagnostic>> {
    let slot_count = if anchor.kind == stackmap::KIND_MORESTACK_ENTRY {
        0
    } else {
        fragment.frame.frame_size / 8
    };
    let mut slots = empty_bitmaps(slot_count);
    let mut registers = [0_u16; 5];
    place_roots(
        body,
        fragment,
        target,
        anchor,
        &mut slots,
        &mut registers,
        slot_count,
    )?;
    if matches!(anchor.kind, 0 | 2 | 3) && registers.iter().any(|mask| *mask != 0) {
        return Err(vec![invalid("调用、挂起或 bridge 点保留了用户寄存器根")]);
    }
    Ok(SafepointLayout {
        pc_offset: pc,
        kind: anchor.kind,
        dirty: anchor.dirty,
        copy_allowed: anchor.flags & 1 != 0,
        scan_allowed: anchor.flags & 2 != 0,
        slots,
        registers,
        function: 0,
        slot_count,
    })
}

fn place_roots(
    body: &Body,
    fragment: &FragmentPayload,
    target: TargetName,
    anchor: &LogicalSafepoint,
    slots: &mut [Vec<u64>; 5],
    registers: &mut [u16; 5],
    slot_count: u32,
) -> Result<(), Vec<Diagnostic>> {
    let groups = [
        &anchor.roots.direct,
        &anchor.roots.interior,
        &anchor.roots.handle,
        &anchor.roots.compressed,
        &anchor.roots.stack,
    ];
    for (class, roots) in groups.iter().enumerate() {
        for root in *roots {
            place_root(
                body, fragment, target, anchor, class, root, slots, registers, slot_count,
            )?;
        }
    }
    Ok(())
}

fn place_root(
    body: &Body,
    fragment: &FragmentPayload,
    target: TargetName,
    anchor: &LogicalSafepoint,
    class: usize,
    root: &LogicalRoot,
    slots: &mut [Vec<u64>; 5],
    registers: &mut [u16; 5],
    slot_count: u32,
) -> Result<(), Vec<Diagnostic>> {
    match root {
        LogicalRoot::Argument { index } => place_argument(
            body, target, anchor, class, *index, registers, slots, slot_count,
        ),
        LogicalRoot::Slot { slot, offset } if *slot == u32::MAX => {
            place_by_value(body, target, anchor, class, *offset, slots, slot_count)
        }
        LogicalRoot::Slot { slot, offset } => {
            place_slot(fragment, class, *slot, *offset, slots, slot_count)
        }
        LogicalRoot::Value { value, .. } => place_value(
            fragment, anchor, class, *value, slots, registers, slot_count,
        ),
    }
}

fn place_argument(
    body: &Body,
    target: TargetName,
    anchor: &LogicalSafepoint,
    class: usize,
    index: u32,
    registers: &mut [u16; 5],
    slots: &mut [Vec<u64>; 5],
    slot_count: u32,
) -> Result<(), Vec<Diagnostic>> {
    if anchor.kind == stackmap::KIND_MORESTACK_ENTRY {
        let abi = abi::classify_signature(&body.signature)
            .map_err(|error| vec![invalid(&error.to_string())])?;
        if let Some(AbiSlot::Integer(gpr)) = argument_slot(&abi, index) {
            set_register(registers, class, gpr)?;
        }
        return Ok(());
    }
    let Some(call) = call_of(body, anchor) else {
        return Ok(());
    };
    let abi =
        abi::classify_call(call, target).map_err(|error| vec![invalid(&error.to_string())])?;
    if let Some(AbiSlot::Stack { offset }) = argument_slot(&abi, index) {
        set_slot(slots, class, offset / 8, slot_count)?;
    }
    Ok(())
}

fn argument_slot(abi: &super::abi::AbiLayout, index: u32) -> Option<AbiSlot> {
    abi.arguments
        .iter()
        .find(|argument| argument.index == index)
        .and_then(|argument| argument.slot)
}

fn place_by_value(
    body: &Body,
    target: TargetName,
    anchor: &LogicalSafepoint,
    class: usize,
    offset: u64,
    slots: &mut [Vec<u64>; 5],
    slot_count: u32,
) -> Result<(), Vec<Diagnostic>> {
    let parameter = u32::try_from(offset >> 32).unwrap_or(u32::MAX);
    let word = offset as u32;
    let Some(call) = call_of(body, anchor) else {
        return Ok(());
    };
    let abi =
        abi::classify_call(call, target).map_err(|error| vec![invalid(&error.to_string())])?;
    let Some(AbiSlot::Stack { offset: base }) = argument_slot(&abi, parameter) else {
        return Err(vec![invalid("按值聚合副本没有可登记的栈槽")]);
    };
    let byte = base
        .checked_add(word.saturating_mul(8))
        .ok_or_else(|| vec![invalid("按值聚合副本偏移溢出")])?;
    set_slot(slots, class, byte / 8, slot_count)
}

fn place_slot(
    fragment: &FragmentPayload,
    class: usize,
    slot: u32,
    offset: u64,
    slots: &mut [Vec<u64>; 5],
    slot_count: u32,
) -> Result<(), Vec<Diagnostic>> {
    let local = fragment
        .frame
        .locals
        .get(slot as usize)
        .ok_or_else(|| vec![invalid("栈槽根没有对应的 frame 局部")])?;
    let byte = local
        .0
        .checked_add(u32::try_from(offset).unwrap_or(u32::MAX))
        .ok_or_else(|| vec![invalid("栈槽根偏移溢出")])?;
    if byte % 8 != 0 {
        return Err(vec![invalid("栈槽根没有对齐到机器字")]);
    }
    set_slot(slots, class, byte / 8, slot_count)
}

fn place_value(
    fragment: &FragmentPayload,
    anchor: &LogicalSafepoint,
    class: usize,
    value: u32,
    slots: &mut [Vec<u64>; 5],
    registers: &mut [u16; 5],
    slot_count: u32,
) -> Result<(), Vec<Diagnostic>> {
    let Some(found) = fragment.values.iter().find(|item| item.value == value) else {
        return Ok(());
    };
    if let Some(slot) = safepoint_slot(fragment, anchor)
        && !covers_slot(found.range, slot)
    {
        return Ok(());
    }
    let Some(code) = found.segments.first().map(|segment| segment.2) else {
        return Ok(());
    };
    if code >= 16 {
        return Err(vec![invalid("受管指针不能放在 XMM")]);
    }
    if code >= 0 {
        if !matches!(anchor.kind, stackmap::KIND_POLL_RESUME) {
            return Err(vec![invalid("该安全点的受管指针必须已经落在栈槽")]);
        }
        let gpr = gpr_from_code(u8::try_from(code).unwrap_or(255))
            .ok_or_else(|| vec![invalid("根寄存器编码非法")])?;
        return set_register(registers, class, gpr);
    }
    let slot = u32::try_from(-code - 1).map_err(|_| vec![invalid("溢出槽编号非法")])?;
    let offset = fragment
        .frame
        .spill_slots
        .get(slot as usize)
        .map(|item| item.0)
        .ok_or_else(|| vec![invalid("溢出槽没有 frame 偏移")])?;
    set_slot(slots, class, offset / 8, slot_count)
}

fn safepoint_slot(fragment: &FragmentPayload, anchor: &LogicalSafepoint) -> Option<u32> {
    let site = fragment
        .sites
        .iter()
        .find(|site| site.block == anchor.block && site.instruction == anchor.instruction)?;
    let start = site.points.0 as usize;
    let end = site.points.1 as usize;
    let points = fragment.points.get(start..end)?;
    let point = points
        .iter()
        .rev()
        .find(|point| point.kind == 1 || point.kind == 2)
        .or_else(|| points.last())?;
    Some(point.use_slot)
}

fn covers_slot(range: (u32, u32), use_slot: u32) -> bool {
    let def_slot = use_slot.saturating_add(1);
    use_slot <= range.1 && def_slot >= range.0
}

fn call_of<'a>(body: &'a Body, anchor: &LogicalSafepoint) -> Option<&'a crate::lir::body::Call> {
    let block = body.blocks.get(anchor.block as usize)?;
    if anchor.instruction == block.instructions.end {
        return match &block.terminator {
            Terminator::Invoke { call, .. } => Some(call),
            _ => None,
        };
    }
    match &body.instructions.get(anchor.instruction as usize)?.op {
        crate::lir::body::Op::Call(call) | crate::lir::body::Op::ForeignCall(call) => Some(call),
        _ => None,
    }
}

fn landings_of(body: &Body, fragment: &FragmentPayload) -> Result<Vec<Landing>, Vec<Diagnostic>> {
    let mut landings = Vec::new();
    for (index, block) in body.blocks.iter().enumerate() {
        let Terminator::Invoke { unwind, .. } = &block.terminator else {
            continue;
        };
        let edge = body
            .edges
            .get(unwind.index())
            .ok_or_else(|| vec![invalid("unwind 边越界")])?;
        let site = fragment
            .sites
            .iter()
            .find(|site| site.block == index as u32 && site.instruction == block.instructions.end)
            .ok_or_else(|| vec![invalid("调用终结符没有机器站点")])?;
        let pc_end = site.call_return_pc.unwrap_or(site.bytes.1);
        if pc_end <= site.bytes.0 {
            return Err(vec![invalid("落地范围为空")]);
        }
        let landing_pc =
            block_start(fragment, edge.to.0).ok_or_else(|| vec![invalid("落地块没有机器字节")])?;
        let cleanup = cleanup_chain(body, edge.to.0);
        landings.push(Landing {
            pc_start: site.bytes.0,
            pc_end,
            landing_pc,
            cleanup,
        });
    }
    landings.sort_by_key(|landing| landing.pc_start);
    Ok(landings)
}

fn cleanup_chain(body: &Body, block: u32) -> u32 {
    let Some(data) = body.blocks.get(block as usize) else {
        return u32::MAX;
    };
    if data.instructions.start == data.instructions.end
        && matches!(data.terminator, Terminator::ResumePanic { .. })
    {
        return u32::MAX;
    }
    block
}

fn block_start(fragment: &FragmentPayload, block: u32) -> Option<u32> {
    fragment
        .sites
        .iter()
        .find(|site| site.block == block)
        .map(|site| site.bytes.0)
}

fn unwind_tables(
    prepared: &[Prepared<'_>],
) -> Result<(Vec<UnwindFunction>, Vec<Landing>, Vec<Vec<(u8, u32)>>), Vec<Diagnostic>> {
    let mut functions = Vec::new();
    let mut landings = Vec::new();
    let mut saves = Vec::new();
    for item in prepared {
        let start = u32::try_from(landings.len()).expect("落地数量适配 u32");
        let count =
            u16::try_from(item.landings.len()).map_err(|_| vec![invalid("落地记录超过 u16")])?;
        landings.extend(item.landings.iter().copied());
        let code_size = u32::try_from(item.fragment.bytes.len()).expect("代码长度适配 u32");
        functions.push(UnwindFunction::new(
            item.rva,
            code_size,
            item.fragment.frame.frame_size,
            &item.fragment.frame.save_offsets,
            start,
            count,
            item.fragment.frame.checked,
        ));
        saves.push(item.fragment.frame.save_offsets.clone());
    }
    unwind::verify_records(&functions, &landings).map_err(|message| vec![invalid(&message)])?;
    Ok((functions, landings, saves))
}

fn stackmap_inputs(
    prepared: &[Prepared<'_>],
) -> (
    Vec<CodeLayout>,
    Vec<(u32, SafepointLayout)>,
    Vec<Option<u32>>,
) {
    let mut layouts = Vec::new();
    let mut points = Vec::new();
    let mut stack_index = vec![None; prepared.len()];
    for item in prepared {
        if item.safepoints.is_empty() {
            continue;
        }
        let index = layouts.len() as u32;
        stack_index[item.unwind_index as usize] = Some(index);
        let has_stack = item
            .safepoints
            .iter()
            .any(|point| point.registers[4] != 0 || point.slots[4].iter().any(|word| *word != 0));
        layouts.push(CodeLayout {
            code_rva: item.rva,
            code_size: item.fragment.bytes.len() as u32,
            frame_size: item.fragment.frame.frame_size,
            unwind_index: item.unwind_index,
            runtime_bridge: item.safepoints.iter().any(|point| point.kind == 3),
            panic_landing: !item.landings.is_empty(),
            has_stack_interior: has_stack,
        });
        for layout in &item.safepoints {
            points.push((index, layout.clone()));
        }
    }
    (layouts, points, stack_index)
}

fn source_section(
    prepared: &[Prepared<'_>],
    stack_index: &[Option<u32>],
    sources: &SourceMap,
) -> Result<Vec<u8>, Vec<Diagnostic>> {
    let mut records = Vec::new();
    for item in prepared {
        let Some(function) = stack_index[item.unwind_index as usize] else {
            continue;
        };
        for site in &item.fragment.sites {
            if let Some(record) = source_record(item.body, site, function, sources) {
                records.push(record);
            }
        }
    }
    records.sort_by(|left, right| {
        (
            left.function,
            left.pc_start,
            left.pc_end,
            left.path.as_str(),
        )
            .cmp(&(
                right.function,
                right.pc_start,
                right.pc_end,
                right.path.as_str(),
            ))
    });
    encode_sources(&records)
}

struct SourceRecord {
    function: u32,
    pc_start: u32,
    pc_end: u32,
    path: String,
    line: u32,
    column: u32,
    flags: u32,
}

fn source_record(
    body: &Body,
    site: &SitePayload,
    function: u32,
    sources: &SourceMap,
) -> Option<SourceRecord> {
    if site.bytes.1 <= site.bytes.0 {
        return None;
    }
    let info = source_info(body, site)?;
    let location = &info.location;
    let snapshot = sources.snapshot(SourceFileId::new(location.source));
    let path = snapshot
        .map(|snapshot| snapshot.logical_path().to_owned())
        .unwrap_or_else(|| "<synthetic>".to_owned());
    let (line, column) = snapshot
        .and_then(|snapshot| {
            snapshot
                .line_column(location.start as usize)
                .ok()
                .map(|position| (position.line, position.column))
        })
        .unwrap_or((1, 1));
    let mut flags = 0_u32;
    if site.op == "Trap"
        || site.op == "ResumePanic"
        || site
            .cold_edges
            .iter()
            .any(|edge| matches!(edge.kind, super::inst::ColdEdgeKind::Trap))
    {
        flags |= 1;
    }
    if location.expansion != 0 || snapshot.is_none() {
        flags |= 1 << 1;
    }
    Some(SourceRecord {
        function,
        pc_start: site.bytes.0,
        pc_end: site.bytes.1,
        path,
        line,
        column,
        flags,
    })
}

fn source_info<'a>(
    body: &'a Body,
    site: &SitePayload,
) -> Option<&'a crate::frontend::gir::body::SourceInfo> {
    let block = body.blocks.get(site.block as usize)?;
    if site.instruction == block.instructions.end {
        return Some(&block.source);
    }
    Some(&body.instructions.get(site.instruction as usize)?.source)
}

fn encode_sources(records: &[SourceRecord]) -> Result<Vec<u8>, Vec<Diagnostic>> {
    let mut paths = BTreeSet::new();
    for record in records {
        paths.insert(record.path.clone());
    }
    let mut pool = Vec::new();
    let mut offsets = std::collections::BTreeMap::new();
    for path in &paths {
        let offset = u32::try_from(pool.len()).map_err(|_| vec![invalid("源码路径池溢出")])?;
        offsets.insert(path.clone(), offset);
        pool.extend_from_slice(path.as_bytes());
    }
    let mut output = vec![0_u8; 24];
    output[..8].copy_from_slice(b"GUGUSRC1");
    output[8..10].copy_from_slice(&1_u16.to_le_bytes());
    output[10] = 8;
    output[11] = 1;
    output[12..16].copy_from_slice(&(records.len() as u32).to_le_bytes());
    output[16..20].copy_from_slice(&(pool.len() as u32).to_le_bytes());
    for record in records {
        let offset = offsets[&record.path];
        let path_len =
            u32::try_from(record.path.len()).map_err(|_| vec![invalid("源码路径过长")])?;
        output.extend_from_slice(&record.function.to_le_bytes());
        output.extend_from_slice(&record.pc_start.to_le_bytes());
        output.extend_from_slice(&record.pc_end.to_le_bytes());
        output.extend_from_slice(&offset.to_le_bytes());
        output.extend_from_slice(&path_len.to_le_bytes());
        output.extend_from_slice(&record.line.to_le_bytes());
        output.extend_from_slice(&record.column.to_le_bytes());
        output.extend_from_slice(&record.flags.to_le_bytes());
    }
    output.extend_from_slice(&pool);
    Ok(output)
}

fn source_count(bytes: &[u8]) -> Result<u32, Vec<Diagnostic>> {
    if bytes.len() < 24 || &bytes[..8] != b"GUGUSRC1" {
        return Err(vec![invalid("源码记录 section 魔数不匹配")]);
    }
    Ok(u32::from_le_bytes(
        bytes[12..16].try_into().expect("计数字段"),
    ))
}

fn check_joint(
    prepared: &[Prepared<'_>],
    decoded: &runtime::DecodedSection,
    unwinds: &[UnwindFunction],
) -> Result<(), Vec<Diagnostic>> {
    for function in &decoded.functions {
        let unwind = unwinds
            .get(function.unwind_index as usize)
            .ok_or_else(|| vec![invalid("栈图 unwind 下标越界")])?;
        if unwind.code_rva != function.code_rva
            || unwind.code_size != function.code_size
            || unwind.frame_size != function.frame_size
        {
            return Err(vec![invalid("栈图函数与展开记录不一致")]);
        }
        if function.frame_size != 0 && function.frame_size % 16 != 8 {
            return Err(vec![invalid("有安全点的函数帧对齐不正确")]);
        }
    }
    let mapped = prepared
        .iter()
        .filter(|item| !item.safepoints.is_empty())
        .count();
    if mapped != decoded.functions.len() {
        return Err(vec![invalid("栈图函数数量与有安全点的片段不一致")]);
    }
    Ok(())
}

fn boundaries(fragment: &FragmentPayload) -> BTreeSet<u32> {
    let mut set = BTreeSet::new();
    for site in &fragment.sites {
        set.insert(site.bytes.0);
        set.insert(site.bytes.1);
        if let Some(pc) = site.call_return_pc {
            set.insert(pc);
        }
        if let Some(pc) = site.morestack_pc {
            set.insert(pc);
        }
    }
    set
}

fn empty_bitmaps(slot_count: u32) -> [Vec<u64>; 5] {
    let bytes = slot_count.div_ceil(8) as usize;
    let lanes = bytes.div_ceil(8).max(1);
    std::array::from_fn(|_| vec![0; lanes])
}

fn set_slot(
    slots: &mut [Vec<u64>; 5],
    class: usize,
    index: u32,
    slot_count: u32,
) -> Result<(), Vec<Diagnostic>> {
    if slot_count == 0 || index >= slot_count {
        return Err(vec![invalid("根槽越出 frame")]);
    }
    for (other, bitmap) in slots.iter().enumerate() {
        if other != class && bit_set(bitmap, index) {
            return Err(vec![invalid("同一槽出现在两类根位图")]);
        }
    }
    let lane = (index / 64) as usize;
    slots[class][lane] |= 1_u64 << (index % 64);
    Ok(())
}

fn bit_set(bitmap: &[u64], index: u32) -> bool {
    bitmap
        .get((index / 64) as usize)
        .is_some_and(|word| word & (1_u64 << (index % 64)) != 0)
}

fn set_register(registers: &mut [u16; 5], class: usize, gpr: Gpr) -> Result<(), Vec<Diagnostic>> {
    let bit = gpr_bit(gpr).ok_or_else(|| vec![invalid("rsp 不能作为栈图根")])?;
    if bit & runtime::stackmap_schema::RESERVED_REGISTER_BITS != 0 {
        return Err(vec![invalid("普通函数占用 runtime 保留寄存器")]);
    }
    for (other, mask) in registers.iter().enumerate() {
        if other != class && mask & bit != 0 {
            return Err(vec![invalid("同一寄存器出现在两类根掩码")]);
        }
    }
    registers[class] |= bit;
    Ok(())
}

fn gpr_bit(gpr: Gpr) -> Option<u16> {
    let bit = match gpr {
        Gpr::Rax => 0,
        Gpr::Rbx => 1,
        Gpr::Rcx => 2,
        Gpr::Rdx => 3,
        Gpr::Rsi => 4,
        Gpr::Rdi => 5,
        Gpr::Rbp => 6,
        Gpr::R8 => 7,
        Gpr::R9 => 8,
        Gpr::R10 => 9,
        Gpr::R11 => 10,
        Gpr::R12 => 11,
        Gpr::R13 => 12,
        Gpr::R14 => 13,
        Gpr::R15 => 14,
        Gpr::Rsp => return None,
    };
    Some(1 << bit)
}

fn gpr_from_code(code: u8) -> Option<Gpr> {
    Some(match code {
        0 => Gpr::Rax,
        1 => Gpr::Rcx,
        2 => Gpr::Rdx,
        3 => Gpr::Rbx,
        4 => Gpr::Rsp,
        5 => Gpr::Rbp,
        6 => Gpr::Rsi,
        7 => Gpr::Rdi,
        8 => Gpr::R8,
        9 => Gpr::R9,
        10 => Gpr::R10,
        11 => Gpr::R11,
        12 => Gpr::R12,
        13 => Gpr::R13,
        14 => Gpr::R14,
        15 => Gpr::R15,
        _ => return None,
    })
}

fn fingerprint(metadata: &MachineMetadata) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new_derive_key("gugu-x64-metadata-v1");
    hasher.update(metadata.stackmap_name.as_bytes());
    hasher.update(metadata.unwind_name.as_bytes());
    hasher.update(metadata.source_name.as_bytes());
    hasher.update(&metadata.stackmap_section);
    hasher.update(&metadata.unwind_section);
    hasher.update(&metadata.source_section);
    *hasher.finalize().as_bytes()
}

fn strip_keeps(metadata: &MachineMetadata, target: TargetName) -> bool {
    let mut names = vec![
        metadata.stackmap_name.as_str(),
        metadata.unwind_name.as_str(),
        metadata.source_name.as_str(),
        ".debug_info",
        ".symtab",
    ];
    names.retain(|name| !name.starts_with(".debug") && *name != ".symtab");
    required_names(target)
        .iter()
        .all(|required| names.contains(required))
}

fn invalid(message: &str) -> Diagnostic {
    super::codegen::invalid_metadata(message)
}
