//! x64 编码与 lowering 的执行验收：用例与解码序列在宿主 VM 上真跑。
//!
//! 字节来自 compiler 的编码器，期望值来自独立参照实现（用例表）与 `CompressionPlane`
//! 的 checked 解码（解码夹具）。任何不匹配都以非零退出码结束并打印首个失败细节。
//! 该二进制不进默认测试套件，供 `cargo bench --bench x64_encoding` 与手工验证使用。

#[path = "support/vm.rs"]
mod vm;

use gugu_compiler::{TargetName, X64Case, X64Harness, X64Register};

/// 代码区大小：片段、冷边 stub 与末尾 `ret` 都放得下。
const CODE_BYTES: usize = 4096;
/// 数据区大小：内存槽位与冷边计数都在里面。
const DATA_BYTES: usize = 4096;
/// 冷边计数字节在数据区末尾。
const COUNTER_OFFSET: usize = DATA_BYTES - 16;
/// `ret` 指令。
const RET: u8 = 0xC3;
/// `inc byte ptr [rip + disp32]` 的操作码。
const INC_RIP_BYTE: [u8; 2] = [0xFE, 0x05];

/// 执行结果：通过的用例数与失败说明（最多保留 [`MAX_FAILURES`] 条，避免刷屏）。
#[derive(Clone, Debug, Eq, PartialEq)]
struct Outcome {
    matched: u32,
    failures: Vec<String>,
}

/// 失败详情上限。
const MAX_FAILURES: usize = 16;

impl Outcome {
    fn ok(&mut self) {
        self.matched += 1;
    }

    fn fail(&mut self, name: &str, detail: String) {
        if self.failures.len() < MAX_FAILURES {
            self.failures.push(format!("{name}: {detail}"));
        }
    }
}

fn main() {
    let target = if cfg!(target_os = "windows") {
        TargetName::X86_64Windows
    } else {
        TargetName::X86_64Linux
    };
    let harness = match X64Harness::new(target) {
        Ok(harness) => harness,
        Err(error) => {
            eprintln!("harness 构建失败: {error}");
            std::process::exit(1);
        }
    };
    let mut outcome = Outcome {
        matched: 0,
        failures: Vec::new(),
    };
    for case in harness.cases() {
        run_case(case, &mut outcome);
    }
    let (decodes, rejections) = run_decode(&harness, &mut outcome);
    let report = harness.report();
    println!(
        "cases={} matched={} decode-words={} decodes={} rejections={} switch-ok={} forms={} beyond-baseline={} decode-sites={} invariants={}",
        report.case_count,
        outcome.matched,
        report.decode_words,
        decodes,
        rejections,
        report.switch_matches_runtime,
        report.form_count,
        report.beyond_baseline_forms,
        report.cage_decode_sites,
        report.invariants_hold
    );
    if !outcome.failures.is_empty() {
        for failure in &outcome.failures {
            eprintln!("用例失败: {failure}");
        }
        std::process::exit(1);
    }
    if outcome.matched != report.case_count || !report.invariants_hold {
        eprintln!("用例数与报告不一致或自检未成立");
        std::process::exit(1);
    }
}

/// 在宿主上执行一个用例并比较结果。
fn run_case(case: &X64Case, outcome: &mut Outcome) {
    if std::env::var_os("X64_BENCH_VERBOSE").is_some() {
        let bytes: Vec<String> = case
            .code
            .bytes
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        eprintln!("case {} bytes={}", case.name, bytes.join(" "));
    }
    let data = vm::reserve(DATA_BYTES);
    vm::protect(data.base, data.bytes, false);
    let counter = data.base.wrapping_add(COUNTER_OFFSET);
    for slot in &case.memory {
        let address = data.base.wrapping_add(slot_offset(slot.index));
        // SAFETY: 槽位于刚提交的私有映射内，地址按 8 字节对齐。
        unsafe { std::ptr::write_unaligned(address.cast::<u64>(), slot.initial) };
    }
    let code = vm::reserve(CODE_BYTES);
    vm::protect(code.base, code.bytes, false);
    write_fragment(
        &code,
        &case.code.bytes,
        case.code.cold_edges.as_slice(),
        counter,
    );
    vm::protect(code.base, code.bytes, true);
    let mut state = [0_u64; vm::STATE_WORDS];
    for (register, value) in &case.inputs {
        set_register(&mut state, *register, *value);
    }
    if let Some(register) = case.memory_base {
        set_register(&mut state, register, data.base.addr() as u128);
    }
    // SAFETY: 片段已映射为可执行、以 `ret` 结束且不修改 rsp；state 是 48 个可写 u64。
    unsafe { vm::invoke(code.base.addr(), state.as_mut_ptr()) };
    if let Some(register) = case.output {
        let actual = get_register(&state, register);
        if actual != case.expected {
            outcome.fail(
                &case.name,
                format!(
                    "{} = {actual:#x}，期望 {:#x}",
                    register.name(),
                    case.expected
                ),
            );
            return;
        }
    }
    for (register, expected) in &case.extra_results {
        let actual = get_register(&state, *register);
        if actual != *expected {
            outcome.fail(
                &case.name,
                format!("{} = {actual:#x}，期望 {expected:#x}", register.name()),
            );
            return;
        }
    }
    for slot in &case.memory {
        let address = data.base.wrapping_add(slot_offset(slot.index));
        // SAFETY: 槽位在提交的映射内，地址按 8 字节对齐。
        let actual = unsafe { std::ptr::read_unaligned(address.cast::<u64>()) };
        if actual != slot.expected {
            outcome.fail(
                &case.name,
                format!(
                    "内存槽 {} = {actual:#x}，期望 {:#x}",
                    slot.index, slot.expected
                ),
            );
            return;
        }
    }
    // SAFETY: 冷边计数字节在提交的映射内。
    let taken = unsafe { std::ptr::read_unaligned(counter) } != 0;
    if taken != case.expects_cold_edge {
        outcome.fail(
            &case.name,
            format!("冷边进入 = {taken}，期望 {}", case.expects_cold_edge),
        );
        return;
    }
    outcome.ok();
}

/// 执行解码夹具：逐字执行解码片段并核对计数。
fn run_decode(harness: &X64Harness, outcome: &mut Outcome) -> (u64, u64) {
    let fixture = harness.decode();
    let data = vm::reserve(DATA_BYTES);
    vm::protect(data.base, data.bytes, false);
    let control = data.base;
    // SAFETY: 控制记录落在刚提交的私有映射内，长度不超过数据区。
    unsafe {
        std::ptr::copy_nonoverlapping(fixture.control.as_ptr(), control, fixture.control.len())
    };
    let code = vm::reserve(CODE_BYTES);
    vm::protect(code.base, code.bytes, false);
    let end = write_fragment(
        &code,
        &fixture.code.bytes,
        fixture.code.cold_edges.as_slice(),
        // 解码拒绝走冷边 stub：计数必须落在控制记录之外，否则会写坏 generation 字段。
        data.base.wrapping_add(COUNTER_OFFSET),
    );
    patch_control_relocations(&code, end, control, &fixture.code);
    vm::protect(code.base, code.bytes, true);
    for (word, expected) in &fixture.words {
        let mut state = [0_u64; vm::STATE_WORDS];
        set_register(&mut state, fixture.word_register, u128::from(*word));
        // SAFETY: 片段已映射为可执行、以 `ret` 结束且不修改 rsp；state 是 48 个可写 u64。
        unsafe { vm::invoke(code.base.addr(), state.as_mut_ptr()) };
        let actual = get_register(&state, fixture.result_register);
        let wanted = u128::from(expected.unwrap_or(0));
        if std::env::var_os("X64_BENCH_VERBOSE").is_some() {
            eprintln!(
                "decode word {word:#x} expected={expected:?} actual={actual:#x} decodes={} rejections={}",
                read_field(control, fixture.decodes_field),
                read_field(control, fixture.rejections_field)
            );
        }
        if expected.is_some() && actual != wanted {
            outcome.fail(
                "decode",
                format!("字 {word:#x} 解出 {actual:#x}，期望 {wanted:#x}"),
            );
        }
        if expected.is_none() && actual != 0 {
            outcome.fail(
                "decode",
                format!("字 {word:#x} 必须不解出地址，实际 {actual:#x}"),
            );
        }
    }
    let decodes = read_field(control, fixture.decodes_field);
    let rejections = read_field(control, fixture.rejections_field);
    if decodes != fixture.decodes || rejections != fixture.rejections {
        outcome.fail(
            "decode",
            format!(
                "计数 decodes={decodes}/{} rejections={rejections}/{}",
                fixture.decodes, fixture.rejections
            ),
        );
    }
    (decodes, rejections)
}

/// 把片段与冷边 stub 写进代码区，返回片段末尾（`ret` 之后）的地址。
fn write_fragment(
    code: &vm::Mapping,
    bytes: &[u8],
    cold_edges: &[u32],
    counter: *mut u8,
) -> *mut u8 {
    // SAFETY: 片段落在刚提交的私有映射内，长度不超过代码区。
    unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), code.base, bytes.len()) };
    let entry_ret = code.base.wrapping_add(bytes.len());
    // SAFETY: 末尾 `ret` 仍在代码区内。
    unsafe { std::ptr::write(entry_ret, RET) };
    let stub = code.base.wrapping_add(bytes.len() + 1);
    // 冷边 stub：`inc byte ptr [rip+disp]; ret`，因此"从冷边离开"可观察。
    // SAFETY: stub 与计数器都在已提交映射内。
    unsafe {
        std::ptr::copy_nonoverlapping(INC_RIP_BYTE.as_ptr(), stub, INC_RIP_BYTE.len());
        let disp_field = stub.wrapping_add(INC_RIP_BYTE.len());
        let disp = counter.addr() as i64 - (disp_field.addr() as i64 + 4);
        let disp = i32::try_from(disp).expect("冷边 stub 位移适配 i32");
        std::ptr::copy_nonoverlapping(disp.to_le_bytes().as_ptr(), disp_field, 4);
        std::ptr::write(stub.wrapping_add(6), RET);
    }
    for offset in cold_edges {
        let field = code
            .base
            .wrapping_add(usize::try_from(*offset).expect("冷边偏移适配 usize"));
        // rel32 覆盖 `field` 起 4 字节；目标地址按「下一条指令」计算。
        let disp = stub.addr() as i64 - (field.addr() as i64 + 4);
        let disp = i32::try_from(disp).expect("冷边位移适配 i32");
        // SAFETY: 位移字段落在片段字节内（编码器登记过该字段）。
        unsafe { std::ptr::copy_nonoverlapping(disp.to_le_bytes().as_ptr(), field, 4) };
    }
    stub.wrapping_add(7)
}

/// 补齐 `cage-control` 的 PC 相对重定位。
fn patch_control_relocations(
    code: &vm::Mapping,
    _end: *mut u8,
    control: *mut u8,
    fragment: &gugu_compiler::X64Code,
) {
    for relocation in &fragment.relocations {
        if relocation.kind != "pc-rel32" || relocation.target != "cage-control" {
            continue;
        }
        let field = code
            .base
            .wrapping_add(usize::try_from(relocation.offset).expect("字段偏移适配 usize"));
        let target = control.wrapping_offset(relocation.addend as isize);
        let disp = target.addr() as i64 - (field.addr() as i64 + 4);
        let disp = i32::try_from(disp).expect("控制记录位移适配 i32");
        // SAFETY: 位移字段落在片段字节内（编码器登记过该字段）。
        unsafe { std::ptr::copy_nonoverlapping(disp.to_le_bytes().as_ptr(), field, 4) };
    }
}

/// 控制记录字段的当前值。
fn read_field(control: *mut u8, field: (u32, u32)) -> u64 {
    let mut bytes = [0_u8; 8];
    let offset = usize::try_from(field.0).expect("字段偏移适配 usize");
    let size = usize::try_from(field.1).expect("字段长度适配 usize");
    // SAFETY: 字段落在控制记录页内，长度不超过 8 字节。
    unsafe {
        std::ptr::copy_nonoverlapping(
            control.wrapping_add(offset),
            bytes.as_mut_ptr(),
            size.min(8),
        )
    };
    u64::from_le_bytes(bytes)
}

/// 内存槽位的字节偏移。
fn slot_offset(index: u32) -> usize {
    usize::try_from(index).expect("槽位下标适配 usize") * 8
}

/// 写入寄存器初值。
fn set_register(state: &mut [u64; vm::STATE_WORDS], register: X64Register, value: u128) {
    match register {
        X64Register::Gpr(code) => state[usize::from(code)] = low(value),
        X64Register::Xmm(code) => {
            let base = vm::GPR_WORDS + usize::from(code) * 2;
            state[base] = low(value);
            state[base + 1] = high(value);
        }
    }
}

/// 读取寄存器结果。
fn get_register(state: &[u64; vm::STATE_WORDS], register: X64Register) -> u128 {
    match register {
        X64Register::Gpr(code) => u128::from(state[usize::from(code)]),
        X64Register::Xmm(code) => {
            let base = vm::GPR_WORDS + usize::from(code) * 2;
            u128::from(state[base]) | (u128::from(state[base + 1]) << 64)
        }
    }
}

fn low(value: u128) -> u64 {
    u64::try_from(value & u128::from(u64::MAX)).expect("低 64 位适配 u64")
}

fn high(value: u128) -> u64 {
    u64::try_from(value >> 64).expect("高 64 位适配 u64")
}
