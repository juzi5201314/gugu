use std::fs;

use tempfile::TempDir;

use super::super::{Project, support::package};

#[test]
fn virtual_workspace_resolves_members_globs_and_defaults() {
    let root = TempDir::new().expect("tempdir");
    fs::write(
        root.path().join("gugu.toml"),
        "[workspace]\nmembers = [\"packages/*\", \"tools/*\"]\nexclude = \
             [\"packages/legacy\"]\ndefault-members = [\"packages/app\"]\n",
    )
    .expect("write workspace manifest");
    let app = package(
        &root,
        "packages/app",
        "[package]\nname = \"app\"\n",
        &[("src/main.gg", "fn main() {}\n")],
    );
    let lib = package(
        &root,
        "packages/lib",
        "[package]\nname = \"lib\"\n",
        &[("src/lib.gg", "fn util() {}\n")],
    );
    package(
        &root,
        "packages/legacy",
        "[package]\nname = \"legacy\"\n",
        &[("src/lib.gg", "fn old() {}\n")],
    );
    let codegen = package(
        &root,
        "tools/codegen",
        "[package]\nname = \"codegen\"\n",
        &[("src/lib.gg", "fn gen() {}\n")],
    );

    // 从 workspace 根构建：default-members 只选 app。
    let project = Project::discover(root.path()).expect("workspace discovers");
    assert_eq!(project.workspace().root(), root.path());
    assert!(project.current_package().is_none());
    assert_eq!(
        project.workspace().default_members(),
        std::slice::from_ref(&app)
    );
    assert_eq!(project.packages().len(), 3);
    let defaults = project
        .select_packages(None, false)
        .expect("default packages");
    assert_eq!(defaults.len(), 1);
    assert_eq!(defaults[0].root(), app);

    // --workspace 覆盖 default-members，但 exclude 仍然生效。
    let whole = project
        .select_packages(None, true)
        .expect("whole workspace");
    let mut roots = whole
        .iter()
        .map(|package| package.root().to_path_buf())
        .collect::<Vec<_>>();
    roots.sort();
    assert_eq!(roots, vec![app.clone(), lib.clone(), codegen.clone()]);

    // 从成员目录构建：当前 package 定位到 lib，不再使用 default-members。
    let from_member = Project::discover(&lib).expect("member project discovers");
    assert_eq!(
        from_member
            .current_package()
            .expect("member current")
            .root(),
        lib
    );
    let member_defaults = from_member
        .select_packages(None, false)
        .expect("member selection");
    assert_eq!(member_defaults.len(), 1);
    assert_eq!(member_defaults[0].root(), lib);
}

#[test]
fn root_package_workspace_selects_root_package() {
    let root = TempDir::new().expect("tempdir");
    let package_root = package(
        &root,
        ".",
        "[package]\nname = \"root\"\n\n[workspace]\nmembers = [\"sub\"]\n",
        &[
            ("src/main.gg", "fn main() {}\n"),
            ("sub/gugu.toml", "[package]\nname = \"sub\"\n"),
            ("sub/src/lib.gg", "fn s() {}\n"),
        ],
    );

    let project = Project::discover(&package_root).expect("root workspace discovers");
    assert_eq!(project.packages().len(), 2);
    let selected = project
        .select_packages(None, false)
        .expect("root selection");
    assert_eq!(selected.len(), 1);
    assert_eq!(selected[0].name(), "root");
}

#[test]
fn root_started_default_members_override_root_package() {
    let root = TempDir::new().expect("tempdir");
    let package_root = package(
        &root,
        ".",
        "[package]\nname = \"root\"\n\n[workspace]\nmembers = [\"sub\"]\ndefault-members = \
             [\"sub\"]\n",
        &[
            ("src/main.gg", "fn main() {}\n"),
            ("sub/gugu.toml", "[package]\nname = \"sub\"\n"),
            ("sub/src/lib.gg", "fn s() {}\n"),
        ],
    );

    // 根启动：default-members 优先于根 package。
    let project = Project::discover(&package_root).expect("root workspace discovers");
    let selected = project
        .select_packages(None, false)
        .expect("default selection");
    assert_eq!(selected.len(), 1);
    assert_eq!(selected[0].name(), "sub");
}

#[test]
fn member_started_workspace_sees_root_package_for_whole_workspace() {
    let root = TempDir::new().expect("tempdir");
    let package_root = package(
        &root,
        ".",
        "[package]\nname = \"root\"\n\n[workspace]\nmembers = [\"sub\"]\n",
        &[
            ("src/main.gg", "fn main() {}\n"),
            ("sub/gugu.toml", "[package]\nname = \"sub\"\n"),
            ("sub/src/lib.gg", "fn s() {}\n"),
        ],
    );
    let sub = package_root.join("sub");

    // 成员目录启动：--workspace 必须包含根 package。
    let project = Project::discover(&sub).expect("member project discovers");
    let names = project
        .select_packages(None, true)
        .expect("whole workspace")
        .iter()
        .map(|package| package.name().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(names, vec!["root".to_owned(), "sub".to_owned()]);
    // 无 --workspace 时只选当前 package。
    let current = project
        .select_packages(None, false)
        .expect("current selection");
    assert_eq!(current.len(), 1);
    assert_eq!(current[0].name(), "sub");
}

#[test]
fn workspace_exclude_allows_nonexistent_directory() {
    let root = TempDir::new().expect("tempdir");
    let package_root = package(
        &root,
        ".",
        "[package]\nname = \"root\"\n\n[workspace]\nmembers = [\"sub\"]\nexclude = [\"nonexistent\"]\n",
        &[
            ("src/main.gg", "fn main() {}\n"),
            ("sub/gugu.toml", "[package]\nname = \"sub\"\n"),
            ("sub/src/lib.gg", "fn s() {}\n"),
        ],
    );
    let project = Project::discover(&package_root).expect("discover with nonexistent exclude");
    assert_eq!(project.packages().len(), 2);
}
