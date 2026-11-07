//! 外部线程回调的 raw 交接协议；节点由等待线程持有，detach 前不得复用。

pub const CONTROL_BYTES: u32 = 80;
/// native 栈持有节点；登记、嵌套与活动链共享同一个固定上界。
pub const MAX_REGISTERED: i32 = 64;
pub const LOCK: i32 = 0;
pub const ACCEPTING: i32 = 8;
pub const HEAD: i32 = 16;
pub const TAIL: i32 = 24;
pub const REGISTERED: i32 = 32;
pub const PROCESSOR: i32 = 40;
pub const ACTIVE: i32 = 48;
pub const EXTERNAL_THREADS: i32 = 56;
pub const REGISTRY: i32 = 64;
pub const MANAGED_THREAD: i32 = 72;
pub const THREAD_ID: i32 = 248;
pub const REGISTRY_NEXT: i32 = 384;
pub const EXTERNAL_LEASE: i32 = 392;
pub const BORROW_CREDIT: i32 = 400;
pub const POLL_BIT: u32 = 4;
pub const NEXT: i32 = 0;
pub const STATE: i32 = 8;
pub const ENTRY: i32 = 16;
pub const OWNER: i32 = 24;
pub const PACK: i32 = 32;
pub const REQUEST: i32 = 256;
pub const REQUEST_READY: i32 = 4;
pub const REQUEST_DONE: i32 = 5;

pub const IDLE: i32 = 0;
pub const QUEUED: i32 = 1;
pub const RUNNING: i32 = 2;
pub const COMPLETE: i32 = 3;

/// 队列操作均为有限指令序列；锁竞争返回 BUSY，调用方在锁外等待。
pub const OPEN: i32 = 0;
pub const SUBMIT: i32 = 1;
pub const TAKE: i32 = 2;
pub const FINISH: i32 = 3;
pub const DETACH: i32 = 4;
pub const CLOSE: i32 = 5;

/// TAKE 成功返回对齐的节点地址，其余成功返回零；状态码不与有效节点重叠。
pub const OK: i32 = 0;
pub const CLOSED: i32 = 1;
pub const INVALID: i32 = 2;
pub const BUSY: i32 = 3;
