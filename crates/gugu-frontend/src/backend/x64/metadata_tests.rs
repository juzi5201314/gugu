//! 分配后的栈图、展开记录与源码记录必须能被 runtime 消费。

use crate::runtime;
use crate::{CompileRequest, Compiler, TargetName};

use super::codegen::X64World;

const CALLS: &str = "fn ping(value: int) int {\n if value <= 0 { return 0 }\n let next = pong(value - 1)\n next + 1\n}\nfn pong(value: int) int {\n if value <= 0 { return 1 }\n let next = ping(value - 1)\n next + 1\n}\nfn main() {\n let index = 0\n let total = 0\n while index < 4 {\n  total = total + ping(index)\n  index = index + 1\n }\n if total > 0 { _ = pong(total) }\n}\n";

fn compile(source: &str, target: TargetName) -> crate::Compilation {
    let compilation =
        Compiler::shared().compile(CompileRequest::single_file("main.gg", source, target));
    assert!(
        compilation.is_success(),
        "{:?}",
        compilation.diagnostics().items()
    );
    compilation
}

#[test]
// 这个测试调用 compile 做完整编译，耗时过长；移出默认套件
#[ignore = "integration test with full compilation"]
fn encoded_metadata_keeps_ranges_and_is_walked() {
    let compilation = compile(CALLS, TargetName::X86_64Linux);
    let plan = compilation.image_plan().expect("镜像计划");
    assert_eq!(plan.x64_stackmap_name(), ".gugu.stackmap");
    assert_eq!(plan.x64_unwind_name(), ".eh_frame");
    assert_eq!(plan.x64_source_name(), ".gugu.src");
    assert!(plan.x64_stackmap_section().starts_with(b"GUGUSM01"));
    assert!(plan.x64_unwind_section().starts_with(b"GUGUUN01"));
    assert!(plan.x64_source_section().starts_with(b"GUGUSRC1"));
    assert!(plan.x64_stackmap_functions() > 0);
    assert!(plan.x64_unwind_functions() >= plan.x64_stackmap_functions());
    assert!(plan.x64_stackmap_safepoints() > 0);
    assert!(plan.x64_stackmap_maps() > 0);
    assert!(plan.x64_stackmap_maps() <= plan.x64_stackmap_safepoints());
    assert!(plan.x64_source_records() > 0);
    assert_ne!(plan.x64_metadata_fingerprint(), [0; 32]);
    let again = compile(CALLS, TargetName::X86_64Linux);
    assert_eq!(
        again
            .image_plan()
            .expect("镜像计划")
            .x64_metadata_fingerprint(),
        plan.x64_metadata_fingerprint()
    );
    let decoded = runtime::decode_tables(plan.x64_stackmap_section()).expect("栈图可解码");
    for pair in decoded.functions.windows(2) {
        let end = pair[0].code_rva + u64::from(pair[0].code_size);
        assert!(pair[0].code_rva < pair[1].code_rva && end <= pair[1].code_rva);
    }
    let mut saw_call = false;
    let mut saw_poll = false;
    let mut saw_entry = false;
    for function in &decoded.functions {
        let start = function.safepoint_start as usize;
        let end = start + function.safepoint_count as usize;
        let points = &decoded.safepoints[start..end];
        assert!(
            points
                .windows(2)
                .all(|pair| pair[0].pc_offset < pair[1].pc_offset)
        );
        for point in points {
            let map = &decoded.maps[point.map_index as usize];
            if matches!(point.kind, 0 | 2 | 3) {
                assert!(map.registers.iter().all(|mask| *mask == 0));
            }
            saw_call |= point.kind == 0;
            saw_poll |= point.kind == 1;
            saw_entry |= point.kind == 4;
        }
    }
    let kinds: Vec<u8> = decoded.safepoints.iter().map(|point| point.kind).collect();
    assert!(saw_call, "缺少 CallReturn：{kinds:?}");
    assert!(saw_poll, "缺少 PollResume：{kinds:?}");
    assert!(saw_entry, "缺少 MorestackEntry：{kinds:?}");
    let consumed =
        runtime::consume_metadata(plan.x64_stackmap_section(), &[], plan.compression_runtime())
            .expect("runtime 必须消费栈图");
    assert_eq!(consumed.functions, plan.x64_stackmap_functions());
    assert_eq!(consumed.safepoints, plan.x64_stackmap_safepoints());
    let names = [
        plan.x64_stackmap_name(),
        plan.x64_unwind_name(),
        plan.x64_source_name(),
        ".debug_info",
        ".symtab",
    ];
    let kept: Vec<_> = names
        .into_iter()
        .filter(|name| !name.starts_with(".debug") && *name != ".symtab")
        .collect();
    assert!(kept.contains(&".gugu.stackmap"));
    assert!(kept.contains(&".eh_frame"));
    assert!(kept.contains(&".gugu.src"));
}

#[test]
// 这个测试调用 compile 做完整编译，耗时过长；移出默认套件
#[ignore = "integration test with full compilation"]
fn windows_metadata_uses_pe_section_names() {
    let compilation = compile(CALLS, TargetName::X86_64Windows);
    let plan = compilation.image_plan().expect("镜像计划");
    assert_eq!(plan.x64_stackmap_name(), ".gugustk");
    assert_eq!(plan.x64_unwind_name(), ".xdata");
    assert_eq!(plan.x64_source_name(), ".gugusrc");
    assert!(plan.x64_stackmap_section().starts_with(b"GUGUSM01"));
    assert!(plan.x64_unwind_section().starts_with(b"GUGUUN01"));
    runtime::decode_tables(plan.x64_stackmap_section()).expect("Windows 栈图可解码");
    assert_eq!(plan.linux_image_kind(), "");
    assert!(plan.linux_image().is_empty());
}

/// 挂起路径上的机器调用（`context_switch`）必须留下调用返回记录。
///
/// 停驻协程的保存上下文里就是这条 call 的返回地址；收集器与镜像内 walker 按它取根，缺记录
/// 会让每个停驻协程都无法扫描。
#[test]
// 这个测试调用 compile 做完整编译，耗时过长；移出默认套件
#[ignore = "integration test with full compilation"]
fn machine_calls_that_park_leave_a_call_return_record() {
    let direct = "fn park() {\n let value: uint = 0\n unsafe {\n  let p = std.ptr.addr_of(value)\n  std.runtime.context_switch(p, p, p, p)\n }\n}\nfn main() {\n park()\n}\n";
    // 真实停驻路径由 runtime 源驱动：子协程在空 channel 上 park，父协程随后唤醒。
    for source in [direct, PARKING] {
        check_machine_calls_have_records(source);
    }
}

/// 与镜像链接器同一算法的代码布局：片段世界与 `registered_fragments` 的登记机器片段按符号名
/// 统一排序、16 字节对齐，返回每个片段的 `code_rva`。
///
/// 生产侧的等价算法在 `metadata::assign_rvas`；这里独立重写一次，好让断言能抓住
/// 「只排片段世界」这类漂移，而不是复用被测实现自我印证。机器片段与 glue Item 实例共用
/// `__gugu_runtime_*` 前缀，两者必然交错。
fn code_layout(world: &X64World, target: TargetName) -> Vec<u64> {
    let machine: Vec<(String, u64)> = crate::backend::x64::fragments::registered_fragments(target)
        .iter()
        .map(|blob| {
            (
                blob.domain.mangle(blob.entries[0].glue),
                (blob.bytes.len() as u64).max(1),
            )
        })
        .collect();
    let mut layout: Vec<(String, u64, Option<usize>)> = world
        .fragments
        .iter()
        .enumerate()
        .map(|(index, fragment)| {
            (
                fragment.symbol.clone(),
                (fragment.bytes.len() as u64).max(1),
                Some(index),
            )
        })
        .collect();
    for (name, span) in &machine {
        layout.push((name.clone(), *span, None));
    }
    assert!(
        machine.iter().any(|(name, _)| world
            .fragments
            .iter()
            .any(|fragment| *name < fragment.symbol)),
        "登记机器片段必须与 glue Item 实例共用符号名空间，否则这条断言覆盖不到布局漂移"
    );
    layout.sort_by(|left, right| left.0.cmp(&right.0));
    let mut rvas = vec![0_u64; world.fragments.len()];
    let mut cursor = 0_u64;
    for (_, span, body) in &layout {
        cursor = cursor.div_ceil(16) * 16;
        if let Some(index) = body {
            rvas[*index] = cursor;
        }
        cursor += *span;
    }
    rvas
}

/// 断言该程序里每个 `context_switch` 机器调用的返回 PC 都有调用返回记录。
fn check_machine_calls_have_records(source: &str) {
    let compilation = compile(source, TargetName::X86_64Linux);
    let world = compilation.x64_world().expect("片段世界");
    let section = compilation
        .image_plan()
        .expect("镜像计划")
        .x64_stackmap_section()
        .to_vec();
    let decoded = runtime::decode_tables(&section).expect("栈图可解码");
    let rvas = code_layout(world, TargetName::X86_64Linux);
    let switch_name = crate::backend::x64::mangle::mangle_runtime("context_switch");
    let mut checked = 0_usize;
    for (index, fragment) in world.fragments.iter().enumerate() {
        let machine = fragment.relocations.iter().any(|relocation| {
            matches!(
                &relocation.target,
                crate::backend::x64::inst::RelocTarget::Lir(symbol)
                    if crate::backend::x64::mangle::mangle_symbol(symbol) == switch_name
            )
        });
        if !machine {
            continue;
        }
        // 停驻请求来自本帧：它的返回 PC 必须落在自己函数的调用返回记录上，且类别就是调用返回。
        let record = decoded
            .functions
            .iter()
            .find(|function| function.code_rva == rvas[index])
            .unwrap_or_else(|| panic!("{} 在栈图里没有函数记录", fragment.symbol));
        let start = usize::try_from(record.safepoint_start).expect("起始下标");
        let end = start + usize::try_from(record.safepoint_count).expect("数量");
        for site in &fragment.sites {
            if site.op != "RuntimeMachineCall" {
                continue;
            }
            for pc in &site.call_return_pcs {
                checked += 1;
                let covered = decoded.safepoints[start..end]
                    .iter()
                    .any(|point| point.pc_offset == *pc && matches!(point.kind, 0 | 2));
                assert!(
                    covered,
                    "{} 站点 op={} 的返回 PC {pc} 没有调用返回记录；line={}",
                    fragment.symbol, site.op, site.line
                );
            }
        }
    }
    assert!(checked > 0, "夹具必须包含 context_switch 机器调用");
}

/// 停驻协程的保存上下文里是调用返回地址，收集器只能按该 PC 找 map。
const PARKING: &str = "fn child(ch: chan[int]) {\n match ch.recv() {\n  Ok(value) => { _ = value }\n  Err(_) => { _ = 0 }\n }\n}\nfn main() {\n let channel: chan[int] = chan[int](0)\n let handle = async child(channel)\n channel.send(7)\n _ = handle.wait()\n}\n";

/// 每个机器站点的全部调用返回 PC 都必须在栈图 section 里有精确 map 记录。
///
/// 收集器（与镜像内 walker）按停驻上下文里的返回地址查 map，缺一条记录就只能在运行期
/// fatal；这条断言把「记录缺失」挡在镜像之前，并顺带钉住 section 的函数顺序与 `assign_rvas`
/// 的同一算法。
#[test]
// 这个测试调用 compile 做完整编译，耗时过长；移出默认套件
#[ignore = "integration test with full compilation"]
fn every_call_return_pc_has_a_stackmap_record() {
    for source in [CALLS, PARKING] {
        let compilation = compile(source, TargetName::X86_64Linux);
        let world = compilation.x64_world().expect("片段世界");
        let section = compilation
            .image_plan()
            .expect("镜像计划")
            .x64_stackmap_section()
            .to_vec();
        let decoded = runtime::decode_tables(&section).expect("栈图可解码");
        // fragment → code_rva：与镜像链接器同一算法，见 `code_layout`。
        let rvas = code_layout(world, TargetName::X86_64Linux);
        let mut sites = 0_usize;
        let mut uncovered: Vec<String> = Vec::new();
        for (index, fragment) in world.fragments.iter().enumerate() {
            if fragment
                .sites
                .iter()
                .all(|site| site.call_return_pcs.is_empty())
            {
                continue;
            }
            let rva = rvas[index];
            let record = decoded
                .functions
                .iter()
                .find(|function| function.code_rva == rva)
                .unwrap_or_else(|| panic!("{} 的 rva {rva} 没有函数记录", fragment.symbol));
            assert_eq!(
                record.code_size,
                u32::try_from(fragment.bytes.len()).expect("片段长度适配 u32"),
                "{} 的函数记录长度必须与片段一致",
                fragment.symbol
            );
            let start = usize::try_from(record.safepoint_start).expect("起始下标适配宿主");
            let end = start + usize::try_from(record.safepoint_count).expect("数量适配宿主");
            for site in &fragment.sites {
                for pc in &site.call_return_pcs {
                    sites += 1;
                    let covered = decoded.safepoints[start..end]
                        .iter()
                        .any(|point| point.pc_offset == *pc);
                    if !covered {
                        uncovered.push(site.op.clone());
                    }
                }
            }
        }
        uncovered.sort();
        uncovered.dedup();
        // 只有纯内存 glue（`rep movsb`/`rep stosb` 机器片段，内部不可能出现停点或回收）可以没有
        // 调用返回记录：它下面不存在能挂起的帧，帧链永远不会停在它的返回 PC 上。其余任何调用返回
        // PC 都必须有精确 map，否则运行期 walker 只能对整条帧链 fatal。
        assert!(
            uncovered
                .iter()
                .all(|op| matches!(op.as_str(), "Memcpy" | "Memmove" | "Memset")),
            "只有纯内存 glue 调用允许缺调用返回记录，实际缺口={uncovered:?}"
        );
        assert!(sites > 0, "夹具必须产生调用返回 PC");
        // 停驻路径必须真的存在机器调用（context_switch），否则这条夹具没有覆盖被检验的形态。
        let switch_name = crate::backend::x64::mangle::mangle_runtime("context_switch");
        let machine_calls = world
            .fragments
            .iter()
            .filter(|fragment| {
                fragment.relocations.iter().any(|relocation| {
                    matches!(
                        &relocation.target,
                        crate::backend::x64::inst::RelocTarget::Lir(symbol)
                            if crate::backend::x64::mangle::mangle_symbol(symbol) == switch_name
                    )
                })
            })
            .count();
        if source == PARKING {
            assert!(
                machine_calls > 0,
                "停驻夹具必须包含 context_switch 机器调用"
            );
        }
    }
}
