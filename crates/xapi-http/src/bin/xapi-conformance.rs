use std::{
    collections::HashMap,
    io::{self, BufRead, Write},
};

use base64::{engine::general_purpose::STANDARD, Engine};
use quick_xml::{events::Event, Reader};
use serde_json::{json, Map, Value};
use xapi::{dynamic, Format, Limits, RawDataset, RawDocument, RawRow, RawValue, RowType, WireType};

#[derive(Default)]
struct Metadata {
    root: Option<Value>,
    root_nested: bool,
    parameters: HashMap<String, ParameterMeta>,
    datasets: HashMap<String, DatasetMeta>,
    nested_dataset_order: Vec<String>,
    direct_dataset_order: Vec<String>,
}

#[derive(Default)]
struct ParameterMeta {
    wire: Option<String>,
    value_form: Option<String>,
}

#[derive(Default)]
struct DatasetMeta {
    columns: HashMap<String, ColumnMeta>,
    const_columns: HashMap<String, ColumnMeta>,
}

#[derive(Default)]
struct ColumnMeta {
    wire: Option<String>,
    size: Option<String>,
    encoding: Option<String>,
}

fn main() {
    if std::env::args().nth(1).as_deref() == Some("stdio") {
        run_stdio();
    } else {
        eprintln!("usage: xapi-conformance stdio");
        std::process::exit(2);
    }
}

fn run_stdio() {
    let stdin = io::stdin();
    let mut output = io::BufWriter::new(io::stdout());
    for line in stdin.lock().lines() {
        let response = match line {
            Ok(line) => handle(&line),
            Err(error) => error_response("invalid-request", "", error.to_string()),
        };
        if serde_json::to_writer(&mut output, &response).is_err() {
            break;
        }
        if writeln!(output).is_err() {
            break;
        }
        let _ = output.flush();
    }
}

fn handle(line: &str) -> Value {
    let request: Value = match serde_json::from_str(line) {
        Ok(value) => value,
        Err(error) => return error_response("invalid-request", "", error.to_string()),
    };
    let operation = request
        .get("operation")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let profile = request
        .get("profile")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let format = match profile_format(profile) {
        Some(format) => format,
        None => return error_response("unsupported-profile", "", profile.to_owned()),
    };
    let options = request.get("options").unwrap_or(&Value::Null);
    let strict = options
        .get("strict")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let limits = limits(options);
    match operation {
        "decode" => match input_bytes(&request) {
            Ok(bytes) => {
                if bytes.len() as u64 > limits.max_document_bytes {
                    return error_response(
                        "limit-exceeded",
                        "input.data",
                        "payload exceeds configured limit".into(),
                    );
                }
                if matches!(format, Format::XPlatformXml4000 | Format::NexacroXml4000)
                    && !xml_attributes_valid(&bytes)
                {
                    return error_response(
                        "malformed-input",
                        "wire",
                        "invalid XML attributes".into(),
                    );
                }
                if matches!(format, Format::XPlatformXml4000 | Format::NexacroXml4000)
                    && strict
                    && !valid_xml_structure(&bytes)
                {
                    return error_response(
                        "malformed-input",
                        "wire",
                        "invalid XML structure".into(),
                    );
                }
                let metadata = metadata(&bytes, format);
                let standalone_original = json_has_standalone_original(&bytes, format);
                match dynamic::decode(bytes.as_slice(), format, limits, strict) {
                    Ok(document) if has_unknown_declared_columns(&document, &metadata) => {
                        error_response("malformed-input", "wire", "column is not declared".into())
                    }
                    Ok(document) => {
                        json!({"ok": true, "value": canonical(&document, &metadata, standalone_original)})
                    }
                    Err(error) => error_value(error),
                }
            }
            Err(error) => error,
        },
        "encode" => match request.get("value") {
            Some(value) => match validate_encode(value, limits) {
                Err(error) => error,
                Ok(()) => match canonical_document(value) {
                    Some(document) => match encode_result(&document, format) {
                        Ok(output) => {
                            json!({"ok": true, "value": request["value"], "output": output})
                        }
                        Err(error) => error_value(error),
                    },
                    None => error_response("invalid-request", "", "value is required".into()),
                },
            },
            None => error_response("invalid-request", "", "value is required".into()),
        },
        "roundtrip" => match input_bytes(&request) {
            Ok(bytes) => {
                if bytes.len() as u64 > limits.max_document_bytes {
                    return error_response(
                        "limit-exceeded",
                        "input.data",
                        "payload exceeds configured limit".into(),
                    );
                }
                let metadata = metadata(&bytes, format);
                let standalone_original = json_has_standalone_original(&bytes, format);
                match dynamic::decode(bytes.as_slice(), format, limits, strict) {
                    Ok(document) => match encode_result(&document, format) {
                        Ok(output) => json!({
                            "ok": true,
                            "value": canonical(&document, &metadata, standalone_original),
                            "output": output
                        }),
                        Err(error) => error_value(error),
                    },
                    Err(error) => error_value(error),
                }
            }
            Err(error) => error,
        },
        _ => error_response("unsupported-operation", "", operation.to_owned()),
    }
}

fn profile_format(profile: &str) -> Option<Format> {
    Some(match profile {
        "xplatform-xml-4000" => Format::XPlatformXml4000,
        "nexacro-xml-4000" => Format::NexacroXml4000,
        "nexacro-json-1.0" => Format::NexacroJson10,
        _ => return None,
    })
}

fn input_bytes(request: &Value) -> Result<Vec<u8>, Value> {
    let input = request
        .get("input")
        .and_then(Value::as_object)
        .ok_or_else(|| error_response("invalid-request", "", "input is required".into()))?;
    let encoded = input
        .get("data")
        .and_then(Value::as_str)
        .ok_or_else(|| error_response("invalid-request", "", "input.data is required".into()))?;
    let mut encoded = encoded.to_owned();
    let ignore = request
        .get("options")
        .and_then(|options| options.get("base64Whitespace"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if ignore {
        encoded.retain(|character| !character.is_ascii_whitespace());
    }
    STANDARD
        .decode(encoded)
        .map_err(|error| error_response("malformed-input", "input.data", error.to_string()))
}

fn limits(options: &Value) -> Limits {
    let mut limits = Limits::default();
    let Some(values) = options.get("limits").and_then(Value::as_object) else {
        return limits;
    };
    if let Some(value) = values.get("payloadBytes").and_then(Value::as_u64) {
        limits.max_document_bytes = value;
    }
    if let Some(value) = values.get("datasets").and_then(Value::as_u64) {
        limits.max_datasets = value as usize;
    }
    if let Some(value) = values.get("rows").and_then(Value::as_u64) {
        limits.max_rows = value as usize;
    }
    if let Some(value) = values.get("columns").and_then(Value::as_u64) {
        limits.max_columns = value as usize;
    }
    if let Some(value) = values.get("scalarBytes").and_then(Value::as_u64) {
        limits.max_scalar_bytes = value as usize;
    }
    if let Some(value) = values.get("blobBytes").and_then(Value::as_u64) {
        limits.max_blob_bytes = value as usize;
    }
    if let Some(value) = values.get("depth").and_then(Value::as_u64) {
        limits.max_depth = value as usize;
    }
    limits
}

fn metadata(bytes: &[u8], format: Format) -> Metadata {
    match format {
        Format::NexacroJson10 => json_metadata(bytes),
        Format::XPlatformXml4000 | Format::NexacroXml4000 => xml_metadata(bytes),
    }
}

fn json_metadata(bytes: &[u8]) -> Metadata {
    let Ok(root) = serde_json::from_slice::<Value>(bytes) else {
        return Metadata::default();
    };
    let mut result = Metadata::default();
    if let Some(version) = root.get("version").and_then(Value::as_str) {
        result.root = Some(json!({"version": version}));
    }
    if let Some(parameters) = root.get("Parameters").or_else(|| root.get("parameters")) {
        if let Some(parameters) = parameters.as_array() {
            for parameter in parameters {
                let Some(object) = parameter.as_object() else {
                    continue;
                };
                let Some(id) = object.get("id").and_then(Value::as_str) else {
                    continue;
                };
                result.parameters.insert(
                    id.to_owned(),
                    ParameterMeta {
                        wire: object
                            .get("type")
                            .and_then(Value::as_str)
                            .map(|value| value.to_ascii_uppercase()),
                        value_form: None,
                    },
                );
            }
        }
    }
    if let Some(datasets) = root.get("Datasets").or_else(|| root.get("datasets")) {
        if let Some(datasets) = datasets.as_array() {
            for dataset in datasets {
                let Some(object) = dataset.as_object() else {
                    continue;
                };
                let Some(id) = object.get("id").and_then(Value::as_str) else {
                    continue;
                };
                let mut meta = DatasetMeta::default();
                if let Some(info) = object.get("ColumnInfo").or_else(|| object.get("columns")) {
                    if let Some(columns) = info.as_array() {
                        for column in columns {
                            add_json_column(&mut meta.columns, column);
                        }
                    } else if let Some(info) = info.as_object() {
                        if let Some(columns) = info.get("Column").or_else(|| info.get("columns")) {
                            if let Some(columns) = columns.as_array() {
                                for column in columns {
                                    add_json_column(&mut meta.columns, column);
                                }
                            }
                        }
                        if let Some(columns) =
                            info.get("ConstColumn").or_else(|| info.get("constColumns"))
                        {
                            if let Some(columns) = columns.as_array() {
                                for column in columns {
                                    add_json_column(&mut meta.const_columns, column);
                                }
                            }
                        }
                    }
                }
                result.datasets.insert(id.to_owned(), meta);
            }
        }
    }
    result
}

fn add_json_column(target: &mut HashMap<String, ColumnMeta>, value: &Value) {
    let Some(object) = value.as_object() else {
        return;
    };
    let Some(id) = object.get("id").and_then(Value::as_str) else {
        return;
    };
    target.insert(
        id.to_owned(),
        ColumnMeta {
            wire: object
                .get("type")
                .and_then(Value::as_str)
                .map(|value| value.to_ascii_uppercase()),
            size: object.get("size").map(value_text),
            encoding: object.get("encoding").map(value_text),
        },
    );
}

fn value_text(value: &Value) -> String {
    value
        .as_str()
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| value.to_string())
}

#[allow(clippy::while_let_loop)]
fn xml_metadata(bytes: &[u8]) -> Metadata {
    let mut result = Metadata {
        root_nested: true,
        ..Metadata::default()
    };
    let mut reader = Reader::from_reader(bytes);
    reader.config_mut().trim_text(true);
    let mut buffer = Vec::new();
    let mut dataset = None::<String>;
    let mut in_datasets = false;
    loop {
        let event = match reader.read_event_into(&mut buffer) {
            Ok(event) => event,
            Err(_) => break,
        };
        match event {
            Event::Start(start) | Event::Empty(start) => {
                let name = start.local_name();
                match name.as_ref() {
                    b"Root" => {
                        let namespace = xml_attr(&start, b"xmlns");
                        let version = xml_attr(&start, b"version");
                        let ver = xml_attr(&start, b"ver");
                        if namespace.is_some() || version.is_some() || ver.is_some() {
                            let mut root = Map::new();
                            if let Some(namespace) = namespace {
                                root.insert("namespace".into(), Value::String(namespace));
                            }
                            if let Some(version) = version {
                                root.insert("version".into(), Value::String(version));
                            }
                            if let Some(ver) = ver {
                                root.insert("ver".into(), Value::String(ver));
                            }
                            result.root = Some(Value::Object(root));
                        }
                    }
                    b"Dataset" => {
                        dataset = Some(xml_attr(&start, b"id").unwrap_or_default());
                        if let Some(id) = &dataset {
                            result.datasets.entry(id.clone()).or_default();
                            if in_datasets {
                                result.nested_dataset_order.push(id.clone());
                            } else {
                                result.direct_dataset_order.push(id.clone());
                            }
                        }
                    }
                    b"Datasets" => in_datasets = true,
                    b"Parameter" => {
                        if let Some(id) = Some(xml_attr(&start, b"id").unwrap_or_default()) {
                            result.parameters.insert(
                                id,
                                ParameterMeta {
                                    wire: Some(
                                        xml_attr(&start, b"type")
                                            .unwrap_or_else(|| "STRING".into())
                                            .to_ascii_uppercase(),
                                    ),
                                    value_form: Some(if xml_attr(&start, b"value").is_some() {
                                        "attribute".into()
                                    } else {
                                        "text".into()
                                    }),
                                },
                            );
                        }
                    }
                    b"Column" | b"ConstColumn" => {
                        if let (Some(dataset), Some(id)) =
                            (dataset.as_ref(), xml_attr(&start, b"id"))
                        {
                            let column = ColumnMeta {
                                wire: xml_attr(&start, b"type")
                                    .map(|value| value.to_ascii_uppercase())
                                    .or_else(|| {
                                        let is_js = result
                                            .root
                                            .as_ref()
                                            .and_then(|root| root.get("namespace"))
                                            .and_then(Value::as_str)
                                            .is_some_and(|value| value.contains("tobesoft"));
                                        Some(if is_js { "".into() } else { "STRING".into() })
                                    }),
                                size: xml_attr(&start, b"size"),
                                encoding: xml_attr(&start, b"encoding")
                                    .or_else(|| xml_attr(&start, b"enc")),
                            };
                            if name.as_ref() == b"ConstColumn" {
                                result
                                    .datasets
                                    .entry(dataset.clone())
                                    .or_default()
                                    .const_columns
                                    .insert(id, column);
                            } else {
                                result
                                    .datasets
                                    .entry(dataset.clone())
                                    .or_default()
                                    .columns
                                    .insert(id, column);
                            }
                        }
                    }
                    _ => {}
                }
            }
            Event::End(end) if end.local_name().as_ref() == b"Dataset" => dataset = None,
            Event::End(end) if end.local_name().as_ref() == b"Datasets" => in_datasets = false,
            Event::Eof => break,
            _ => {}
        }
        buffer.clear();
    }
    result
}

fn xml_attr(start: &quick_xml::events::BytesStart<'_>, key: &[u8]) -> Option<String> {
    start
        .attributes()
        .with_checks(true)
        .filter_map(Result::ok)
        .find(|attribute| attribute.key.local_name().as_ref() == key)
        .and_then(|attribute| String::from_utf8(attribute.value.into_owned()).ok())
}

#[allow(clippy::collapsible_match, clippy::while_let_loop)]
fn valid_xml_structure(bytes: &[u8]) -> bool {
    let mut reader = Reader::from_reader(bytes);
    let mut buffer = Vec::new();
    let allowed = [
        b"Root".as_slice(),
        b"Parameters",
        b"Parameter",
        b"Datasets",
        b"Dataset",
        b"ColumnInfo",
        b"Column",
        b"ConstColumn",
        b"Rows",
        b"Row",
        b"OrgRow",
        b"Col",
    ];
    loop {
        match reader.read_event_into(&mut buffer) {
            Ok(Event::Start(start)) | Ok(Event::Empty(start)) => {
                if !allowed
                    .iter()
                    .any(|name| *name == start.local_name().as_ref())
                    || start
                        .attributes()
                        .with_checks(true)
                        .any(|item| item.is_err())
                {
                    return false;
                }
            }
            Ok(Event::Eof) => return true,
            Err(_) => return false,
            _ => {}
        }
        buffer.clear();
    }
}

#[allow(clippy::collapsible_match)]
fn xml_attributes_valid(bytes: &[u8]) -> bool {
    let mut reader = Reader::from_reader(bytes);
    let mut buffer = Vec::new();
    loop {
        match reader.read_event_into(&mut buffer) {
            Ok(Event::Start(start)) | Ok(Event::Empty(start)) => {
                if start
                    .attributes()
                    .with_checks(true)
                    .any(|item| item.is_err())
                {
                    return false;
                }
            }
            Ok(Event::Eof) => return true,
            Err(_) => return false,
            _ => {}
        }
        buffer.clear();
    }
}

fn has_unknown_declared_columns(document: &RawDocument, metadata: &Metadata) -> bool {
    document.datasets.iter().any(|dataset| {
        let Some(meta) = metadata.datasets.get(&dataset.id) else {
            return false;
        };
        let declared = meta
            .columns
            .keys()
            .chain(meta.const_columns.keys())
            .collect::<std::collections::HashSet<_>>();
        dataset.rows.iter().any(|row| {
            row.values
                .iter()
                .chain(row.original.iter().flatten())
                .any(|(id, _)| !declared.contains(id))
        })
    })
}

fn validate_encode(value: &Value, limits: Limits) -> Result<(), Value> {
    if let Some(parameters) = value.get("parameters").and_then(Value::as_array) {
        for (index, parameter) in parameters.iter().enumerate() {
            if parameter
                .get("lexical")
                .and_then(Value::as_str)
                .is_some_and(|lexical| lexical.len() > limits.max_scalar_bytes)
            {
                return Err(error_response(
                    "limit-exceeded",
                    &format!("value.parameters[{index}]"),
                    "scalar limit exceeded".into(),
                ));
            }
        }
    }
    let datasets = value
        .get("datasets")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            error_response(
                "invalid-request",
                "value.datasets",
                "datasets is required".into(),
            )
        })?;
    if datasets.len() > limits.max_datasets {
        return Err(error_response(
            "limit-exceeded",
            "value.datasets",
            "dataset limit exceeded".into(),
        ));
    }
    for (dataset_index, dataset) in datasets.iter().enumerate() {
        let object = dataset.as_object().ok_or_else(|| {
            error_response(
                "invalid-request",
                "value.datasets",
                "dataset must be an object".into(),
            )
        })?;
        let columns = object.get("columns").and_then(Value::as_array).unwrap();
        let constants = object
            .get("constColumns")
            .and_then(Value::as_array)
            .unwrap();
        if columns.len() + constants.len() > limits.max_columns {
            return Err(error_response(
                "limit-exceeded",
                &format!("value.datasets[{dataset_index}].columns"),
                "column limit exceeded".into(),
            ));
        }
        let rows = object.get("rows").and_then(Value::as_array).unwrap();
        if rows.len() > limits.max_rows {
            return Err(error_response(
                "limit-exceeded",
                &format!("value.datasets[{dataset_index}].rows"),
                "row limit exceeded".into(),
            ));
        }
        let blob_columns = columns
            .iter()
            .filter_map(|column| {
                Some((
                    column.get("id")?.as_str()?.to_owned(),
                    column.get("type")?.as_str()?.eq_ignore_ascii_case("BLOB"),
                ))
            })
            .collect::<std::collections::HashMap<_, _>>();
        for (row_index, row) in rows.iter().enumerate() {
            validate_cells(
                row.get("values"),
                dataset_index,
                row_index,
                limits,
                &blob_columns,
            )?;
            if let Some(original) = row.get("orgRow").filter(|value| !value.is_null()) {
                if limits.max_depth < 2 {
                    return Err(error_response(
                        "limit-exceeded",
                        &format!("value.datasets[{dataset_index}].rows[{row_index}].orgRow"),
                        "depth limit exceeded".into(),
                    ));
                }
                validate_cells(
                    original.get("values"),
                    dataset_index,
                    row_index,
                    limits,
                    &blob_columns,
                )?;
            }
        }
    }
    Ok(())
}

fn validate_cells(
    values: Option<&Value>,
    dataset: usize,
    row: usize,
    limits: Limits,
    blob_columns: &std::collections::HashMap<String, bool>,
) -> Result<(), Value> {
    let Some(values) = values.and_then(Value::as_object) else {
        return Ok(());
    };
    for (id, cell) in values {
        let Some(lexical) = cell.get("lexical").and_then(Value::as_str) else {
            continue;
        };
        let path = format!("value.datasets[{dataset}].rows[{row}].values.{id}");
        if lexical.len() > limits.max_scalar_bytes {
            return Err(error_response(
                "limit-exceeded",
                &path,
                "scalar limit exceeded".into(),
            ));
        }
        if let Ok(decoded) = STANDARD.decode(lexical) {
            if decoded.len() > limits.max_blob_bytes
                && blob_columns.get(id).copied().unwrap_or(false)
            {
                return Err(error_response(
                    "limit-exceeded",
                    &path,
                    "blob limit exceeded".into(),
                ));
            }
        }
    }
    Ok(())
}

fn json_has_standalone_original(bytes: &[u8], format: Format) -> bool {
    if format != Format::NexacroJson10 {
        return false;
    }
    let Ok(root) = serde_json::from_slice::<Value>(bytes) else {
        return false;
    };
    root.get("Datasets")
        .or_else(|| root.get("datasets"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|dataset| dataset.get("Rows").or_else(|| dataset.get("rows")))
        .filter_map(Value::as_array)
        .flatten()
        .any(|row| row.get("_RowType_").and_then(Value::as_str) == Some("O"))
}

fn canonical(document: &RawDocument, metadata: &Metadata, standalone_original: bool) -> Value {
    let parameters = document
        .parameters
        .iter()
        .enumerate()
        .map(|(index, (id, value))| {
            let mut result = cell(value);
            let object = result.as_object_mut().unwrap();
            object.insert("id".into(), Value::String(id.clone()));
            object.insert(
                "type".into(),
                Value::String(
                    metadata
                        .parameters
                        .get(id)
                        .and_then(|meta| meta.wire.clone())
                        .unwrap_or_else(|| infer_wire(value).to_string()),
                ),
            );
            if index > 0 {
                object.insert("index".into(), json!(index));
            }
            if let Some(value_form) = metadata
                .parameters
                .get(id)
                .and_then(|meta| meta.value_form.clone())
            {
                object.insert("wire".into(), json!({"valueForm": value_form}));
            }
            Value::Object(object.clone())
        })
        .collect::<Vec<_>>();
    let mut ordered_datasets = document.datasets.iter().collect::<Vec<_>>();
    if !metadata.nested_dataset_order.is_empty() && !metadata.direct_dataset_order.is_empty() {
        ordered_datasets.sort_by_key(|dataset| {
            metadata
                .nested_dataset_order
                .iter()
                .chain(metadata.direct_dataset_order.iter())
                .position(|id| id == &dataset.id)
                .unwrap_or(usize::MAX)
        });
    }
    let datasets = ordered_datasets
        .into_iter()
        .map(|dataset| {
            let columns = dataset
                .columns
                .iter()
                .enumerate()
                .map(|(index, (id, wire))| {
                    {
                        let mut column = json!({"id": id, "type": wire.to_string(), "index": index});
                        if let Some(meta) = metadata
                            .datasets
                            .get(&dataset.id)
                            .and_then(|dataset| dataset.columns.get(id))
                        {
                            if let Some(size) = &meta.size {
                                column["size"] = Value::String(size.clone());
                            }
                            if let Some(wire) = &meta.wire {
                                column["type"] = Value::String(wire.clone());
                            }
                            if let Some(encoding) = &meta.encoding {
                                column["encoding"] = Value::String(encoding.clone());
                            }
                        }
                        column
                    }
                })
                .collect::<Vec<_>>();
            let const_columns = dataset
                .const_columns
                .iter()
                .map(|(id, wire, value)| {
                    {
                        let mut column = json!({"id": id, "type": wire.to_string(), "value": cell(value)});
                        if let Some(meta) = metadata
                            .datasets
                            .get(&dataset.id)
                            .and_then(|dataset| dataset.const_columns.get(id))
                        {
                            if let Some(size) = &meta.size {
                                column["size"] = Value::String(size.clone());
                            }
                            if let Some(wire) = &meta.wire {
                                column["type"] = Value::String(wire.clone());
                            }
                            if let Some(encoding) = &meta.encoding {
                                column["encoding"] = Value::String(encoding.clone());
                            }
                        }
                        column
                    }
                })
                .collect::<Vec<_>>();
            let mut rows = Vec::new();
            for row in &dataset.rows {
                rows.push(canonical_row(row, &dataset.columns));
                if standalone_original && row.original.is_some() {
                    if let Some(original) = row.original.as_ref() {
                        rows.push(json!({
                            "type": "O",
                            "orgRow": Value::Null,
                            "values": canonical_values(original, &dataset.columns)
                        }));
                    }
                }
            }
            json!({"id": dataset.id, "columns": columns, "constColumns": const_columns, "rows": rows})
        })
        .collect::<Vec<_>>();
    let mut result = json!({"parameters": parameters, "datasets": datasets});
    if let Some(root) = &metadata.root {
        result["wire"] = if metadata.root_nested {
            json!({"root": root})
        } else {
            root.clone()
        };
    }
    result
}

fn canonical_row(row: &RawRow, columns: &[(String, WireType)]) -> Value {
    let original = row.original.as_ref().map(|values| {
        json!({
            "type": "O",
            "orgRow": Value::Null,
            "values": canonical_values(values, columns)
        })
    });
    json!({"type": row_type(row.row_type), "orgRow": original, "values": canonical_values(&row.values, columns)})
}

fn canonical_values(
    values: &[(String, RawValue)],
    columns: &[(String, WireType)],
) -> Map<String, Value> {
    let mut result = columns
        .iter()
        .map(|(id, _)| (id.clone(), cell(&RawValue::Missing)))
        .collect::<Map<_, _>>();
    for (id, value) in values {
        result.insert(id.clone(), cell(value));
    }
    result
}

fn cell(value: &RawValue) -> Value {
    match value {
        RawValue::Missing => json!({"state": "missing"}),
        RawValue::Null => json!({"state": "null"}),
        RawValue::Text(value) => {
            if value.is_empty() {
                json!({"state": "empty"})
            } else {
                json!({"state": "value", "lexical": value})
            }
        }
        RawValue::Bytes(value) => json!({"state": "value", "lexical": STANDARD.encode(value)}),
    }
}

fn infer_wire(value: &RawValue) -> WireType {
    match value {
        RawValue::Text(value) if value.parse::<i64>().is_ok() => WireType::Int,
        RawValue::Text(value) if value == "true" || value == "false" => WireType::Boolean,
        RawValue::Bytes(_) => WireType::Blob,
        _ => WireType::String,
    }
}

fn row_type(row_type: RowType) -> &'static str {
    match row_type {
        RowType::Normal => "N",
        RowType::Insert => "I",
        RowType::Update => "U",
        RowType::Delete => "D",
        RowType::Original => "O",
    }
}

fn canonical_document(value: &Value) -> Option<RawDocument> {
    let object = value.as_object()?;
    let parameters = object
        .get("parameters")?
        .as_array()?
        .iter()
        .map(|parameter| {
            let parameter = parameter.as_object()?;
            Some((
                parameter.get("id")?.as_str()?.to_owned(),
                raw_cell(parameter)?,
            ))
        })
        .collect::<Option<Vec<_>>>()?;
    let datasets = object
        .get("datasets")?
        .as_array()?
        .iter()
        .map(raw_dataset)
        .collect::<Option<Vec<_>>>()?;
    Some(RawDocument {
        parameters,
        datasets,
    })
}

fn raw_dataset(value: &Value) -> Option<RawDataset> {
    let object = value.as_object()?;
    let columns = object
        .get("columns")?
        .as_array()?
        .iter()
        .map(|column| {
            let column = column.as_object()?;
            Some((
                column.get("id")?.as_str()?.to_owned(),
                parse_wire(column.get("type")?.as_str()?)?,
            ))
        })
        .collect::<Option<Vec<_>>>()?;
    let const_columns = object
        .get("constColumns")
        .and_then(Value::as_array)
        .map_or(&[][..], Vec::as_slice)
        .iter()
        .map(|column| {
            let column = column.as_object()?;
            Some((
                column.get("id")?.as_str()?.to_owned(),
                parse_wire(column.get("type")?.as_str()?)?,
                raw_cell(column.get("value")?.as_object()?)?,
            ))
        })
        .collect::<Option<Vec<_>>>()?;
    let rows = object
        .get("rows")?
        .as_array()?
        .iter()
        .map(raw_row)
        .collect::<Option<Vec<_>>>()?;
    Some(RawDataset {
        id: object.get("id")?.as_str()?.to_owned(),
        columns,
        const_columns,
        rows,
    })
}

fn raw_row(value: &Value) -> Option<RawRow> {
    let object = value.as_object()?;
    let values = object.get("values")?.as_object()?;
    let original = match object.get("orgRow").and_then(Value::as_object) {
        Some(row) => Some(raw_values(row.get("values")?.as_object()?)),
        None => None,
    };
    Some(RawRow {
        row_type: parse_row_type(object.get("type")?.as_str()?)?,
        values: raw_values(values),
        original,
    })
}

fn raw_values(values: &Map<String, Value>) -> Vec<(String, RawValue)> {
    values
        .iter()
        .filter_map(|(id, value)| {
            value
                .as_object()
                .and_then(|cell| Some((id.clone(), raw_cell(cell)?)))
        })
        .collect()
}

fn raw_cell(cell: &Map<String, Value>) -> Option<RawValue> {
    match cell.get("state")?.as_str()? {
        "missing" => Some(RawValue::Missing),
        "null" => Some(RawValue::Null),
        "empty" => Some(RawValue::Text(String::new())),
        "value" => Some(RawValue::Text(cell.get("lexical")?.as_str()?.to_owned())),
        _ => None,
    }
}

fn parse_wire(value: &str) -> Option<WireType> {
    Some(match value.to_ascii_uppercase().as_str() {
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
        "BLOB" => WireType::Blob,
        "BOOLEAN" => WireType::Boolean,
        _ => return None,
    })
}

fn parse_row_type(value: &str) -> Option<RowType> {
    Some(match value {
        "N" => RowType::Normal,
        "I" => RowType::Insert,
        "U" => RowType::Update,
        "D" => RowType::Delete,
        "O" => RowType::Original,
        _ => return None,
    })
}

fn encode_result(document: &RawDocument, format: Format) -> Result<Value, xapi::Error> {
    let mut output = Vec::new();
    dynamic::encode(&mut output, document, format)?;
    Ok(json!({"encoding": "base64", "data": STANDARD.encode(output)}))
}

fn error_value(error: xapi::Error) -> Value {
    match error {
        xapi::Error::Decode { path, source } => {
            let class = if matches!(source, xapi::DecodeError::Limit(_)) {
                "limit-exceeded"
            } else {
                "malformed-input"
            };
            let path = path.to_string();
            let path = if path.is_empty() {
                if matches!(source, xapi::DecodeError::Limit(_)) {
                    "input.data"
                } else {
                    "wire"
                }
            } else {
                path.as_str()
            };
            error_response(class, path, source.to_string())
        }
        xapi::Error::Encode { path, source } => {
            error_response("invalid-value", &path.to_string(), source.to_string())
        }
        xapi::Error::Io(error) => error_response("internal", "", error.to_string()),
    }
}

fn error_response(class: &str, path: &str, message: String) -> Value {
    json!({"ok": false, "error": {"class": class, "path": path, "message": message}})
}
