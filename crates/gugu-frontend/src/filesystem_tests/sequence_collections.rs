use super::{
    CompileRequest, Compiler, TargetName,
    harness::{Harness, word},
};

#[test]
fn vec_builtin_index_requires_standard_identity_and_keeps_helpers_private() {
    for (source, code, message) in [
        (
            include_str!("fixtures/vec_index_private.gg"),
            crate::DiagnosticCode::InvalidType,
            "不能访问私有固有方法",
        ),
        (
            include_str!("fixtures/vec_index_identity.gg"),
            crate::DiagnosticCode::ReservedName,
            "声明名 `Vec` 是保留的预导入名称",
        ),
    ] {
        let compiled = Compiler::shared().compile_sema_for_test(CompileRequest::single_file(
            "main.gg",
            source,
            TargetName::X86_64Linux,
        ));
        assert!(!compiled.is_success());
        assert!(
            compiled
                .diagnostics()
                .items()
                .iter()
                .any(|diagnostic| diagnostic.code() == code
                    && diagnostic.message().contains(message)),
            "{:?}",
            compiled.diagnostics().items()
        );
    }
}

#[test]
fn vec_builtin_index_reads_writes_and_checks_bounds() {
    let compiled = Compiler::shared().compile_to_lir(CompileRequest::single_file(
        "main.gg",
        include_str!("fixtures/vec_index.gg"),
        TargetName::X86_64Linux,
    ));
    assert!(
        compiled.is_success(),
        "{:?}",
        compiled.diagnostics().items()
    );
    let mut harness = Harness::new(&compiled);
    let mut memory = vec![0; 65536];
    assert_eq!(
        harness.invoke("main.gg", "vec_index_case", &[], &mut memory),
        [1]
    );
    for entry in ["vec_index_read", "vec_index_write"] {
        for index in [-1_i64, 1] {
            harness.reset_for_next_call(&mut memory);
            let error = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                harness.invoke("main.gg", entry, &[index as u64], &mut memory);
            }))
            .expect_err("负数与越界的 Vec 下标必须 panic");
            let message = error.downcast_ref::<String>().expect("解释器 panic 消息");
            assert!(message.contains("Vec 下标越界"), "{entry}: {message}");
        }
    }
}

#[test]
fn nominal_const_trait_impls_match_and_reject_crossing_patterns() {
    for (accepted, source) in [
        (
            true,
            "trait Value { type Item\n fn value(self) Self::Item }\nstruct Block[T, comptime N: int] { data: [T; N] }\nimpl[T, comptime N: int] Value for Block[T, N] { type Item = [T; N]\n fn value(self) [T; N] = self.data }\nfn main() { let b = Block[int, 2] { data: [3, 4] }\n let values: [int; 2] = b.value()\n _ = values }",
        ),
        (
            false,
            "trait Marker {}\nstruct Pair[comptime A: int, comptime B: int] {}\nimpl[comptime N: int] Marker for Pair[N, 2] {}\nimpl[comptime M: int] Marker for Pair[1, M] {}\nfn main() {}",
        ),
        (
            true,
            "trait Marker {}\nstruct Pair[comptime A: int, comptime B: int] {}\nimpl[comptime N: int] Marker for Pair[N, N] {}\nimpl Marker for Pair[1, 2] {}\nfn main() {}",
        ),
    ] {
        let compiled = Compiler::shared().compile_sema_for_test(CompileRequest::single_file(
            "main.gg",
            source,
            TargetName::X86_64Linux,
        ));
        assert_eq!(
            compiled.is_success(),
            accepted,
            "{:?}",
            compiled.diagnostics().items()
        );
    }
}

#[test]
fn sequence_collections_share_identity_and_preserve_snapshots() {
    let compiled = Compiler::shared().compile_to_lir(CompileRequest::single_file(
        "main.gg",
        include_str!("fixtures/sequence_collections.gg"),
        TargetName::X86_64Linux,
    ));
    assert!(
        compiled.is_success(),
        "{:?}",
        compiled.diagnostics().items()
    );
    let mut harness = Harness::new(&compiled);
    let mut memory = vec![0; 262144];
    for entry in [
        "deque_case",
        "heap_case",
        "bitset_case",
        "smallvec_case",
        "list_case",
    ] {
        harness.reset_for_next_call(&mut memory);
        assert_eq!(
            harness.invoke("main.gg", entry, &[], &mut memory),
            [1],
            "{entry}"
        );
    }
    // 公开 cursor 必须继续报告 stale，但不能因此让已经删除的 payload 留在 GC 可达图中。
    // ListCursor 的三字段为 owner、node、generation，Node 的首字段为可清空 payload 引用。
    // Option 的活动变体编号为 Some=0、None=1，与 HIR 和 GC trace descriptor 一致。
    for removed in [false, true] {
        harness.reset_for_next_call(&mut memory);
        let cursor = harness.invoke(
            "main.gg",
            "list_payload_cursor",
            &[u64::from(removed)],
            &mut memory,
        );
        let [_, node, _] = cursor.as_slice() else {
            panic!("ListCursor 布局")
        };
        assert_eq!(
            word(&memory, *node),
            u64::from(removed),
            "删除必须清空 payload 的活动变体"
        );
        if !removed {
            let payload = word(&memory, *node + 8);
            assert_eq!(word(&memory, word(&memory, payload)), 7345);
        }
    }
    harness.reset_for_next_call(&mut memory);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        harness.invoke("main.gg", "list_view_write", &[], &mut memory);
    }));
    let panic = result.unwrap_err();
    let message = panic.downcast_ref::<String>().expect("解释器 panic 消息");
    assert!(
        message.contains("LinkedList 不能在 scoped view 内修改"),
        "{message}"
    );
}
