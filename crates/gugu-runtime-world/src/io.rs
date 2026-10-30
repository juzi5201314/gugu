//! `std.io` 的确定性参照模型。
//!
//! 错误用密集 `u8` 编号加可选来源下标，不使用字符串 kind。字节流、缓冲和等待都在
//! 进程内完成：不调用文件系统、网络、进程或真实时钟。

use super::foreign::{AdmitOutcome, BridgeMode, ForeignWorld};
use super::slab::RawInvariant;
use super::text::{CowBytes, TextFault, utf8_decode};
use crate::runtime::coroutine::CoroutineHandle;

/// 组合函数的内部块长度。
pub const IO_CHUNK_BYTES: usize = 8192;
/// 无载荷变体的个数。编号 0..29 是它们，30 是 `Other`。
const BARE_KINDS: u8 = 30;
/// `Other` 的稳定编号。
pub const OTHER: u8 = 30;
/// 错误链的最大深度。超过后 `with_source` 返回 `InvalidInput`。
const MAX_CHAIN_DEPTH: usize = 8;

const NOT_FOUND: u8 = 0;
const PERMISSION_DENIED: u8 = 1;
const ALREADY_EXISTS: u8 = 2;
pub const INVALID_INPUT: u8 = 3;
pub const INVALID_DATA: u8 = 4;
pub const UNEXPECTED_EOF: u8 = 5;
pub const WRITE_ZERO: u8 = 6;
const INTERRUPTED: u8 = 7;
const TIMED_OUT: u8 = 8;
const CANCELLED: u8 = 9;
const CLOSED: u8 = 10;
const BROKEN_PIPE: u8 = 11;
const NOT_CONNECTED: u8 = 12;
const CONNECTION_RESET: u8 = 13;
const CONNECTION_REFUSED: u8 = 14;
const CONNECTION_ABORTED: u8 = 15;
const ADDR_IN_USE: u8 = 16;
const ADDR_NOT_AVAILABLE: u8 = 17;
const NOT_A_DIRECTORY: u8 = 18;
const IS_A_DIRECTORY: u8 = 19;
const DIRECTORY_NOT_EMPTY: u8 = 20;
const CROSSES_DEVICES: u8 = 21;
const NOT_SEEKABLE: u8 = 22;
const INVALID_STATE: u8 = 23;
const MESSAGE_TOO_LARGE: u8 = 24;
pub const UNSUPPORTED: u8 = 25;
const STORAGE_FULL: u8 = 26;
const READ_ONLY_FILESYSTEM: u8 = 27;
const FILE_TOO_LARGE: u8 = 28;
const QUOTA_EXCEEDED: u8 = 29;

const MESSAGES: [&str; 31] = [
    "NotFound",
    "PermissionDenied",
    "AlreadyExists",
    "InvalidInput",
    "InvalidData",
    "UnexpectedEof",
    "WriteZero",
    "Interrupted",
    "TimedOut",
    "Cancelled",
    "Closed",
    "BrokenPipe",
    "NotConnected",
    "ConnectionReset",
    "ConnectionRefused",
    "ConnectionAborted",
    "AddrInUse",
    "AddrNotAvailable",
    "NotADirectory",
    "IsADirectory",
    "DirectoryNotEmpty",
    "CrossesDevices",
    "NotSeekable",
    "InvalidState",
    "MessageTooLarge",
    "Unsupported",
    "StorageFull",
    "ReadOnlyFilesystem",
    "FileTooLarge",
    "QuotaExceeded",
    "Other",
];

/// 一次 I/O 失败。`source` 是拥有者错误表里的下标。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IoError {
    kind: u8,
    code: i32,
    source: Option<u16>,
}

/// 错误链的拥有者。下标就是 `IoError.source`。
#[derive(Clone, Debug, Default)]
pub struct IoErrors {
    errors: Vec<IoError>,
}

/// 内存游标。读到尾返回 `Ok(0)`，不返回 EOF 错误。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Cursor {
    bytes: Vec<u8>,
    pos: usize,
}

/// 脚本化的短读/短写。每调用一次消费一项。
#[derive(Clone, Debug)]
pub struct Scripted {
    reads: Vec<Result<Vec<u8>, IoError>>,
    writes: Vec<Result<usize, IoError>>,
    writes_done: usize,
}

/// `Seek` 的起点。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SeekFrom {
    Start(u64),
    End(i64),
    Current(i64),
}

/// 阻塞操作的逻辑等待。时钟由调用方给出。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IoWait {
    cursor: u64,
    timeout: Option<u64>,
    cancel: bool,
    closed: bool,
    credit: bool,
    submitted: Option<IoWaitResult>,
}

/// 一次等待的稳定结果。先提交的结果不会被后到事件覆盖。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IoWaitResult {
    Ready(usize),
    Eof,
    TimedOut,
    Cancelled,
    Closed,
    Parked,
}

/// 可重复读取的字节源。
pub trait ReadBytes {
    fn read(&mut self, destination: &mut [u8]) -> Result<usize, IoError>;
}

/// 可重复写入的字节宿。
pub trait WriteBytes {
    fn write(&mut self, source: &[u8]) -> Result<usize, IoError>;
}

impl IoError {
    /// 构造一个无载荷变体。编号必须落在 0..30。
    pub fn bare(kind: u8) -> Self {
        debug_assert!(kind < BARE_KINDS, "无载荷变体编号 < 30");
        Self {
            kind,
            code: i32::from(kind),
            source: None,
        }
    }

    /// 构造 `Other`。`code` 是调用方给出的稳定编号。
    pub fn other(code: i32) -> Self {
        Self {
            kind: OTHER,
            code,
            source: None,
        }
    }

    pub const fn kind_code(&self) -> u8 {
        self.kind
    }

    pub const fn code(&self) -> i32 {
        self.code
    }

    pub const fn source_index(&self) -> Option<u16> {
        self.source
    }

    /// 人读文本。只返回变体名，不拼接 code 或来源。
    pub fn message(self) -> &'static str {
        MESSAGES[usize::from(self.kind)]
    }
}

impl IoErrors {
    pub fn new() -> Self {
        Self::default()
    }

    /// 登记一个错误并返回它在表中的下标。
    pub fn push(&mut self, error: IoError) -> u16 {
        let index = u16::try_from(self.errors.len()).expect("错误链下标适配 u16");
        self.errors.push(error);
        index
    }

    pub fn get(&self, index: u16) -> Option<&IoError> {
        self.errors.get(usize::from(index))
    }

    /// 剥掉全部 `Other.source`，返回无嵌套来源的变体。
    pub fn kind(&self, error: IoError) -> IoError {
        let mut current = error;
        let mut seen = 0;
        while current.kind == OTHER {
            let Some(index) = current.source else {
                return IoError {
                    kind: OTHER,
                    code: current.code,
                    source: None,
                };
            };
            let Some(next) = self.get(index).copied() else {
                return IoError {
                    kind: OTHER,
                    code: current.code,
                    source: None,
                };
            };
            seen += 1;
            debug_assert!(seen <= MAX_CHAIN_DEPTH, "错误链深度有上界");
            current = next;
        }
        IoError::bare(current.kind)
    }

    /// 只有 `Other` 且来源存在时返回直接原因。
    pub fn source(&self, error: IoError) -> Option<IoError> {
        if error.kind != OTHER {
            return None;
        }
        error.source.and_then(|index| self.get(index).copied())
    }

    /// 构造带一层来源的 `Other`。环或超深返回 `InvalidInput`，且该失败不再包来源。
    pub fn with_source(&mut self, error: IoError, cause: IoError) -> IoError {
        let kind = self.kind(error);
        if self.forms_cycle(kind.kind, cause) {
            return IoError::bare(INVALID_INPUT);
        }
        let index = self.push(cause);
        IoError {
            kind: OTHER,
            code: i32::from(kind.kind),
            source: Some(index),
        }
    }

    fn forms_cycle(&self, kind: u8, cause: IoError) -> bool {
        let mut current = Some(cause);
        let mut depth = 0;
        while let Some(error) = current {
            let bare = if error.kind == OTHER {
                self.kind(error).kind
            } else {
                error.kind
            };
            if bare == kind {
                return true;
            }
            depth += 1;
            if depth > MAX_CHAIN_DEPTH {
                return true;
            }
            current = self.source(error);
        }
        false
    }
}

impl Cursor {
    pub fn new(bytes: Vec<u8>) -> Self {
        Self { bytes, pos: 0 }
    }

    pub fn position(&self) -> usize {
        self.pos
    }

    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub fn seek(&mut self, from: SeekFrom) -> Result<u64, IoError> {
        let next = match from {
            SeekFrom::Start(offset) => offset,
            SeekFrom::End(offset) => self.checked_offset(self.bytes.len(), offset)?,
            SeekFrom::Current(offset) => self.checked_offset(self.pos, offset)?,
        };
        self.pos = usize::try_from(next).unwrap_or(usize::MAX);
        Ok(next)
    }

    fn checked_offset(&self, base: usize, offset: i64) -> Result<u64, IoError> {
        let base = i64::try_from(base).unwrap_or(i64::MAX);
        let next = base.checked_add(offset).filter(|value| *value >= 0);
        next.map(|value| u64::try_from(value).unwrap_or(u64::MAX))
            .ok_or_else(|| IoError::bare(INVALID_INPUT))
    }
}

impl ReadBytes for Cursor {
    fn read(&mut self, destination: &mut [u8]) -> Result<usize, IoError> {
        if self.pos >= self.bytes.len() {
            return Ok(0);
        }
        let available = &self.bytes[self.pos..];
        let count = destination.len().min(available.len());
        destination[..count].copy_from_slice(&available[..count]);
        self.pos += count;
        Ok(count)
    }
}

impl WriteBytes for Cursor {
    fn write(&mut self, source: &[u8]) -> Result<usize, IoError> {
        let end = self.pos.saturating_add(source.len());
        if end > self.bytes.len() {
            self.bytes.resize(end, 0);
        }
        self.bytes[self.pos..end].copy_from_slice(source);
        self.pos = end;
        Ok(source.len())
    }
}

impl Scripted {
    pub fn new(reads: Vec<Result<Vec<u8>, IoError>>, writes: Vec<Result<usize, IoError>>) -> Self {
        Self {
            reads,
            writes,
            writes_done: 0,
        }
    }

    pub fn writes_done(&self) -> usize {
        self.writes_done
    }
}

impl ReadBytes for Scripted {
    fn read(&mut self, destination: &mut [u8]) -> Result<usize, IoError> {
        if self.reads.is_empty() {
            return Err(IoError::bare(UNEXPECTED_EOF));
        }
        match self.reads.remove(0)? {
            chunk if chunk.is_empty() => Ok(0),
            chunk => {
                let count = destination.len().min(chunk.len());
                destination[..count].copy_from_slice(&chunk[..count]);
                Ok(count)
            }
        }
    }
}

impl WriteBytes for Scripted {
    fn write(&mut self, source: &[u8]) -> Result<usize, IoError> {
        self.writes_done += 1;
        if self.writes.is_empty() {
            return Err(IoError::bare(UNEXPECTED_EOF));
        }
        let written = self.writes.remove(0)?;
        Ok(written.min(source.len()))
    }
}

/// 填满 destination。中途 `Ok(0)` 变成 `UnexpectedEof`，已写前缀保留。
pub fn read_exact<R: ReadBytes>(reader: &mut R, destination: &mut [u8]) -> Result<(), IoError> {
    let mut filled = 0;
    while filled < destination.len() {
        match reader.read(&mut destination[filled..])? {
            0 => return Err(IoError::bare(UNEXPECTED_EOF)),
            count => filled += count,
        }
    }
    Ok(())
}

/// 写完 source。空输入不调用 `write`；非空写入的 `Ok(0)` 变成 `WriteZero`。
pub fn write_all<W: WriteBytes>(writer: &mut W, source: &[u8]) -> Result<(), IoError> {
    let mut written = 0;
    if source.is_empty() {
        return Ok(());
    }
    while written < source.len() {
        match writer.write(&source[written..])? {
            0 => return Err(IoError::bare(WRITE_ZERO)),
            count => written += count,
        }
    }
    Ok(())
}

/// 用固定 8192 字节缓冲复制。返回成功写出的累计字节。
pub fn copy<R: ReadBytes, W: WriteBytes>(reader: &mut R, writer: &mut W) -> Result<u64, IoError> {
    let mut buffer = [0_u8; IO_CHUNK_BYTES];
    let mut copied = 0_u64;
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            return Ok(copied);
        }
        write_all(writer, &buffer[..read])?;
        copied += u64::try_from(read).unwrap_or(u64::MAX);
    }
}

/// 以 8192 为块读到 EOF，返回封存后的字节。
pub fn read_to_end<R: ReadBytes>(reader: &mut R) -> Result<Vec<u8>, IoError> {
    let mut buffer = CowBytes::new();
    loop {
        let limit = isize::try_from(IO_CHUNK_BYTES).expect("块长");
        match read_buffer(&mut buffer, limit, reader) {
            Ok(0) => return Ok(buffer.freeze().as_bytes().to_vec()),
            Ok(_) => {}
            Err(TextFault::Negative) => unreachable!("固定正块长"),
            Err(_) => return Err(IoError::bare(INVALID_INPUT)),
        }
    }
}

/// 读到 EOF 后做严格 UTF-8 解码。非法序列是 `InvalidData`，不返回部分 string。
pub fn read_to_string<R: ReadBytes>(reader: &mut R) -> Result<String, IoError> {
    let bytes = read_to_end(reader)?;
    utf8_decode(&bytes).map_err(|_| IoError::bare(INVALID_DATA))
}

/// 向缓冲追加至多 `limit` 字节。负 limit 是 panic 级 `TextFault::Negative`。
pub fn read_buffer<R: ReadBytes>(
    buffer: &mut CowBytes,
    limit: isize,
    reader: &mut R,
) -> Result<usize, TextFault> {
    if limit < 0 {
        return Err(TextFault::Negative);
    }
    if limit == 0 {
        return Ok(0);
    }
    let mut chunk = vec![0_u8; usize::try_from(limit).expect("limit 非负")];
    let count = reader.read(&mut chunk).unwrap_or(0);
    buffer.extend_from_slice(&chunk[..count]);
    Ok(count)
}

/// 把缓冲封存后交给一次 `write`。一次调用只写一个前缀。
pub fn write_buffer<W: WriteBytes>(
    writer: &mut W,
    source: &mut CowBytes,
) -> Result<usize, IoError> {
    let frozen = source.freeze();
    writer.write(frozen.as_bytes())
}

/// `limit == 0` 不读。其余把一次 `read_buffer` 的结果封存成字节。
pub fn fill_buffer<R: ReadBytes>(reader: &mut R, limit: isize) -> Result<Vec<u8>, TextFault> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    let mut buffer = CowBytes::new();
    read_buffer(&mut buffer, limit, reader)?;
    Ok(buffer.freeze().as_bytes().to_vec())
}

impl IoWait {
    pub fn new(cursor: u64, timeout: Option<u64>, cancel: bool, closed: bool) -> Self {
        Self {
            cursor,
            timeout,
            cancel,
            closed,
            credit: false,
            submitted: None,
        }
    }

    /// 发放一次额度。已提交的结果不变。
    pub fn grant_credit(&mut self) {
        self.credit = true;
    }

    /// 判定顺序：关闭、取消、timeout、无额度、就绪。
    pub fn poll(&mut self, ready_bytes: usize, eof: bool) -> IoWaitResult {
        if let Some(result) = self.submitted {
            return result;
        }
        let result = if self.closed {
            IoWaitResult::Closed
        } else if self.cancel {
            IoWaitResult::Cancelled
        } else if self.timeout.is_some_and(|deadline| self.cursor >= deadline) {
            IoWaitResult::TimedOut
        } else if !self.credit {
            return IoWaitResult::Parked;
        } else if eof {
            IoWaitResult::Eof
        } else {
            IoWaitResult::Ready(ready_bytes)
        };
        self.submitted = Some(result);
        result
    }
}

/// 无额度时进入现有 waiting 队列；有额度后进入普通 `ForeignBridge`。
pub fn admit_blocking(
    world: &mut ForeignWorld,
    handle: CoroutineHandle,
    credit: bool,
) -> Result<AdmitOutcome, RawInvariant> {
    if !credit {
        return world.admit(handle, BridgeMode::Ordinary, &[], 0, 0, 0);
    }
    let admitted = world.admit(handle, BridgeMode::Ordinary, &[], 1, 0, 8)?;
    if matches!(admitted, AdmitOutcome::Waiting) {
        world.complete(handle, 0)?;
        return world.admit(handle, BridgeMode::Ordinary, &[], 1, 0, 8);
    }
    Ok(admitted)
}

/// 稳定编号。未列入的名字按语义映射，不新增变体。
pub fn kind_of(name: &str) -> Option<u8> {
    MESSAGES
        .iter()
        .position(|item| *item == name)
        .map(|index| u8::try_from(index).expect("变体编号适配 u8"))
}

/// 这些常量保留给参照测试与调用方按名字取变体。
pub fn not_found() -> IoError {
    IoError::bare(NOT_FOUND)
}

pub fn permission_denied() -> IoError {
    IoError::bare(PERMISSION_DENIED)
}

pub fn already_exists() -> IoError {
    IoError::bare(ALREADY_EXISTS)
}

pub fn interrupted() -> IoError {
    IoError::bare(INTERRUPTED)
}

pub fn timed_out() -> IoError {
    IoError::bare(TIMED_OUT)
}

pub fn cancelled() -> IoError {
    IoError::bare(CANCELLED)
}

pub fn closed() -> IoError {
    IoError::bare(CLOSED)
}

pub fn broken_pipe() -> IoError {
    IoError::bare(BROKEN_PIPE)
}

pub fn not_connected() -> IoError {
    IoError::bare(NOT_CONNECTED)
}

pub fn connection_reset() -> IoError {
    IoError::bare(CONNECTION_RESET)
}

pub fn connection_refused() -> IoError {
    IoError::bare(CONNECTION_REFUSED)
}

pub fn connection_aborted() -> IoError {
    IoError::bare(CONNECTION_ABORTED)
}

pub fn addr_in_use() -> IoError {
    IoError::bare(ADDR_IN_USE)
}

pub fn addr_not_available() -> IoError {
    IoError::bare(ADDR_NOT_AVAILABLE)
}

pub fn not_a_directory() -> IoError {
    IoError::bare(NOT_A_DIRECTORY)
}

pub fn is_a_directory() -> IoError {
    IoError::bare(IS_A_DIRECTORY)
}

pub fn directory_not_empty() -> IoError {
    IoError::bare(DIRECTORY_NOT_EMPTY)
}

pub fn crosses_devices() -> IoError {
    IoError::bare(CROSSES_DEVICES)
}

pub fn not_seekable() -> IoError {
    IoError::bare(NOT_SEEKABLE)
}

pub fn invalid_state() -> IoError {
    IoError::bare(INVALID_STATE)
}

pub fn message_too_large() -> IoError {
    IoError::bare(MESSAGE_TOO_LARGE)
}

pub fn storage_full() -> IoError {
    IoError::bare(STORAGE_FULL)
}

pub fn read_only_filesystem() -> IoError {
    IoError::bare(READ_ONLY_FILESYSTEM)
}

pub fn file_too_large() -> IoError {
    IoError::bare(FILE_TOO_LARGE)
}

pub fn quota_exceeded() -> IoError {
    IoError::bare(QUOTA_EXCEEDED)
}
