use crate::frontend::gir::placement::PlacementKind;
use crate::lir::pass::{
    self, constants,
    policy::{OptimizationPolicyV1, POLL_BUDGET},
    poll,
    rewrite::{Editor, Term},
};
use crate::lir::{
    body::{
        self, Body, Definition, Op, Origin, Provenance, Terminator, Type, ValueId, ValueType, id,
        range,
    },
    verify,
};
use crate::{Compilation, CompileRequest, Compiler, DiagnosticCode, TargetName};
use std::num::NonZeroU32;

/// 测试用目标 oracle：与 driver 注入的同一 x86_64 实现。
const ORACLE: crate::backend::x64::oracle::X64Oracle =
    crate::backend::x64::oracle::X64Oracle::new(crate::target::CpuBaseline::X86_64V1);

#[test]
fn strength_reduction_constants_dominate_machine_order() {
    let compilation = compile(include_str!("fixtures/constant_dominance.gg"));
    let body = named(&compilation, "scale");
    let unchanged = Editor::new(body.clone()).unchanged();
    assert_eq!(&unchanged, body, "无变化路径必须保留全部 arena 与元数据");
    assert_eq!(unchanged.fingerprint(), body.fingerprint());
    let rewritten = Editor::new(body.clone()).finish().expect("重建合法 SSA");
    for input in [0, 1, 17, 63] {
        assert_eq!(interpret(body, &[input]), vec![input * 14]);
        assert_eq!(interpret(&rewritten, &[input]), vec![input * 14]);
    }
}

#[test]
fn poll_linear_leaf_matches_generic_and_rejects_calls_and_loops() {
    let compilation =
        compile("#[used] fn pure() int { 7 }\n#[used] fn spin() { loop {} }\nfn main() { }");
    let body = named(&compilation, "pure");
    assert!(poll::has_linear_leaf_shape(body));
    assert_eq!(
        poll::body_flow(body, &ORACLE),
        poll::generic_body_flow(body, &ORACLE)
    );
    let mut called = body.clone();
    called.instructions[0].op = Op::Call(body::Call {
        target: body::CallTarget::Runtime(body::RuntimeCall::Yield),
        kind: crate::frontend::gir::body::CallKind::Managed,
        parameters: Vec::new(),
        results: Vec::new(),
        may_unwind: false,
        may_suspend: true,
        may_allocate: false,
        captures_arguments: false,
        by_value: Vec::new(),
        sret: None,
        poll_free_leaf: false,
    });
    assert!(!poll::has_linear_leaf_shape(&called));
    assert!(!poll::has_linear_leaf_shape(named(&compilation, "spin")));
}
const SSA: &str = include_str!("fixtures/ssa.gg");
const CONCRETE: &str = include_str!("fixtures/concrete.gg");
const EFFECTS: &str = include_str!("fixtures/effects.gg");
const POLL: &str = include_str!("fixtures/poll.gg");
const OPTIMIZE: &str = include_str!("fixtures/optimize.gg");
const PUBLISH: &str = include_str!("fixtures/publish.gg");
const STACKMAP: &str = include_str!("fixtures/stackmap.gg");
/// 资源样例：ResourceCell 的构造、按值转移与结束时释放。
const RESOURCE: &str = "struct ResourceCell { id: uint }\nfn main() {\n let a = ResourceCell { id: 1 }\n let b = a\n _ = b\n}";
/// 切片是胖指针（数据指针 + 长度）：数据指针是 internal 类根，必须随槽一起登记。
///
/// 该槽若不上报，nursery 回收后切片会指向被复用的内存（实测：`window[0]` 变成后续迭代写入的值）。
/// 这里直接从编译产物断言槽与根的成立，定位到「没物化成栈槽」还是「没发根」。
#[test]
fn slice_values_register_their_data_pointer_root() {
    let compilation = compile(
        "fn total(block: [int; 512]) int { let sum = 0\n let index = 0\n while index < 512 { sum = sum + block[index]\n index = index + 1 }\n sum }\nfn make(n: int) fn() &[int] { let block: [int; 512] = [n; 512]\n fn() &[int] = block[0..512] }\nfn churn(n: int) fn(int) int { let block: [int; 512] = [n; 512]\n fn(x: int) int = total(block) + x }\nfn main() { let maker = make(41)\n let view = maker()\n let index = 0\n while index < 600 { let f = churn(index)\n if f(1) != index * 512 + 1 { panic(\"churn\") }\n if view[0] != 41 { panic(\"slice-live\") }\n _ = f\n index = index + 1 }\n}",
    );
    let body = named(&compilation, "main");
    // 未取地址的切片 local 按标量车道拆开，根可以以「槽根」或「值根」形式出现。
    // 关键断言：**分配（CallReturn）安全点**上必须仍然登记它——回收正是发生在那里；
    // 只在别处登记不足以让切片跨过 nursery 回收。
    // 先验证调用点确实建立栈图记录：被误判为 poll-free leaf 的调用会让调用者的
    // managed 指针不落栈，从而在镜像里丢掉这条根。
    let mut managed_calls = 0_usize;
    for instruction in &body.instructions {
        if let crate::lir::body::Op::Call(call) | crate::lir::body::Op::ForeignCall(call) =
            &instruction.op
        {
            managed_calls += 1;
            assert!(!call.poll_free_leaf, "会分配的调用不得被标为 poll-free 叶");
        }
    }
    // 终结符形式的调用（`Invoke`）同样建立栈图记录，同样不得被标为 poll-free 叶。
    for block in &body.blocks {
        if let crate::lir::body::Terminator::Invoke { call, .. } = &block.terminator {
            managed_calls += 1;
            assert!(
                !call.poll_free_leaf,
                "会分配的 invoke 不得被标为 poll-free 叶"
            );
        }
    }
    assert!(managed_calls > 0, "夹具必须有调用点");
    let safepoints = crate::lir::stackmap::anchors(body).expect("安全点");
    let allocation_points: Vec<_> = safepoints
        .iter()
        .filter(|it| it.kind == crate::lir::stackmap::KIND_CALL_RETURN)
        .collect();
    assert!(
        !allocation_points.is_empty(),
        "夹具必须产生分配/调用返回安全点"
    );
    // 循环的调用环上有两个调用返回点（分配闭包与调用它），切片在**两处**都必须有 internal 根：
    // 回收就发生在这些点上。只在定义点（`maker()` 调用）登记是不够的——那样循环里的回收
    // 会把捕获对象搬走而 `view` 不更新，`view[0]` 随即读到复用后的内存。
    // 未取地址的切片会退化成一条块参数链，而块参数必须留在 `LiveValues` 的 live-in 里、
    // 再由边实参替换，否则跨块携带的值在前驱块里凭空消失（见 `liveness.rs`）。
    let rooted = allocation_points
        .iter()
        .filter(|it| !it.roots.interior.is_empty())
        .count();
    assert!(
        rooted >= 2,
        "循环的两个调用返回点都必须登记切片的 internal 根：带根点数={rooted}，分配点数={}",
        allocation_points.len()
    );
}

/// 活跃位图的边界：末字不得覆盖不存在的槽。
#[test]
fn live_set_masks_respect_slot_count() {
    for slots in [0usize, 1, 63, 64, 65, 128] {
        let full = crate::lir::liveness::LiveSet::full(slots);
        for slot in 0..slots {
            assert!(full.is_live(slot), "{slots} 槽时槽 {slot} 必须活跃");
        }
        assert!(!full.is_live(slots), "{slots} 槽时越界槽不得活跃");
        for slot in 0..slots {
            let mut set = crate::lir::liveness::LiveSet::full(slots);
            set.set(slot, false);
            assert!(!set.is_live(slot), "清位必须生效");
            set.set(slot, true);
            assert!(set.is_live(slot), "置位必须生效");
        }
    }
}

/// 安全点必须跳过已死的栈槽根：登记集合严格小于该 body 的全部带根槽。
///
/// `saw_roots` 先失败的意义在于：夹具一旦不再产生栈槽根，这条断言就会退化成恒真，
/// 必须让覆盖缺口自己暴露，而不是静默通过。
#[test]
fn safepoints_skip_dead_stack_slots() {
    let compilation = compile(STACKMAP);
    let bodies = compilation.lir.as_ref().expect("LIR").bodies();
    let mut saw_roots = false;
    let mut skipped = false;
    for body in bodies {
        let rooted: std::collections::BTreeSet<u32> = body
            .stack_slots
            .iter()
            .enumerate()
            .filter(|(_, slot)| !slot.roots.is_empty())
            .map(|(index, _)| index as u32)
            .collect();
        if rooted.is_empty() {
            continue;
        }
        saw_roots = true;
        for safepoint in crate::lir::stackmap::anchors(body).expect("安全点") {
            let roots = &safepoint.roots;
            let registered: std::collections::BTreeSet<u32> = roots
                .direct
                .iter()
                .chain(&roots.interior)
                .chain(&roots.handle)
                .chain(&roots.compressed)
                .chain(&roots.stack)
                .filter_map(|root| match root {
                    crate::lir::stackmap::LogicalRoot::Slot { slot, .. } => Some(*slot),
                    _ => None,
                })
                .collect();
            if registered.len() < rooted.len() {
                skipped = true;
            }
        }
    }
    assert!(saw_roots, "夹具必须至少有一个带根栈槽，否则断言无意义");
    assert!(skipped, "必须存在跳过死槽根的安全点");
}

/// 逻辑 schema 变化必须拒绝旧值：缓存或旧产物里的世界不得静默进入编码。
#[test]
fn stackmap_world_rejects_stale_schema() {
    let stale = crate::lir::stackmap::LogicalStackMap {
        schema: crate::lir::stackmap::STACKMAP_SCHEMA - 1,
        functions: Vec::new(),
        safepoints: Vec::new(),
        map_count: 0,
        fingerprint: [0; 32],
    };
    let error = crate::lir::stackmap::verify_world(&stale).expect_err("旧 schema 必须被拒绝");
    assert_eq!(
        error.message(),
        "逻辑栈图 schema 不匹配",
        "拒绝必须来自 schema 检查而不是别的结构错误"
    );
}

/// 借出地址的槽必须在调用点登记为槽根：被调方执行期间调用方帧仍按该点的 map 扫描，而借出的
/// 槽没有活跃值、只有活跃地址，唯一的登记来源就是调用帧自己的槽根表。
#[test]
fn borrowed_slot_roots_are_registered_at_call_sites() {
    let compilation = compile(
        "enum Slot { Scalar(int, int), Callable(fn() int) }\nfn touch(pointer: &Slot) fn() int {\n let captured = 1\n return fn() int { return captured }\n}\nfn main() {\n let value = Slot::Callable(fn() int { return 1 })\n let view = &value\n _ = touch(view)\n}",
    );
    let body = named(&compilation, "main");
    let rooted: std::collections::BTreeSet<u32> = body
        .stack_slots
        .iter()
        .enumerate()
        .filter(|(_, slot)| !slot.roots.is_empty())
        .map(|(index, _)| index as u32)
        .collect();
    assert!(
        !rooted.is_empty(),
        "夹具必须产生带根的槽：{:?}",
        body.stack_slots
    );
    let mut registered = std::collections::BTreeSet::new();
    for safepoint in crate::lir::stackmap::anchors(body).expect("安全点") {
        let roots = &safepoint.roots;
        registered.extend(
            roots
                .direct
                .iter()
                .chain(&roots.interior)
                .chain(&roots.handle)
                .chain(&roots.compressed)
                .chain(&roots.stack)
                .filter_map(|root| match root {
                    crate::lir::stackmap::LogicalRoot::Slot { slot, .. } => Some(*slot),
                    _ => None,
                }),
        );
    }
    assert!(
        rooted.is_subset(&registered),
        "借出地址的带根槽必须在安全点上登记：rooted={rooted:?} registered={registered:?}"
    );
}

/// 静态访问必须带惰性初始化守卫：模块 static 与函数局部 static 都在首次读取时发布值。
///
/// 三段由小到大：只有模块 static、只有局部 static、两者同时出现在被调用两次的函数里。
///
/// 程序规模递增，逐个成测：一次整体编译约 1 秒，三段合测会超出单项耗时上限。
fn static_access_compiles(source: &str) {
    let compilation = Compiler::shared().compile(CompileRequest::single_file(
        "main.gg",
        source,
        TargetName::X86_64Linux,
    ));
    assert!(
        compilation.is_success(),
        "程序失败：{:?}",
        compilation.diagnostics().items()
    );
}

#[test]
fn static_access_compiles_with_only_module_static() {
    static_access_compiles("static M: int = 41\nfn main() { _ = M }");
}

#[test]
fn static_access_compiles_with_only_local_static() {
    static_access_compiles("fn take() int {\n static L: int = 9\n L\n}\nfn main() { _ = take() }");
}

#[test]
fn static_access_compiles_with_module_and_local_static() {
    static_access_compiles(
        "static M: int = 41\nfn take() int {\n static L: int = 9\n L + M\n}\nfn main() {\n if take() != 50 { panic(\"static\") }\n if take() != 50 { panic(\"static-again\") }\n}",
    );
}

/// 初始化器必须在镜像里真实发布：调用点保留成调用，实例末尾 release 置位。
///
/// 这两条一起挡住「内联把发布动作静默抹掉」的回退：只断言能编译会漏掉整类错误。
#[test]
fn static_initializer_call_survives_and_publishes() {
    let compilation = compile("static M: int = 41\nfn main() { _ = M }");
    let initializer = named(&compilation, "M");
    let publishes = initializer
        .instructions
        .iter()
        .filter(|instruction| {
            matches!(
                instruction.op,
                Op::Atomic {
                    op: body::AtomicOp::Store,
                    ordering: crate::frontend::gir::body::MemoryOrdering::Release,
                    ..
                }
            )
        })
        .count();
    assert!(publishes > 0, "初始化器必须 release 发布 initialized 字");

    let entry = named(&compilation, "main");
    let called = entry.blocks.iter().any(|block| {
        matches!(
            &block.terminator,
            Terminator::Invoke { call, .. }
                if call.target == body::CallTarget::Instance(initializer.instance)
        )
    });
    assert!(called, "读点守卫必须真实调用初始化器实例");
}

fn compile(source: &str) -> Compilation {
    let compilation = Compiler::shared().compile(CompileRequest::single_file(
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
fn named<'a>(compilation: &'a Compilation, name: &str) -> &'a Body {
    let module = compilation.hir.as_ref().unwrap().module();
    let key = module
        .definitions
        .iter()
        .find(|definition| definition.name == name)
        .unwrap()
        .key;
    compilation
        .lir
        .as_ref()
        .unwrap()
        .bodies()
        .iter()
        .find(|body| body.owner == key)
        .unwrap()
}
fn rejected(compilation: &Compilation, mut body: Body) {
    crate::lir::uses::rebuild(&mut body);
    let error = verify::verify(&body, compilation.hir.as_ref().unwrap().module(), &ORACLE)
        .expect_err("非法 LIR 必须在后端前失败");
    assert_eq!(error.code(), DiagnosticCode::LirInvariant);
}

/// 断言非法 LIR 在 runtime raw 平面契约上被拒绝。
fn rejected_raw(compilation: &Compilation, mut body: Body) {
    crate::lir::uses::rebuild(&mut body);
    let error = verify::verify(&body, compilation.hir.as_ref().unwrap().module(), &ORACLE)
        .expect_err("非法 publish 区域必须在后端前失败");
    assert_eq!(
        error.code(),
        DiagnosticCode::RuntimeRawInvariant,
        "{}",
        error.message()
    );
}

fn publish_body(compilation: &Compilation) -> Body {
    compilation
        .lir
        .as_ref()
        .expect("已生成 LIR")
        .bodies()
        .iter()
        .find(|body| !body.no_safepoint_regions.is_empty())
        .expect("publish 闭包必须保留 NoSafepointRegion")
        .clone()
}

/// 返回 region 内第一条 `Store` 的指令下标。
fn region_store(body: &Body, region: u32) -> usize {
    let mut inside = false;
    for (index, instruction) in body.instructions.iter().enumerate() {
        match instruction.op {
            Op::NoSafepointBegin(open) if open == region => inside = true,
            Op::NoSafepointEnd(close) if close == region => inside = false,
            Op::Store(_) if inside => return index,
            _ => {}
        }
    }
    panic!("publish 区域必须包含 Store");
}

/// 返回资源样例中承载资源调用的 body。
fn resource_body(compilation: &Compilation) -> Body {
    compilation
        .lir
        .as_ref()
        .expect("已生成 LIR")
        .bodies()
        .iter()
        .find(|body| {
            crate::lir::verify::resource_isolation::resource_descriptors(std::slice::from_ref(
                *body,
            ))
            .is_ok_and(|descriptors| !descriptors.is_empty())
        })
        .expect("资源样例必须保留 ResourceAcquire/Release 调用")
        .clone()
}

#[test]
fn panic_invoke_carries_original_source_location() {
    let compilation = compile("fn main() {\n    panic(\"boom\")\n}");
    let body = named(&compilation, "main");
    let panic = body
        .blocks
        .iter()
        .find_map(|block| {
            let Terminator::Invoke {
                call, arguments, ..
            } = &block.terminator
            else {
                return None;
            };
            matches!(
                call.target,
                body::CallTarget::Runtime(body::RuntimeCall::Panic)
            )
            .then_some((call, body.args(arguments)))
        })
        .expect("panic 必须调用 runtime glue");
    let (call, arguments) = panic;
    assert_eq!(call.parameters.len(), 6);
    assert_eq!(call.results, [ValueType::scalar(Type::I64)]);
    assert!(body.data.iter().any(|data| data.bytes == b"main.gg"));
    let constants: Vec<_> = arguments[3..]
        .iter()
        .filter_map(|value| {
            let body::Definition::Instruction { instruction, .. } =
                body.values[value.index()].definition
            else {
                return None;
            };
            match body.instructions[instruction.index()].op {
                Op::IConst(value) => Some(value),
                _ => None,
            }
        })
        .collect();
    assert_eq!(constants, [7, 2, 5]);
}

/// spawn 第四参（结果字里根的 lane map 编码）必须由**结果载荷类型**的根车道决定。
///
/// 结果字在 ABI 上是一个字：标量结果内联，含根车道的结果是指向载荷对象的引用。分类必须复用
/// 帧存储那套 `Builder::roots`，并且要用 `n + 1` 编码——Direct 的判别值是 0，直接写判别值会
/// 与「不含根」混淆。
#[test]
fn spawn_result_kind_matches_the_payload_roots() {
    let scalar =
        compile("fn child() int { 42 }\nfn main() {\n let handle = async child()\n _ = handle\n}");
    let managed = compile(
        "fn child() fn(int) int { fn(x: int) int = x + 1 }\nfn main() {\n let handle = async child()\n _ = handle\n}",
    );
    assert_eq!(spawn_result_kind(&scalar), 0, "标量结果字不含根");
    assert_eq!(spawn_result_kind(&managed), 1, "闭包载荷的根车道是 Direct");
}

/// 取 `main` 里 spawn 调用的第四参常量。
fn spawn_result_kind(compilation: &Compilation) -> u64 {
    let body = named(compilation, "main");
    for instruction in &body.instructions {
        let Op::Call(call) = &instruction.op else {
            continue;
        };
        if !matches!(
            call.target,
            body::CallTarget::Runtime(body::RuntimeCall::Spawn)
        ) {
            continue;
        }
        let arguments = body.args(&instruction.arguments);
        let operand = *arguments.last().expect("spawn 至少四参");
        let body::Definition::Instruction { instruction, .. } =
            body.values[operand.index()].definition
        else {
            panic!("spawn 第四参必须是常量");
        };
        let Op::IConst(value) =
            body.instructions[usize::try_from(instruction.0).expect("指令编号")].op
        else {
            panic!("spawn 第四参必须是常量");
        };
        return value;
    }
    panic!("没有 spawn 调用");
}

#[test]
fn panic_integer_report_buffer_avoids_value_repeat_runtime_call() {
    let compilation = compile("fn main() {\n    panic(\"boom\")\n}");
    let body = named(&compilation, "write_uint");
    assert!(!body.instructions.iter().any(|instruction| matches!(
        &instruction.op,
        Op::Call(call)
            if matches!(
                call.target,
                body::CallTarget::Runtime(body::RuntimeCall::ValueRepeat)
            )
    )));
}

#[test]
fn block_parameters_preserve_branch_and_loop_values() {
    let compilation = compile(SSA);
    let body = named(&compilation, "choose");
    assert_eq!(interpret(body, &[1, 10, 4]), vec![17]);
    assert_eq!(interpret(body, &[0, 10, 0]), vec![12]);
    assert_eq!(interpret(body, &[0, 10, 3]), vec![15]);
}

#[test]
fn concrete_instances_preserve_scalar_and_wide_returns() {
    let compilation = compile(CONCRETE);
    let module = compilation.hir.as_ref().unwrap().module();
    let identity = module
        .definitions
        .iter()
        .find(|definition| definition.name == "identity")
        .unwrap()
        .key;
    let bodies: Vec<_> = compilation
        .lir
        .as_ref()
        .unwrap()
        .bodies()
        .iter()
        .filter(|body| body.owner == identity)
        .collect();
    let narrow = bodies
        .iter()
        .find(|body| body.signature.parameters == [ValueType::scalar(Type::I64)])
        .unwrap();
    let wide = bodies
        .iter()
        .find(|body| body.signature.parameters == [ValueType::scalar(Type::I64); 2])
        .unwrap();
    assert_eq!(interpret(narrow, &[17]), vec![17]);
    assert_eq!(interpret(wide, &[7, 9]), vec![7, 9]);
    assert!(bodies.iter().any(|body| body.signature.sret.is_some()));
}

#[test]
fn memory_chain_cannot_skip_a_volatile_effect() {
    let compilation = compile(EFFECTS);
    let mut body = named(&compilation, "main").clone();
    let block = body
        .blocks
        .iter()
        .find(|block| {
            body.instructions[range(&block.instructions)]
                .iter()
                .any(|instruction| matches!(&instruction.op, Op::Store(access) if access.volatile))
        })
        .unwrap();
    let entry = body.parameters[usize::try_from(block.parameters.start).unwrap()].value;
    let store = body.instructions[range(&block.instructions)]
        .iter()
        .position(|instruction| matches!(&instruction.op, Op::Store(access) if access.volatile))
        .unwrap()
        + usize::try_from(block.instructions.start).unwrap();
    assert_ne!(body.instructions[store].memory.unwrap().input, entry);
    body.instructions[store].memory.as_mut().unwrap().input = entry;
    rejected(&compilation, body);
}

#[test]
fn branch_values_cannot_be_used_on_the_other_edge() {
    let compilation = compile(SSA);
    let mut body = named(&compilation, "choose").clone();
    let additions: Vec<_> = body
        .instructions
        .iter()
        .enumerate()
        .filter(|(_, instruction)| matches!(instruction.op, Op::Integer(body::IntOp::Add)))
        .map(|(index, instruction)| (index, ValueId(instruction.results.start)))
        .collect();
    let left = additions[0].1;
    let right = additions[1].1;
    let operand = body
        .edges
        .iter()
        .flat_map(|edge| range(&edge.arguments))
        .find(|&index| body.operands[index] == left)
        .unwrap();
    body.operands[operand] = right;
    rejected(&compilation, body);
}

#[test]
fn integer_pointer_cast_cannot_forge_a_managed_root() {
    let compilation = compile("fn main() { let value = 1\n let pointer = &value\n _ = *pointer }");
    let mut body = named(&compilation, "main").clone();
    let instruction = body
        .instructions
        .iter()
        .position(|instruction| matches!(instruction.op, Op::PtrOffset | Op::StackAddr(_)))
        .unwrap();
    let result = ValueId(body.instructions[instruction].results.start);
    body.values[result.index()].kind.provenance = Some(Provenance::Code);
    rejected(&compilation, body);
}

#[test]
fn atomic_load_rejects_release_order() {
    let compilation = compile(EFFECTS);
    let mut body = named(&compilation, "main").clone();
    let load = body
        .instructions
        .iter_mut()
        .find(|instruction| matches!(instruction.op, Op::Load(_)))
        .unwrap();
    load.op = Op::Atomic {
        op: body::AtomicOp::Load,
        ordering: crate::frontend::gir::body::MemoryOrdering::Release,
        failure: None,
        align: 8,
    };
    rejected(&compilation, body);
}

#[test]
fn managed_store_requires_its_hybrid_barrier() {
    let compilation = compile(
        "fn main() { let value = 1\n let closure = fn() int { return value }\n _ = closure() }",
    );
    let mut body = named(&compilation, "main").clone();
    let barrier = body
        .instructions
        .iter_mut()
        .find(|instruction| matches!(instruction.op, Op::GcWriteBarrier { .. }))
        .unwrap();
    barrier.op = Op::StackCheck;
    barrier.arguments.end = barrier.arguments.start;
    let safepoint = barrier.safepoint.unwrap();
    body.safepoints[safepoint.index()].kind = body::SafepointKind::StackCheck;
    rejected(&compilation, body);
}

#[test]
fn bare_barrier_sites_have_mandatory_call_return_records() {
    // 裸屏障没有 permit：slow path 会 shade，返回 PC 必须在栈图里能找到根；预留屏障由
    // `BarrierReserve` 额度保证不进入 slow path，且只出现在 no-safepoint region 内，不留记录。
    let compilation = compile(
        "fn main() {\n let value = 1\n let closure = fn() int { return value }\n _ = closure()\n}",
    );
    let body = named(&compilation, "main").clone();
    let barrier = body
        .instructions
        .iter()
        .position(|instruction| matches!(instruction.op, Op::GcWriteBarrier { .. }))
        .expect("闭包捕获写入必须带裸屏障");
    let module = compilation.hir.as_ref().expect("已生成 HIR").module();
    let world = crate::lir::stackmap::derive(std::slice::from_ref(&body), module)
        .expect("栈图推导必须通过 verifier");
    let anchor = world
        .safepoints
        .iter()
        .find(|point| point.instruction == u32::try_from(barrier).expect("指令编号"))
        .expect("裸屏障必须留下安全点记录");
    assert_eq!(anchor.kind, crate::lir::stackmap::KIND_CALL_RETURN);
    assert_eq!(anchor.flags, 0b011);
    assert!(anchor.roots.words() > 0, "屏障返回点的根集不得为空");

    let publish = compile(PUBLISH);
    let publish_body = publish_body(&publish);
    let reserved = publish_body
        .instructions
        .iter()
        .position(|instruction| matches!(instruction.op, Op::GcWriteBarrierReserved { .. }))
        .expect("publish 区域必须带预留屏障");
    let module = publish.hir.as_ref().expect("已生成 HIR").module();
    let world = crate::lir::stackmap::derive(std::slice::from_ref(&publish_body), module)
        .expect("栈图推导必须通过 verifier");
    assert!(
        world
            .safepoints
            .iter()
            .all(|point| point.instruction != u32::try_from(reserved).expect("指令编号")),
        "预留屏障在 no-safepoint region 内，不得留下记录"
    );
}

/// 分配 op（`GcAlloc`/`RegionAlloc`/`PromoteManaged`）的返回 PC 必须是强制 `CallReturn`：
/// 同一站点发射的 slow call 可以启动回收，回收要在该返回 PC 上按本帧 map 扫描根
/// （计划第 2.4 条）。改动前这些站点被当作叶操作，不留下任何记录。
#[test]
fn allocation_sites_have_mandatory_call_return_records() {
    // 逃逸闭包的环境落受管堆：这是仓库里已知会产生分配的程序形态。
    let compilation = compile(
        "fn make() fn() int {\n let value = 1\n return fn() int { return value }\n}\nfn main() {\n let closure = make()\n _ = closure()\n}",
    );
    let body = named(&compilation, "make").clone();
    let allocation = body
        .instructions
        .iter()
        .position(|instruction| matches!(instruction.op, Op::GcAlloc { .. }))
        .expect("闭包字面量的环境必须在创建处分配");
    let module = compilation.hir.as_ref().expect("已生成 HIR").module();
    let world = crate::lir::stackmap::derive(std::slice::from_ref(&body), module)
        .expect("栈图推导必须通过 verifier");
    let anchor = world
        .safepoints
        .iter()
        .find(|point| point.instruction == u32::try_from(allocation).expect("指令编号"))
        .expect("分配站点必须留下强制调用返回记录");
    assert_eq!(anchor.kind, crate::lir::stackmap::KIND_CALL_RETURN);
    assert_eq!(anchor.flags, 0b011);
    assert!(
        world
            .functions
            .iter()
            .any(|function| function.has_alloc_slow),
        "分配站点必须计入 has_alloc_slow"
    );
}

/// 断言世界里每条记录都覆盖该点的跨点活跃受管值。
///
/// 锁住的是接线：`LiveValues`（SSA 逆向活跃）与 `push_live_value_roots` 缺一不可。跨点存活
/// 但既不是本条指令操作数、也不在 LIR 栈槽的受管值，其物理落点是分配器生成的 spill 槽，
/// 而该槽是否被收集器扫描只能由这条路径提供登记。
fn assert_records_root_live_values(compilation: &Compilation, fixture: &str) {
    let lir = compilation.lir.as_ref().expect("已生成 LIR");
    let module = compilation.hir.as_ref().expect("已生成 HIR").module();
    let world =
        crate::lir::stackmap::derive(lir.bodies(), module).expect("栈图推导必须通过 verifier");
    let mut covered = 0_usize;
    for function in &world.functions {
        let Some(body) = lir
            .bodies()
            .iter()
            .find(|body| body.instance == function.instance)
        else {
            continue;
        };
        let graph = crate::lir::pass::graph::Graph::new(body).expect("CFG 必须成立");
        let values = crate::lir::liveness::LiveValues::new(body, &graph);
        let start = usize::try_from(function.safepoint_start).expect("起始下标适配宿主");
        let end = start + usize::try_from(function.safepoint_count).expect("数量适配宿主");
        for point in &world.safepoints[start..end] {
            // 入口检查的根来自 ABI 参数表（寄存器映射），不是值活跃集。
            if point.kind == crate::lir::stackmap::KIND_MORESTACK_ENTRY {
                continue;
            }
            let block = body::BlockId(point.block);
            let instructions = &body.blocks[block.index()].instructions;
            let after = values.after(block, point.instruction - instructions.start);
            for (index, value) in body.values.iter().enumerate() {
                if !after.is_live(index) || value.kind.ty != Type::Ptr {
                    continue;
                }
                if value
                    .kind
                    .provenance
                    .and_then(|it| it.root_kind())
                    .is_none()
                {
                    continue;
                }
                let rooted = [
                    &point.roots.direct,
                    &point.roots.interior,
                    &point.roots.handle,
                    &point.roots.compressed,
                    &point.roots.stack,
                ]
                .iter()
                .any(|group| {
                    group.iter().any(|root| {
                        matches!(
                            root,
                            crate::lir::stackmap::LogicalRoot::Value { value, .. } if *value == id(index)
                        )
                    })
                });
                assert!(
                    rooted,
                    "{fixture}: kind {} 的安全点漏掉跨点活跃值 v{index}",
                    point.kind
                );
                covered += 1;
            }
            // 负向：点位自身的定义（分配结果、调用结果）不得登记成根——安全点在该指令执行
            // 期间触发，那个值还不存在；登记它会让收集器读到一个尚未写入的槽位。
            for group in [
                &point.roots.direct,
                &point.roots.interior,
                &point.roots.handle,
                &point.roots.compressed,
                &point.roots.stack,
            ] {
                for root in group {
                    let crate::lir::stackmap::LogicalRoot::Value { value, .. } = root else {
                        continue;
                    };
                    let definition =
                        body.values[usize::try_from(*value).expect("值下标适配宿主")].definition;
                    let (def_block, def_position) = match definition {
                        Definition::Parameter { block, .. } => (block, 0),
                        Definition::Instruction { instruction, .. } => {
                            let def_block = graph.instruction_blocks[instruction.index()];
                            (
                                def_block,
                                instruction.0 - body.blocks[def_block.index()].instructions.start,
                            )
                        }
                        Definition::Invoke { block, .. } => {
                            let def = &body.blocks[block.index()].instructions;
                            (block, def.end - def.start)
                        }
                    };
                    assert!(
                        def_block != block || def_position < point.instruction - instructions.start,
                        "{fixture}: kind {} 的安全点把点位自身的定义 v{value} 登记成了根",
                        point.kind
                    );
                }
            }
        }
    }
    assert!(
        covered > 0,
        "{fixture}: 夹具没有覆盖任何跨点活跃受管值，断言会空转"
    );
}

/// 异常边（Invoke 的 unwind 目标）依赖同一张调用返回 map：unwind 发生时长跳转到落地垫，
/// 落地垫按该点的 map 扫描调用方帧。因此带 unwind 边的 Invoke 必须留下调用返回记录。
#[test]
fn unwind_edges_share_the_invoke_call_return_map() {
    let compilation = compile(
        "fn bail() int {\n panic(\"boom\")\n}\nfn main() {\n let value = 1\n let closure = fn() int { return value }\n if value != 0 { _ = bail() }\n _ = closure()\n}",
    );
    let body = named(&compilation, "main").clone();
    let invoke_block = body
        .blocks
        .iter()
        .position(|block| matches!(block.terminator, Terminator::Invoke { .. }))
        .expect("panic 必须落在 Invoke 终结符上");
    let block = &body.blocks[invoke_block];
    let Terminator::Invoke { call, .. } = &block.terminator else {
        unreachable!("已按 Invoke 定位");
    };
    let invoke_id = u32::try_from(invoke_block).expect("块编号适配 u32");
    assert!(
        body.edges
            .iter()
            .any(|edge| edge.unwind && edge.from == body::BlockId(invoke_id)),
        "可能 unwind 的调用必须带 unwind 边"
    );
    assert!(
        !call.poll_free_leaf,
        "会 unwind 的调用不得被标为 poll-free 叶"
    );
    let module = compilation.hir.as_ref().expect("已生成 HIR").module();
    let world = crate::lir::stackmap::derive(std::slice::from_ref(&body), module)
        .expect("栈图推导必须通过 verifier");
    let anchor = world
        .safepoints
        .iter()
        .find(|point| point.instruction == block.instructions.end && point.block == invoke_id)
        .expect("Invoke 必须留下调用返回记录");
    assert_eq!(anchor.kind, crate::lir::stackmap::KIND_CALL_RETURN);
}

/// 跨点存活的受管值必须逐点在根集合里：这是物理层把分配器 spill 槽标成根的唯一来源。
///
/// 每个 fixture 单独成测：一次整体编译约 1 秒，合测会超出单项耗时上限。
#[test]
fn safepoints_root_every_live_value_in_their_world_closure() {
    let source =
        "fn main() {\n let value = 1\n let closure = fn() int { return value }\n _ = closure()\n}";
    assert_records_root_live_values(&compile(source), "closure");
}

/// 菱形合流：受管值在两个分支上都写入，合流点之后仍被使用，根必须跟着合流参数走。
#[test]
fn safepoints_root_every_live_value_in_their_world_diamond() {
    let source = "fn main() {\n let flag = 1\n let closure = fn() int { return flag }\n let chosen = closure\n if flag > 0 { chosen = closure } else { chosen = closure }\n _ = chosen()\n}";
    assert_records_root_live_values(&compile(source), "diamond");
}

#[test]
fn safepoints_root_every_live_value_in_their_world_poll() {
    assert_records_root_live_values(&compile(POLL), "poll");
}

#[test]
fn safepoints_root_every_live_value_in_their_world_stackmap() {
    assert_records_root_live_values(&compile(STACKMAP), "stackmap");
}

#[test]
fn publish_region_keeps_raw_plane_contract() {
    let compilation = compile(PUBLISH);
    let body = publish_body(&compilation);
    assert!(
        body.no_safepoint_regions.iter().any(|reason| matches!(
            reason,
            crate::frontend::gir::body::NoSafepointReason::OwnershipPublish
                | crate::frontend::gir::body::NoSafepointReason::RootPublish
        )),
        "publish 区域必须保留 reason"
    );
    let store = region_store(&body, 0);
    assert!(matches!(body.instructions[store].op, Op::Store(_)));
}

#[test]
fn publish_region_rejects_managed_interior_value() {
    use crate::lir::body::Provenance;

    let compilation = compile(PUBLISH);
    let mut body = publish_body(&compilation);
    let store = region_store(&body, 0);
    let value = body.args(&body.instructions[store].arguments)[1];
    assert_eq!(
        body.values[value.index()].kind.provenance,
        Some(Provenance::GcHeap),
        "fixture 的 publish 区域写入的是句柄身份"
    );
    body.values[value.index()].kind.provenance = Some(Provenance::GcInterior);
    rejected_raw(&compilation, body);
}

#[test]
fn publish_region_rejects_bulk_memory_operation() {
    let compilation = compile(PUBLISH);
    let mut body = publish_body(&compilation);
    let store = region_store(&body, 0);
    let arguments = body.instructions[store].arguments.clone();
    let byte = body
        .instructions
        .iter()
        .find_map(|instruction| match instruction.op {
            Op::IConst(_) if !instruction.results.is_empty() => {
                Some(ValueId(instruction.results.start))
            }
            _ => None,
        })
        .expect("fixture 必须含整数常量");
    body.operands[usize::try_from(arguments.start).expect("操作数起点")] = byte;
    body.instructions[store].op = Op::Memset { bytes: 8 };
    rejected_raw(&compilation, body);
}

#[test]
fn resource_descriptor_cannot_be_region_allocated() {
    let compilation = compile(RESOURCE);
    let mut body = resource_body(&compilation);
    let descriptors =
        crate::lir::verify::resource_isolation::resource_descriptors(std::slice::from_ref(&body))
            .expect("资源调用必须登记类型描述符");
    let descriptor = *descriptors.iter().next().expect("资源描述符集合非空");
    // 常量指令既不是资源调用也不是描述符定义，替换它不会破坏描述符反查。
    let index = body
        .instructions
        .iter()
        .position(|instruction| matches!(instruction.op, Op::IConst(_)))
        .expect("资源样例必须保留整数常量");
    body.instructions[index].op = Op::RegionAlloc {
        region: 0,
        descriptor,
        align: 8,
    };
    let error = crate::lir::verify::resource_isolation::verify(std::slice::from_ref(&body))
        .expect_err("资源描述符不能进入 RegionAlloc");
    assert_eq!(
        error.code(),
        DiagnosticCode::ResourceInvariant,
        "{}",
        error.message()
    );
}

#[test]
fn resource_descriptors_are_derived_from_bodies() {
    let compilation = compile(RESOURCE);
    let body = resource_body(&compilation);
    let bodies = std::slice::from_ref(&body);
    let first = crate::lir::verify::resource_isolation::resource_descriptors(bodies)
        .expect("资源调用必须登记类型描述符");
    let second = crate::lir::verify::resource_isolation::resource_descriptors(bodies)
        .expect("资源调用必须登记类型描述符");
    assert_eq!(first, second, "同一 body 的描述符扫描必须确定");
    assert!(!first.is_empty(), "资源样例必须产生资源描述符");
    assert!(
        crate::lir::verify::resource_isolation::verify(bodies).is_ok(),
        "未进入 region 的资源样例必须通过隔离闸门"
    );
}

#[test]
fn barrier_reserve_materializes_permits() {
    use crate::frontend::gir::body::NoSafepointReason::{OwnershipPublish, RootPublish};
    use crate::runtime::barrier_schema::EDGE_DELTAS_PER_WRITE;

    let compilation = compile(PUBLISH);
    let body = compilation
        .lir
        .as_ref()
        .expect("已生成 LIR")
        .bodies()
        .iter()
        .find(|body| !body.no_safepoint_regions.is_empty())
        .expect("publish 闭包必须保留 NoSafepointRegion");
    assert_eq!(body.no_safepoint_regions, [OwnershipPublish, RootPublish]);
    assert_eq!(body.barrier_permits.len(), 2);
    for (index, permit) in body.barrier_permits.iter().enumerate() {
        assert_eq!(permit.region, body::id(index));
        // 每个 publish region 只包住一条句柄 Assign：一个 store、一个写入地址。
        assert_eq!(permit.max_shades, 2);
        assert_eq!(permit.max_card_marks, 1);
        // shade 额度同时是 edge scratch 的容量证明：每条写入最多贡献
        // `EDGE_DELTAS_PER_WRITE` 条边变更，两者上界同源，因此不需要第二份额度字段。
        assert_eq!(
            EDGE_DELTAS_PER_WRITE,
            crate::runtime::barrier_schema::SHADE_SLOTS_PER_WRITE
        );
        assert!(permit.max_shades >= EDGE_DELTAS_PER_WRITE);
    }
    let mut reserves = [0; 2];
    let mut barriers = [0; 2];
    for instruction in &body.instructions {
        match instruction.op {
            Op::BarrierReserve(permit) => reserves[permit.index()] += 1,
            Op::GcWriteBarrierReserved { permit, .. } => barriers[permit.index()] += 1,
            Op::GcWriteBarrier { .. } => panic!("region 内屏障必须已预留"),
            _ => {}
        }
    }
    assert_eq!(reserves, [1, 1]);
    assert_eq!(barriers, [1, 1]);
    assert!(body.blocks.iter().any(|block| {
        body.instructions[range(&block.instructions)]
            .iter()
            .filter(|instruction| matches!(instruction.op, Op::NoSafepointBegin(_)))
            .count()
            == 2
    }));
}

#[test]
fn missing_safepoint_and_unclosed_region_are_rejected() {
    // 带参数的叶函数不会被 GIR 内联；这里用带调用的非叶函数保留该记录。
    let compilation = compile("fn helper(value: int) int { value }\nfn main() { _ = helper(1) }");
    let mut body = named(&compilation, "main").clone();
    let stack_check = body
        .instructions
        .iter()
        .position(|instruction| matches!(instruction.op, Op::StackCheck))
        .expect("入口必须保留 StackCheck");
    body.instructions[stack_check].safepoint = None;
    rejected(&compilation, body);
    let mut body = named(&compilation, "main").clone();
    body.no_safepoint_regions
        .push(crate::frontend::gir::body::NoSafepointReason::RootPublish);
    body.instructions[stack_check].op = Op::NoSafepointBegin(0);
    body.instructions[stack_check].safepoint = None;
    body.safepoints.clear();
    rejected(&compilation, body);
}

#[test]
fn cached_lir_matches_fresh_output_and_action_identity() {
    let compiler = Compiler::new();
    let request = || CompileRequest::single_file("main.gg", SSA, TargetName::X86_64Linux);
    let cold = compiler.compile(request());
    let warm = compiler.compile(request());
    assert!(cold.is_success(), "{:?}", cold.diagnostics().items());
    assert!(warm.is_success(), "{:?}", warm.diagnostics().items());
    assert_eq!(cold.dump_lir(), warm.dump_lir());
    assert_eq!(cold.action_key(), warm.action_key());
    let changed = compiler.compile(CompileRequest::single_file(
        "main.gg",
        SSA.replace("value + 2", "value + 3"),
        TargetName::X86_64Linux,
    ));
    assert!(changed.is_success(), "{:?}", changed.diagnostics().items());
    assert_ne!(cold.lir_fingerprint(), changed.lir_fingerprint());
}

/// 用户可观察效果的多重集合；poll 与纯计算不属于可观察效果。
fn effect_signature(body: &Body) -> Vec<String> {
    body.instructions
        .iter()
        .filter_map(|instruction| match &instruction.op {
            Op::Load(access) if access.volatile => Some("volatile-load".to_owned()),
            Op::Store(access) if access.volatile => Some("volatile-store".to_owned()),
            Op::Atomic { op, .. } => Some(format!("atomic-{op:?}")),
            Op::ForeignCall(_) => Some("foreign-call".to_owned()),
            Op::Call(_) => Some("call".to_owned()),
            Op::TrapIf => Some("trap".to_owned()),
            Op::GcWriteBarrier { .. } | Op::GcWriteBarrierReserved { .. } => {
                Some("barrier".to_owned())
            }
            Op::InlineAsm(_) => Some("asm".to_owned()),
            _ => None,
        })
        .collect()
}

#[test]
fn pipeline_preserves_observable_effects() {
    let compilation = compile(EFFECTS);
    let body = named(&compilation, "main").clone();
    let before = effect_signature(&body);
    assert!(
        before.iter().any(|effect| effect == "volatile-store"),
        "fixture 必须包含可观察效果：{before:?}"
    );
    let mut bodies = vec![body];
    pass::optimize_world(
        &mut bodies,
        compilation.hir.as_ref().unwrap().module(),
        &crate::target::baseline_cost_profile(),
        &ORACLE,
    )
    .expect("固定管线必须通过每个 pass 后的结构 verifier");
    assert_eq!(effect_signature(&bodies[0]), before);
    verify::verify(
        &bodies[0],
        compilation.hir.as_ref().unwrap().module(),
        &ORACLE,
    )
    .expect("优化后的 LIR 必须通过结构 verifier");
}

#[test]
fn algebraic_simplification_preserves_wrapping_and_division() {
    let compilation = compile(
        "fn identity(x: int) int { let a = x + 0\n let b = a - 0\n return b }\nfn divide(y: int) int = y / 2 + y % 2\nfn main() { _ = identity(7)\n _ = divide(9) }",
    );
    let body = named(&compilation, "identity").clone();
    let expected = interpret(&body, &[7]);
    let mut editor = Editor::new(body);
    constants::algebraic_simplify(&mut editor).expect("代数化简");
    let simplified = editor.finish().expect("finish");
    assert_eq!(interpret(&simplified, &[7]), expected);

    let mut editor = Editor::new(named(&compilation, "divide").clone());
    constants::algebraic_simplify(&mut editor).expect("代数化简");
    let divide = editor.finish().expect("finish");
    assert!(
        divide
            .instructions
            .iter()
            .any(|instruction| matches!(instruction.op, Op::Integer(body::IntOp::DivSigned))),
        "有符号除法不得被消除"
    );
    assert!(
        divide
            .instructions
            .iter()
            .any(|instruction| matches!(instruction.op, Op::Integer(body::IntOp::RemSigned))),
        "取余不得被消除"
    );
}

#[test]
fn infinite_loop_gets_budgeted_poll() {
    let compilation = compile(POLL);
    let body = named(&compilation, "spin");
    assert!(
        body.instructions
            .iter()
            .any(|instruction| matches!(instruction.op, Op::SafepointPoll { .. })),
        "无限循环必须被 poll 切断"
    );
    assert!(poll::body_clean_cycle(body, &ORACLE).is_none());
    assert!(!body.poll_summary.has_poll_free_cycle);
}

#[test]
fn counted_loop_strip_mining_has_poll_free_inner_loop() {
    let compilation = compile(POLL);
    let body = named(&compilation, "chunked");
    let polls: Vec<_> = body
        .instructions
        .iter()
        .filter(|instruction| matches!(instruction.op, Op::SafepointPoll { .. }))
        .collect();
    assert_eq!(polls.len(), 1, "内层必须 poll-free，只有外层每次 poll");
    assert_eq!(
        polls[0].op,
        Op::SafepointPoll {
            interval: NonZeroU32::new(1).expect("interval 非零"),
        }
    );
    assert!(poll::body_clean_cycle(body, &ORACLE).is_none());
    assert!(poll::body_poll_free_cost(body, &ORACLE) <= POLL_BUDGET);
}

#[test]
fn small_counted_loop_stays_poll_free() {
    let compilation = compile(
        "fn small() int {\n let i = 0\n let total = 0\n while i < 3 {\n total = total + i\n i = i + 1\n }\n return total\n}\nfn main() { _ = small() }",
    );
    let body = named(&compilation, "small");
    assert!(
        !body
            .instructions
            .iter()
            .any(|instruction| matches!(instruction.op, Op::SafepointPoll { .. })),
        "总成本不超预算的计数循环不得插 poll"
    );
    assert_eq!(interpret(body, &[]), vec![3]);
}

/// 逐根初始化事件只跟随根字：整值写入为每个被覆盖的根字置位，标量槽不产生事件。
#[test]
fn root_init_events_track_pointer_fields_only() {
    let compilation = compile(
        "struct Pair { a: &int, b: &int }\nfn main() {\n let x = 1\n let y = 2\n let first = Pair { a: &x, b: &y }\n let second = first\n let view = &second\n _ = view\n}",
    );
    let module = compilation.hir.as_ref().expect("已生成 HIR").module();
    let body = named(&compilation, "main");
    let tracked = body
        .stack_slots
        .iter()
        .enumerate()
        .find(|(index, slot)| {
            slot.roots.len() == 2
                && body
                    .root_inits
                    .iter()
                    .any(|event| event.slot.index() == *index && event.initialized)
        })
        .map(|(index, _)| index)
        .unwrap_or_else(|| panic!("整值写入必须为双根字槽置位：{:?}", body.root_inits));
    let events: Vec<(bool, u32)> = body
        .root_inits
        .iter()
        .filter(|event| event.slot.index() == tracked)
        .map(|event| (event.initialized, event.bit))
        .collect();
    assert_eq!(
        events.first().copied(),
        Some((false, 0)),
        "槽的第一个事件必须是 StorageLive 复位：{events:?}"
    );
    let set: Vec<u32> = events
        .iter()
        .filter(|(initialized, _)| *initialized)
        .map(|(_, bit)| *bit)
        .collect();
    assert!(
        set.contains(&0) && set.contains(&1),
        "整值写入必须为两个根字置位：{events:?}"
    );
    // 事件的位置必须落在所属块的指令范围内（verifier 与重写 pass 共用同一约束）。
    crate::lir::verify::verify(body, module, &ORACLE).expect("逐根初始化记录必须通过 verifier");
    assert!(
        body.root_inits
            .iter()
            .all(|event| !body.stack_slots[event.slot.index()].roots.is_empty()),
        "事件只能指向带根字的栈槽"
    );

    // 标量结构：写入不携带根，因此既不置位也不复位。
    let scalars = compile(
        "struct Pair { a: int, b: int }\nfn main() {\n let pair = Pair { a: 1, b: 2 }\n _ = pair\n}",
    );
    let body = named(&scalars, "main");
    assert!(
        body.root_inits.is_empty(),
        "标量槽不得产生逐根事件：{:?}",
        body.root_inits
    );
}

/// 变体感知置位：构造标量变体时，同一偏移在别的变体里是指针的并集条目不得被置位。
///
/// `Slot` 的两个变体在 tag 之后的同一偏移分别放 `int` 与 `String`，因此槽的 root 表把该偏移
/// 记成指针（并集）。只有构造 `Text` 变体才允许置位；构造 `Scalar` 变体必须是"只复位、不置位"。
#[test]
fn root_init_narrows_union_words_by_variant() {
    let declaration = "enum Slot { Scalar(int, int), Callable(fn() int) }\n";
    // 标量变体的两个 int 覆盖并集根字所在偏移，但字段自身不携带根，因此不得置位。
    let scalar = compile(&format!(
        "{declaration}fn main() {{\n let value = Slot::Scalar(7, 8)\n _ = value\n}}"
    ));
    let body = named(&scalar, "main");
    let unions: Vec<usize> = body
        .stack_slots
        .iter()
        .enumerate()
        .filter(|(_, slot)| slot.local == u32::MAX && !slot.roots.is_empty())
        .map(|(index, _)| index)
        .collect();
    assert!(
        !unions.is_empty(),
        "enum 临时槽必须把并集偏移登记为根：{:?}",
        body.stack_slots
    );
    assert!(
        body.root_inits
            .iter()
            .filter(|event| unions.contains(&event.slot.index()))
            .all(|event| !event.initialized),
        "构造标量变体不得在并集临时槽上置位：{:?}",
        body.root_inits
    );

    // 携带根的变体写入同一偏移：必须留下置位，证明收窄是按字段类型而不是按偏移。
    let callable = compile(&format!(
        "{declaration}fn main() {{\n let value = Slot::Callable(fn() int {{ return 1 }})\n _ = value\n}}"
    ));
    let body = named(&callable, "main");
    assert!(
        body.root_inits.iter().any(|event| {
            event.initialized && body.stack_slots[event.slot.index()].local == u32::MAX
        }),
        "构造携带根的变体必须为并集条目置位：{:?}",
        body.root_inits
    );
}

/// 逐根初始化证明：整值构造写全根字的槽判定为完整初始化；只写标量变体的并集槽不是。
#[test]
fn init_proof_marks_only_fully_written_slots() {
    let compilation = compile(
        "struct Pair { a: &int, b: &int }\nfn main() {\n let x = 1\n let y = 2\n let full = Pair { a: &x, b: &y }\n let view = &full\n _ = view\n}",
    );
    let body = named(&compilation, "main");
    let graph = crate::lir::pass::graph::Graph::new(body).expect("CFG");
    let init = crate::lir::root_init::InitSlots::new(body, &graph);
    let entry = body.entry;
    let end =
        body.blocks[entry.index()].instructions.end - body.blocks[entry.index()].instructions.start;
    let tracked: Vec<usize> = body
        .stack_slots
        .iter()
        .enumerate()
        .filter(|(_, slot)| slot.roots.len() == 2)
        .map(|(index, _)| index)
        .collect();
    assert!(!tracked.is_empty(), "夹具必须产生带两个根字的栈槽");
    assert!(
        tracked
            .iter()
            .any(|slot| init.fully_initialized(body, entry, end, *slot)),
        "整值构造写全根字后必须判定为完整初始化"
    );

    // 负向：只构造标量变体时并集临时槽只收到复位，不能判定为完整初始化。
    let scalar = compile(
        "enum Slot { Scalar(int, int), Callable(fn() int) }\nfn main() {\n let value = Slot::Scalar(7, 8)\n _ = value\n}",
    );
    let body = named(&scalar, "main");
    let graph = crate::lir::pass::graph::Graph::new(body).expect("CFG");
    let init = crate::lir::root_init::InitSlots::new(body, &graph);
    let entry = body.entry;
    let end =
        body.blocks[entry.index()].instructions.end - body.blocks[entry.index()].instructions.start;
    let unions: Vec<usize> = body
        .stack_slots
        .iter()
        .enumerate()
        .filter(|(_, slot)| slot.local == u32::MAX && !slot.roots.is_empty())
        .map(|(index, _)| index)
        .collect();
    assert!(!unions.is_empty(), "enum 临时槽必须把并集偏移登记为根");
    assert!(
        unions
            .iter()
            .all(|slot| !init.fully_initialized(body, entry, end, *slot)),
        "标量变体的并集槽不得判定为完整初始化"
    );
}

#[test]
fn verifier_rejects_over_budget_poll_free_path() {
    let compilation = compile(POLL);
    let mut editor = Editor::new(named(&compilation, "spin").clone());
    let poll_site = editor
        .live_blocks()
        .into_iter()
        .find_map(|block| {
            (0..editor.instruction_count(block))
                .find(|index| {
                    matches!(
                        editor.instruction((block, *index)).op,
                        Op::SafepointPoll { .. }
                    )
                })
                .map(|index| (block, index))
        })
        .expect("无限循环必须被 poll 切断");
    editor.remove_instruction(poll_site).expect("删除 poll");
    let mut body = editor.finish().expect("finish");
    crate::lir::uses::rebuild(&mut body);
    let error = verify::verify(&body, compilation.hir.as_ref().unwrap().module(), &ORACLE)
        .expect_err("去掉 poll 后的 poll-free 环必须被拒绝");
    assert_eq!(error.code(), DiagnosticCode::LirInvariant);
}

#[test]
fn action_key_is_sensitive_to_optimization_policy() {
    let base = OptimizationPolicyV1::default();
    let mut changed = base;
    changed.vector_policy_revision = base.vector_policy_revision + 1;
    assert_ne!(base.canonical_bytes(), changed.canonical_bytes());
}

#[test]
fn stackmap_world_covers_call_poll_suspend_and_select() {
    let compilation = compile(STACKMAP);
    let world = crate::lir::stackmap::derive(
        compilation.lir.as_ref().expect("已生成 LIR").bodies(),
        compilation.hir.as_ref().unwrap().module(),
    )
    .expect("栈图推导必须通过 verifier");
    // 函数按实例键排序；安全点按（函数序、站点序、种类、身份）确定性排列。
    assert!(
        world
            .functions
            .windows(2)
            .all(|pair| pair[0].instance < pair[1].instance),
        "函数必须按实例键排序"
    );
    let kinds: Vec<u8> = world.safepoints.iter().map(|point| point.kind).collect();
    assert!(
        kinds.contains(&crate::lir::stackmap::KIND_CALL_RETURN),
        "普通调用必须有 CallReturn 记录：{kinds:?}"
    );
    assert!(
        kinds.contains(&crate::lir::stackmap::KIND_POLL_RESUME),
        "循环 poll 必须有 PollResume 记录：{kinds:?}"
    );
    assert!(
        kinds.contains(&crate::lir::stackmap::KIND_SUSPEND_RESUME),
        "挂起与无 default select 必须有 SuspendResume 记录：{kinds:?}"
    );
    assert!(
        kinds.contains(&crate::lir::stackmap::KIND_MORESTACK_ENTRY),
        "入口检查必须有 MorestackEntry 记录：{kinds:?}"
    );
    // kind 分类计数与安全点总数一致；去重 map 不超过安全点数。
    let (demand, compression) = compilation
        .lir
        .as_ref()
        .expect("已生成 LIR")
        .stackmap_demands(compilation.hir.as_ref().unwrap().module());
    assert_eq!(
        compression.decode_sites, 0,
        "本夹具不含 DecodeCompressedRef，压缩需求必须为零"
    );
    assert_eq!(
        compression.compressed_root_slots, 0,
        "本夹具不含压缩根，压缩需求必须为零"
    );
    assert_eq!(
        demand.call_return
            + demand.poll_resume
            + demand.suspend_resume
            + demand.foreign_bridge
            + demand.morestack_entry,
        demand.safepoints,
        "kind 分类必须求和为安全点总数"
    );
    assert!(
        demand.maps <= demand.safepoints && demand.maps > 0,
        "去重 map 必须非空且不超过安全点数"
    );
    assert!(
        demand.functions_with_landing <= demand.functions,
        "落地函数不得超过函数总数"
    );
    // `MorestackEntry` 只含 ABI 参数根。
    for point in world
        .safepoints
        .iter()
        .filter(|point| point.kind == crate::lir::stackmap::KIND_MORESTACK_ENTRY)
    {
        for root in point
            .roots
            .direct
            .iter()
            .chain(&point.roots.interior)
            .chain(&point.roots.handle)
            .chain(&point.roots.compressed)
            .chain(&point.roots.stack)
        {
            assert!(
                matches!(root, crate::lir::stackmap::LogicalRoot::Argument { .. }),
                "MorestackEntry 只允许 ABI 参数根"
            );
        }
    }
    // 冷热编译的栈图需求一致。
    let compiler = Compiler::new();
    let request = || CompileRequest::single_file("main.gg", STACKMAP, TargetName::X86_64Linux);
    let cold = compiler.compile(request());
    let warm = compiler.compile(request());
    assert!(cold.is_success() && warm.is_success());
    assert_eq!(
        cold.image_plan().expect("image-plan").stackmap_demand(),
        warm.image_plan().expect("image-plan").stackmap_demand()
    );
}

/// 在一个 body 里插入 `DecodeCompressedRef` 并追加一个压缩槽根；没有空闲槽时返回 `None`。
///
/// Editor 维护 Mem 链与 uses 链；槽根是纯数据，只选空闲的 8 字节对齐偏移，避免与既有根
/// identity 冲突。
fn body_with_compressed_root(body: &Body) -> Option<Body> {
    let mut editor = Editor::new(body.clone());
    let block = editor
        .live_blocks()
        .into_iter()
        .find(|block| editor.terminator_memory(*block).is_some())?;
    let argument = editor.constant(0, Type::I64);
    let position = editor.instruction_count(block);
    let input = editor.terminator_memory(block)?;
    let (_, output) = editor.insert_memory(
        (block, position),
        Op::DecodeCompressedRef,
        &[argument],
        // 真实 lowering 的解码结果是普通 GcHeap 引用；`CompressedRef` 只作为类型保留。
        &[(ValueType::pointer(Provenance::GcHeap), Origin::None)],
        input,
    );
    editor.set_terminator_memory(block, output);
    let mut edited = editor.finish().ok()?;
    let slot_index = edited.stack_slots.iter().position(|slot| {
        (0..slot.bytes / 8).map(|step| step * 8).any(|offset| {
            offset + 8 <= slot.bytes && !slot.roots.iter().any(|(used, _)| *used == offset)
        })
    })?;
    let offset = (0..edited.stack_slots[slot_index].bytes / 8)
        .map(|step| step * 8)
        .find(|offset| {
            offset + 8 <= edited.stack_slots[slot_index].bytes
                && !edited.stack_slots[slot_index]
                    .roots
                    .iter()
                    .any(|(used, _)| used == offset)
        })?;
    edited.stack_slots[slot_index]
        .roots
        .push((offset, Provenance::CompressedRef));
    Some(edited)
}

/// 压缩需求的两个口径：解码点数与按安全点聚合的压缩根槽数。
#[test]
fn stackmap_demands_counts_decode_sites_and_compressed_roots() {
    let compilation = compile(STACKMAP);
    let module = compilation.hir.as_ref().unwrap().module();
    let lir = compilation.lir.as_ref().expect("已生成 LIR");
    let (pristine_stackmap, pristine_compression) = lir.stackmap_demands(module);
    // 逐个候选 body 探测：只有 poll/suspend/select/bridge 安全点收集槽根，纯调用返回点不收集，
    // 因此必须选一个推导后真的产生压缩根的 body。
    let mut world = None;
    for (index, body) in lir.bodies().iter().enumerate() {
        let Some(edited) = body_with_compressed_root(body) else {
            continue;
        };
        let mut candidate = lir.bodies().to_vec();
        candidate[index] = edited;
        let Ok(derived) = crate::lir::stackmap::derive(&candidate, module) else {
            continue;
        };
        if derived
            .safepoints
            .iter()
            .all(|point| point.roots.compressed.is_empty())
        {
            continue;
        }
        let probed = lir.with_bodies(candidate);
        let (stackmap, _) = probed.stackmap_demands(module);
        // 镜像 runtime 的 body 也可能有安全点。只接受不新增函数、安全点或 map 的候选，
        // 这样压缩根是加进既有 map，而不是给一个原先没有 map 的 body 新开一张。
        if stackmap.functions != pristine_stackmap.functions
            || stackmap.safepoints != pristine_stackmap.safepoints
            || stackmap.maps != pristine_stackmap.maps
        {
            continue;
        }
        world = Some(probed);
        break;
    }
    let mutated = world.expect("fixture 必须有能产生压缩根槽且不新增 map 的 body");
    let (stackmap, compression) = mutated.stackmap_demands(module);
    assert_eq!(
        compression.decode_sites,
        pristine_compression.decode_sites + 1,
        "每条 DecodeCompressedRef 必须计为一个解码点"
    );
    assert_eq!(stackmap.functions, pristine_stackmap.functions);
    assert_eq!(stackmap.safepoints, pristine_stackmap.safepoints);
    assert_eq!(stackmap.maps, pristine_stackmap.maps);
    // 压缩根槽按安全点聚合：同一槽根出现在多个安全点时逐点计数。
    let derived = crate::lir::stackmap::derive(mutated.bodies(), module)
        .expect("压缩槽根必须通过推导 verifier");
    let expected = derived
        .safepoints
        .iter()
        .map(|point| point.roots.compressed.len())
        .sum::<usize>();
    assert_eq!(
        usize::try_from(compression.compressed_root_slots).expect("槽数适配宿主"),
        expected,
        "压缩根槽必须等于推导世界里逐安全点的压缩根数之和"
    );
    assert!(
        expected
            > usize::try_from(pristine_compression.compressed_root_slots).expect("槽数适配宿主"),
        "压缩槽根必须真的进入压缩根集合"
    );
}

/// 在挂起的 foreign 调用前插入 `DecodeCompressedRef`。
///
/// `pass_to_call` 为真时把调用实参换成解码结果并同步声明参数类型；为假时解码结果没有使用
/// 者，作为对照组证明拒绝来自调用边界。
fn foreign_call_with_decoded_argument(body: &Body, pass_to_call: bool) -> Option<Body> {
    let mut editor = Editor::new(body.clone());
    let (block, term) = editor.live_blocks().into_iter().find_map(|block| {
        let term = editor.terminator(block).clone();
        matches!(
            &term,
            Term::Invoke { call, arguments, .. }
                if call.kind != crate::frontend::gir::body::CallKind::Managed
                    && !arguments.is_empty()
        )
        .then_some((block, term))
    })?;
    let Term::Invoke {
        call,
        arguments,
        results,
        memory,
        normal,
        unwind,
        safepoint,
    } = term
    else {
        return None;
    };
    let decoded = editor.constant(0, Type::I64);
    let input = editor.terminator_memory(block)?;
    let (compressed, output) = editor.insert_memory(
        (block, editor.instruction_count(block)),
        Op::DecodeCompressedRef,
        &[decoded],
        // 解码结果与真实 lowering 一致是 GcHeap；实参声明改写为 `CompressedRef` 时，
        // 拒绝仍然落在非 Managed 调用的 managed-provenance 边界上。
        &[(ValueType::pointer(Provenance::GcHeap), Origin::None)],
        input,
    );
    if pass_to_call {
        let mut parameters = call.parameters.clone();
        parameters[0] = ValueType::pointer(Provenance::CompressedRef);
        let mut arguments = arguments;
        arguments[0] = compressed[0];
        editor.set_terminator(
            block,
            Term::Invoke {
                call: body::Call { parameters, ..call },
                arguments,
                results,
                memory,
                normal,
                unwind,
                safepoint,
            },
        );
    }
    // 终结符重建之后再把它的 Mem 输入接到新解码结果上，避免覆写回旧链。
    editor.set_terminator_memory(block, output);
    editor.finish().ok()
}

/// `DecodeCompressedRef` 的结果作为非 Managed 调用参数必须被 verifier 拒绝。
#[test]
fn compressed_ref_as_foreign_argument_is_rejected() {
    let compilation = compile(EFFECTS);
    let module = compilation.hir.as_ref().unwrap().module();
    // 对照组：同一个解码点不跨调用边界时通过，证明拒绝来自调用 ABI 规则而不是解码指令本身。
    let control = foreign_call_with_decoded_argument(&named(&compilation, "main").clone(), false)
        .expect("effects 夹具必须含挂起的 foreign 调用");
    verify::verify(&control, module, &ORACLE).expect("未跨调用边界的压缩引用解码必须通过");
    let body = foreign_call_with_decoded_argument(&named(&compilation, "main").clone(), true)
        .expect("effects 夹具必须含挂起的 foreign 调用");
    let error = verify::verify(&body, module, &ORACLE).expect_err("压缩引用不得作为 foreign 参数");
    assert_eq!(error.code(), DiagnosticCode::LirInvariant);
    assert_eq!(
        error.message(),
        "LIR 终结符、unwind 或调用 ABI 不合法",
        "拒绝必须来自调用 ABI 规则"
    );
}

/// cage profile 开启的捕获闭包源码：闭包逃逸出创建帧，环境按 LocalHeap 分配
/// （turn 私有的 region 环境本阶段不压缩，因此夹具必须让环境落 stable storage）。
const COMPRESSED_CAPTURE: &str = "fn make() fn() int {\n let value = 1\n return fn() int { return value }\n}\nfn main() {\n let closure = make()\n _ = closure()\n}";

fn compile_with_compression(source: &str) -> Compilation {
    use crate::runtime::compression_schema::CompressionPolicyV1;
    let compilation = Compiler::shared().compile(
        CompileRequest::single_file("main.gg", source, TargetName::X86_64Linux)
            .with_compression_policy(CompressionPolicyV1::cage(
                4 * crate::runtime::gc_metadata_contract::GC_ARENA_BYTES,
            )),
    );
    assert!(
        compilation.is_success(),
        "{:?}",
        compilation.diagnostics().items()
    );
    compilation
}

/// cage 开启的捕获闭包：构造侧 encode、实例侧 decode、非零解码点与冷热一致指纹。
#[test]
fn compression_enabled_captures_produce_encode_and_decode_sequences() {
    let compilation = compile_with_compression(COMPRESSED_CAPTURE);
    let raw = compilation.raw_contract().expect("真实契约必须存在");
    let compression = raw.compression();
    assert!(compression.enabled());
    assert_eq!(
        compression.cage_bytes(),
        4 * crate::runtime::gc_metadata_contract::GC_ARENA_BYTES
    );
    assert!(
        compression.demand().decode_sites >= 1,
        "压缩环境的 capture 槽必须产出解码点"
    );
    assert_eq!(
        compression.demand().compressed_root_slots,
        0,
        "引用值与栈根声明保持全指针，压缩根槽本阶段为 0"
    );
    // image-plan 与 dump 的解码点口径与 LIR 指令数一致。
    assert_eq!(
        compilation
            .image_plan()
            .expect("镜像计划")
            .compression_demand()
            .decode_sites,
        compression.demand().decode_sites
    );
    assert!(
        compilation
            .dump_runtime()
            .expect("契约 dump")
            .contains(&format!(
                "decodes-sites={}",
                compression.demand().decode_sites
            )),
        "dump 必须携带同一解码点计数"
    );

    let lir = compilation.lir.as_ref().expect("已生成 LIR");
    let mut decode_sites = 0;
    for body in lir.bodies() {
        for instruction in &body.instructions {
            let Op::DecodeCompressedRef = &instruction.op else {
                continue;
            };
            decode_sites += 1;
            // 实例侧序列：Load(pointer(Raw)) → DecodeCompressedRef，结果是 GcHeap 引用。
            let word = body.args(&instruction.arguments)[0];
            let body::Definition::Instruction {
                instruction: load, ..
            } = body.values[word.index()].definition
            else {
                panic!("压缩字必须来自指令");
            };
            let load = &body.instructions[load.index()];
            assert!(matches!(load.op, Op::Load(_)), "压缩字必须由槽 load 读出");
            let result = &body.values[range(&instruction.results).start];
            assert_eq!(
                result.kind,
                ValueType::pointer(Provenance::GcHeap),
                "解码结果必须是普通 GcHeap 引用"
            );
        }
    }
    assert_eq!(
        usize::try_from(compression.demand().decode_sites).expect("解码点适配宿主"),
        decode_sites
    );

    // 构造侧：环境分配带 compressed 标志，PointerToInt → 整数域 → IntToPointer 的 encode
    // 序列与显式写屏障同 body 出现。
    let mut encode_bodies = 0;
    for body in lir.bodies() {
        let has_compressed_alloc = body.instructions.iter().any(|instruction| {
            matches!(
                instruction.op,
                Op::GcAlloc {
                    compressed: true,
                    ..
                }
            )
        });
        let has_encode = body.instructions.iter().any(|instruction| {
            matches!(instruction.op, Op::Convert(body::Conversion::IntToPointer))
        });
        let has_barrier = body
            .instructions
            .iter()
            .any(|instruction| matches!(instruction.op, Op::GcWriteBarrier { .. }));
        if has_compressed_alloc {
            assert!(
                has_encode && has_barrier,
                "压缩环境构造必须同时有 encode 序列与显式写屏障"
            );
            encode_bodies += 1;
        }
    }
    assert_eq!(encode_bodies, 1, "夹具只有 main 构造一个压缩环境");

    // 冷/热编译指纹一致：压缩输入进入 LIR 输入身份但不破坏确定性。
    let warm = compile_with_compression(COMPRESSED_CAPTURE);
    assert_eq!(
        warm.lir_fingerprint(),
        compilation.lir_fingerprint(),
        "同一压缩输入必须给出同一 LIR 内容身份"
    );
}

/// 关闭态同一源码：零解码点、无 encode/decode 序列，分配不带压缩标志。
#[test]
fn compression_disabled_captures_keep_full_pointer_slots() {
    let compilation = compile(COMPRESSED_CAPTURE);
    let raw = compilation.raw_contract().expect("真实契约必须存在");
    assert!(!raw.compression().enabled());
    assert_eq!(raw.compression().demand().decode_sites, 0);
    assert_eq!(raw.compression().demand().compressed_root_slots, 0);
    let lir = compilation.lir.as_ref().expect("已生成 LIR");
    for body in lir.bodies() {
        for instruction in &body.instructions {
            assert!(
                !matches!(instruction.op, Op::DecodeCompressedRef),
                "关闭态不得出现解码点"
            );
            assert!(
                !matches!(
                    instruction.op,
                    Op::Convert(body::Conversion::PointerToInt)
                        | Op::Convert(body::Conversion::IntToPointer)
                ),
                "关闭态不得出现 encode 序列"
            );
            assert!(
                !matches!(
                    instruction.op,
                    Op::GcAlloc {
                        compressed: true,
                        ..
                    }
                ),
                "关闭态分配不得带压缩标志"
            );
        }
    }
}

/// cage 开启时 Shared 环境不被压缩：仍是 SharedHandle 车道，无 encode/decode 序列。
#[test]
fn shared_environment_stays_uncompressed_with_cage_enabled() {
    let compilation = compile_with_compression(SHARED_SENDER);
    let raw = compilation.raw_contract().expect("真实契约必须存在");
    assert!(raw.compression().enabled());
    assert_eq!(
        raw.compression().demand().decode_sites,
        0,
        "Shared 环境按值捕获，不得产生解码点"
    );
    let lir = compilation.lir.as_ref().expect("已生成 LIR");
    for body in lir.bodies() {
        for instruction in &body.instructions {
            assert!(
                !matches!(instruction.op, Op::DecodeCompressedRef),
                "Shared 环境不得解码压缩字"
            );
            assert!(
                !matches!(
                    instruction.op,
                    Op::GcAlloc {
                        compressed: true,
                        ..
                    }
                ),
                "Shared 环境分配不得带压缩标志"
            );
        }
    }
    // 共享车道本身不受影响：fresh payload 紧跟解析，环境值保持 handle provenance。
    let shared = shared_body(&compilation);
    assert!(
        shared
            .instructions
            .iter()
            .any(|instruction| matches!(instruction.op, Op::ResolveSharedHandle)),
        "Shared 环境仍走 handle 解析"
    );
}

/// 开启路径上的真实 lowering 同样拒绝压缩引用跨非 Managed 调用边界。
#[test]
fn compressed_ref_as_foreign_argument_is_rejected_with_cage_enabled() {
    let compilation = compile_with_compression(EFFECTS);
    let module = compilation.hir.as_ref().unwrap().module();
    let body = foreign_call_with_decoded_argument(&named(&compilation, "main").clone(), true)
        .expect("effects 夹具必须含挂起的 foreign 调用");
    let error = verify::verify(&body, module, &ORACLE).expect_err("压缩引用不得作为 foreign 参数");
    assert_eq!(error.code(), DiagnosticCode::LirInvariant);
    assert_eq!(
        error.message(),
        "LIR 终结符、unwind 或调用 ABI 不合法",
        "拒绝必须来自调用 ABI 规则"
    );
}

/// 契约闭合：开启 profile 但没有解码点的空程序仍然合法。
///
/// 反方向（关闭却有解码点）由 `stackmap_demands_counts_decode_sites_and_compressed_roots`
/// 的递增口径与 `unsupported_capability_and_stray_decode_sites_are_rejected` 的契约拒绝共同闭合。
#[test]
fn enabled_compression_without_decode_sites_is_accepted() {
    let compilation = compile_with_compression("fn main() { let value = 1 _ = value }");
    let raw = compilation.raw_contract().expect("真实契约必须存在");
    assert!(raw.compression().enabled());
    assert_eq!(raw.compression().demand().decode_sites, 0);
    assert_eq!(
        raw.compression().demand().compressed_root_slots,
        0,
        "无捕获闭包时压缩根槽保持 0"
    );
}

#[test]
fn stackmap_rejects_duplicate_root_across_kinds() {
    let compilation = compile(STACKMAP);
    let mut world = crate::lir::stackmap::derive(
        compilation.lir.as_ref().expect("已生成 LIR").bodies(),
        compilation.hir.as_ref().unwrap().module(),
    )
    .expect("栈图推导必须通过 verifier");
    let point = world
        .safepoints
        .iter_mut()
        .find(|point| !point.roots.direct.is_empty() || !point.roots.stack.is_empty())
        .expect("fixture 必须含非空根集合");
    let duplicate = point
        .roots
        .direct
        .first()
        .or(point.roots.stack.first())
        .expect("根集合非空")
        .clone();
    // 同一身份进入两类位图：verifier 必须拒绝。
    if !point.roots.direct.is_empty() {
        point.roots.stack.push(duplicate);
    } else {
        point.roots.direct.push(duplicate);
    }
    world.fingerprint = world.fingerprint_of();
    let error = crate::lir::stackmap::verify_world(&world).expect_err("重复根必须被拒绝");
    assert_eq!(error.code(), DiagnosticCode::LirInvariant);
}

#[test]
fn optimize_fixture_preserves_results_after_pipeline() {
    let compilation = compile(OPTIMIZE);
    assert_eq!(interpret(named(&compilation, "choose"), &[1]), vec![7]);
    assert_eq!(interpret(named(&compilation, "choose"), &[0]), vec![9]);
    assert_eq!(interpret(named(&compilation, "countdown"), &[4]), vec![10]);
}

#[test]
fn slice_index_lowers_with_element_type() {
    // 回归：`v[i]` 的 place 类型必须先去引用再取元素，否则 LIR 会报分量不匹配。
    let compilation =
        compile("fn g(v: &[int], i: int) int = v[i]\nfn main() { let a = [0; 8]\n _ = g(&a, 1) }");
    let body = named(&compilation, "g");
    assert!(
        body.instructions
            .iter()
            .any(|instruction| matches!(instruction.op, Op::Load(_))),
        "下标读取必须 lowering 成一次 Load"
    );
}

#[test]
fn unsigned_shift_amount_needs_no_width_upper_bound_check() {
    let compilation = compile(
        "fn shift(x: uint, n: uint) uint { return x << n }\nfn main() { _ = shift(1, 100) }",
    );
    let body = named(&compilation, "shift");
    // 无符号移位量的区间下界按类型播种为非负，检查被全程序证明省略；
    // 即使保留检查，拒绝负值也是恒真条件，不得产生宽度上限比较。
    assert!(
        body.instructions.iter().all(|instruction| {
            !matches!(
                instruction.op,
                Op::Compare {
                    condition: body::Condition::Lt,
                    signed: false
                }
            ) || const_operand(body, body.args(&instruction.arguments)[1]) != Some(64)
        }),
        "无符号移位量按左操作数位宽取模，不得产生宽度上限检查"
    );
    // 证明省略路径按左操作数位宽取模执行：100 mod 64 = 36。
    assert_eq!(interpret(body, &[1, 100]), vec![1 << 36]);
    assert_eq!(interpret(body, &[3, 64]), vec![3]);
}

#[test]
fn signed_shift_amount_checks_non_negative() {
    let compilation =
        compile("fn shift(x: int, n: int) int { return x << n }\nfn main() { _ = shift(1, 2) }");
    let body = named(&compilation, "shift");
    assert!(
        body.instructions.iter().any(|instruction| matches!(
            instruction.op,
            Op::Compare {
                condition: body::Condition::Ge,
                signed: true
            }
        )),
        "有符号移位量必须检查非负"
    );
    // 非负移位量通过检查后按左操作数位宽取模（100 mod 64 = 36）；
    // 旧实现的宽度上限比较会拒绝 100，与证明路径按模执行的语义不一致。
    assert_eq!(interpret(body, &[1, 100]), vec![1 << 36]);
    assert_eq!(interpret(body, &[1, 63]), vec![1 << 63]);
}

/// 返回 value 直接由 `IConst` 定义时的常量。
fn const_operand(body: &Body, value: ValueId) -> Option<u64> {
    let body::Definition::Instruction { instruction, .. } = &body.values[value.index()].definition
    else {
        return None;
    };
    match body.instructions[instruction.index()].op {
        Op::IConst(value) => Some(value),
        _ => None,
    }
}

/// 仅解释 fixture 中的整数 SSA 子集，检验循环回边与合流的可观察结果。
fn interpret(body: &Body, arguments: &[u64]) -> Vec<u64> {
    let mut values = vec![0u64; body.values.len()];
    let mut block = body.entry;
    for (parameter, value) in body.params(block).iter().skip(1).zip(arguments) {
        values[parameter.value.index()] = *value;
    }
    for _ in 0..256 {
        for instruction in &body.instructions[range(&body.blocks[block.index()].instructions)] {
            let args: Vec<_> = body
                .args(&instruction.arguments)
                .iter()
                .map(|value| values[value.index()])
                .collect();
            let value = match instruction.op {
                Op::StackCheck
                | Op::SafepointPoll { .. }
                | Op::NoSafepointBegin(_)
                | Op::NoSafepointEnd(_) => continue,
                Op::IConst(value) => value,
                Op::Integer(body::IntOp::Add) => args[0].wrapping_add(args[1]),
                Op::Integer(body::IntOp::Sub) => args[0].wrapping_sub(args[1]),
                Op::Integer(body::IntOp::Shl) => {
                    // 语言语义：移位量按操作数位宽取模（64 位操作数 → mod 64）。
                    args[0].wrapping_shl(u32::try_from(args[1] % 64).expect("取模结果小于 64"))
                }
                Op::Compare { condition, signed } => {
                    let ordering = if signed {
                        i64::from_le_bytes(args[0].to_le_bytes())
                            .cmp(&i64::from_le_bytes(args[1].to_le_bytes()))
                    } else {
                        args[0].cmp(&args[1])
                    };
                    u64::from(match condition {
                        body::Condition::Eq => ordering.is_eq(),
                        body::Condition::Ne => !ordering.is_eq(),
                        body::Condition::Lt => ordering.is_lt(),
                        body::Condition::Le => !ordering.is_gt(),
                        body::Condition::Gt => ordering.is_gt(),
                        body::Condition::Ge => !ordering.is_lt(),
                    })
                }
                ref op => panic!("fixture 解释器遇到未登记操作：{op:?}"),
            };
            values[usize::try_from(instruction.results.start).unwrap()] = value;
        }
        let edge = match &body.blocks[block.index()].terminator {
            Terminator::Jump(edge) => *edge,
            Terminator::Branch { condition, yes, no } => {
                if values[condition.index()] != 0 {
                    *yes
                } else {
                    *no
                }
            }
            Terminator::Switch {
                value,
                cases,
                otherwise,
            } => body.switch_cases[range(cases)]
                .iter()
                .find(|(case, _)| *case == values[value.index()])
                .map_or(*otherwise, |(_, edge)| *edge),
            Terminator::Return { values: output, .. } => {
                return body
                    .args(output)
                    .iter()
                    .map(|value| values[value.index()])
                    .collect();
            }
            terminator => panic!("fixture 解释器遇到未登记终结符：{terminator:?}"),
        };
        let edge = &body.edges[edge.index()];
        let incoming: Vec<_> = body
            .args(&edge.arguments)
            .iter()
            .map(|value| values[value.index()])
            .collect();
        for (parameter, value) in body.params(edge.to).iter().zip(incoming) {
            values[parameter.value.index()] = value;
        }
        block = edge.to;
    }
    panic!("固定有界 fixture 的 LIR 没有终止")
}

#[test]
fn permit_quota_beyond_buffer_capacity_is_rejected() {
    use crate::runtime::barrier_schema::CARD_MARK_BUFFER_ENTRIES;

    // permit 是 compile-time 容量证明：`max_card_marks` 超过 processor 的
    // `CardMarkBuffer` 容量时，region 内必然需要补容量，而补容量只能发生在 region 外。
    // verifier 必须在后端前拒绝这种 permit。
    let compilation = compile(PUBLISH);
    let mut body = publish_body(&compilation);
    let permit = body
        .barrier_permits
        .first_mut()
        .expect("publish region 必须带 permit");
    permit.max_card_marks = CARD_MARK_BUFFER_ENTRIES + 1;
    // 必须断言具体分支：额度一致性检查也会以同一错误码拒绝，只断言错误码无法分辨。
    crate::lir::uses::rebuild(&mut body);
    let error = verify::verify(&body, compilation.hir.as_ref().unwrap().module(), &ORACLE)
        .expect_err("超额 permit 必须在后端前失败");
    assert_eq!(error.code(), DiagnosticCode::LirInvariant);
    assert!(
        error.message().contains("超过 CardMarkBuffer 容量"),
        "必须由容量检查拒绝，实际为：{}",
        error.message()
    );
}

#[test]
fn permit_quota_at_buffer_capacity_passes_the_capacity_check() {
    use crate::runtime::barrier_schema::CARD_MARK_BUFFER_ENTRIES;

    // 边界是 `>`：额度恰好等于容量时容量检查不触发，随后的失败必须来自额度一致性检查。
    let compilation = compile(PUBLISH);
    let mut body = publish_body(&compilation);
    let permit = body
        .barrier_permits
        .first_mut()
        .expect("publish region 必须带 permit");
    permit.max_card_marks = CARD_MARK_BUFFER_ENTRIES;
    crate::lir::uses::rebuild(&mut body);
    let error = verify::verify(&body, compilation.hir.as_ref().unwrap().module(), &ORACLE)
        .expect_err("额度与静态复算不一致仍必须失败");
    assert_eq!(error.code(), DiagnosticCode::LirInvariant);
    assert!(
        !error.message().contains("超过 CardMarkBuffer 容量"),
        "恰好等于容量不得触发容量检查，实际为：{}",
        error.message()
    );
    assert!(
        error.message().contains("静态消费上界不一致"),
        "应落到额度一致性检查，实际为：{}",
        error.message()
    );
}

/// TurnRegion：带环境的闭包在 turn 结束时整区发布并重置。
#[test]
fn turn_region_ops_close_at_suspend_boundaries() {
    let compilation = compile(
        "fn main() {\n let value = 1\n let closure = fn() int { return value }\n _ = closure()\n }",
    );
    let body = named(&compilation, "main");
    let allocs: Vec<u32> = body
        .instructions
        .iter()
        .filter_map(|instruction| match instruction.op {
            Op::RegionAlloc { region, .. } => Some(region),
            _ => None,
        })
        .collect();
    let publishes: Vec<(u32, u8)> = body
        .instructions
        .iter()
        .filter_map(|instruction| match instruction.op {
            Op::RegionPublish { region, export } => Some((region, export)),
            _ => None,
        })
        .collect();
    let resets: Vec<u32> = body
        .instructions
        .iter()
        .filter_map(|instruction| match instruction.op {
            Op::RegionReset { region } => Some(region),
            _ => None,
        })
        .collect();
    let transfers: Vec<u32> = body
        .instructions
        .iter()
        .filter_map(|instruction| match instruction.op {
            Op::RegionTransfer { region } => Some(region),
            _ => None,
        })
        .collect();
    assert_eq!(allocs.len(), 1, "闭包环境是唯一的 region 分配点");
    assert_eq!(
        publishes.len(),
        resets.len(),
        "每个出口恰好发布一次并重置一次"
    );
    assert!(publishes.iter().any(|(_, export)| *export == 0));
    assert!(transfers.is_empty(), "普通 turn 结束不产生转移");
    verify::verify_structure(body, compilation.hir.as_ref().unwrap().module())
        .expect("region 生命周期必须自洽");
}

/// channel send 在 sender 之后不再使用该闭包时整区移交，而不是复制或保留。
#[test]
fn channel_send_transfers_region_when_sender_is_dead() {
    let compilation = compile(
        "fn main() {\n let channel = chan[fn() int](1)\n let value = 1\n let closure = fn() int { return value }\n channel.send(closure)\n }",
    );
    let body = named(&compilation, "main");
    let transfers: Vec<u32> = body
        .instructions
        .iter()
        .filter_map(|instruction| match instruction.op {
            Op::RegionTransfer { region } => Some(region),
            _ => None,
        })
        .collect();
    assert_eq!(transfers.len(), 1, "移交语义必须落在一个 RegionTransfer 上");
    let resets = body
        .instructions
        .iter()
        .filter(|instruction| matches!(instruction.op, Op::RegionReset { .. }))
        .count();
    assert_eq!(resets, 0, "移交后 sender 不得重置同一个 region");
    let plan = compilation.image_plan().expect("镜像计划");
    assert_eq!(plan.turn_region_transfer_sites(), 1);
    assert!(plan.turn_region_sites() >= 1);
    verify::verify_structure(body, compilation.hir.as_ref().unwrap().module())
        .expect("移交路径必须自洽");
}

/// sender 在 send 之后仍使用闭包时不得选择 region，退回 SharedHeap。
#[test]
fn channel_send_with_live_sender_keeps_stable_storage() {
    let compilation = compile(
        "fn main() {\n let channel = chan[fn() int](1)\n let value = 1\n let closure = fn() int { return value }\n channel.send(closure)\n _ = closure()\n }",
    );
    let body = named(&compilation, "main");
    assert!(
        body.instructions
            .iter()
            .all(|instruction| !matches!(instruction.op, Op::RegionAlloc { .. })),
        "sender 仍在使用时必须落在 stable storage"
    );
    assert!(body.instructions.iter().any(|instruction| matches!(
        instruction.op,
        Op::GcAlloc {
            placement: crate::frontend::gir::placement::PlacementKind::SharedHeap,
            ..
        }
    )));
    assert_eq!(
        compilation
            .image_plan()
            .expect("镜像计划")
            .turn_region_sites(),
        0
    );
}

/// 未闭合 export summary 的 region 不允许 reset。
#[test]
fn reset_is_rejected_when_export_summary_is_open() {
    let compilation = compile(
        "fn main() {\n let value = 1\n let closure = fn() int { return value }\n _ = closure()\n }",
    );
    let mut body = named(&compilation, "main").clone();
    for instruction in &mut body.instructions {
        if let Op::RegionPublish { export, .. } = &mut instruction.op {
            *export = 1;
        }
    }
    let error = verify::verify(&body, compilation.hir.as_ref().unwrap().module(), &ORACLE)
        .expect_err("未闭合 summary 不能重置");
    assert_eq!(error.code(), DiagnosticCode::LirInvariant);
    assert!(
        error.message().contains("export summary 闭合"),
        "{}",
        error.message()
    );
}

/// 发布之后缺少结束动作的 region 必须被拒绝。
#[test]
fn region_without_end_action_is_rejected() {
    let compilation = compile(
        "fn main() {\n let value = 1\n let closure = fn() int { return value }\n _ = closure()\n }",
    );
    let mut body = named(&compilation, "main").clone();
    body.instructions.retain(|instruction| {
        !matches!(
            instruction.op,
            Op::RegionReset { .. } | Op::RegionTransfer { .. }
        )
    });
    crate::lir::uses::rebuild(&mut body);
    let error = verify::verify(&body, compilation.hir.as_ref().unwrap().module(), &ORACLE)
        .expect_err("发布后必须有结束动作");
    assert_eq!(error.code(), DiagnosticCode::LirInvariant);
}

/// region 移交只能发生在 channel send 边界上。
#[test]
fn region_transfer_requires_channel_send_boundary() {
    let compilation = compile(
        "fn main() {\n let value = 1\n let closure = fn() int { return value }\n _ = closure()\n }",
    );
    let mut body = named(&compilation, "main").clone();
    for instruction in &mut body.instructions {
        // 换成同样不需要 safepoint 的结束动作：生命周期自洽，但普通出口上出现移交必须被
        // channel 闸门拒绝。
        if let Op::RegionReset { region } = instruction.op {
            instruction.op = Op::RegionTransfer { region };
        }
    }
    crate::lir::uses::rebuild(&mut body);
    let error = verify::verify(&body, compilation.hir.as_ref().unwrap().module(), &ORACLE)
        .expect_err("普通出口不得移交 region");
    assert_eq!(error.code(), DiagnosticCode::LirInvariant);
    assert!(
        error.message().contains("channel send"),
        "{}",
        error.message()
    );
}

/// region 指令不引用任何值：参数数量由 verifier 强制。
#[test]
fn region_lifecycle_ops_carry_no_values() {
    let compilation = compile(
        "fn main() {\n let value = 1\n let closure = fn() int { return value }\n _ = closure()\n }",
    );
    let mut body = named(&compilation, "main").clone();
    for instruction in &mut body.instructions {
        if let Op::RegionPublish { .. } = instruction.op {
            instruction.arguments = 0..1;
        }
    }
    crate::lir::uses::rebuild(&mut body);
    let error = verify::verify(&body, compilation.hir.as_ref().unwrap().module(), &ORACLE)
        .expect_err("region 指令不得带参数");
    assert_eq!(error.code(), DiagnosticCode::LirInvariant);
}

/// sender 在 send 之后仍使用闭包：共享存储必须走 handle + guard。
#[test]
fn shared_handle_storage_is_guard_wrapped() {
    let compilation = compile(SHARED_SENDER);
    let body = shared_body(&compilation);
    let mut allocations = 0;
    let mut begins = 0;
    let mut ends = 0;
    let mut barriers = 0;
    let mut stores = 0;
    let mut loads = 0;
    for (index, instruction) in body.instructions.iter().enumerate() {
        match &instruction.op {
            Op::GcAlloc {
                placement: PlacementKind::SharedHeap,
                ..
            } => {
                allocations += 1;
                let value = ValueId(id(range(&instruction.results).start));
                let next = &body.instructions[index + 1];
                assert!(
                    matches!(next.op, Op::ResolveSharedHandle),
                    "fresh shared payload 必须紧跟解析"
                );
                assert_eq!(body.args(&next.arguments), [value]);
                assert_eq!(
                    body.values[range(&next.results).start].kind.provenance,
                    Some(Provenance::SharedHandle)
                );
            }
            Op::SharedAccessBegin { .. } => begins += 1,
            Op::SharedAccessEnd { .. } => ends += 1,
            Op::SharedFieldBarrier { .. } => barriers += 1,
            Op::Store(_) => stores += 1,
            Op::Load(_) => loads += 1,
            _ => {}
        }
    }
    assert!(
        allocations >= 1,
        "共享捕获必须有 SharedHeap 分配（提升后的 cell 与环境）"
    );
    assert_eq!(begins, ends, "guard 必须成对");
    assert!(begins > 0 && loads > 0, "共享读取必须经过 guard");
    // 捕获槽提升成的 cell 与环境都在共享平面：环境槽里放的必须是 handle，而不是本地地址或副本。
    assert!(
        body.instructions.iter().any(|instruction| {
            let args = body.args(&instruction.arguments);
            matches!(&instruction.op, Op::Store(_))
                && args.len() == 2
                && body.values[args[1].index()].kind.provenance == Some(Provenance::SharedHandle)
        }),
        "共享环境槽必须保存 stable handle"
    );
    let _ = (stores, barriers);
}

/// 两个捕获的共享环境：第二个捕获必须经 `PtrOffset` 落在同一个 guard 内。
#[test]
fn shared_environment_offset_stays_inside_guard() {
    let compilation = compile(
        "fn main() {\n let channel = chan[fn() int](1)\n let a = 1\n let b = 2\n let closure = fn() int { return a + b }\n channel.send(closure)\n _ = closure()\n }",
    );
    let body = shared_body(&compilation);
    let mut offsets = 0;
    for (token, instruction) in guard_members(&body) {
        if let Op::PtrOffset = instruction.op {
            let Some(base) = body.args(&instruction.arguments).first() else {
                continue;
            };
            if body.values[base.index()].kind.provenance != Some(Provenance::SharedHandle) {
                continue;
            }
            offsets += 1;
            assert!(token.is_some(), "shared 字段偏移必须在 guard 内");
        }
    }
    assert!(offsets > 0, "第二个捕获必须产生共享字段偏移");
}

/// 缺少 guard 结束、或 fresh payload 没有被解析的 body 必须被拒绝。
#[test]
fn shared_guard_and_resolution_rules_are_enforced() {
    let compilation = compile(SHARED_SENDER);
    let mut missing_end = shared_body(&compilation);
    missing_end
        .instructions
        .retain(|instruction| !matches!(instruction.op, Op::SharedAccessEnd { .. }));
    crate::lir::uses::rebuild(&mut missing_end);
    let error = verify::verify(
        &missing_end,
        compilation.hir.as_ref().unwrap().module(),
        &ORACLE,
    )
    .expect_err("guard 必须结束");
    assert_eq!(error.code(), DiagnosticCode::LirInvariant);

    let mut missing_resolve = shared_body(&compilation);
    missing_resolve
        .instructions
        .retain(|instruction| !matches!(instruction.op, Op::ResolveSharedHandle));
    crate::lir::uses::rebuild(&mut missing_resolve);
    let error = verify::verify(
        &missing_resolve,
        compilation.hir.as_ref().unwrap().module(),
        &ORACLE,
    )
    .expect_err("fresh payload 必须被解析");
    assert_eq!(error.code(), DiagnosticCode::LirInvariant);

    let mut wrong_token = shared_body(&compilation);
    for instruction in &mut wrong_token.instructions {
        if let Op::SharedAccessEnd { token } = &mut instruction.op {
            *token += 7;
        }
    }
    crate::lir::uses::rebuild(&mut wrong_token);
    let error = verify::verify(
        &wrong_token,
        compilation.hir.as_ref().unwrap().module(),
        &ORACLE,
    )
    .expect_err("guard 的 begin/end 必须配对");
    assert_eq!(error.code(), DiagnosticCode::LirInvariant);
}

/// 返回含 SharedHeap 分配的 body 的可变副本。
fn shared_body(compilation: &Compilation) -> Body {
    compilation
        .lir
        .as_ref()
        .expect("已生成 LIR")
        .bodies()
        .iter()
        .find(|body| {
            body.instructions.iter().any(|instruction| {
                matches!(
                    instruction.op,
                    Op::GcAlloc {
                        placement: PlacementKind::SharedHeap,
                        ..
                    }
                )
            })
        })
        .expect("存在 SharedHeap 分配")
        .clone()
}

/// 没有共享 placement 的源码必须零共享需求与零共享站点。
///
/// 这是验收项「共享访问额外成本不扩散到 LocalHeap」在 LIR 层的证据：同一份源码只要不把闭包
/// 送出 owner，就不会出现 handle 解析、guard、字段屏障或搬迁站点，LocalHeap 路径上没有任何
/// 共享记账。
#[test]
fn local_only_source_has_no_shared_heap_sites() {
    let compilation = compile(
        "fn main() {\n let value = 1\n let closure = fn() int { return value }\n _ = closure()\n }",
    );
    let lir = compilation.lir.as_ref().expect("已生成 LIR");
    assert_eq!(
        lir.shared_heap_demand(0),
        crate::runtime::SharedHeapDemand::default(),
        "LocalHeap-only 源码不得产生任何共享需求"
    );
    for body in lir.bodies() {
        for instruction in &body.instructions {
            assert!(
                !matches!(
                    instruction.op,
                    Op::ResolveSharedHandle
                        | Op::SharedAccessBegin { .. }
                        | Op::SharedAccessEnd { .. }
                        | Op::SharedFieldBarrier { .. }
                        | Op::SharedFieldBarrierReserved { .. }
                        | Op::ForwardSharedHandle
                        | Op::GcAlloc {
                            placement: PlacementKind::SharedHeap,
                            ..
                        }
                ),
                "LocalHeap-only 源码不得出现共享站点"
            );
        }
    }
}

/// 返回每个指令与它当时所处的 guard token。
fn guard_members(body: &Body) -> Vec<(Option<u32>, &crate::lir::body::Instruction)> {
    let mut members = Vec::new();
    let mut stack: Vec<u32> = Vec::new();
    for block in &body.blocks {
        for instruction in &body.instructions[range(&block.instructions)] {
            match &instruction.op {
                Op::SharedAccessBegin { token } => stack.push(*token),
                Op::SharedAccessEnd { .. } => {
                    stack.pop();
                }
                _ => {}
            }
            members.push((stack.last().copied(), instruction));
        }
    }
    members
}

/// sender 在 send 之后仍使用闭包：闭包环境必须落在 SharedHeap。
///
/// 源码与 runtime harness/bench 共用同一个夹具文件，因此 LIR 需求与真实推导不会各自漂移。
const SHARED_SENDER: &str = include_str!("../runtime/fixtures/shared_sender.gg");

#[test]
fn runtime_machine_calls_enter_lir_with_glue_shape() {
    let compilation = compile(
        "fn main() { unsafe { let code: uint = 0\n std.runtime.process_exit(code)\n _ = std.runtime.current_slot() } }",
    );
    let body = named(&compilation, "main");
    assert!(body.instructions.iter().any(|instruction| matches!(
        instruction.op,
        Op::RuntimeMachineCall(crate::runtime::RuntimeMachineOp::ProcessExit)
    )));
    assert!(body.instructions.iter().any(|instruction| matches!(
        instruction.op,
        Op::RuntimeMachineCall(crate::runtime::RuntimeMachineOp::CurrentSlot)
    )));

    let compilation = compile(
        "use std.ptr\nfn main() { let value: uint = 0\n unsafe { let p = ptr.addr_of(value)\n std.runtime.context_switch(p, p, p, p)\n std.runtime.context_restore(p, p, p) } }",
    );
    let body = named(&compilation, "main");
    assert!(body.instructions.iter().any(|instruction| matches!(
        instruction.op,
        Op::RuntimeMachineCall(crate::runtime::RuntimeMachineOp::ContextSwitch)
    )));
    assert!(body.instructions.iter().any(|instruction| matches!(
        instruction.op,
        Op::RuntimeMachineCall(crate::runtime::RuntimeMachineOp::ContextRestore)
    )));
}
