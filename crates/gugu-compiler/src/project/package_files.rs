use std::{
    collections::BTreeMap,
    fs,
    io::{Cursor, Read},
    path::{Component, Path},
};

use flate2::read::GzDecoder;
use sha2::{Digest, Sha256};
use tar::Archive;

use super::{CacheError, MAX_CACHE_PATH_BYTES, hex_encode, invalid_error, io_error, update_u64_be};

/// 经过路径检查的 package 文件集合。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PackageFiles {
    files: BTreeMap<String, Vec<u8>>,
}

impl PackageFiles {
    /// 从规范相对路径和原始文件字节构造文件集合。
    pub fn new(files: BTreeMap<String, Vec<u8>>) -> Result<Self, CacheError> {
        for path in files.keys() {
            validate_file_path(path)?;
        }
        Ok(Self { files })
    }

    /// 从 package 目录读取普通文件。
    pub fn from_directory(root: impl AsRef<Path>) -> Result<Self, CacheError> {
        let root = root.as_ref();
        let mut files = BTreeMap::new();
        collect_directory_files(root, root, &mut files)?;
        Self::new(files)
    }

    /// 返回按 UTF-8 字节序排列的文件。
    pub fn files(&self) -> &BTreeMap<String, Vec<u8>> {
        &self.files
    }

    /// 按 package checksum 规范计算 SHA-256。
    pub fn checksum(&self) -> String {
        let mut hasher = Sha256::new();
        hasher.update(b"gugu-package-v1\n");
        for (path, bytes) in &self.files {
            update_u64_be(
                &mut hasher,
                u64::try_from(path.len()).expect("path length fits u64"),
            );
            hasher.update(path.as_bytes());
            update_u64_be(
                &mut hasher,
                u64::try_from(bytes.len()).expect("file length fits u64"),
            );
            hasher.update(bytes);
        }
        hex_encode(&hasher.finalize())
    }
}

pub(super) fn parse_archive(bytes: &[u8]) -> Result<PackageFiles, CacheError> {
    let decoder = GzDecoder::new(Cursor::new(bytes));
    let mut archive = Archive::new(decoder);
    let entries = archive
        .entries()
        .map_err(|error| invalid_error(Path::new("<archive>"), error))?;
    let mut files = BTreeMap::new();
    for entry in entries {
        let mut entry = entry.map_err(|error| invalid_error(Path::new("<archive>"), error))?;
        let path_bytes = entry.path_bytes();
        let path = std::str::from_utf8(&path_bytes)
            .map_err(|_| invalid_error(Path::new("<archive>"), "归档路径不是 UTF-8"))?
            .to_owned();
        validate_file_path(&path)?;
        if entry.header().entry_type().is_dir() {
            continue;
        }
        if !entry.header().entry_type().is_file() {
            return Err(invalid_error(Path::new("<archive>"), "归档包含非普通文件"));
        }
        let mut content = Vec::new();
        entry
            .read_to_end(&mut content)
            .map_err(|error| invalid_error(Path::new("<archive>"), error))?;
        if files.insert(path, content).is_some() {
            return Err(invalid_error(Path::new("<archive>"), "归档包含重复文件"));
        }
    }
    PackageFiles::new(files)
}

fn collect_directory_files(
    root: &Path,
    directory: &Path,
    files: &mut BTreeMap<String, Vec<u8>>,
) -> Result<(), CacheError> {
    let entries = fs::read_dir(directory).map_err(|error| io_error(directory, error))?;
    let mut entries = entries
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| io_error(directory, error))?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        let name = entry.file_name();
        let name = name
            .to_str()
            .ok_or_else(|| invalid_error(&path, "文件名不是 UTF-8"))?;
        if name == "target" || name == "vendor" || name == ".git" || name == ".gugu" {
            continue;
        }
        let file_type = entry.file_type().map_err(|error| io_error(&path, error))?;
        if file_type.is_symlink() {
            return Err(invalid_error(&path, "依赖输入不能包含符号链接"));
        }
        let relative = path
            .strip_prefix(root)
            .map_err(|_| invalid_error(&path, "无法计算 package 相对路径"))?;
        if file_type.is_dir() {
            collect_directory_files(root, &path, files)?;
        } else if file_type.is_file() {
            let logical = relative
                .to_str()
                .ok_or_else(|| invalid_error(&path, "路径不是 UTF-8"))?;
            validate_file_path(logical)?;
            let bytes = fs::read(&path).map_err(|error| io_error(&path, error))?;
            files.insert(logical.replace(std::path::MAIN_SEPARATOR, "/"), bytes);
        } else {
            return Err(invalid_error(&path, "依赖输入包含非普通文件"));
        }
    }
    Ok(())
}

pub(super) fn validate_file_path(path: &str) -> Result<(), CacheError> {
    if path.is_empty()
        || path.len() > MAX_CACHE_PATH_BYTES
        || path.starts_with('/')
        || path.contains('\0')
        || path.contains('\\')
        || path.contains("//")
    {
        return Err(invalid_error(Path::new(path), "路径不是规范相对路径"));
    }
    let path = Path::new(path);
    if path.components().any(|component| {
        matches!(component, Component::CurDir | Component::ParentDir)
            || !matches!(component, Component::Normal(_))
    }) {
        return Err(invalid_error(path, "路径包含非法分量"));
    }
    Ok(())
}
