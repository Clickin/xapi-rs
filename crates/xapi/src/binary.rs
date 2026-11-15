use std::io::Read;

use crate::{
    DecodeError, Error, Limits, RawDataset, RawDocument, RawRow, RawValue, Result, RowType,
    WireType,
};

const VARIABLE: u16 = 0xfe10;
const DATASET: u16 = 0xfe01;
const VERSION: u16 = 5000;
const MAX: usize = 10 << 20;

struct Reader<'a> {
    b: &'a [u8],
    p: usize,
}
impl<'a> Reader<'a> {
    fn new(b: &'a [u8]) -> Self {
        Self { b, p: 0 }
    }
    fn left(&self) -> usize {
        self.b.len().saturating_sub(self.p)
    }
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if n > MAX || n > self.left() {
            return Err(malformed("binary length exceeds payload"));
        }
        let x = &self.b[self.p..self.p + n];
        self.p += n;
        Ok(x)
    }
    fn u16(&mut self) -> Result<u16> {
        let b = self.take(2)?;
        Ok(u16::from_be_bytes([b[0], b[1]]))
    }
    fn i32(&mut self) -> Result<i32> {
        let b = self.take(4)?;
        Ok(i32::from_be_bytes(b.try_into().unwrap()))
    }
    fn f64(&mut self) -> Result<f64> {
        let b = self.take(8)?;
        Ok(f64::from_bits(u64::from_be_bytes(b.try_into().unwrap())))
    }
    fn len(&mut self) -> Result<usize> {
        let first = self.u16()?;
        if first & 0x8000 == 0 {
            return Ok(first as usize);
        }
        let low = self.u16()?;
        let n = (((first & 0x7fff) as usize) << 16) | (low as usize);
        if n > MAX {
            return Err(malformed("binary length exceeds payload"));
        }
        Ok(n)
    }
    fn string(&mut self) -> Result<String> {
        let n = self.u16()? as usize;
        String::from_utf8(self.take(n)?.to_vec()).map_err(|_| malformed("invalid binary string"))
    }
}

pub fn decode_raw<R: Read>(mut input: R, limits: Limits, _strict: bool) -> Result<RawDocument> {
    let mut b = Vec::new();
    input.read_to_end(&mut b).map_err(Error::from)?;
    if b.len() as u64 > limits.max_document_bytes || b.len() > MAX {
        return Err(Error::from(DecodeError::Limit("max_document_bytes")));
    }
    let mut r = Reader::new(&b);
    let first = r.u16()?;
    if first != VARIABLE && first != DATASET {
        return Err(malformed("invalid PlatformBinary mark"));
    }
    let mut out = RawDocument::default();
    if first == VARIABLE {
        read_variables(&mut r, &mut out)?;
    } else {
        read_dataset(&mut r, &mut out, limits)?;
    }
    while r.left() > 0 {
        let mark = r.u16()?;
        if mark != DATASET {
            return Err(malformed("invalid PlatformBinary dataset mark"));
        }
        read_dataset(&mut r, &mut out, limits)?;
    }
    Ok(out)
}

fn read_variables(r: &mut Reader<'_>, out: &mut RawDocument) -> Result<()> {
    version(r)?;
    let n = r.len()?;
    let block = r.take(n)?;
    let mut q = Reader::new(block);
    let n = q.u16()? as usize;
    for _ in 0..n {
        let id = q.string()?;
        let tag = q.u16()?;
        let v = read_value(&mut q, tag)?;
        out.parameters.push((id, cell(v)));
        out.parameter_wires.push(Some(variant_name(tag).into()));
    }
    if q.left() != 0 {
        return Err(malformed("trailing variable block"));
    }
    Ok(())
}
fn version(r: &mut Reader<'_>) -> Result<()> {
    if r.u16()? != VERSION {
        Err(malformed("invalid PlatformBinary version"))
    } else {
        Ok(())
    }
}
fn read_dataset(r: &mut Reader<'_>, out: &mut RawDocument, limits: Limits) -> Result<()> {
    version(r)?;
    let n = r.len()?;
    let block = r.take(n)?;
    let mut h = Reader::new(block);
    let id = h.string()?;
    if h.u16()? != VARIABLE {
        return Err(malformed("invalid constant block mark"));
    }
    version(&mut h)?;
    let n = h.len()?;
    let constants = h.take(n)?;
    let mut c = Reader::new(constants);
    let mut const_columns = Vec::new();
    for _ in 0..c.u16()? {
        let name = c.string()?;
        let tag = c.u16()?;
        let v = read_value(&mut c, tag)?;
        const_columns.push((name, variant_wire(v.0), cell(v)));
    }
    if c.left() != 0 {
        return Err(malformed("trailing constant block"));
    }
    let mut columns = Vec::new();
    let count = h.u16()? as usize;
    if count > limits.max_columns {
        return Err(Error::from(DecodeError::Limit("max_columns")));
    }
    for _ in 0..count {
        let name = h.string()?;
        let code = h.u16()?;
        let size = h.u16()?;
        let attr = h.u16()?;
        let mut sum = None;
        if attr & 0xf000 == 0x6000 {
            sum = Some(h.string()?);
        }
        columns.push((
            name,
            column_wire(code),
            if size == 0 { None } else { Some(size as usize) },
            sum,
        ));
    }
    if h.left() != 0 {
        return Err(malformed("trailing dataset header"));
    }
    let mut rows = Vec::new();
    loop {
        let n = r.len()?;
        let kind = r.u16()?;
        if n == 0 && kind == 0 {
            break;
        }
        if n < 2 {
            return Err(malformed("invalid row length"));
        }
        let body = r.take(n - 2)?;
        let mut q = Reader::new(body);
        let _count = q.u16()? as usize;
        let row_type = row_wire(kind);
        let mut values = Vec::new();
        for (i, (name, wire, _, _)) in columns.iter().enumerate() {
            if i >= _count {
                break;
            }
            let tag = q.u16()?;
            let v = read_value(&mut q, tag)?;
            values.push((name.clone(), cell_for(v, *wire)));
        }
        let mut original = None;
        if row_type == RowType::Update {
            let n = q.u16()? as usize;
            let mut values2 = Vec::new();
            for (i, (name, wire, _, _)) in columns.iter().enumerate() {
                if i >= n {
                    break;
                }
                let tag = q.u16()?;
                let v = read_value(&mut q, tag)?;
                values2.push((name.clone(), cell_for(v, *wire)));
            }
            original = Some(values2);
        }
        if q.left() != 0 {
            return Err(malformed("trailing binary row"));
        }
        rows.push(RawRow {
            row_type,
            values,
            original,
        });
        if rows.len() > limits.max_rows {
            return Err(Error::from(DecodeError::Limit("max_rows")));
        }
    }
    out.datasets.push(RawDataset {
        id,
        columns: columns.iter().map(|(n, w, _, _)| (n.clone(), *w)).collect(),
        column_sizes: columns
            .iter()
            .map(|(_, _, s, _)| s.map(|v| v.to_string()))
            .collect(),
        column_meta: Vec::new(),
        const_columns,
        const_column_sizes: Vec::new(),
        rows,
    });
    Ok(())
}

/// A value as read: the tag and the driver lexical (bytes for BLOB tags);
/// `None` for the null tags.
#[derive(Clone)]
struct Value(u16, Option<Vec<u8>>, Option<f64>);
fn read_value(r: &mut Reader<'_>, tag: u16) -> Result<Value> {
    let v = match tag {
        0 | 1 => return Ok(Value(tag, None, None)),
        2 => {
            let b = r.take(2)?;
            let v = i16::from_be_bytes([b[0], b[1]]);
            Some((if v != 0 { "true" } else { "false" }).as_bytes().to_vec())
        }
        3 => Some(r.i32()?.to_string().into_bytes()),
        4 => Some(crate::driver::java_float_string(r.f64()?, 64).into_bytes()),
        21 | 40 | 26 => {
            let n = r.len()?;
            Some(r.take(n)?.to_vec())
        }
        41 => {
            let millis = r.f64()?;
            return Ok(Value(tag, Some(Vec::new()), Some(millis)));
        }
        _ => return Err(malformed("invalid PlatformBinary value type")),
    };
    Ok(Value(tag, v, None))
}
/// A variable or constant value: dates read as DATETIME.
fn cell(v: Value) -> RawValue {
    cell_for(v, WireType::DateTime)
}
/// A value read as the column type, like the drivers: a date as the column's
/// date type, string text in a BLOB column as its bytes.
fn cell_for(v: Value, wire: WireType) -> RawValue {
    if let Some(millis) = v.2 {
        return RawValue::Text(crate::driver::format_epoch(millis, &wire.to_string()));
    }
    match v.1 {
        None => RawValue::Null,
        Some(b) if v.0 == 26 || wire == WireType::Blob && v.0 == 21 => RawValue::Bytes(b),
        Some(b) => RawValue::Text(String::from_utf8_lossy(&b).into_owned()),
    }
}
fn variant_wire(v: u16) -> WireType {
    match v {
        2 => WireType::Boolean,
        3 => WireType::Int,
        4 => WireType::Double,
        21 => WireType::String,
        26 => WireType::Blob,
        40 => WireType::BigDecimal,
        41 => WireType::DateTime,
        _ => WireType::String,
    }
}
fn variant_name(v: u16) -> &'static str {
    match v {
        0 | 1 => "UNDEFINED",
        2 => "BOOLEAN",
        3 => "INT",
        4 => "DOUBLE",
        21 => "STRING",
        26 => "BLOB",
        40 => "BIGDECIMAL",
        41 => "DATETIME",
        _ => "UNDEFINED",
    }
}
fn column_wire(v: u16) -> WireType {
    match v {
        1 => WireType::String,
        2 => WireType::Int,
        3 => WireType::Double,
        4 => WireType::BigDecimal,
        5 => WireType::Date,
        6 => WireType::Time,
        7 => WireType::DateTime,
        8 => WireType::Blob,
        _ => WireType::String,
    }
}
fn row_wire(v: u16) -> RowType {
    match v {
        2 => RowType::Insert,
        4 => RowType::Update,
        8 => RowType::Delete,
        _ => RowType::Normal,
    }
}

fn malformed(s: &str) -> Error {
    Error::from(DecodeError::message(s))
}
