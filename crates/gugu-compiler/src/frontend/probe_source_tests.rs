//! 探针源安装规则与 artifact 探针源集合的一致性。
//!
//! 规则本身是「探针源只在探针编译安装」，探针集合由 artifact 源表与 `is_probe_builtin_source`
//! 共同定义。这里把两侧钉在一起：builder 增删探针源而规则没跟上时立即失败，而不是等到某个
//! 编译静默多编一份 `main` 或把探针库的诊断混进用户诊断。

use super::{installs_builtin_source, is_probe_builtin_source};
use crate::runtime::{RuntimeResources, RuntimeSource};

/// 探针源恰好是探针库与探针入口程序两个。
#[test]
fn probe_builtin_sources_are_the_probe_library_and_entry() {
    let resources = RuntimeResources::builtin();
    let mut probe: Vec<&str> = resources
        .sources()
        .iter()
        .map(RuntimeSource::logical_path)
        .filter(|path| is_probe_builtin_source(path))
        .collect();
    probe.sort_unstable();
    assert_eq!(
        probe,
        [
            "std/runtime/fixtures/gc_image.gg",
            "std/runtime/gc_probes.gg"
        ]
    );
}

/// 探针源只在入口本身是探针程序时安装；其余内建源始终安装。
#[test]
fn probe_sources_install_only_for_probe_entries() {
    let entry = "std/runtime/fixtures/gc_image.gg";
    for path in ["std/runtime/gc_probes.gg", entry] {
        assert!(
            installs_builtin_source(path, entry),
            "探针编译必须安装探针源 `{path}`"
        );
        for user_entry in ["main.gg", "src/lib.gg"] {
            assert!(
                !installs_builtin_source(path, user_entry),
                "入口 `{user_entry}` 不得安装探针源 `{path}`"
            );
        }
    }
    assert!(installs_builtin_source("std/runtime/core.gg", "main.gg"));
    assert!(installs_builtin_source("std/collections.gg", entry));
}
