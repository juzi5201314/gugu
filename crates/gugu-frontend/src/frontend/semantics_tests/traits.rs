use super::{accept_cases, accepts};

#[test]
fn inherent_methods_accept_receiver_and_type_ufcs_forms() {
    assert!(accepts(
        "struct Point { x: int }\nimpl Point { fn new(x: int) Point = Point { x }\n fn value(self) int = self.x\n fn set(self: &Self, x: int) { self.x = x } }\nfn main() { let p = Point::new(1)\n p.set(2)\n _ = p.value()\n _ = Point::value(p)\n _ = (&p).value() }"
    ));
    assert!(!accepts(
        "struct Point {}\nimpl Point { fn new() Point = Point {} }\nfn main() { let p = Point {}\n _ = p.new() }"
    ));
}

#[test]
fn trait_impls_check_required_items_associated_types_and_signatures() {
    assert!(accepts(
        "trait Measure { type Unit\n const SCALE: int\n fn measure(self) Self::Unit }\nstruct Point { x: int }\nimpl Measure for Point { type Unit = int\n const SCALE = 2\n fn measure(self) int = self.x * Self::SCALE }\nfn main() { let p = Point { x: 1 }\n let n: int = p.measure()\n _ = n }"
    ));
    assert!(!accepts(
        "trait Measure { type Unit\n fn measure(self) Self::Unit }\nstruct Point {}\nimpl Measure for Point { type Unit = int }\nfn main() {}"
    ));
    assert!(!accepts(
        "trait Measure { fn measure(self) int }\nstruct Point {}\nimpl Measure for Point { fn measure(self) bool = true }\nfn main() {}"
    ));
    assert!(!accepts(
        "trait Measure { fn measure(self) int }\nstruct Point {}\nimpl Measure for Point { fn measure(self) int = 1\n fn extra(self) {} }\nfn main() {}"
    ));
}

#[test]
fn inherent_methods_win_and_trait_ambiguity_requires_ufcs() {
    assert!(accepts(
        "trait Left { fn value(self) bool }\ntrait Right { fn value(self) int }\nstruct Point {}\nimpl Left for Point { fn value(self) bool = true }\nimpl Right for Point { fn value(self) int = 1 }\nimpl Point { fn value(self) string = \"inherent\" }\nfn main() { let p = Point {}\n let s: string = p.value()\n let b: bool = Left::value(p)\n _ = s\n _ = b }"
    ));
    assert!(!accepts(
        "trait Left { fn value(self) int }\ntrait Right { fn value(self) int }\nstruct Point {}\nimpl Left for Point { fn value(self) int = 1 }\nimpl Right for Point { fn value(self) int = 2 }\nfn main() { _ = Point {}.value() }"
    ));
}

#[test]
fn generic_trait_bounds_validate_at_calls_and_support_default_methods() {
    assert!(accepts(
        "trait Measure { fn value(self) int\n fn doubled(self) int = self.value() * 2 }\nstruct Point { x: int }\nimpl Measure for Point { fn value(self) int = self.x }\nfn measure[T: Measure](value: T) int = value.doubled()\nfn main() { _ = measure(Point { x: 3 }) }"
    ));
    assert!(!accepts(
        "trait Measure { fn value(self) int }\nfn measure[T: Measure](value: T) int = value.value()\nfn main() { _ = measure(1) }"
    ));
}

#[test]
fn specialization_rejects_crossing_patterns_and_changed_associated_types() {
    assert!(accepts(
        "trait Value { fn value(self) int }\nstruct Box[T] { value: T }\nimpl Value for Box[T] { fn value(self) int = 1 }\nimpl Value for Box[int] { fn value(self) int = 2 }\nfn main() { let b: Box[int] = Box { value: 0 }\n _ = b.value() }"
    ));
    assert!(!accepts(
        "trait Value {}\nstruct Pair[A, B] { a: A, b: B }\nimpl Value for Pair[T, int] {}\nimpl Value for Pair[int, U] {}\nfn main() {}"
    ));
    assert!(!accepts(
        "trait Value { type Output }\nstruct Box[T] { value: T }\nimpl Value for Box[T] { type Output = int }\nimpl Value for Box[int] { type Output = bool }\nfn main() {}"
    ));
}

#[test]
fn negative_impls_remove_only_strictly_more_general_positive_candidates() {
    assert!(accepts(
        "trait Marker {}\nstruct Box[T] { value: T }\nimpl[T] Marker for Box[T] {}\nimpl !Marker for Box[bool] {}\nfn use_marker[T: Marker](value: T) {}\nfn main() { let value: Box[int] = Box { value: 1 }\n use_marker(value) }"
    ));
    assert!(!accepts(
        "trait Marker {}\nstruct Box[T] { value: T }\nimpl[T] Marker for Box[T] {}\nimpl !Marker for Box[bool] {}\nfn use_marker[T: Marker](value: T) {}\nfn main() { let value: Box[bool] = Box { value: true }\n use_marker(value) }"
    ));
    assert!(!accepts(
        "trait Marker {}\nstruct Point {}\nimpl Marker for Point {}\nimpl !Marker for Point {}\nfn main() {}"
    ));
    assert!(!accepts(
        "struct Point {}\nimpl !Clone for Point { fn extra(self) {} }\nfn main() {}"
    ));
}

accept_cases! {
    user_operators_dispatch_without_implicit_scalar_conversion_00: true,
        "struct Point { x: int }\nimpl Add[Point] for Point { type Output = Point\n fn add(self, rhs: Point) Point = Point { x: self.x + rhs.x } }\nimpl Eq for Point { fn eq(self: &Self, other: &Self) bool = self.x == other.x }\nfn main() { let a = Point { x: 1 }\n let b = a + a\n _ = a == b }";
    user_operators_dispatch_without_implicit_scalar_conversion_01: false,
        "struct Point {}\nimpl Add[int] for Point { type Output = int\n fn add(self, rhs: int) int = rhs }\nfn main() { let rhs: i8 = 1\n _ = Point {} + rhs }";
}

#[test]
fn forbidden_builtin_impls_and_forbid_downgrades_are_rejected() {
    assert!(!accepts("impl Clone for chan[int] {}\nfn main() {}"));
    assert!(!accepts(
        "struct Point {}\nimpl Any for Point {}\nfn main() {}"
    ));
    assert!(!accepts(
        "#![forbid(unused)]\n#[allow(unused)] fn main() {}"
    ));
}

accept_cases! {
    generic_associated_calls_preserve_type_arguments_and_field_calls_00: true,
        "struct Box[T] { value: T }\nimpl Box[T] { fn new(value: T) Self = Self { value }\n fn get(self) T = self.value }\nfn main() { let b = Box[int]::new(1)\n let n: int = b.get()\n _ = n }";
    generic_associated_calls_preserve_type_arguments_and_field_calls_01: true,
        "struct Holder { callback: fn(int) int }\nfn identity(x: int) int = x\nfn main() { let h = Holder { callback: identity }\n _ = h.callback(1)\n _ = (h).callback(2) }";
}

#[test]
fn associated_projection_and_constant_paths_resolve_without_bare_names() {
    assert!(accepts(
        "trait Value { type Output\n const N: int\n fn get(self) Self::Output }\nstruct Point {}\nimpl Value for Point { type Output = int\n const N = 3\n fn get(self) int = Self::N }\nfn extract[T: Value](v: T) T::Output = v.get()\nfn main() { let n: Point::Output = extract(Point {})\n let a: [int; Point::N] = [0; Point::N]\n _ = n\n _ = a }"
    ));
    assert!(!accepts(
        "trait Value { type Output }\nfn invalid[T: Value](v: T) Output = v\nfn main() {}"
    ));
}

#[test]
fn bounded_impls_are_not_available_without_their_evidence() {
    assert!(!accepts(
        "trait Marker {}\ntrait Value { fn get(self) int }\nstruct Box[T] { value: T }\nimpl[T: Marker] Value for Box[T] { fn get(self) int = 1 }\nfn main() { let b: Box[int] = Box { value: 1 }\n _ = b.get() }"
    ));
    assert!(!accepts(
        "struct Box[T] { value: T }\ntrait Marker {}\nimpl[T: Marker] Box[T] { fn get(self) int = 1 }\nfn main() { let b: Box[int] = Box { value: 1 }\n _ = b.get() }"
    ));
}

accept_cases! {
    lint_forbid_is_scoped_and_cfg_removed_nodes_do_not_participate_00: true,
        "#[forbid(unused)] fn f() {}\n#[allow(unused)] fn main() {}";
    lint_forbid_is_scoped_and_cfg_removed_nodes_do_not_participate_01: true,
        "#![forbid(unused)]\n#[cfg(false)] #[allow(unused)] fn absent() {}\nfn main() {}";
    lint_forbid_is_scoped_and_cfg_removed_nodes_do_not_participate_02: false,
        "#[forbid(unused)] fn main() { #[warn(unused)] let x = 1\n _ = x }";
}

#[test]
fn user_index_and_compound_assignment_use_their_designated_traits() {
    assert!(accepts(
        "struct Cell { value: int }\nimpl Index for Cell { type Output = int\n fn index(self: &Self, i: int) int = self.value\n fn index_set(self: &Self, i: int, v: int) { self.value = v } }\nimpl AddAssign[int] for Cell { fn add_assign(self: &Self, rhs: int) { self.value += rhs } }\nfn main() { let c = Cell { value: 0 }\n c[0] = 1\n c[0] += 2\n c += 2\n let n: int = c[0]\n _ = n }"
    ));
}

#[test]
fn builtin_arithmetic_impls_serve_generic_bounds_and_string_operators() {
    assert!(accepts(
        "fn sum[T: Add[T]](a: T, b: T) T::Output = a + b\nfn main() { _ = sum(1, 2)\n _ = sum(\"a\", \"b\") }"
    ));
    assert!(accepts(
        "fn bump[T: AddAssign[T]](value: &T, step: T) { *value += step }\nfn main() { let n = 1\n let _ = bump::[int](&n, 2)\n _ = n }"
    ));
    assert!(accepts(
        "fn main() { let text = \"a\"\n text += \"b\"\n _ = text + \"c\" }"
    ));
}

accept_cases! {
    associated_function_generic_order_and_method_contracts_are_preserved_00: true,
        "trait Convert[Z, A] { fn convert(self, value: Z) A }\nstruct Point {}\nimpl Convert[int, bool] for Point { fn convert(self, value: int) bool = true }\nfn main() { let b: bool = Convert[int, bool]::convert(Point {}, 1)\n _ = b }";
    associated_function_generic_order_and_method_contracts_are_preserved_01: true,
        "trait Identity { fn identity[T](self, value: T) T }\nstruct Point {}\nimpl Identity for Point { fn identity[U](self, value: U) U = value }\nfn main() { _ = Point {}.identity::[int](1) }";
    associated_function_generic_order_and_method_contracts_are_preserved_02: false,
        "trait Marker {}\ntrait Identity { fn identity[T](self, value: T) T }\nstruct Point {}\nimpl Identity for Point { fn identity[T: Marker](self, value: T) T = value }\nfn main() {}";
    associated_function_generic_order_and_method_contracts_are_preserved_03: true,
        "trait Create { fn create(value: int) Self }\nstruct Point { value: int }\nimpl Create for Point { fn create(value: int) Self = Self { value } }\nfn main() { let point: Point = Create::create(1)\n _ = point }";
}

#[test]
fn associated_types_do_not_merge_different_trait_identities() {
    assert!(!accepts(
        "trait Left { type Output }\ntrait Right { type Output }\nstruct Point {}\nimpl Left for Point { type Output = int }\nimpl Right for Point { type Output = int }\nfn f(x: Point::Output) {}\nfn main() {}"
    ));
}

#[test]
fn inherent_ownership_and_private_trait_methods_respect_modules() {
    let queries = crate::QueryEngine::new();
    let ownership = super::frontend(
        &[
            (
                "main.gg",
                "use point.{Point}\nimpl Point { fn value(self) int = 1 }\nfn main() {}",
            ),
            ("point.gg", "pub struct Point {}"),
        ],
        &queries,
    )
    .unwrap_err();
    assert!(
        ownership
            .iter()
            .any(|error| error.code() == crate::DiagnosticCode::InvalidDeclaration)
    );
    let visibility = super::frontend(&[("main.gg", "use point.{Point}\nfn main() { _ = Point {}.value() }"), ("point.gg", "pub struct Point {}\ntrait Secret { fn value(self) int }\nimpl Secret for Point { fn value(self) int = 1 }")], &queries).unwrap_err();
    assert!(
        visibility
            .iter()
            .any(|error| error.code() == crate::DiagnosticCode::InvalidType)
    );
    super::frontend(&[("main.gg", "use point.{Point}\nfn main() { _ = Point {}.value() }"), ("point.gg", "pub struct Point {}\npub trait Value { fn value(self) int }\nimpl Value for Point { fn value(self) int = 1 }")], &queries).unwrap();
}

#[test]
fn specialization_dispatch_survives_cached_frontend_results() {
    let source = "trait Value { fn value(self) int }\nstruct Box[T] { v: T }\nimpl Value for Box[T] { fn value(self) int = 1 }\nimpl Value for Box[int] { fn value(self) int = 2 }\nfn main() { let b: Box[int] = Box { v: 1 }\n _ = b.value() }";
    let queries = crate::QueryEngine::new();
    for _ in 0..2 {
        let checked = super::frontend(&[("main.gg", source)], &queries).unwrap();
        let user_module = super::module_index(&checked, "main");
        let dispatch = checked
            .semantics
            .bodies
            .iter()
            .filter(|body| body.definition.module == user_module)
            .flat_map(|body| &body.dispatches)
            .find(|dispatch| dispatch.implementation.is_some())
            .unwrap();
        let callable = dispatch.callable.unwrap();
        let module = &checked.modules[callable.module];
        let crate::frontend::ast::FnBody::Eq(body) =
            module.arena.fns[callable.function as usize].body
        else {
            panic!("示例方法返回常量表达式")
        };
        let model = crate::frontend::semantics::model::Model::new(
            &checked.modules,
            &checked.names,
            &std::collections::BTreeSet::new(),
        )
        .unwrap();
        assert_eq!(model.constant_int(callable.module, body).unwrap(), 2);
    }
}

#[test]
fn associated_constants_retain_non_integer_types_and_specialization_values() {
    assert!(accepts(
        "trait Flags { const ENABLED: bool\n const LABEL: string\n fn enabled(self) bool }\nstruct Point {}\nimpl Flags for Point { const ENABLED = !false\n const LABEL = \"point\" + \"!\"\n fn enabled(self) bool = Self::ENABLED }\nfn main() { let label: string = Point::LABEL\n _ = Point {}.enabled()\n _ = label }"
    ));
    assert!(!accepts(
        "trait Flag { const ENABLED: bool }\nstruct Box[T] { value: T }\nimpl Flag for Box[T] { const ENABLED = true }\nimpl Flag for Box[int] { const ENABLED = false }\nfn main() {}"
    ));
    assert!(!accepts(
        "impl Eq for int { fn eq(self: &Self, other: &Self) bool = true }\nfn main() {}"
    ));
}

accept_cases! {
    user_try_preserves_error_identity_and_records_conversion_calls_00: true,
        "struct Attempt { value: Result[int, string] }\nimpl Try for Attempt { type Value = int\n type Error = string\n fn branch(self) Result[int, string] = self.value\n fn from_value(value: int) Self = Self { value: Ok(value) }\n fn from_error(error: string) Self = Self { value: Err(error) } }\nfn increment(value: Attempt) Attempt = try { value? + 1 }\nfn main() { _ = increment(Attempt { value: Ok(1) }) }";
    user_try_preserves_error_identity_and_records_conversion_calls_01: false,
        "struct Attempt { value: Result[int, string] }\nimpl Try for Attempt { type Value = int\n type Error = string\n fn branch(self) Result[int, string] = self.value\n fn from_value(value: int) Self = Self { value: Ok(value) }\n fn from_error(error: string) Self = Self { value: Err(error) } }\nfn invalid(value: Attempt) Result[int, bool] = Ok(value?)\nfn main() {}";
}

accept_cases! {
    into_iter_checks_iterator_contract_and_item_projection_00: true,
        "struct Counter {}\nimpl Iter for Counter { type Item = int\n fn next(self: &Self) Option[int] = None }\nimpl IntoIter for Counter { type Item = int\n type Iter = Counter\n fn into_iter(self) Counter = self }\nfn main() { for value in (Counter {}) { let n: int = value\n _ = n } }";
    into_iter_checks_iterator_contract_and_item_projection_01: false,
        "struct Counter {}\nimpl Iter for Counter { type Item = int\n fn next(self: &Self) Option[int] = None }\nimpl IntoIter for Counter { type Item = bool\n type Iter = Counter\n fn into_iter(self) Counter = self }\nfn main() {}";
}

accept_cases! {
    associated_items_resolve_trait_qualification_and_forward_aliases_00: true,
        "trait Dim { const N: int\n type A\n type B }\nstruct Point {}\nimpl Dim for Point { const N = 4\n type A = Self::B\n type B = int }\nfn main() { let n: Dim::A = Dim::N\n _ = n }";
    associated_items_resolve_trait_qualification_and_forward_aliases_01: false,
        "trait Dim { type A\n type B }\nstruct Point {}\nimpl Dim for Point { type A = Self::B\n type B = Self::A }\nfn main() {}";
}

/// 跨模块前向别名：同一份 frontend 结果在两份源码上都要解析成功。
#[test]
fn associated_items_resolve_across_modules() {
    let queries = crate::QueryEngine::new();
    super::frontend(&[("main.gg", "use boxes\nfn main() { let b = boxes.Box[int]::new(1)\n let n: int = b.get()\n _ = n }"), ("boxes.gg", "pub struct Box[T] { value: T }\nimpl Box[T] { pub fn new(value: T) Self = Self { value }\n pub fn get(self) T = self.value }")], &queries).unwrap();
}

#[test]
fn generic_iteration_retains_the_into_iter_item_equality() {
    assert!(accepts(
        "struct Counter {}\nimpl Iter for Counter { type Item = int\n fn next(self: &Self) Option[int] = None }\nimpl IntoIter for Counter { type Item = int\n type Iter = Counter\n fn into_iter(self) Counter = self }\nfn consume[T: IntoIter](iterable: T) { for item in iterable { let value: T::Item = item\n _ = value } }\nfn main() { consume(Counter {}) }"
    ));
}

accept_cases! {
    core_prelude_traits_apply_conditionally_00: true,
        "fn show[T: Print + Debug + Eq + Hash + Default](value: T) {}\nfn main() { show(1)\n show(true)\n show((1, true))\n show([1, 2])\n show(Some(1))\n let text = f\"{1}\"\n let hex = f\"{1:x}\"\n _ = text\n _ = hex }";
    core_prelude_traits_apply_conditionally_01: true,
        "fn step(value: Option[int]) Option[int] { let n = value?\n Some(n + 1) }\nfn main() { _ = step(Some(1)) }";
    core_prelude_traits_apply_conditionally_02: true,
        "fn show[T: Print + Debug + Eq + Hash](value: T) {}\nfn main() { let value: Result[int, string] = Ok(1)\n show(value) }";
    core_prelude_traits_apply_conditionally_03: true,
        "struct Bare {}\nfn take[T: Default](value: T) {}\nfn main() { take(Some(Bare {})) }";
    core_prelude_traits_apply_conditionally_04: false,
        "struct Bare {}\nfn take[T: Default](value: T) {}\nfn main() { take(Bare {}) }";
    core_prelude_traits_apply_conditionally_05: true,
        "struct Probe { text: string }\nimpl Error for Probe { fn message(self: &Self) string = self.text\n fn source(self: &Self) Option[&dyn Error] { let missing: Option[&dyn Error] = None\n missing } }\nimpl Print for Probe { fn print(self: &Self, out: &Formatter) {} }\nimpl Debug for Probe { fn debug(self: &Self, out: &Formatter) {} }\nfn main() { let probe = Probe { text: \"x\" }\n _ = probe.message() }";
    core_prelude_traits_apply_conditionally_06: false,
        "fn show[T: Default](value: T) {}\nfn main() { let value: Result[int, string] = Ok(1)\n show(value) }";
    core_prelude_traits_apply_conditionally_07: false,
        "fn hash[T: Hash](value: T) {}\nfn main() { hash(1.0) }";
    core_prelude_traits_apply_conditionally_08: false,
        "fn eq[T: Eq](value: T) {}\nfn main() { eq(Some(1.0)) }";
    core_prelude_traits_apply_conditionally_09: false,
        "fn main() { let text = f\"{true:x}\"\n _ = text }";
    core_prelude_traits_apply_conditionally_10: false,
        "impl Print for int { fn print(self: &Self, out: &Formatter) {} }\nfn main() {}";
}

fn request(source: &str) -> crate::CompileRequest {
    crate::CompileRequest::single_file("main.gg", source, crate::TargetName::X86_64Linux)
}

fn compile(source: &str) -> crate::Compilation {
    crate::Compiler::shared().compile_to_lir(request(source))
}

/// 该源码是否报告了指定诊断代码。
fn reports(source: &str, code: crate::DiagnosticCode) -> bool {
    compile(source)
        .diagnostics()
        .items()
        .iter()
        .any(|diagnostic| diagnostic.code() == code)
}

#[test]
fn unused_must_use_warns_by_default() {
    let warned = compile("fn main() { Some(1); }");
    assert!(warned.is_success());
    assert!(warned.diagnostics().items().iter().any(|diagnostic| {
        diagnostic.code() == crate::DiagnosticCode::UnusedMustUse
            && diagnostic.severity() == crate::Severity::Warning
    }));
}

#[test]
fn unused_must_use_is_absent_when_the_value_is_kept() {
    let kept = compile("fn main() { _ = Some(1)\n let x = Some(1)\n _ = x }");
    assert!(kept.is_success());
    assert!(
        kept.diagnostics()
            .items()
            .iter()
            .all(|diagnostic| diagnostic.code() != crate::DiagnosticCode::UnusedMustUse)
    );
}

#[test]
fn denied_unused_must_use_fails_without_an_image_plan() {
    let denied = crate::Compiler::shared()
        .compile_full_for_test(request("#![deny(unused_must_use)]\nfn main() { Some(1); }"));
    assert!(!denied.is_success());
    assert!(denied.image_plan().is_none());
}

#[test]
fn allowed_unused_must_use_produces_no_lint() {
    let allowed = compile("#![allow(unused_must_use)]\nfn main() { Some(1); }");
    assert!(allowed.is_success());
    assert!(
        allowed
            .diagnostics()
            .items()
            .iter()
            .all(|diagnostic| diagnostic.code() != crate::DiagnosticCode::UnusedMustUse)
    );
}

#[test]
fn must_use_function_result_reports_the_lint() {
    assert!(reports(
        "#[must_use]\nfn answer() int = 1\nfn main() { answer(); }",
        crate::DiagnosticCode::UnusedMustUse
    ));
}

#[test]
fn must_use_struct_value_reports_the_lint() {
    assert!(reports(
        "#[must_use]\nstruct Token {}\nfn main() { Token {}; }",
        crate::DiagnosticCode::UnusedMustUse
    ));
}

#[test]
fn reserved_platform_install_name_is_rejected() {
    assert!(reports(
        "use std.runtime.platform.{install}\nfn main() { _ = install() }",
        crate::DiagnosticCode::ReservedName
    ));
}

#[test]
fn public_std_modules_are_importable() {
    let imported = compile("use std.option\nfn main() {}");
    assert!(
        imported.is_success(),
        "{:?}",
        imported.diagnostics().items()
    );
    assert!(
        imported
            .source_map()
            .snapshots()
            .iter()
            .any(|snapshot| snapshot.logical_path() == "std/option.gg")
    );
}
