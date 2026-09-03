use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
};

use super::{
    error::ProjectError,
    manifest::{RawManifest, RawWorkspace},
    model::Package,
};

pub(crate) fn discover_workspace_members(
    root: &Path,
    workspace: &RawWorkspace,
) -> Result<Vec<PathBuf>, ProjectError> {
    let mut members = BTreeSet::new();
    for pattern in &workspace.members {
        for directory in expand_directory_pattern(root, pattern)? {
            let manifest = directory.join("gugu.toml");
            if !manifest.is_file() {
                return Err(ProjectError::WorkspaceMember {
                    path: directory,
                    workspace: root.to_path_buf(),
                });
            }
            members.insert(
                fs::canonicalize(manifest).map_err(|error| ProjectError::Io {
                    path: directory,
                    message: error.to_string(),
                })?,
            );
        }
    }
    let mut excluded = BTreeSet::new();
    for pattern in &workspace.exclude {
        excluded.extend(expand_directory_pattern(root, pattern)?);
    }
    members.retain(|manifest| {
        manifest
            .parent()
            .is_some_and(|directory| !excluded.contains(directory))
    });
    Ok(members.into_iter().collect())
}

pub(crate) fn resolve_default_members(
    root: &Path,
    workspace_manifest: &RawManifest,
    packages: &[Package],
) -> Result<Vec<PathBuf>, ProjectError> {
    let Some(workspace) = workspace_manifest.workspace.as_ref() else {
        return Ok(Vec::new());
    };
    let mut defaults = BTreeSet::new();
    for pattern in &workspace.default_members {
        for directory in expand_directory_pattern(root, pattern)? {
            if !packages.iter().any(|package| package.root() == directory) {
                return Err(ProjectError::WorkspaceMember {
                    path: directory,
                    workspace: root.to_path_buf(),
                });
            }
            defaults.insert(directory);
        }
    }
    Ok(defaults.into_iter().collect())
}

fn expand_directory_pattern(root: &Path, pattern: &str) -> Result<Vec<PathBuf>, ProjectError> {
    let components = split_pattern(pattern).ok_or_else(|| ProjectError::WorkspaceMember {
        path: root.join(pattern),
        workspace: root.to_path_buf(),
    })?;
    let mut directories = Vec::new();
    collect_directories(root, root, &mut directories)?;
    let mut matches = directories
        .into_iter()
        .filter(|directory| {
            let relative = directory
                .strip_prefix(root)
                .expect("directory is under root")
                .components()
                .filter_map(|component| component.as_os_str().to_str())
                .map(str::to_owned)
                .collect::<Vec<_>>();
            matches_pattern(&components, &relative)
        })
        .collect::<Vec<_>>();
    matches.sort();
    if matches.is_empty() {
        return Err(ProjectError::WorkspaceMember {
            path: root.join(pattern),
            workspace: root.to_path_buf(),
        });
    }
    Ok(matches)
}

fn collect_directories(
    root: &Path,
    directory: &Path,
    output: &mut Vec<PathBuf>,
) -> Result<(), ProjectError> {
    output.push(directory.to_path_buf());
    let mut entries = fs::read_dir(directory)
        .map_err(|error| ProjectError::Io {
            path: directory.to_path_buf(),
            message: error.to_string(),
        })?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| ProjectError::Io {
            path: directory.to_path_buf(),
            message: error.to_string(),
        })?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let file_type = entry.file_type().map_err(|error| ProjectError::Io {
            path: entry.path(),
            message: error.to_string(),
        })?;
        if file_type.is_symlink() {
            continue;
        }
        if file_type.is_dir() {
            let path = entry.path();
            if path.starts_with(root) {
                collect_directories(root, &path, output)?;
            }
        }
    }
    Ok(())
}

fn split_pattern(pattern: &str) -> Option<Vec<String>> {
    if pattern.is_empty() || pattern.starts_with('/') || pattern.contains('\\') {
        return None;
    }
    let mut components = Vec::new();
    for component in pattern.split('/') {
        if component.is_empty() || component == "." || component == ".." {
            return None;
        }
        components.push(component.to_owned());
    }
    Some(components)
}

fn matches_pattern(pattern: &[String], path: &[String]) -> bool {
    if pattern.is_empty() {
        return path.is_empty();
    }
    if pattern[0] == "**" {
        return matches_pattern(&pattern[1..], path)
            || (!path.is_empty() && matches_pattern(pattern, &path[1..]));
    }
    !path.is_empty()
        && wildcard_component(&pattern[0], &path[0])
        && matches_pattern(&pattern[1..], &path[1..])
}

fn wildcard_component(pattern: &str, value: &str) -> bool {
    let pattern = pattern.as_bytes();
    let value = value.as_bytes();
    let mut pattern_index = 0;
    let mut value_index = 0;
    let mut star = None;
    let mut star_value = 0;
    while value_index < value.len() {
        if pattern_index < pattern.len() && pattern[pattern_index] == value[value_index] {
            pattern_index += 1;
            value_index += 1;
        } else if pattern_index < pattern.len() && pattern[pattern_index] == b'*' {
            star = Some(pattern_index);
            pattern_index += 1;
            star_value = value_index;
        } else if let Some(star_index) = star {
            pattern_index = star_index + 1;
            star_value += 1;
            value_index = star_value;
        } else {
            return false;
        }
    }
    while pattern_index < pattern.len() && pattern[pattern_index] == b'*' {
        pattern_index += 1;
    }
    pattern_index == pattern.len()
}

pub(crate) fn relative_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .components()
        .filter_map(|component| component.as_os_str().to_str())
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
#[path = "workspace_tests.rs"]
mod tests;
