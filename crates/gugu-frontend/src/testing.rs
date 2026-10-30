//! 前端测试的 artifact 与 session helper；artifact 装载复用 `gugu_gir::testing`。

pub(crate) use gugu_gir::testing::{provider, resources};

use crate::Compilation;

/// 断言会话源码表恰由给定用户源码与内建库源组成。
///
/// 探针源只在入口本身是探针程序时安装，因此期望集合按安装规则从 artifact 源表派生，
/// 而不是把内建源个数写死。
pub(crate) fn assert_session_sources(compilation: &Compilation, user: &[&str], entry: &str) {
    let mut installed: Vec<&str> = compilation
        .source_map()
        .snapshots()
        .iter()
        .map(|snapshot| snapshot.logical_path())
        .filter(|path| !user.contains(path))
        .collect();
    installed.sort_unstable();
    let builtin = resources();
    let mut expected: Vec<&str> = builtin
        .sources()
        .iter()
        .map(|source| source.logical_path.as_str())
        .filter(|path| crate::frontend::installs_builtin_source(path, entry))
        .collect();
    expected.sort_unstable();
    assert_eq!(installed, expected, "会话源码表必须恰为内建库源与用户源码");
}
