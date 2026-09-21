//! 寄存器分配与 frame 合成的契约回归。
//!
//! 断言的是分配结果的**对外契约**：逐片段 frame 与 payload 的关系、溢出流量与破环统计、
//! 重编译的确定性，以及取址局部跨调用时仍然拿到 frame。序列在宿主上的真实执行由
//! `benches/x64_frame.rs` 覆盖，这里只保证分配计划本身自洽。

use crate::{CompileRequest, Compiler, TargetName};

/// 寄存器压力场景：9 个参数、循环里两次 6 参数调用，必须产生溢出与破环。
const PRESSURE: &str = "fn mix(a: int, b: int, c: int, d: int, e: int, f: int) int {
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
    let total = 0
    let index = 0
    while index < 4 {
        let left = mix(index + 1, index + 2, index + 3, index + 4, index + 5, index + 6)
        let right = mix(left, spare(left), index + 2, index + 3, index + 4, index + 5)
        total = total + left
        total = total + right
        index = index + 1
    }
    if total > 10 { _ = spare(total) }
}
";

/// 取址场景：局部聚合经 `&Pair` 跨调用使用，栈地址值必须在 frame 里有位置。
const ADDRESSED: &str = "#[repr(C, align(8))]
struct Pair { a: int, b: int }

fn bump(p: &Pair) int {
    p.a + p.b
}

fn main() {
    let pair = Pair { a: 1, b: 2 }
    let sum = bump(&pair)
    if sum > 0 { _ = sum }
}
";

fn compile(source: &str) -> crate::Compilation {
    let compilation = Compiler::new().compile(CompileRequest::single_file(
        "main.gg",
        source,
        TargetName::X86_64Linux,
    ));
    assert!(
        compilation.is_success(),
        "{:?}",
        compilation.diagnostics().items()
    );
    compilation
}

#[test]
fn allocated_frames_keep_payload_inside_frame() {
    let compilation = compile(PRESSURE);
    let fragments = compilation.x64_fragments().expect("片段视图");
    assert!(!fragments.fragments.is_empty(), "分配必须产出片段");
    let mut checked = 0_u32;
    let mut spilling = 0_u32;
    for fragment in &fragments.fragments {
        let frame = &fragment.frame;
        assert!(
            frame.payload_bytes <= frame.frame_size,
            "{}: payload {} 超过 frame {}",
            fragment.symbol,
            frame.payload_bytes,
            frame.frame_size
        );
        assert!(
            frame.spill_slot_count * 8 <= frame.payload_bytes,
            "{}: {} 个溢出槽装不进 {} 字节 payload",
            fragment.symbol,
            frame.spill_slot_count,
            frame.payload_bytes
        );
        // 保存集只有 rbp/r12/r13。
        assert!(
            frame.saved_gpr_count <= 3,
            "{}: 保存了 {} 个 callee-saved GPR",
            fragment.symbol,
            frame.saved_gpr_count
        );
        if let Some(offset) = frame.scratch_offset {
            assert!(
                offset + 16 <= frame.payload_bytes,
                "{}: copy scratch {}..{} 超出 payload {}",
                fragment.symbol,
                offset,
                offset + 16,
                frame.payload_bytes
            );
        }
        assert!(
            frame.required_frame >= frame.frame_size,
            "{}: required {} 小于 frame {}",
            fragment.symbol,
            frame.required_frame,
            frame.frame_size
        );
        assert_eq!(
            frame.entry_required_frame,
            frame.required_frame + 8,
            "{}: entry_required_frame 必须是 required_frame 加返回地址",
            fragment.symbol
        );
        if frame.frame_size > 0 {
            assert!(frame.checked, "{}: 有帧就必须做栈检查", fragment.symbol);
        }
        checked += u32::from(frame.checked);
        spilling += u32::from(frame.spill_slot_count > 0);
    }
    assert!(checked > 0, "入口函数必须发射栈检查");
    assert!(spilling > 0, "该场景必须出现溢出槽");
}

#[test]
fn calls_report_spill_traffic_and_copy_cycles() {
    let compilation = compile(PRESSURE);
    let plan = compilation.image_plan().expect("镜像计划");
    assert!(plan.x64_spill_slot_count() > 0, "必须分配溢出槽");
    assert!(plan.x64_reload_count() > 0, "必须出现溢出重载");
    assert!(plan.x64_spill_store_count() > 0, "必须出现溢出写回");
    assert!(plan.x64_copy_cycle_count() >= 1, "参数置换必须破环");
    assert!(
        plan.x64_copy_move_count() >= plan.x64_copy_cycle_count(),
        "拷贝移动数不能少于破环数"
    );
    assert!(plan.x64_allocated_values() > 0, "必须处理过虚拟值");
    assert!(plan.x64_frame_size_max() > 0, "带溢出的函数必须有 frame");
    // GPR 池是 12 个通用寄存器：峰值并发不可能超过它。
    assert!(plan.x64_peak_live_gpr() <= 12, "峰值活跃 GPR 超过池大小");
}

#[test]
fn allocation_is_deterministic() {
    let first = compile(PRESSURE);
    let second = compile(PRESSURE);
    let left = first.image_plan().expect("镜像计划");
    let right = second.image_plan().expect("镜像计划");
    assert_eq!(
        left.x64_fragment_fingerprint(),
        right.x64_fragment_fingerprint()
    );
    assert_eq!(left.x64_frame_size_max(), right.x64_frame_size_max());
    assert_eq!(left.x64_spill_slot_count(), right.x64_spill_slot_count());
    assert_eq!(left.x64_copy_cycle_count(), right.x64_copy_cycle_count());
    assert_eq!(first.dump_x64(), second.dump_x64());
}

/// 直线、无调用，但活值多到要用 `rbp`：叶分类会删掉栈检查，分配后必须补回来。
const WIDE_LEAF: &str = "fn wide(
    a: int, b: int, c: int, d: int, e: int, f: int, g: int, h: int, i: int
) int {
    let extra = a + 1
    extra + b + c + d + e + f + g + h + i
}

fn main() {
    _ = wide(1, 2, 3, 4, 5, 6, 7, 8, 9)
}
";

/// 第十个参数是栈上的指针，调用点必须记下 outgoing 指针字。
const STACK_POINTER_ARG: &str = "#[repr(C, align(8))]
struct Pair { a: int, b: int }

fn take(
    a: int, b: int, c: int, d: int, e: int, f: int, g: int, h: int, i: int, p: &Pair
) int {
    p.a + i
}

fn main() {
    let pair = Pair { a: 4, b: 5 }
    _ = take(1, 2, 3, 4, 5, 6, 7, 8, 9, &pair)
}
";

#[test]
fn addressed_local_survives_a_call() {
    let compilation = compile(ADDRESSED);
    let plan = compilation.image_plan().expect("镜像计划");
    let lir = compilation.dump_lir().expect("入口有 LIR");
    assert!(lir.contains("StackAddr"), "取址局部必须走 StackAddr");
    assert!(
        plan.x64_spill_slot_count() > 0,
        "跨调用的栈地址值必须有 frame 位置"
    );
    assert!(plan.x64_frame_size_max() > 0, "取址函数必须有 frame");
    let entry = compilation
        .x64_fragments()
        .expect("片段视图")
        .fragments
        .iter()
        .any(|fragment| fragment.frame.frame_size > 0);
    assert!(entry, "至少一个片段必须有非空 frame");
    let dump = compilation.dump_x64().expect("机器码转储");
    assert!(
        dump.contains("root=2"),
        "取址局部的指针必须记成栈指针根：{dump}"
    );
    assert!(
        dump.contains("locals=1") || dump.contains("locals=2"),
        "取址局部必须落在 frame 里：{dump}"
    );
}

/// 字段偏移同时重建基址和常量索引。聚合初始化会调用 memmove glue，执行 bench 接不住，
/// 这里直接看机器序列：两条 scratch 不能是同一个寄存器。
const FIELD_OFFSET: &str = "#[repr(C, align(8))]
struct Pair { a: int, b: int }

fn main() {
    let pair = Pair { a: 20, b: 22 }
    _ = pair.b
}
";

#[test]
fn field_offset_reloads_base_and_index_into_different_scratches() {
    let compilation = compile(FIELD_OFFSET);
    let dump = compilation.dump_x64().expect("机器码转储");
    let offset = dump
        .lines()
        .find(|line| line.contains("PtrOffset [") && line.matches("lea ").count() >= 2)
        .unwrap_or_else(|| panic!("必须有带重建的字段偏移：{dump}"));
    assert!(
        !offset.contains("[r11 + r11]"),
        "基址和索引不能共用 scratch：{offset}"
    );
    assert!(
        offset.contains("lea [r11 + "),
        "基址应先重建进 r11，索引用另一个寄存器：{offset}"
    );
}

#[test]
fn wide_leaf_regains_stack_check() {
    let compilation = compile(WIDE_LEAF);
    let dump = compilation.dump_x64().expect("机器码转储");
    let mut checked_leaves = 0_u32;
    for piece in dump.split("x64-fragment ").skip(1) {
        if piece.contains(" cmp ") && piece.contains(" jg ") && piece.contains(" sub ") {
            checked_leaves += 1;
        }
    }
    assert!(
        checked_leaves >= 2,
        "入口和多活值叶都必须有 cmp/jg/sub：{dump}"
    );
}

#[test]
fn stack_pointer_argument_is_an_outgoing_root() {
    let compilation = compile(STACK_POINTER_ARG);
    let dump = compilation.dump_x64().expect("机器码转储");
    assert!(
        dump.contains("x64-outgoing "),
        "栈上的指针实参必须记到调用点：{dump}"
    );
}
