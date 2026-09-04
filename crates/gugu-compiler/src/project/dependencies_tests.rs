use tempfile::TempDir;

use super::super::{Project, support::package};
use super::*;

fn registry_candidate(name: &str, version: &str) -> PackageMetadata {
    PackageMetadata::new(PackageId::new(
        name,
        Version::parse(version).expect("valid candidate version"),
        PackageSource::Registry {
            registry: "public".to_owned(),
        },
    ))
}

#[test]
fn semver_supports_caret_wildcards_relational_and_prerelease() {
    let version = |value| Version::parse(value).expect("valid version");

    let caret = VersionReq::parse("1.2").expect("caret requirement");
    assert!(caret.matches(&version("1.9.0")));
    assert!(!caret.matches(&version("2.0.0")));

    let zero = VersionReq::parse("^0.0.0").expect("zero caret requirement");
    assert!(zero.matches(&version("0.0.0")));
    assert!(!zero.matches(&version("0.0.1")));

    let relational = VersionReq::parse(">=1.2, <2.0.0").expect("relational requirement");
    assert!(relational.matches(&version("1.2.0")));
    assert!(!relational.matches(&version("2.0.0")));

    let prerelease = VersionReq::parse(">=1.0.0-alpha.1, <2.0.0").expect("prerelease requirement");
    assert!(prerelease.matches(&version("1.0.0-alpha.2")));
    assert!(!prerelease.matches(&version("1.1.0-alpha.1")));

    let wildcard = VersionReq::parse("1.*").expect("wildcard requirement");
    assert!(wildcard.matches(&version("1.99.0")));
    assert!(!wildcard.matches(&version("2.0.0")));
    assert!(VersionReq::parse("1.*.2").is_err());
    let minor_wildcard = VersionReq::parse("1.2.*").expect("minor wildcard requirement");
    assert!(minor_wildcard.matches(&version("1.2.99")));
    assert!(!minor_wildcard.matches(&version("1.3.0")));
    let tilde = VersionReq::parse("~1.2").expect("tilde requirement");
    assert!(tilde.matches(&version("1.2.9")));
    assert!(!tilde.matches(&version("1.3.0")));
    let build = VersionReq::parse("=1.2.3+one").expect("build requirement");
    assert!(build.matches(&version("1.2.3+two")));
}

#[test]
fn target_conditions_only_activate_matching_target() {
    let linux = TargetCondition::parse("cfg(all(target_os = \"linux\", target_arch = \"x86_64\"))")
        .expect("condition parses");
    assert!(linux.matches("x86_64-linux"));
    assert!(!linux.matches("x86_64-windows"));

    let not_windows =
        TargetCondition::parse("cfg(not(target_os = \"windows\"))").expect("not condition parses");
    assert!(not_windows.matches("x86_64-linux"));
    assert!(!not_windows.matches("x86_64-windows"));
    assert!(TargetCondition::parse("target_os = \"linux\"").is_err());
}

#[test]
fn resolves_path_registry_alias_features_and_domains_deterministically() {
    let root = TempDir::new().expect("tempdir");
    let app = package(
        &root,
        "app",
        "[package]\nowner = \"acme\"\nname = \"app\"\nversion = \"1.0.0\"\n\n[dependencies]\ncore_alias = { package = \"core\", path = \"../core\" }\njson = { package = \"acme/json\", version = \"^1\", default-features = false, features = [\"base\"] }\n\n[test-dependencies]\ncheck = { package = \"acme/check\", version = \"0.4\" }\n\n[features]\ndefault = [\"json/serde\"]\n\n[target.'cfg(target_os = \"windows\")'.dependencies]\nwin = { package = \"acme/win\", version = \"1\" }\n\n[build.dependencies]\nbuilder = { package = \"acme/builder\", version = \"1\" }\n",
        &[("src/main.gg", "fn main() {}\n")],
    );
    package(
        &root,
        "core",
        "[package]\nname = \"core\"\nversion = \"0.1.0\"\n",
        &[("src/lib.gg", "fn core() {}\n")],
    );
    std::fs::write(
        root.path().join("gugu.toml"),
        "[workspace]\nmembers = [\"app\", \"core\"]\n",
    )
    .expect("workspace manifest");

    let project = Project::discover(&app).expect("project discovers");
    let mut json_110 = registry_candidate("acme/json", "1.1.0");
    json_110.features.insert("default".to_owned(), Vec::new());
    json_110.features.insert("base".to_owned(), Vec::new());
    json_110.features.insert("serde".to_owned(), Vec::new());
    let mut json_120 = registry_candidate("acme/json", "1.2.0");
    json_120.features.insert("default".to_owned(), Vec::new());
    json_120.features.insert("base".to_owned(), Vec::new());
    json_120.features.insert("serde".to_owned(), Vec::new());
    let check = registry_candidate("acme/check", "0.4.2");
    let builder = registry_candidate("acme/builder", "1.0.0");
    let win = registry_candidate("acme/win", "1.0.0");
    let candidate_order = vec![
        win.clone(),
        check.clone(),
        json_110.clone(),
        builder.clone(),
        json_120.clone(),
    ];
    let graph = project
        .resolve_dependencies(ResolveOptions {
            target: "x86_64-linux".to_owned(),
            host: "x86_64-linux".to_owned(),
            default_registry: "public".to_owned(),
            registry_packages: candidate_order,
            ..ResolveOptions::default()
        })
        .expect("dependencies resolve");
    let reversed = project
        .resolve_dependencies(ResolveOptions {
            target: "x86_64-linux".to_owned(),
            host: "x86_64-linux".to_owned(),
            default_registry: "public".to_owned(),
            registry_packages: vec![json_120, builder, json_110, check, win],
            ..ResolveOptions::default()
        })
        .expect("dependencies resolve in reversed order");
    assert_eq!(
        graph.to_toml().expect("lock encodes"),
        reversed.to_toml().expect("reversed lock encodes")
    );
    let names = graph
        .packages
        .iter()
        .map(|package| package.id.name())
        .collect::<Vec<_>>();
    assert_eq!(
        names,
        vec![
            "acme/app",
            "acme/builder",
            "acme/check",
            "acme/json",
            "core"
        ]
    );
    let json = graph
        .packages
        .iter()
        .find(|package| package.id.name() == "acme/json")
        .expect("json package");
    assert_eq!(json.id.version().to_string(), "1.2.0");
    assert!(
        json.features
            .values()
            .flatten()
            .any(|feature| feature == "serde")
    );
    assert!(
        graph
            .packages
            .iter()
            .all(|package| package.id.name() != "acme/win")
    );

    let encoded = graph.to_toml().expect("lock encodes");
    assert!(!encoded.contains(root.path().to_str().expect("utf8 temp path")));
    assert_eq!(
        LockGraph::from_toml(&encoded)
            .expect("lock round trips")
            .to_toml()
            .expect("lock re-encodes"),
        encoded
    );
}

#[test]
fn rejects_dependency_cycles_and_unsatisfied_versions() {
    let root = TempDir::new().expect("tempdir");
    let app = package(
        &root,
        "app",
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\n[dependencies]\ncore = { path = \"../core\" }\n",
        &[("src/main.gg", "fn main() {}\n")],
    );
    package(
        &root,
        "core",
        "[package]\nname = \"core\"\nversion = \"0.1.0\"\n[dependencies]\napp = { path = \"../app\" }\n",
        &[("src/lib.gg", "fn core() {}\n")],
    );
    std::fs::write(
        root.path().join("gugu.toml"),
        "[workspace]\nmembers = [\"app\", \"core\"]\n",
    )
    .expect("workspace manifest");
    let project = Project::discover(&app).expect("project discovers");
    let cycle = project
        .resolve_dependencies(ResolveOptions::default())
        .expect_err("cycle fails");
    assert!(cycle.to_string().contains("循环"));

    let root = TempDir::new().expect("tempdir");
    let app = package(
        &root,
        "app",
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\n[dependencies]\njson = { package = \"acme/json\", version = \"^1\" }\n",
        &[("src/main.gg", "fn main() {}\n")],
    );
    let project = Project::discover(&app).expect("project discovers");
    let mut options = ResolveOptions::default();
    options.registry_packages = vec![registry_candidate("acme/json", "2.0.0")];
    let unsatisfied = project
        .resolve_dependencies(options)
        .expect_err("unsatisfied version fails");
    assert!(unsatisfied.to_string().contains("没有满足"));
}

#[test]
fn lock_graph_rejects_absolute_path_and_unknown_edges() {
    let absolute = "version = 1\n[[package]]\nname = \"app\"\nversion = \"1.0.0\"\nsource = \"path+/tmp/app\"\ndependencies = []\nfeatures = { normal = [], test = [], build = [] }\n";
    assert!(LockGraph::from_toml(absolute).is_err());

    let unknown = "version = 1\n[[package]]\nname = \"app\"\nversion = \"1.0.0\"\nsource = \"registry+public\"\ndependencies = [{ alias = \"missing\", package = \"missing\", version = \"1.0.0\", source = \"registry+public\", kind = \"normal\", features = [], default-features = true }]\nfeatures = { normal = [], test = [], build = [] }\n";
    assert!(LockGraph::from_toml(unknown).is_err());
}

#[test]
fn git_source_selects_locked_commit_and_skips_yanked_candidate() {
    let root = TempDir::new().expect("tempdir");
    let app = package(
        &root,
        "app",
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\n[dependencies]\nparser = { package = \"acme/parser\", git = \"https://example.com/acme/parser.git\", rev = \"abc\" }\n",
        &[("src/main.gg", "fn main() {}\n")],
    );
    let project = Project::discover(&app).expect("project discovers");
    let source = |commit: &str, tree: &str| PackageSource::Git {
        url: "https://example.com/acme/parser.git".to_owned(),
        commit: commit.to_owned(),
        tree: tree.to_owned(),
    };
    let yanked = PackageMetadata::new(PackageId::new(
        "acme/parser",
        Version::new(2, 0, 0),
        source("abc", "old"),
    ))
    .with_yanked(true);
    let selected = PackageMetadata::new(PackageId::new(
        "acme/parser",
        Version::new(1, 4, 0),
        source("abc", "tree"),
    ));
    let graph = project
        .resolve_dependencies(ResolveOptions {
            git_packages: vec![yanked, selected],
            ..ResolveOptions::default()
        })
        .expect("git dependency resolves");
    let parser = graph
        .packages
        .iter()
        .find(|package| package.id.name() == "acme/parser")
        .expect("parser package");
    assert_eq!(parser.id.version().to_string(), "1.4.0");
    assert!(
        matches!(parser.id.source(), PackageSource::Git { commit, tree, .. } if commit == "abc" && tree == "tree")
    );
    assert!(
        graph
            .to_toml()
            .expect("lock encodes")
            .contains("commit=abc&tree=tree")
    );
}
