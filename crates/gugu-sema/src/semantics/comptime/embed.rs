//! `embed_file` 的编译输入根：把逻辑源目录映射回物理目录，并把
//! 嵌入路径约束在输入根之内（含符号链接解析后的最终位置）。
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::rc::Rc;

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
    /// 记录缺失会让增量缓存把旧字节当成当前输入。`Rc` 共享：语义检查会克隆宿主，
    /// 复制出的登记表必须写回同一集合，否则 action key 读到空表。
    recorded: Rc<RefCell<BTreeMap<PathBuf, Vec<u8>>>>,
    packages: BTreeMap<String, Rc<BTreeMap<String, Vec<u8>>>>,
}

impl EmbedHost {
    /// `root` 为已 canonicalize 的物理输入根，`dirs` 把每个源文件的
    /// 逻辑父目录映射回物理父目录。
    pub fn new(root: PathBuf, dirs: BTreeMap<PathBuf, PathBuf>) -> Self {
        Self {
            root,
            dirs,
            recorded: Rc::new(RefCell::new(BTreeMap::new())),
            packages: BTreeMap::new(),
        }
    }

    /// 登记依赖的已验证文件集；嵌入文件始终从同一锁定输入读取。
    pub fn add_package(&mut self, root: String, files: BTreeMap<String, Vec<u8>>) {
        self.packages.insert(root, Rc::new(files));
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
        for (root, files) in &self.packages {
            if let Ok(directory) = caller_dir.strip_prefix(root) {
                let mut path = PathBuf::from(directory);
                for component in relative_path.components() {
                    match component {
                        Component::Normal(segment) => path.push(segment),
                        Component::CurDir => {}
                        Component::ParentDir if path.pop() => {}
                        _ => {
                            return Err(format!(
                                "embed_file 参数 `{relative}` 越出依赖输入根 {root}"
                            ));
                        }
                    }
                }
                let key = path
                    .to_str()
                    .ok_or_else(|| "embed_file 路径不是 UTF-8".to_owned())?
                    .replace(std::path::MAIN_SEPARATOR, "/");
                let bytes = files
                    .get(&key)
                    .ok_or_else(|| format!("embed_file 目标 `{relative}` 不在依赖输入中"))?
                    .clone();
                self.recorded
                    .borrow_mut()
                    .insert(Path::new(root).join(path), bytes.clone());
                return Ok(bytes);
            }
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
