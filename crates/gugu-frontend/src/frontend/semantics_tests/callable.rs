use crate::{CompileRequest, Compiler, TargetName};

fn accepts(source: &str) -> bool {
    let result = Compiler::shared().compile(CompileRequest::single_file(
        "main.gg",
        source,
        TargetName::X86_64Linux,
    ));
    if !result.is_success() {
        eprintln!("{:?}", result.diagnostics().items());
    }
    result.is_success()
}

use super::accept_cases;

accept_cases! {
    closure_signatures_infer_from_context_and_expression_bodies_00: true,
        "fn apply(f: fn(int) int) int { f(2) }\nfn main() { let inc = fn(x) = x + 1\n _ = inc(1)\n _ = apply(fn(x) = x + 2) }";
    closure_signatures_infer_from_context_and_expression_bodies_01: false,
        "fn main() { let f = fn(x) { _ = x } }";
    closure_signatures_infer_from_context_and_expression_bodies_02: false,
        "fn main() { let f = fn() = loop {} }";
}

accept_cases! {
    named_functions_and_closures_erase_only_in_callable_contexts_00: true,
        "fn inc(x: int) int = x + 1\nfn main() { let f: fn(int) int = inc\n let a: [fn(int) int; 2] = [inc, fn(x: int) int = x - 1]\n _ = a[0](f(1)) }";
    named_functions_and_closures_erase_only_in_callable_contexts_01: false,
        "fn a() {}\nfn b() {}\nfn main() { let f = a\n f = b }";
    named_functions_and_closures_erase_only_in_callable_contexts_02: true,
        "fn diverges() ! = loop {}\nfn main() { let f: fn() int = diverges\n _ = f }";
    named_functions_and_closures_erase_only_in_callable_contexts_03: false,
        "fn normal() int = 1\nfn main() { let f: fn() ! = normal }";
    named_functions_and_closures_erase_only_in_callable_contexts_04: false,
        "fn id[T](value: T) T = value\nfn main() { _ = id[int](1) }";
    named_functions_and_closures_erase_only_in_callable_contexts_05: true,
        "fn id(value: int) int = value\nstatic FS: [fn(int) int; 1] = [id]\nfn main() { _ = FS[0](1) }";
    named_functions_and_closures_erase_only_in_callable_contexts_06: true,
        "static F: fn() int = read\nfn read() int { _ = F\n 1 }\nfn main() { _ = F() }";
    named_functions_and_closures_erase_only_in_callable_contexts_07: true,
        "static F: fn() int = fn() int { _ = F\n 1 }\nfn main() { _ = F() }";
    named_functions_and_closures_erase_only_in_callable_contexts_08: false,
        "static N: int = read()\nfn read() int = N\nfn main() {}";
}

accept_cases! {
    closure_construction_does_not_execute_or_initialize_captures_00: true,
        "fn main() { let x: int\n let f = fn() int = x\n x = 2\n _ = f() }";
    closure_construction_does_not_execute_or_initialize_captures_01: false,
        "fn item[T]() {}\nfn main() { let f = item::[int]\n f = item::[bool] }";
    closure_construction_does_not_execute_or_initialize_captures_02: false,
        "fn main() { let x: int\n let f = fn() int = x\n _ = f() }";
    closure_construction_does_not_execute_or_initialize_captures_03: false,
        "fn main() { let x: int\n let f = fn() { x = 1 }\n _ = x }";
    closure_construction_does_not_execute_or_initialize_captures_04: true,
        "fn counter() fn() int { let value = 0\n fn() int { value += 1\n value } }\nfn main() { let f = counter()\n _ = f() }";
}

accept_cases! {
    escaping_callables_require_initialized_captures_after_cleanup_00: false,
        "fn invoke(f: fn() int) int = f()\nfn main() { let x: int\n _ = invoke(fn() int = x) }";
    escaping_callables_require_initialized_captures_after_cleanup_01: false,
        "fn invalid() fn() int { let x: int\n fn() int = x }\nfn main() {}";
    escaping_callables_require_initialized_captures_after_cleanup_02: true,
        "fn valid() fn() int { let x: int\n defer { x = 1 }\n fn() int = x }\nfn main() { _ = valid() }";
}

accept_cases! {
    recursive_and_mutually_recursive_closures_share_initialized_slots_00: true,
        "fn main() { let fact: fn(int) int = fn(n: int) int { if n == 0 { 1 } else { n * fact(n - 1) } }\n _ = fact(5) }";
    recursive_and_mutually_recursive_closures_share_initialized_slots_01: true,
        "fn main() { let even: fn(int) bool\n let odd: fn(int) bool\n even = fn(n: int) bool { if n == 0 { true } else { odd(n - 1) } }\n odd = fn(n: int) bool { if n == 0 { false } else { even(n - 1) } }\n _ = even(6) }";
    recursive_and_mutually_recursive_closures_share_initialized_slots_02: false,
        "fn main() { let f: fn()\n f()\n f = fn() {} }";
}

accept_cases! {
    closure_exits_do_not_consume_outer_loop_try_or_defer_state_00: true,
        "fn f() Option[int] { defer { let ok: int = 1 }\n let g = fn() int { return 2 }\n Some(g()) }\nfn main() { _ = f() }";
    closure_exits_do_not_consume_outer_loop_try_or_defer_state_01: false,
        "fn main() { loop { let f = fn() { break }\n break } }";
    closure_exits_do_not_consume_outer_loop_try_or_defer_state_02: false,
        "fn f() Option[int] { let g = fn() int { Some(1)? }\n Some(g()) }\nfn main() {}";
}

accept_cases! {
    async_launch_separates_parent_evaluation_from_child_execution_00: true,
        "fn identity(x: int) int = x\nfn main() { let x: int\n let task: Join[int] = async identity({ x = 1\n x })\n _ = x\n _ = task }";
    async_launch_separates_parent_evaluation_from_child_execution_01: false,
        "fn main() { let x: int\n let task = async { x = 1 }\n _ = x }";
    async_launch_separates_parent_evaluation_from_child_execution_02: true,
        "fn make() Join[int] { let n = 1\n async { return n } }\nfn main() { _ = make() }";
    async_launch_separates_parent_evaluation_from_child_execution_03: false,
        "fn main() { let x: int\n let task = async { x }\n x = 1\n _ = task }";
}

accept_cases! {
    homogeneous_variadics_materialize_the_tail_and_reject_wrong_elements_00: true,
        "fn sum(start: int, ...xs: &[int]) int { let result = start\n for x in xs { result += x }\n result }\nfn main() { _ = sum(0)\n _ = sum(1, 2, 3) }";
    homogeneous_variadics_materialize_the_tail_and_reject_wrong_elements_01: false,
        "fn f(...xs: &[int]) {}\nfn main() { f(true) }";
    homogeneous_variadics_materialize_the_tail_and_reject_wrong_elements_02: false,
        "fn f(...xs: &[int], last: int) {}\nfn main() {}";
}

accept_cases! {
    generic_function_items_infer_and_accept_explicit_type_arguments_00: true,
        "fn identity[T](value: T) T = value\nfn main() { let n: i8 = identity(1)\n let s = identity::[string](\"text\")\n _ = n\n _ = s }";
    generic_function_items_infer_and_accept_explicit_type_arguments_01: false,
        "fn identity[T](value: T) T = value\nfn main() { _ = identity::[bool](1) }";
    generic_function_items_infer_and_accept_explicit_type_arguments_02: false,
        "fn identity[T](value: T) T = value\nfn main() { _ = identity::[int, bool](1) }";
}

accept_cases! {
    builtin_callable_bounds_preserve_the_concrete_function_identity_00: true,
        "fn apply[T, U, F: Fn(T) U](f: F, value: T) U = f(value)\nfn main() { let n: int = apply(fn(x) = x + 1, 2)\n _ = n }";
    builtin_callable_bounds_preserve_the_concrete_function_identity_01: false,
        "fn apply[F: Fn(int) bool](f: F) bool = f(1)\nfn main() { _ = apply(fn(x: int) int = x) }";
}

accept_cases! {
    heterogeneous_parameter_packs_do_not_coerce_or_box_elements_00: true,
        "fn consume[Ts...](...args: Ts) {}\nfn main() { consume()\n consume(1, true, \"text\") }";
    heterogeneous_parameter_packs_do_not_coerce_or_box_elements_01: false,
        "fn consume[T](...args: T) {}\nfn main() { consume(1, true) }";
    heterogeneous_parameter_packs_do_not_coerce_or_box_elements_02: true,
        "fn consume[Ts...](...args: Ts) {}\nfn main() { consume::[int, bool](1, true) }";
    heterogeneous_parameter_packs_do_not_coerce_or_box_elements_03: false,
        "fn consume[Ts...](...args: Ts) {}\nfn main() { consume::[int, bool](1, 2) }";
    heterogeneous_parameter_packs_do_not_coerce_or_box_elements_04: true,
        "fn consume[Fs: Fn() int...](...args: Fs) {}\nfn main() { consume(fn() int = 1, fn() int = 2) }";
    heterogeneous_parameter_packs_do_not_coerce_or_box_elements_05: false,
        "fn consume[Fs: Fn() int...](...args: Fs) {}\nfn main() { consume(1) }";
    heterogeneous_parameter_packs_do_not_coerce_or_box_elements_06: true,
        "fn consume[T, Fs: Fn(T)...](...args: Fs) {}\nfn main() { consume(fn(x: int) {}) }";
}

#[test]
fn captured_slot_identity_and_storage_survive_query_reuse() {
    use crate::frontend::semantics::output::{CAPTURED, CROSS_COROUTINE};
    let source = "fn main() { let n = 1\n let read = fn() int = n\n let write = fn() { n += 1 }\n let n = 3\n let next = fn() int = n\n let job = async { n } }";
    let queries = crate::QueryEngine::new();
    let cold = super::frontend(&[("main.gg", source)], &queries).unwrap();
    let warm = super::frontend(&[("main.gg", source)], &queries).unwrap();
    assert_eq!(cold.semantics, warm.semantics);
    let body = warm
        .semantics
        .bodies
        .iter()
        .find(|body| body.captures.len() == 4)
        .unwrap();
    let first = body.captures[0].captures[0].slot;
    let shadowed = body.captures[2].captures[0].slot;
    assert_eq!(first, body.captures[1].captures[0].slot);
    assert_ne!(first, shadowed);
    assert_eq!(shadowed, body.captures[3].captures[0].slot);
    assert!(body.captures[1].captures[0].written);
    assert_eq!(body.slot_storage[first] & CAPTURED, CAPTURED);
    assert_eq!(
        body.slot_storage[shadowed] & (CAPTURED | CROSS_COROUTINE),
        CAPTURED | CROSS_COROUTINE
    );
}
