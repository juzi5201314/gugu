//! 将锁定依赖的已验证源码装入同一闭世界，保留每个包的名字与配置边界。

use super::*;
use gugu_sema::modules::{PackageScope, PackageSources};
use std::sync::Arc;

struct Library<'a> {
    package: &'a LockedPackage,
    input: &'a DependencyInput,
    target: Target,
    declared: Vec<String>,
    namespace: String,
}

fn invalid(identity: &str, message: impl Into<String>) -> LoadInputError {
    LoadInputError::ModuleTree {
        path: identity.into(),
        message: message.into(),
    }
}

fn namespace(identity: &str) -> String {
    let digest = hash::hash_domain("gugu-package-module-v1", identity.as_bytes());
    let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    // `@` 不是源码模块名字符，内部 package 根不能被源码伪造或直接导入。
    format!("@{hex}")
}

fn logical(path: &Path) -> String {
    path.to_str()
        .expect("已验证的 package 路径是 UTF-8")
        .replace(std::path::MAIN_SEPARATOR, "/")
}

fn module_path(entry: &str, root: &str) -> String {
    let relative = if root.is_empty() {
        entry
    } else {
        entry.strip_prefix(root).unwrap().trim_start_matches('/')
    };
    let stem = relative.strip_suffix(".gg").unwrap();
    if stem == "mod" {
        String::new()
    } else {
        stem.strip_suffix("/mod").unwrap_or(stem).replace('/', ".")
    }
}

fn active_edges(
    package: &LockedPackage,
    root: bool,
    domain: DependencyDomain,
    target: TargetName,
) -> impl Iterator<Item = &LockedDependency> {
    package.dependencies.iter().filter(move |edge| {
        let active = if !root {
            edge.domain == DependencyDomain::Normal
        } else {
            match domain {
                DependencyDomain::Build => edge.domain == DependencyDomain::Build,
                DependencyDomain::Test => edge.domain != DependencyDomain::Build,
                DependencyDomain::Normal => edge.domain == DependencyDomain::Normal,
            }
        };
        active
            && edge.target.as_ref().is_none_or(|condition| {
                TargetCondition::parse(condition)
                    .expect("锁图 target 条件已验证")
                    .matches(target.name())
            })
    })
}

fn collect_libraries<'a>(
    lock: &'a LockGraph,
    inputs: &'a [DependencyInput],
    root: &LockedPackage,
    cfg: &CfgContext,
    external: &BTreeSet<String>,
    domain: DependencyDomain,
) -> Result<BTreeMap<String, Library<'a>>, LoadInputError> {
    let packages: BTreeMap<_, _> = lock
        .packages
        .iter()
        .map(|package| (package.id.to_string(), package))
        .collect();
    let files: BTreeMap<_, _> = inputs
        .iter()
        .map(|input| (input.package().to_string(), input))
        .collect();
    let mut pending: Vec<_> = active_edges(root, true, domain, cfg.target())
        .filter(|edge| external.contains(&edge.alias))
        .map(|edge| edge.package.to_string())
        .collect();
    let mut selected = BTreeMap::new();
    while let Some(id) = pending.pop() {
        if selected.contains_key(&id) {
            continue;
        }
        let package = *packages
            .get(&id)
            .ok_or_else(|| invalid(&id, "依赖不在锁图中"))?;
        let input = *files
            .get(&id)
            .ok_or_else(|| invalid(&id, "依赖缺少已验证源码输入"))?;
        let (target, declared) = input
            .library_configuration()
            .map_err(|error| invalid(&id, error.to_string()))?;
        pending.extend(
            active_edges(package, false, domain, cfg.target()).map(|edge| edge.package.to_string()),
        );
        let namespace = namespace(&id);
        selected.insert(
            id,
            Library {
                package,
                input,
                target,
                declared,
                namespace,
            },
        );
    }
    Ok(selected)
}

fn imports<'a>(
    edges: impl Iterator<Item = &'a LockedDependency>,
    libraries: &BTreeMap<String, Library<'_>>,
) -> BTreeMap<String, (String, String)> {
    edges
        .map(|edge| {
            let library = &libraries[&edge.package.to_string()];
            let local = module_path(
                &logical(library.target.entry()),
                &logical(library.target.source_root()),
            );
            let entry = if local.is_empty() {
                library.namespace.clone()
            } else {
                format!("{}.{local}", library.namespace)
            };
            (edge.alias.clone(), (library.namespace.clone(), entry))
        })
        .collect()
}

fn install_library(
    snapshots: &mut Vec<SourceSnapshot>,
    embed: &mut EmbedHost,
    library: &Library<'_>,
    imports: BTreeMap<String, (String, String)>,
    target: TargetName,
    domain: DependencyDomain,
) -> Result<PackageSources, LoadInputError> {
    let logical_root = format!("__packages/{}", &library.namespace[1..]);
    let root = logical(library.target.source_root());
    let prefix = if root.is_empty() {
        String::new()
    } else {
        format!("{root}/")
    };
    for (path, bytes) in library.input.files().files() {
        if path.starts_with(&prefix) && path.ends_with(".gg") {
            snapshots.push(
                SourceSnapshot::from_bytes(format!("{logical_root}/{path}"), bytes.clone())
                    .map_err(LoadInputError::Snapshot)?,
            );
        }
    }
    embed.add_package(logical_root.clone(), library.input.files().files().clone());
    let enabled = library
        .package
        .features
        .get(&domain)
        .cloned()
        .unwrap_or_default();
    Ok(PackageSources {
        source_root: if root.is_empty() {
            logical_root.clone()
        } else {
            format!("{logical_root}/{root}")
        },
        logical_root,
        scope: Arc::new(PackageScope {
            identity: library.package.id.to_string(),
            namespace: library.namespace.clone(),
            dependencies: imports,
            cfg: CfgContext::new(
                target,
                library.declared.clone(),
                enabled,
                false,
                false,
                BTreeMap::new(),
            ),
        }),
    })
}

#[expect(
    clippy::too_many_arguments,
    reason = "源码装配显式接收当前包、锁图、配置与已验证输入"
)]
pub(super) fn load(
    snapshots: &mut Vec<SourceSnapshot>,
    embed: &mut EmbedHost,
    lock: &LockGraph,
    inputs: &[DependencyInput],
    identity: &str,
    source_root: &str,
    cfg: &CfgContext,
    external: &BTreeSet<String>,
    domain: DependencyDomain,
) -> Result<Vec<PackageSources>, LoadInputError> {
    let root = lock
        .packages
        .iter()
        .find(|package| package.id.to_string() == identity)
        .ok_or_else(|| invalid(identity, "当前 package 不在锁图中"))?;
    let libraries = collect_libraries(lock, inputs, root, cfg, external, domain)?;
    let mut sources = Vec::with_capacity(libraries.len() + 1);
    for library in libraries.values() {
        let edges = active_edges(library.package, false, domain, cfg.target());
        sources.push(install_library(
            snapshots,
            embed,
            library,
            imports(edges, &libraries),
            cfg.target(),
            domain,
        )?);
    }
    let edges = active_edges(root, true, domain, cfg.target())
        .filter(|edge| external.contains(&edge.alias));
    // 当前包匹配所有剩余用户源码，因此必须排在带显式逻辑根的依赖之后。
    sources.push(PackageSources {
        logical_root: String::new(),
        source_root: source_root.into(),
        scope: Arc::new(PackageScope {
            identity: identity.into(),
            namespace: String::new(),
            dependencies: imports(edges, &libraries),
            cfg: cfg.clone(),
        }),
    });
    Ok(sources)
}
