//! demand-driven query 状态机与编译中间对象缓存。

use std::{
    collections::BTreeMap,
    fmt,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{Arc, Condvar, Mutex},
};

const OBJECT_MAGIC: [u8; 8] = *b"GUGUCV01";
const OBJECT_HEADER_LEN: usize = 56;
const MAX_OBJECT_PAYLOAD_LEN: usize = u32::MAX as usize;
const OBJECT_CACHE_VERSION: &str = "v1";

/// 固定登记的 compiler query 编号。
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[repr(u16)]
pub enum QueryKind {
    /// 固化源码快照。
    SourceSnapshot = 1,
    /// 词法分析。
    Lex = 2,
    /// 解析源码。
    Parse = 3,
    /// 配置裁项。
    Configure = 4,
    /// 收集定义。
    CollectDefinitions = 5,
    /// 解析导入。
    ResolveImports = 6,
    /// 降低 HIR。
    LowerHir = 7,
    /// 类型检查。
    TypeCheck = 8,
    /// trait 选择。
    TraitSelection = 9,
    /// 求值早期 comptime。
    EvaluateEarlyComptime = 10,
    /// 构造泛型 GIR。
    BuildGenericGir = 11,
    /// 收集单态化根。
    CollectMonoRoots = 12,
    /// 实例化 GIR。
    InstantiateGir = 13,
    /// 计算布局。
    LayoutOf = 14,
    /// 构造 LIR。
    BuildLir = 15,
    /// 生成机器码片段。
    CodegenFragment = 16,
    /// 生成类型元数据。
    TypeMetadata = 17,
    /// 生成运行时元数据。
    RuntimeMetadata = 18,
    /// 规划镜像。
    PlanImage = 19,
    /// 写出镜像。
    EmitImage = 20,
    /// 解析生成源码。
    ParseSource = 21,
    /// 展开源码宏。
    ExpandSourceMacro = 22,
    /// 函数抽象分析摘要。
    FunctionAnalysisSummary = 23,
    /// 全程序抽象分析。
    WholeProgramAnalysis = 24,
    /// 冻结具体类型集合。
    FreezeTypeUniverse = 25,
    /// 求值 late comptime。
    EvaluateLateComptime = 26,
    /// SCC 抽象分析摘要。
    AnalysisSccSummary = 27,
    /// 跨 package 的公共函数摘要。
    PublicFunctionSummary = 28,
    /// 逃逸分析与 placement 选择。
    EscapeAndPlacement = 29,
}

/// query 的规范身份。
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct QueryKey {
    kind: QueryKind,
    schema_version: u32,
    canonical_key: Vec<u8>,
}

impl QueryKey {
    /// 用固定 kind、schema 版本与规范 key 字节创建身份。
    pub fn new(kind: QueryKind, schema_version: u32, canonical_key: impl AsRef<[u8]>) -> Self {
        Self {
            kind,
            schema_version,
            canonical_key: canonical_key.as_ref().to_vec(),
        }
    }

    /// 返回 query kind。
    pub const fn kind(&self) -> QueryKind {
        self.kind
    }

    /// 返回该 kind 的 schema 版本。
    pub const fn schema_version(&self) -> u32 {
        self.schema_version
    }

    /// 返回规范 key 字节。
    pub fn canonical_key(&self) -> &[u8] {
        &self.canonical_key
    }

    /// 返回域隔离的 key 摘要。
    pub fn fingerprint(&self) -> [u8; 32] {
        let mut bytes = Vec::with_capacity(2 + 4 + 8 + self.canonical_key.len());
        bytes.extend_from_slice(&(self.kind as u16).to_le_bytes());
        bytes.extend_from_slice(&self.schema_version.to_le_bytes());
        encode_bytes(&mut bytes, &self.canonical_key);
        hash_domain("gugu-query-key-v1", &bytes)
    }
}

/// query 读取的直接依赖及其当时指纹。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DependencyFingerprint {
    key: QueryKey,
    fingerprint: [u8; 32],
}

impl DependencyFingerprint {
    /// 创建依赖记录。
    pub fn new(key: QueryKey, fingerprint: [u8; 32]) -> Self {
        Self { key, fingerprint }
    }

    /// 返回依赖 query 身份。
    pub fn key(&self) -> &QueryKey {
        &self.key
    }

    /// 返回依赖结果或输入的指纹。
    pub const fn fingerprint(&self) -> [u8; 32] {
        self.fingerprint
    }
}

/// session 内 query cell 的状态。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QueryState {
    /// 尚未计算。
    Uncomputed,
    /// 一个请求者正在计算。
    Computing,
    /// 已得到可复用的成功结果。
    Complete,
    /// 已失败，只在当前 session memoize。
    Failed,
    /// 上游 action 已取消。
    Cancelled,
}

/// 成功 query 的不可变结果。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QueryResult {
    payload: Arc<[u8]>,
    result_fingerprint: [u8; 32],
    dependencies: Arc<[DependencyFingerprint]>,
}

impl QueryResult {
    /// 用规范成功结果、排序诊断和直接依赖构造结果。
    pub fn new(
        key: &QueryKey,
        payload: impl Into<Vec<u8>>,
        diagnostics: impl AsRef<[u8]>,
        mut dependencies: Vec<DependencyFingerprint>,
    ) -> Self {
        dependencies.sort_by(|left, right| left.key.cmp(&right.key));
        dependencies.dedup_by(|left, right| left.key == right.key);
        let payload = payload.into();
        let mut canonical = Vec::with_capacity(
            2 + 4
                + 8
                + key.canonical_key.len()
                + 8
                + payload.len()
                + 8
                + diagnostics.as_ref().len(),
        );
        canonical.extend_from_slice(&(key.kind as u16).to_le_bytes());
        canonical.extend_from_slice(&key.schema_version.to_le_bytes());
        encode_bytes(&mut canonical, &key.canonical_key);
        encode_bytes(&mut canonical, &payload);
        encode_bytes(&mut canonical, diagnostics.as_ref());
        Self {
            payload: Arc::from(payload),
            result_fingerprint: hash_domain("gugu-query-result-v1", &canonical),
            dependencies: Arc::from(dependencies),
        }
    }

    /// 返回序列化结果。
    pub fn payload(&self) -> &[u8] {
        &self.payload
    }

    /// 返回结果指纹。
    pub const fn fingerprint(&self) -> [u8; 32] {
        self.result_fingerprint
    }

    /// 返回已按稳定 key 排序的直接依赖。
    pub fn dependencies(&self) -> &[DependencyFingerprint] {
        &self.dependencies
    }
}

/// query 执行错误。
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum QueryError {
    /// 同一依赖栈中发生 cycle。
    Cycle(Vec<QueryKey>),
    /// query 计算失败。
    Failed(String),
    /// query 或上游 action 已取消。
    Cancelled,
}

impl fmt::Display for QueryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cycle(_) => formatter.write_str("query 依赖形成循环"),
            Self::Failed(message) => formatter.write_str(message),
            Self::Cancelled => formatter.write_str("query 已取消"),
        }
    }
}

impl std::error::Error for QueryError {}

/// 传给计算闭包的依赖记录器。
#[derive(Debug)]
pub struct QueryContext {
    dependencies: Vec<DependencyFingerprint>,
}

impl QueryContext {
    /// 登记本 query 读取的输入或已完成 query 结果。
    pub fn record_dependency(&mut self, key: QueryKey, fingerprint: [u8; 32]) {
        self.dependencies
            .push(DependencyFingerprint::new(key, fingerprint));
    }

    fn take_dependencies(&mut self) -> Vec<DependencyFingerprint> {
        std::mem::take(&mut self.dependencies)
    }
}

/// demand-driven query session。
#[derive(Debug, Default)]
pub struct QueryEngine {
    cells: Mutex<BTreeMap<QueryKey, Arc<QueryCell>>>,
}

impl QueryEngine {
    /// 创建空 query session。
    pub const fn new() -> Self {
        Self {
            cells: Mutex::new(BTreeMap::new()),
        }
    }

    /// 取得 query 当前状态；未登记 key 返回 `Uncomputed`。
    pub fn state(&self, key: &QueryKey) -> QueryState {
        let cells = self.cells.lock().expect("query registry lock poisoned");
        let Some(cell) = cells.get(key) else {
            return QueryState::Uncomputed;
        };
        cell.state.lock().expect("query cell lock poisoned").state
    }

    /// 取消尚在计算的 query；取消不会写持久缓存。
    pub fn cancel(&self, key: &QueryKey) -> bool {
        let cells = self.cells.lock().expect("query registry lock poisoned");
        let Some(cell) = cells.get(key) else {
            return false;
        };
        let mut state = cell.state.lock().expect("query cell lock poisoned");
        if state.state != QueryState::Computing {
            return false;
        }
        state.state = QueryState::Cancelled;
        cell.ready.notify_all();
        true
    }

    /// 只计算一次同一 query，其他请求者等待并复用相同不可变结果。
    pub fn compute<F>(&self, key: QueryKey, compute: F) -> Result<QueryResult, QueryError>
    where
        F: FnOnce(&mut QueryContext) -> Result<(Vec<u8>, Vec<u8>), QueryError>,
    {
        let cell = {
            let mut cells = self.cells.lock().expect("query registry lock poisoned");
            Arc::clone(
                cells
                    .entry(key.clone())
                    .or_insert_with(|| Arc::new(QueryCell::default())),
            )
        };
        let mut state = cell.state.lock().expect("query cell lock poisoned");
        loop {
            match state.state {
                QueryState::Complete => {
                    return Ok(state.result.clone().expect("complete query result"));
                }
                QueryState::Failed | QueryState::Cancelled => {
                    return Err(state.error.clone().expect("terminal query error"));
                }
                QueryState::Computing => {
                    state = cell.ready.wait(state).expect("query cell lock poisoned");
                }
                QueryState::Uncomputed => {
                    state.state = QueryState::Computing;
                    break;
                }
            }
        }
        drop(state);

        let mut context = QueryContext {
            dependencies: Vec::new(),
        };
        let outcome = compute(&mut context).map(|(payload, diagnostics)| {
            QueryResult::new(&key, payload, diagnostics, context.take_dependencies())
        });

        let mut state = cell.state.lock().expect("query cell lock poisoned");
        if state.state == QueryState::Cancelled {
            state.error = Some(QueryError::Cancelled);
            cell.ready.notify_all();
            return Err(QueryError::Cancelled);
        }
        match outcome {
            Ok(result) => {
                state.state = QueryState::Complete;
                state.result = Some(result.clone());
                cell.ready.notify_all();
                Ok(result)
            }
            Err(error) => {
                state.state = match error {
                    QueryError::Cancelled => QueryState::Cancelled,
                    QueryError::Cycle(_) | QueryError::Failed(_) => QueryState::Failed,
                };
                state.error = Some(error.clone());
                cell.ready.notify_all();
                Err(error)
            }
        }
    }
}

#[derive(Debug, Default)]
struct QueryCell {
    state: Mutex<QueryCellState>,
    ready: Condvar,
}

#[derive(Debug)]
struct QueryCellState {
    state: QueryState,
    result: Option<QueryResult>,
    error: Option<QueryError>,
}

impl Default for QueryCellState {
    fn default() -> Self {
        Self {
            state: QueryState::Uncomputed,
            result: None,
            error: None,
        }
    }
}

/// 内容寻址编译对象的完整身份。
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ObjectKey([u8; 32]);

impl ObjectKey {
    /// 返回完整摘要字节。
    pub const fn bytes(self) -> [u8; 32] {
        self.0
    }

    /// 返回小写十六进制文件名。
    pub fn hex(self) -> String {
        hex_encode(&self.0)
    }
}

/// 编译对象缓存错误。
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ObjectError {
    /// 文件系统操作失败。
    Io {
        /// 相关路径。
        path: PathBuf,
        /// 稳定错误文本。
        message: String,
    },
    /// 对象格式或完整性无效。
    Invalid {
        /// 相关路径。
        path: PathBuf,
        /// 稳定错误文本。
        message: String,
    },
    /// 读取到的对象不匹配所请求的 kind/schema。
    Mismatch {
        /// 相关路径。
        path: PathBuf,
        /// 稳定错误文本。
        message: String,
    },
}

impl fmt::Display for ObjectError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, message }
            | Self::Invalid { path, message }
            | Self::Mismatch { path, message } => {
                write!(formatter, "{}: {message}", path.display())
            }
        }
    }
}

impl std::error::Error for ObjectError {}

/// 编译中间对象的内容寻址存储。
#[derive(Clone, Debug)]
pub struct ObjectCache {
    root: PathBuf,
}

impl ObjectCache {
    /// 创建位于给定全局 cache 根下的 `compile/v1` 存储。
    pub fn new(cache_root: impl Into<PathBuf>) -> Self {
        Self {
            root: cache_root.into().join("compile").join(OBJECT_CACHE_VERSION),
        }
    }

    /// 返回编译对象存储根目录。
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 写入已验证 payload，并以 create-if-absent 原子发布。
    pub fn store(
        &self,
        kind: QueryKind,
        schema_version: u32,
        payload: &[u8],
    ) -> Result<ObjectKey, ObjectError> {
        if payload.len() > MAX_OBJECT_PAYLOAD_LEN {
            return Err(ObjectError::Invalid {
                path: self.root.clone(),
                message: "对象 payload 超过 u32 最大长度".to_owned(),
            });
        }
        let key = object_key(kind, schema_version, payload);
        let path = self.object_path(key);
        if path.exists() {
            self.load(key, kind, schema_version, |_| Ok(()))?;
            return Ok(key);
        }
        let directory = path.parent().expect("object path has parent");
        fs::create_dir_all(directory).map_err(|error| io_error(directory, error))?;
        let temporary_directory = self.root.join("tmp");
        fs::create_dir_all(&temporary_directory)
            .map_err(|error| io_error(&temporary_directory, error))?;
        let temporary = temporary_directory.join(format!("{}.tmp", key.hex()));
        let bytes = encode_object(kind, schema_version, payload);
        write_new_file(&temporary, &bytes)?;
        self.validate_file(&temporary, kind, schema_version, |_| Ok(()))?;
        match fs::hard_link(&temporary, &path) {
            Ok(()) => {
                fs::remove_file(&temporary).map_err(|error| io_error(&temporary, error))?;
                Ok(key)
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                fs::remove_file(&temporary).map_err(|remove| io_error(&temporary, remove))?;
                self.load(key, kind, schema_version, |_| Ok(()))?;
                Ok(key)
            }
            Err(error) => Err(io_error(&path, error)),
        }
    }

    /// 读取并验证对象；verifier 只会收到完整验证后的 payload。
    pub fn load<F>(
        &self,
        key: ObjectKey,
        kind: QueryKind,
        schema_version: u32,
        verifier: F,
    ) -> Result<Vec<u8>, ObjectError>
    where
        F: FnOnce(&[u8]) -> Result<(), String>,
    {
        let path = self.object_path(key);
        match self.validate_file(&path, kind, schema_version, verifier) {
            Ok(payload) => Ok(payload),
            Err(error @ ObjectError::Invalid { .. } | error @ ObjectError::Mismatch { .. }) => {
                self.quarantine(&path)?;
                Err(error)
            }
            Err(error) => Err(error),
        }
    }

    fn object_path(&self, key: ObjectKey) -> PathBuf {
        let hex = key.hex();
        self.root.join("objects").join(&hex[..2]).join(hex)
    }

    fn validate_file<F>(
        &self,
        path: &Path,
        kind: QueryKind,
        schema_version: u32,
        verifier: F,
    ) -> Result<Vec<u8>, ObjectError>
    where
        F: FnOnce(&[u8]) -> Result<(), String>,
    {
        let mut file = File::open(path).map_err(|error| io_error(path, error))?;
        let metadata = file.metadata().map_err(|error| io_error(path, error))?;
        if metadata.len() < OBJECT_HEADER_LEN as u64 {
            return Err(invalid_object(path, "对象小于固定 header"));
        }
        let mut header = [0_u8; OBJECT_HEADER_LEN];
        file.read_exact(&mut header)
            .map_err(|error| io_error(path, error))?;
        if header[..8] != OBJECT_MAGIC {
            return Err(invalid_object(path, "对象 magic 无效"));
        }
        let actual_kind = u16::from_le_bytes(header[8..10].try_into().expect("u16 header field"));
        let flags = u16::from_le_bytes(header[10..12].try_into().expect("u16 header field"));
        let actual_schema =
            u32::from_le_bytes(header[12..16].try_into().expect("u32 header field"));
        let payload_len = u64::from_le_bytes(header[16..24].try_into().expect("u64 header field"));
        if actual_kind != kind as u16 || actual_schema != schema_version {
            return Err(ObjectError::Mismatch {
                path: path.to_path_buf(),
                message: "对象 kind 或 schema version 不匹配".to_owned(),
            });
        }
        if flags != 0 || payload_len > MAX_OBJECT_PAYLOAD_LEN as u64 {
            return Err(invalid_object(path, "对象 flags 或 payload 长度无效"));
        }
        if metadata.len() != OBJECT_HEADER_LEN as u64 + payload_len {
            return Err(invalid_object(path, "对象文件长度与 header 不一致"));
        }
        let mut payload = vec![0_u8; payload_len as usize];
        file.read_exact(&mut payload)
            .map_err(|error| io_error(path, error))?;
        if *blake3::hash(&domain_bytes("gugu-object-v1", &payload)).as_bytes() != header[24..56] {
            return Err(invalid_object(path, "对象 payload BLAKE3 摘要不匹配"));
        }
        verifier(&payload).map_err(|message| ObjectError::Invalid {
            path: path.to_path_buf(),
            message,
        })?;
        Ok(payload)
    }

    fn quarantine(&self, path: &Path) -> Result<(), ObjectError> {
        if !path.exists() {
            return Ok(());
        }
        let quarantine = self.root.join("quarantine");
        fs::create_dir_all(&quarantine).map_err(|error| io_error(&quarantine, error))?;
        let destination = quarantine.join(path.file_name().expect("object file name"));
        fs::rename(path, &destination).map_err(|error| io_error(path, error))
    }
}

fn object_key(kind: QueryKind, schema_version: u32, payload: &[u8]) -> ObjectKey {
    let mut canonical = Vec::with_capacity(2 + 4 + 8 + payload.len());
    canonical.extend_from_slice(&(kind as u16).to_le_bytes());
    canonical.extend_from_slice(&schema_version.to_le_bytes());
    encode_bytes(&mut canonical, payload);
    ObjectKey(hash_domain("gugu-object-v1", &canonical))
}

fn encode_object(kind: QueryKind, schema_version: u32, payload: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(OBJECT_HEADER_LEN + payload.len());
    bytes.extend_from_slice(&OBJECT_MAGIC);
    bytes.extend_from_slice(&(kind as u16).to_le_bytes());
    bytes.extend_from_slice(&0_u16.to_le_bytes());
    bytes.extend_from_slice(&schema_version.to_le_bytes());
    bytes.extend_from_slice(&(payload.len() as u64).to_le_bytes());
    bytes.extend_from_slice(blake3::hash(&domain_bytes("gugu-object-v1", payload)).as_bytes());
    bytes.extend_from_slice(payload);
    bytes
}

fn write_new_file(path: &Path, bytes: &[u8]) -> Result<(), ObjectError> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|error| io_error(path, error))?;
    file.write_all(bytes)
        .map_err(|error| io_error(path, error))?;
    file.sync_all().map_err(|error| io_error(path, error))
}

fn io_error(path: &Path, error: std::io::Error) -> ObjectError {
    ObjectError::Io {
        path: path.to_path_buf(),
        message: error.to_string(),
    }
}

fn invalid_object(path: &Path, message: impl Into<String>) -> ObjectError {
    ObjectError::Invalid {
        path: path.to_path_buf(),
        message: message.into(),
    }
}

fn encode_bytes(output: &mut Vec<u8>, bytes: &[u8]) {
    output.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
    output.extend_from_slice(bytes);
}

fn domain_bytes(domain: &str, bytes: &[u8]) -> Vec<u8> {
    let mut domain_bytes = Vec::with_capacity(domain.len() + 1 + bytes.len());
    domain_bytes.extend_from_slice(domain.as_bytes());
    domain_bytes.push(0);
    domain_bytes.extend_from_slice(bytes);
    domain_bytes
}

fn hash_domain(domain: &str, bytes: &[u8]) -> [u8; 32] {
    *blake3::hash(&domain_bytes(domain, bytes)).as_bytes()
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        text.push(HEX[(byte >> 4) as usize] as char);
        text.push(HEX[(byte & 15) as usize] as char);
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{sync::Barrier, thread};
    use tempfile::TempDir;

    #[test]
    fn concurrent_query_is_computed_once() {
        let engine = Arc::new(QueryEngine::new());
        let key = QueryKey::new(QueryKind::Parse, 1, b"src/main.gg");
        let barrier = Arc::new(Barrier::new(2));
        let worker_engine = Arc::clone(&engine);
        let worker_key = key.clone();
        let worker_barrier = Arc::clone(&barrier);
        let worker = thread::spawn(move || {
            worker_engine.compute(worker_key, |_| {
                worker_barrier.wait();
                Ok((b"parsed".to_vec(), Vec::new()))
            })
        });
        barrier.wait();
        let result = engine
            .compute(key.clone(), |_| {
                Err(QueryError::Failed("重复执行".to_owned()))
            })
            .expect("等待同一 query 的结果");
        assert_eq!(result.payload(), b"parsed");
        assert_eq!(
            worker.join().expect("worker 完成").expect("worker result"),
            result
        );
        assert_eq!(engine.state(&key), QueryState::Complete);
    }

    #[test]
    fn result_fingerprint_ignores_equal_dependency_reexecution() {
        let key = QueryKey::new(QueryKind::Parse, 1, b"src/main.gg");
        let dependency = QueryKey::new(QueryKind::Lex, 1, b"src/main.gg");
        let first = QueryResult::new(
            &key,
            b"ast".to_vec(),
            b"",
            vec![DependencyFingerprint::new(dependency.clone(), [1; 32])],
        );
        let second = QueryResult::new(
            &key,
            b"ast".to_vec(),
            b"",
            vec![DependencyFingerprint::new(dependency, [2; 32])],
        );
        assert_eq!(first.fingerprint(), second.fingerprint());
        assert_ne!(first.dependencies(), second.dependencies());
    }

    #[test]
    fn failed_query_is_not_persisted_as_success() {
        let engine = QueryEngine::new();
        let key = QueryKey::new(QueryKind::Parse, 1, b"src/main.gg");
        let failure = engine.compute(key.clone(), |_| {
            Err(QueryError::Failed("解析失败".to_owned()))
        });
        assert_eq!(failure, Err(QueryError::Failed("解析失败".to_owned())));
        assert_eq!(engine.state(&key), QueryState::Failed);
    }

    #[test]
    fn object_cache_verifies_and_quarantines_tampering() {
        let root = TempDir::new().expect("cache root");
        let cache = ObjectCache::new(root.path());
        let key = cache
            .store(QueryKind::Parse, 3, b"verified ast")
            .expect("store object");
        assert_eq!(
            cache
                .load(key, QueryKind::Parse, 3, |payload| {
                    (payload == b"verified ast")
                        .then_some(())
                        .ok_or_else(|| "IR verifier rejected payload".to_owned())
                })
                .expect("load object"),
            b"verified ast"
        );
        let object = cache.object_path(key);
        fs::write(&object, b"tampered").expect("tamper object");
        assert!(matches!(
            cache.load(key, QueryKind::Parse, 3, |_| Ok(())),
            Err(ObjectError::Invalid { .. })
        ));
        assert!(!object.exists());
        assert!(cache.root().join("quarantine").join(key.hex()).exists());
    }
}
