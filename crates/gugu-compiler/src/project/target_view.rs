use std::{
    collections::BTreeSet,
    fs,
    path::{Component, Path, PathBuf},
};

use super::{CacheError, invalid_error, io_error, write_file};

/// workspace target 视图物化器。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TargetView {
    root: PathBuf,
}

impl TargetView {
    /// 创建 target 视图句柄。
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// 返回 target 视图根目录。
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 原子物化一个 target 的用户产物集合。
    pub fn materialize(
        &self,
        target: &str,
        artifacts: &[TargetArtifact],
    ) -> Result<Vec<PathBuf>, CacheError> {
        validate_target_name(target)?;
        let target_root = self.root.join(target);
        let mut names = BTreeSet::new();
        for artifact in artifacts {
            if !names.insert(artifact.relative_path.clone()) {
                return Err(CacheError::Invalid {
                    path: target_root.clone(),
                    message: format!("target 产物重复 `{}`", artifact.relative_path.display()),
                });
            }
            validate_relative_path(&artifact.relative_path)?;
        }
        let mut paths = Vec::with_capacity(artifacts.len());
        for artifact in artifacts {
            let path = target_root.join(&artifact.relative_path);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).map_err(|error| io_error(parent, error))?;
            }
            write_file(&path, &artifact.bytes)?;
            paths.push(path);
        }
        Ok(paths)
    }
}

/// target 视图中的一个相对产物。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TargetArtifact {
    relative_path: PathBuf,
    bytes: Vec<u8>,
}

impl TargetArtifact {
    /// 构造 target 产物；路径必须相对且不能穿越目录。
    pub fn new(path: impl Into<PathBuf>, bytes: impl Into<Vec<u8>>) -> Result<Self, CacheError> {
        let path = path.into();
        validate_relative_path(&path)?;
        Ok(Self {
            relative_path: path,
            bytes: bytes.into(),
        })
    }

    /// 返回相对产物路径。
    pub fn relative_path(&self) -> &Path {
        &self.relative_path
    }

    /// 返回产物字节。
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

fn validate_relative_path(path: &Path) -> Result<(), CacheError> {
    let text = path
        .to_str()
        .ok_or_else(|| invalid_error(path, "target 产物路径不是 UTF-8"))?;
    if text.is_empty() || text.len() > 4096 || text.contains('\0') || text.contains('\\') {
        return Err(invalid_error(path, "路径不是规范相对路径"));
    }
    if path.components().any(|component| {
        matches!(component, Component::CurDir | Component::ParentDir)
            || !matches!(component, Component::Normal(_))
    }) {
        return Err(invalid_error(path, "路径包含非法分量"));
    }
    Ok(())
}

fn validate_target_name(name: &str) -> Result<(), CacheError> {
    if name.is_empty()
        || name.contains('/')
        || name.contains('\\')
        || name.contains('\0')
        || name == "."
        || name == ".."
    {
        return Err(invalid_error(Path::new(name), "target 名称无效"));
    }
    Ok(())
}
