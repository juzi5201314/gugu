use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

use super::super::{error::ProjectError, manifest, model::Package};
use super::{
    dependency_manifest::{canonical_path, dependency_error, package_metadata},
    dependency_model::*,
};

#[derive(Clone, Debug)]
struct ResolverContext {
    workspace_root: PathBuf,
    target: String,
    host: String,
    default_registry: String,
    path_packages: BTreeMap<PathBuf, PackageMetadata>,
    path_roots: BTreeMap<PackageId, PathBuf>,
    registry_packages: Vec<PackageMetadata>,
    git_packages: Vec<PackageMetadata>,
}

#[derive(Clone, Debug, Default)]
struct ResolveState {
    nodes: Vec<ResolveNode>,
    edges: Vec<ResolveEdge>,
    pending: Vec<PendingEdge>,
}

#[derive(Clone, Debug)]
struct ResolveNode {
    metadata: PackageMetadata,
    domain: DependencyDomain,
    root: bool,
    features: BTreeSet<String>,
    expanded_features: BTreeSet<String>,
    optional_dependencies: BTreeSet<String>,
    dependency_features: BTreeMap<String, BTreeSet<String>>,
}

#[derive(Clone, Debug)]
struct ResolveEdge {
    parent: usize,
    target: usize,
    alias: String,
    domain: DependencyDomain,
    target_condition: Option<TargetCondition>,
    features: BTreeSet<String>,
    default_features: bool,
}

#[derive(Clone, Debug)]
struct PendingEdge {
    parent: usize,
    spec: DependencySpec,
    features: BTreeSet<String>,
}

impl ResolveState {
    fn add_node(
        &mut self,
        metadata: PackageMetadata,
        domain: DependencyDomain,
        root: bool,
    ) -> usize {
        self.nodes.push(ResolveNode {
            metadata,
            domain,
            root,
            features: BTreeSet::new(),
            expanded_features: BTreeSet::new(),
            optional_dependencies: BTreeSet::new(),
            dependency_features: BTreeMap::new(),
        });
        self.nodes.len() - 1
    }

    fn into_lock_graph(self) -> LockGraph {
        let node_ids = self
            .nodes
            .iter()
            .map(|node| node.metadata.id.clone())
            .collect::<Vec<_>>();
        let mut packages = BTreeMap::<PackageId, LockedPackage>::new();
        for node in self.nodes {
            let record =
                packages
                    .entry(node.metadata.id.clone())
                    .or_insert_with(|| LockedPackage {
                        id: node.metadata.id.clone(),
                        checksum: node.metadata.checksum.clone(),
                        dependencies: Vec::new(),
                        features: BTreeMap::new(),
                    });
            record
                .features
                .entry(node.domain)
                .or_default()
                .extend(node.features);
        }
        for edge in self.edges {
            let parent_id = node_ids[edge.parent].clone();
            let target_id = node_ids[edge.target].clone();
            let record = packages.get_mut(&parent_id).expect("parent package exists");
            if let Some(existing) = record.dependencies.iter_mut().find(|dependency| {
                dependency.alias == edge.alias
                    && dependency.package == target_id
                    && dependency.domain == edge.domain
                    && dependency.target == edge.target_condition.as_ref().map(ToString::to_string)
            }) {
                existing.features.extend(edge.features);
                existing.default_features |= edge.default_features;
            } else {
                record.dependencies.push(LockedDependency {
                    alias: edge.alias,
                    package: target_id,
                    domain: edge.domain,
                    target: edge.target_condition.map(|condition| condition.to_string()),
                    features: edge.features.into_iter().collect(),
                    default_features: edge.default_features,
                });
            }
        }
        for package in packages.values_mut() {
            for features in package.features.values_mut() {
                features.sort();
                features.dedup();
            }
            for dependency in &mut package.dependencies {
                dependency.features.sort();
                dependency.features.dedup();
            }
            package.dependencies.sort_by(|left, right| {
                (left.domain, &left.alias, &left.package).cmp(&(
                    right.domain,
                    &right.alias,
                    &right.package,
                ))
            });
        }
        LockGraph {
            version: 1,
            packages: packages.into_values().collect(),
        }
    }
}

/// 解析 workspace 及 source index，生成确定性锁图。
pub(crate) fn resolve_project(
    workspace_root: &Path,
    packages: &[Package],
    options: ResolveOptions,
) -> Result<LockGraph, ProjectError> {
    let workspace_manifest = workspace_root.join("gugu.toml");
    let workspace_raw = manifest::read_manifest(&workspace_manifest)?;
    let workspace_dependencies = workspace_raw
        .workspace
        .as_ref()
        .and_then(|workspace| workspace.dependencies.clone())
        .unwrap_or_default();
    let mut path_packages = BTreeMap::new();
    let mut path_roots = BTreeMap::new();
    for package in packages {
        let metadata = package_metadata(workspace_root, package, &workspace_dependencies)?;
        path_roots.insert(metadata.id.clone(), package.root().to_path_buf());
        path_packages.insert(package.root().to_path_buf(), metadata);
    }
    let mut context = ResolverContext {
        workspace_root: workspace_root.to_path_buf(),
        target: options.target,
        host: options.host,
        default_registry: options.default_registry,
        path_packages,
        path_roots,
        registry_packages: unique_candidates(options.registry_packages, "registry")?,
        git_packages: unique_candidates(options.git_packages, "git")?,
    };
    collect_external_path_packages(&mut context, &workspace_dependencies)?;
    let roots = select_roots(packages, &options.roots)?;
    let mut state = ResolveState::default();
    for package in roots {
        let metadata = context
            .path_packages
            .get(package.root())
            .cloned()
            .expect("workspace package metadata exists");
        for domain in [
            DependencyDomain::Normal,
            DependencyDomain::Test,
            DependencyDomain::Build,
        ] {
            let node = state.add_node(metadata.clone(), domain, true);
            let mut features = options
                .root_features
                .get(&package.package_name())
                .cloned()
                .unwrap_or_default();
            if options
                .root_default_features
                .get(&package.package_name())
                .copied()
                .unwrap_or(true)
            {
                features.push("default".to_owned());
            }
            activate_node(&context, &mut state, node, features)?;
        }
    }
    let state = solve(&context, state)?;
    validate_acyclic(&state)?;
    Ok(state.into_lock_graph())
}

fn unique_candidates(
    candidates: Vec<PackageMetadata>,
    source_kind: &str,
) -> Result<Vec<PackageMetadata>, ProjectError> {
    let mut unique = BTreeMap::<PackageId, PackageMetadata>::new();
    for candidate in candidates {
        validate_candidate(&candidate, source_kind)?;
        if let Some(previous) = unique.insert(candidate.id.clone(), candidate.clone()) {
            if previous != candidate {
                return Err(dependency_error(
                    candidate.id.name(),
                    "同一 package ID 的 source metadata 不一致",
                ));
            }
        }
    }
    Ok(unique.into_values().collect())
}

fn validate_candidate(candidate: &PackageMetadata, source_kind: &str) -> Result<(), ProjectError> {
    let source_matches_kind = matches!(
        (candidate.id.source(), source_kind),
        (PackageSource::Registry { .. }, "registry") | (PackageSource::Git { .. }, "git")
    );
    if !source_matches_kind {
        return Err(dependency_error(
            candidate.id.name(),
            "source index 中的 package source 类型不匹配",
        ));
    }
    if candidate.checksum.as_ref().is_some_and(|checksum| {
        checksum.len() != 64
            || !checksum
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    }) {
        return Err(dependency_error(
            candidate.id.name(),
            "package checksum 必须是 64 位小写十六进制",
        ));
    }
    Ok(())
}

fn select_roots<'packages>(
    packages: &'packages [Package],
    requested: &[String],
) -> Result<Vec<&'packages Package>, ProjectError> {
    if requested.is_empty() {
        return Ok(packages.iter().collect());
    }
    let mut roots = Vec::new();
    for requested in requested {
        let matches = packages
            .iter()
            .filter(|package| package.package_name() == *requested || package.name() == requested)
            .collect::<Vec<_>>();
        match matches.as_slice() {
            [package] => roots.push(*package),
            [] => {
                return Err(ProjectError::PackageSelection {
                    requested: requested.clone(),
                });
            }
            _ => {
                return Err(ProjectError::AmbiguousPackage {
                    requested: requested.clone(),
                });
            }
        }
    }
    Ok(roots)
}

fn collect_external_path_packages(
    context: &mut ResolverContext,
    workspace_dependencies: &BTreeMap<String, toml::Value>,
) -> Result<(), ProjectError> {
    let mut pending = context
        .path_packages
        .iter()
        .map(|(root, metadata)| (root.clone(), metadata.clone()))
        .collect::<Vec<_>>();
    while let Some((root, metadata)) = pending.pop() {
        for dependency in
            metadata
                .dependencies
                .iter()
                .filter_map(|dependency| match &dependency.source {
                    DependencySource::Path { path } => Some((dependency, path)),
                    _ => None,
                })
        {
            let resolved = canonical_path(&root, dependency.1, metadata.id.name())?;
            if context.path_packages.contains_key(&resolved) {
                continue;
            }
            let package = manifest::build_package(&resolved.join("gugu.toml"))?;
            let metadata =
                package_metadata(&context.workspace_root, &package, workspace_dependencies)?;
            context
                .path_roots
                .insert(metadata.id.clone(), resolved.clone());
            context
                .path_packages
                .insert(resolved.clone(), metadata.clone());
            pending.push((resolved, metadata));
        }
    }
    Ok(())
}

fn solve(context: &ResolverContext, state: ResolveState) -> Result<ResolveState, ProjectError> {
    let Some(pending) = state.pending.first().cloned() else {
        return Ok(state);
    };
    let mut base = state;
    base.pending.remove(0);
    let context_domain = base.nodes[pending.parent].domain;
    let candidates = candidate_choices(context, &base, &pending, context_domain)?;
    let mut best = None;
    let mut last_error = None;
    for (metadata, existing) in candidates {
        let mut next = base.clone();
        let target = existing.unwrap_or_else(|| {
            next.add_node(
                metadata.expect("new candidate has metadata"),
                context_domain,
                false,
            )
        });
        attach_edge(&mut next, &pending, target);
        if let Err(error) = activate_node(
            context,
            &mut next,
            target,
            pending.features.iter().cloned().collect(),
        ) {
            last_error = Some(error);
            continue;
        }
        match solve(context, next) {
            Ok(result) => {
                if best
                    .as_ref()
                    .is_none_or(|current| is_better(&result, current))
                {
                    best = Some(result);
                }
            }
            Err(error) => last_error = Some(error),
        }
    }
    best.ok_or_else(|| {
        last_error.unwrap_or_else(|| {
            dependency_error(&pending.spec.package, "依赖约束无法得到一致的版本选择")
        })
    })
}
fn candidate_choices(
    context: &ResolverContext,
    state: &ResolveState,
    pending: &PendingEdge,
    context_domain: DependencyDomain,
) -> Result<Vec<(Option<PackageMetadata>, Option<usize>)>, ProjectError> {
    let mut candidates = source_candidates(
        context,
        &state.nodes[pending.parent].metadata.id,
        &pending.spec,
    )?;
    candidates.retain(|candidate| {
        candidate.id.name() == pending.spec.package
            && !candidate.yanked
            && pending.spec.version.matches(candidate.id.version())
    });
    let mut choices = Vec::new();
    for candidate in candidates {
        let existing = state
            .nodes
            .iter()
            .position(|node| node.domain == context_domain && node.metadata.id == candidate.id);
        choices.push((Some(candidate), existing));
    }
    if choices.is_empty() {
        return Err(dependency_error(
            &pending.spec.package,
            format!("source 没有满足 `{}` 的候选版本", pending.spec.version),
        ));
    }
    Ok(choices)
}
fn source_candidates(
    context: &ResolverContext,
    parent: &PackageId,
    dependency: &DependencySpec,
) -> Result<Vec<PackageMetadata>, ProjectError> {
    match &dependency.source {
        DependencySource::Path { path } => {
            let parent_root = context
                .path_roots
                .get(parent)
                .ok_or_else(|| dependency_error(&dependency.package, "无法确定 path 依赖的父 package 根"))?;
            let resolved = canonical_path(parent_root, path, &dependency.package)?;
            context
                .path_packages
                .get(&resolved)
                .cloned()
                .map(|metadata| vec![metadata])
                .ok_or_else(|| dependency_error(&dependency.package, format!("path source `{}` 不是可解析 package", path.display())))
        }
        DependencySource::Registry { registry } => Ok(context
            .registry_packages
            .iter()
            .filter(|candidate| {
                matches!(candidate.id.source(), PackageSource::Registry { registry: identity } if identity == registry || (registry == "default" && identity == &context.default_registry))
            })
            .cloned()
            .collect()),
        DependencySource::Git { url, rev, .. } => Ok(context
            .git_packages
            .iter()
            .filter(|candidate| {
                matches!(candidate.id.source(), PackageSource::Git { url: identity, commit, .. } if identity == url && rev.as_ref().is_none_or(|wanted| wanted == commit))
            })
            .cloned()
            .collect()),
    }
}

fn is_better(left: &ResolveState, right: &ResolveState) -> bool {
    if left.nodes.len() != right.nodes.len() {
        return left.nodes.len() < right.nodes.len();
    }
    let left_ids = left
        .nodes
        .iter()
        .map(|node| node.metadata.id.clone())
        .collect::<Vec<_>>();
    let right_ids = right
        .nodes
        .iter()
        .map(|node| node.metadata.id.clone())
        .collect::<Vec<_>>();
    left_ids > right_ids
}

fn attach_edge(state: &mut ResolveState, pending: &PendingEdge, target: usize) {
    if let Some(edge) = state.edges.iter_mut().find(|edge| {
        edge.parent == pending.parent
            && edge.target == target
            && edge.alias == pending.spec.alias
            && edge.domain == pending.spec.domain
            && edge.target_condition == pending.spec.target
    }) {
        edge.features.extend(pending.features.iter().cloned());
        edge.default_features |= pending.spec.default_features;
        return;
    }
    state.edges.push(ResolveEdge {
        parent: pending.parent,
        target,
        alias: pending.spec.alias.clone(),
        domain: pending.spec.domain,
        target_condition: pending.spec.target.clone(),
        features: pending.features.clone(),
        default_features: pending.spec.default_features,
    });
}

fn activate_node(
    context: &ResolverContext,
    state: &mut ResolveState,
    node_index: usize,
    requested: Vec<String>,
) -> Result<(), ProjectError> {
    let node = &mut state.nodes[node_index];
    let mut requested_changed = false;
    for feature in requested {
        requested_changed |= node.features.insert(feature);
    }
    if !requested_changed {
        return Ok(());
    }
    loop {
        let mut changed = false;
        let features = state.nodes[node_index].features.clone();
        for feature in features {
            if state.nodes[node_index].expanded_features.contains(&feature) {
                continue;
            }
            state.nodes[node_index]
                .expanded_features
                .insert(feature.clone());
            changed = true;
            if let Some(references) = state.nodes[node_index]
                .metadata
                .features
                .get(&feature)
                .cloned()
            {
                for reference in references {
                    activate_feature_reference(state, node_index, &reference)?;
                }
            } else if feature == "default"
                || optional_alias(&state.nodes[node_index].metadata, &feature)
            {
                if feature != "default" {
                    state.nodes[node_index]
                        .optional_dependencies
                        .insert(feature);
                }
            } else if let Some(alias) = feature.strip_prefix("dep:") {
                if !has_optional_dependency(&state.nodes[node_index].metadata, alias) {
                    return Err(dependency_error(
                        state.nodes[node_index].metadata.id.name(),
                        format!("feature 引用了不存在的 optional dependency `{alias}`"),
                    ));
                }
                state.nodes[node_index]
                    .optional_dependencies
                    .insert(alias.to_owned());
            } else {
                return Err(dependency_error(
                    state.nodes[node_index].metadata.id.name(),
                    format!("未声明 feature `{feature}`"),
                ));
            }
        }
        refresh_edges(context, state, node_index)?;
        if !changed {
            break;
        }
    }
    Ok(())
}

fn activate_feature_reference(
    state: &mut ResolveState,
    node_index: usize,
    reference: &str,
) -> Result<(), ProjectError> {
    if let Some(alias) = reference.strip_prefix("dep:") {
        if !has_optional_dependency(&state.nodes[node_index].metadata, alias) {
            return Err(dependency_error(
                state.nodes[node_index].metadata.id.name(),
                format!("feature 引用了不存在的 optional dependency `{alias}`"),
            ));
        }
        state.nodes[node_index]
            .optional_dependencies
            .insert(alias.to_owned());
        return Ok(());
    }
    if let Some((alias, feature)) = reference.split_once('/') {
        if !has_dependency(&state.nodes[node_index].metadata, alias) || feature.is_empty() {
            return Err(dependency_error(
                state.nodes[node_index].metadata.id.name(),
                format!("feature 引用了不存在的 dependency `{alias}`"),
            ));
        }
        state.nodes[node_index]
            .dependency_features
            .entry(alias.to_owned())
            .or_default()
            .insert(feature.to_owned());
        return Ok(());
    }
    if !state.nodes[node_index]
        .metadata
        .features
        .contains_key(reference)
    {
        return Err(dependency_error(
            state.nodes[node_index].metadata.id.name(),
            format!("feature 引用了不存在的 feature `{reference}`"),
        ));
    }
    state.nodes[node_index]
        .features
        .insert(reference.to_owned());
    Ok(())
}

fn refresh_edges(
    context: &ResolverContext,
    state: &mut ResolveState,
    node_index: usize,
) -> Result<(), ProjectError> {
    let domain = state.nodes[node_index].domain;
    let root = state.nodes[node_index].root;
    let features = state.nodes[node_index].features.clone();
    let optional = state.nodes[node_index].optional_dependencies.clone();
    let dependency_features = state.nodes[node_index].dependency_features.clone();
    let mut dependencies = state.nodes[node_index].metadata.dependencies.clone();
    dependencies.sort();
    for dependency in dependencies {
        if !active_declaration(domain, root, dependency.domain)
            || dependency.target.as_ref().is_some_and(|condition| {
                !condition.matches(if domain == DependencyDomain::Build {
                    &context.host
                } else {
                    &context.target
                })
            })
            || (dependency.optional
                && !optional.contains(&dependency.alias)
                && !features.contains(&dependency.alias))
        {
            continue;
        }
        let mut child_features = dependency.features.iter().cloned().collect::<BTreeSet<_>>();
        if dependency.default_features {
            child_features.insert("default".to_owned());
        }
        if let Some(requested) = dependency_features.get(&dependency.alias) {
            child_features.extend(requested.iter().cloned());
        }
        let edge = state.edges.iter().position(|edge| {
            edge.parent == node_index
                && edge.alias == dependency.alias
                && edge.domain == dependency.domain
                && edge.target_condition == dependency.target
        });
        if let Some(edge_index) = edge {
            let target = state.edges[edge_index].target;
            state.edges[edge_index]
                .features
                .extend(child_features.iter().cloned());
            activate_node(context, state, target, child_features.into_iter().collect())?;
        } else if let Some(pending) = state.pending.iter_mut().find(|pending| {
            pending.parent == node_index
                && pending.spec.alias == dependency.alias
                && pending.spec.domain == dependency.domain
                && pending.spec.target == dependency.target
        }) {
            pending.features.extend(child_features);
        } else {
            state.pending.push(PendingEdge {
                parent: node_index,
                spec: dependency,
                features: child_features,
            });
        }
    }
    state.pending.sort_by(|left, right| {
        (
            state.nodes[left.parent].metadata.id.clone(),
            left.parent,
            left.spec.domain,
            left.spec.alias.clone(),
            left.spec.package.clone(),
        )
            .cmp(&(
                state.nodes[right.parent].metadata.id.clone(),
                right.parent,
                right.spec.domain,
                right.spec.alias.clone(),
                right.spec.package.clone(),
            ))
    });
    Ok(())
}

fn active_declaration(domain: DependencyDomain, root: bool, declaration: DependencyDomain) -> bool {
    match domain {
        DependencyDomain::Normal => declaration == DependencyDomain::Normal,
        DependencyDomain::Test => {
            if root {
                matches!(
                    declaration,
                    DependencyDomain::Normal | DependencyDomain::Test
                )
            } else {
                declaration == DependencyDomain::Normal
            }
        }
        DependencyDomain::Build => {
            if root {
                declaration == DependencyDomain::Build
            } else {
                declaration == DependencyDomain::Normal
            }
        }
    }
}

fn optional_alias(metadata: &PackageMetadata, feature: &str) -> bool {
    has_optional_dependency(metadata, feature) && !metadata.features.contains_key(feature)
}

fn has_optional_dependency(metadata: &PackageMetadata, alias: &str) -> bool {
    metadata
        .dependencies
        .iter()
        .any(|dependency| dependency.alias == alias && dependency.optional)
}

fn has_dependency(metadata: &PackageMetadata, alias: &str) -> bool {
    metadata
        .dependencies
        .iter()
        .any(|dependency| dependency.alias == alias)
}

fn validate_acyclic(state: &ResolveState) -> Result<(), ProjectError> {
    let mut marks = vec![0_u8; state.nodes.len()];
    for node in 0..state.nodes.len() {
        visit_cycle(state, node, &mut marks)?;
    }
    Ok(())
}

fn visit_cycle(state: &ResolveState, node: usize, marks: &mut [u8]) -> Result<(), ProjectError> {
    match marks[node] {
        1 => {
            return Err(dependency_error(
                state.nodes[node].metadata.id.name(),
                "依赖图包含循环",
            ));
        }
        2 => return Ok(()),
        _ => {}
    }
    marks[node] = 1;
    for edge in state.edges.iter().filter(|edge| edge.parent == node) {
        visit_cycle(state, edge.target, marks)?;
    }
    marks[node] = 2;
    Ok(())
}
