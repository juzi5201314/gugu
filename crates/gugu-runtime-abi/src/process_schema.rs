//! 进程 raw 协议：只有布局与操作目录，不包含运行时状态机执行体。
use crate::coroutine_schema::{CoroutineFieldLayout, CoroutineRecordLayout};
use serde::{Deserialize, Serialize};
use std::mem::{align_of, offset_of, size_of};

pub const SCHEMA: u32 = 1;
pub const WORKER_OPS: [&str; 4] = ["spawn", "create_pipe", "open_null", "duplicate_stdio"];
pub const CALL_OPS: [&str; 7] = [
    "try_wait",
    "kill",
    "close_handle",
    "pipe_read",
    "pipe_write",
    "pipe_completion",
    "pipe_cancel",
];
pub const STARTUP_OPS: [&str; 2] = ["getcwd", "system_directory"];
pub const LINUX_ENVIRONMENT_OPS: [&str; 4] = ["environ", "setenv", "unsetenv", "getenv"];
pub const WINDOWS_ENVIRONMENT_OPS: [&str; 4] = [
    "GetEnvironmentStringsW",
    "FreeEnvironmentStringsW",
    "SetEnvironmentVariableW",
    "CompareStringOrdinal",
];

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProcessContract {
    pub schema: u32,
    pub worker: Vec<String>,
    pub call: Vec<String>,
    pub startup: Vec<String>,
    pub linux_environment: Vec<String>,
    pub windows_environment: Vec<String>,
}
impl ProcessContract {
    pub fn fixed() -> Self {
        Self {
            schema: SCHEMA,
            worker: WORKER_OPS.map(str::to_owned).into(),
            call: CALL_OPS.map(str::to_owned).into(),
            startup: STARTUP_OPS.map(str::to_owned).into(),
            linux_environment: LINUX_ENVIRONMENT_OPS.map(str::to_owned).into(),
            windows_environment: WINDOWS_ENVIRONMENT_OPS.map(str::to_owned).into(),
        }
    }
}

#[repr(C, align(8))]
pub struct ProcessRequest {
    pub operation: u64,
    pub arguments: [u64; 8],
    pub value: u64,
    pub error: u64,
}
#[repr(C, align(8))]
pub struct SpawnPacket {
    pub executable: u64,
    pub argv: u64,
    pub environment: u64,
    pub cwd: u64,
    pub stdin_handle: u64,
    pub stdout_handle: u64,
    pub stderr_handle: u64,
    pub process_handle: u64,
    pub process_id: u64,
    pub error: u64,
}
#[repr(C, align(8))]
pub struct ProcessChildControl {
    pub resource: [u64; 25],
    pub lifecycle: u64,
    pub cached_status_kind: u64,
    pub cached_status_value: u64,
    pub observer: u64,
    pub stdin_control: u64,
    pub stdout_control: u64,
    pub stderr_control: u64,
    pub wait_head: u64,
    pub wait_tail: u64,
    pub service_next: u64,
    pub service_prev: u64,
}
#[repr(C, align(8))]
pub struct ProcessPipeControl {
    pub resource: [u64; 25],
    pub direction: u64,
    pub queue_head: u64,
    pub queue_tail: u64,
    pub active_request: u64,
    pub service_next: u64,
    pub service_prev: u64,
}

pub const REQUEST_VALUE: i32 = 72;
pub const REQUEST_ERROR: i32 = 80;
pub const REQUEST_ARGUMENTS: i32 = 8;

pub fn layouts() -> Vec<CoroutineRecordLayout> {
    macro_rules! record {
        ($ty:ty; $($field:ident),+ $(,)?) => {
            CoroutineRecordLayout {
                name: stringify!($ty).into(),
                bytes: u32::try_from(size_of::<$ty>()).expect("进程 record 大小适配 u32"),
                alignment: u32::try_from(align_of::<$ty>()).expect("进程 record 对齐适配 u32"),
                fields: vec![$(CoroutineFieldLayout {
                    name: stringify!($field).into(),
                    offset: u32::try_from(offset_of!($ty, $field)).expect("进程字段偏移适配 u32"),
                }),+],
            }
        };
    }
    vec![
        record!(ProcessRequest; operation, arguments, value, error),
        record!(SpawnPacket; executable, argv, environment, cwd, stdin_handle, stdout_handle, stderr_handle, process_handle, process_id, error),
        record!(ProcessChildControl; resource, lifecycle, cached_status_kind, cached_status_value, observer, stdin_control, stdout_control, stderr_control, wait_head, wait_tail, service_next, service_prev),
        record!(ProcessPipeControl; resource, direction, queue_head, queue_tail, active_request, service_next, service_prev),
    ]
}
const _: () = {
    assert!(size_of::<ProcessRequest>() == 88 && align_of::<ProcessRequest>() == 8);
    assert!(offset_of!(ProcessRequest, value) == 72);
    assert!(offset_of!(ProcessRequest, error) == 80);
    assert!(offset_of!(ProcessRequest, arguments) == 8);
    assert!(size_of::<SpawnPacket>() == 80 && align_of::<SpawnPacket>() == 8);
    assert!(offset_of!(ProcessChildControl, lifecycle) == 200);
    assert!(offset_of!(ProcessPipeControl, direction) == 200);
};
