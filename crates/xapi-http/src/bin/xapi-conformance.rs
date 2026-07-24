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
    index: Option<usize>,
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
    if matches!(format, Format::NexacroSsv | Format::XPlatformSsv)
        && (options.get("ssvUnitSeparator").is_some()
            || options.get("ssvRecordSeparator").is_some())
    {
        return error_response(
            "malformed-input",
            "",
            "custom SSV separators are unsupported".into(),
        );
    }
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
                let decode_bytes = match decode_payload(&bytes, options, format, limits) {
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
                let mut metadata = metadata(&decode_bytes, format);
                add_ssv_options(&mut metadata, options);
                let standalone_original = json_has_standalone_original(&decode_bytes, format);
                match dynamic::decode(decode_bytes.as_slice(), format, limits, strict) {
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
            Some(value) => match validate_encode(value, Limits::default()) {
                Err(error) => error,
                Ok(()) => match canonical_document(value) {
                    Some(document) => match encode_result(&document, format, options) {
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
                let decode_bytes = match decode_payload(&bytes, options, format, limits) {
                    Ok(bytes) => bytes,
                    Err(error) => return error,
                };
                let mut metadata = metadata(&decode_bytes, format);
                add_ssv_options(&mut metadata, options);
                let standalone_original = json_has_standalone_original(&decode_bytes, format);
                match dynamic::decode(decode_bytes.as_slice(), format, limits, strict) {
                    Ok(document) => match encode_result(&document, format, options) {
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

fn capabilities() -> Value {
    let profiles = [
        "nexacro-json-1.0",
        "nexacro-xml-4000",
        "xplatform-xml-4000",
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
        "xplatform-xml-4000" => Format::XPlatformXml4000,
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
                    ("http://www.tobesoft.com/platform/Dataset", "4000", "ver")
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
                            .insert(column, ColumnMeta { wire: Some(wire) });
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
                            .insert(column, ColumnMeta { wire: Some(wire) });
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
                            wire: Some(wire),
                            value_form: variable.contains('=').then(|| "value".into()),
                            index: None,
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

fn normalize_ssv(bytes: &[u8], options: &Value, format: Format) -> Vec<u8> {
    if !matches!(format, Format::XPlatformSsv | Format::NexacroSsv) {
        return bytes.to_vec();
    }
    let unit = options
        .get("ssvUnitSeparator")
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty())
        .unwrap_or("\x1f")
        .as_bytes();
    let record = options
        .get("ssvRecordSeparator")
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty())
        .unwrap_or("\x1e")
        .as_bytes();
    if unit == b"\x1f" && record == b"\x1e" {
        return bytes.to_vec();
    }
    let mut out = bytes.to_vec();
    if record != b"\x1e" {
        out = replace_bytes(&out, record, b"\x1e");
    }
    if unit != b"\x1f" {
        out = replace_bytes(&out, unit, b"\x1f");
    }
    out
}

fn decode_payload(
    bytes: &[u8],
    options: &Value,
    format: Format,
    limits: Limits,
) -> Result<Vec<u8>, Value> {
    let bytes = if xapi::zlib::is_wrapped(bytes) {
        xapi::zlib::inflate(bytes, limits.max_document_bytes as usize)
            .map_err(|message| error_response("malformed-input", "wire", message))?
    } else {
        bytes.to_vec()
    };
    Ok(normalize_ssv(&bytes, options, format))
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
                        wire: object
                            .get("type")
                            .and_then(Value::as_str)
                            .map(|value| normalize_wire_name(value, "UNDEFINED")),
                        value_form: None,
                        index: object
                            .get("index")
                            .and_then(Value::as_u64)
                            .and_then(|index| usize::try_from(index).ok()),
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
                                    value_form: Some(if xml_attr(&start, b"value").is_some() {
                                        "attribute".into()
                                    } else {
                                        "text".into()
                                    }),
                                    index: None,
                                },
                            );
                        }
                    }
                    b"Column" | b"ConstColumn" => {
                        if let (Some(dataset), Some(id)) =
                            (dataset.as_ref(), xml_attr(&start, b"id"))
                        {
                            let column = ColumnMeta {
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
    if value.get("saveType").is_some() {
        return Err(error_response(
            "malformed-input",
            "",
            "saveType is not supported by the protocol".into(),
        ));
    }
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

fn normalize_lenient_datetime(value: &str) -> String {
    let bytes = value.as_bytes();
    let component = |offset: usize, length: usize| -> i64 {
        bytes[offset..offset + length]
            .iter()
            .fold(0_i64, |number, byte| {
                number * 10 + i64::from(*byte) - i64::from(b'0')
            })
    };
    let year = component(0, 4);
    let mut cycles = (year - 2000) / 400;
    if year < 2000 && (year - 2000) % 400 != 0 {
        cycles -= 1;
    }
    let base_year = year - cycles * 400;
    let month = component(4, 2);
    let day = component(6, 2);
    let hour = component(8, 2);
    let minute = component(10, 2);
    let second = component(12, 2);

    let month_zero = month - 1;
    let normalized_year = base_year + month_zero.div_euclid(12);
    let normalized_month = month_zero.rem_euclid(12) + 1;
    let date = days_from_civil(normalized_year, normalized_month, 1) + day - 1;
    let (mut output_year, mut output_month, mut output_day) = civil_from_days(date);
    let seconds = hour * 3600 + minute * 60 + second;
    let day_delta = seconds.div_euclid(86_400);
    let seconds = seconds.rem_euclid(86_400);
    let (hour, remainder) = (seconds / 3600, seconds % 3600);
    let (minute, second) = (remainder / 60, remainder % 60);
    if day_delta != 0 {
        (output_year, output_month, output_day) = civil_from_days(date + day_delta);
    }
    output_year += cycles * 400;
    format!("{output_year:04}{output_month:02}{output_day:02}{hour:02}{minute:02}{second:02}000")
}

fn days_from_civil(mut year: i64, month: i64, day: i64) -> i64 {
    year -= i64::from(month <= 2);
    let era = (if year >= 0 { year } else { year - 399 }) / 400;
    let year_of_era = year - era * 400;
    let month_prime = month + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * month_prime + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

fn civil_from_days(mut days: i64) -> (i64, i64, i64) {
    days += 719_468;
    let era = (if days >= 0 { days } else { days - 146_096 }) / 146_097;
    let day_of_era = days - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    (year, month, day)
}

fn cell_for(value: &RawValue, wire: Option<&str>, json_blob: bool) -> Value {
    let value = if json_blob && wire.is_some_and(|wire| wire.eq_ignore_ascii_case("BLOB")) {
        match value {
            RawValue::Text(value) => RawValue::Bytes(value.as_bytes().to_vec()),
            _ => value.clone(),
        }
    } else {
        value.clone()
    };
    let value = match (&value, wire.map(str::to_ascii_uppercase).as_deref()) {
        (RawValue::Text(value), Some("BLOB")) if STANDARD.decode(value.as_bytes()).is_err() => {
            RawValue::Null
        }
        (RawValue::Text(value), Some("DECIMAL"))
            if value.eq_ignore_ascii_case("nan")
                || value.eq_ignore_ascii_case("infinity")
                || value.eq_ignore_ascii_case("+infinity")
                || value.eq_ignore_ascii_case("-infinity") =>
        {
            RawValue::Text(value.clone())
        }
        (RawValue::Text(value), Some("BIGDECIMAL"))
            if value.eq_ignore_ascii_case("nan")
                || value.eq_ignore_ascii_case("infinity")
                || value.eq_ignore_ascii_case("+infinity")
                || value.eq_ignore_ascii_case("-infinity") =>
        {
            RawValue::Null
        }
        (RawValue::Text(value), Some("FLOAT" | "DOUBLE")) if value.parse::<f64>().is_ok() => {
            let mut normalized = value.parse::<f64>().unwrap().to_string();
            if !normalized.contains(['.', 'e', 'E'])
                && normalized != "NaN"
                && !normalized.contains("inf")
            {
                normalized.push_str(".0");
            }
            RawValue::Text(normalized)
        }
        (RawValue::Text(value), Some("INT" | "SHORT" | "USHORT" | "UINT"))
            if value.parse::<i64>().is_err() =>
        {
            RawValue::Text("0".into())
        }
        (RawValue::Text(value), Some("FLOAT" | "DOUBLE")) if value.parse::<f64>().is_err() => {
            RawValue::Text("0.0".into())
        }
        (RawValue::Text(value), Some("DATE"))
            if value.len() != 8 || value.parse::<u64>().is_err() =>
        {
            RawValue::Null
        }
        (RawValue::Text(value), Some("TIME")) if value.len() == 6 => {
            RawValue::Text(format!("{value}000"))
        }
        (RawValue::Text(value), Some("TIME"))
            if !matches!(value.len(), 6 | 9) || value.parse::<u64>().is_err() =>
        {
            RawValue::Null
        }
        (RawValue::Text(value), Some("DATETIME")) if value.len() == 14 => {
            RawValue::Text(normalize_lenient_datetime(value))
        }
        (RawValue::Text(value), Some("DATETIME"))
            if !matches!(value.len(), 14 | 17) || value.parse::<u64>().is_err() =>
        {
            RawValue::Null
        }
        _ => value,
    };
    cell(&value)
}

fn cell(value: &RawValue) -> Value {
    match value {
        RawValue::Missing => json!({"state": "missing"}),
        RawValue::Null => json!({"state": "null"}),
        RawValue::Text(value) if value.is_empty() => json!({"state": "empty", "lexical": ""}),
        RawValue::Text(value) => json!({"state": "value", "lexical": value}),
        RawValue::Bytes(value) => json!({
            "state": "value",
            "lexical": STANDARD.encode(value),
        }),
    }
}

fn infer_wire(value: &RawValue) -> WireType {
    match value {
        RawValue::Bytes(_) => WireType::Blob,
        RawValue::Text(value)
            if value.eq_ignore_ascii_case("true") || value.eq_ignore_ascii_case("false") =>
        {
            WireType::Boolean
        }
        RawValue::Text(value) if value.parse::<i64>().is_ok() => WireType::Int,
        RawValue::Text(value) if value.parse::<f64>().is_ok() => WireType::Float,
        _ => WireType::String,
    }
}
fn canonical(document: &RawDocument, metadata: &Metadata, standalone_original: bool) -> Value {
    let is_ssv = metadata
        .root
        .as_ref()
        .and_then(|root| root.get("ssv"))
        .is_some();
    let parameters = document
        .parameters
        .iter()
        .enumerate()
        .filter(|(_, (id, value))| {
            !(is_ssv
                && (matches!(value, RawValue::Missing)
                    || metadata
                        .parameters
                        .get(id)
                        .is_none_or(|meta| meta.value_form.is_none())))
        })
        .map(|(_index, (id, value))| {
            let wire = metadata
                .parameters
                .get(id)
                .and_then(|meta| meta.wire.clone())
                .unwrap_or_else(|| infer_wire(value).to_string());
            let value = if !is_ssv && matches!(value, RawValue::Missing) {
                &RawValue::Null
            } else {
                value
            };
            let mut result = cell_for(value, Some(&wire), !metadata.root_nested && !is_ssv);
            let object = result.as_object_mut().unwrap();
            object.insert("id".into(), Value::String(id.clone()));
            object.insert("type".into(), Value::String(wire));
            if let Some(index) = metadata.parameters.get(id).and_then(|meta| meta.index) {
                object.insert("index".into(), json!(index));
            }
            Value::Object(object.clone())
        })
        .collect::<Vec<_>>();
    let datasets = document
        .datasets
        .iter()
        .map(|dataset| {
            let meta = metadata.datasets.get(&dataset.id);
            let const_first = meta.map(|meta| meta.const_first).unwrap_or(false);
            let columns = dataset
                .columns
                .iter()
                .enumerate()
                .map(|(index, (id, wire))| {
                    let index = if const_first {
                        dataset.const_columns.len() + index
                    } else {
                        index
                    };
                    let wire = meta
                        .and_then(|dataset| dataset.columns.get(id))
                        .and_then(|column| column.wire.clone())
                        .unwrap_or_else(|| wire.to_string());
                    json!({"id": id, "type": wire, "index": index})
                })
                .collect::<Vec<_>>();
            let const_columns = dataset
                .const_columns
                .iter()
                .enumerate()
                .filter(|(_, (_, _, value))| !(is_ssv && matches!(value, RawValue::Missing)))
                .map(|(index, (id, wire, _value))| {
                    let index = if const_first {
                        index
                    } else {
                        dataset.columns.len() + index
                    };
                    let wire = meta
                        .and_then(|dataset| dataset.const_columns.get(id))
                        .and_then(|column| column.wire.clone())
                        .unwrap_or_else(|| wire.to_string());
                    json!({"id": id, "type": wire, "index": index})
                })
                .collect::<Vec<_>>();
            let rows = dataset
                .rows
                .iter()
                .filter(|row| {
                    row.row_type != RowType::Delete
                        && !(standalone_original && row.row_type == RowType::Original)
                })
                .map(|row| {
                    let mut values = row.values.clone();
                    for (id, _, value) in &dataset.const_columns {
                        if !matches!(value, RawValue::Missing)
                            && !values.iter().any(|(column, _)| column == id)
                        {
                            values.push((id.clone(), value.clone()));
                        }
                    }
                    canonical_row(
                        row,
                        &dataset.columns,
                        &values,
                        meta,
                        !metadata.root_nested && !is_ssv,
                        !is_ssv,
                    )
                })
                .collect::<Vec<_>>();
            json!({"id": dataset.id, "columns": columns, "constColumns": const_columns, "rows": rows})
        })
        .collect::<Vec<_>>();
    let mut result = json!({"parameters": parameters, "datasets": datasets});
    if let Some(root) = &metadata.root {
        if !is_ssv && root.get("format").is_none() {
            result["wire"] = if metadata.root_nested {
                json!({"root": root})
            } else {
                root.clone()
            };
        }
    }
    result
}

fn canonical_row(
    row: &RawRow,
    columns: &[(String, WireType)],
    values: &[(String, RawValue)],
    metadata: Option<&DatasetMeta>,
    json_blob: bool,
    fill_missing: bool,
) -> Value {
    json!({
        "type": row_type(row.row_type),
        "orgRow": Value::Null,
        "values": canonical_values(values, columns, metadata, json_blob, fill_missing),
    })
}

fn canonical_values(
    values: &[(String, RawValue)],
    columns: &[(String, WireType)],
    metadata: Option<&DatasetMeta>,
    json_blob: bool,
    fill_missing: bool,
) -> Map<String, Value> {
    let mut result = values
        .iter()
        .map(|(id, value)| {
            let source_wire = columns
                .iter()
                .find(|(column, _)| column == id)
                .map(|(_, wire)| wire.to_string());
            let wire = metadata
                .and_then(|metadata| metadata.columns.get(id))
                .and_then(|column| column.wire.clone())
                .or_else(|| {
                    metadata
                        .and_then(|metadata| metadata.const_columns.get(id))
                        .and_then(|column| column.wire.clone())
                })
                .or_else(|| source_wire.clone());
            let validation_wire = source_wire.or_else(|| wire.clone());
            (
                id.clone(),
                cell_for(value, validation_wire.as_deref(), json_blob),
            )
        })
        .collect::<Map<_, _>>();
    if fill_missing {
        if let Some(metadata) = metadata {
            for (id, column) in metadata.columns.iter().chain(metadata.const_columns.iter()) {
                if !result.contains_key(id) {
                    result.insert(
                        id.clone(),
                        cell_for(&RawValue::Null, column.wire.as_deref(), json_blob),
                    );
                }
            }
        } else {
            for (id, wire) in columns {
                if !result.contains_key(id) {
                    result.insert(
                        id.clone(),
                        cell_for(&RawValue::Null, Some(&wire.to_string()), json_blob),
                    );
                }
            }
        }
    }
    result
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
    let mut datasets = object
        .get("datasets")?
        .as_array()?
        .iter()
        .map(raw_dataset)
        .collect::<Option<Vec<_>>>()?;
    if object.get("saveType").and_then(Value::as_u64) == Some(5) {
        for dataset in &mut datasets {
            dataset.rows.retain(|row| row.row_type != RowType::Normal);
        }
    }
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
