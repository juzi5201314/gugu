//! 分配后的机器码执行验收：把编译产物按内部 ABI 真正跑在宿主 CPU 上。
//!
//! 覆盖默认测试套件无法覆盖的部分：spill 槽读写、prologue/epilogue 的 `rsp` 调整、
//! callee-saved 保存、栈参数传递与并行拷贝（含破环 scratch）必须在真实执行里自洽。
//!
//! 片段按符号顺序连续映射；指向内部片段的重定位解析到映射内地址，`report` 解析到
//! 捕获 stub（`mov [rip+disp32], rdi; ret`），其余符号、冷边与 cage-control 一律指向
//! `ret` stub。入口片段的 `StackCheck` 读取 `[r14 + stack_check_offset]`，因此 `r14`
//! 指向数据区、该处写 0（栈永远充足）；`r15` 指向同一页的可写槽供轮询写入。
//! 片段在自己的 epilogue 里恢复 `rsp` 后 `ret`，所以适配器的栈状态始终自洽。
//!
//! 该二进制不进默认测试套件，供 `cargo bench --bench x64_frame` 与手工验证使用。

#[path = "support/vm.rs"]
mod vm;

use gugu_compiler::{CompileRequest, Compiler, TargetName, X64Fragment};
use std::collections::BTreeMap;

/// 代码区大小：所有片段、捕获 stub 与 `ret` stub 都放得下。
const CODE_BYTES: usize = 1 << 20;
/// 数据区大小：栈上限字、轮询槽与捕获槽都在第一页内。
const DATA_BYTES: usize = 1 << 16;
/// `ret`。
const RET: u8 = 0xC3;
/// 每个 stub 占用的字节数。
const STUB_BYTES: usize = 16;
/// `mov [rip+disp32], rdi`：`rdi` 是 C ABI 的第一个参数寄存器。
const CAPTURE_PREFIX: [u8; 3] = [0x48, 0x89, 0x3D];
/// `inc qword [rip+disp32]`。
const INC_RIP_QWORD: [u8; 3] = [0x48, 0xFF, 0x05];
/// `r14` 在状态里的下标：内部 ABI 保留的运行时上下文寄存器。
const R14: usize = 14;
/// `r15` 在状态里的下标：内部 ABI 保留的调度器轮询寄存器。
const R15: usize = 15;
/// 轮询槽在数据区内的偏移。
const POLL_OFFSET: usize = 16;
/// 捕获槽在数据区内的偏移。
const CAPTURE_OFFSET: usize = 24;
/// 捕获 stub 命中计数在数据区内的偏移。
const COUNT_OFFSET: usize = 32;
/// 普通 stub 命中计数在数据区内的偏移。
const PLAIN_COUNT_OFFSET: usize = 40;

/// 一个执行场景：只改 `report` 的实参表达式，并在需要时追加一个纯算术循环。
struct Scenario {
    /// 场景名。
    name: &'static str,
    /// `report(...)` 的实参表达式。
    body: &'static str,
    /// 主函数里是否包含调用（`mix` 循环）。
    calls: bool,
    /// 是否要求产生溢出。
    spills: bool,
}

fn main() {
    let target = if cfg!(target_os = "windows") {
        TargetName::X86_64Windows
    } else {
        TargetName::X86_64Linux
    };
    let scenarios = [
        Scenario {
            name: "leaf",
            body: "spare(7) * 3",
            calls: false,
            spills: false,
        },
        Scenario {
            name: "loop",
            body: "sum",
            calls: false,
            spills: false,
        },
        Scenario {
            name: "spill",
            body: "total",
            calls: true,
            spills: true,
        },
        Scenario {
            name: "call",
            body: "spare(total) + mix(1, 2, 3, 4, 5, 6)",
            calls: true,
            spills: true,
        },
    ];
    let only = std::env::var("X64_FRAME_ONLY")
        .ok()
        .filter(|name| !name.is_empty());
    let mut failures = Vec::new();
    let mut ran = 0;
    for scenario in &scenarios {
        if only.as_deref().is_some_and(|name| name != scenario.name) {
            continue;
        }
        ran += 1;
        if let Err(failure) = run(scenario, target) {
            failures.push(format!("{}: {failure}", scenario.name));
        }
    }
    if !failures.is_empty() {
        for failure in &failures {
            eprintln!("场景失败: {failure}");
        }
        std::process::exit(1);
    }
    println!("x64_frame scenarios={ran} ok");
}

/// 编译、映射并执行一个场景。
fn run(scenario: &Scenario, target: TargetName) -> Result<(), String> {
    let compilation = Compiler::new().compile(CompileRequest::single_file(
        "main.gg",
        program(scenario),
        target,
    ));
    if !compilation.is_success() {
        return Err(format!("{:?}", compilation.diagnostics().items()));
    }
    let plan = compilation.image_plan().expect("镜像计划");
    if scenario.spills && (plan.x64_spill_slot_count() == 0 || plan.x64_reload_count() == 0) {
        return Err(format!(
            "场景必须触发溢出：slots={} reloads={}",
            plan.x64_spill_slot_count(),
            plan.x64_reload_count()
        ));
    }
    if scenario.calls && plan.x64_copy_cycle_count() == 0 {
        return Err("带调用的场景必须出现破环".to_owned());
    }
    let fragments = compilation.x64_fragments().expect("片段视图");
    let entry = fragments
        .find(fragments.entry_symbol.as_str())
        .ok_or_else(|| "找不到入口片段".to_owned())?;
    if !entry.frame.checked || entry.frame.frame_size == 0 {
        return Err(format!(
            "入口片段必须有栈检查与 frame：checked={} frame={}",
            entry.frame.checked, entry.frame.frame_size
        ));
    }
    if !entry
        .relocations
        .iter()
        .any(|relocation| relocation.target == "report")
    {
        return Err("入口片段必须直接调用 report".to_owned());
    }
    let (base, total) = layout(&fragments.fragments)?;
    let entry_offset = base
        .get(&entry.symbol)
        .copied()
        .ok_or_else(|| "入口片段没有机器字节".to_owned())?;
    let code = vm::reserve(CODE_BYTES);
    vm::protect(code.base, code.bytes, false);
    let data = vm::reserve(DATA_BYTES);
    vm::protect(data.base, data.bytes, false);
    let capture = code.base.wrapping_add(total);
    let plain = capture.wrapping_add(STUB_BYTES);
    let capture_slot = data.base.wrapping_add(CAPTURE_OFFSET);
    let capture_count = data.base.wrapping_add(COUNT_OFFSET);
    let plain_count = data.base.wrapping_add(PLAIN_COUNT_OFFSET);
    // SAFETY: 数据区刚落成可写；四个槽都在第一页内、按 8 字节对齐，stub 指向刚预留的代码区。
    unsafe {
        std::ptr::write_unaligned(data.base.wrapping_add(POLL_OFFSET).cast::<u64>(), 0);
        std::ptr::write_unaligned(capture_slot.cast::<u64>(), 0);
        std::ptr::write_unaligned(capture_count.cast::<u64>(), 0);
        std::ptr::write_unaligned(plain_count.cast::<u64>(), 0);
        let limit = data
            .base
            .wrapping_add(entry.frame.stack_check_offset as usize);
        std::ptr::write_unaligned(limit.cast::<u64>(), 0);
        write_stubs(capture, capture_slot, capture_count, plain, plain_count);
    }
    write_code(&code, &fragments.fragments, &base);
    patch(
        &code,
        &fragments.fragments,
        &base,
        data.base,
        plain,
        capture,
    )?;
    dump_fragments(&fragments.fragments, &base, scenario.name);
    vm::protect(code.base, code.bytes, true);
    let mut state = [0_u64; vm::STATE_WORDS];
    state[R14] = data.base.addr() as u64;
    state[R15] = data.base.addr() as u64 + POLL_OFFSET as u64;
    // SAFETY: 片段已映射为可执行；每个片段在自己的 epilogue 里恢复 `rsp` 后 `ret`。
    unsafe {
        vm::invoke(
            code.base.wrapping_add(entry_offset).addr(),
            state.as_mut_ptr(),
        )
    };
    // SAFETY: 三个槽都在已提交的数据映射内、按 8 字节对齐。
    let (captured, calls, stubbed) = unsafe {
        (
            std::ptr::read_unaligned(capture_slot.cast::<u64>()),
            std::ptr::read_unaligned(capture_count.cast::<u64>()),
            std::ptr::read_unaligned(plain_count.cast::<u64>()),
        )
    };
    if calls != 1 {
        return Err(format!("report 调用次数 {calls}（stub 命中 {stubbed}）"));
    }
    if stubbed != 0 {
        return Err(format!("执行进入了未知目标 stub 共 {stubbed} 次"));
    }
    let expected = reference(scenario);
    if captured != expected as u64 {
        return Err(format!(
            "report 捕获 {captured}，期望 {expected}（stub 命中 {stubbed}）"
        ));
    }
    Ok(())
}

/// 场景程序：`report` 是宿主捕获用的外部 C 函数，其余都是受管代码。
fn program(scenario: &Scenario) -> String {
    const TEMPLATE: &str = r#"#[ffi(leaf(stack = 0))]
extern "C" fn report(value: int)

fn mix(a: int, b: int, c: int, d: int, e: int, f: int) int {
    let s0 = a + b
    let s1 = s0 * c
    let s2 = s1 - d
    let s3 = s2 + e
    s3 * f
}

fn spare(x: int) int {
    let y = x * 7
    y - 3
}

fn main() {
    let sum = 0
    let step = 0
    while step < 6 {
        sum = sum + step * step + 1
        step = step + 1
    }
{CALL_LOOP}    report({BODY})
}
"#;
    let calls = if scenario.calls {
        "    let total = sum\n    let index = 0\n    while index < 4 {\n\
         \x20       let left = mix(index + 1, index + 2, index + 3, index + 4, index + 5, index + 6)\n\
         \x20       let right = mix(left, spare(left), index + 2, index + 3, index + 4, index + 5)\n\
         \x20       total = total + left\n\
         \x20       total = total + right\n\
         \x20       index = index + 1\n\
         \x20   }\n"
    } else {
        ""
    };
    TEMPLATE
        .replace("{CALL_LOOP}", calls)
        .replace("{BODY}", scenario.body)
}

/// 宿主参照实现：与场景程序同一套算式，独立用 Rust 再算一遍。
fn reference(scenario: &Scenario) -> i64 {
    fn mix(a: i64, b: i64, c: i64, d: i64, e: i64, f: i64) -> i64 {
        let s0 = a + b;
        let s1 = s0 * c;
        let s2 = s1 - d;
        let s3 = s2 + e;
        s3 * f
    }
    fn spare(x: i64) -> i64 {
        x * 7 - 3
    }
    let mut sum = 0_i64;
    let mut step = 0_i64;
    while step < 6 {
        sum += step * step + 1;
        step += 1;
    }
    let mut total = sum;
    if scenario.calls {
        let mut index = 0_i64;
        while index < 4 {
            let left = mix(
                index + 1,
                index + 2,
                index + 3,
                index + 4,
                index + 5,
                index + 6,
            );
            let right = mix(
                left,
                spare(left),
                index + 2,
                index + 3,
                index + 4,
                index + 5,
            );
            total += left;
            total += right;
            index += 1;
        }
    }
    match scenario.body {
        "spare(7) * 3" => spare(7) * 3,
        "sum" => sum,
        "total" => total,
        _ => spare(total) + mix(1, 2, 3, 4, 5, 6),
    }
}

/// 调试用：`X64_FRAME_DUMP=<目录>` 时把每个片段的机器字节与重定位写成文件。
fn dump_fragments(fragments: &[X64Fragment], base: &BTreeMap<String, usize>, scenario: &str) {
    let Some(directory) = std::env::var_os("X64_FRAME_DUMP") else {
        return;
    };
    let directory = std::path::PathBuf::from(directory);
    for fragment in fragments {
        let Some(offset) = base.get(&fragment.symbol) else {
            continue;
        };
        let _ = std::fs::write(
            directory.join(format!("{scenario}-{offset:06}-{}.bin", fragment.symbol)),
            &fragment.bytes,
        );
    }
}

/// 按符号顺序连续排布片段，返回符号到偏移的映射与片段字节总数。
fn layout(fragments: &[X64Fragment]) -> Result<(BTreeMap<String, usize>, usize), String> {
    let mut base = BTreeMap::new();
    let mut at = 0_usize;
    for fragment in fragments {
        if fragment.bytes.is_empty() {
            continue;
        }
        if base.insert(fragment.symbol.clone(), at).is_some() {
            return Err(format!("片段符号重复：{}", fragment.symbol));
        }
        at += fragment.bytes.len();
    }
    if at + STUB_BYTES * 2 > CODE_BYTES {
        return Err(format!("片段总长 {at} 超过代码区"));
    }
    Ok((base, at))
}

/// 把片段逐个写进代码映射。
fn write_code(code: &vm::Mapping, fragments: &[X64Fragment], base: &BTreeMap<String, usize>) {
    for fragment in fragments {
        let Some(offset) = base.get(&fragment.symbol) else {
            continue;
        };
        // SAFETY: 片段字节写进刚提交的私有映射，范围由 `layout` 保证。
        unsafe {
            std::ptr::copy_nonoverlapping(
                fragment.bytes.as_ptr(),
                code.base.wrapping_add(*offset),
                fragment.bytes.len(),
            )
        };
    }
}

/// 写捕获 stub（`mov [rip+disp32], rdi; inc qword [rip+disp32]; ret`）与普通 stub
/// （`inc qword [rip+disp32]; ret`）。两个 stub 各自占 [`STUB_BYTES`] 字节。
///
/// # Safety
/// `capture` 与 `plain` 必须指向预留代码区内未使用、互不重叠的 [`STUB_BYTES`] 字节。
unsafe fn write_stubs(
    capture: *mut u8,
    slot: *mut u8,
    count: *mut u8,
    plain: *mut u8,
    plain_count: *mut u8,
) {
    // RIP 相对位移以「指令的下一条」为基准：`mov` 7 字节、`inc` 7 字节。
    let mov_end = capture.addr() as i64 + 7;
    let inc_end = capture.addr() as i64 + 14;
    let plain_end = plain.addr() as i64 + 7;
    let slot_disp = i32::try_from(slot.addr() as i64 - mov_end).expect("捕获槽位移适配 i32");
    let count_disp = i32::try_from(count.addr() as i64 - inc_end).expect("计数位移适配 i32");
    let plain_disp =
        i32::try_from(plain_count.addr() as i64 - plain_end).expect("计数位移适配 i32");
    // SAFETY: 由调用者保证两个 stub 落在预留代码区内且不重叠。
    unsafe {
        std::ptr::copy_nonoverlapping(CAPTURE_PREFIX.as_ptr(), capture, CAPTURE_PREFIX.len());
        std::ptr::copy_nonoverlapping(slot_disp.to_le_bytes().as_ptr(), capture.add(3), 4);
        std::ptr::copy_nonoverlapping(INC_RIP_QWORD.as_ptr(), capture.add(7), INC_RIP_QWORD.len());
        std::ptr::copy_nonoverlapping(count_disp.to_le_bytes().as_ptr(), capture.add(10), 4);
        std::ptr::write(capture.add(14), RET);
        std::ptr::copy_nonoverlapping(INC_RIP_QWORD.as_ptr(), plain, INC_RIP_QWORD.len());
        std::ptr::copy_nonoverlapping(plain_disp.to_le_bytes().as_ptr(), plain.add(3), 4);
        std::ptr::write(plain.add(7), RET);
    }
}

/// 修正全部重定位：内部符号解析到映射内地址，`report` 落到捕获 stub，其余落到 `ret`。
fn patch(
    code: &vm::Mapping,
    fragments: &[X64Fragment],
    base: &BTreeMap<String, usize>,
    data: *mut u8,
    plain: *mut u8,
    capture: *mut u8,
) -> Result<(), String> {
    for fragment in fragments {
        let Some(offset) = base.get(&fragment.symbol) else {
            continue;
        };
        let here = code.base.wrapping_add(*offset);
        for relocation in &fragment.relocations {
            let target = if let Some(target) = base.get(&relocation.target) {
                code.base.wrapping_add(*target)
            } else if relocation.target == "report" {
                capture
            } else if relocation.target == "cage-control" {
                data
            } else {
                plain
            };
            let field = here.wrapping_add(relocation.offset as usize);
            let addend = relocation.addend;
            match relocation.kind {
                "pc-rel32" => {
                    let next = field.addr() as i64 + 4;
                    let displacement = target.addr() as i64 + addend - next;
                    let displacement = i32::try_from(displacement)
                        .map_err(|_| format!("{} 的 rel32 位移溢出", relocation.target))?;
                    // SAFETY: 字段落在片段字节内（编码器登记过该字段）。
                    unsafe {
                        std::ptr::copy_nonoverlapping(displacement.to_le_bytes().as_ptr(), field, 4)
                    };
                }
                "abs64" => {
                    let absolute = target.addr() as i64 + addend;
                    // SAFETY: 字段落在片段字节内（编码器登记过该字段）。
                    unsafe {
                        std::ptr::copy_nonoverlapping(absolute.to_le_bytes().as_ptr(), field, 8)
                    };
                }
                other => return Err(format!("不支持的重定位种类 {other}")),
            }
        }
    }
    Ok(())
}
