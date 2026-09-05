use super::tests::{accepts, frontend};

#[test]
fn apit_occurrences_have_independent_types_and_enforce_bounds() {
    assert!(accepts(
        "trait Value { fn value(self) int }\nstruct A {}\nstruct B {}\nimpl Value for A { fn value(self) int = 1 }\nimpl Value for B { fn value(self) int = 2 }\nfn sum(a: impl Value, b: impl Value) int = a.value() + b.value()\nfn main() { _ = sum(A {}, B {}) }"
    ));
    assert!(!accepts(
        "trait Value { fn value(self) int }\nfn read(value: impl Value) int = value.value()\nfn main() { _ = read(true) }"
    ));
    assert!(accepts(
        "fn twice(f: impl Fn(int) int, value: int) int = f(f(value))\nfn main() { _ = twice(fn(x) = x + 1, 2) }"
    ));
}

#[test]
fn rpit_and_tait_preserve_hidden_callable_identity() {
    assert!(accepts(
        "fn adder(n: int) impl Fn(int) int = fn(x) = x + n\ntype Identity = impl Fn(int) int\nfn identity() Identity = fn(x) = x\nstruct Holder { callback: Identity }\nfn main() { let f = adder(2)\n _ = f(3)\n let h = Holder { callback: identity() }\n _ = h.callback(4) }"
    ));
    assert!(!accepts(
        "trait Value {}\nstruct A {}\nstruct B {}\nimpl Value for A {}\nimpl Value for B {}\nfn make(flag: bool) impl Value = if flag { A {} } else { B {} }\nfn main() {}"
    ));
    assert!(!accepts(
        "type Hidden = impl Clone\nfn one() Hidden = 1\nfn two() Hidden = true\nfn main() {}"
    ));
    assert!(!accepts("type Hidden = impl Clone\nfn main() {}"));
}

#[test]
fn rpit_boundaries_do_not_reveal_fields_or_accept_concrete_expectations() {
    let queries = crate::QueryEngine::new();
    frontend(&[("main.gg", "use factory\nfn main() { let value = factory.make()\n _ = value.value() }"), ("factory.gg", "pub trait Value { fn value(self) int }\nstruct Point { x: int }\nimpl Value for Point { fn value(self) int = self.x }\npub fn make() impl Value = Point { x: 1 }")], &queries).unwrap();
    assert!(frontend(&[("main.gg", "use factory\nfn main() { let value = factory.make()\n _ = value.x }"), ("factory.gg", "pub trait Value { fn value(self) int }\nstruct Point { pub x: int }\nimpl Value for Point { fn value(self) int = self.x }\npub fn make() impl Value = Point { x: 1 }")], &queries).is_err());
    assert!(!accepts(
        "trait Value {}\nstruct Point {}\nimpl Value for Point {}\nfn make() impl Value = Point {}\nfn main() { let point: Point = make()\n _ = point }"
    ));
}

#[test]
fn opaque_types_are_rejected_in_forbidden_positions() {
    assert!(!accepts(
        "extern \"C\" fn exported(value: impl Clone)\nfn main() {}"
    ));
    assert!(!accepts("union U { value: impl Clone }\nfn main() {}"));
    assert!(!accepts(
        "fn main() { let c: chan[impl Clone] = chan[int](1)\n _ = c }"
    ));
    assert!(!accepts(
        "fn main() { let value: impl Clone = 1\n _ = value }"
    ));
}

#[test]
fn trait_return_opaque_is_per_implementation_and_not_object_safe() {
    assert!(accepts(
        "trait Value { fn value(self) int }\nstruct Point {}\nimpl Value for Point { fn value(self) int = 1 }\ntrait Factory { fn make(self) impl Value }\nstruct Maker {}\nimpl Factory for Maker { fn make(self) impl Value = Point {} }\nfn main() { _ = Maker {}.make().value() }"
    ));
    assert!(!accepts(
        "trait Value {}\ntrait Factory { fn make(self) impl Value }\nfn invalid(factory: dyn Factory) {}\nfn main() {}"
    ));
}

#[test]
fn dyn_erasure_dispatches_only_object_safe_interfaces() {
    assert!(accepts(
        "trait Value { fn value(self: &Self) int }\nstruct Point { value: int }\nimpl Value for Point { fn value(self: &Self) int = self.value }\nfn read(value: dyn Value) int = value.value()\nfn main() { let object: dyn Value = Point { value: 2 }\n _ = read(object) }"
    ));
    assert!(!accepts(
        "trait Assoc { type Item\n fn value(self) int }\nfn invalid(value: dyn Assoc) {}\nfn main() {}"
    ));
    assert!(!accepts(
        "trait Generic { fn identity[T](self, value: T) T }\nfn invalid(value: dyn Generic) {}\nfn main() {}"
    ));
    assert!(!accepts(
        "trait Static { fn create() int }\nfn invalid(value: dyn Static) {}\nfn main() {}"
    ));
    assert!(!accepts("fn invalid(value: dyn Eq) {}\nfn main() {}"));
    assert!(!accepts("fn invalid(value: dyn Fn) {}\nfn main() {}"));
}

#[test]
fn any_downcasts_are_typed_and_never_peel_trait_objects() {
    assert!(accepts(
        "trait Value { fn value(self) int }\nstruct Point {}\nimpl Value for Point { fn value(self) int = 1 }\nfn main() { let object: dyn Value = Point {}\n let erased: dyn Any = object\n let same: dyn Any = erased\n let object_slot: Option[&dyn Value] = same.downcast()\n let point_slot: Option[&Point] = same.downcast::[Point]()\n let copied: Option[Point] = same.downcast_copy()\n let found: bool = same.is::[Point]()\n let id: TypeId = same.type_of()\n _ = id.name()\n _ = id.as_int()\n _ = object_slot\n _ = point_slot\n _ = copied\n _ = found }"
    ));
    assert!(!accepts(
        "trait Value { fn value(self) int }\nfn invalid(value: dyn Value) { _ = value.downcast::[int]() }\nfn main() {}"
    ));
    assert!(!accepts("fn main() { let value: any = 1\n _ = value }"));
}

#[test]
fn generic_opaque_instances_preserve_declared_type_parameters() {
    assert!(accepts(
        "fn identity[T: Clone](value: T) impl Clone = value\nfn anonymous(value: impl Clone) impl Clone = value\nfn main() { _ = identity(1).clone()\n _ = identity(true).clone()\n _ = anonymous(1).clone()\n _ = anonymous(true).clone() }"
    ));
    assert!(accepts(
        "trait Factory { type F\n fn make(self) Self::F }\nstruct Maker {}\nimpl Factory for Maker { type F = impl Fn(int) int\n fn make(self) Self::F = fn(x) = x }\nfn main() { let f = Maker {}.make()\n _ = f(1) }"
    ));
    assert!(!accepts(
        "type Hidden = impl Clone\nfn hide[T: Clone](value: T) Hidden = value\nfn main() {}"
    ));
}

#[test]
fn trait_apit_contract_cannot_strengthen_parameter_bounds() {
    assert!(accepts(
        "trait Reader { fn read(self, value: impl Clone) }\nstruct R {}\nimpl Reader for R { fn read(self, value: impl Clone) { _ = value.clone() } }\nfn main() { R {}.read(1)\n R {}.read(true) }"
    ));
    assert!(!accepts(
        "trait Reader { fn read(self, value: impl Clone) }\nstruct R {}\nimpl Reader for R { fn read(self, value: impl Eq) {} }\nfn main() {}"
    ));
}

#[test]
fn opaque_callable_erasure_retains_capture_initialization_checks() {
    assert!(accepts(
        "fn make(n: int) impl Fn(int) int = fn(x) = x + n\nfn main() { let f: fn(int) int = make(1)\n _ = f(2) }"
    ));
    assert!(!accepts(
        "fn make() impl Fn() int { let n: int\n return fn() = n }\nfn main() {}"
    ));
}

#[test]
fn opaque_into_iter_exposes_associated_iterator_requirements() {
    assert!(accepts(
        "struct Counter {}\nimpl Iter for Counter { type Item = int\n fn next(self: &Self) Option[int] = None }\nimpl IntoIter for Counter { type Item = int\n type Iter = Counter\n fn into_iter(self) Self::Iter = self }\nfn drain(values: impl IntoIter) { for value in values { _ = value } }\nfn make() impl IntoIter = Counter {}\nfn main() { drain(Counter {})\n for value in make() { _ = value } }"
    ));
}

#[test]
fn opaque_callable_satisfies_named_fn_bounds_without_erasure() {
    assert!(accepts(
        "fn make() impl Fn(int) int = fn(x) = x\nfn call[F: Fn(int) int](f: F) int = f(1)\nfn main() { _ = call(make()) }"
    ));
}

#[test]
fn type_id_is_distinct_from_integer_and_rejects_never() {
    assert!(accepts(
        "fn main() { let id: TypeId = type_id[int]()\n let same: bool = id == type_id[int]()\n let ordered: bool = id < type_id[bool]()\n let count: int = type_id_count()\n _ = id.name()\n _ = id.as_int()\n _ = same\n _ = ordered\n _ = count }"
    ));
    assert!(!accepts("fn main() { let id: TypeId = 1\n _ = id }"));
    assert!(!accepts("fn main() { _ = type_id[!]() }"));
}

#[test]
fn any_plans_preserve_exact_payload_across_cached_queries() {
    use super::model::{Model, Ty};
    use super::output::ReflectionKind;
    let source = "trait Value { fn value(self) int }\nstruct Point {}\nimpl Value for Point { fn value(self) int = 1 }\nfn main() { let object: dyn Value = Point {}\n let erased: dyn Any = object\n let same: dyn Any = erased\n let restored: Option[&dyn Value] = same.downcast()\n let point: Option[Point] = same.downcast_copy()\n _ = restored\n _ = point }";
    let queries = crate::QueryEngine::new();
    let cold = frontend(&[("main.gg", source)], &queries).unwrap();
    let warm = frontend(&[("main.gg", source)], &queries).unwrap();
    assert_eq!(cold.semantics, warm.semantics);
    let model = Model::new(&warm.modules, &warm.names).unwrap();
    let main = warm
        .semantics
        .bodies
        .iter()
        .find(|body| body.erasures.len() == 2)
        .unwrap();
    let [object, erased] = main.erasures.as_slice() else {
        panic!("仅具体值和接口值进入不同擦除容器");
    };
    assert_eq!(object.target, erased.source);
    assert!(
        matches!(&erased.source, Ty::Dyn(bounds) if model.traits.interfaces[bounds[0].id].name == "Value")
    );
    assert_eq!(
        main.reflections
            .iter()
            .map(|plan| &plan.kind)
            .collect::<Vec<_>>(),
        [
            &ReflectionKind::Downcast(erased.source.clone()),
            &ReflectionKind::DowncastCopy(object.source.clone()),
        ]
    );
    let mut corrupted = warm.semantics.clone();
    let reflection = corrupted
        .bodies
        .iter_mut()
        .flat_map(|body| &mut body.reflections)
        .next()
        .unwrap();
    reflection.kind = ReflectionKind::Downcast(Ty::Never);
    assert!(corrupted.verify(&model).is_err());
}

#[test]
fn object_safety_checks_self_inside_nested_dynamic_arguments() {
    assert!(accepts(
        "trait Container[T] { fn hold(self, value: T) }\nfn valid(value: dyn Container[int]) { value.hold(1) }\nfn main() {}"
    ));
    assert!(!accepts(
        "trait Container[T] { fn hold(self, value: T) }\ntrait Bad { fn hold(self, value: dyn Container[Self]) }\nfn invalid(value: dyn Bad) {}\nfn main() {}"
    ));
}
