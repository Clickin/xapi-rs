//! What the original XPLATFORM and Nexacro drivers write and read.
//!
//! The drivers convert every scalar through Java's `StringTypeConverter` and
//! print the result with the Java `toString` forms. The functions here
//! reproduce that conversion, and the writers produce the drivers' bytes for
//! each format: the XML, JSON, SSV and PlatformBinary encoders of this crate
//! all go through them.
//!
//! Rows are written in the drivers' saveType ALL layout: live rows in order,
//! then deleted rows; an updated row is written over its original row (an
//! empty one when it has none), and a cell the updated row leaves out keeps
//! the original value. Other rows are written without an original row.

use std::io::Write;

use base64::{engine::general_purpose::STANDARD, Engine};

use crate::{EncodeError, Error, RawDataset, RawDocument, RawValue, Result, RowType};

// ------------------------------------------------------------------- types

/// Upper-cases a type name; a missing type is `STRING`.
pub fn type_name(data_type: &str) -> String {
    if data_type.is_empty() {
        "STRING".to_owned()
    } else {
        data_type.to_ascii_uppercase()
    }
}

/// Whether the type holds bytes (`BLOB`, `FILE`).
pub fn is_blob(data_type: &str) -> bool {
    matches!(type_name(data_type).as_str(), "BLOB" | "FILE")
}

/// Whether the type holds text (`STRING`, `CHAR`, `BLOB`, `FILE`).
pub fn is_text(data_type: &str) -> bool {
    matches!(
        type_name(data_type).as_str(),
        "STRING" | "CHAR" | "BLOB" | "FILE"
    )
}

/// The type name the drivers put on the wire.
pub fn wire_type(data_type: &str) -> String {
    let name = type_name(data_type);
    match name.as_str() {
        "BOOLEAN" | "SHORT" | "USHORT" | "INT" | "UINT" => "int".to_owned(),
        "LONG" | "ULONG" | "DECIMAL" | "BIGDECIMAL" => "bigdecimal".to_owned(),
        "FLOAT" | "DOUBLE" => "float".to_owned(),
        "FILE" | "BLOB" => "blob".to_owned(),
        "STRING" | "CHAR" => "string".to_owned(),
        _ => name.to_ascii_lowercase(),
    }
}

/// The column size the XML, JSON and PlatformBinary writers use when none is set.
pub fn default_size(data_type: &str) -> u32 {
    match type_name(data_type).as_str() {
        "SHORT" | "USHORT" | "INT" | "UINT" | "FLOAT" => 4,
        "LONG" | "ULONG" | "DOUBLE" => 8,
        "DECIMAL" | "BIGDECIMAL" => 16,
        "BOOLEAN" => 2,
        "DATE" => 6,
        "TIME" => 9,
        "DATETIME" => 17,
        "FILE" | "BLOB" => 256,
        _ => 32,
    }
}

/// The column size the SSV writer uses when none is set.
pub fn ssv_default_size(data_type: &str) -> u32 {
    match type_name(data_type).as_str() {
        "CHAR" => 1,
        "STRING" => 32,
        "SHORT" | "USHORT" | "BOOLEAN" => 2,
        "INT" | "UINT" | "FLOAT" => 4,
        "LONG" | "ULONG" | "DOUBLE" => 8,
        "DECIMAL" | "BIGDECIMAL" => 16,
        "DATE" => 6,
        "TIME" => 9,
        "DATETIME" => 17,
        "BLOB" | "FILE" => 256,
        _ => 0,
    }
}

// ------------------------------------------------------------- conversions

/// The values the drivers read as true.
pub fn is_true(value: &str) -> bool {
    matches!(
        value,
        "true" | "True" | "TRUE" | "yes" | "Yes" | "YES" | "y" | "Y" | "on" | "On" | "ON" | "1"
    )
}

/// Converts a value before it is written: the lexical the drivers write for a
/// cell or parameter of the type. `None` where the driver holds null (an
/// invalid BIGDECIMAL or date). An empty value stays empty.
pub fn encode(data_type: &str, lexical: &str) -> Option<String> {
    if lexical.is_empty() {
        return Some(String::new());
    }
    encode_value(&type_name(data_type), lexical)
}

fn encode_value(name: &str, lexical: &str) -> Option<String> {
    Some(match name {
        "BOOLEAN" => (if is_true(lexical) { "1" } else { "0" }).to_owned(),
        "SHORT" | "USHORT" | "INT" | "UINT" => java_integer(lexical, 32),
        "LONG" | "ULONG" => java_integer(lexical, 64),
        "FLOAT" => java_floating(lexical, 32),
        "DOUBLE" => java_floating(lexical, 64),
        "DECIMAL" | "BIGDECIMAL" => return java_big_decimal(lexical),
        "DATE" | "TIME" | "DATETIME" => return java_date_lexical(lexical, name),
        _ => lexical.to_owned(),
    })
}

/// Converts a decoded wire value as the drivers' typed setters do; the type
/// is the wire type. An empty number reads as zero, an empty BIGDECIMAL or
/// date as null, an empty BOOLEAN as false. BOOLEAN reads as `true`/`false`,
/// FLOAT as a double. BLOB text must be base64; other text reads as null.
pub fn decode(data_type: &str, lexical: &str) -> Option<String> {
    let name = type_name(data_type);
    match name.as_str() {
        "CHAR" | "SHORT" | "USHORT" | "INT" | "UINT" => Some(java_integer(lexical, 32)),
        "LONG" | "ULONG" => Some(java_integer(lexical, 64)),
        "FLOAT" | "DOUBLE" => Some(java_floating(lexical, 64)),
        "BIGDECIMAL" => java_big_decimal(lexical),
        "DATE" | "TIME" | "DATETIME" => java_date_lexical(lexical, &name),
        "BOOLEAN" => Some((if is_true(lexical) { "true" } else { "false" }).to_owned()),
        "BLOB" | "FILE" => {
            if lexical.is_empty() || STANDARD.decode(lexical).is_ok() {
                Some(lexical.to_owned())
            } else {
                None
            }
        }
        _ => Some(lexical.to_owned()),
    }
}

/// The text a BLOB parameter holds after a driver reads it from a text
/// format: the base64 of the bytes of the wire text itself.
pub fn text_blob(text: &str) -> String {
    if text.is_empty() {
        String::new()
    } else {
        STANDARD.encode(text.as_bytes())
    }
}

fn trim_java(value: &str) -> &str {
    value.trim_matches(|c: char| c <= ' ')
}

/// `StringTypeConverter.needTrim`: trims only when the value starts or ends
/// with a space, and then like `String.trim`.
fn trim_if_space(value: &str) -> &str {
    if value.starts_with(' ') || value.ends_with(' ') {
        trim_java(value)
    } else {
        value
    }
}

fn signed_digits(value: &str, digit: fn(u8) -> bool) -> bool {
    let body = value.strip_prefix(['+', '-']).unwrap_or(value);
    !body.is_empty() && body.bytes().all(digit)
}

/// `StringTypeConverter.toInt` (32 bits) or `toLong` (64 bits) as a decimal string.
pub fn java_integer(value: &str, bits: u32) -> String {
    if value.is_empty() {
        return "0".to_owned();
    }
    let value = trim_if_space(value).replace(',', "");
    if !value.contains('.') {
        if bits == 32 && (value.starts_with("0x") || value.starts_with("0X")) {
            let hex = &value[2..];
            if !signed_digits(hex, |b| b.is_ascii_hexdigit()) {
                return "0".to_owned();
            }
            return i32::from_str_radix(hex, 16)
                .map(|n| n.to_string())
                .unwrap_or_else(|_| "0".to_owned());
        }
        if !signed_digits(&value, |b| b.is_ascii_digit()) {
            return "0".to_owned();
        }
        return if bits == 32 {
            value.parse::<i32>().map(|n| n.to_string())
        } else {
            value.parse::<i64>().map(|n| n.to_string())
        }
        .unwrap_or_else(|_| "0".to_owned());
    }
    let Some(decimal) = JavaDecimal::parse(&value) else {
        return "0".to_owned();
    };
    // BigDecimal.intValue/longValue: truncate toward zero, keep the low bits.
    let digits = decimal.digits.as_bytes();
    let (integer, zeros): (&[u8], u64) = if decimal.scale > 0 {
        let keep = digits.len() as i64 - decimal.scale;
        if keep <= 0 {
            (&[][..], 0)
        } else {
            (&digits[..keep as usize], 0)
        }
    } else {
        (digits, decimal.scale.unsigned_abs())
    };
    let mut low: u64 = 0;
    for digit in integer {
        low = low.wrapping_mul(10).wrapping_add(u64::from(digit - b'0'));
    }
    // 10^64 is a multiple of 2^64, so more zeros leave no low bits.
    if zeros >= 64 {
        low = 0;
    } else {
        for _ in 0..zeros {
            low = low.wrapping_mul(10);
        }
    }
    if decimal.negative {
        low = low.wrapping_neg();
    }
    if bits == 32 {
        (low as u32 as i32).to_string()
    } else {
        (low as i64).to_string()
    }
}

/// The `BigDecimal(String)` grammar: an unscaled digit string and a scale.
struct JavaDecimal {
    negative: bool,
    /// Digits without the sign, possibly with leading zeros.
    digits: String,
    scale: i64,
}

impl JavaDecimal {
    fn parse(value: &str) -> Option<Self> {
        let (negative, body) = match value.as_bytes().first() {
            Some(b'-') => (true, &value[1..]),
            Some(b'+') => (false, &value[1..]),
            _ => (false, value),
        };
        let (mantissa, exponent) = match body.find(['e', 'E']) {
            Some(index) => {
                let exponent = &body[index + 1..];
                if !signed_digits(exponent, |b| b.is_ascii_digit()) {
                    return None;
                }
                let exponent = exponent.strip_prefix('+').unwrap_or(exponent);
                (&body[..index], exponent.parse::<i64>().ok()?)
            }
            None => (body, 0),
        };
        let (integer, fraction) = match mantissa.split_once('.') {
            Some((integer, fraction)) => (integer, fraction),
            None => (mantissa, ""),
        };
        if integer.is_empty() && fraction.is_empty()
            || !integer.bytes().all(|b| b.is_ascii_digit())
            || !fraction.bytes().all(|b| b.is_ascii_digit())
        {
            return None;
        }
        Some(Self {
            negative,
            digits: format!("{integer}{fraction}"),
            scale: (fraction.len() as i64).checked_sub(exponent)?,
        })
    }
}

/// `StringTypeConverter.toBigDecimal` followed by `BigDecimal.toString`;
/// `None` where the driver yields null.
pub fn java_big_decimal(value: &str) -> Option<String> {
    if value.is_empty() {
        return None;
    }
    let decimal = JavaDecimal::parse(&value.replace(',', ""))?;
    let digits = decimal.digits.trim_start_matches('0');
    let digits = if digits.is_empty() { "0" } else { digits };
    let sign = if decimal.negative && digits != "0" {
        "-"
    } else {
        ""
    };
    let scale = decimal.scale;
    let length = digits.len() as i64;
    let adjusted = -scale + length - 1;
    if scale == 0 {
        return Some(format!("{sign}{digits}"));
    }
    if scale > 0 && adjusted >= -6 {
        if length <= scale {
            let zeros = "0".repeat((scale - length) as usize);
            return Some(format!("{sign}0.{zeros}{digits}"));
        }
        let point = (length - scale) as usize;
        return Some(format!("{sign}{}.{}", &digits[..point], &digits[point..]));
    }
    let mut out = format!("{sign}{}", &digits[..1]);
    if digits.len() > 1 {
        out.push('.');
        out.push_str(&digits[1..]);
    }
    out.push('E');
    if adjusted >= 0 {
        out.push('+');
    }
    out.push_str(&adjusted.to_string());
    Some(out)
}

/// Java's decimal floating literal: digits with an optional point, an
/// optional exponent and an optional `f`/`d` suffix, or NaN/Infinity.
fn is_java_double(value: &str) -> bool {
    let body = value.strip_prefix(['+', '-']).unwrap_or(value);
    if body == "NaN" || body == "Infinity" {
        return true;
    }
    let body = body.strip_suffix(['f', 'F', 'd', 'D']).unwrap_or(body);
    let (mantissa, exponent) = match body.find(['e', 'E']) {
        Some(index) => (&body[..index], Some(&body[index + 1..])),
        None => (body, None),
    };
    if let Some(exponent) = exponent {
        if !signed_digits(exponent, |b| b.is_ascii_digit()) {
            return false;
        }
    }
    let (integer, fraction) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    !(integer.is_empty() && fraction.is_empty())
        && integer.bytes().all(|b| b.is_ascii_digit())
        && fraction.bytes().all(|b| b.is_ascii_digit())
}

/// Java's hexadecimal floating literal (`0x1.8p1`), as a value.
fn java_hex_double(value: &str) -> Option<f64> {
    let (negative, body) = match value.as_bytes().first() {
        Some(b'-') => (true, &value[1..]),
        Some(b'+') => (false, &value[1..]),
        _ => (false, value),
    };
    let body = body
        .strip_prefix("0x")
        .or_else(|| body.strip_prefix("0X"))?;
    let body = body.strip_suffix(['f', 'F', 'd', 'D']).unwrap_or(body);
    let index = body.find(['p', 'P'])?;
    let (mantissa, exponent) = (&body[..index], &body[index + 1..]);
    if !signed_digits(exponent, |b| b.is_ascii_digit()) {
        return None;
    }
    let (integer, fraction) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    if integer.is_empty() && fraction.is_empty()
        || !integer.bytes().all(|b| b.is_ascii_hexdigit())
        || !fraction.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return None;
    }
    let mut x = 0f64;
    for digit in integer.chars().chain(fraction.chars()) {
        x = x * 16.0 + f64::from(digit.to_digit(16)?);
    }
    let exponent = exponent
        .strip_prefix('+')
        .unwrap_or(exponent)
        .parse::<i64>()
        .unwrap_or(i64::MAX)
        .saturating_sub(4 * fraction.len() as i64)
        .clamp(-2200, 2200) as i32;
    let x = x * 2f64.powi(exponent);
    Some(if negative { -x } else { x })
}

/// `StringTypeConverter.toDouble` (64 bits) or `toFloat` (32 bits) followed by
/// `Double.toString` or `Float.toString`.
pub fn java_floating(value: &str, bits: u32) -> String {
    let value = value.replace(',', "");
    let value = trim_java(&value);
    let x = if is_java_double(value) {
        let text = value.trim_end_matches(['f', 'F', 'd', 'D']);
        if text.ends_with("Infinity") {
            if text.starts_with('-') {
                f64::NEG_INFINITY
            } else {
                f64::INFINITY
            }
        } else if text.ends_with("NaN") {
            f64::NAN
        } else {
            let parsed = if bits == 32 {
                text.parse::<f32>().map(f64::from)
            } else {
                text.parse::<f64>()
            };
            parsed.unwrap_or(0.0)
        }
    } else if let Some(x) = java_hex_double(value) {
        if bits == 32 {
            f64::from(x as f32)
        } else {
            x
        }
    } else {
        0.0
    };
    java_float_string(x, bits)
}

/// `Double.toString` (64 bits) or `Float.toString` (32 bits) as JDK 19 and
/// later print them: the shortest digits that read back as the same value.
pub fn java_float_string(x: f64, bits: u32) -> String {
    if x.is_nan() {
        return "NaN".to_owned();
    }
    if x.is_infinite() {
        return (if x > 0.0 { "Infinity" } else { "-Infinity" }).to_owned();
    }
    if x == 0.0 {
        return (if x.is_sign_negative() { "-0.0" } else { "0.0" }).to_owned();
    }
    let sign = if x < 0.0 { "-" } else { "" };
    let x = x.abs();
    let scientific = if bits == 32 {
        format!("{:e}", x as f32)
    } else {
        format!("{x:e}")
    };
    let (mantissa, exponent) = scientific.split_once('e').unwrap_or((&scientific, "0"));
    let exponent: i32 = exponent.parse().unwrap_or(0);
    let digits = mantissa.replacen('.', "", 1);
    if (1e-3..1e7).contains(&x) {
        if exponent >= 0 {
            let whole = exponent as usize + 1;
            if digits.len() <= whole {
                return format!("{sign}{digits}{}.0", "0".repeat(whole - digits.len()));
            }
            return format!("{sign}{}.{}", &digits[..whole], &digits[whole..]);
        }
        return format!("{sign}0.{}{digits}", "0".repeat((-exponent - 1) as usize));
    }
    let rest = if digits.len() > 1 { &digits[1..] } else { "0" };
    format!("{sign}{}.{rest}E{exponent}", &digits[..1])
}

// ------------------------------------------------------------------- dates

const DAY_MILLIS: i64 = 86_400_000;

fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let yoe = year - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let days = days + 719_468;
    let era = days.div_euclid(146_097);
    let doe = days - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(month <= 2), month, day)
}

/// A lenient calendar date as epoch milliseconds (UTC): out-of-range fields
/// carry into the next larger field.
fn lenient(fields: [i64; 7]) -> i64 {
    let [year, month, day, hour, minute, second, millis] = fields;
    let month = month - 1;
    let days = days_from_civil(year + month.div_euclid(12), month.rem_euclid(12) + 1, 1) + day - 1;
    days * DAY_MILLIS + ((hour * 60 + minute) * 60 + second) * 1000 + millis
}

/// `StringTypeConverter.toDate`/`toTime`/`toDateTime` as epoch milliseconds
/// (UTC): the length picks the layout (17 `yyyyMMddHHmmssSSS`, 14
/// `yyyyMMddHHmmss`, 8 `yyyyMMdd`, 9 `HHmmssSSS`, 6 `HHmmss`), each field is
/// read as `(char - '0')` arithmetic and overflow is normalized leniently.
/// DATE drops the time and TIME moves to 1970-01-01; a time-only value read
/// as DATE or DATETIME is put on 1970-01-01. `None` for any other length.
pub fn java_date(value: &str, data_type: &str) -> Option<i64> {
    let units: Vec<u16> = value.encode_utf16().collect();
    let field = |offset: usize, length: usize| {
        units[offset..offset + length]
            .iter()
            .fold(0i64, |n, unit| n * 10 + i64::from(*unit) - i64::from(b'0'))
    };
    let fields = match units.len() {
        17 => [
            field(0, 4),
            field(4, 2),
            field(6, 2),
            field(8, 2),
            field(10, 2),
            field(12, 2),
            field(14, 3),
        ],
        14 => [
            field(0, 4),
            field(4, 2),
            field(6, 2),
            field(8, 2),
            field(10, 2),
            field(12, 2),
            0,
        ],
        8 => [field(0, 4), field(4, 2), field(6, 2), 0, 0, 0, 0],
        9 => [
            1970,
            1,
            1,
            field(0, 2),
            field(2, 2),
            field(4, 2),
            field(6, 3),
        ],
        6 => [1970, 1, 1, field(0, 2), field(2, 2), field(4, 2), 0],
        _ => return None,
    };
    let millis = lenient(fields);
    Some(match type_name(data_type).as_str() {
        "DATE" => millis.div_euclid(DAY_MILLIS) * DAY_MILLIS,
        "TIME" => millis.rem_euclid(DAY_MILLIS),
        _ => millis,
    })
}

fn date_parts(millis: i64) -> (i64, i64, i64, i64, i64, i64, i64) {
    let (year, month, day) = civil_from_days(millis.div_euclid(DAY_MILLIS));
    let clock = millis.rem_euclid(DAY_MILLIS);
    (
        year,
        month,
        day,
        clock / 3_600_000,
        clock / 60_000 % 60,
        clock / 1000 % 60,
        clock % 1000,
    )
}

/// Formats epoch milliseconds like the drivers' `DatetimeFormat` for the
/// type; the year is padded to four digits and never cut.
pub fn format_date(millis: i64, data_type: &str) -> String {
    let (year, month, day, hour, minute, second, ms) = date_parts(millis);
    match type_name(data_type).as_str() {
        "DATE" => format!("{year:04}{month:02}{day:02}"),
        "TIME" => format!("{hour:02}{minute:02}{second:02}{ms:03}"),
        _ => format!("{year:04}{month:02}{day:02}{hour:02}{minute:02}{second:02}{ms:03}"),
    }
}

/// The driver lexical of a date value, or `None` where the driver yields null.
pub fn java_date_lexical(value: &str, data_type: &str) -> Option<String> {
    java_date(value, data_type).map(|millis| format_date(millis, data_type))
}

/// Epoch milliseconds (UTC) of a date lexical as PlatformBinary carries it,
/// or `None` when the lexical is not a date. A year longer than four digits
/// is accepted in the normalized form.
pub fn epoch_millis(lexical: &str, data_type: &str) -> Option<i64> {
    long_year_date(lexical, data_type).or_else(|| java_date(lexical, data_type))
}

fn long_year_date(value: &str, data_type: &str) -> Option<i64> {
    let tail = match type_name(data_type).as_str() {
        "TIME" => return None,
        "DATE" => 4,
        _ => 13,
    };
    if value.len() <= tail + 4 || !value.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let year = i64::from(value[..value.len() - tail].parse::<i32>().ok()?);
    let rest = &value[value.len() - tail..];
    let n = |from: usize, to: usize| rest[from..to].parse::<i64>().unwrap_or(0);
    Some(if tail == 4 {
        lenient([year, n(0, 2), n(2, 4), 0, 0, 0, 0])
    } else {
        lenient([
            year,
            n(0, 2),
            n(2, 4),
            n(4, 6),
            n(6, 8),
            n(8, 10),
            n(10, 13),
        ])
    })
}

/// The lexical of a PlatformBinary date (epoch milliseconds, UTC) read as the type.
pub fn format_epoch(millis: f64, data_type: &str) -> String {
    let (year, month, day, hour, minute, second, ms) = date_parts(millis as i64);
    let year = if year < 0 {
        format!("-{:04}", -year)
    } else {
        format!("{year:04}")
    };
    let date = format!("{year}{month:02}{day:02}");
    let clock = format!("{hour:02}{minute:02}{second:02}{ms:03}");
    match type_name(data_type).as_str() {
        "DATE" => date,
        "TIME" => clock,
        _ => date + &clock,
    }
}

// -------------------------------------------------------------------- rows

/// A value as the writers see it: missing, null, or a lexical (`""` is empty).
#[derive(Debug, Clone, PartialEq, Eq)]
enum Cell {
    Missing,
    Null,
    Value(String),
}

impl Cell {
    fn of(value: &RawValue) -> Self {
        match value {
            RawValue::Missing => Self::Missing,
            RawValue::Null => Self::Null,
            RawValue::Text(text) => Self::Value(text.clone()),
            RawValue::Bytes(bytes) => Self::Value(STANDARD.encode(bytes)),
        }
    }

    /// Converted to the type as the drivers convert a written value.
    fn encoded(self, data_type: &str) -> Self {
        match self {
            Self::Value(text) => encode(data_type, &text).map_or(Self::Null, Self::Value),
            other => other,
        }
    }

    fn lexical(&self) -> Option<&str> {
        match self {
            Self::Value(text) => Some(text),
            _ => None,
        }
    }
}

struct Row {
    row_type: RowType,
    values: Vec<(String, RawValue)>,
    original: Option<Vec<(String, RawValue)>>,
}

impl Row {
    fn get(&self, id: &str) -> Cell {
        Cell::of(crate::raw_get(&self.values, id))
    }
}

/// The rows of a dataset in the order and shape the drivers write them.
fn driver_rows(dataset: &RawDataset) -> Vec<Row> {
    let mut live = Vec::new();
    let mut removed = Vec::new();
    for row in &dataset.rows {
        match row.row_type {
            RowType::Original => continue,
            RowType::Delete => removed.push(Row {
                row_type: RowType::Delete,
                values: row.values.clone(),
                original: None,
            }),
            RowType::Update => {
                let original = row.original.clone().unwrap_or_default();
                let mut values = original.clone();
                for (id, value) in &row.values {
                    match values.iter_mut().find(|(column, _)| column == id) {
                        Some(cell) => cell.1 = value.clone(),
                        None => values.push((id.clone(), value.clone())),
                    }
                }
                live.push(Row {
                    row_type: RowType::Update,
                    values,
                    original: Some(original),
                });
            }
            row_type => live.push(Row {
                row_type,
                values: row.values.clone(),
                original: None,
            }),
        }
    }
    live.extend(removed);
    live
}

// --------------------------------------------------------------- document

fn parameter_type(document: &RawDocument, index: usize) -> String {
    type_name(
        document
            .parameter_wires
            .get(index)
            .and_then(Option::as_deref)
            .unwrap_or("STRING"),
    )
}

fn column_size(sizes: &[Option<String>], index: usize, fallback: u32) -> String {
    sizes
        .get(index)
        .and_then(Option::as_deref)
        .filter(|size| !size.is_empty())
        .map_or_else(|| fallback.to_string(), str::to_owned)
}

fn encode_error(message: impl Into<String>) -> Error {
    Error::from(EncodeError::Message(message.into()))
}

/// The text of a parameter or cell in XML and JSON, or `None` when the
/// drivers write no value. An unset BOOLEAN is written as 0, an empty number
/// as its zero.
fn scalar_text(data_type: &str, cell: &Cell) -> Option<String> {
    let name = type_name(data_type);
    match cell {
        Cell::Value(text) if !text.is_empty() => Some(text.clone()),
        Cell::Value(_) => match name.as_str() {
            "STRING" | "CHAR" | "FILE" | "BLOB" => Some(String::new()),
            "BOOLEAN" => Some("0".to_owned()),
            _ => None,
        },
        _ if name == "BOOLEAN" => Some("0".to_owned()),
        _ => None,
    }
}

/// The value of a constant column in the text formats: as given, except that
/// a BOOLEAN constant is always 1 or 0 (null and empty as 0).
fn const_text(data_type: &str, cell: &Cell) -> Option<String> {
    if type_name(data_type) == "BOOLEAN" && *cell != Cell::Missing {
        let truth = cell.lexical().is_some_and(is_true);
        return Some((if truth { "1" } else { "0" }).to_owned());
    }
    cell.lexical().map(str::to_owned)
}

// --------------------------------------------------------------------- XML

/// Writes the XML the drivers write: XPLATFORM (`xplatform`) or Nexacro.
///
/// XPLATFORM writes a `ver` attribute, constant columns after the columns,
/// and every character below U+0020 and the space as character references.
/// Nexacro writes constant columns first and only the predefined entities;
/// it rejects the control characters other than TAB, LF and CR, which XML
/// 1.0 cannot hold.
pub fn write_xml<W: Write>(mut output: W, document: &RawDocument, xplatform: bool) -> Result<()> {
    let mut out = String::new();
    out.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    if xplatform {
        out.push_str("<Root xmlns=\"http://www.tobesoft.com/platform/dataset\" ver=\"5000\">\n");
    } else {
        out.push_str("<Root xmlns=\"http://www.nexacroplatform.com/platform/dataset\">\n");
    }
    if document.parameters.is_empty() {
        out.push_str("\t<Parameters/>\n");
    } else {
        out.push_str("\t<Parameters>\n");
        for (index, (id, value)) in document.parameters.iter().enumerate() {
            let data_type = parameter_type(document, index);
            let text = scalar_text(&data_type, &Cell::of(value).encoded(&data_type));
            out.push_str("\t\t<Parameter id=\"");
            out.push_str(&escape_xml(id, xplatform)?);
            out.push_str("\" type=\"");
            out.push_str(&escape_xml(&wire_type(&data_type), xplatform)?);
            out.push('"');
            let Some(text) = text else {
                out.push_str("/>\n");
                continue;
            };
            if is_blob(&data_type) {
                out.push_str(" encrypt=\"base64\"");
            }
            out.push('>');
            out.push_str(&escape_xml(&text, xplatform)?);
            out.push_str("</Parameter>\n");
        }
        out.push_str("\t</Parameters>\n");
    }
    for dataset in &document.datasets {
        write_xml_dataset(&mut out, dataset, xplatform)?;
    }
    out.push_str("</Root>\n");
    output.write_all(out.as_bytes()).map_err(Error::from)
}

fn write_xml_dataset(out: &mut String, dataset: &RawDataset, xplatform: bool) -> Result<()> {
    out.push_str("\t<Dataset id=\"");
    out.push_str(&escape_xml(&dataset.id, xplatform)?);
    out.push_str("\">\n\t\t<ColumnInfo>\n");
    if !xplatform {
        write_xml_constants(out, dataset, xplatform)?;
    }
    for (index, (id, wire)) in dataset.columns.iter().enumerate() {
        let data_type = wire.to_string();
        out.push_str("\t\t\t<Column id=\"");
        out.push_str(&escape_xml(id, xplatform)?);
        out.push_str("\" type=\"");
        out.push_str(&escape_xml(&wire_type(&data_type), xplatform)?);
        out.push_str("\" size=\"");
        let size = column_size(&dataset.column_sizes, index, default_size(&data_type));
        out.push_str(&escape_xml(&size, xplatform)?);
        out.push('"');
        if is_blob(&data_type) {
            out.push_str(" encrypt=\"base64\"");
        }
        out.push_str("/>\n");
    }
    if xplatform {
        write_xml_constants(out, dataset, xplatform)?;
    }
    out.push_str("\t\t</ColumnInfo>\n\t\t<Rows>\n");
    for row in driver_rows(dataset) {
        out.push_str("\t\t\t<Row");
        match row.row_type {
            RowType::Insert => out.push_str(" type=\"insert\""),
            RowType::Update => out.push_str(" type=\"update\""),
            RowType::Delete => out.push_str(" type=\"delete\""),
            _ => {}
        }
        out.push_str(">\n");
        write_xml_cells(out, dataset, &row.values, "\t\t\t\t", xplatform, false)?;
        if let Some(original) = &row.original {
            out.push_str("\t\t\t\t<OrgRow>\n");
            write_xml_cells(out, dataset, original, "\t\t\t\t\t", xplatform, true)?;
            out.push_str("\t\t\t\t</OrgRow>\n");
        }
        out.push_str("\t\t\t</Row>\n");
    }
    out.push_str("\t\t</Rows>\n\t</Dataset>\n");
    Ok(())
}

fn write_xml_constants(out: &mut String, dataset: &RawDataset, xplatform: bool) -> Result<()> {
    for (index, (id, wire, value)) in dataset.const_columns.iter().enumerate() {
        let data_type = wire.to_string();
        out.push_str("\t\t\t<ConstColumn id=\"");
        out.push_str(&escape_xml(id, xplatform)?);
        out.push_str("\" type=\"");
        out.push_str(&escape_xml(&wire_type(&data_type), xplatform)?);
        out.push_str("\" size=\"");
        let size = column_size(&dataset.const_column_sizes, index, default_size(&data_type));
        out.push_str(&escape_xml(&size, xplatform)?);
        out.push('"');
        let Some(text) = const_text(&data_type, &Cell::of(value)) else {
            out.push_str("/>\n");
            continue;
        };
        let blob = is_blob(&data_type);
        if blob {
            out.push_str(" encrypt=\"base64\"");
        }
        out.push_str(" value=\"");
        out.push_str(&escape_xml(&text, xplatform)?);
        out.push('"');
        out.push_str(if blob { "></ConstColumn>\n" } else { "/>\n" });
    }
    Ok(())
}

fn write_xml_cells(
    out: &mut String,
    dataset: &RawDataset,
    values: &[(String, RawValue)],
    indent: &str,
    xplatform: bool,
    original: bool,
) -> Result<()> {
    for (id, wire) in &dataset.columns {
        let data_type = wire.to_string();
        let cell = Cell::of(crate::raw_get(values, id)).encoded(&data_type);
        let text = match cell {
            Cell::Value(text) => text,
            // The drivers hold an unset BOOLEAN as false; an original row the
            // driver never filled has no cells.
            _ if original || data_type != "BOOLEAN" => continue,
            _ => "0".to_owned(),
        };
        out.push_str(indent);
        out.push_str("<Col id=\"");
        out.push_str(&escape_xml(id, xplatform)?);
        out.push_str("\">");
        out.push_str(&escape_xml(&text, xplatform)?);
        out.push_str("</Col>\n");
    }
    Ok(())
}

fn escape_xml(value: &str, xplatform: bool) -> Result<String> {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            ' ' if xplatform => out.push_str("&#32;"),
            c if c < ' ' && xplatform => out.push_str(&format!("&#{};", c as u32)),
            c if c < ' ' && !matches!(c, '\t' | '\n' | '\r') => {
                return Err(encode_error(format!(
                    "character U+{:04X} is not valid XML 1.0",
                    c as u32
                )))
            }
            c => out.push(c),
        }
    }
    Ok(out)
}

// -------------------------------------------------------------------- JSON

/// A JSON value the JSON writer builds: a string, an array, or an object
/// whose keys come out in `java.util.HashMap` order.
enum Json {
    Text(String),
    Array(Vec<Json>),
    Object(Vec<(&'static str, Json)>),
    Row(Vec<(String, Json)>),
}

/// The bucket `java.util.HashMap` puts a key in.
fn java_bucket(key: &str, capacity: usize) -> usize {
    let mut hash: u32 = 0;
    for unit in key.encode_utf16() {
        hash = hash.wrapping_mul(31).wrapping_add(u32::from(unit));
    }
    hash ^= hash >> 16;
    hash as usize & (capacity - 1)
}

fn java_order<K: AsRef<str>, V>(mut fields: Vec<(K, V)>) -> Vec<(K, V)> {
    let mut capacity = 16;
    while fields.len() > capacity * 3 / 4 {
        capacity *= 2;
    }
    fields.sort_by_key(|(key, _)| java_bucket(key.as_ref(), capacity));
    fields
}

fn write_json_value(out: &mut String, value: &Json) {
    match value {
        Json::Text(text) => write_json_string(out, text),
        Json::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_json_value(out, item);
            }
            out.push(']');
        }
        Json::Object(fields) => {
            out.push('{');
            for (index, (key, item)) in fields.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_json_string(out, key);
                out.push(':');
                write_json_value(out, item);
            }
            out.push('}');
        }
        Json::Row(fields) => {
            out.push('{');
            for (index, (key, item)) in fields.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_json_string(out, key);
                out.push(':');
                write_json_value(out, item);
            }
            out.push('}');
        }
    }
}

/// Escapes like json-simple's `JSONValue.escape`: `/` is escaped,
/// U+0000-U+001F, U+007F-U+009F and U+2000-U+20FF become an upper-case
/// `\uXXXX`, nothing else is.
fn write_json_string(out: &mut String, value: &str) {
    out.push('"');
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '/' => out.push_str("\\/"),
            c if c <= '\u{1f}'
                || ('\u{7f}'..='\u{9f}').contains(&c)
                || ('\u{2000}'..='\u{20ff}').contains(&c) =>
            {
                out.push_str(&format!("\\u{:04X}", c as u32))
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Writes the Nexacro JSON the driver writes (json-simple): keys in hash
/// order, every value a string.
pub fn write_json<W: Write>(mut output: W, document: &RawDocument) -> Result<()> {
    let mut root = vec![("version", Json::Text("1.0".to_owned()))];
    let parameters = document
        .parameters
        .iter()
        .enumerate()
        .map(|(index, (id, value))| {
            let data_type = parameter_type(document, index);
            let mut fields = vec![
                ("id", Json::Text(id.clone())),
                ("type", Json::Text(wire_type(&data_type))),
            ];
            if let Some(text) = scalar_text(&data_type, &Cell::of(value).encoded(&data_type)) {
                fields.push(("value", Json::Text(text)));
            }
            Json::Object(java_order(fields))
        })
        .collect::<Vec<_>>();
    if !parameters.is_empty() {
        root.push(("Parameters", Json::Array(parameters)));
    }
    let datasets = document
        .datasets
        .iter()
        .map(json_dataset)
        .collect::<Vec<_>>();
    if !datasets.is_empty() {
        root.push(("Datasets", Json::Array(datasets)));
    }
    let mut out = String::new();
    write_json_value(&mut out, &Json::Object(java_order(root)));
    output.write_all(out.as_bytes()).map_err(Error::from)
}

fn json_dataset(dataset: &RawDataset) -> Json {
    let columns = dataset
        .columns
        .iter()
        .enumerate()
        .map(|(index, (id, wire))| {
            let data_type = wire.to_string();
            Json::Object(java_order(vec![
                ("id", Json::Text(id.clone())),
                ("type", Json::Text(wire_type(&data_type))),
                (
                    "size",
                    Json::Text(column_size(
                        &dataset.column_sizes,
                        index,
                        default_size(&data_type),
                    )),
                ),
            ]))
        })
        .collect::<Vec<_>>();
    let constants = dataset
        .const_columns
        .iter()
        .enumerate()
        .map(|(index, (id, wire, value))| {
            let data_type = wire.to_string();
            let mut fields = vec![
                ("id", Json::Text(id.clone())),
                ("type", Json::Text(wire_type(&data_type))),
                (
                    "size",
                    Json::Text(column_size(
                        &dataset.const_column_sizes,
                        index,
                        default_size(&data_type),
                    )),
                ),
            ];
            if let Some(text) = const_text(&data_type, &Cell::of(value)) {
                fields.push(("value", Json::Text(text)));
            }
            Json::Object(java_order(fields))
        })
        .collect::<Vec<_>>();
    let json_row = |code: &str, values: &[(String, RawValue)], original: bool| {
        let mut fields = vec![("_RowType_".to_owned(), Json::Text(code.to_owned()))];
        for (id, wire) in &dataset.columns {
            let data_type = wire.to_string();
            let raw = crate::raw_get(values, id);
            // An original row writes only the cells it has.
            if original && matches!(raw, RawValue::Missing) {
                continue;
            }
            if let Some(text) = scalar_text(&data_type, &Cell::of(raw).encoded(&data_type)) {
                fields.push((id.clone(), Json::Text(text)));
            }
        }
        Json::Row(java_order(fields))
    };
    let mut rows = Vec::new();
    for row in driver_rows(dataset) {
        rows.push(json_row(row_code(row.row_type), &row.values, false));
        if let Some(original) = &row.original {
            rows.push(json_row("O", original, true));
        }
    }
    let mut fields = vec![("id", Json::Text(dataset.id.clone()))];
    if !constants.is_empty() || !columns.is_empty() {
        let mut info = Vec::new();
        if !constants.is_empty() {
            info.push(("ConstColumn", Json::Array(constants)));
        }
        if !columns.is_empty() {
            info.push(("Column", Json::Array(columns)));
        }
        fields.push(("ColumnInfo", Json::Object(java_order(info))));
    }
    if !rows.is_empty() {
        fields.push(("Rows", Json::Array(rows)));
    }
    Json::Object(java_order(fields))
}

fn row_code(row_type: RowType) -> &'static str {
    match row_type {
        RowType::Insert => "I",
        RowType::Update => "U",
        RowType::Delete => "D",
        RowType::Original => "O",
        RowType::Normal => "N",
    }
}

// --------------------------------------------------------------------- SSV

const UNIT: char = '\u{1f}';
const RECORD: char = '\u{1e}';

/// Writes the SSV the drivers write: XPLATFORM (`xplatform`) or Nexacro.
///
/// Nexacro marks null with U+0003 and ends every dataset and the document
/// with an empty record; XPLATFORM writes null as an empty field and puts an
/// empty record only between datasets.
pub fn write_ssv<W: Write>(mut output: W, document: &RawDocument, xplatform: bool) -> Result<()> {
    let mut out = String::new();
    let record = |out: &mut String, text: &str| {
        out.push_str(text);
        out.push(RECORD);
    };
    record(&mut out, "SSV:UTF-8");
    for (index, (id, value)) in document.parameters.iter().enumerate() {
        check_ssv(id)?;
        let data_type = parameter_type(document, index);
        let mut text = format!("{id}:{}", wire_type(&data_type));
        let mut cell = Cell::of(value);
        if cell != Cell::Missing {
            if cell == Cell::Null && is_blob(&data_type) {
                // The drivers write a null BLOB variable as an empty value.
                cell = Cell::Value(String::new());
            }
            text.push('=');
            text.push_str(&ssv_typed_cell(
                &data_type,
                cell.encoded(&data_type),
                xplatform,
            )?);
        }
        record(&mut out, &text);
    }
    for (index, dataset) in document.datasets.iter().enumerate() {
        check_ssv(&dataset.id)?;
        record(&mut out, &format!("Dataset:{}", dataset.id));
        if !dataset.const_columns.is_empty() {
            let mut fields = vec!["_Const_".to_owned()];
            for (position, (id, wire, value)) in dataset.const_columns.iter().enumerate() {
                check_ssv(id)?;
                let data_type = wire.to_string();
                let size = column_size(
                    &dataset.const_column_sizes,
                    position,
                    ssv_default_size(&data_type),
                );
                let mut field = format!("{id}:{}({size})", wire_type(&data_type));
                let cell = Cell::of(value);
                if cell != Cell::Missing {
                    // A constant has no null marker: null is an empty value.
                    let text = const_text(&data_type, &cell).unwrap_or_default();
                    check_ssv(&text)?;
                    field.push('=');
                    field.push_str(&text);
                }
                fields.push(field);
            }
            record(&mut out, &fields.join(&UNIT.to_string()));
        }
        if dataset.columns.is_empty() {
            return Err(encode_error(format!(
                "SSV dataset {:?} has no columns",
                dataset.id
            )));
        }
        let mut fields = vec!["_RowType_".to_owned()];
        for (position, (id, wire)) in dataset.columns.iter().enumerate() {
            check_ssv(id)?;
            let data_type = wire.to_string();
            let size = column_size(
                &dataset.column_sizes,
                position,
                ssv_default_size(&data_type),
            );
            fields.push(format!("{id}:{}({size})", wire_type(&data_type)));
        }
        record(&mut out, &fields.join(&UNIT.to_string()));
        for row in driver_rows(dataset) {
            record(
                &mut out,
                &ssv_row(
                    dataset,
                    row_code(row.row_type),
                    &row.values,
                    xplatform,
                    false,
                )?,
            );
            if let Some(original) = &row.original {
                record(&mut out, &ssv_row(dataset, "O", original, xplatform, true)?);
            }
        }
        if !xplatform || index + 1 < document.datasets.len() {
            record(&mut out, "");
        }
    }
    if !xplatform && document.datasets.is_empty() {
        record(&mut out, "");
    }
    output.write_all(out.as_bytes()).map_err(Error::from)
}

fn ssv_row(
    dataset: &RawDataset,
    code: &str,
    values: &[(String, RawValue)],
    xplatform: bool,
    original: bool,
) -> Result<String> {
    let mut fields = vec![code.to_owned()];
    for (id, wire) in &dataset.columns {
        let data_type = wire.to_string();
        let cell = Cell::of(crate::raw_get(values, id)).encoded(&data_type);
        // An original row keeps no BOOLEAN default.
        let typed = if original {
            "STRING"
        } else {
            data_type.as_str()
        };
        fields.push(ssv_typed_cell(typed, cell, xplatform)?);
    }
    Ok(fields.join(&UNIT.to_string()))
}

/// An SSV field; an unset BOOLEAN is written as 0, as the drivers hold it as false.
fn ssv_typed_cell(data_type: &str, cell: Cell, xplatform: bool) -> Result<String> {
    match cell {
        Cell::Value(text) => {
            check_ssv(&text)?;
            Ok(text)
        }
        _ if type_name(data_type) == "BOOLEAN" => Ok("0".to_owned()),
        _ if xplatform => Ok(String::new()),
        _ => Ok("\u{3}".to_owned()),
    }
}

fn check_ssv(value: &str) -> Result<()> {
    if value.contains([UNIT, RECORD]) {
        return Err(encode_error("value contains an SSV separator"));
    }
    Ok(())
}

// ------------------------------------------------------------- PlatformBinary

const VARIABLE_MARK: u16 = 0xfe10;
const DATASET_MARK: u16 = 0xfe01;
const BINARY_VERSION: u16 = 5000;
const MAX_LENGTH: usize = 0x7fff_ffff;

struct BinaryOut(Vec<u8>);

impl BinaryOut {
    fn u16(&mut self, value: u16) {
        self.0.extend_from_slice(&value.to_be_bytes());
    }

    fn u32(&mut self, value: u32) {
        self.0.extend_from_slice(&value.to_be_bytes());
    }

    fn f64(&mut self, value: f64) {
        // Double.doubleToLongBits writes one canonical NaN.
        let bits = if value.is_nan() {
            0x7ff8_0000_0000_0000
        } else {
            value.to_bits()
        };
        self.0.extend_from_slice(&bits.to_be_bytes());
    }

    fn length(&mut self, length: usize) -> Result<()> {
        if length > MAX_LENGTH {
            return Err(encode_error(
                "PlatformBinary value exceeds the length limit",
            ));
        }
        if length < 0x8000 {
            self.u16(length as u16);
        } else {
            self.u32(length as u32 | 0x8000_0000);
        }
        Ok(())
    }

    fn bytes(&mut self, bytes: &[u8]) -> Result<()> {
        self.length(bytes.len())?;
        self.0.extend_from_slice(bytes);
        Ok(())
    }

    fn short_string(&mut self, text: &str) -> Result<()> {
        if text.len() > 0x7fff {
            return Err(encode_error(
                "PlatformBinary string exceeds the short length",
            ));
        }
        self.u16(text.len() as u16);
        self.0.extend_from_slice(text.as_bytes());
        Ok(())
    }

    fn block(&mut self, block: BinaryOut) -> Result<()> {
        self.length(block.0.len())?;
        self.0.extend(block.0);
        Ok(())
    }
}

/// Writes PlatformBinary 5000 as the drivers write it (both profiles share
/// the layout). Dates go on the wire as epoch milliseconds in UTC.
pub fn write_binary<W: Write>(mut output: W, document: &RawDocument) -> Result<()> {
    let mut out = BinaryOut(Vec::new());
    // The drivers write the variable block only when there are variables.
    if !document.parameters.is_empty() {
        let mut block = BinaryOut(Vec::new());
        block.u16(
            u16::try_from(document.parameters.len())
                .map_err(|_| encode_error("too many PlatformBinary variables"))?,
        );
        for (index, (id, value)) in document.parameters.iter().enumerate() {
            let data_type = parameter_type(document, index);
            block.short_string(id)?;
            let cell = primitive_null_as_zero(&data_type, Cell::of(value)).encoded(&data_type);
            binary_value(&mut block, &data_type, &cell)?;
        }
        out.u16(VARIABLE_MARK);
        out.u16(BINARY_VERSION);
        out.block(block)?;
    }
    for dataset in &document.datasets {
        write_binary_dataset(&mut out, dataset)?;
    }
    output.write_all(&out.0).map_err(Error::from)
}

/// A null of a primitive Java type is written as that type's zero.
fn primitive_null_as_zero(data_type: &str, cell: Cell) -> Cell {
    match (&cell, type_name(data_type).as_str()) {
        (
            Cell::Null | Cell::Missing,
            "SHORT" | "USHORT" | "INT" | "UINT" | "LONG" | "ULONG" | "BOOLEAN" | "FLOAT" | "DOUBLE",
        ) => Cell::Value("0".to_owned()),
        _ => cell,
    }
}

fn write_binary_dataset(out: &mut BinaryOut, dataset: &RawDataset) -> Result<()> {
    let mut header = BinaryOut(Vec::new());
    header.short_string(&dataset.id)?;
    let mut constants = BinaryOut(Vec::new());
    constants.u16(
        u16::try_from(dataset.const_columns.len())
            .map_err(|_| encode_error("too many PlatformBinary constants"))?,
    );
    for (id, wire, value) in &dataset.const_columns {
        let data_type = wire.to_string();
        constants.short_string(id)?;
        // PlatformBinary writes a constant converted to the column type; an
        // empty value converts like any other lexical.
        let cell = primitive_null_as_zero(&data_type, Cell::of(value));
        let cell = if is_text(&data_type) {
            cell
        } else {
            match cell {
                Cell::Value(text) if text.is_empty() => {
                    encode_value(&type_name(&data_type), "").map_or(Cell::Null, Cell::Value)
                }
                other => other.encoded(&data_type),
            }
        };
        binary_value(&mut constants, &data_type, &cell)?;
    }
    header.u16(VARIABLE_MARK);
    header.u16(BINARY_VERSION);
    header.block(constants)?;
    header.u16(
        u16::try_from(dataset.columns.len())
            .map_err(|_| encode_error("too many PlatformBinary columns"))?,
    );
    for (index, (id, wire)) in dataset.columns.iter().enumerate() {
        let data_type = wire.to_string();
        header.short_string(id)?;
        header.u16(binary_column_code(&data_type));
        let size = column_size(&dataset.column_sizes, index, default_size(&data_type));
        header.u16(size.parse::<u16>().unwrap_or(0));
        let (property, sum_text) = dataset
            .column_meta
            .get(index)
            .map(|(property, sum_text)| (property.as_deref(), sum_text.as_deref()))
            .unwrap_or((None, None));
        if property.is_some_and(|property| property.eq_ignore_ascii_case("SUM")) {
            header.u16(0x6000);
            header.short_string(sum_text.unwrap_or_default())?;
        } else {
            header.u16(1);
        }
    }
    out.u16(DATASET_MARK);
    out.u16(BINARY_VERSION);
    out.block(header)?;
    for row in driver_rows(dataset) {
        let mut body = BinaryOut(Vec::new());
        body.u16(match row.row_type {
            RowType::Insert => 2,
            RowType::Update => 4,
            RowType::Delete => 8,
            _ => 1,
        });
        binary_cells(&mut body, dataset, &row)?;
        if let Some(original) = &row.original {
            let original = Row {
                row_type: RowType::Original,
                values: original.clone(),
                original: None,
            };
            binary_cells(&mut body, dataset, &original)?;
        }
        out.block(body)?;
    }
    out.u32(0);
    Ok(())
}

fn binary_cells(out: &mut BinaryOut, dataset: &RawDataset, row: &Row) -> Result<()> {
    out.u16(
        u16::try_from(dataset.columns.len())
            .map_err(|_| encode_error("too many PlatformBinary columns"))?,
    );
    for (id, wire) in &dataset.columns {
        let data_type = wire.to_string();
        binary_value(out, &data_type, &row.get(id).encoded(&data_type))?;
    }
    Ok(())
}

fn binary_column_code(data_type: &str) -> u16 {
    match type_name(data_type).as_str() {
        "STRING" | "CHAR" => 1,
        "SHORT" | "USHORT" | "INT" | "UINT" | "BOOLEAN" => 2,
        "FLOAT" | "DOUBLE" => 3,
        "LONG" | "ULONG" | "DECIMAL" | "BIGDECIMAL" => 4,
        "DATE" => 5,
        "TIME" => 6,
        "DATETIME" => 7,
        "BLOB" | "FILE" => 8,
        _ => 0,
    }
}

fn binary_value(out: &mut BinaryOut, data_type: &str, cell: &Cell) -> Result<()> {
    let Some(lexical) = cell.lexical() else {
        out.u16(0);
        return Ok(());
    };
    let name = type_name(data_type);
    match name.as_str() {
        "STRING" | "CHAR" | "UNDEFINED" | "DATASET" | "INVALID" => {
            out.u16(21);
            out.bytes(lexical.as_bytes())
        }
        "SHORT" | "USHORT" | "INT" | "UINT" | "BOOLEAN" => {
            let value = binary_integer(lexical).ok_or_else(|| {
                encode_error(format!("invalid PlatformBinary integer {lexical:?}"))
            })?;
            out.u16(3);
            out.u32(value as i32 as u32);
            Ok(())
        }
        "LONG" | "ULONG" | "DECIMAL" | "BIGDECIMAL" => {
            out.u16(40);
            out.bytes(lexical.as_bytes())
        }
        "FLOAT" | "DOUBLE" => {
            let value = parse_java_double(lexical.trim()).ok_or_else(|| {
                encode_error(format!("invalid PlatformBinary number {lexical:?}"))
            })?;
            out.u16(4);
            out.f64(value);
            Ok(())
        }
        "DATE" | "TIME" | "DATETIME" => {
            let millis = epoch_millis(lexical, &name).ok_or_else(|| {
                encode_error(format!(
                    "invalid PlatformBinary {} {lexical:?}",
                    name.to_ascii_lowercase()
                ))
            })?;
            out.u16(41);
            out.f64(millis as f64);
            Ok(())
        }
        "BLOB" | "FILE" => {
            let bytes = STANDARD
                .decode(lexical)
                .map_err(|_| encode_error("invalid base64 BLOB value"))?;
            out.u16(26);
            out.bytes(&bytes)
        }
        "NULL" => {
            out.u16(0);
            Ok(())
        }
        _ => Err(encode_error(format!(
            "unsupported PlatformBinary type {data_type:?}"
        ))),
    }
}

fn binary_integer(text: &str) -> Option<i64> {
    let text = text.trim().replace(',', "");
    match text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
        Some(hex) => i64::from_str_radix(hex, 16).ok(),
        None => text.parse().ok(),
    }
}

/// A double from its Java `toString` form (`NaN`, `Infinity`, digits).
fn parse_java_double(text: &str) -> Option<f64> {
    match text {
        "NaN" => Some(f64::NAN),
        "Infinity" | "+Infinity" => Some(f64::INFINITY),
        "-Infinity" => Some(f64::NEG_INFINITY),
        _ if is_java_double(text) => text.trim_end_matches(['f', 'F', 'd', 'D']).parse().ok(),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_scalars_as_the_driver_does() {
        // From vectors/driver/scalar/driver.nexacro-ssv.scalar-*.encode.
        let cases = [
            ("BIGDECIMAL", "001.20", Some("1.20")),
            ("BIGDECIMAL", "-0.0", Some("0.0")),
            ("BIGDECIMAL", "1e3", Some("1E+3")),
            ("BIGDECIMAL", "-1.5e-3", Some("-0.0015")),
            ("BIGDECIMAL", "1E7", Some("1E+7")),
            ("BIGDECIMAL", "1,234", Some("1234")),
            ("BIGDECIMAL", " 2 ", None),
            ("BIGDECIMAL", "NaN", None),
            ("INT", "0x1F", Some("31")),
            ("INT", "2147483648", Some("0")),
            ("INT", "+5", Some("5")),
            ("INT", "1.9", Some("1")),
            ("LONG", "9223372036854775808", Some("0")),
            ("BOOLEAN", "Y", Some("1")),
            ("BOOLEAN", " true", Some("0")),
            ("FLOAT", "0.1", Some("0.1")),
            ("DOUBLE", "1e7", Some("1.0E7")),
            ("DOUBLE", "abc", Some("0.0")),
            ("DOUBLE", "1.5f", Some("1.5")),
            ("DATE", "20261332", Some("20270201")),
            ("DATE", "2026111", None),
            ("TIME", "123456", Some("123456000")),
            ("DATETIME", "20261116123456", Some("20261116123456000")),
        ];
        for (data_type, input, expected) in cases {
            assert_eq!(
                encode(data_type, input).as_deref(),
                expected,
                "{data_type} {input:?}"
            );
        }
        assert_eq!(java_float_string(1.25e-4, 64), "1.25E-4");
        assert_eq!(java_float_string(1200.0, 64), "1200.0");
        assert_eq!(java_floating("0x1p1", 64), "2.0");
        assert_eq!(decode("BOOLEAN", "yes").as_deref(), Some("true"));
        assert_eq!(decode("BLOB", "!"), None);
        assert_eq!(format_epoch(86_400_000.0, "DATE"), "19700102");
        assert_eq!(epoch_millis("19700102", "DATE"), Some(86_400_000));
    }

    #[test]
    fn orders_json_keys_like_java_hash_map() {
        let fields = java_order(vec![("id", 0), ("type", 1), ("size", 2)]);
        let keys: Vec<_> = fields.iter().map(|(key, _)| *key).collect();
        assert_eq!(keys, ["size", "id", "type"]);
    }
}
