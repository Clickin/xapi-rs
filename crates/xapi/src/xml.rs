use std::io::{Read, Write};

use base64::{engine::general_purpose::STANDARD, Engine};
use quick_xml::{
    escape::unescape,
    events::{BytesEnd, BytesStart, BytesText, Event},
    Reader, Writer, XmlVersion,
};

use crate::{
    optimize_raw, DecodeError, EncodeError, Error, FieldSchema, Format, Limits, RawDataset,
    RawDocument, RawRow, RawValue, Result, RowType, XapiEncodeSink, XapiRoot, XapiRootDecoder,
};

#[allow(dead_code)]
pub fn decode_raw<R: Read>(input: R, limits: Limits) -> Result<RawDocument> {
    let mut reader = Reader::from_reader(std::io::BufReader::new(
        input.take(limits.max_document_bytes.saturating_add(1)),
    ));
    reader.config_mut().trim_text(false);
    let mut buffer = Vec::new();
    let mut document = RawDocument::default();
    let mut dataset: Option<RawDataset> = None;
    let mut row: Option<RawRow> = None;
    let mut in_original = false;
    let mut value_target: Option<(String, bool, bool)> = None;
    let mut value = String::new();
    let mut depth = 0usize;
    loop {
        match reader.read_event_into(&mut buffer) {
            Ok(Event::Start(start)) => {
                depth += 1;
                if depth > limits.max_depth {
                    return Err(Error::from(DecodeError::Limit("max_depth")));
                }
                match start.local_name().as_ref() {
                    b"Dataset" => {
                        if document.datasets.len() >= limits.max_datasets {
                            return Err(Error::from(DecodeError::Limit("max_datasets")));
                        }
                        dataset = Some(RawDataset {
                            id: attr(&reader, &start, b"id")?.unwrap_or_default(),
                            columns: Vec::new(),
                            const_columns: Vec::new(),
                            rows: Vec::new(),
                        })
                    }
                    b"Row" => {
                        row = Some(RawRow {
                            row_type: parse_row_type(attr(&reader, &start, b"type")?.as_deref())?,
                            values: Vec::new(),
                            original: None,
                        })
                    }
                    b"OrgRow" => {
                        if let Some(current) = row.as_mut() {
                            current.original = Some(Vec::new());
                        }
                        in_original = true;
                    }
                    b"Column" if row.is_none() => {
                        if let Some(current) = dataset.as_mut() {
                            if current.columns.len() >= limits.max_columns {
                                return Err(Error::from(DecodeError::Limit("max_columns")));
                            }
                            current.columns.push((
                                attr(&reader, &start, b"id")?.unwrap_or_default(),
                                parse_wire(attr(&reader, &start, b"type")?.as_deref())?,
                            ));
                        }
                    }
                    b"ConstColumn" if row.is_none() => {
                        if let Some(current) = dataset.as_mut() {
                            if current.columns.len() + current.const_columns.len()
                                >= limits.max_columns
                            {
                                return Err(Error::from(DecodeError::Limit("max_columns")));
                            }
                            let id = attr(&reader, &start, b"id")?.unwrap_or_default();
                            let wire = parse_wire(attr(&reader, &start, b"type")?.as_deref())?;
                            let value = if is_null(&reader, &start)? {
                                RawValue::Null
                            } else {
                                RawValue::Text(attr(&reader, &start, b"value")?.unwrap_or_default())
                            };
                            current.const_columns.push((id, wire, value));
                        }
                    }
                    b"Col" => {
                        if row.is_none() {
                            return Err(Error::from(DecodeError::message(
                                "column value outside row",
                            )));
                        }
                        value_target = Some((
                            attr(&reader, &start, b"id")?.unwrap_or_default(),
                            true,
                            is_null(&reader, &start)?,
                        ))
                    }
                    b"Parameter" => {
                        let explicit_value = attr(&reader, &start, b"value")?;
                        value.clear();
                        if let Some(explicit_value) = explicit_value {
                            value.push_str(&explicit_value);
                        }
                        value_target = Some((
                            attr(&reader, &start, b"id")?.unwrap_or_default(),
                            false,
                            is_null(&reader, &start)?,
                        ))
                    }
                    _ => {}
                }
            }
            Ok(Event::Text(text)) if value_target.is_some() => {
                append_text(&mut value, &reader, &text)?;
                if value.len() > limits.max_scalar_bytes {
                    return Err(Error::from(DecodeError::Limit("max_scalar_bytes")));
                }
            }
            Ok(Event::GeneralRef(reference)) => {
                let decoded = reference.decode().map_err(|error| {
                    Error::from(DecodeError::message(format!(
                        "invalid XML character reference: {error}"
                    )))
                })?;
                if let Some(character) = reference.resolve_char_ref().map_err(|error| {
                    Error::from(DecodeError::message(format!(
                        "invalid XML character reference: {error}"
                    )))
                })? {
                    value.push(character);
                } else {
                    value.push_str(
                        unescape(&format!("&{decoded};"))
                            .map_err(|error| {
                                Error::from(DecodeError::message(format!(
                                    "invalid XML entity: {error}"
                                )))
                            })?
                            .as_ref(),
                    );
                }
                if value.len() > limits.max_scalar_bytes {
                    return Err(Error::from(DecodeError::Limit("max_scalar_bytes")));
                }
            }
            Ok(Event::CData(text)) if value_target.is_some() => {
                value.push_str(std::str::from_utf8(&text).map_err(|error| {
                    Error::from(DecodeError::message(format!("invalid XML CDATA: {error}")))
                })?);
                if value.len() > limits.max_scalar_bytes {
                    return Err(Error::from(DecodeError::Limit("max_scalar_bytes")));
                }
            }
            Ok(Event::Empty(start)) => match start.local_name().as_ref() {
                b"Column" if row.is_none() => {
                    if let Some(current) = dataset.as_mut() {
                        if current.columns.len() >= limits.max_columns {
                            return Err(Error::from(DecodeError::Limit("max_columns")));
                        }
                        current.columns.push((
                            attr(&reader, &start, b"id")?.unwrap_or_default(),
                            parse_wire(attr(&reader, &start, b"type")?.as_deref())?,
                        ));
                    }
                }
                b"Col" => {
                    if let Some(current) = row.as_mut() {
                        let target = if in_original {
                            current.original.as_mut().unwrap_or(&mut current.values)
                        } else {
                            &mut current.values
                        };
                        target.push((
                            attr(&reader, &start, b"id")?.unwrap_or_default(),
                            if is_null(&reader, &start)? {
                                RawValue::Null
                            } else {
                                RawValue::Text(String::new())
                            },
                        ));
                    }
                }
                b"Parameter" => document.parameters.push((
                    attr(&reader, &start, b"id")?.unwrap_or_default(),
                    if is_null(&reader, &start)? {
                        RawValue::Null
                    } else {
                        RawValue::Text(attr(&reader, &start, b"value")?.unwrap_or_default())
                    },
                )),
                _ => {}
            },
            Ok(Event::End(end)) => {
                match end.local_name().as_ref() {
                    b"Col" | b"Parameter" => {
                        if let Some((id, is_col, null)) = value_target.take() {
                            let raw = if null {
                                RawValue::Null
                            } else {
                                RawValue::Text(std::mem::take(&mut value))
                            };
                            if is_col {
                                if let Some(current) = row.as_mut() {
                                    let target = if in_original {
                                        current.original.as_mut().unwrap_or(&mut current.values)
                                    } else {
                                        &mut current.values
                                    };
                                    target.push((id, raw));
                                }
                            } else {
                                document.parameters.push((id, raw));
                            }
                        }
                    }
                    b"Row" => {
                        if let Some(current) = row.take() {
                            if let Some(current_dataset) = dataset.as_mut() {
                                if current_dataset.rows.len() >= limits.max_rows {
                                    return Err(Error::from(DecodeError::Limit("max_rows")));
                                }
                                current_dataset.rows.push(current);
                            }
                        }
                    }
                    b"OrgRow" => in_original = false,
                    b"Dataset" => {
                        if let Some(current) = dataset.take() {
                            document.datasets.push(current);
                        }
                    }
                    _ => {}
                }
                depth = depth.saturating_sub(1);
            }
            Ok(Event::DocType(_)) => {
                return Err(Error::from(DecodeError::message(
                    "DOCTYPE/DTD is not permitted",
                )))
            }
            Ok(Event::Eof) => break,
            Err(error) => {
                return Err(Error::from(DecodeError::message(format!(
                    "malformed XML: {error}"
                ))))
            }
            _ => {}
        }
        buffer.clear();
    }
    if reader.into_inner().into_inner().limit() == 0 {
        return Err(Error::from(DecodeError::Limit("max_document_bytes")));
    }
    if depth != 0 {
        return Err(Error::from(DecodeError::message(
            "malformed XML: unclosed element",
        )));
    }
    Ok(document)
}

pub fn decode_stream<T: XapiRoot, R: Read>(input: R, limits: Limits, strict: bool) -> Result<T> {
    let mut reader = Reader::from_reader(std::io::BufReader::new(
        input.take(limits.max_document_bytes.saturating_add(1)),
    ));
    reader.config_mut().trim_text(false);
    let mut buffer = Vec::new();
    let mut decoder = T::decoder(strict, limits);
    let mut dataset = None::<String>;
    let mut dataset_columns = Vec::<(String, crate::WireType)>::new();
    let mut dataset_const_columns = Vec::<(String, crate::WireType, RawValue)>::new();
    let mut row = None::<RawRow>;
    let mut in_original = false;
    let mut value_target = None::<(String, bool, bool)>;
    let mut value = String::new();
    let mut depth = 0usize;
    let mut datasets = 0usize;
    let mut columns = 0usize;
    let mut rows = 0usize;
    loop {
        match reader.read_event_into(&mut buffer) {
            Ok(Event::Start(start)) => {
                depth += 1;
                if depth > limits.max_depth {
                    return Err(Error::from(DecodeError::Limit("max_depth")));
                }
                if depth == 2
                    && !matches!(
                        start.local_name().as_ref(),
                        b"Parameters" | b"Datasets" | b"Dataset" | b"Parameter"
                    )
                {
                    let local_name = start.local_name();
                    let id = std::str::from_utf8(local_name.as_ref()).map_err(|_| {
                        Error::from(DecodeError::message("invalid XML root member"))
                    })?;
                    decoder.unknown_root_member(id)?;
                }
                match start.local_name().as_ref() {
                    b"Dataset" => {
                        datasets += 1;
                        if datasets > limits.max_datasets {
                            return Err(Error::from(DecodeError::Limit("max_datasets")));
                        }
                        let id = attr(&reader, &start, b"id")?.unwrap_or_default();
                        decoder.dataset_begin(&id)?;
                        dataset = Some(id);
                        dataset_columns.clear();
                        dataset_const_columns.clear();
                        columns = 0;
                        rows = 0;
                    }
                    b"Column" => {
                        columns += 1;
                        if columns > limits.max_columns {
                            return Err(Error::from(DecodeError::Limit("max_columns")));
                        }
                        if let Some(id) = attr(&reader, &start, b"id")? {
                            if dataset_columns.iter().any(|(column, _)| column == &id) {
                                return Err(Error::from(DecodeError::message(
                                    "duplicate XML column id",
                                )));
                            }
                            dataset_columns.push((
                                id,
                                parse_wire(attr(&reader, &start, b"type")?.as_deref())?,
                            ));
                        }
                    }
                    b"ConstColumn" => {
                        columns += 1;
                        if columns > limits.max_columns {
                            return Err(Error::from(DecodeError::Limit("max_columns")));
                        }
                        let id = attr(&reader, &start, b"id")?.unwrap_or_default();
                        let wire = parse_wire(attr(&reader, &start, b"type")?.as_deref())?;
                        if dataset_columns.iter().any(|(column, _)| column == &id)
                            || dataset_const_columns
                                .iter()
                                .any(|(column, _, _)| column == &id)
                        {
                            return Err(Error::from(DecodeError::message(
                                "duplicate XML column id",
                            )));
                        }
                        dataset_columns.push((id.clone(), wire));
                        let raw = if is_null(&reader, &start)? {
                            RawValue::Null
                        } else {
                            let value = attr(&reader, &start, b"value")?.unwrap_or_default();
                            enforce_scalar_limit(&value, limits)?;
                            RawValue::Text(value)
                        };
                        dataset_const_columns.push((
                            id,
                            wire,
                            optimize_raw(raw, Some(wire), limits)?,
                        ));
                    }
                    b"Row" => {
                        row = Some(RawRow {
                            row_type: parse_row_type(attr(&reader, &start, b"type")?.as_deref())?,
                            values: Vec::new(),
                            original: None,
                        })
                    }
                    b"OrgRow" => {
                        if let Some(current) = row.as_mut() {
                            current.original = Some(Vec::new());
                        }
                        in_original = true;
                    }
                    b"Col" => {
                        if row.is_none() {
                            return Err(Error::from(DecodeError::message(
                                "column value outside row",
                            )));
                        }
                        value_target = Some((
                            attr(&reader, &start, b"id")?.unwrap_or_default(),
                            true,
                            is_null(&reader, &start)?,
                        ))
                    }
                    b"Parameter" => {
                        let explicit_value = attr(&reader, &start, b"value")?;
                        value.clear();
                        if let Some(explicit_value) = explicit_value {
                            value.push_str(&explicit_value);
                            enforce_scalar_limit(&value, limits)?;
                        }
                        value_target = Some((
                            attr(&reader, &start, b"id")?.unwrap_or_default(),
                            false,
                            is_null(&reader, &start)?,
                        ))
                    }
                    _ => {}
                }
            }
            Ok(Event::Text(text)) if value_target.is_some() => {
                append_text(&mut value, &reader, &text)?;
                if value.len() > limits.max_scalar_bytes {
                    return Err(Error::from(DecodeError::Limit("max_scalar_bytes")));
                }
            }
            Ok(Event::GeneralRef(reference)) => {
                let decoded = reference.decode().map_err(|error| {
                    Error::from(DecodeError::message(format!(
                        "invalid XML character reference: {error}"
                    )))
                })?;
                if let Some(character) = reference.resolve_char_ref().map_err(|error| {
                    Error::from(DecodeError::message(format!(
                        "invalid XML character reference: {error}"
                    )))
                })? {
                    value.push(character);
                } else {
                    value.push_str(
                        unescape(&format!("&{decoded};"))
                            .map_err(|error| {
                                Error::from(DecodeError::message(format!(
                                    "invalid XML entity: {error}"
                                )))
                            })?
                            .as_ref(),
                    );
                }
                if value.len() > limits.max_scalar_bytes {
                    return Err(Error::from(DecodeError::Limit("max_scalar_bytes")));
                }
            }
            Ok(Event::CData(text)) if value_target.is_some() => {
                value.push_str(std::str::from_utf8(&text).map_err(|error| {
                    Error::from(DecodeError::message(format!("invalid XML CDATA: {error}")))
                })?);
                if value.len() > limits.max_scalar_bytes {
                    return Err(Error::from(DecodeError::Limit("max_scalar_bytes")));
                }
            }
            Ok(Event::Empty(start)) => {
                if depth == 1
                    && !matches!(
                        start.local_name().as_ref(),
                        b"Parameters" | b"Datasets" | b"Dataset" | b"Parameter"
                    )
                {
                    let local_name = start.local_name();
                    let id = std::str::from_utf8(local_name.as_ref()).map_err(|_| {
                        Error::from(DecodeError::message("invalid XML root member"))
                    })?;
                    decoder.unknown_root_member(id)?;
                }
                match start.local_name().as_ref() {
                    b"Row" => {
                        let current = RawRow {
                            row_type: parse_row_type(attr(&reader, &start, b"type")?.as_deref())?,
                            values: Vec::new(),
                            original: None,
                        };
                        rows += 1;
                        if rows > limits.max_rows {
                            return Err(Error::from(DecodeError::Limit("max_rows")));
                        }
                        decoder.dataset_row(dataset.as_deref().unwrap_or_default(), current)?;
                    }
                    b"OrgRow" => {
                        // A self-closing OrgRow carries no original values.
                        in_original = false;
                    }
                    b"Dataset" => {
                        datasets += 1;
                        if datasets > limits.max_datasets {
                            return Err(Error::from(DecodeError::Limit("max_datasets")));
                        }
                        let id = attr(&reader, &start, b"id")?.unwrap_or_default();
                        decoder.dataset_begin(&id)?;
                        decoder.dataset_columns(&id, &[])?;
                        columns = 0;
                        rows = 0;
                    }
                    b"Column" => {
                        columns += 1;
                        if columns > limits.max_columns {
                            return Err(Error::from(DecodeError::Limit("max_columns")));
                        }
                        if let Some(id) = attr(&reader, &start, b"id")? {
                            if dataset_columns.iter().any(|(column, _)| column == &id) {
                                return Err(Error::from(DecodeError::message(
                                    "duplicate XML column id",
                                )));
                            }
                            dataset_columns.push((
                                id,
                                parse_wire(attr(&reader, &start, b"type")?.as_deref())?,
                            ));
                        }
                    }
                    b"ConstColumn" => {
                        columns += 1;
                        if columns > limits.max_columns {
                            return Err(Error::from(DecodeError::Limit("max_columns")));
                        }
                        let id = attr(&reader, &start, b"id")?.unwrap_or_default();
                        let wire = parse_wire(attr(&reader, &start, b"type")?.as_deref())?;
                        if dataset_columns.iter().any(|(column, _)| column == &id)
                            || dataset_const_columns
                                .iter()
                                .any(|(column, _, _)| column == &id)
                        {
                            return Err(Error::from(DecodeError::message(
                                "duplicate XML column id",
                            )));
                        }
                        dataset_columns.push((id.clone(), wire));
                        let raw = if is_null(&reader, &start)? {
                            RawValue::Null
                        } else {
                            let value = attr(&reader, &start, b"value")?.unwrap_or_default();
                            enforce_scalar_limit(&value, limits)?;
                            RawValue::Text(value)
                        };
                        dataset_const_columns.push((
                            id,
                            wire,
                            optimize_for::<T>(raw, Some(wire), limits)?,
                        ));
                    }
                    b"Col" => {
                        if let Some(current) = row.as_mut() {
                            let id = attr(&reader, &start, b"id")?.unwrap_or_default();
                            let raw = if is_null(&reader, &start)? {
                                RawValue::Null
                            } else {
                                RawValue::Text(String::new())
                            };
                            let wire = dataset.as_deref().and_then(|dataset| {
                                dataset_columns
                                    .iter()
                                    .find(|(column, _)| column == &id)
                                    .map(|(_, wire)| *wire)
                                    .or_else(|| T::dataset_wire(dataset, &id))
                            });
                            let target = if in_original {
                                current.original.as_mut().unwrap_or(&mut current.values)
                            } else {
                                &mut current.values
                            };
                            target.push((id, optimize_for::<T>(raw, wire, limits)?));
                        }
                    }
                    b"Parameter" => {
                        let id = attr(&reader, &start, b"id")?.unwrap_or_default();
                        let raw = if is_null(&reader, &start)? {
                            RawValue::Null
                        } else {
                            let value = attr(&reader, &start, b"value")?.unwrap_or_default();
                            enforce_scalar_limit(&value, limits)?;
                            RawValue::Text(value)
                        };
                        let wire = T::schema()
                            .fields
                            .iter()
                            .find(|field| {
                                field.kind == crate::RootFieldKind::Parameter && field.id == id
                            })
                            .map(|field| field.wire);
                        let raw = optimize_for::<T>(raw, wire, limits)?;
                        decoder.parameter(&id, &raw)?;
                    }
                    _ => {}
                }
            }
            Ok(Event::End(end)) => {
                match end.local_name().as_ref() {
                    b"Col" | b"Parameter" => {
                        if let Some((id, is_col, null)) = value_target.take() {
                            let raw = if null {
                                RawValue::Null
                            } else {
                                RawValue::Text(std::mem::take(&mut value))
                            };
                            let wire = if is_col {
                                dataset.as_deref().and_then(|dataset| {
                                    dataset_columns
                                        .iter()
                                        .find(|(column, _)| column == &id)
                                        .map(|(_, wire)| *wire)
                                        .or_else(|| T::dataset_wire(dataset, &id))
                                })
                            } else {
                                T::schema()
                                    .fields
                                    .iter()
                                    .find(|field| {
                                        field.kind == crate::RootFieldKind::Parameter
                                            && field.id == id
                                    })
                                    .map(|field| field.wire)
                            };
                            let raw = optimize_for::<T>(raw, wire, limits)?;
                            if is_col {
                                if let Some(current) = row.as_mut() {
                                    let target = if in_original {
                                        current.original.as_mut().unwrap_or(&mut current.values)
                                    } else {
                                        &mut current.values
                                    };
                                    target.push((id, raw));
                                }
                            } else {
                                decoder.parameter(&id, &raw)?;
                            }
                        }
                    }
                    b"Row" => {
                        if let Some(current) = row.take() {
                            rows += 1;
                            if rows > limits.max_rows {
                                return Err(Error::from(DecodeError::Limit("max_rows")));
                            }
                            decoder.dataset_row(dataset.as_deref().unwrap_or_default(), current)?;
                        }
                    }
                    b"OrgRow" => in_original = false,
                    b"Dataset" => {
                        let dataset_id = dataset.as_deref().unwrap_or_default();
                        for (column, wire, value) in &dataset_const_columns {
                            decoder.dataset_const_column(dataset_id, column, *wire, value)?;
                        }
                        decoder.dataset_columns(dataset_id, &dataset_columns)?;
                        dataset = None;
                    }
                    _ => {}
                }
                depth = depth.saturating_sub(1);
            }
            Ok(Event::DocType(_)) => {
                return Err(Error::from(DecodeError::message(
                    "DOCTYPE/DTD is not permitted",
                )))
            }
            Ok(Event::Eof) => break,
            Err(error) => {
                return Err(Error::from(DecodeError::message(format!(
                    "malformed XML: {error}"
                ))))
            }
            _ => {}
        }
        buffer.clear();
    }
    if reader.into_inner().into_inner().limit() == 0 {
        return Err(Error::from(DecodeError::Limit("max_document_bytes")));
    }
    if depth != 0 {
        return Err(Error::from(DecodeError::message(
            "malformed XML: unclosed element",
        )));
    }
    decoder.finish()
}

fn parse_row_type(value: Option<&str>) -> Result<RowType> {
    match value.unwrap_or("N").to_ascii_uppercase().as_str() {
        "N" | "NORMAL" => Ok(RowType::Normal),
        "I" | "INSERT" => Ok(RowType::Insert),
        "U" | "UPDATE" => Ok(RowType::Update),
        "D" | "DELETE" => Ok(RowType::Delete),
        _ => Err(Error::from(DecodeError::message("invalid row type"))),
    }
}

#[allow(dead_code)]
fn optimize_for<T: XapiRoot>(
    raw: RawValue,
    wire: Option<crate::WireType>,
    limits: Limits,
) -> Result<RawValue> {
    if T::schema().fields.is_empty() {
        Ok(raw)
    } else {
        optimize_raw(raw, wire, limits)
    }
}

fn parse_wire(value: Option<&str>) -> Result<crate::WireType> {
    match value.unwrap_or("STRING").to_ascii_uppercase().as_str() {
        "STRING" | "STRING(L)" => Ok(crate::WireType::String),
        "INT" => Ok(crate::WireType::Int),
        "LONG" => Ok(crate::WireType::Long),
        "FLOAT" => Ok(crate::WireType::Float),
        "DOUBLE" => Ok(crate::WireType::Double),
        "DECIMAL" => Ok(crate::WireType::Decimal),
        "BIGDECIMAL" => Ok(crate::WireType::BigDecimal),
        "DATE" => Ok(crate::WireType::Date),
        "TIME" => Ok(crate::WireType::Time),
        "DATETIME" => Ok(crate::WireType::DateTime),
        "BLOB" => Ok(crate::WireType::Blob),
        "BOOLEAN" | "BOOL" => Ok(crate::WireType::Boolean),
        _ => Ok(crate::WireType::String),
    }
}

fn attr<R: Read>(
    reader: &Reader<std::io::BufReader<R>>,
    start: &BytesStart<'_>,
    key: &[u8],
) -> Result<Option<String>> {
    for item in start.attributes().with_checks(true) {
        let item = item.map_err(|error| {
            Error::from(DecodeError::message(format!(
                "invalid XML attribute: {error}"
            )))
        })?;
        if item.key.local_name().as_ref() == key {
            return item
                .decoded_and_normalized_value(XmlVersion::Implicit1_0, reader.decoder())
                .map(|value| Some(value.into_owned()))
                .map_err(|error| {
                    Error::from(DecodeError::message(format!(
                        "invalid XML attribute value: {error}"
                    )))
                });
        }
    }
    Ok(None)
}

fn is_null<R: Read>(
    reader: &Reader<std::io::BufReader<R>>,
    start: &BytesStart<'_>,
) -> Result<bool> {
    Ok(attr(reader, start, b"null")?.as_deref() == Some("true")
        || attr(reader, start, b"isNull")?.as_deref() == Some("true"))
}

fn append_text<R: Read>(
    value: &mut String,
    _reader: &Reader<std::io::BufReader<R>>,
    text: &BytesText<'_>,
) -> Result<()> {
    let decoded = text
        .decode()
        .map_err(|error| Error::from(DecodeError::message(format!("invalid XML text: {error}"))))?;
    value.push_str(&unescape(&decoded).map_err(|error| {
        Error::from(DecodeError::message(format!("invalid XML entity: {error}")))
    })?);
    Ok(())
}

fn enforce_scalar_limit(value: &str, limits: Limits) -> Result<()> {
    if value.len() > limits.max_scalar_bytes {
        return Err(Error::from(DecodeError::Limit("max_scalar_bytes")));
    }
    Ok(())
}

pub fn encode_stream<T: XapiRoot, W: Write>(output: W, value: &T, format: Format) -> Result<()> {
    let mut sink = XmlSink::new(output, format);
    value.encode_stream(&mut sink)
}

#[allow(dead_code)]
pub fn encode<T: XapiRoot, W: Write>(output: W, value: &T) -> Result<()> {
    let document = value.encode_raw()?;
    let mut writer = Writer::new(output);
    writer
        .write_event(Event::Start(BytesStart::new("Root")))
        .map_err(io_encode)?;
    if !document.parameters.is_empty() {
        writer
            .write_event(Event::Start(BytesStart::new("Parameters")))
            .map_err(io_encode)?;
        for (id, raw) in &document.parameters {
            write_value(&mut writer, "Parameter", id, raw, None)?;
        }
        writer
            .write_event(Event::End(BytesEnd::new("Parameters")))
            .map_err(io_encode)?;
    }
    writer
        .write_event(Event::Start(BytesStart::new("Datasets")))
        .map_err(io_encode)?;
    for dataset in &document.datasets {
        let mut start = BytesStart::new("Dataset");
        start.push_attribute(("id", dataset.id.as_str()));
        writer.write_event(Event::Start(start)).map_err(io_encode)?;
        writer
            .write_event(Event::Start(BytesStart::new("ColumnInfo")))
            .map_err(io_encode)?;
        for (id, wire) in &dataset.columns {
            let type_name = wire.to_string();
            let mut column = BytesStart::new("Column");
            column.push_attribute(("id", id.as_str()));
            column.push_attribute(("type", type_name.as_str()));
            writer
                .write_event(Event::Empty(column))
                .map_err(io_encode)?;
        }
        writer
            .write_event(Event::End(BytesEnd::new("ColumnInfo")))
            .map_err(io_encode)?;
        writer
            .write_event(Event::Start(BytesStart::new("Rows")))
            .map_err(io_encode)?;
        for row in &dataset.rows {
            let mut row_start = BytesStart::new("Row");
            row_start.push_attribute(("type", row_type_name(row.row_type)));
            writer
                .write_event(Event::Start(row_start))
                .map_err(io_encode)?;
            for (id, raw) in &row.values {
                write_value(&mut writer, "Col", id, raw, None)?;
            }
            if let Some(original) = &row.original {
                writer
                    .write_event(Event::Start(BytesStart::new("OrgRow")))
                    .map_err(io_encode)?;
                for (id, raw) in original {
                    write_value(&mut writer, "Col", id, raw, None)?;
                }
                for (id, wire, raw) in &dataset.const_columns {
                    let type_name = wire.to_string();
                    let mut column = BytesStart::new("ConstColumn");
                    column.push_attribute(("id", id.as_str()));
                    column.push_attribute(("type", type_name.as_str()));
                    if let RawValue::Text(value) = raw {
                        column.push_attribute(("value", value.as_str()));
                    }
                    if matches!(raw, RawValue::Null) {
                        column.push_attribute(("null", "true"));
                    }
                    writer
                        .write_event(Event::Empty(column))
                        .map_err(io_encode)?;
                }
                writer
                    .write_event(Event::End(BytesEnd::new("OrgRow")))
                    .map_err(io_encode)?;
            }
            writer
                .write_event(Event::End(BytesEnd::new("Row")))
                .map_err(io_encode)?;
        }
        writer
            .write_event(Event::End(BytesEnd::new("Rows")))
            .map_err(io_encode)?;
        writer
            .write_event(Event::End(BytesEnd::new("Dataset")))
            .map_err(io_encode)?;
    }
    writer
        .write_event(Event::End(BytesEnd::new("Datasets")))
        .map_err(io_encode)?;
    writer
        .write_event(Event::End(BytesEnd::new("Root")))
        .map_err(io_encode)?;
    Ok(())
}

struct XmlSink<W: Write> {
    writer: Writer<W>,
    format: Format,
    parameters_open: bool,
    datasets_open: bool,
}

impl<W: Write> XmlSink<W> {
    fn new(output: W, format: Format) -> Self {
        Self {
            writer: Writer::new(output),
            format,
            parameters_open: false,
            datasets_open: false,
        }
    }
    fn close_parameters(&mut self) -> Result<()> {
        if self.parameters_open {
            self.writer
                .write_event(Event::End(BytesEnd::new("Parameters")))
                .map_err(io_encode)?;
            self.parameters_open = false;
        }
        Ok(())
    }
}

impl<W: Write> XapiEncodeSink for XmlSink<W> {
    fn begin(&mut self) -> Result<()> {
        self.writer
            .write_event(Event::Decl(quick_xml::events::BytesDecl::new(
                "1.0",
                Some("UTF-8"),
                None,
            )))
            .map_err(io_encode)?;
        let (namespace, version_key) = match self.format {
            Format::XPlatformXml4000 => ("http://www.tobesoft.com/platform/Dataset", "ver"),
            Format::NexacroXml4000 => {
                ("http://www.nexacroplatform.com/platform/dataset", "version")
            }
            Format::NexacroJson10 => unreachable!(),
        };
        let mut root = BytesStart::new("Root");
        root.push_attribute(("xmlns", namespace));
        root.push_attribute((version_key, "4000"));
        self.writer
            .write_event(Event::Start(root))
            .map_err(io_encode)?;
        self.writer
            .write_event(Event::Start(BytesStart::new("Parameters")))
            .map_err(io_encode)?;
        self.parameters_open = true;
        Ok(())
    }
    fn parameter(&mut self, id: &str, value: RawValue, wire: crate::WireType) -> Result<()> {
        if !self.parameters_open {
            return Err(Error::from(EncodeError::Message(
                "parameter emitted after dataset".into(),
            )));
        }
        write_value(&mut self.writer, "Parameter", id, &value, Some(wire))
    }
    fn dataset_begin(&mut self, id: &str, columns: &'static [FieldSchema]) -> Result<()> {
        self.close_parameters()?;
        if !self.datasets_open {
            self.writer
                .write_event(Event::Start(BytesStart::new("Datasets")))
                .map_err(io_encode)?;
            self.datasets_open = true;
        }
        let mut dataset = BytesStart::new("Dataset");
        dataset.push_attribute(("id", id));
        self.writer
            .write_event(Event::Start(dataset))
            .map_err(io_encode)?;
        self.writer
            .write_event(Event::Start(BytesStart::new("ColumnInfo")))
            .map_err(io_encode)?;
        for field in columns.iter().filter(|field| field.const_value.is_some()) {
            let type_name = field.wire.to_string();
            let mut column = BytesStart::new("ConstColumn");
            column.push_attribute(("id", field.id));
            if let Some(size) = field.size {
                let size = size.to_string();
                column.push_attribute(("size", size.as_str()));
            }
            column.push_attribute(("type", type_name.as_str()));
            column.push_attribute(("value", field.const_value.unwrap_or_default()));
            self.writer
                .write_event(Event::Empty(column))
                .map_err(io_encode)?;
        }
        for field in columns.iter().filter(|field| field.const_value.is_none()) {
            let type_name = field.wire.to_string();
            let mut column = BytesStart::new("Column");
            column.push_attribute(("id", field.id));
            if let Some(size) = field.size {
                let size = size.to_string();
                column.push_attribute(("size", size.as_str()));
            }
            column.push_attribute(("type", type_name.as_str()));
            self.writer
                .write_event(Event::Empty(column))
                .map_err(io_encode)?;
        }
        self.writer
            .write_event(Event::End(BytesEnd::new("ColumnInfo")))
            .map_err(io_encode)?;
        self.writer
            .write_event(Event::Start(BytesStart::new("Rows")))
            .map_err(io_encode)
    }
    fn row(&mut self, row: RawRow) -> Result<()> {
        let mut start = BytesStart::new("Row");
        start.push_attribute(("type", row_type_name(row.row_type)));
        self.writer
            .write_event(Event::Start(start))
            .map_err(io_encode)?;
        for (id, value) in &row.values {
            write_value(&mut self.writer, "Col", id, value, None)?;
        }
        if let Some(original) = row.original {
            self.writer
                .write_event(Event::Start(BytesStart::new("OrgRow")))
                .map_err(io_encode)?;
            for (id, value) in &original {
                write_value(&mut self.writer, "Col", id, value, None)?;
            }
            self.writer
                .write_event(Event::End(BytesEnd::new("OrgRow")))
                .map_err(io_encode)?;
        }
        self.writer
            .write_event(Event::End(BytesEnd::new("Row")))
            .map_err(io_encode)
    }
    fn dataset_end(&mut self) -> Result<()> {
        self.writer
            .write_event(Event::End(BytesEnd::new("Rows")))
            .map_err(io_encode)?;
        self.writer
            .write_event(Event::End(BytesEnd::new("Dataset")))
            .map_err(io_encode)
    }
    fn end(&mut self) -> Result<()> {
        self.close_parameters()?;
        if self.datasets_open {
            self.writer
                .write_event(Event::End(BytesEnd::new("Datasets")))
                .map_err(io_encode)?;
        } else {
            self.writer
                .write_event(Event::Start(BytesStart::new("Datasets")))
                .map_err(io_encode)?;
            self.writer
                .write_event(Event::End(BytesEnd::new("Datasets")))
                .map_err(io_encode)?;
        }
        self.writer
            .write_event(Event::End(BytesEnd::new("Root")))
            .map_err(io_encode)
    }
}

fn row_type_name(row_type: RowType) -> &'static str {
    match row_type {
        RowType::Normal => "N",
        RowType::Insert => "I",
        RowType::Update => "U",
        RowType::Delete => "D",
        RowType::Original => "N",
    }
}

fn write_value<W: Write>(
    writer: &mut Writer<W>,
    tag: &str,
    id: &str,
    raw: &RawValue,
    wire: Option<crate::WireType>,
) -> Result<()> {
    if matches!(raw, RawValue::Missing) {
        return Ok(());
    }
    let mut start = BytesStart::new(tag);
    start.push_attribute(("id", id));
    if let Some(wire) = wire {
        let wire = wire.to_string();
        start.push_attribute(("type", wire.as_str()));
    }
    if matches!(raw, RawValue::Null) {
        start.push_attribute(("null", "true"));
    }
    writer.write_event(Event::Start(start)).map_err(io_encode)?;
    match raw {
        RawValue::Text(value) => write_text(writer, value)?,
        RawValue::Bytes(value) => write_base64(writer, value)?,
        RawValue::Missing | RawValue::Null => {}
    }
    writer
        .write_event(Event::End(BytesEnd::new(tag)))
        .map_err(io_encode)
}

fn write_base64<W: Write>(writer: &mut Writer<W>, value: &[u8]) -> Result<()> {
    const INPUT_CHUNK: usize = 3 * 1024;
    let mut rest = value;
    let mut encoded = [0_u8; 4096];
    while rest.len() > INPUT_CHUNK {
        let (chunk, next) = rest.split_at(INPUT_CHUNK);
        let length = STANDARD
            .encode_slice(chunk, &mut encoded)
            .map_err(|error| Error::from(EncodeError::Message(error.to_string())))?;
        let text = std::str::from_utf8(&encoded[..length]).map_err(|error| {
            Error::from(EncodeError::Message(format!(
                "invalid base64 output: {error}"
            )))
        })?;
        writer
            .write_event(Event::Text(BytesText::from_escaped(text)))
            .map_err(io_encode)?;
        rest = next;
    }
    let length = STANDARD
        .encode_slice(rest, &mut encoded)
        .map_err(|error| Error::from(EncodeError::Message(error.to_string())))?;
    let text = std::str::from_utf8(&encoded[..length]).map_err(|error| {
        Error::from(EncodeError::Message(format!(
            "invalid base64 output: {error}"
        )))
    })?;
    writer
        .write_event(Event::Text(BytesText::from_escaped(text)))
        .map_err(io_encode)
}

fn write_text<W: Write>(writer: &mut Writer<W>, value: &str) -> Result<()> {
    let mut normal = String::new();
    for character in value.chars() {
        match character {
            '\0' => return Err(EncodeError::InvalidScalar("NUL is not valid in XML".into()).into()),
            '\u{1}'..='\u{8}' | '\u{B}'..='\u{C}' | '\u{E}'..='\u{1F}' => {
                if !normal.is_empty() {
                    writer
                        .write_event(Event::Text(BytesText::new(&normal)))
                        .map_err(io_encode)?;
                    normal.clear();
                }
                let entity = format!("&#{};", character as u32);
                writer
                    .write_event(Event::Text(BytesText::from_escaped(entity)))
                    .map_err(io_encode)?;
            }
            _ => normal.push(character),
        }
    }
    if !normal.is_empty() {
        writer
            .write_event(Event::Text(BytesText::new(&normal)))
            .map_err(io_encode)?;
    }
    Ok(())
}

fn io_encode(error: std::io::Error) -> Error {
    Error::from(EncodeError::Message(error.to_string()))
}
