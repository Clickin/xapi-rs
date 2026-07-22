use base64::{engine::general_purpose::STANDARD, Engine};
use std::{borrow::Cow, fmt, str::FromStr};

use crate::{DecodeError, EncodeError};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireType {
    String,
    Int,
    Long,
    Float,
    Double,
    Decimal,
    BigDecimal,
    Date,
    Time,
    DateTime,
    Blob,
    Boolean,
}

impl fmt::Display for WireType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::String => "STRING",
            Self::Int => "INT",
            Self::Long => "LONG",
            Self::Float => "FLOAT",
            Self::Double => "DOUBLE",
            Self::Decimal => "DECIMAL",
            Self::BigDecimal => "BIGDECIMAL",
            Self::Date => "DATE",
            Self::Time => "TIME",
            Self::DateTime => "DATETIME",
            Self::Blob => "BLOB",
            Self::Boolean => "BOOLEAN",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LexicalValue<'a> {
    Missing,
    Null,
    Borrowed(&'a str),
    Owned(String),
    Bytes(Cow<'a, [u8]>),
}

impl<'a> LexicalValue<'a> {
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::Borrowed(v) => Some(v),
            Self::Owned(v) => Some(v),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub max_document_bytes: u64,
    pub max_depth: usize,
    pub max_datasets: usize,
    pub max_columns: usize,
    pub max_rows: usize,
    pub max_scalar_bytes: usize,
    pub max_blob_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_document_bytes: 64 << 20,
            max_depth: 64,
            max_datasets: 1024,
            max_columns: 4096,
            max_rows: 1_000_000,
            // A 32 MiB BLOB is about 43 MiB in base64 form.
            max_scalar_bytes: 64 << 20,
            max_blob_bytes: 32 << 20,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Presence<T> {
    Missing,
    Null,
    Value(T),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decimal(String);

impl Decimal {
    pub fn new(value: impl Into<String>) -> Result<Self, DecodeError> {
        let value = value.into();
        if validate_decimal(&value) {
            Ok(Self(value))
        } else {
            Err(DecodeError::scalar(value, WireType::BigDecimal, "Decimal"))
        }
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Decimal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
impl FromStr for Decimal {
    type Err = DecodeError;
    fn from_str(v: &str) -> Result<Self, Self::Err> {
        Self::new(v)
    }
}

fn validate_decimal(v: &str) -> bool {
    let bytes = v.as_bytes();
    if bytes.is_empty() {
        return false;
    }
    let mut i = usize::from(bytes[0] == b'+' || bytes[0] == b'-');
    let start = i;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    let integer = i > start;
    let fraction = if i < bytes.len() && bytes[i] == b'.' {
        i += 1;
        let begin = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        i > begin
    } else {
        false
    };
    (integer || fraction) && i == bytes.len()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Blob(Vec<u8>);

impl Blob {
    pub fn new(bytes: Vec<u8>) -> Self {
        Self(bytes)
    }
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
    pub fn into_inner(self) -> Vec<u8> {
        self.0
    }
    pub fn from_base64(value: &str, max: usize) -> Result<Self, DecodeError> {
        let decoded = STANDARD
            .decode(value)
            .map_err(|_| DecodeError::message("invalid BLOB base64"))?;
        if decoded.len() > max {
            return Err(DecodeError::Limit("max_blob_bytes"));
        }
        Ok(Self(decoded))
    }
    pub fn to_base64(&self) -> String {
        STANDARD.encode(&self.0)
    }
}

pub(crate) fn decode_base64_in_place(
    value: String,
    max_bytes: usize,
) -> Result<Vec<u8>, DecodeError> {
    let mut bytes = value.into_bytes();
    if !bytes.len().is_multiple_of(4) {
        return Err(DecodeError::message("invalid BLOB base64 length"));
    }
    let padding = bytes.iter().rev().take_while(|byte| **byte == b'=').count();
    if padding > 2 || bytes.len() / 4 * 3 - padding > max_bytes {
        return Err(DecodeError::Limit("max_blob_bytes"));
    }
    let output_len = bytes.len() / 4 * 3 - padding;
    let mut write = 0;
    for index in 0..bytes.len() / 4 {
        let offset = index * 4;
        let chunk = [
            bytes[offset],
            bytes[offset + 1],
            bytes[offset + 2],
            bytes[offset + 3],
        ];
        let last = index + 1 == bytes.len() / 4;
        let a =
            base64_digit(chunk[0]).ok_or_else(|| DecodeError::message("invalid BLOB base64"))?;
        let b =
            base64_digit(chunk[1]).ok_or_else(|| DecodeError::message("invalid BLOB base64"))?;
        let c = if chunk[2] == b'=' {
            if !last || chunk[3] != b'=' {
                return Err(DecodeError::message("invalid BLOB base64 padding"));
            }
            0
        } else {
            base64_digit(chunk[2]).ok_or_else(|| DecodeError::message("invalid BLOB base64"))?
        };
        let d = if chunk[3] == b'=' {
            if !last {
                return Err(DecodeError::message("invalid BLOB base64 padding"));
            }
            0
        } else {
            base64_digit(chunk[3]).ok_or_else(|| DecodeError::message("invalid BLOB base64"))?
        };
        if chunk[2] == b'=' && (b & 0x0F) != 0 || chunk[3] == b'=' && (c & 0x03) != 0 {
            return Err(DecodeError::message("invalid BLOB base64 padding"));
        }
        if write < output_len {
            bytes[write] = (a << 2) | (b >> 4);
            write += 1;
        }
        if write < output_len {
            bytes[write] = (b << 4) | (c >> 2);
            write += 1;
        }
        if write < output_len {
            bytes[write] = (c << 6) | d;
            write += 1;
        }
    }
    bytes.truncate(output_len);
    bytes.shrink_to_fit();
    Ok(bytes)
}

fn base64_digit(byte: u8) -> Option<u8> {
    match byte {
        b'A'..=b'Z' => Some(byte - b'A'),
        b'a'..=b'z' => Some(byte - b'a' + 26),
        b'0'..=b'9' => Some(byte - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XapiDate(String);
impl XapiDate {
    pub fn new(v: impl Into<String>) -> Result<Self, DecodeError> {
        let v = v.into();
        if valid_date(&v) {
            Ok(Self(v))
        } else {
            Err(DecodeError::scalar(v, WireType::Date, "XapiDate"))
        }
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Time(String);
impl Time {
    pub fn new(v: impl Into<String>) -> Result<Self, DecodeError> {
        let v = v.into();
        if valid_time(&v) {
            Ok(Self(v))
        } else {
            Err(DecodeError::scalar(v, WireType::Time, "Time"))
        }
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DateTime(String);
impl DateTime {
    pub fn new(v: impl Into<String>) -> Result<Self, DecodeError> {
        let v = v.into();
        let valid = if v.as_bytes().get(8) == Some(&b'T') {
            v.len() >= 9 && v.get(..8).is_some_and(valid_date) && v.get(9..).is_some_and(valid_time)
        } else {
            valid_compact_datetime(&v)
        };
        if valid {
            Ok(Self(v))
        } else {
            Err(DecodeError::scalar(v, WireType::DateTime, "DateTime"))
        }
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

fn valid_date(v: &str) -> bool {
    if v.len() != 8 || !v.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    let month: u32 = v[4..6].parse().unwrap_or(0);
    let day: u32 = v[6..8].parse().unwrap_or(0);
    let year: u32 = v[..4].parse().unwrap_or(0);
    (1..=12).contains(&month)
        && day >= 1
        && day <= [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31][month as usize - 1]
        && (month != 2
            || day != 29
            || year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400)))
}
fn valid_time(v: &str) -> bool {
    let bytes = v.as_bytes();
    if matches!(bytes.len(), 6 | 9) && bytes.iter().all(u8::is_ascii_digit) {
        return valid_clock_digits(&bytes[..6]);
    }
    let main = bytes.get(..6).unwrap_or(&[]);
    let precision = match bytes.get(6..) {
        Some([]) => true,
        Some([b'.', fraction @ ..]) => {
            !fraction.is_empty() && fraction.iter().all(u8::is_ascii_digit)
        }
        _ => false,
    };
    main.len() == 6
        && main.iter().all(u8::is_ascii_digit)
        && precision
        && main[..2].iter().all(u8::is_ascii_digit)
        && main[2..4].iter().all(u8::is_ascii_digit)
        && main[4..6].iter().all(u8::is_ascii_digit)
        && valid_clock_digits(main)
}

fn valid_clock_digits(main: &[u8]) -> bool {
    main.len() == 6
        && main.iter().all(u8::is_ascii_digit)
        && main[..2]
            .iter()
            .fold(0_u32, |v, digit| v * 10 + u32::from(digit - b'0'))
            < 24
        && main[2..4]
            .iter()
            .fold(0_u32, |v, digit| v * 10 + u32::from(digit - b'0'))
            < 60
        && main[4..6]
            .iter()
            .fold(0_u32, |v, digit| v * 10 + u32::from(digit - b'0'))
            < 60
}

fn valid_compact_datetime(v: &str) -> bool {
    matches!(v.len(), 14..=17)
        && v.bytes().all(|byte| byte.is_ascii_digit())
        && v.get(..8).is_some_and(valid_date)
        && v.get(8..14)
            .is_some_and(|clock| valid_clock_digits(clock.as_bytes()))
}

pub trait XapiScalar: Sized {
    fn decode_lexical(value: LexicalValue<'_>, wire_type: WireType) -> Result<Self, DecodeError>;
    fn decode_lexical_with_limits(
        value: LexicalValue<'_>,
        wire_type: WireType,
        limits: Limits,
    ) -> Result<Self, DecodeError> {
        let _ = limits;
        Self::decode_lexical(value, wire_type)
    }
    fn encode_lexical(&self, wire_type: WireType) -> Result<String, EncodeError>;

    fn encode_bytes(&self, _: WireType) -> Option<&[u8]> {
        None
    }
}

pub trait XapiField: Sized {
    fn decode_field(value: &crate::RawValue, wire_type: WireType) -> Result<Self, DecodeError>;
    fn decode_field_with_limits(
        value: &crate::RawValue,
        wire_type: WireType,
        limits: Limits,
    ) -> Result<Self, DecodeError> {
        let _ = limits;
        Self::decode_field(value, wire_type)
    }
    fn decode_field_owned(
        value: crate::RawValue,
        wire_type: WireType,
        limits: Limits,
    ) -> Result<Self, DecodeError> {
        match value {
            crate::RawValue::Text(value) => {
                Self::decode_field_with_limits(&crate::RawValue::Text(value), wire_type, limits)
            }
            crate::RawValue::Bytes(value) => {
                Self::decode_field_with_limits(&crate::RawValue::Bytes(value), wire_type, limits)
            }
            crate::RawValue::Missing => Err(DecodeError::message("missing required value")),
            crate::RawValue::Null => Err(DecodeError::message("null is not valid for this field")),
        }
    }
    fn encode_field(&self, wire_type: WireType) -> Result<crate::RawValue, EncodeError>;
}

impl<T: XapiScalar> XapiField for T {
    fn decode_field(value: &crate::RawValue, wire_type: WireType) -> Result<Self, DecodeError> {
        match value {
            crate::RawValue::Text(value) => {
                T::decode_lexical(LexicalValue::Borrowed(value), wire_type)
            }
            crate::RawValue::Bytes(value) => {
                T::decode_lexical(LexicalValue::Bytes(Cow::Borrowed(value)), wire_type)
            }
            crate::RawValue::Missing => Err(DecodeError::message("missing required value")),
            crate::RawValue::Null => Err(DecodeError::message("null is not valid for this field")),
        }
    }
    fn decode_field_with_limits(
        value: &crate::RawValue,
        wire_type: WireType,
        limits: Limits,
    ) -> Result<Self, DecodeError> {
        match value {
            crate::RawValue::Text(value) => {
                T::decode_lexical_with_limits(LexicalValue::Borrowed(value), wire_type, limits)
            }
            crate::RawValue::Bytes(value) => T::decode_lexical_with_limits(
                LexicalValue::Bytes(Cow::Borrowed(value)),
                wire_type,
                limits,
            ),
            crate::RawValue::Missing => Err(DecodeError::message("missing required value")),
            crate::RawValue::Null => Err(DecodeError::message("null is not valid for this field")),
        }
    }
    fn decode_field_owned(
        value: crate::RawValue,
        wire_type: WireType,
        limits: Limits,
    ) -> Result<Self, DecodeError> {
        match value {
            crate::RawValue::Text(value) => {
                T::decode_lexical_with_limits(LexicalValue::Owned(value), wire_type, limits)
            }
            crate::RawValue::Bytes(value) => T::decode_lexical_with_limits(
                LexicalValue::Bytes(Cow::Owned(value)),
                wire_type,
                limits,
            ),
            crate::RawValue::Missing => Err(DecodeError::message("missing required value")),
            crate::RawValue::Null => Err(DecodeError::message("null is not valid for this field")),
        }
    }
    fn encode_field(&self, wire_type: WireType) -> Result<crate::RawValue, EncodeError> {
        if wire_type == WireType::Blob {
            if let Some(bytes) = T::encode_bytes(self, wire_type) {
                return Ok(crate::RawValue::Bytes(bytes.to_owned()));
            }
        }
        Ok(crate::RawValue::Text(self.encode_lexical(wire_type)?))
    }
}

impl<T: XapiField> XapiField for Option<T> {
    fn decode_field(value: &crate::RawValue, wire_type: WireType) -> Result<Self, DecodeError> {
        match value {
            crate::RawValue::Missing | crate::RawValue::Null => Ok(None),
            _ => T::decode_field(value, wire_type).map(Some),
        }
    }
    fn decode_field_with_limits(
        value: &crate::RawValue,
        wire_type: WireType,
        limits: Limits,
    ) -> Result<Self, DecodeError> {
        match value {
            crate::RawValue::Missing | crate::RawValue::Null => Ok(None),
            _ => T::decode_field_with_limits(value, wire_type, limits).map(Some),
        }
    }
    fn decode_field_owned(
        value: crate::RawValue,
        wire_type: WireType,
        limits: Limits,
    ) -> Result<Self, DecodeError> {
        match value {
            crate::RawValue::Missing | crate::RawValue::Null => Ok(None),
            value => T::decode_field_owned(value, wire_type, limits).map(Some),
        }
    }
    fn encode_field(&self, wire_type: WireType) -> Result<crate::RawValue, EncodeError> {
        match self {
            Some(value) => value.encode_field(wire_type),
            None => Ok(crate::RawValue::Missing),
        }
    }
}

impl<T: XapiField> XapiField for crate::Presence<T> {
    fn decode_field(value: &crate::RawValue, wire_type: WireType) -> Result<Self, DecodeError> {
        match value {
            crate::RawValue::Missing => Ok(Self::Missing),
            crate::RawValue::Null => Ok(Self::Null),
            value => T::decode_field(value, wire_type).map(Self::Value),
        }
    }
    fn decode_field_with_limits(
        value: &crate::RawValue,
        wire_type: WireType,
        limits: Limits,
    ) -> Result<Self, DecodeError> {
        match value {
            crate::RawValue::Missing => Ok(Self::Missing),
            crate::RawValue::Null => Ok(Self::Null),
            value => T::decode_field_with_limits(value, wire_type, limits).map(Self::Value),
        }
    }
    fn decode_field_owned(
        value: crate::RawValue,
        wire_type: WireType,
        limits: Limits,
    ) -> Result<Self, DecodeError> {
        match value {
            crate::RawValue::Missing => Ok(Self::Missing),
            crate::RawValue::Null => Ok(Self::Null),
            value => T::decode_field_owned(value, wire_type, limits).map(Self::Value),
        }
    }
    fn encode_field(&self, wire_type: WireType) -> Result<crate::RawValue, EncodeError> {
        match self {
            Self::Missing => Ok(crate::RawValue::Missing),
            Self::Null => Ok(crate::RawValue::Null),
            Self::Value(value) => value.encode_field(wire_type),
        }
    }
}

impl XapiScalar for String {
    fn decode_lexical(value: LexicalValue<'_>, _: WireType) -> Result<Self, DecodeError> {
        match value {
            LexicalValue::Borrowed(value) => Ok(value.to_owned()),
            LexicalValue::Owned(value) => Ok(value),
            _ => Err(DecodeError::message("expected text value")),
        }
    }
    fn encode_lexical(&self, _: WireType) -> Result<String, EncodeError> {
        Ok(self.clone())
    }
}

macro_rules! scalar_number {
    ($($ty:ty),* $(,)?) => { $(
        impl XapiScalar for $ty {
            fn decode_lexical(value: LexicalValue<'_>, wire_type: WireType) -> Result<Self, DecodeError> {
                let text = value.as_str().ok_or_else(|| DecodeError::message("expected numeric text"))?;
                text.parse().map_err(|_| DecodeError::scalar(text, wire_type, stringify!($ty)))
            }
            fn encode_lexical(&self, _: WireType) -> Result<String, EncodeError> { Ok(self.to_string()) }
        }
    )* };
}

scalar_number!(i32, i64, f32, f64);

impl XapiScalar for bool {
    fn decode_lexical(value: LexicalValue<'_>, wire_type: WireType) -> Result<Self, DecodeError> {
        match value.as_str() {
            Some("true") | Some("1") => Ok(true),
            Some("false") | Some("0") => Ok(false),
            Some(text) => Err(DecodeError::scalar(text, wire_type, "bool")),
            None => Err(DecodeError::message("expected boolean text")),
        }
    }
    fn encode_lexical(&self, _: WireType) -> Result<String, EncodeError> {
        Ok(self.to_string())
    }
}

impl XapiScalar for Blob {
    fn decode_lexical(value: LexicalValue<'_>, _: WireType) -> Result<Self, DecodeError> {
        match value {
            LexicalValue::Bytes(value) => Ok(Self::new(value.into_owned())),
            value => Self::from_base64(
                value
                    .as_str()
                    .ok_or_else(|| DecodeError::message("expected BLOB text"))?,
                Limits::default().max_blob_bytes,
            ),
        }
    }
    fn decode_lexical_with_limits(
        value: LexicalValue<'_>,
        _: WireType,
        limits: Limits,
    ) -> Result<Self, DecodeError> {
        match value {
            LexicalValue::Bytes(value) => {
                if value.len() > limits.max_blob_bytes {
                    Err(DecodeError::Limit("max_blob_bytes"))
                } else {
                    Ok(Self::new(value.into_owned()))
                }
            }
            LexicalValue::Owned(value) => {
                decode_base64_in_place(value, limits.max_blob_bytes).map(Self::new)
            }
            value => Self::from_base64(
                value
                    .as_str()
                    .ok_or_else(|| DecodeError::message("expected BLOB text"))?,
                limits.max_blob_bytes,
            ),
        }
    }
    fn encode_lexical(&self, _: WireType) -> Result<String, EncodeError> {
        Ok(self.to_base64())
    }
    fn encode_bytes(&self, _: WireType) -> Option<&[u8]> {
        Some(&self.0)
    }
}

impl XapiScalar for Vec<u8> {
    fn decode_lexical(value: LexicalValue<'_>, wire_type: WireType) -> Result<Self, DecodeError> {
        Blob::decode_lexical(value, wire_type).map(Blob::into_inner)
    }

    fn decode_lexical_with_limits(
        value: LexicalValue<'_>,
        wire_type: WireType,
        limits: Limits,
    ) -> Result<Self, DecodeError> {
        Blob::decode_lexical_with_limits(value, wire_type, limits).map(Blob::into_inner)
    }

    fn encode_lexical(&self, _: WireType) -> Result<String, EncodeError> {
        Ok(STANDARD.encode(self))
    }
    fn encode_bytes(&self, _: WireType) -> Option<&[u8]> {
        Some(self)
    }
}

macro_rules! scalar_lexical_type {
    ($ty:ty) => {
        impl XapiScalar for $ty {
            fn decode_lexical(value: LexicalValue<'_>, _: WireType) -> Result<Self, DecodeError> {
                <$ty>::new(
                    value
                        .as_str()
                        .ok_or_else(|| DecodeError::message("expected text"))?
                        .to_owned(),
                )
            }
            fn encode_lexical(&self, _: WireType) -> Result<String, EncodeError> {
                Ok(self.as_str().to_owned())
            }
        }
    };
}

scalar_lexical_type!(XapiDate);
scalar_lexical_type!(Time);
scalar_lexical_type!(DateTime);

impl XapiScalar for Decimal {
    fn decode_lexical(value: LexicalValue<'_>, _: WireType) -> Result<Self, DecodeError> {
        value
            .as_str()
            .ok_or_else(|| DecodeError::message("expected decimal text"))?
            .parse()
    }
    fn encode_lexical(&self, _: WireType) -> Result<String, EncodeError> {
        Ok(self.0.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn decimal_preserves_lexical_scale() {
        assert_eq!(Decimal::new("65.7700").unwrap().as_str(), "65.7700");
    }
    #[test]
    fn blob_round_trip() {
        let b = Blob::new(vec![0, 1, 2, 255]);
        assert_eq!(Blob::from_base64(&b.to_base64(), 8).unwrap(), b);
        let bytes =
            Vec::<u8>::decode_lexical(LexicalValue::Borrowed("AAEC/w=="), WireType::Blob).unwrap();
        assert_eq!(bytes, vec![0, 1, 2, 255]);
    }

    #[test]
    fn in_place_base64_decode_preserves_padding_rules() {
        assert_eq!(decode_base64_in_place("TQ==".into(), 1).unwrap(), b"M");
        assert_eq!(decode_base64_in_place("TWE=".into(), 2).unwrap(), b"Ma");
        assert_eq!(decode_base64_in_place("TWFu".into(), 3).unwrap(), b"Man");
        assert!(decode_base64_in_place("TQ=".into(), 8).is_err());
        assert!(decode_base64_in_place("T=Fu".into(), 8).is_err());
    }
    #[test]
    fn default_limits_cover_the_32mb_blob_lexical_form() {
        let limits = Limits::default();
        let encoded = limits.max_blob_bytes.div_ceil(3) * 4;
        assert!(encoded <= limits.max_scalar_bytes);
    }

    #[test]
    fn option_distinguishes_missing_null_and_empty() {
        assert_eq!(
            Option::<String>::decode_field(&crate::RawValue::Missing, WireType::String).unwrap(),
            None
        );
        assert_eq!(
            Option::<String>::decode_field(&crate::RawValue::Null, WireType::String).unwrap(),
            None
        );
        assert_eq!(
            Option::<String>::decode_field(&crate::RawValue::Text(String::new()), WireType::String)
                .unwrap(),
            Some(String::new())
        );
    }
    #[test]
    fn dates_are_timezone_free_and_validated() {
        assert!(XapiDate::new("20240229").is_ok());
        assert!(XapiDate::new("20230229").is_err());
        assert!(Time::new("235959").is_ok());
        assert!(Time::new("235959123").is_ok());
        assert!(Time::new("235959.123").is_ok());
        assert!(Time::new("235959x").is_err());
        assert!(DateTime::new("20240229T235959.123").is_ok());
        assert!(DateTime::new("20240229235959123").is_ok());
        assert!(DateTime::new("20240229X235959").is_err());
        assert!(DateTime::new("20240229T235959Z").is_err());
    }
}
