use std::borrow::Cow;
use std::io::Read;

use quick_xml::{
    escape::unescape,
    events::{BytesStart, BytesText, Event},
    Reader, XmlVersion,
};

use crate::{
    optimize_raw, DecodeError, Error, Limits, RawDataset, RawDocument, RawRow, RawValue, Result,
    RowType, WireType, XapiRoot, XapiRootDecoder,
};

#[allow(dead_code)]
pub fn decode_raw<R: Read>(input: R, limits: Limits) -> Result<RawDocument> {
    let mut reader = Reader::from_reader(std::io::BufReader::new(
        input.take(limits.max_document_bytes.saturating_add(1)),
    ));
    reader.config_mut().trim_text(false);
    let mut buffer = Vec::new();
    let mut document = RawDocument::default();
    let mut rules = DocumentRules::default();
    let mut dataset: Option<RawDataset> = None;
    let mut row: Option<RawRow> = None;
    let mut in_original = false;
    let mut value_target: Option<(String, bool, bool)> = None;
    let mut value = String::new();
    let mut depth = 0usize;
    loop {
        let event = reader.read_event_into(&mut buffer);
        if let Ok(event) = &event {
            rules.check(event)?;
        }
        match event {
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
                        let id = attr(&reader, &start, b"id")?.ok_or_else(|| {
                            Error::from(DecodeError::message("dataset id is required"))
                        })?;
                        dataset = Some(RawDataset {
                            id,
                            columns: Vec::new(),
                            column_sizes: Vec::new(),
                            column_meta: Vec::new(),
                            const_columns: Vec::new(),
                            const_column_sizes: Vec::new(),
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
                        if row.is_none() {
                            return Err(Error::from(DecodeError::message("OrgRow outside Row")));
                        }
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
                            // The drivers fail on a BLOB constant without a value attribute.
                            if wire == WireType::Blob && attr(&reader, &start, b"value")?.is_none()
                            {
                                return Err(Error::from(DecodeError::message(
                                    "BLOB ConstColumn has no value",
                                )));
                            }
                            let value = if is_null(&reader, &start)? {
                                RawValue::Null
                            } else if attr(&reader, &start, b"value")?.is_none() {
                                RawValue::Missing
                            } else {
                                RawValue::Text(attr(&reader, &start, b"value")?.unwrap_or_default())
                            };
                            current.const_columns.push((id, wire, value));
                        }
                    }
                    b"Col" if row.is_some() => {
                        value_target = Some((
                            attr(&reader, &start, b"id")?.unwrap_or_default(),
                            true,
                            is_null(&reader, &start)?,
                        ))
                    }
                    b"Parameter" => {
                        value.clear();
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
                value.push_str(&normalize_line_endings(
                    std::str::from_utf8(&text).map_err(|error| {
                        Error::from(DecodeError::message(format!("invalid XML CDATA: {error}")))
                    })?,
                ));
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
                b"Parameter" => {
                    let id = attr(&reader, &start, b"id")?
                        .filter(|id| !id.is_empty())
                        .ok_or_else(|| {
                            Error::from(DecodeError::message("parameter id is required"))
                        })?;
                    let raw = if is_null(&reader, &start)? {
                        RawValue::Null
                    } else {
                        RawValue::Text(String::new())
                    };
                    document.parameters.push((id, raw));
                }
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
    rules.finish()?;
    Ok(document)
}

pub fn decode_stream<T: XapiRoot, R: Read>(input: R, limits: Limits, strict: bool) -> Result<T> {
    let mut reader = Reader::from_reader(std::io::BufReader::new(
        input.take(limits.max_document_bytes.saturating_add(1)),
    ));
    reader.config_mut().trim_text(false);
    let mut buffer = Vec::new();
    let mut decoder = T::decoder(strict, limits);
    let mut rules = DocumentRules::default();
    let mut dataset = None::<String>;
    let mut dataset_columns = Vec::<(String, crate::WireType)>::new();
    let mut dataset_const_columns = Vec::<(String, crate::WireType, RawValue)>::new();
    let mut row = None::<RawRow>;
    let mut in_original = false;
    let mut value_target = None::<(String, bool, bool)>;
    let mut parameter_wire = None::<crate::WireType>;
    let mut value = String::new();
    let mut depth = 0usize;
    let mut datasets = 0usize;
    let mut columns = 0usize;
    let mut rows = 0usize;
    loop {
        let event = reader.read_event_into(&mut buffer);
        if let Ok(event) = &event {
            rules.check(event)?;
        }
        match event {
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
                        let id = attr(&reader, &start, b"id")?
                            .filter(|id| !id.is_empty())
                            .ok_or_else(|| {
                                Error::from(DecodeError::message("dataset id is required"))
                            })?;
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
                        // The drivers fail on a BLOB constant without a value attribute.
                        if wire == WireType::Blob && attr(&reader, &start, b"value")?.is_none() {
                            return Err(Error::from(DecodeError::message(
                                "BLOB ConstColumn has no value",
                            )));
                        }
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
                        let raw = if is_null(&reader, &start)?
                            || attr(&reader, &start, b"value")?.is_none()
                        {
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
                        if row.is_none() {
                            return Err(Error::from(DecodeError::message("OrgRow outside Row")));
                        }
                        if let Some(current) = row.as_mut() {
                            current.original = Some(Vec::new());
                        }
                        in_original = true;
                    }
                    b"Col" if row.is_some() => {
                        value_target = Some((
                            attr(&reader, &start, b"id")?.unwrap_or_default(),
                            true,
                            is_null(&reader, &start)?,
                        ))
                    }
                    b"Parameter" => {
                        value.clear();
                        let id = attr(&reader, &start, b"id")?
                            .filter(|id| !id.is_empty())
                            .ok_or_else(|| {
                                Error::from(DecodeError::message("parameter id is required"))
                            })?;
                        value_target = Some((id, false, is_null(&reader, &start)?));
                        parameter_wire = attr(&reader, &start, b"type")?
                            .as_deref()
                            .map(|wire| parse_wire(Some(wire)))
                            .transpose()?;
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
                value.push_str(&normalize_line_endings(
                    std::str::from_utf8(&text).map_err(|error| {
                        Error::from(DecodeError::message(format!("invalid XML CDATA: {error}")))
                    })?,
                ));
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
                    b"Parameter" => {
                        let id = attr(&reader, &start, b"id")?
                            .filter(|id| !id.is_empty())
                            .ok_or_else(|| {
                                Error::from(DecodeError::message("parameter id is required"))
                            })?;
                        let declared_wire = attr(&reader, &start, b"type")?;
                        let wire = if let Some(field) = T::schema().fields.iter().find(|field| {
                            field.kind == crate::RootFieldKind::Parameter && field.id == id
                        }) {
                            Some(field.wire)
                        } else {
                            declared_wire
                                .as_deref()
                                .map(|wire| parse_wire(Some(wire)))
                                .transpose()?
                        };
                        let raw = if is_null(&reader, &start)? {
                            RawValue::Null
                        } else {
                            RawValue::Text(String::new())
                        };
                        let raw = optimize_for::<T>(raw, wire, limits)?;
                        decoder.parameter(&id, &raw)?;
                        decoder.parameter_wire(wire)?;
                    }
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
                        let id = attr(&reader, &start, b"id")?.ok_or_else(|| {
                            Error::from(DecodeError::message("dataset id is required"))
                        })?;
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
                        // The drivers fail on a BLOB constant without a value attribute.
                        if wire == WireType::Blob && attr(&reader, &start, b"value")?.is_none() {
                            return Err(Error::from(DecodeError::message(
                                "BLOB ConstColumn has no value",
                            )));
                        }
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
                        } else if attr(&reader, &start, b"value")?.is_none() {
                            RawValue::Missing
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
                                decoder.parameter_wire(parameter_wire.take())?;
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
    rules.finish()?;
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
    value.push_str(
        &unescape(&normalize_line_endings(&decoded)).map_err(|error| {
            Error::from(DecodeError::message(format!("invalid XML entity: {error}")))
        })?,
    );
    Ok(())
}

/// Applies XML line-ending normalization to raw text. Character references are
/// read separately, so `&#13;` stays a carriage return.
fn normalize_line_endings(text: &str) -> Cow<'_, str> {
    if text.contains('\r') {
        Cow::Owned(text.replace("\r\n", "\n").replace('\r', "\n"))
    } else {
        Cow::Borrowed(text)
    }
}

/// Document-level rules the event reader leaves to its caller.
#[derive(Default)]
struct DocumentRules {
    depth: usize,
    closed: bool,
    root_seen: bool,
    leading_whitespace: bool,
    prefixes: Vec<(usize, Vec<u8>)>,
}

impl DocumentRules {
    fn check(&mut self, event: &Event<'_>) -> Result<()> {
        match event {
            Event::Decl(_) if self.leading_whitespace => Err(malformed(
                "XML declaration must be at the start of the document",
            )),
            Event::Start(start) => {
                self.open(start)?;
                self.depth += 1;
                Ok(())
            }
            Event::Empty(start) => {
                self.open(start)?;
                self.close_scope();
                Ok(())
            }
            Event::End(_) => {
                self.depth = self.depth.saturating_sub(1);
                self.close_scope();
                Ok(())
            }
            Event::Text(text) if self.depth == 0 => {
                if !text
                    .iter()
                    .all(|byte| matches!(byte, b' ' | b'\t' | b'\n' | b'\r'))
                {
                    return Err(malformed("text outside the root element"));
                }
                self.leading_whitespace = !self.closed;
                Ok(())
            }
            Event::CData(_) | Event::GeneralRef(_) if self.depth == 0 => {
                Err(malformed("text outside the root element"))
            }
            Event::GeneralRef(reference) => match reference.resolve_char_ref() {
                Ok(Some(character)) if !is_xml_char(character) => Err(malformed(
                    "character reference to a character XML does not allow",
                )),
                _ => Ok(()),
            },
            _ => Ok(()),
        }
    }

    fn open(&mut self, start: &BytesStart<'_>) -> Result<()> {
        if self.closed {
            return Err(malformed("more than one root element"));
        }
        self.root_seen |= start.local_name().as_ref() == b"Root";
        // Most elements have no colon anywhere, so they skip the attribute walk.
        if start.contains(&b':') {
            self.check_prefixes(start)?;
        }
        Ok(())
    }

    fn close_scope(&mut self) {
        while self
            .prefixes
            .last()
            .is_some_and(|(depth, _)| *depth >= self.depth)
        {
            self.prefixes.pop();
        }
        self.closed = self.depth == 0;
    }

    fn check_prefixes(&mut self, start: &BytesStart<'_>) -> Result<()> {
        let mut names = vec![start.name().as_ref().to_vec()];
        for attribute in start.attributes() {
            let attribute =
                attribute.map_err(|error| malformed(format!("invalid XML attribute: {error}")))?;
            let key = attribute.key.as_ref();
            match key.strip_prefix(b"xmlns:") {
                Some(prefix) => self.prefixes.push((self.depth, prefix.to_vec())),
                None => names.push(key.to_vec()),
            }
        }
        for name in &names {
            let Some(colon) = name.iter().position(|byte| *byte == b':') else {
                continue;
            };
            let prefix = &name[..colon];
            if prefix != b"xml" && !self.prefixes.iter().any(|(_, known)| known == prefix) {
                return Err(malformed(format!(
                    "undeclared XML prefix {:?}",
                    String::from_utf8_lossy(prefix)
                )));
            }
        }
        Ok(())
    }

    fn finish(&self) -> Result<()> {
        if self.root_seen {
            Ok(())
        } else {
            Err(malformed("root element must be Root"))
        }
    }
}

fn malformed(message: impl Into<String>) -> Error {
    Error::from(DecodeError::message(format!(
        "malformed XML: {}",
        message.into()
    )))
}

fn is_xml_char(character: char) -> bool {
    matches!(character, '\t' | '\n' | '\r' | '\u{20}'..='\u{d7ff}' | '\u{e000}'..='\u{fffd}' | '\u{10000}'..)
}

fn enforce_scalar_limit(value: &str, limits: Limits) -> Result<()> {
    if value.len() > limits.max_scalar_bytes {
        return Err(Error::from(DecodeError::Limit("max_scalar_bytes")));
    }
    Ok(())
}
