use super::super::ast::ExprId;
use super::model::Ty;

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum Projection {
    Field(usize),
    Index(Option<i128>),
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct BorrowCheck {
    pub expression: ExprId,
    pub base: Ty,
    pub projection: Vec<Projection>,
    pub target: Ty,
}
