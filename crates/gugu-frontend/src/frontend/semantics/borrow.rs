use super::super::ast::ExprId;
use super::model::Ty;

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) enum Projection {
    Field(usize),
    Index(Option<i128>),
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) struct BorrowCheck {
    pub(crate) expression: ExprId,
    pub(crate) base: Ty,
    pub(crate) projection: Vec<Projection>,
    pub(crate) target: Ty,
}
