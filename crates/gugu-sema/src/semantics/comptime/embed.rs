//! `embed_file` 的编译输入根：把逻辑源目录映射回物理目录，并把
//! 嵌入路径约束在输入根之内（含符号链接解析后的最终位置）。
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

/// 一次编译可见的包输入根。
///
/// 快照加载只保留逻辑路径；本结构由 `CompileInput` 装载期按输入形态重建：
/// `File` 输入的根是文件所在目录，`Project` 的根是 `package_root`，内存
/// 源码（`SingleFileSource`）没有文件系统边界，不提供宿主。
#[derive(Clone, Debug)]
pub struct EmbedHost {
    /// canonicalize 后的输入根：相对路径与符号链接解析都不得越过它。
    root: PathBuf,
    /// 逻辑目录（如 `src/util`）→ 物理目录；调用文件目录先查表，缺失回退 `root` 拼接。
    dirs: BTreeMap<PathBuf, PathBuf>,
    /// 已解析的文件：canonical 路径 → 字节。编译输入登记（action key）依赖它，
    /// 记录缺失会让增量缓存把旧字节当成当前输入。
    recorded: RefCell<BTreeMap<PathBuf, Vec<u8>>>,
}

impl EmbedHost {
    /// `root` 为已 canonicalize 的物理输入根，`dirs` 把每个源文件的
    /// 逻辑父目录映射回物理父目录。
    pub fn new(root: PathBuf, dirs: BTreeMap<PathBuf, PathBuf>) -> Self {
        Self {
            root,
            dirs,
            recorded: RefCell::new(BTreeMap::new()),
        }
    }

    /// 编译期实际嵌入的文件集合；供 action key 把内容纳入编译输入。
    pub fn recorded(&self) -> BTreeMap<PathBuf, Vec<u8>> {
        self.recorded.borrow().clone()
    }

    /// 把调用源文件目录下的相对路径解析为文件字节；越界与读失败都是编译错误。
    pub fn resolve(&self, caller_dir: &Path, relative: &str) -> Result<Vec<u8>, String> {
        let relative_path = Path::new(relative);
        if relative_path.is_absolute()
            || relative_path
                .components()
                .any(|component| matches!(component, Component::Prefix(_)))
        {
            return Err(format!("embed_file 参数 `{relative}` 必须是相对路径"));
        }
        let physical_dir = self
            .dirs
            .get(caller_dir)
            .cloned()
            .unwrap_or_else(|| self.root.join(caller_dir));
        let joined = physical_dir.join(relative_path);
        let canonical = joined
            .canonicalize()
            .map_err(|error| format!("embed_file 无法解析 `{relative}`: {error}"))?;
        if !canonical.starts_with(&self.root) {
            return Err(format!(
                "embed_file 参数 `{relative}` 越出编译输入根 {}",
                self.root.display()
            ));
        }
        canonical
            .metadata()
            .ok()
            .filter(|meta| meta.is_file())
            .ok_or_else(|| format!("embed_file 目标 `{relative}` 不是文件"))?;
        let bytes = std::fs::read(&canonical)
            .map_err(|error| format!("embed_file 读取 `{relative}` 失败: {error}"))?;
        self.recorded.borrow_mut().insert(canonical, bytes.clone());
        Ok(bytes)
    }
}
