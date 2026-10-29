//! 协程契约携带的登记机器片段数据：字节、续点与修补点。
//!
//! 本模块只持有数据形状；字节由 backend 编码器生成（`backend::x64::fragments`），经
//! `RuntimeRawContractV1::with_coroutine_code` 注入契约，并由 backend 逐字节比对后才被消费。
//! `Default` 表示尚未注入：runtime 层构造契约时不接触编码器。

/// 片段内的一个 4 字节 PC 相对修补点（字段偏移相对片段起点）。
///
/// 目标以 glue 名表达：镜像 writer 按绑定表解析为镜像内符号（函数实例、数据保留区或片段
/// 入口），bench 与调试器按同一份字段自行打补丁。`addend` 用于 RIP 相对数据引用的字段内位移。
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct FragmentPatch {
    /// `rel32`/`disp32` 字段在片段字节内的偏移。
    pub field: u32,
    /// 目标 glue 名（`mangle_runtime` 的输入）。
    pub glue: String,
    /// 字段内位移。
    pub addend: i32,
}

/// `morestack_or_poll` 片段：backend `morestack_entry_code` 的产物。
///
/// 入口 `rsp` 指向 return PC，`r14 = CoroutineSlot*`，`r15 = LogicalProcessor*`，`r11` 是候选
/// 低水位；除 `r11`/`rsp` 外不写入口寄存器，也不读写候选以下的用户栈。`patches` 指向
/// `morestack_mailbox` 数据区与 `context_switch` 片段。
#[derive(Clone, Debug, Default, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct MorestackEntryCode {
    /// 内部边界与字节的版本。
    pub revision: u32,
    /// 经 `context_restore` 回到协程后的续点偏移。
    pub resume_offset: u32,
    /// 指令字节。
    pub bytes: Vec<u8>,
    /// 修补点，按字段偏移升序。
    pub patches: Vec<FragmentPatch>,
}

/// main 返回哨兵：main 的 `ret` 落到这里，把退出值记进 mailbox，再单向恢复系统上下文。
///
/// 入口 `rax` 是 main 的返回值（`main` 返回 `()` 时无意义，按退出码 0 处理）；`rsp` 指向
/// `rt0_start` 预植的返回槽。片段不保存任何可恢复 PC：恢复系统上下文后不再回到这里。
#[derive(Clone, Debug, Default, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct MainReturnCode {
    /// 内部边界的版本。
    pub revision: u32,
    /// 指令字节。
    pub bytes: Vec<u8>,
    /// 修补点，按字段偏移升序。
    pub patches: Vec<FragmentPatch>,
}
