use std::fmt;

/// bootstrap action graph 中的阶段。
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum ActionKind {
    /// 解析目标描述。
    ResolveTarget,
    /// 读取用户源输入。
    LoadSources,
    /// 执行阶段 1 前端检查。
    Frontend,
    /// 构造 bootstrap IR。
    BuildIr,
    /// 规划目标后端输入。
    PlanBackend,
    /// 附加 Gugu runtime 源资源。
    AttachRuntime,
    /// 校验内存中的镜像计划。
    ValidateImage,
    /// 最终镜像写出。
    EmitImage,
}

impl fmt::Display for ActionKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::ResolveTarget => "resolve-target",
            Self::LoadSources => "load-sources",
            Self::Frontend => "frontend",
            Self::BuildIr => "build-ir",
            Self::PlanBackend => "plan-backend",
            Self::AttachRuntime => "attach-runtime",
            Self::ValidateImage => "validate-image",
            Self::EmitImage => "emit-image",
        };
        formatter.write_str(name)
    }
}

/// 一个 action 的最终状态。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ActionStatus {
    /// 尚未执行。
    Pending,
    /// 已成功完成。
    Complete,
    /// 因前置条件或阶段能力而跳过。
    Skipped,
    /// 执行失败。
    Failed,
}

impl fmt::Display for ActionStatus {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::Pending => "pending",
            Self::Complete => "complete",
            Self::Skipped => "skipped",
            Self::Failed => "failed",
        };
        formatter.write_str(name)
    }
}

/// action graph 中的一个节点。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActionNode {
    id: u32,
    kind: ActionKind,
    status: ActionStatus,
    detail: String,
}

impl ActionNode {
    /// 返回节点在 graph 中的稠密编号。
    pub fn id(&self) -> u32 {
        self.id
    }

    /// 返回节点类型。
    pub fn kind(&self) -> ActionKind {
        self.kind
    }

    /// 返回节点状态。
    pub fn status(&self) -> ActionStatus {
        self.status
    }

    /// 返回阶段结果摘要。
    pub fn detail(&self) -> &str {
        &self.detail
    }
}

/// 阶段 1 的确定性编译 action graph。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActionGraph {
    nodes: Vec<ActionNode>,
}

impl ActionGraph {
    pub(crate) fn new() -> Self {
        let kinds = [
            ActionKind::ResolveTarget,
            ActionKind::LoadSources,
            ActionKind::Frontend,
            ActionKind::BuildIr,
            ActionKind::PlanBackend,
            ActionKind::AttachRuntime,
            ActionKind::ValidateImage,
            ActionKind::EmitImage,
        ];
        let nodes = kinds
            .into_iter()
            .enumerate()
            .map(|(id, kind)| ActionNode {
                id: id as u32,
                kind,
                status: ActionStatus::Pending,
                detail: String::new(),
            })
            .collect();
        Self { nodes }
    }

    pub(crate) fn complete(&mut self, kind: ActionKind, detail: impl Into<String>) {
        self.set(kind, ActionStatus::Complete, detail);
    }

    pub(crate) fn fail(&mut self, kind: ActionKind, detail: impl Into<String>) {
        self.set(kind, ActionStatus::Failed, detail);
    }

    pub(crate) fn skip_after(&mut self, kind: ActionKind, detail: impl Into<String>) {
        let Some(index) = self.nodes.iter().position(|node| node.kind == kind) else {
            return;
        };
        let detail = detail.into();
        for node in &mut self.nodes[index + 1..] {
            node.status = ActionStatus::Skipped;
            node.detail = detail.clone();
        }
    }

    fn set(&mut self, kind: ActionKind, status: ActionStatus, detail: impl Into<String>) {
        let node = self
            .nodes
            .iter_mut()
            .find(|node| node.kind == kind)
            .expect("bootstrap action kind must be registered");
        debug_assert_eq!(node.status, ActionStatus::Pending);
        node.status = status;
        node.detail = detail.into();
    }

    /// 返回按执行顺序排列的 action 节点。
    pub fn nodes(&self) -> &[ActionNode] {
        &self.nodes
    }
}
