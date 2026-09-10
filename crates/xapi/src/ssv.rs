use std::io::{Read, Write};

use crate::{
    DecodeError, Error, Format, Limits, RawDataset, RawDocument, RawRow, RawValue, Result, RowType,
    WireType,
};

const RS: u8 = 0x1e;
const US: u8 = 0x1f;
const ETX: u8 = 0x03;

pub fn decode_raw<R: Read>(
    mut input: R,
    format: Format,
    limits: Limits,
    strict: bool,
) -> Result<RawDocument> {
    let mut bytes = Vec::new();
    input.read_to_end(&mut bytes).map_err(Error::from)?;
    if bytes.len() as u64 > limits.max_document_bytes {
        return Err(Error::from(DecodeError::Limit("max_document_bytes")));
    }
    let mut document = RawDocument::default();
    let mut records = bytes
        .split(|byte| *byte == RS)
        .filter(|record| !record.is_empty());
    let Some(header) = records.next() else {
        return Ok(document);
    };
    if !header.starts_with(b"SSV") {
        return Ok(document);
    }
    let mut dataset = None;
    let mut columns = Vec::new();
    let mut const_columns = Vec::new();
    let mut pending: Option<RawRow> = None;
    for record in records {
        if record.is_empty() || record == [ETX] {
            continue;
        }
        let fields = record.split(|byte| *byte == US).collect::<Vec<_>>();
        let first = text(fields[0])?;
        if dataset.is_none() && !first.starts_with("Dataset:") {
            let parameter = first.as_str();
            let (header, value, has_value) = parameter
                .split_once('=')
                .map(|(header, value)| (header, value, true))
                .unwrap_or((parameter, "", false));
            let (id, _wire) = typed_name(header)?;
            document.parameters.push((
                id,
                if has_value {
                    scalar_for(value, format_profile(format))
                } else {
                    RawValue::Missing
                },
            ));
            continue;
        }
        if let Some(id) = first.strip_prefix("Dataset:") {
            if let Some(current) = dataset.take() {
                document.datasets.push(current);
            }
            dataset = Some(RawDataset {
                id: id.to_owned(),
                columns: Vec::new(),
                column_sizes: Vec::new(),
                column_meta: Vec::new(),
                const_columns: Vec::new(),
                const_column_sizes: Vec::new(),
                rows: Vec::new(),
            });
            columns.clear();
            const_columns.clear();
            pending = None;
            if strict && id.is_empty() {
                return Err(malformed("empty SSV dataset id"));
            }
            continue;
        }
        if first == "_Const_" {
            const_columns.clear();
            for field in &fields[1..] {
                let descriptor = text(field)?;
                let (header, value, has_value) = descriptor
                    .split_once('=')
                    .map(|(header, value)| (header, value, true))
                    .unwrap_or((descriptor.as_str(), "", false));
                let (id, wire) = typed_name(header)?;
                if has_value {
                    const_columns.push((id, wire, scalar_for(value, format_profile(format))));
                }
            }
            if let Some(current) = dataset.as_mut() {
                current.const_columns = const_columns.clone();
            }
            continue;
        }
        if first == "_RowType_" {
            columns.clear();
            for field in &fields[1..] {
                let descriptor = text(field)?;
                let (id, wire) = column(&descriptor)?;
                columns.push((id, wire));
            }
            if let Some(current) = dataset.as_mut() {
                current.columns = columns.clone();
                current.const_columns = const_columns.clone();
            }
            continue;
        }
        let Some(current) = dataset.as_mut() else {
            if strict {
                return Err(malformed("record outside dataset"));
            }
            continue;
        };
        let kind = match first.as_str() {
            "N" => RowType::Normal,
            "I" => RowType::Insert,
            "U" => RowType::Update,
            "D" => RowType::Delete,
            "O" => RowType::Original,
            _ => continue,
        };
        let values = fields[1..]
            .iter()
            .enumerate()
            .filter_map(|(index, value)| {
                columns.get(index).map(|(id, _)| {
                    (
                        id.clone(),
                        scalar_for(
                            std::str::from_utf8(value).unwrap_or_default(),
                            format_profile(format),
                        ),
                    )
                })
            })
            .collect::<Vec<_>>();
        if kind == RowType::Original {
            if let Some(row) = pending.as_mut() {
                row.original = Some(values);
            }
        } else {
            if let Some(row) = pending.take() {
                current.rows.push(row);
            }
            pending = Some(RawRow {
                row_type: kind,
                values,
                original: None,
            });
        }
        if current.rows.len() > limits.max_rows {
            return Err(Error::from(DecodeError::Limit("max_rows")));
        }
    }
    if let Some(current) = dataset.as_mut() {
        if let Some(row) = pending.take() {
            current.rows.push(row);
        }
    }
    if let Some(current) = dataset {
        document.datasets.push(current);
    }
    Ok(document)
}

pub fn encode<W: Write>(mut output: W, document: &RawDocument, format: Format) -> Result<()> {
    let header = "SSV:UTF-8";
    output.write_all(header.as_bytes()).map_err(Error::from)?;
    output.write_all(&[RS]).map_err(Error::from)?;
    for (index, (id, value)) in document.parameters.iter().enumerate() {
        let wire = document
            .parameter_wires
            .get(index)
            .and_then(|wire| wire.as_deref())
            .unwrap_or("STRING")
            .to_ascii_lowercase();
        output
            .write_all(format!("{}:{}{}", id, wire, parameter_value(value, format)).as_bytes())
            .map_err(Error::from)?;
        output.write_all(&[RS]).map_err(Error::from)?;
    }
    for dataset in &document.datasets {
        output
            .write_all(format!("Dataset:{}", dataset.id).as_bytes())
            .map_err(Error::from)?;
        output.write_all(&[RS]).map_err(Error::from)?;
        if !dataset.const_columns.is_empty() {
            output.write_all(b"_Const_").map_err(Error::from)?;
            for (id, wire, value) in dataset.const_columns.iter() {
                let size = default_size(wire);
                output
                    .write_all(
                        format!(
                            "{}:{}({})={}",
                            id,
                            wire_name(wire),
                            size,
                            cell_value(value, format, Some(wire))
                        )
                        .as_bytes(),
                    )
                    .map_err(Error::from)?;
            }
            output.write_all(&[RS]).map_err(Error::from)?;
        }
        output.write_all(b"_RowType_").map_err(Error::from)?;
        for (index, (id, wire)) in dataset.columns.iter().enumerate() {
            output.write_all(&[US]).map_err(Error::from)?;
            let size = default_size(wire);
            let (prop, sumtext) = dataset
                .column_meta
                .get(index)
                .cloned()
                .unwrap_or((None, None));
            let meta = match (prop, sumtext) {
                (None, None) => String::new(),
                (prop, sum) => format!(
                    ":{}{}",
                    prop.unwrap_or_default(),
                    sum.map(|value| format!(":{value}")).unwrap_or_default()
                ),
            };
            output
                .write_all(format!("{}:{}({}){}", id, wire_name(wire), size, meta).as_bytes())
                .map_err(Error::from)?;
        }
        output.write_all(&[RS]).map_err(Error::from)?;
        for row in &dataset.rows {
            output
                .write_all(row_name(row.row_type).as_bytes())
                .map_err(Error::from)?;
            for (id, _) in &dataset.columns {
                output.write_all(&[US]).map_err(Error::from)?;
                output
                    .write_all(
                        cell_value(
                            crate::raw_get(&row.values, id),
                            format,
                            Some(
                                dataset
                                    .columns
                                    .iter()
                                    .find(|(column, _)| column == id)
                                    .map(|(_, wire)| wire)
                                    .unwrap_or(&WireType::String),
                            ),
                        )
                        .as_bytes(),
                    )
                    .map_err(Error::from)?;
            }
            output.write_all(&[RS]).map_err(Error::from)?;
            if let Some(original) = &row.original {
                output.write_all(b"O").map_err(Error::from)?;
                for (id, _) in &dataset.columns {
                    output.write_all(&[US]).map_err(Error::from)?;
                    output
                        .write_all(
                            cell_value(
                                crate::raw_get(original, id),
                                format,
                                Some(
                                    dataset
                                        .columns
                                        .iter()
                                        .find(|(column, _)| column == id)
                                        .map(|(_, wire)| wire)
                                        .unwrap_or(&WireType::String),
                                ),
                            )
                            .as_bytes(),
                        )
                        .map_err(Error::from)?;
                }
                output.write_all(&[RS]).map_err(Error::from)?;
            }
        }
    }
    if !matches!(format, Format::XPlatformSsv) {
        output.write_all(&[RS]).map_err(Error::from)?;
    }
    Ok(())
}

fn column(field: &str) -> Result<(String, WireType)> {
    let (id, wire) = typed_name(
        field
            .split(':')
            .take(2)
            .collect::<Vec<_>>()
            .join(":")
            .as_str(),
    )?;
    Ok((id, wire))
}
fn typed_name(value: &str) -> Result<(String, WireType)> {
    let (id, type_and_size) = value.split_once(':').unwrap_or((value, "STRING"));
    let wire = parse_wire(type_and_size.split('(').next().unwrap_or(type_and_size))?;
    if id.is_empty() {
        Ok((value.to_owned(), wire))
    } else {
        Ok((id.to_owned(), wire))
    }
}
fn parse_wire(value: &str) -> Result<WireType> {
    if value.trim().is_empty() {
        return Ok(WireType::String);
    }
    Ok(match value.to_ascii_uppercase().as_str() {
        "STRING" => WireType::String,
        "INT" => WireType::Int,
        "LONG" => WireType::Long,
        "FLOAT" => WireType::Float,
        "DOUBLE" => WireType::Double,
        "DECIMAL" => WireType::Decimal,
        "BIGDECIMAL" => WireType::BigDecimal,
        "DATE" => WireType::Date,
        "TIME" => WireType::Time,
        "DATETIME" => WireType::DateTime,
        "BLOB" | "FILE" => WireType::Blob,
        "BOOLEAN" | "BOOL" => WireType::Boolean,
        _ => WireType::String,
    })
}
fn scalar(value: &str) -> RawValue {
    if value == "#NULL#" {
        RawValue::Null
    } else {
        RawValue::Text(value.to_owned())
    }
}

fn wire_name(wire: &WireType) -> String {
    wire.to_string().to_ascii_lowercase()
}

fn default_size(wire: &WireType) -> &'static str {
    match wire {
        WireType::String => "32",
        WireType::Int => "4",
        WireType::Long => "8",
        WireType::Float => "4",
        WireType::Double => "8",
        WireType::Decimal | WireType::BigDecimal => "16",
        WireType::Boolean => "2",
        WireType::Date => "6",
        WireType::Time => "9",
        WireType::DateTime => "17",
        WireType::Blob => "256",
    }
}

fn scalar_for(value: &str, profile: &str) -> RawValue {
    if profile == "xplatform-ssv" {
        if value == "\x02" {
            return RawValue::Null;
        }
        if value.is_empty() {
            return RawValue::Text(String::new());
        }
    } else if value == "\x03" {
        return RawValue::Null;
    }
    scalar(value)
}
fn format_profile(format: Format) -> &'static str {
    match format {
        Format::XPlatformSsv => "xplatform-ssv",
        _ => "nexacro-ssv",
    }
}
fn parameter_value(value: &RawValue, format: Format) -> String {
    match value {
        RawValue::Missing => String::new(),
        _ => format!("={}", cell_value(value, format, None)),
    }
}
fn cell_value(value: &RawValue, format: Format, _wire: Option<&WireType>) -> String {
    match value {
        RawValue::Missing | RawValue::Null => {
            if matches!(format, Format::XPlatformSsv) {
                String::new()
            } else {
                "\x03".into()
            }
        }
        RawValue::Text(value) if value.is_empty() && matches!(format, Format::XPlatformSsv) => {
            String::new()
        }
        RawValue::Text(value) if value.is_empty() => String::new(),
        RawValue::Text(value) => value.clone(),
        RawValue::Bytes(value) => String::from_utf8_lossy(value).into_owned(),
    }
}
fn text(value: &[u8]) -> Result<String> {
    String::from_utf8(value.to_vec()).map_err(|_| malformed("SSV is not UTF-8"))
}
fn row_name(row: RowType) -> &'static str {
    match row {
        RowType::Normal => "N",
        RowType::Insert => "I",
        RowType::Update => "U",
        RowType::Delete => "D",
        RowType::Original => "O",
    }
}
fn malformed(message: &str) -> Error {
    Error::from(DecodeError::message(message))
}
