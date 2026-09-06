use super::tests::accepts;

#[test]
fn unsafe_calls_require_explicit_blocks_through_function_values() {
    assert!(!accepts("unsafe fn raw() int = 1\nfn main() { _ = raw() }"));
    assert!(!accepts(
        "unsafe fn raw() int = 1\nfn main() { let f = raw\n _ = f() }"
    ));
    assert!(accepts(
        "unsafe fn raw() int = 1\nfn main() { let f = raw\n unsafe { _ = f() } }"
    ));
    assert!(!accepts("unsafe fn raw(p: *int) int = *p\nfn main() {}"));
}

#[test]
fn unsafe_callable_cannot_be_erased_into_safe_fn_contract() {
    assert!(!accepts(
        "unsafe fn raw() int = 1\nfn main() { let f: fn() int = raw\n _ = f() }"
    ));
    assert!(!accepts(
        "unsafe fn raw() int = 1\nfn call[F: Fn() int](f: F) int = f()\nfn main() { unsafe { _ = call(raw) } }"
    ));
    assert!(accepts(
        "unsafe fn raw() int = 1\nfn main() { let f: fn() int = fn() { unsafe { raw() } }\n _ = f() }"
    ));
}

#[test]
fn unsafe_trait_methods_keep_call_contract_after_dyn_erasure() {
    assert!(!accepts(
        "trait Raw { unsafe fn read(self) int }\nstruct R {}\nimpl Raw for R { unsafe fn read(self) int = 1 }\nfn main() { let r: dyn Raw = R {}\n _ = r.read() }"
    ));
    assert!(accepts(
        "trait Raw { unsafe fn read(self) int }\nstruct R {}\nimpl Raw for R { unsafe fn read(self) int = 1 }\nfn main() { let r: dyn Raw = R {}\n unsafe { _ = r.read() } }"
    ));
    assert!(accepts(
        "unsafe trait Valid { fn read(self) int }\nstruct R {}\nunsafe impl Valid for R { fn read(self) int = 1 }\nfn main() { _ = R {}.read() }"
    ));
}

#[test]
fn maybe_uninit_has_explicit_assume_init_and_no_type_id() {
    assert!(accepts(
        "use std.mem.{MaybeUninit}\nfn main() { let value: MaybeUninit[int] = MaybeUninit::uninit()\n value.write(4)\n unsafe { let n: int = value.assume_init()\n _ = n } }"
    ));
    assert!(!accepts(
        "use std.mem.{MaybeUninit}\nfn main() { let value = MaybeUninit::new(1)\n _ = value.assume_init() }"
    ));
    assert!(!accepts(
        "use std.mem.{MaybeUninit}\nfn main() { _ = type_id[MaybeUninit[int]]() }"
    ));
    assert!(!accepts(
        "use std.mem.{MaybeUninit}\nfn main() { let value: dyn Any = MaybeUninit::new(1)\n _ = value }"
    ));
}

#[test]
fn pointer_intrinsics_preserve_place_and_management_contracts() {
    assert!(accepts(
        "use std.ptr as ptr\nfn main() { let n: int\n let p = ptr.addr_of(n)\n unsafe { ptr.ptr_write(p, 2)\n let value: int = ptr.read_unaligned(p)\n ptr.volatile_store(p, value)\n _ = ptr.volatile_load(p) } }"
    ));
    assert!(!accepts("use std.ptr\nfn main() { _ = ptr.addr_of(1) }"));
    assert!(!accepts(
        "use std.ptr\nfn bad(p: *int) int = ptr.ptr_read(p)\nfn main() {}"
    ));
    assert!(!accepts(
        "use std.ptr\nfn bad(p: *string) string = unsafe { ptr.ptr_read(p) }\nfn main() {}"
    ));
}

#[test]
fn transmute_checks_size_and_rejects_managed_values() {
    assert!(accepts(
        "use std.mem.{transmute as bits}\nfn main() { unsafe { let n: uint = bits::[int, uint](1)\n _ = n } }"
    ));
    assert!(!accepts(
        "use std.mem.{transmute}\nfn main() { unsafe { _ = transmute::[int, u8](1) } }"
    ));
    assert!(!accepts(
        "use std.mem.{transmute}\nfn bad(value: string) (uint, uint) = unsafe { transmute(value) }\nfn main() {}"
    ));
}

#[test]
fn c_abi_declarations_accept_the_c_string_value() {
    assert!(accepts("extern \"C\" fn raw(p: *int) int\nfn main() {}"));
    assert!(accepts(
        "unsafe extern \"C\" fn raw(p: *int) int = unsafe { *p }\nfn main() {}"
    ));
    assert!(accepts("extern raw\"C\" fn raw(p: *int) int\nfn main() {}"));
}

#[test]
fn union_construction_selects_one_bit_field_and_access_requires_unsafe() {
    assert!(accepts(
        "union Word { i: int\n f: float }\nfn main() { let word = Word { i: 1 }\n unsafe { word.f = 2.0\n _ = word.i } }"
    ));
    assert!(!accepts(
        "union Word { i: int\n f: float }\nfn main() { let word = Word { i: 1, f: 1.0 }\n _ = word }"
    ));
    assert!(!accepts(
        "union Word { i: int }\nfn main() { let word = Word { i: 1 }\n _ = word.i }"
    ));
    assert!(!accepts("union Word { text: string }\nfn main() {}"));
}

#[test]
fn c_signatures_reject_managed_and_non_c_values() {
    assert!(accepts(
        "#[repr(C)] struct Pair { n: int\n flag: bool }\nextern \"C\" fn pass(value: Pair) Pair\nfn main() {}"
    ));
    assert!(!accepts(
        "struct Pair { n: int }\nextern \"C\" fn pass(value: Pair) Pair\nfn main() {}"
    ));
    assert!(!accepts(
        "#[repr(C)] struct Pair { p: &int }\nextern \"C\" fn pass(value: Pair) Pair\nfn main() {}"
    ));
    assert!(!accepts(
        "extern \"C\" fn pass(value: TypeId) TypeId\nfn main() {}"
    ));
    assert!(!accepts(
        "extern \"C\" fn pass(value: [int; 2])\nfn main() {}"
    ));
    assert!(!accepts(
        "extern \"C\" fn pass(...values: &[int])\nfn main() {}"
    ));
}

#[test]
fn windows_c_signature_rejects_nested_128_bit_values() {
    use crate::{CompileRequest, Compiler, TargetName};
    let source = "#[repr(C)] struct Wide { value: i128 }\nextern \"C\" fn pass(value: Wide) Wide\nfn main() {}";
    for target in [TargetName::X86_64Linux, TargetName::X86_64Windows] {
        let result =
            Compiler::new().compile(CompileRequest::single_file("main.gg", source, target));
        assert_eq!(result.is_success(), target == TargetName::X86_64Linux);
    }
}

#[test]
fn repr_layouts_control_bit_reinterpretation() {
    assert!(accepts(
        "use std.mem.{transmute}\n#[repr(packed)] struct Packed { byte: byte\n value: int }\n#[repr(u32)] enum Tag { A, B }\nfn main() { unsafe { let raw: [byte; 9] = transmute(Packed { byte: 1, value: 2 })\n let tag: u32 = transmute(Tag::A)\n _ = raw\n _ = tag } }"
    ));
    assert!(!accepts(
        "#[repr(packed)] struct Packed { text: string }\nfn main() {}"
    ));
    assert!(!accepts(
        "#[repr(align(3))] struct Wrong { value: int }\nfn main() {}"
    ));
    assert!(!accepts(
        "#[repr(transparent)] struct Wrong { a: int\n b: int }\nfn main() {}"
    ));
}

#[test]
fn managed_asm_proves_forward_control_flow_and_restored_stack() {
    assert!(accepts(
        "const BODY = \"push %rax; jmp 1f; nop; 1: pop %rax\"\nfn main() { unsafe { asm(BODY, clobber(\"rax\")) } }"
    ));
    for template in [
        "1: pause; jnz 1b",
        "notrack jmp *%rax",
        ".byte 0xc3",
        "syscall",
        "jnz 1f; push %rax; 1: nop",
    ] {
        assert!(!accepts(&format!(
            "fn main() {{ unsafe {{ asm(\"{template}\") }} }}"
        )));
    }
    assert!(!accepts("fn main() { asm(\"nop\") }"));
    assert!(!accepts("fn main() { unsafe { asm(1) } }"));
}

#[test]
fn asm_outputs_initialize_slots_after_input_evaluation() {
    assert!(accepts(
        "fn main() { let sum: int\n unsafe { asm(\"add %rsi, %rax\", in(\"rax\") 1, in(\"rsi\") 2, lateout(\"rax\") sum, clobber(\"cc\")) }\n _ = sum }"
    ));
    assert!(!accepts(
        "fn main() { let sum: int\n unsafe { asm(\"nop\", out(\"rax\") sum, in(\"rdi\") sum) } }"
    ));
    assert!(!accepts(
        "fn main() { let sum: int\n unsafe { asm(\"nop\", in(\"rax\") 1, out(\"eax\") sum) } }"
    ));
    assert!(!accepts(
        "fn main() { unsafe { asm(\"nop\", out(\"rax\") 1) } }"
    ));
}

#[test]
fn naked_requires_one_asm_and_an_unsafe_c_definition() {
    assert!(accepts(
        "#[naked] unsafe extern \"C\" fn identity(n: int) int = asm(\"mov %rdi, %rax; ret\")\nfn main() { unsafe { _ = identity(1) } }"
    ));
    assert!(accepts(
        "global_asm(\".text\\n\" + \"custom: ret\")\nfn main() {}"
    ));
    assert!(!accepts(
        "#[naked] extern \"C\" fn invalid() = asm(\"ret\")\nfn main() {}"
    ));
    assert!(!accepts(
        "#[naked] unsafe fn invalid() = asm(\"ret\")\nfn main() {}"
    ));
    assert!(!accepts(
        "#[naked] unsafe extern \"C\" fn invalid() { let n = 1\n asm(\"ret\") }\nfn main() {}"
    ));
}

#[test]
fn direct_foreign_effects_obey_callsite_precedence() {
    use super::{foreign::ForeignEffect, tests::frontend};
    let source = "#[ffi(leaf(stack = 17))] extern \"C\" fn leaf(n: int) int\n#[ffi(dirty_cpu)] extern \"C\" fn dirty(n: int) int\nextern \"C\" fn ordinary(n: int) int\nfn main() { _ = leaf(1)\n _ = #[ffi(bridge)] leaf(1)\n _ = #[ffi(dirty_cpu)] ordinary(1)\n _ = dirty(1) }";
    let queries = crate::QueryEngine::new();
    let cold = frontend(&[("main.gg", source)], &queries).unwrap();
    let warm = frontend(&[("main.gg", source)], &queries).unwrap();
    assert_eq!(cold.semantics, warm.semantics);
    assert_eq!(
        warm.semantics
            .bodies
            .iter()
            .flat_map(|body| body.foreign_calls.iter().map(|call| call.effect))
            .collect::<Vec<_>>(),
        [
            ForeignEffect::Leaf { stack: 32 },
            ForeignEffect::Bridge,
            ForeignEffect::DirtyCpu,
            ForeignEffect::DirtyCpu
        ]
    );
    assert!(!accepts(
        "extern \"C\" fn plain()\nfn main() { #[ffi(leaf)] plain() }"
    ));
    assert!(!accepts(
        "fn plain() {}\nfn main() { #[ffi(bridge)] plain() }"
    ));
    assert!(!accepts(
        "#[ffi(bridge)] extern \"C\" fn plain()\nfn main() {}"
    ));
}

#[test]
fn dirty_native_definitions_exclude_managed_operations() {
    assert!(accepts(
        "#[ffi(dirty_cpu)] unsafe extern \"C\" fn work(n: int) int { unsafe { asm(\"1: pause; jnz 1b\") }\n let result = n\n result /= 2\n result <<= 1\n result }\nfn main() {}"
    ));
    assert!(!accepts(
        "#[ffi(dirty_cpu)] extern \"C\" fn work() {}\nfn main() {}"
    ));
    assert!(!accepts(
        "#[ffi(dirty_cpu)] unsafe extern \"C\" fn work() { yield }\nfn main() {}"
    ));
    assert!(!accepts(
        "#[ffi(dirty_cpu)] unsafe extern \"C\" fn work() { let text = \"managed\"\n _ = text }\nfn main() {}"
    ));
    assert!(!accepts(
        "fn managed() int = 1\n#[ffi(dirty_cpu)] unsafe extern \"C\" fn work() int = managed()\nfn main() {}"
    ));
    assert!(!accepts(
        "#[ffi(dirty_cpu)] unsafe extern \"C\" fn work(n: int) int = 1 / n\nfn main() {}"
    ));
    assert!(!accepts(
        "#[ffi(dirty_cpu)] unsafe extern \"C\" fn work() { panic(\"no unwind\") }\nfn main() {}"
    ));
}

#[test]
fn asm_registers_cannot_truncate_values_or_forge_managed_payloads() {
    assert!(accepts(
        "fn main() { let value: i32\n unsafe { asm(\"nop\", in(\"eax\") 1, lateout(\"eax\") value) }\n _ = value }"
    ));
    assert!(!accepts(
        "fn main() { let value: i128\n unsafe { asm(\"nop\", out(\"rax\") value) } }"
    ));
    assert!(!accepts(
        "fn main() { let value: string\n unsafe { asm(\"nop\", out(\"xmm0\") value) } }"
    ));
    assert!(!accepts(
        "fn main() { unsafe { asm(\"nop\", clobber(\"rsp\")) } }"
    ));
}

#[test]
fn pointer_type_constructors_preserve_reference_obligations() {
    assert!(accepts(
        "type Raw = *int\nfn main() { let n = 1\n let p: Raw = (*int)(&n)\n let bytes = (*byte)(p)\n let address: uint = uint(bytes)\n let back = Raw(address)\n unsafe { let value: &int = (&int)(back)\n _ = *value } }"
    ));
    assert!(!accepts(
        "fn main() { let n = 1\n let p = (*int)(&n)\n _ = (&int)(p) }"
    ));
    assert!(!accepts("fn main() { let n = 1\n _ = (&uint)(&n) }"));
    assert!(accepts(
        "fn main() { let a = [1, 2]\n let view = (&[int])(&a)\n let array = (*[int; 2])(&a)\n unsafe { let again: &[int] = (&[int])(array)\n _ = again }\n _ = view }"
    ));
    assert!(!accepts(
        "fn main() { unsafe { let address = (*int)(0)\n _ = (&[int])(address) } }"
    ));
    assert!(accepts(
        "type Callback = fn(int) int\nfn pointer(p: *Callback) *fn(int) int = (*fn(int) int)(p)\nfn main() {}"
    ));
}

#[test]
fn pointer_constructor_syntax_does_not_erase_function_reference_captures() {
    assert!(accepts(
        "fn main() { let n = 1\n let f = fn() = n\n let p = &f\n _ = (*p)()\n _ = (*&f)() }"
    ));
    assert!(!accepts(
        "fn main() { let n: int\n let f = fn() = n\n let p = &f\n _ = (*p)() }"
    ));
    assert!(!accepts(
        "unsafe fn raw() int = 1\nfn main() { let f = raw\n let p = &f\n _ = (*p)() }"
    ));
}

#[test]
fn packed_fields_cannot_form_unaligned_references() {
    assert!(!accepts(
        "#[repr(packed)] struct Packed { byte: byte\n value: int }\nfn bad(p: &Packed) &int = &p.value\nfn main() {}"
    ));
    assert!(!accepts(
        "#[repr(packed)] struct Packed { byte: byte\n value: int }\nfn bad(p: &Packed) &int = unsafe { &p.value }\nfn main() {}"
    ));
    assert!(!accepts(
        "struct Value { n: int }\nimpl Value { fn read(self: &Self) int = self.n }\n#[repr(packed)] struct Packed { byte: byte\n value: Value }\nfn bad(p: &Packed) int = p.value.read()\nfn main() {}"
    ));
}

#[test]
fn aligned_projections_and_unaligned_intrinsics_remain_valid() {
    assert!(accepts(
        "use std.ptr\n#[repr(packed)] struct Packed { byte: byte\n value: int }\nfn read(p: &Packed) int = unsafe { ptr.read_unaligned(ptr.addr_of(p.value)) }\nfn main() {}"
    ));
    assert!(accepts(
        "#[repr(packed, align(8))] struct Packed { value: int\n byte: byte }\nfn read(p: &[Packed; 2], i: int) &int = &p[i].value\nfn main() {}"
    ));
    assert!(accepts(
        "fn read(p: *int) &int = unsafe { &*p }\nfn main() {}"
    ));
}

#[test]
fn linkage_attributes_validate_owner_and_target_section() {
    assert!(accepts(
        "#[link_section = \".data\"] #[used] static VALUE: int = 1\n#[link_section = \".text\"] global_asm(\"custom: ret\")\n#[export_name = \"entry\"] fn main() {}"
    ));
    assert!(!accepts("#[used] struct Wrong {}\nfn main() {}"));
    assert!(!accepts("#[link_name = \"bad\"] fn main() {}"));
    assert!(!accepts(
        "#[link_section = \".gugu.stackmap\"] static VALUE: int = 1\nfn main() {}"
    ));
    assert!(!accepts(
        "#[link_section = \"\"] static VALUE: int = 1\nfn main() {}"
    ));
    use crate::{CompileRequest, Compiler, TargetName};
    let source = "#[link_section = \".text.custom\"] fn main() {}";
    for target in [TargetName::X86_64Linux, TargetName::X86_64Windows] {
        let result =
            Compiler::new().compile(CompileRequest::single_file("main.gg", source, target));
        assert_eq!(result.is_success(), target == TargetName::X86_64Linux);
    }
}

#[test]
fn conditional_cleanup_retains_the_captured_callable_slot() {
    assert!(accepts(
        "fn work(flag: bool) { let n: int\n if flag { let call = fn() = n\n defer ret { _ = call() }\n n = 1 } }\nfn main() {}"
    ));
    assert!(!accepts(
        "fn work(flag: bool) { let n: int\n if flag { let call = fn() = n\n defer ret { _ = call() } } }\nfn main() {}"
    ));
    assert!(accepts(
        "fn work(flag: bool) { let n: int\n let call: fn() int = fn() = 0\n if flag { defer ret { _ = call() }\n call = fn() = n\n n = 1 } }\nfn main() {}"
    ));
    assert!(!accepts(
        "fn work(flag: bool) { let n: int\n let call: fn() int = fn() = 0\n if flag { defer ret { _ = call() }\n call = fn() = n } }\nfn main() {}"
    ));
}
