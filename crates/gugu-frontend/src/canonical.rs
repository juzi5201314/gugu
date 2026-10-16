//! 编译器内部的确定性二进制编码基础。
//!
//! 所有整数使用小端固定宽度；变长字段使用 `u64` 长度前缀；枚举使用 `u16`
//! 判别值。解码器统一检查截断、长度溢出和尾随字节，适合 query 身份与 IR 指纹。

use serde::de::{self, DeserializeOwned, EnumAccess, MapAccess, SeqAccess, VariantAccess, Visitor};
use serde::ser::{
    self, SerializeMap, SerializeSeq, SerializeStruct, SerializeStructVariant, SerializeTuple,
    SerializeTupleStruct, SerializeTupleVariant,
};
use serde::{Deserializer, Serialize, Serializer};
use std::fmt;

/// 二进制编码 revision，进入每个 query key，拒绝旧 JSON 缓存。
pub(crate) const REVISION: u32 = 1;

/// 编解码错误。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Error(String);

impl Error {
    fn message(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}
impl std::error::Error for Error {}
impl ser::Error for Error {
    fn custom<T: fmt::Display>(message: T) -> Self {
        Self::message(message.to_string())
    }
}
impl de::Error for Error {
    fn custom<T: fmt::Display>(message: T) -> Self {
        Self::message(message.to_string())
    }
}

/// 将可序列化对象编码为稳定二进制。
pub(crate) fn to_vec<T: Serialize + ?Sized>(value: &T) -> Result<Vec<u8>, Error> {
    let mut output = Vec::new();
    value.serialize(Encoder {
        output: &mut output,
    })?;
    Ok(output)
}

/// 解码一个完整稳定二进制对象；尾随字节会被拒绝。
pub(crate) fn from_slice<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, Error> {
    let mut decoder = Decoder { bytes, offset: 0 };
    let value = T::deserialize(&mut decoder)?;
    if decoder.offset != bytes.len() {
        return Err(Error::message("规范编码含尾随字节"));
    }
    Ok(value)
}

struct Encoder<'a> {
    output: &'a mut Vec<u8>,
}

impl Encoder<'_> {
    fn length(&mut self, length: usize) -> Result<(), Error> {
        self.output.extend_from_slice(
            &u64::try_from(length)
                .map_err(|_| Error::message("规范编码长度超过 u64"))?
                .to_le_bytes(),
        );
        Ok(())
    }
    fn variant(&mut self, index: u32) {
        let index = u16::try_from(index).expect("IR schema 的枚举变体数量小于 65536");
        self.output.extend_from_slice(&index.to_le_bytes());
    }
    fn child<T: ?Sized + Serialize>(&mut self, value: &T) -> Result<(), Error> {
        value.serialize(Encoder {
            output: self.output,
        })
    }
}

impl<'a> Serializer for Encoder<'a> {
    type Ok = ();
    type Error = Error;
    type SerializeSeq = Sequence<'a>;
    type SerializeTuple = Sequence<'a>;
    type SerializeTupleStruct = Sequence<'a>;
    type SerializeTupleVariant = Sequence<'a>;
    type SerializeMap = MapEncoder<'a>;
    type SerializeStruct = Sequence<'a>;
    type SerializeStructVariant = Sequence<'a>;

    fn serialize_bool(self, value: bool) -> Result<(), Self::Error> {
        self.output.push(u8::from(value));
        Ok(())
    }
    fn serialize_i8(self, value: i8) -> Result<(), Self::Error> {
        self.output.extend_from_slice(&value.to_le_bytes());
        Ok(())
    }
    fn serialize_i16(self, value: i16) -> Result<(), Self::Error> {
        self.output.extend_from_slice(&value.to_le_bytes());
        Ok(())
    }
    fn serialize_i32(self, value: i32) -> Result<(), Self::Error> {
        self.output.extend_from_slice(&value.to_le_bytes());
        Ok(())
    }
    fn serialize_i64(self, value: i64) -> Result<(), Self::Error> {
        self.output.extend_from_slice(&value.to_le_bytes());
        Ok(())
    }
    fn serialize_i128(self, value: i128) -> Result<(), Self::Error> {
        self.output.extend_from_slice(&value.to_le_bytes());
        Ok(())
    }
    fn serialize_u8(self, value: u8) -> Result<(), Self::Error> {
        self.output.push(value);
        Ok(())
    }
    fn serialize_u16(self, value: u16) -> Result<(), Self::Error> {
        self.output.extend_from_slice(&value.to_le_bytes());
        Ok(())
    }
    fn serialize_u32(self, value: u32) -> Result<(), Self::Error> {
        self.output.extend_from_slice(&value.to_le_bytes());
        Ok(())
    }
    fn serialize_u64(self, value: u64) -> Result<(), Self::Error> {
        self.output.extend_from_slice(&value.to_le_bytes());
        Ok(())
    }
    fn serialize_u128(self, value: u128) -> Result<(), Self::Error> {
        self.output.extend_from_slice(&value.to_le_bytes());
        Ok(())
    }
    fn serialize_f32(self, value: f32) -> Result<(), Self::Error> {
        self.output.extend_from_slice(&value.to_le_bytes());
        Ok(())
    }
    fn serialize_f64(self, value: f64) -> Result<(), Self::Error> {
        self.output.extend_from_slice(&value.to_le_bytes());
        Ok(())
    }
    fn serialize_char(self, value: char) -> Result<(), Self::Error> {
        self.output
            .extend_from_slice(&u32::from(value).to_le_bytes());
        Ok(())
    }
    fn serialize_str(mut self, value: &str) -> Result<(), Self::Error> {
        self.length(value.len())?;
        self.output.extend_from_slice(value.as_bytes());
        Ok(())
    }
    fn serialize_bytes(mut self, value: &[u8]) -> Result<(), Self::Error> {
        self.length(value.len())?;
        self.output.extend_from_slice(value);
        Ok(())
    }
    fn serialize_none(self) -> Result<(), Self::Error> {
        self.output.push(0);
        Ok(())
    }
    fn serialize_some<T: ?Sized + Serialize>(mut self, value: &T) -> Result<(), Self::Error> {
        self.output.push(1);
        self.child(value)
    }
    fn serialize_unit(self) -> Result<(), Self::Error> {
        Ok(())
    }
    fn serialize_unit_struct(self, _name: &'static str) -> Result<(), Self::Error> {
        Ok(())
    }
    fn serialize_unit_variant(
        mut self,
        _name: &'static str,
        index: u32,
        _variant: &'static str,
    ) -> Result<(), Self::Error> {
        self.variant(index);
        Ok(())
    }
    fn serialize_newtype_struct<T: ?Sized + Serialize>(
        mut self,
        _name: &'static str,
        value: &T,
    ) -> Result<(), Self::Error> {
        self.child(value)
    }
    fn serialize_newtype_variant<T: ?Sized + Serialize>(
        mut self,
        _name: &'static str,
        index: u32,
        _variant: &'static str,
        value: &T,
    ) -> Result<(), Self::Error> {
        self.variant(index);
        self.child(value)
    }
    fn serialize_seq(self, _len: Option<usize>) -> Result<Self::SerializeSeq, Self::Error> {
        Sequence::new(self.output, None, true)
    }
    fn serialize_tuple(self, len: usize) -> Result<Self::SerializeTuple, Self::Error> {
        Sequence::new(self.output, Some(len), true)
    }
    fn serialize_tuple_struct(
        self,
        _name: &'static str,
        len: usize,
    ) -> Result<Self::SerializeTupleStruct, Self::Error> {
        Sequence::new(self.output, Some(len), true)
    }
    fn serialize_tuple_variant(
        mut self,
        _name: &'static str,
        index: u32,
        _variant: &'static str,
        len: usize,
    ) -> Result<Self::SerializeTupleVariant, Self::Error> {
        self.variant(index);
        Sequence::new(self.output, Some(len), true)
    }
    fn serialize_map(self, len: Option<usize>) -> Result<Self::SerializeMap, Self::Error> {
        Ok(MapEncoder {
            output: self.output,
            entries: Vec::with_capacity(len.unwrap_or(0)),
            key: None,
        })
    }
    fn serialize_struct(
        self,
        _name: &'static str,
        len: usize,
    ) -> Result<Self::SerializeStruct, Self::Error> {
        Sequence::new(self.output, Some(len), false)
    }
    fn serialize_struct_variant(
        mut self,
        _name: &'static str,
        index: u32,
        _variant: &'static str,
        len: usize,
    ) -> Result<Self::SerializeStructVariant, Self::Error> {
        self.variant(index);
        Sequence::new(self.output, Some(len), false)
    }
    fn is_human_readable(&self) -> bool {
        false
    }
}

struct Sequence<'a> {
    output: &'a mut Vec<u8>,
    expected: Option<usize>,
    written: usize,
    prefix: Option<usize>,
}
impl<'a> Sequence<'a> {
    fn new(
        output: &'a mut Vec<u8>,
        expected: Option<usize>,
        with_prefix: bool,
    ) -> Result<Self, Error> {
        let prefix = with_prefix.then(|| {
            let position = output.len();
            output.extend_from_slice(&[0; 8]);
            position
        });
        Ok(Self {
            output,
            expected,
            written: 0,
            prefix,
        })
    }
    fn element<T: ?Sized + Serialize>(&mut self, value: &T) -> Result<(), Error> {
        value.serialize(Encoder {
            output: self.output,
        })?;
        self.written += 1;
        Ok(())
    }
    fn finish(self) -> Result<(), Error> {
        if self
            .expected
            .is_some_and(|expected| expected != self.written)
        {
            return Err(Error::message("规范编码序列长度不符"));
        }
        if let Some(position) = self.prefix {
            let length = u64::try_from(self.written)
                .map_err(|_| Error::message("规范编码序列长度超过 u64"))?;
            self.output[position..position + 8].copy_from_slice(&length.to_le_bytes());
        }
        Ok(())
    }
}
impl SerializeSeq for Sequence<'_> {
    type Ok = ();
    type Error = Error;
    fn serialize_element<T: ?Sized + Serialize>(&mut self, value: &T) -> Result<(), Error> {
        self.element(value)
    }
    fn end(self) -> Result<(), Error> {
        self.finish()
    }
}
impl SerializeTuple for Sequence<'_> {
    type Ok = ();
    type Error = Error;
    fn serialize_element<T: ?Sized + Serialize>(&mut self, value: &T) -> Result<(), Error> {
        self.element(value)
    }
    fn end(self) -> Result<(), Error> {
        self.finish()
    }
}
impl SerializeTupleStruct for Sequence<'_> {
    type Ok = ();
    type Error = Error;
    fn serialize_field<T: ?Sized + Serialize>(&mut self, value: &T) -> Result<(), Error> {
        self.element(value)
    }
    fn end(self) -> Result<(), Error> {
        self.finish()
    }
}
impl SerializeTupleVariant for Sequence<'_> {
    type Ok = ();
    type Error = Error;
    fn serialize_field<T: ?Sized + Serialize>(&mut self, value: &T) -> Result<(), Error> {
        self.element(value)
    }
    fn end(self) -> Result<(), Error> {
        self.finish()
    }
}
impl SerializeStruct for Sequence<'_> {
    type Ok = ();
    type Error = Error;
    fn serialize_field<T: ?Sized + Serialize>(
        &mut self,
        _key: &'static str,
        value: &T,
    ) -> Result<(), Error> {
        self.element(value)
    }
    fn end(self) -> Result<(), Error> {
        self.finish()
    }
}
impl SerializeStructVariant for Sequence<'_> {
    type Ok = ();
    type Error = Error;
    fn serialize_field<T: ?Sized + Serialize>(
        &mut self,
        _key: &'static str,
        value: &T,
    ) -> Result<(), Error> {
        self.element(value)
    }
    fn end(self) -> Result<(), Error> {
        self.finish()
    }
}

struct MapEncoder<'a> {
    output: &'a mut Vec<u8>,
    entries: Vec<(Vec<u8>, Vec<u8>)>,
    key: Option<Vec<u8>>,
}
impl SerializeMap for MapEncoder<'_> {
    type Ok = ();
    type Error = Error;
    fn serialize_key<T: ?Sized + Serialize>(&mut self, key: &T) -> Result<(), Error> {
        let mut bytes = Vec::new();
        key.serialize(Encoder { output: &mut bytes })?;
        self.key = Some(bytes);
        Ok(())
    }
    fn serialize_value<T: ?Sized + Serialize>(&mut self, value: &T) -> Result<(), Error> {
        let key = self
            .key
            .take()
            .ok_or_else(|| Error::message("规范映射值缺少 key"))?;
        let mut bytes = Vec::new();
        value.serialize(Encoder { output: &mut bytes })?;
        self.entries.push((key, bytes));
        Ok(())
    }
    fn end(mut self) -> Result<(), Error> {
        if self.key.is_some() {
            return Err(Error::message("规范映射 key 缺少值"));
        }
        self.entries.sort_by(|left, right| left.0.cmp(&right.0));
        let length = u64::try_from(self.entries.len())
            .map_err(|_| Error::message("规范编码映射长度超过 u64"))?;
        self.output.extend_from_slice(&length.to_le_bytes());
        for (key, value) in self.entries {
            self.output.extend_from_slice(&key);
            self.output.extend_from_slice(&value);
        }
        Ok(())
    }
}

struct Decoder<'a> {
    bytes: &'a [u8],
    offset: usize,
}
impl<'a> Decoder<'a> {
    fn take(&mut self, length: usize) -> Result<&'a [u8], Error> {
        let end = self
            .offset
            .checked_add(length)
            .ok_or_else(|| Error::message("规范编码偏移溢出"))?;
        let bytes = self
            .bytes
            .get(self.offset..end)
            .ok_or_else(|| Error::message("规范编码被截断"))?;
        self.offset = end;
        Ok(bytes)
    }
    fn byte(&mut self) -> Result<u8, Error> {
        Ok(self.take(1)?[0])
    }
    fn u32(&mut self) -> Result<u32, Error> {
        Ok(u32::from_le_bytes(
            self.take(4)?.try_into().expect("u32 width"),
        ))
    }
    fn length(&mut self) -> Result<usize, Error> {
        usize::try_from(u64::from_le_bytes(
            self.take(8)?.try_into().expect("u64 宽度"),
        ))
        .map_err(|_| Error::message("规范编码长度不适配宿主"))
    }
    fn sequence<'de>(&'de mut self, length: usize) -> SequenceAccess<'de, 'a> {
        SequenceAccess {
            decoder: self,
            remaining: length,
        }
    }
}
struct SequenceAccess<'a, 'de> {
    decoder: &'a mut Decoder<'de>,
    remaining: usize,
}
impl<'de> SeqAccess<'de> for SequenceAccess<'_, 'de> {
    type Error = Error;
    fn next_element_seed<T: de::DeserializeSeed<'de>>(
        &mut self,
        seed: T,
    ) -> Result<Option<T::Value>, Error> {
        if self.remaining == 0 {
            return Ok(None);
        }
        self.remaining -= 1;
        seed.deserialize(&mut *self.decoder).map(Some)
    }
    fn size_hint(&self) -> Option<usize> {
        Some(
            self.remaining
                .min(self.decoder.bytes.len() - self.decoder.offset),
        )
    }
}
struct MapAccessImpl<'a, 'de> {
    decoder: &'a mut Decoder<'de>,
    remaining: usize,
    value_pending: bool,
    previous_key: Option<&'de [u8]>,
}
impl<'de> MapAccess<'de> for MapAccessImpl<'_, 'de> {
    type Error = Error;
    fn next_key_seed<K: de::DeserializeSeed<'de>>(
        &mut self,
        seed: K,
    ) -> Result<Option<K::Value>, Error> {
        if self.value_pending {
            return Err(Error::message("规范映射前一个值尚未消费"));
        }
        if self.remaining == 0 {
            return Ok(None);
        }
        self.value_pending = true;
        let start = self.decoder.offset;
        let key = seed.deserialize(&mut *self.decoder)?;
        let bytes = &self.decoder.bytes[start..self.decoder.offset];
        if self.previous_key.is_some_and(|previous| previous >= bytes) {
            return Err(Error::message("规范映射 key 未严格排序或重复"));
        }
        self.previous_key = Some(bytes);
        Ok(Some(key))
    }
    fn next_value_seed<V: de::DeserializeSeed<'de>>(&mut self, seed: V) -> Result<V::Value, Error> {
        if !self.value_pending {
            return Err(Error::message("规范映射未读取 key 就请求值"));
        }
        self.value_pending = false;
        self.remaining -= 1;
        seed.deserialize(&mut *self.decoder)
    }
    fn size_hint(&self) -> Option<usize> {
        Some(
            self.remaining
                .min(self.decoder.bytes.len() - self.decoder.offset),
        )
    }
}
struct EnumAccessImpl<'a, 'de> {
    decoder: &'a mut Decoder<'de>,
    index: u32,
}
impl<'a, 'de> EnumAccess<'de> for EnumAccessImpl<'a, 'de> {
    type Error = Error;
    type Variant = VariantAccessImpl<'a, 'de>;
    fn variant_seed<V: de::DeserializeSeed<'de>>(
        self,
        seed: V,
    ) -> Result<(V::Value, Self::Variant), Error> {
        let value =
            seed.deserialize(serde::de::value::U32Deserializer::<Error>::new(self.index))?;
        Ok((
            value,
            VariantAccessImpl {
                decoder: self.decoder,
            },
        ))
    }
}
struct VariantAccessImpl<'a, 'de> {
    decoder: &'a mut Decoder<'de>,
}
impl<'a, 'de> VariantAccess<'de> for VariantAccessImpl<'a, 'de> {
    type Error = Error;
    fn unit_variant(self) -> Result<(), Error> {
        Ok(())
    }
    fn newtype_variant_seed<T: de::DeserializeSeed<'de>>(self, seed: T) -> Result<T::Value, Error> {
        seed.deserialize(self.decoder)
    }
    fn tuple_variant<V: Visitor<'de>>(self, len: usize, visitor: V) -> Result<V::Value, Error> {
        de::Deserializer::deserialize_tuple(self.decoder, len, visitor)
    }
    fn struct_variant<V: Visitor<'de>>(
        self,
        fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Error> {
        de::Deserializer::deserialize_struct(self.decoder, "variant", fields, visitor)
    }
}

impl<'de, 'a> Deserializer<'de> for &'a mut Decoder<'de> {
    type Error = Error;
    fn deserialize_any<V: Visitor<'de>>(self, _visitor: V) -> Result<V::Value, Error> {
        Err(Error::message("规范编码不支持 deserialize_any"))
    }
    fn deserialize_bool<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        match self.byte()? {
            0 => visitor.visit_bool(false),
            1 => visitor.visit_bool(true),
            _ => Err(Error::message("规范编码 bool 判别值无效")),
        }
    }
    fn deserialize_i8<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        visitor.visit_i8(i8::from_le_bytes(
            self.take(1)?.try_into().expect("i8 width"),
        ))
    }
    fn deserialize_i16<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        visitor.visit_i16(i16::from_le_bytes(
            self.take(2)?.try_into().expect("i16 width"),
        ))
    }
    fn deserialize_i32<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        visitor.visit_i32(i32::from_le_bytes(
            self.take(4)?.try_into().expect("i32 width"),
        ))
    }
    fn deserialize_i64<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        visitor.visit_i64(i64::from_le_bytes(
            self.take(8)?.try_into().expect("i64 width"),
        ))
    }
    fn deserialize_i128<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        visitor.visit_i128(i128::from_le_bytes(
            self.take(16)?.try_into().expect("i128 width"),
        ))
    }
    fn deserialize_u8<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        visitor.visit_u8(self.byte()?)
    }
    fn deserialize_u16<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        visitor.visit_u16(u16::from_le_bytes(
            self.take(2)?.try_into().expect("u16 width"),
        ))
    }
    fn deserialize_u32<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        visitor.visit_u32(self.u32()?)
    }
    fn deserialize_u64<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        visitor.visit_u64(u64::from_le_bytes(
            self.take(8)?.try_into().expect("u64 width"),
        ))
    }
    fn deserialize_u128<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        visitor.visit_u128(u128::from_le_bytes(
            self.take(16)?.try_into().expect("u128 width"),
        ))
    }
    fn deserialize_f32<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        visitor.visit_f32(f32::from_le_bytes(
            self.take(4)?.try_into().expect("f32 width"),
        ))
    }
    fn deserialize_f64<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        visitor.visit_f64(f64::from_le_bytes(
            self.take(8)?.try_into().expect("f64 width"),
        ))
    }
    fn deserialize_char<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        char::from_u32(self.u32()?)
            .ok_or_else(|| Error::message("规范编码字符无效"))
            .and_then(|value| visitor.visit_char(value))
    }
    fn deserialize_str<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        let length = self.length()?;
        let bytes = self.take(length)?;
        let value =
            std::str::from_utf8(bytes).map_err(|_| Error::message("规范编码字符串不是 UTF-8"))?;
        visitor.visit_borrowed_str(value)
    }
    fn deserialize_string<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        let length = self.length()?;
        let bytes = self.take(length)?;
        let value = String::from_utf8(bytes.to_owned())
            .map_err(|_| Error::message("规范编码字符串不是 UTF-8"))?;
        visitor.visit_string(value)
    }
    fn deserialize_bytes<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        let length = self.length()?;
        let bytes = self.take(length)?;
        visitor.visit_borrowed_bytes(bytes)
    }
    fn deserialize_byte_buf<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        let length = self.length()?;
        visitor.visit_byte_buf(self.take(length)?.to_owned())
    }
    fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        match self.byte()? {
            0 => visitor.visit_none(),
            1 => visitor.visit_some(self),
            _ => Err(Error::message("规范编码 Option 判别值无效")),
        }
    }
    fn deserialize_unit<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        visitor.visit_unit()
    }
    fn deserialize_unit_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value, Error> {
        visitor.visit_unit()
    }
    fn deserialize_newtype_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value, Error> {
        visitor.visit_newtype_struct(self)
    }
    fn deserialize_seq<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        let length = self.length()?;
        let mut access = self.sequence(length);
        let value = visitor.visit_seq(&mut access)?;
        if access.remaining != 0 {
            return Err(Error::message("规范编码序列未完整消费"));
        }
        Ok(value)
    }
    fn deserialize_tuple<V: Visitor<'de>>(
        self,
        length: usize,
        visitor: V,
    ) -> Result<V::Value, Error> {
        if self.length()? != length {
            return Err(Error::message("规范编码元组长度不符"));
        }
        let mut access = self.sequence(length);
        let value = visitor.visit_seq(&mut access)?;
        if access.remaining != 0 {
            return Err(Error::message("规范编码元组未完整消费"));
        }
        Ok(value)
    }
    fn deserialize_tuple_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        length: usize,
        visitor: V,
    ) -> Result<V::Value, Error> {
        self.deserialize_tuple(length, visitor)
    }
    fn deserialize_map<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        let length = self.length()?;
        let mut access = MapAccessImpl {
            decoder: self,
            remaining: length,
            value_pending: false,
            previous_key: None,
        };
        let value = visitor.visit_map(&mut access)?;
        if access.remaining != 0 || access.value_pending {
            return Err(Error::message("规范编码映射未完整消费"));
        }
        Ok(value)
    }
    fn deserialize_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Error> {
        let mut access = self.sequence(fields.len());
        let value = visitor.visit_seq(&mut access)?;
        if access.remaining != 0 {
            return Err(Error::message("规范编码结构未完整消费"));
        }
        Ok(value)
    }
    fn deserialize_enum<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Error> {
        let index = u32::from(u16::from_le_bytes(
            self.take(2)?.try_into().expect("u16 宽度"),
        ));
        visitor.visit_enum(EnumAccessImpl {
            decoder: self,
            index,
        })
    }
    fn deserialize_identifier<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        self.deserialize_u32(visitor)
    }
    fn deserialize_ignored_any<V: Visitor<'de>>(self, _visitor: V) -> Result<V::Value, Error> {
        Err(Error::message("规范编码不支持忽略未知字段"))
    }
    fn is_human_readable(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::{from_slice, to_vec};
    use serde::{Deserialize, Serialize};
    use std::collections::BTreeMap;

    #[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
    struct Sample {
        number: u32,
        label: String,
        values: Vec<u8>,
        choice: Choice,
        optional: Option<[u8; 2]>,
        map: BTreeMap<String, u16>,
    }
    #[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
    enum Choice {
        Empty,
        Pair(u16, bool),
        Named { key: String },
    }

    #[test]
    fn round_trip_is_deterministic_and_rejects_malformed_payloads() {
        let mut map = BTreeMap::new();
        map.insert("b".to_owned(), 2);
        map.insert("a".to_owned(), 1);
        let value = Sample {
            number: 7,
            label: "gugu".to_owned(),
            values: vec![1, 2, 3],
            choice: Choice::Named {
                key: "x".to_owned(),
            },
            optional: Some([4, 5]),
            map,
        };
        let bytes = to_vec(&value).expect("encode");
        assert_eq!(bytes, to_vec(&value).expect("encode again"));
        assert_eq!(from_slice::<Sample>(&bytes).expect("decode"), value);
        let mut trailing = bytes.clone();
        trailing.push(0);
        assert!(from_slice::<Sample>(&trailing).is_err());
        assert!(from_slice::<Sample>(&bytes[..bytes.len() - 1]).is_err());
    }

    #[test]
    fn map_insertion_order_does_not_change_bytes_and_decode_rejects_duplicate_keys() {
        use std::collections::HashMap;
        let first: HashMap<u16, u8> = [(256, 3), (1, 7)].into_iter().collect();
        let second: HashMap<u16, u8> = [(1, 7), (256, 3)].into_iter().collect();
        let bytes = to_vec(&first).expect("映射编码");
        assert_eq!(bytes, to_vec(&second).expect("反序插入映射编码"));
        assert_eq!(
            from_slice::<HashMap<u16, u8>>(&bytes).expect("映射解码"),
            first
        );
        let mut duplicate = bytes.clone();
        duplicate[11..13].copy_from_slice(&bytes[8..10]);
        assert!(from_slice::<HashMap<u16, u8>>(&duplicate).is_err());
        let mut unordered = bytes;
        let left = unordered[8..11].to_vec();
        let right = unordered[11..14].to_vec();
        unordered[8..11].copy_from_slice(&right);
        unordered[11..14].copy_from_slice(&left);
        assert!(from_slice::<HashMap<u16, u8>>(&unordered).is_err());
    }

    #[test]
    fn tags_lengths_and_utf8_are_checked() {
        for choice in [
            Choice::Empty,
            Choice::Pair(13, true),
            Choice::Named {
                key: "喵".to_owned(),
            },
        ] {
            let bytes = to_vec(&choice).expect("枚举编码");
            assert_eq!(from_slice::<Choice>(&bytes).expect("枚举解码"), choice);
        }
        assert_eq!(
            from_slice::<Option<u32>>(&to_vec(&Some(42_u32)).expect("Option 编码"))
                .expect("Option 解码"),
            Some(42)
        );
        assert_eq!(from_slice::<Option<u32>>(&[0]).expect("None 解码"), None);
        assert!(from_slice::<Option<u32>>(&[2]).is_err());
        assert!(from_slice::<bool>(&[2]).is_err());
        assert!(from_slice::<Choice>(&u16::MAX.to_le_bytes()).is_err());
        assert!(from_slice::<Vec<u8>>(&u64::MAX.to_le_bytes()).is_err());
        let mut invalid_utf8 = 1_u64.to_le_bytes().to_vec();
        invalid_utf8.push(255);
        assert!(from_slice::<String>(&invalid_utf8).is_err());
        let pair = (7_u16, [2_u8, 3]);
        assert_eq!(
            from_slice::<(u16, [u8; 2])>(&to_vec(&pair).expect("元组编码")).expect("元组解码"),
            pair
        );
        assert!(from_slice::<Sample>(br#"{"number":7}"#).is_err());
    }
}
