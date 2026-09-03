use tempfile::TempDir;

use super::super::{
    Project, TargetKind, TargetSelection,
    support::{package, target_summary},
};
use super::*;

#[test]
fn discovers_single_package_targets() {
    let root = TempDir::new().expect("tempdir");
    let package = package(
        &root,
        "demo",
        "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n",
        &[
            ("src/main.gg", "fn main() {}\n"),
            ("src/lib.gg", "fn util() {}\n"),
            ("src/bin/extra.gg", "fn main() {}\n"),
            ("src/bin/nested/main.gg", "fn main() {}\n"),
            ("tests/basic.gg", "fn checks() {}\n"),
            ("benches/perf.gg", "fn bench() {}\n"),
            ("examples/hello.gg", "fn main() {}\n"),
            ("build.gg", "fn main() {}\n"),
        ],
    );

    let project = Project::discover(&package).expect("project discovers");
    assert_eq!(project.workspace().root(), &package);
    let current = project.current_package().expect("current package");
    assert_eq!(current.name(), "demo");
    assert_eq!(
        target_summary(current),
        vec![
            (TargetKind::Lib, "demo".into()),
            (TargetKind::Bin, "demo".into()),
            (TargetKind::Bin, "extra".into()),
            (TargetKind::Bin, "nested".into()),
            (TargetKind::Test, "basic".into()),
            (TargetKind::Bench, "perf".into()),
            (TargetKind::Example, "hello".into()),
            (TargetKind::Build, "build".into()),
        ]
    );

    // src/bin/nested/main.gg 的 source root 是 src，与默认 target 共享。
    assert!(
        current
            .targets()
            .iter()
            .any(|target| target.name() == "nested" && target.source_root() == package.join("src"))
    );

    // 默认构建集合是 lib + 所有 bin。
    let default = current
        .select_targets(&TargetSelection::DefaultBuild)
        .expect("default targets");
    assert_eq!(default.len(), 4);
    assert!(
        default
            .iter()
            .all(|target| { matches!(target.kind(), TargetKind::Lib | TargetKind::Bin) })
    );
}

#[test]
fn rejects_module_file_and_directory_conflict() {
    let root = TempDir::new().expect("tempdir");
    let conflict = package(
        &root,
        "conflict",
        "[package]\nname = \"conflict\"\n",
        &[
            ("src/main.gg", "fn main() {}\n"),
            ("src/foo.gg", "fn a() {}\n"),
            ("src/foo/mod.gg", "fn b() {}\n"),
        ],
    );
    assert!(matches!(
        Project::discover(&conflict),
        Err(ProjectError::TargetDiscovery { .. })
    ));

    // 相邻目录没有 mod.gg 时文件形式合法。
    let file_only = package(
        &root,
        "file-only",
        "[package]\nname = \"file-only\"\n",
        &[
            ("src/main.gg", "fn main() {}\n"),
            ("src/foo.gg", "fn a() {}\n"),
            ("src/foo/util.gg", "fn b() {}\n"),
        ],
    );
    assert!(Project::discover(&file_only).is_ok());
}

#[test]
fn rejects_duplicate_targets_and_escaping_entries() {
    let root = TempDir::new().expect("tempdir");
    let duplicated = package(
        &root,
        "dup",
        "[package]\nname = \"dup\"\n\n[[bin]]\nname = \"extra\"\npath = \
             \"src/bin/extra.gg\"\n",
        &[
            ("src/main.gg", "fn main() {}\n"),
            ("src/bin/extra.gg", "fn main() {}\n"),
        ],
    );
    assert!(matches!(
        Project::discover(&duplicated),
        Err(ProjectError::TargetDiscovery { .. })
    ));

    let outside = package(
        &root,
        "escape",
        "[package]\nname = \"escape\"\n\n[[bin]]\nname = \"outer\"\npath = \
             \"../outer.gg\"\n",
        &[
            ("src/main.gg", "fn main() {}\n"),
            ("outer.gg", "fn main() {}\n"),
        ],
    );
    assert!(matches!(
        Project::discover(&outside),
        Err(ProjectError::TargetDiscovery { .. })
    ));
}

#[test]
fn explicit_lib_and_target_paths_override_autodiscovery() {
    let root = TempDir::new().expect("tempdir");
    // 包名带 `-` 时，lib target 名按规范把 `-` 换成 `_`。
    let package = package(
        &root,
        "explicit",
        "[package]\nname = \"my-lib\"\nauto-bins = false\n\n[lib]\npath = \
             \"source/lib.gg\"\n\n[[bin]]\nname = \"cli\"\npath = \"source/cli.gg\"\n",
        &[
            ("source/lib.gg", "fn util() {}\n"),
            ("source/cli.gg", "fn main() {}\n"),
            ("src/main.gg", "fn main() {}\n"),
        ],
    );
    let project = Project::discover(&package).expect("project discovers");
    let current = project.current_package().expect("current package");
    assert_eq!(
        target_summary(current),
        vec![
            (TargetKind::Lib, "my_lib".into()),
            (TargetKind::Bin, "cli".into()),
        ]
    );
    assert_eq!(current.targets()[0].entry(), &package.join("source/lib.gg"));
    assert_eq!(current.targets()[1].entry(), &package.join("source/cli.gg"));
}
