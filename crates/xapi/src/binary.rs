use std::io::{Read, Write};

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

#[derive(Clone)]
struct Value(u16, Option<Vec<u8>>);
fn read_value(r: &mut Reader<'_>, tag: u16) -> Result<Value> {
    let v = match tag {
        0 | 1 => None,
        2 => Some((r.i32()? as i64).to_string().into_bytes()),
        3 => Some(r.i32()?.to_string().into_bytes()),
        4 => Some(r.f64()?.to_string().into_bytes()),
        21 | 40 | 26 => {
            let n = r.len()?;
            Some(r.take(n)?.to_vec())
        }
        41 => Some((r.f64()? as i64).to_string().into_bytes()),
        _ => return Err(malformed("invalid PlatformBinary value type")),
    };
    Ok(Value(tag, v))
}
fn cell(v: Value) -> RawValue {
    match v.1 {
        None => RawValue::Text(String::new()),
        Some(b) => RawValue::Text(String::from_utf8_lossy(&b).into_owned()),
    }
}
fn cell_for(v: Value, _wire: WireType) -> RawValue {
    if v.0 == 26 {
        return RawValue::Bytes(v.1.unwrap_or_default());
    }
    if v.0 == 0 || v.0 == 1 {
        return RawValue::Text(String::new());
    }
    cell(v)
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

pub fn encode<W: Write>(mut out: W, doc: &RawDocument) -> Result<()> {
    if doc.parameters.is_empty() && doc.datasets.is_empty() {
        return Ok(());
    }
    let mut vars = Vec::new();
    put16(&mut vars, doc.parameters.len());
    for (index, (id, value)) in doc.parameters.iter().enumerate() {
        putstr(&mut vars, id)?;
        let wire = doc
            .parameter_wires
            .get(index)
            .and_then(Option::as_deref)
            .unwrap_or("STRING");
        put_typed_name(&mut vars, wire, value)?;
    }
    put16(&mut out, VARIABLE);
    put16(&mut out, VERSION);
    putlen(&mut out, vars.len());
    out.write_all(&vars).map_err(Error::from)?;
    fn put16<W: Write, N: TryInto<u16>>(w: &mut W, n: N) {
        let _ = w.write_all(&n.try_into().ok().unwrap_or(u16::MAX).to_be_bytes());
    }
    fn putstr<W: Write>(w: &mut W, s: &str) -> Result<()> {
        put16(w, s.len());
        w.write_all(s.as_bytes()).map_err(Error::from)
    }
    fn putlen<W: Write>(w: &mut W, n: usize) {
        put16(w, n)
    }
    for dataset in &doc.datasets {
        let mut block = Vec::new();
        putstr(&mut block, &dataset.id)?;
        put16(&mut block, VARIABLE);
        put16(&mut block, VERSION);
        let mut constants = Vec::new();
        put16(&mut constants, dataset.const_columns.len());
        for (id, wire, value) in &dataset.const_columns {
            putstr(&mut constants, id)?;
            put_typed(&mut constants, *wire, value)?;
        }
        putlen(&mut block, constants.len());
        block.extend(constants);
        put16(&mut block, dataset.columns.len());
        for (index, (id, wire)) in dataset.columns.iter().enumerate() {
            putstr(&mut block, id)?;
            put16(&mut block, column_code(*wire));
            let size = dataset
                .column_sizes
                .get(index)
                .and_then(Option::as_deref)
                .and_then(|size| size.parse::<u16>().ok())
                .unwrap_or(0);
            put16(&mut block, size);
            let sumtext = dataset
                .column_meta
                .get(index)
                .and_then(|(_, sumtext)| sumtext.as_deref());
            put16(&mut block, if sumtext.is_some() { 0x6000 } else { 0 });
            if let Some(sumtext) = sumtext {
                putstr(&mut block, sumtext)?;
            }
        }
        for row in &dataset.rows {
            let mut body = Vec::new();
            put16(&mut body, dataset.columns.len());
            for (id, wire) in &dataset.columns {
                put_typed(&mut body, *wire, crate::raw_get(&row.values, id))?;
            }
            if row.row_type == RowType::Update {
                let original = row.original.as_deref().unwrap_or_default();
                put16(&mut body, dataset.columns.len());
                for (id, wire) in &dataset.columns {
                    put_typed(&mut body, *wire, crate::raw_get(original, id))?;
                }
            }
            putlen(&mut out, body.len() + 2);
            put16(&mut out, row_code(row.row_type));
            out.write_all(&body).map_err(Error::from)?;
        }
        put16(&mut out, 0);
        put16(&mut out, 0);
        put16(&mut out, DATASET);
        put16(&mut out, VERSION);
        putlen(&mut out, block.len());
        out.write_all(&block).map_err(Error::from)?;
    }
    Ok(())
}
fn put16<W: Write, N: TryInto<u16>>(w: &mut W, n: N) {
    let _ = w.write_all(&n.try_into().ok().unwrap_or(u16::MAX).to_be_bytes());
}
fn putlen<W: Write>(w: &mut W, n: usize) {
    put16(w, n);
}
fn put_typed_name<W: Write>(w: &mut W, name: &str, v: &RawValue) -> Result<()> {
    match name.to_ascii_uppercase().as_str() {
        "NULL" => {
            put16(w, 0);
            Ok(())
        }
        "UNDEFINED" | "DATASET" | "INVALID" => {
            put16(w, 21);
            put16(w, 0);
            Ok(())
        }
        "INT" => put_typed(w, WireType::Int, v),
        _ => put_typed(w, WireType::String, v),
    }
}
fn put_typed<W: Write>(w: &mut W, wire: WireType, v: &RawValue) -> Result<()> {
    if !matches!(v, RawValue::Text(_) | RawValue::Bytes(_)) {
        put16(w, 0);
        return Ok(());
    }
    let text = match v {
        RawValue::Text(x) => x.as_bytes().to_vec(),
        RawValue::Bytes(x) => x.clone(),
        _ => Vec::new(),
    };
    match wire {
        WireType::Blob => {
            put16(w, 26);
            putlen(w, text.len());
            w.write_all(&text).map_err(Error::from)
        }
        WireType::Int => {
            put16(w, 3);
            let n = std::str::from_utf8(&text)
                .unwrap_or("0")
                .parse::<i32>()
                .unwrap_or(0);
            w.write_all(&n.to_be_bytes()).map_err(Error::from)
        }
        WireType::Double => {
            put16(w, 4);
            let n = std::str::from_utf8(&text)
                .unwrap_or("0")
                .parse::<f64>()
                .unwrap_or(0.0);
            w.write_all(&n.to_bits().to_be_bytes()).map_err(Error::from)
        }
        WireType::BigDecimal => {
            put16(w, 40);
            putlen(w, text.len());
            w.write_all(&text).map_err(Error::from)
        }
        _ => {
            put16(w, 21);
            putlen(w, text.len());
            w.write_all(&text).map_err(Error::from)
        }
    }
}
fn column_code(w: WireType) -> u16 {
    match w {
        WireType::String => 1,
        WireType::Int => 2,
        WireType::Double => 3,
        WireType::BigDecimal => 4,
        WireType::Date => 5,
        WireType::Time => 6,
        WireType::DateTime => 7,
        WireType::Blob => 8,
        _ => 1,
    }
}
fn row_code(w: RowType) -> u16 {
    match w {
        RowType::Insert => 2,
        RowType::Update => 4,
        RowType::Delete => 8,
        _ => 1,
    }
}
fn malformed(s: &str) -> Error {
    Error::from(DecodeError::message(s))
}
