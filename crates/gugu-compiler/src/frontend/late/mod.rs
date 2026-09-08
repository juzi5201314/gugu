//! LateConst 只消费 HIR、闭合实例与冻结类型记录，不持有 Model 或 QueryEngine。
mod eval;
mod graph;
mod query;
#[cfg(test)]
mod tests;
pub(crate) mod universe;

use crate::Diagnostic;
use crate::frontend::hir;
use crate::frontend::mono::{
    MonoWorldV1,
    keys::{StableTypeKey, hash_domain},
};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) enum Value {
    Unit,
    Bool(bool),
    Char(char),
    Integer(u128),
    Float(u64),
    Type(StableTypeKey),
    String(String),
    Aggregate(Vec<Value>),
    Range(i128, i128),
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub(crate) struct LateKey {
    pub instance: [u8; 32],
    pub expression: u32,
    pub ty: StableTypeKey,
    pub closure: [u8; 32],
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct LateResult {
    pub key: LateKey,
    pub value: Value,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct LateTable {
    pub universe: [u8; 32],
    pub results: Vec<LateResult>,
    pub fingerprint: [u8; 32],
}

impl LateTable {
    pub(crate) fn fingerprint(&self) -> [u8; 32] {
        hash_domain(
            "gugu-late-table-v1",
            &serde_json::to_vec(&(&self.universe, &self.results)).expect("late 表可序列化"),
        )
    }

    pub(crate) fn verify(&self, world: &MonoWorldV1) -> Result<(), Diagnostic> {
        if self.universe != world.universe.fingerprint
            || self.fingerprint != self.fingerprint()
            || !self.results.windows(2).all(|p| p[0].key < p[1].key)
        {
            return Err(universe::invalid(
                "late 结果表的 universe、顺序或指纹不合法",
            ));
        }
        for result in &self.results {
            if !world
                .instances
                .iter()
                .any(|i| crate::frontend::mono::digest_of(&i.mono_key) == result.key.instance)
            {
                return Err(universe::invalid("late 结果引用未闭合实例"));
            }
            validate_value(&result.value, &result.key.ty, &world.universe)?;
        }
        Ok(())
    }
}

fn validate_value(
    value: &Value,
    key: &StableTypeKey,
    universe: &universe::TypeUniverse,
) -> Result<(), Diagnostic> {
    use universe::Shape;
    let valid = match (&universe.record(key)?.shape, value) {
        (Shape::Unit, Value::Unit)
        | (Shape::Bool, Value::Bool(_))
        | (Shape::Char, Value::Char(_))
        | (Shape::Float(_), Value::Float(_)) => true,
        (Shape::Int { bits, .. }, Value::Integer(value)) => *bits == 128 || *value < 1u128 << bits,
        (Shape::TypeId, Value::Type(key)) => universe.type_id(key).is_some(),
        (Shape::Tuple(types), Value::Aggregate(values)) if types.len() == values.len() => {
            for (value, ty) in values.iter().zip(types) {
                validate_value(value, ty, universe)?;
            }
            true
        }
        (Shape::Struct(fields), Value::Aggregate(values)) if fields.len() == values.len() => {
            for (value, (_, ty)) in values.iter().zip(fields) {
                validate_value(value, ty, universe)?;
            }
            true
        }
        (Shape::Array(ty, n), Value::Aggregate(values)) if *n == values.len() as u64 => {
            for value in values {
                validate_value(value, ty, universe)?;
            }
            true
        }
        _ => false,
    };
    if valid {
        Ok(())
    } else {
        Err(universe::invalid("late 结果只能发布形状已固定的标量叶值"))
    }
}

pub(crate) fn run(
    module: &hir::Module,
    world: &mut MonoWorldV1,
    queries: &crate::QueryEngine,
    sources: &crate::SourceMap,
) -> Result<(), Vec<Diagnostic>> {
    query::run(module, world, queries, sources)
}
