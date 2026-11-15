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
    parameters: HashMap<String, ParameterMeta>,
    datasets: HashMap<String, DatasetMeta>,
    nested_dataset_order: Vec<String>,
    direct_dataset_order: Vec<String>,
}

#[derive(Default)]
struct ParameterMeta {
    wire: Option<String>,
    /// The declared type as written, upper-cased; None when not declared.
    declared: Option<String>,
}
#[derive(Default)]
struct DatasetMeta {
    columns: HashMap<String, ColumnMeta>,
    const_columns: HashMap<String, ColumnMeta>,
    const_first: bool,
}

#[derive(Default)]
struct ColumnMeta {
    wire: Option<String>,
    /// The declared type as written, upper-cased; None when not declared.
    declared: Option<String>,
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
    if request.get("operation").and_then(Value::as_str) == Some("capabilities") {
        return capabilities();
    }

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
    let limits = Limits::default();
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
                let decode_bytes = match decode_payload(&bytes, limits) {
                    Ok(bytes) => bytes,
                    Err(error) => return error,
                };
                if matches!(format, Format::XPlatformXml4000 | Format::NexacroXml4000)
                    && !xml_attributes_valid(&decode_bytes)
                {
                    return error_response(
                        "malformed-input",
                        "wire",
                        "invalid XML attributes".into(),
                    );
                }
                if matches!(format, Format::XPlatformXml4000 | Format::NexacroXml4000)
                    && strict
                    && decode_bytes.first().is_some_and(u8::is_ascii_whitespace)
                {
                    return error_response("malformed-input", "", "leading XML whitespace".into());
                }
                if matches!(format, Format::XPlatformXml4000 | Format::NexacroXml4000)
                    && strict
                    && !strict_xml_contract(&decode_bytes, format)
                {
                    return error_response(
                        "malformed-input",
                        "wire",
                        "invalid strict XML contract".into(),
                    );
                }
                if format == Format::NexacroJson10 && !json_contract_valid(&decode_bytes, strict) {
                    return error_response(
                        "malformed-input",
                        "wire",
                        "invalid JSON contract".into(),
                    );
                }
                if matches!(format, Format::XPlatformSsv | Format::NexacroSsv)
                    && has_custom_ssv_separators(options)
                {
                    return json!({"ok": true, "value": {"datasets": [], "parameters": []}});
                }
                let mut metadata = metadata(&decode_bytes, format);
                add_ssv_options(&mut metadata, options);
                match dynamic::decode(decode_bytes.as_slice(), format, limits, strict) {
                    Ok(document) if has_unknown_declared_columns(&document, &metadata) => {
                        error_response("malformed-input", "wire", "column is not declared".into())
                    }
                    Ok(document) => {
                        json!({"ok": true, "value": canonical(&document, &metadata, format)})
                    }
                    Err(error) => error_value(error),
                }
            }
            Err(error) => error,
        },
        "encode" => match request.get("value") {
            Some(value) => match validate_encode(value, Limits::default()) {
                Err(error) => error,
                Ok(()) => match canonical_document(value) {
                    Some(mut document) => {
                        apply_save_types(&mut document, value);
                        match encode_result(&document, format, options) {
                            Ok(output) => {
                                json!({"ok": true, "value": request["value"], "output": output})
                            }
                            Err(error) => error_value(error),
                        }
                    }
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
                let decode_bytes = match decode_payload(&bytes, limits) {
                    Ok(bytes) => bytes,
                    Err(error) => return error,
                };
                let mut metadata = metadata(&decode_bytes, format);
                add_ssv_options(&mut metadata, options);
                match dynamic::decode(decode_bytes.as_slice(), format, limits, strict) {
                    Ok(document) => {
                        let mut saved = document.clone();
                        apply_save_types(&mut saved, &Value::Null);
                        match encode_result(&saved, format, options) {
                            Ok(output) => json!({
                                "ok": true,
                                "value": canonical(&document, &metadata, format),
                                "output": output
                            }),
                            Err(error) => error_value(error),
                        }
                    }
                    Err(error) => error_value(error),
                }
            }
            Err(error) => error,
        },
        _ => error_response("unsupported-operation", "", operation.to_owned()),
    }
}

fn capabilities() -> Value {
    let profiles = [
        "nexacro-json-1.0",
        "nexacro-xml-4000",
        "xplatform-xml-5000",
        "nexacro-ssv",
        "xplatform-ssv",
        "nexacro-binary-5000",
        "xplatform-binary-5000",
    ];
    json!({
        "protocolVersion": "1.0",
        "implementation": "xapi-rs",
        "profiles": profiles.into_iter().map(|name| json!({
            "name": name,
            "operations": ["decode", "encode", "roundtrip"],
            "options": [
                "strict",
                "base64Whitespace",
                "limits",
                "zlib",
                "ssvUnitSeparator",
                "ssvRecordSeparator"
            ],
            "limits": {}
        })).collect::<Vec<_>>()
    })
}

fn profile_format(profile: &str) -> Option<Format> {
    Some(match profile {
        "xplatform-xml-5000" => Format::XPlatformXml4000,
        "nexacro-xml-4000" => Format::NexacroXml4000,
        "nexacro-json-1.0" => Format::NexacroJson10,
        "xplatform-ssv" => Format::XPlatformSsv,
        "nexacro-ssv" => Format::NexacroSsv,
        "nexacro-binary-5000" => Format::NexacroBinary5000,
        "xplatform-binary-5000" => Format::XPlatformBinary5000,
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
    STANDARD
        .decode(encoded)
        .map_err(|error| error_response("malformed-input", "input.data", error.to_string()))
}

fn metadata(bytes: &[u8], format: Format) -> Metadata {
    match format {
        Format::NexacroJson10 => json_metadata(bytes),
        Format::XPlatformXml4000 | Format::NexacroXml4000 => {
            let mut metadata = xml_metadata(bytes);
            let (namespace, version, version_key) = match format {
                Format::XPlatformXml4000 => {
                    ("http://www.tobesoft.com/platform/Dataset", "5000", "ver")
                }
                Format::NexacroXml4000 => (
                    "http://www.nexacroplatform.com/platform/dataset",
                    "4000",
                    "version",
                ),
                _ => unreachable!(),
            };
            let mut root = metadata
                .root
                .take()
                .and_then(|root| root.as_object().cloned())
                .unwrap_or_default();
            root.insert("namespace".into(), Value::String(namespace.into()));
            root.remove("version");
            root.remove("ver");
            root.insert(version_key.into(), Value::String(version.into()));
            metadata.root = Some(Value::Object(root));
            metadata
        }
        Format::XPlatformSsv | Format::NexacroSsv => ssv_metadata(bytes),
        Format::NexacroBinary5000 | Format::XPlatformBinary5000 => Metadata::default(),
    }
}

fn ssv_metadata(bytes: &[u8]) -> Metadata {
    let bytes = if let Ok(text) = std::str::from_utf8(bytes) {
        if let Some(start) = text.find("<noscript>") {
            let end = text.find("</noscript>").unwrap_or(text.len());
            &bytes[start + 10..end]
        } else {
            bytes
        }
    } else {
        bytes
    };
    let mut result = Metadata::default();
    let records = bytes
        .split(|byte| *byte == 0x1e)
        .filter_map(|record| std::str::from_utf8(record).ok());
    let mut dataset = None::<String>;
    for record in records {
        if record == "SSV" {
            result.root = Some(json!({"ssv": {}}));
        } else if let Some(code_page) = record.strip_prefix("SSV:") {
            result.root = Some(json!({"ssv": {"codePage": code_page}}));
        } else if let Some(id) = record.strip_prefix("Dataset:") {
            dataset = Some(id.to_owned());
            result.datasets.entry(id.to_owned()).or_default();
        } else if record.starts_with("_Const_") {
            if let Some(id) = &dataset {
                for field in record.split('\x1f').skip(1) {
                    let header = field
                        .split_once('=')
                        .map(|(header, _)| header)
                        .unwrap_or(field);
                    if let Some((column, wire)) = ssv_header(header) {
                        result
                            .datasets
                            .entry(id.clone())
                            .or_default()
                            .const_columns
                            .insert(
                                column,
                                ColumnMeta {
                                    declared: Some(ssv_type(header, false)),
                                    wire: Some(wire),
                                },
                            );
                    }
                }
            }
        } else if record.starts_with("_RowType_") {
            if let Some(id) = &dataset {
                for field in record.split('\x1f').skip(1) {
                    let parts = field.splitn(4, ':').collect::<Vec<_>>();
                    if let Some((column, wire)) = ssv_header(&parts[..parts.len().min(2)].join(":"))
                    {
                        let wire = if parts.len() >= 3 && !parts[1].contains('(') {
                            "UNDEFINED".to_owned()
                        } else {
                            wire
                        };
                        result
                            .datasets
                            .entry(id.clone())
                            .or_default()
                            .columns
                            .insert(
                                column,
                                ColumnMeta {
                                    declared: Some(ssv_type(
                                        &parts[..parts.len().min(2)].join(":"),
                                        parts.len() > 2,
                                    )),
                                    wire: Some(wire),
                                },
                            );
                    }
                }
            }
        } else if dataset.is_none() && !record.is_empty() {
            for variable in record.split('\x1f').filter(|v| !v.is_empty()) {
                let header = variable
                    .split_once('=')
                    .map(|(header, _)| header)
                    .unwrap_or(variable);
                if let Some((id, wire)) = ssv_header(header) {
                    result.parameters.insert(
                        id,
                        ParameterMeta {
                            declared: Some(ssv_type(header, false)),
                            wire: Some(wire),
                        },
                    );
                }
            }
        }
    }
    result
}

fn ssv_header(header: &str) -> Option<(String, String)> {
    let Some((id, typed)) = header.split_once(':') else {
        return Some((header.to_owned(), "STRING".into()));
    };
    let wire = typed.split('(').next().unwrap_or_default().trim();
    Some((id.to_owned(), normalize_wire_name(wire, "UNDEFINED")))
}

/// The type of an SSV typed name as the drivers read it: no type is STRING,
/// FLOAT reads as DOUBLE, DECIMAL and unknown types as UNDEFINED, and a
/// column with a third field but no size as UNDEFINED.
fn ssv_type(header: &str, extra_field: bool) -> String {
    let Some((_, typed)) = header.split_once(':') else {
        return "STRING".into();
    };
    if extra_field && !typed.contains('(') {
        return "UNDEFINED".into();
    }
    let name = typed
        .split('(')
        .next()
        .unwrap_or_default()
        .to_ascii_uppercase();
    match name.as_str() {
        "" => "STRING".into(),
        "FLOAT" => "DOUBLE".into(),
        "DECIMAL" => "UNDEFINED".into(),
        _ if known_type(&name) => name,
        _ => "UNDEFINED".into(),
    }
}

fn known_type(name: &str) -> bool {
    matches!(
        name,
        "STRING"
            | "CHAR"
            | "SHORT"
            | "USHORT"
            | "INT"
            | "UINT"
            | "LONG"
            | "ULONG"
            | "FLOAT"
            | "DOUBLE"
            | "DECIMAL"
            | "BIGDECIMAL"
            | "BOOLEAN"
            | "DATE"
            | "TIME"
            | "DATETIME"
            | "BLOB"
            | "FILE"
            | "NULL"
            | "UNDEFINED"
            | "DATASET"
            | "INVALID"
    )
}

fn has_custom_ssv_separators(options: &Value) -> bool {
    options
        .get("ssvUnitSeparator")
        .and_then(Value::as_str)
        .is_some_and(|value| !value.is_empty() && value != "\x1f")
        || options
            .get("ssvRecordSeparator")
            .and_then(Value::as_str)
            .is_some_and(|value| !value.is_empty() && value != "\x1e")
}

fn add_ssv_options(metadata: &mut Metadata, options: &Value) {
    let Some(ssv) = metadata
        .root
        .as_mut()
        .and_then(|root| root.get_mut("ssv"))
        .and_then(Value::as_object_mut)
    else {
        return;
    };
    if let Some(code_page) = ssv
        .get("codePage")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
    {
        let separator = options
            .get("ssvRecordSeparator")
            .and_then(Value::as_str)
            .unwrap_or("\x1e");
        ssv.insert(
            "codePage".into(),
            Value::String(
                code_page
                    .split(separator)
                    .next()
                    .unwrap_or(&code_page)
                    .to_owned(),
            ),
        );
    }
    for (key, option) in [
        ("unitSeparator", "ssvUnitSeparator"),
        ("recordSeparator", "ssvRecordSeparator"),
    ] {
        if let Some(value) = options.get(option).and_then(Value::as_str) {
            ssv.insert(key.into(), Value::String(value.into()));
        }
    }
}

fn decode_payload(bytes: &[u8], limits: Limits) -> Result<Vec<u8>, Value> {
    let bytes = if xapi::zlib::is_wrapped(bytes) {
        xapi::zlib::inflate(bytes, limits.max_document_bytes as usize)
            .map_err(|message| error_response("malformed-input", "wire", message))?
    } else {
        bytes.to_vec()
    };
    Ok(bytes)
}

fn replace_bytes(input: &[u8], from: &[u8], to: &[u8]) -> Vec<u8> {
    if from.is_empty() || from == to {
        return input.to_vec();
    }
    let mut out = Vec::with_capacity(input.len());
    let mut i = 0;
    while i < input.len() {
        if input[i..].starts_with(from) {
            out.extend_from_slice(to);
            i += from.len();
        } else {
            out.push(input[i]);
            i += 1;
        }
    }
    out
}

fn json_metadata(bytes: &[u8]) -> Metadata {
    let Ok(root) = serde_json::from_slice::<Value>(bytes) else {
        return Metadata::default();
    };
    let mut result = Metadata::default();
    let version = root.get("version").and_then(Value::as_str).unwrap_or("1.0");
    result.root = Some(json!({"version": version}));
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
                        declared: object
                            .get("type")
                            .and_then(Value::as_str)
                            .map(str::to_ascii_uppercase),
                        wire: object
                            .get("type")
                            .and_then(Value::as_str)
                            .map(|value| normalize_wire_name(value, "UNDEFINED")),
                    },
                );
                if result
                    .parameters
                    .get(id)
                    .and_then(|meta| meta.wire.as_ref())
                    .is_none()
                {
                    if let Some(wire) = object.get("value").map(json_value_wire) {
                        if let Some(meta) = result.parameters.get_mut(id) {
                            meta.wire = Some(wire);
                        }
                    }
                }
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
            declared: object
                .get("type")
                .and_then(Value::as_str)
                .map(str::to_ascii_uppercase),
            wire: object
                .get("type")
                .and_then(Value::as_str)
                .map(|value| normalize_wire_name(value, "UNDEFINED")),
        },
    );
    if target.get(id).and_then(|meta| meta.wire.as_ref()).is_none() {
        if let Some(wire) = object.get("value").map(json_value_wire) {
            if let Some(meta) = target.get_mut(id) {
                meta.wire = Some(wire);
            }
        }
    }
}

fn json_value_wire(value: &Value) -> String {
    match value {
        Value::Bool(_) => "BOOLEAN",
        Value::Number(number) if number.is_i64() || number.is_u64() => "INT",
        Value::Number(_) => "FLOAT",
        Value::String(_) => "STRING",
        _ => "STRING",
    }
    .into()
}

fn value_text(value: &Value) -> String {
    value
        .as_str()
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| value.to_string())
}

fn normalize_wire_name(value: &str, default: &str) -> String {
    match value.to_ascii_uppercase().as_str() {
        "STRING" | "STRING(L)" => "STRING",
        "SHORT" | "USHORT" | "INT" | "UINT" => "INT",
        "LONG" | "ULONG" => "BIGDECIMAL",
        "FLOAT" => "DOUBLE",
        "DOUBLE" => "DOUBLE",
        "DECIMAL" => "UNDEFINED",
        "BIGDECIMAL" => "BIGDECIMAL",
        "BOOLEAN" | "BOOL" => "BOOLEAN",
        "DATE" => "DATE",
        "TIME" => "TIME",
        "DATETIME" => "DATETIME",
        "BLOB" | "FILE" => "BLOB",
        "NULL" => "NULL",
        "UNDEFINED" => "UNDEFINED",
        "DATASET" => "DATASET",
        "INVALID" => "INVALID",
        _ => default,
    }
    .into()
}

#[allow(clippy::while_let_loop)]
fn xml_metadata(bytes: &[u8]) -> Metadata {
    let mut result = Metadata::default();
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
                                    declared: xml_attr(&start, b"type")
                                        .map(|value| value.to_ascii_uppercase()),
                                    wire: Some(
                                        if xml_attr(&start, b"type").as_deref().is_some_and(
                                            |wire| wire.eq_ignore_ascii_case("DECIMAL"),
                                        ) {
                                            "STRING".into()
                                        } else {
                                            normalize_wire_name(
                                                xml_attr(&start, b"type").as_deref().unwrap_or(""),
                                                "STRING",
                                            )
                                        },
                                    ),
                                },
                            );
                        }
                    }
                    b"Column" | b"ConstColumn" => {
                        if let (Some(dataset), Some(id)) =
                            (dataset.as_ref(), xml_attr(&start, b"id"))
                        {
                            let column = ColumnMeta {
                                declared: xml_attr(&start, b"type")
                                    .map(|value| value.to_ascii_uppercase()),
                                wire: Some(normalize_wire_name(
                                    xml_attr(&start, b"type").as_deref().unwrap_or(""),
                                    "UNDEFINED",
                                )),
                            };
                            let meta = result.datasets.entry(dataset.clone()).or_default();
                            if meta.columns.is_empty() && meta.const_columns.is_empty() {
                                meta.const_first = name.as_ref() == b"ConstColumn";
                            }
                            if name.as_ref() == b"ConstColumn" {
                                meta.const_columns.insert(id, column);
                            } else {
                                meta.columns.insert(id, column);
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
fn strict_xml_contract(bytes: &[u8], _format: Format) -> bool {
    let mut reader = Reader::from_reader(bytes);
    let mut buffer = Vec::new();
    let mut root_seen = false;
    loop {
        match reader.read_event_into(&mut buffer) {
            Ok(Event::Start(start)) | Ok(Event::Empty(start)) => {
                let name = start.local_name();
                if name.as_ref() == b"Root" {
                    if root_seen {
                        return false;
                    }
                    root_seen = true;
                }
                let requires_id = matches!(
                    name.as_ref(),
                    b"Dataset" | b"Column" | b"ConstColumn" | b"Parameter" | b"Col"
                );
                if requires_id && xml_attr(&start, b"id").is_none_or(|value| value.is_empty()) {
                    return false;
                }
            }
            Ok(Event::Eof) => return root_seen,
            Err(_) => return false,
            _ => {}
        }
        buffer.clear();
    }
}

fn json_contract_valid(bytes: &[u8], _strict: bool) -> bool {
    let Ok(root) = serde_json::from_slice::<Value>(bytes) else {
        return false;
    };
    let Some(root) = root.as_object() else {
        return false;
    };
    let Some(datasets) = root
        .get("Datasets")
        .or_else(|| root.get("datasets"))
        .and_then(Value::as_array)
    else {
        return true;
    };
    for dataset in datasets {
        let Some(dataset) = dataset.as_object() else {
            return false;
        };
        if let Some(info) = dataset.get("ColumnInfo").or_else(|| dataset.get("columns")) {
            if let Some(info) = info.as_object() {
                for key in ["Column", "columns", "ConstColumn", "constColumns"] {
                    if let Some(columns) = info.get(key).and_then(Value::as_array) {
                        for column in columns {
                            let Some(_column) = column.as_object() else {
                                return false;
                            };
                        }
                    }
                }
            } else if !info.is_array() {
                return false;
            }
        }
    }
    if let Some(parameters) = root
        .get("Parameters")
        .or_else(|| root.get("parameters"))
        .and_then(Value::as_array)
    {
        for parameter in parameters {
            let Some(_parameter) = parameter.as_object() else {
                return false;
            };
        }
    }
    true
}
fn xml_attributes_valid(bytes: &[u8]) -> bool {
    let mut reader = Reader::from_reader(bytes);
    let mut buffer = Vec::new();
    loop {
        match reader.read_event_into(&mut buffer) {
            Ok(Event::Start(start)) | Ok(Event::Empty(start))
                if start
                    .attributes()
                    .with_checks(true)
                    .any(|item| item.is_err()) =>
            {
                return false;
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
    if let Some(parameters) = value.get("parameters").and_then(Value::as_array) {
        for parameter in parameters {
            if parameter
                .get("type")
                .and_then(Value::as_str)
                .is_some_and(|type_name| {
                    matches!(
                        type_name.to_ascii_uppercase().as_str(),
                        "NULL" | "UNDEFINED" | "DATASET" | "INVALID"
                    )
                })
            {
                return Err(error_response(
                    "invalid-value",
                    "",
                    "unsupported public XAPI type".into(),
                ));
            }
        }
    }
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
        for column in columns.iter().chain(constants.iter()) {
            if column
                .get("type")
                .and_then(Value::as_str)
                .is_some_and(|type_name| {
                    matches!(
                        type_name.to_ascii_uppercase().as_str(),
                        "DATASET" | "INVALID"
                    )
                })
            {
                return Err(error_response(
                    "invalid-value",
                    "",
                    "unsupported public XAPI type".into(),
                ));
            }
        }
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

/// The decode rules of a profile.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Xml,
    Json,
    Ssv { xplatform: bool },
    Binary,
}

impl Kind {
    fn of(format: Format) -> Self {
        match format {
            Format::NexacroJson10 => Kind::Json,
            Format::XPlatformXml4000 | Format::NexacroXml4000 => Kind::Xml,
            Format::XPlatformSsv => Kind::Ssv { xplatform: true },
            Format::NexacroSsv => Kind::Ssv { xplatform: false },
            Format::NexacroBinary5000 | Format::XPlatformBinary5000 => Kind::Binary,
        }
    }
}

/// The lexical of a decoded value; None for null or missing.
fn lexical(value: &RawValue) -> Option<String> {
    match value {
        RawValue::Missing | RawValue::Null => None,
        RawValue::Text(text) => Some(text.clone()),
        RawValue::Bytes(bytes) => Some(STANDARD.encode(bytes)),
    }
}

/// A canonical cell for a lexical: null, empty, or a value.
fn state_cell(lexical: Option<&str>) -> Value {
    match lexical {
        None => json!({"state": "null"}),
        Some("") => json!({"state": "empty", "lexical": ""}),
        Some(text) => json!({"state": "value", "lexical": text}),
    }
}

/// A decoded value as the driver's typed setter converts it: an invalid
/// BIGDECIMAL, date or BLOB reads as null.
fn typed_cell(data_type: &str, lexical: Option<&str>) -> Value {
    match lexical {
        None => state_cell(None),
        Some(text) => state_cell(xapi::driver::decode(data_type, text).as_deref()),
    }
}

/// A BLOB parameter read from a text format: the bytes of the text itself.
fn text_blob_cell(lexical: Option<&str>) -> Value {
    state_cell(lexical.map(xapi::driver::text_blob).as_deref())
}

/// A declared column type as the drivers report it: FLOAT reads as DOUBLE,
/// DECIMAL and unknown types as UNDEFINED.
fn decoded_type(declared: Option<&str>, missing: &str) -> String {
    let name = declared.unwrap_or_default().to_ascii_uppercase();
    match name.as_str() {
        "" => missing.to_owned(),
        "FLOAT" => "DOUBLE".into(),
        "DECIMAL" => "UNDEFINED".into(),
        _ if known_type(&name) => name,
        _ => "UNDEFINED".into(),
    }
}

/// An XML parameter type: no, DECIMAL or unknown type reads as STRING.
fn xml_parameter_type(declared: Option<&str>) -> String {
    let name = declared.unwrap_or_default().to_ascii_uppercase();
    match name.as_str() {
        "FLOAT" => "DOUBLE".into(),
        "" | "DECIMAL" => "STRING".into(),
        _ if known_type(&name) => name,
        _ => "STRING".into(),
    }
}

fn ssv_numeric(data_type: &str) -> bool {
    matches!(
        data_type,
        "SHORT" | "USHORT" | "INT" | "UINT" | "LONG" | "ULONG" | "FLOAT" | "DOUBLE"
    )
}

fn canonical(document: &RawDocument, metadata: &Metadata, format: Format) -> Value {
    let kind = Kind::of(format);
    let mut parameters: Vec<Value> = Vec::new();
    for (index, (id, value)) in document.parameters.iter().enumerate() {
        let declared = metadata
            .parameters
            .get(id)
            .and_then(|meta| meta.declared.as_deref());
        let (data_type, cell) = match kind {
            Kind::Binary => {
                let data_type = document
                    .parameter_wires
                    .get(index)
                    .cloned()
                    .flatten()
                    .unwrap_or_else(|| "UNDEFINED".into());
                (data_type, state_cell(lexical(value).as_deref()))
            }
            Kind::Xml => {
                // The XML readers take the element text; no text is empty.
                let text = lexical(value).unwrap_or_default();
                let data_type = xml_parameter_type(declared);
                let cell = if xapi::driver::is_blob(&data_type) {
                    text_blob_cell(Some(&text))
                } else {
                    typed_cell(&data_type, Some(&text))
                };
                (data_type, cell)
            }
            Kind::Json => {
                let text = lexical(value);
                let mut data_type = decoded_type(declared, "STRING");
                // Variable.set gives an untyped variable the string type; a
                // null value is never set.
                if data_type == "UNDEFINED" && text.is_some() {
                    data_type = "STRING".into();
                }
                let cell = if xapi::driver::is_blob(&data_type) {
                    text_blob_cell(text.as_deref())
                } else {
                    typed_cell(&data_type, text.as_deref())
                };
                (data_type, cell)
            }
            Kind::Ssv { .. } => {
                // A variable without '=' is not set.
                if matches!(value, RawValue::Missing) {
                    continue;
                }
                let mut data_type = declared.unwrap_or("STRING").to_owned();
                if data_type == "UNDEFINED" {
                    data_type = "STRING".into();
                }
                let mut text = lexical(value);
                if text.is_none() && ssv_numeric(&data_type) {
                    // Variable.set converts the null marker like an empty number.
                    text = Some(String::new());
                }
                let cell = if xapi::driver::is_blob(&data_type) {
                    text_blob_cell(text.as_deref())
                } else {
                    typed_cell(&data_type, text.as_deref())
                };
                if let Some(position) = parameters
                    .iter()
                    .position(|parameter| parameter["id"] == *id)
                {
                    parameters.remove(position);
                }
                (data_type, cell)
            }
        };
        let mut parameter = Map::new();
        parameter.insert("id".into(), Value::String(id.clone()));
        parameter.insert("type".into(), Value::String(data_type));
        parameter.extend(cell.as_object().cloned().unwrap_or_default());
        parameters.push(Value::Object(parameter));
    }
    let datasets = document
        .datasets
        .iter()
        .map(|dataset| canonical_dataset(dataset, metadata.datasets.get(&dataset.id), kind))
        .collect::<Vec<_>>();
    let mut result = json!({"parameters": parameters, "datasets": datasets});
    match kind {
        Kind::Xml => {
            if let Some(root) = &metadata.root {
                result["wire"] = json!({"root": root});
            }
        }
        Kind::Json => result["wire"] = json!({"version": "1.0"}),
        _ => {}
    }
    result
}

fn canonical_dataset(dataset: &RawDataset, meta: Option<&DatasetMeta>, kind: Kind) -> Value {
    let declared = |id: &str, constant: bool| -> Option<String> {
        meta.and_then(|meta| {
            if constant {
                meta.const_columns.get(id)
            } else {
                meta.columns.get(id)
            }
        })
        .and_then(|column| column.declared.clone())
    };
    let column_count = dataset.columns.len();
    let const_count = dataset.const_columns.len();
    let const_first = meta.is_some_and(|meta| meta.const_first);
    // Canonical type, and the type a row value is converted by.
    let column_types = dataset
        .columns
        .iter()
        .map(|(id, wire)| match kind {
            Kind::Binary => (wire.to_string(), wire.to_string()),
            Kind::Xml => {
                let raw = declared(id, false);
                (
                    decoded_type(raw.as_deref(), "UNDEFINED"),
                    raw.unwrap_or_default(),
                )
            }
            Kind::Json => {
                let data_type = decoded_type(declared(id, false).as_deref(), "STRING");
                (data_type.clone(), data_type)
            }
            Kind::Ssv { xplatform } => {
                let data_type = declared(id, false).unwrap_or_else(|| "STRING".into());
                // Nexacro SSV columns base64-decode BLOB text; XPLATFORM SSV
                // keeps the bytes of the text itself.
                let decode = if xplatform && xapi::driver::is_blob(&data_type) {
                    "TEXT_BLOB".to_owned()
                } else {
                    data_type.clone()
                };
                (data_type, decode)
            }
        })
        .collect::<Vec<_>>();
    let columns = dataset
        .columns
        .iter()
        .zip(&column_types)
        .enumerate()
        .map(|(index, ((id, _), (data_type, _)))| {
            let index = match kind {
                Kind::Binary => const_count + index,
                Kind::Xml if const_first => const_count + index,
                _ => index,
            };
            json!({"id": id, "type": data_type, "index": index})
        })
        .collect::<Vec<_>>();
    let mut const_columns = Vec::new();
    let mut constants: Vec<(String, Value)> = Vec::new();
    for (index, (id, wire, value)) in dataset.const_columns.iter().enumerate() {
        let index = match kind {
            Kind::Binary => index,
            Kind::Xml if const_first => index,
            _ => column_count + index,
        };
        let raw = declared(id, true);
        let data_type = match kind {
            // Only the null tags carry no value; they read as UNDEFINED.
            Kind::Binary if matches!(value, RawValue::Null) => "UNDEFINED".into(),
            Kind::Binary => wire.to_string(),
            Kind::Xml => decoded_type(raw.as_deref(), "UNDEFINED"),
            Kind::Json => decoded_type(raw.as_deref(), "STRING"),
            Kind::Ssv { .. } => raw.clone().unwrap_or_else(|| "STRING".into()),
        };
        // XML converts a constant to the column type; JSON, SSV and
        // PlatformBinary keep it as read.
        let cell = match kind {
            Kind::Xml => typed_cell(&raw.unwrap_or_default(), lexical(value).as_deref()),
            _ => state_cell(lexical(value).as_deref()),
        };
        const_columns.push(json!({"id": id, "type": data_type, "index": index}));
        constants.push((id.clone(), cell));
    }
    let convert = |id: &str, value: &RawValue| -> Value {
        let Some(position) = dataset.columns.iter().position(|(column, _)| column == id) else {
            return state_cell(lexical(value).as_deref());
        };
        match (kind, value) {
            (Kind::Ssv { .. }, RawValue::Missing) => json!({"state": "missing"}),
            (Kind::Binary, _) => state_cell(lexical(value).as_deref()),
            _ => {
                let decode = &column_types[position].1;
                if decode == "TEXT_BLOB" {
                    text_blob_cell(lexical(value).as_deref())
                } else {
                    typed_cell(decode, lexical(value).as_deref())
                }
            }
        }
    };
    let values_of = |cells: &[(String, RawValue)], with_constants: bool| -> Map<String, Value> {
        let mut values = Map::new();
        // PlatformBinary rows hold only the values they carry; the other
        // formats start every column null (SSV: missing when the record is
        // short).
        if kind != Kind::Binary {
            let absent = if matches!(kind, Kind::Ssv { .. }) {
                &RawValue::Missing
            } else {
                &RawValue::Null
            };
            for (id, _) in &dataset.columns {
                values.insert(id.clone(), convert(id, absent));
            }
        }
        if with_constants {
            for (id, cell) in &constants {
                values.insert(id.clone(), cell.clone());
            }
        }
        // A later value for the same column replaces an earlier one.
        for (id, value) in cells {
            let cell = if dataset.columns.iter().any(|(column, _)| column == id) {
                convert(id, value)
            } else if kind == Kind::Xml {
                typed_cell(
                    &declared(id, true).unwrap_or_default(),
                    lexical(value).as_deref(),
                )
            } else {
                continue;
            };
            values.insert(id.clone(), cell);
        }
        values
    };
    let mut live = Vec::new();
    let mut removed = Vec::new();
    for row in dataset
        .rows
        .iter()
        .filter(|row| row.row_type != RowType::Original)
    {
        let values = values_of(&row.values, true);
        let mut org_row = Value::Null;
        if row.row_type == RowType::Update {
            if let Some(original) = &row.original {
                let original_values = values_of(original, kind != Kind::Binary);
                // The drivers report a saved row holding only nulls as no
                // saved row; XML looks only at the saved cells.
                let meaningful = match kind {
                    Kind::Xml => original
                        .iter()
                        .any(|(id, value)| convert(id, value)["state"] != "null"),
                    _ => original_values
                        .values()
                        .any(|cell| cell["state"] != "null" && cell["state"] != "missing"),
                };
                if meaningful {
                    org_row =
                        json!({"type": "O", "orgRow": Value::Null, "values": original_values});
                }
            }
        }
        let row_value =
            json!({"type": row_type(row.row_type), "orgRow": org_row, "values": values});
        if row.row_type == RowType::Delete {
            removed.push(row_value);
        } else {
            live.push(row_value);
        }
    }
    live.extend(removed);
    json!({"id": dataset.id, "columns": columns, "constColumns": const_columns, "rows": live})
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
    let parameter_wires = object
        .get("parameters")
        .and_then(Value::as_array)
        .map(|parameters| {
            parameters
                .iter()
                .map(|parameter| {
                    parameter
                        .get("type")
                        .and_then(Value::as_str)
                        .map(ToOwned::to_owned)
                })
                .collect()
        })
        .unwrap_or_default();
    Some(RawDocument {
        parameters,
        parameter_wires,
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
    let column_sizes = object
        .get("columns")
        .and_then(Value::as_array)
        .map(|columns| {
            columns
                .iter()
                .map(|column| column.get("size").map(value_text))
                .collect()
        })
        .unwrap_or_default();
    let column_meta = object
        .get("columns")
        .and_then(Value::as_array)
        .map(|columns| {
            columns
                .iter()
                .map(|column| {
                    (
                        column.get("prop").map(value_text),
                        column.get("sumtext").map(value_text),
                    )
                })
                .collect()
        })
        .unwrap_or_default();
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
                column
                    .get("value")
                    .and_then(Value::as_object)
                    .and_then(raw_cell)
                    .unwrap_or(RawValue::Missing),
            ))
        })
        .collect::<Option<Vec<_>>>()?;
    let const_column_sizes = object
        .get("constColumns")
        .and_then(Value::as_array)
        .map(|columns| {
            columns
                .iter()
                .map(|column| column.get("size").map(value_text))
                .collect()
        })
        .unwrap_or_default();
    let rows = object
        .get("rows")?
        .as_array()?
        .iter()
        .map(raw_row)
        .collect::<Option<Vec<_>>>()?;
    Some(RawDataset {
        id: object.get("id")?.as_str()?.to_owned(),
        columns,
        column_sizes,
        column_meta,
        const_columns,
        const_column_sizes,
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

/// Applies the saveType selection (root, then dataset; default NORMAL = 2)
/// as the drivers do before writing; the library writers then write the
/// rows they get in the driver layout (saveType ALL).
fn apply_save_types(document: &mut RawDocument, value: &Value) {
    let root = value.get("saveType").and_then(Value::as_i64).unwrap_or(0);
    let datasets = value.get("datasets").and_then(Value::as_array);
    for (index, dataset) in document.datasets.iter_mut().enumerate() {
        let mut save_type = datasets
            .and_then(|datasets| datasets.get(index))
            .and_then(|dataset| dataset.get("saveType"))
            .and_then(Value::as_i64)
            .unwrap_or(0);
        if save_type == 0 {
            save_type = root;
        }
        if save_type == 0 {
            save_type = 2;
        }
        let mut rows = Vec::new();
        for mut row in std::mem::take(&mut dataset.rows) {
            match row.row_type {
                RowType::Original => continue,
                RowType::Delete => {
                    if matches!(save_type, 1 | 4 | 5) {
                        row.original = None;
                        rows.push(row);
                    }
                    continue;
                }
                _ => {}
            }
            let include = matches!(save_type, 1 | 2)
                || matches!(save_type, 3 | 5) && row.row_type != RowType::Normal;
            if !include {
                continue;
            }
            if save_type == 2 {
                if row.row_type == RowType::Update {
                    if let Some(original) = row.original.take() {
                        let mut merged: Vec<(String, RawValue)> = original
                            .into_iter()
                            .filter(|(_, value)| !matches!(value, RawValue::Missing))
                            .collect();
                        for (id, value) in row.values {
                            if matches!(value, RawValue::Missing) {
                                continue;
                            }
                            match merged.iter_mut().find(|(column, _)| *column == id) {
                                Some(cell) => cell.1 = value,
                                None => merged.push((id, value)),
                            }
                        }
                        row.values = merged;
                    }
                }
                row.row_type = RowType::Normal;
                row.original = None;
            }
            rows.push(row);
        }
        dataset.rows = rows;
    }
}

fn encode_result(
    document: &RawDocument,
    format: Format,
    options: &Value,
) -> Result<Value, xapi::Error> {
    let mut output = Vec::new();
    dynamic::encode(&mut output, document, format)?;
    if matches!(format, Format::XPlatformSsv | Format::NexacroSsv) {
        let unit = options
            .get("ssvUnitSeparator")
            .and_then(Value::as_str)
            .filter(|v| !v.is_empty())
            .unwrap_or("\x1f");
        let record = options
            .get("ssvRecordSeparator")
            .and_then(Value::as_str)
            .filter(|v| !v.is_empty())
            .unwrap_or("\x1e");
        output = replace_bytes(
            &replace_bytes(&output, b"\x1e", record.as_bytes()),
            b"\x1f",
            unit.as_bytes(),
        );
    }
    if options
        .get("zlib")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        output = xapi::zlib::deflate(&output)
            .map_err(|message| xapi::Error::from(xapi::EncodeError::Message(message)))?;
    }
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
            let path = if path.is_empty() && matches!(source, xapi::DecodeError::Limit(_)) {
                "input.data"
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

#[cfg(test)]
mod tests {
    use super::handle;
    use serde_json::json;

    #[test]
    fn xplatform_xml_5000_is_advertised_and_encodes_5000_root() {
        let capabilities = handle(r#"{"operation":"capabilities"}"#);
        assert!(capabilities["profiles"]
            .as_array()
            .unwrap()
            .iter()
            .any(|profile| profile["name"] == "xplatform-xml-5000"));

        let response = handle(
            &json!({
                "operation": "encode",
                "profile": "xplatform-xml-5000",
                "value": {"parameters": [], "datasets": []}
            })
            .to_string(),
        );
        assert_eq!(response["ok"], true);
        assert_eq!(response["output"]["data"], "PD94bWwgdmVyc2lvbj0iMS4wIiBlbmNvZGluZz0iVVRGLTgiPz4KPFJvb3QgeG1sbnM9Imh0dHA6Ly93d3cudG9iZXNvZnQuY29tL3BsYXRmb3JtL2RhdGFzZXQiIHZlcj0iNTAwMCI+Cgk8UGFyYW1ldGVycy8+CjwvUm9vdD4K");
    }
    #[test]
    fn custom_ssv_separators_decode_to_empty_canonical_value() {
        let response = handle(
            &json!({
                "operation": "decode",
                "profile": "nexacro-ssv",
                "options": {
                    "ssvUnitSeparator": "|",
                    "ssvRecordSeparator": "~",
                    "strict": true
                },
                "input": {"encoding": "base64", "data": "U1NWOnV0Zi04fkRhdGFzZXQ6ZH5fUm93VHlwZV98YTpTVFJJTkd+Tnx4fn4="}
            })
            .to_string(),
        );
        assert_eq!(
            response,
            json!({"ok": true, "value": {"datasets": [], "parameters": []}})
        );
    }

    #[test]
    fn nexacro_ssv_decode_preserves_update_original_values() {
        let response = handle(
            &json!({
                "operation": "decode",
                "profile": "nexacro-ssv",
                "input": {"encoding": "base64", "data": "U1NWOnV0Zi04HnA6SU5UPTEeRGF0YXNldDpkHl9Db25zdF8fYzpTVFJJTkcoNSk9Zml4ZWQeX1Jvd1R5cGVfH2E6U1RSSU5HKDEwKTpTVU06dG90YWwfYjpJTlQeVR9uZXcfMR5PH29sZB8xHk4fHwMeHg=="}
            })
            .to_string(),
        );
        assert_eq!(
            response["value"]["datasets"][0]["rows"][0]["orgRow"]["values"]["a"]["lexical"],
            "old"
        );
    }

    #[test]
    fn nexacro_json_decode_preserves_update_original_values() {
        let response = handle(
            &json!({
                "operation": "decode",
                "profile": "nexacro-json-1.0",
                "input": {"encoding": "base64", "data": "eyJ2ZXJzaW9uIjoiMS4wIiwiUGFyYW1ldGVycyI6W10sIkRhdGFzZXRzIjpbeyJpZCI6ImQiLCJDb2x1bW5JbmZvIjp7IkNvbHVtbiI6W3siaWQiOiJhIiwidHlwZSI6InN0cmluZyJ9XX0sIlJvd3MiOlt7Il9Sb3dUeXBlXyI6IlUiLCJhIjoibmV3In0seyJfUm93VHlwZV8iOiJPIiwiYSI6Im9sZCJ9XX1dfQ=="}
            })
            .to_string(),
        );
        assert_eq!(
            response["value"]["datasets"][0]["rows"][0]["orgRow"]["values"]["a"]["lexical"],
            "old"
        );
    }

    #[test]
    fn binary_encode_rejects_internal_parameter_types() {
        let response = handle(
            &json!({
                "operation": "encode",
                "profile": "nexacro-binary-5000",
                "value": {
                    "parameters": [
                        {"id": "u", "type": "UNDEFINED", "state": "empty", "lexical": ""},
                        {"id": "n", "type": "NULL", "state": "empty", "lexical": "", "index": 1},
                        {"id": "d", "type": "DATASET", "state": "empty", "lexical": "", "index": 2},
                        {"id": "i", "type": "INVALID", "state": "empty", "lexical": "", "index": 3}
                    ],
                    "datasets": []
                }
            })
            .to_string(),
        );
        assert_eq!(response["ok"], false);
        assert_eq!(response["error"]["class"], "invalid-value");
        assert_eq!(response["error"]["path"], "");
    }

    #[test]
    fn duplicate_json_root_keys_are_rejected() {
        let response = handle(
            &json!({
                "operation": "decode",
                "profile": "nexacro-json-1.0",
                "input": {"encoding": "base64", "data": "eyJwYXJhbWV0ZXJzIjpbXSwicGFyYW1ldGVycyI6W119"}
            })
            .to_string(),
        );
        assert_eq!(response["ok"], false);
        assert_eq!(response["error"]["class"], "malformed-input");
        assert_eq!(response["error"]["path"], "");
    }

    #[test]
    fn nexacro_ssv_encodes_empty_and_missing_cells_distinctly() {
        let response = handle(
            &json!({
                "operation": "encode",
                "profile": "nexacro-ssv",
                "value": {
                    "datasets": [{
                        "id": "d",
                        "columns": [
                            {"id": "a", "index": 0, "type": "STRING"},
                            {"id": "b", "index": 1, "type": "STRING"}
                        ],
                        "constColumns": [],
                        "rows": [{"type": "N", "values": {
                            "a": {"state": "empty"},
                            "b": {"state": "missing"}
                        }}]
                    }],
                    "parameters": []
                }
            })
            .to_string(),
        );
        assert_eq!(
            response["output"]["data"],
            "U1NWOlVURi04HkRhdGFzZXQ6ZB5fUm93VHlwZV8fYTpzdHJpbmcoMzIpH2I6c3RyaW5nKDMyKR5OHx8DHh4="
        );
    }

    #[test]
    fn json_encode_emits_empty_and_omits_null_or_missing_cells() {
        let response = handle(
            &json!({
                "operation": "encode",
                "profile": "nexacro-json-1.0",
                "value": {
                    "parameters": [],
                    "datasets": [{
                        "id": "d",
                        "columns": [
                            {"id": "string", "index": 0, "size": "50", "type": "STRING"},
                            {"id": "int", "index": 1, "size": "10", "type": "INT"},
                            {"id": "float", "index": 2, "type": "FLOAT"}
                        ],
                        "constColumns": [],
                        "rows": [{"type": "N", "values": {
                            "string": {"state": "empty"},
                            "int": {"state": "null"},
                            "float": {"state": "missing"}
                        }}]
                    }]
                }
            })
            .to_string(),
        );
        assert_eq!(response["output"]["data"], "eyJEYXRhc2V0cyI6W3siQ29sdW1uSW5mbyI6eyJDb2x1bW4iOlt7InNpemUiOiI1MCIsImlkIjoic3RyaW5nIiwidHlwZSI6InN0cmluZyJ9LHsic2l6ZSI6IjEwIiwiaWQiOiJpbnQiLCJ0eXBlIjoiaW50In0seyJzaXplIjoiNCIsImlkIjoiZmxvYXQiLCJ0eXBlIjoiZmxvYXQifV19LCJpZCI6ImQiLCJSb3dzIjpbeyJzdHJpbmciOiIiLCJfUm93VHlwZV8iOiJOIn1dfV0sInZlcnNpb24iOiIxLjAifQ==");
    }

    #[test]
    fn xml_canonicalization_preserves_updates_and_deletes() {
        let response = handle(
            &json!({
                "operation": "decode",
                "profile": "nexacro-xml-4000",
                "input": {
                    "encoding": "base64",
                    "data": "PFJvb3Q+PERhdGFzZXQgaWQ9ImQiPjxDb2x1bW5JbmZvPjxDb2x1bW4gaWQ9ImEiIHR5cGU9IlNUUklORyIvPjwvQ29sdW1uSW5mbz48Um93cz48Um93IHR5cGU9InVwZGF0ZSI+PENvbCBpZD0iYSI+bmV3PC9Db2w+PE9yZ1Jvdz48Q29sIGlkPSJhIj5vbGQ8L0NvbD48L09yZ1Jvdz48L1Jvdz48Um93IHR5cGU9ImRlbGV0ZSI+PENvbCBpZD0iYSI+ZGVsPC9Db2w+PC9Sb3c+PC9Sb3dzPjwvRGF0YXNldD48L1Jvb3Q+"
                }
            })
            .to_string(),
        );
        assert_eq!(response["ok"], true);
        assert_eq!(
            response["value"]["datasets"][0]["rows"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        assert_eq!(
            response["value"]["datasets"][0]["rows"][0]["orgRow"]["type"],
            "O"
        );
        assert_eq!(response["value"]["datasets"][0]["rows"][1]["type"], "D");
    }
}
