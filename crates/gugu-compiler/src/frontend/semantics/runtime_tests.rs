use super::tests::accepts;

#[test]
fn runtime_publish_resolves_imports_and_canonical_paths() {
    assert!(accepts(
        "use std.runtime.{ownership_publish, root_publish}\nfn main() { let a = chan[int](0)\n let b = chan[int](0)\n ownership_publish(a, b)\n root_publish(b, a) }"
    ));
    assert!(accepts(
        "fn main() { let a = chan[int](0)\n let b = chan[int](0)\n std.runtime.ownership_publish(a, b) }"
    ));
}

#[test]
fn runtime_publish_requires_destination_and_value() {
    assert!(!accepts(
        "use std.runtime.{ownership_publish}\nfn main() { let a = chan[int](0)\n ownership_publish(a) }"
    ));
}

#[test]
fn runtime_publish_rejects_temporary_destination() {
    assert!(!accepts(
        "use std.runtime.{ownership_publish}\nfn main() { let a = chan[int](0)\n ownership_publish(chan[int](0), a) }"
    ));
}

#[test]
fn runtime_publish_rejects_aggregate_values() {
    assert!(!accepts(
        "use std.runtime.{ownership_publish}\nstruct Holder { value: int }\nfn main() { let a = Holder { value: 1 }\n let b = Holder { value: 2 }\n ownership_publish(a, b) }"
    ));
}

#[test]
fn runtime_publish_checks_explicit_handle_type() {
    assert!(accepts(
        "use std.runtime.{root_publish as publish}\nfn main() { let a = chan[int](0)\n let b = chan[int](0)\n publish::[chan[int]](a, b) }"
    ));
    assert!(!accepts(
        "use std.runtime.{root_publish}\nfn main() { let a = chan[int](0)\n let b = chan[int](0)\n root_publish::[chan[int], chan[int]](a, b) }"
    ));
    assert!(!accepts(
        "use std.runtime.{root_publish}\nfn main() { let a = chan[int](0)\n let b = chan[bool](0)\n root_publish(a, b) }"
    ));
}
