//! 冻结 HIR、具体 GIR 与 runtime 契约之间的交叉视图：契约源记录视图、runtime 根需求与
//! 控制块布局校验。
//!
//! runtime 层只持有契约形状与自校验；凡是需要读取 HIR/GIR 的对照都落在这里。

pub(crate) mod barrier_layout;
pub(crate) mod channel_layout;
pub(crate) mod contract_view;
pub(crate) mod coroutine_layout;
pub(crate) mod demand;
pub(crate) mod local_heap_layout;
pub(crate) mod mark_layout;
pub(crate) mod shared_heap_layout;
pub(crate) mod sync_layout;
