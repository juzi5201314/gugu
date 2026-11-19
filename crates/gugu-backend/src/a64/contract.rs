//! 表目录与编码规则的域隔离身份；修改编码规则必须提高 revision 并更新测试钉值。

use super::{Error, error, table::*};
use crate::hash::hash_domain;

pub const ENCODER_SCHEMA: u32 = 1;
pub const ENCODER_REVISION: u32 = 3;

pub fn catalog() -> &'static [Form] {
    FORMS
}

pub fn verify() -> Result<(), Error> {
    for (index, form) in FORMS.iter().enumerate() {
        if form.fixed & !form.mask != 0 {
            return Err(error("固定值落入可变字段"));
        }
        let mut owned = form.mask;
        for slot in form.operands {
            let mut value_mask = 0u32;
            for field in slot.fields {
                if field.width == 0 {
                    continue;
                }
                if field.width >= 32
                    || field.lsb + field.width > 32
                    || field.value_lsb + field.width > 32
                {
                    return Err(error("字段越界"));
                }
                let value = ((1u32 << field.width) - 1) << field.value_lsb;
                if owned & field.mask() != 0 || value_mask & value != 0 {
                    return Err(error("字段重叠"));
                }
                owned |= field.mask();
                value_mask |= value;
            }
        }
        if owned != u32::MAX {
            return Err(error("指令位未被描述"));
        }
        for other in &FORMS[..index] {
            if form.key == other.key || ((form.fixed ^ other.fixed) & form.mask & other.mask) == 0 {
                return Err(Error(format!(
                    "A64 form 歧义：{} / {}",
                    other.key, form.key
                )));
            }
        }
    }
    Ok(())
}

pub fn catalog_fingerprint() -> [u8; 32] {
    let mut bytes = Vec::new();
    for (id, form) in FORMS.iter().enumerate() {
        bytes.extend_from_slice(&(id as u16).to_le_bytes());
        for text in [form.key, form.mnemonic] {
            bytes.extend_from_slice(&(text.len() as u32).to_le_bytes());
            bytes.extend_from_slice(text.as_bytes());
        }
        bytes.extend_from_slice(&form.fixed.to_le_bytes());
        bytes.extend_from_slice(&form.mask.to_le_bytes());
        let shape = format!("{:?}:{:?}", form.feature, form.operands);
        bytes.extend_from_slice(&(shape.len() as u32).to_le_bytes());
        bytes.extend_from_slice(shape.as_bytes());
    }
    hash_domain("gugu-a64-encoder-catalog-v1", &bytes)
}

pub fn fingerprint() -> [u8; 32] {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&ENCODER_SCHEMA.to_le_bytes());
    bytes.extend_from_slice(&ENCODER_REVISION.to_le_bytes());
    bytes.extend_from_slice(&catalog_fingerprint());
    hash_domain("gugu-a64-encoder-v1", &bytes)
}
