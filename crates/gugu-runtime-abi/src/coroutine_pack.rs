//! 子协程参数包的机器布局常量：入口片段、LIR 发包点与调度器复制点的唯一来源。

/// 子协程参数包（spawn pack）的机器布局契约。
///
/// 一个包描述被 `spawn` 的协程入口所需的全部内部 ABI 车道：9 个整数车道（`Rax` 起各 8 字节）
/// 与 8 个 XMM 车道（各 16 字节），打包成一段 16 字节对齐的原始内存。入口片段、LIR 发包点与
/// 调度器复制点都必须从这一份常量派生偏移，避免三处各写一套字面量。
pub const COROUTINE_PACK_INTEGER_LANES: usize = 9;
pub const COROUTINE_PACK_INTEGER_STRIDE: usize = 8;
pub const COROUTINE_PACK_FLOAT_OFFSET: usize = 72;
pub const COROUTINE_PACK_FLOAT_LANES: usize = 8;
pub const COROUTINE_PACK_FLOAT_STRIDE: usize = 16;
/// 发包与复制使用的字节数：车道区 200 字节 + 8 字节保留（16 对齐）。
pub const COROUTINE_PACK_BYTES: usize = 208;
/// 参数包要求的对齐：XMM 车道按 16 字节装载。
pub const COROUTINE_PACK_ALIGN: usize = 16;

/// 整数车道在包内的相对偏移。
pub const fn coroutine_pack_integer_offset(lane: usize) -> usize {
    lane * COROUTINE_PACK_INTEGER_STRIDE
}

/// 浮点车道在包内的相对偏移。
pub const fn coroutine_pack_float_offset(lane: usize) -> usize {
    COROUTINE_PACK_FLOAT_OFFSET + lane * COROUTINE_PACK_FLOAT_STRIDE
}

/// 参数包内的车道总数。
pub const COROUTINE_PACK_LANES: usize = COROUTINE_PACK_INTEGER_LANES + COROUTINE_PACK_FLOAT_LANES;

/// 参数包 lane map：每个车道 3 位，`0` 表示不是根，`n + 1` 表示根种类判别值 `n`
/// （与 `Provenance::root_kind` 同一套判别值：0=Direct、1=Interior、2=Handle、3=Compressed、
/// 4=StackInterior）。17 个车道占 51 位，正好落在一个机器字里。
pub const COROUTINE_PACK_TRACE_BITS: usize = 3;

/// 参数包里第 `lane` 个车道的 lane map 位偏移。
pub const fn coroutine_pack_trace_shift(lane: usize) -> usize {
    lane * COROUTINE_PACK_TRACE_BITS
}

/// 车道不是根时的 lane map 编码。
pub const COROUTINE_PACK_TRACE_NONE: u64 = 0;

/// 车道是根时的 lane map 编码：根种类判别值加一。
pub const fn coroutine_pack_trace_code(root_kind: u32) -> u64 {
    // 常量上下文里的无损加宽：`From` 不能在 const fn 里用（const trait 未稳定）。
    (root_kind as u64) + 1
}

const _: () = {
    // lane map 必须落在一个机器字内：收集器按这一个字取全部车道的根种类。
    assert!(COROUTINE_PACK_LANES * COROUTINE_PACK_TRACE_BITS <= 64);
    // 车道数由整数段与浮点段拼成，两段的偏移必须与车道编号一致。
    assert!(
        coroutine_pack_integer_offset(COROUTINE_PACK_INTEGER_LANES - 1) + 8
            <= COROUTINE_PACK_FLOAT_OFFSET
    );
    assert!(
        coroutine_pack_float_offset(COROUTINE_PACK_FLOAT_LANES - 1) + COROUTINE_PACK_FLOAT_STRIDE
            <= COROUTINE_PACK_BYTES
    );
};
