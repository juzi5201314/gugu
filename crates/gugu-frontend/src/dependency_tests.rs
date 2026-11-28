use super::harness::Harness;
use crate::*;

fn dependency(name: &str, source: &str) -> (project::LockedPackage, DependencyInput) {
    let id = project::PackageId::new(
        name,
        project::Version::parse("1.0.0").unwrap(),
        project::PackageSource::Path { path: name.into() },
    );
    let files = project::PackageFiles::new(BTreeMap::from([
        (
            "gugu.toml".into(),
            format!("[package]\nname = \"{name}\"\nversion = \"1.0.0\"\n").into_bytes(),
        ),
        ("src/lib.gg".into(), source.as_bytes().to_vec()),
    ]))
    .unwrap();
    (
        project::LockedPackage {
            id: id.clone(),
            checksum: None,
            dependencies: Vec::new(),
            features: BTreeMap::new(),
        },
        DependencyInput::new(id, files),
    )
}

fn edge(alias: &str, package: &project::PackageId) -> project::LockedDependency {
    project::LockedDependency {
        alias: alias.into(),
        package: package.clone(),
        domain: project::DependencyDomain::Normal,
        target: None,
        features: Vec::new(),
        default_features: true,
    }
}

fn compile(
    source: &str,
    packages: Vec<project::LockedPackage>,
    inputs: Vec<DependencyInput>,
    aliases: &[(&str, usize)],
) -> Compilation {
    compile_target(
        source,
        packages,
        inputs,
        aliases,
        TargetKind::Bin,
        Compiler::compile_sema_for_test,
    )
}

fn compile_target(
    source: &str,
    mut packages: Vec<project::LockedPackage>,
    inputs: Vec<DependencyInput>,
    aliases: &[(&str, usize)],
    kind: TargetKind,
    compile: fn(&Compiler, CompileRequest) -> Compilation,
) -> Compilation {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("src")).unwrap();
    std::fs::write(
        root.path().join("gugu.toml"),
        "[package]\nname = \"app\"\nversion = \"1.0.0\"\n",
    )
    .unwrap();
    std::fs::write(root.path().join("src/main.gg"), source).unwrap();
    let project = Project::discover(root.path()).unwrap();
    let package = project.current_package().unwrap();
    let (mut app, _) = dependency("app", "");
    app.dependencies = aliases
        .iter()
        .map(|(alias, index)| edge(alias, &packages[*index].id))
        .collect();
    for dependency in &mut app.dependencies {
        dependency.domain = match kind {
            TargetKind::Build => DependencyDomain::Build,
            TargetKind::Example | TargetKind::Test | TargetKind::Bench => DependencyDomain::Test,
            _ => DependencyDomain::Normal,
        };
    }
    let identity = app.id.to_string();
    packages.push(app);
    let lock = project::LockGraph {
        version: 1,
        packages,
    };
    let selected = Target::new(
        kind,
        "app".into(),
        package.root().join("src/main.gg"),
        package.root().join("src"),
        Vec::new(),
        true,
    );
    let request = CompileRequest::project_target(
        package,
        &selected,
        identity,
        TargetName::X86_64Linux,
        Vec::new(),
        aliases.iter().map(|(alias, _)| (*alias).into()).collect(),
    )
    .with_dependencies(&lock, &inputs);
    compile(&Compiler::shared(), request)
}

fn replace_files(input: &DependencyInput, additions: &[(&str, &str)]) -> DependencyInput {
    let mut files = input.files().files().clone();
    for (path, text) in additions {
        files.insert((*path).into(), text.as_bytes().to_vec());
    }
    DependencyInput::new(input.package().clone(), PackageFiles::new(files).unwrap())
}

#[test]
fn dependency_sources_resolve_direct_aliases_and_transitive_reexports() {
    let (base, base_input) = dependency("base", "use std.ptr\npub fn triple(x: int) int = x * 3\n");
    let (mut facade, facade_input) = dependency("facade", "pub use inner.{triple}\n");
    facade.dependencies.push(edge("inner", &base.id));
    let result = compile_target(
        "use math.{triple}\nfn answer(x: int) int = triple(x)\nfn main() { _ = answer(14) }\n",
        vec![base, facade],
        vec![base_input, facade_input],
        &[("math", 1)],
        TargetKind::Bin,
        Compiler::compile_to_lir,
    );
    assert!(result.is_success(), "{:?}", result.diagnostics());
    let mut harness = Harness::new(&result);
    let mut memory = vec![0; 65536];
    assert_eq!(
        harness.invoke("src/main.gg", "answer", &[14], &mut memory),
        [42]
    );
}

#[test]
fn dependency_private_items_and_transitive_aliases_stay_inaccessible() {
    let (base, base_input) = dependency(
        "base",
        "fn secret() int = 42\npub fn value() int = secret()\n",
    );
    let (mut facade, facade_input) = dependency("facade", "pub use inner.{value}\n");
    facade.dependencies.push(edge("inner", &base.id));
    for (source, index, code) in [
        (
            "use math.{secret}\nfn main() { secret() }\n",
            0,
            DiagnosticCode::PrivateImport,
        ),
        (
            "use inner.{value}\nfn main() { value() }\n",
            1,
            DiagnosticCode::ModuleNotFound,
        ),
    ] {
        let result = compile(
            source,
            vec![base.clone(), facade.clone()],
            vec![base_input.clone(), facade_input.clone()],
            &[("math", index)],
        );
        assert!(
            result
                .diagnostics()
                .items()
                .iter()
                .any(|diagnostic| diagnostic.code() == code),
            "{:?}",
            result.diagnostics()
        );
    }
}

#[test]
fn dependency_modules_keep_package_identity_and_support_all_source_kinds() {
    let (mut first, first_input) =
        dependency("first", "use util.{value}\npub fn first() int = value()\n");
    let first_input = replace_files(
        &first_input,
        &[("src/util/mod.gg", "pub fn value() int = 1\n")],
    );
    let (mut second, second_input) = dependency(
        "second",
        "use util.{value}\npub fn second() int = value()\n",
    );
    let second_input = replace_files(
        &second_input,
        &[("src/util.gg", "pub fn value() int = 2\n")],
    );
    first.id = PackageId::new(
        "first",
        Version::parse("1.0.0").unwrap(),
        PackageSource::Registry {
            registry: "test".into(),
        },
    );
    second.id = PackageId::new(
        "second",
        Version::parse("1.0.0").unwrap(),
        PackageSource::Git {
            url: "https://example.com/second".into(),
            commit: "a".repeat(40),
            tree: "b".repeat(40),
        },
    );
    let first_input = DependencyInput::new(first.id.clone(), first_input.files().clone());
    let second_input = DependencyInput::new(second.id.clone(), second_input.files().clone());
    let result = compile(
        "use one\nuse two.{second}\nfn main() { _ = one.first() + second() }\n",
        vec![first, second],
        vec![first_input, second_input],
        &[("one", 0), ("two", 1)],
    );
    assert!(result.is_success(), "{:?}", result.diagnostics());
}

#[test]
fn dependency_features_and_transitive_normal_edges_follow_example_and_build_domains() {
    let source = "#[cfg(feature = \"answer\")] pub fn value() int = 42\n#[cfg(feature = \"default\")] fn default_enabled() {}\n#[cfg(test)] fn excluded() { unknown() }\n";
    for (kind, domain) in [
        (TargetKind::Example, DependencyDomain::Test),
        (TargetKind::Build, DependencyDomain::Build),
    ] {
        let (mut base, base_input) = dependency("base", source);
        base.features
            .insert(domain, vec!["answer".into(), "default".into()]);
        let base_input = replace_files(
            &base_input,
            &[(
                "gugu.toml",
                "[package]\nname = \"base\"\n[features]\nanswer = []\n",
            )],
        );
        let (mut facade, facade_input) = dependency("facade", "pub use inner.{value}\n");
        facade.dependencies.push(edge("inner", &base.id));
        let result = compile_target(
            "use api.{value}\nfn main() { _ = value() }\n",
            vec![base, facade],
            vec![base_input, facade_input],
            &[("api", 1)],
            kind,
            Compiler::compile_sema_for_test,
        );
        assert!(result.is_success(), "{:?}", result.diagnostics());
    }
}

#[test]
fn dependency_custom_lib_entry_and_embedded_files_use_the_verified_input() {
    let (package, input) = dependency("assets", "");
    let input = replace_files(
        &input,
        &[
            (
                "gugu.toml",
                "[package]\nname = \"assets\"\n[lib]\npath = \"./api/mod.gg\"\n",
            ),
            (
                "api/mod.gg",
                "const data: [byte; 3] = std.mem.embed_file(\"../data.txt\")\npub fn size() int = int(data[0])\n",
            ),
            ("data.txt", "abc"),
        ],
    );
    let result = compile(
        "use assets.{size}\nfn main() { _ = size() }\n",
        vec![package],
        vec![input],
        &[("assets", 0)],
    );
    assert!(result.is_success(), "{:?}", result.diagnostics());
}
