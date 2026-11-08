#![allow(dead_code)]

use base64::{engine::general_purpose::STANDARD, Engine};
use std::{
    collections::{BTreeMap, HashSet},
    io::{Read, Write},
};

use serde::de::{self, DeserializeSeed, Deserializer, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde::ser::{SerializeMap, Serializer};
use serde_json::value::RawValue;
use std::fmt;

use crate::{
    optimize_raw, Blob, DecodeError, EncodeError, Error, RawDataset, RawDocument, RawRow,
    RawValue as XValue, Result, RowType, WireType, XapiEncodeSink, XapiRoot, XapiRootDecoder,
};

pub fn decode_stream<T: XapiRoot, R: Read>(
    reader: R,
    limits: crate::Limits,
    strict: bool,
) -> Result<T> {
    let mut limited = reader.take(limits.max_document_bytes.saturating_add(1));
    let mut depth_reader = JsonDepthReader::new(&mut limited, limits.max_depth);
    let mut deserializer = serde_json::Deserializer::from_reader(&mut depth_reader);
    let mut decoder = T::decoder(strict, limits);
    deserializer
        .deserialize_any(RootVisitor::<T> {
            decoder: &mut decoder,
            limits,
            strict,
        })
        .map_err(|error| {
            if error.to_string().contains("limit exceeded: max_depth") {
                Error::from(DecodeError::Limit("max_depth"))
            } else {
                json_error(error)
            }
        })?;
    if limited.limit() == 0 {
        return Err(Error::from(DecodeError::Limit("max_document_bytes")));
    }
    decoder.finish()
}

struct JsonDepthReader<R> {
    inner: R,
    max_depth: usize,
    depth: usize,
    in_string: bool,
    escaped: bool,
}

impl<R> JsonDepthReader<R> {
    fn new(inner: R, max_depth: usize) -> Self {
        Self {
            inner,
            max_depth,
            depth: 0,
            in_string: false,
            escaped: false,
        }
    }
}

impl<R: Read> Read for JsonDepthReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let count = self.inner.read(buffer)?;
        for byte in &buffer[..count] {
            if self.in_string {
                if self.escaped {
                    self.escaped = false;
                } else if *byte == b'\\' {
                    self.escaped = true;
                } else if *byte == b'"' {
                    self.in_string = false;
                }
                continue;
            }
            match *byte {
                b'"' => self.in_string = true,
                b'{' | b'[' => {
                    self.depth += 1;
                    if self.depth > self.max_depth {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            "limit exceeded: max_depth",
                        ));
                    }
                }
                b'}' | b']' => self.depth = self.depth.saturating_sub(1),
                _ => {}
            }
        }
        Ok(count)
    }
}

struct RootVisitor<'a, T: XapiRoot> {
    decoder: &'a mut T::Decoder,
    limits: crate::Limits,
    strict: bool,
}
impl<'de, T: XapiRoot> Visitor<'de> for RootVisitor<'_, T> {
    type Value = ();
    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("an XAPI JSON root object")
    }
    fn visit_map<A: MapAccess<'de>>(
        self,
        mut map: A,
    ) -> std::result::Result<Self::Value, A::Error> {
        let mut seen = HashSet::new();
        while let Some(key) = map.next_key::<String>()? {
            if !seen.insert(key.to_ascii_lowercase()) {
                return Err(de::Error::custom("duplicate JSON object key"));
            }
            let normalized = key.to_ascii_lowercase();
            match normalized.as_str() {
                "version" => {
                    let version = map.next_value::<String>()?;
                    if self.strict && version != "1.0" {
                        return Err(de::Error::custom("unsupported Nexacro JSON version"));
                    }
                }
                "parameters" => {
                    map.next_value_seed(ParametersSeed::<T> {
                        decoder: self.decoder,
                        limits: self.limits,
                    })?;
                }
                "datasets" => {
                    map.next_value_seed(DatasetsSeed::<T> {
                        decoder: self.decoder,
                        limits: self.limits,
                    })?;
                }
                _ => {
                    self.decoder
                        .unknown_root_member(&key)
                        .map_err(de::Error::custom)?;
                    let _: IgnoredAny = map.next_value()?;
                }
            }
        }
        Ok(())
    }
}

struct ParametersSeed<'a, T: XapiRoot> {
    decoder: &'a mut T::Decoder,
    limits: crate::Limits,
}
impl<'de, T: XapiRoot> DeserializeSeed<'de> for ParametersSeed<'_, T> {
    type Value = ();
    fn deserialize<E: de::Deserializer<'de>>(
        self,
        deserializer: E,
    ) -> std::result::Result<(), E::Error> {
        deserializer.deserialize_any(ParametersVisitor::<T> {
            decoder: self.decoder,
            limits: self.limits,
        })
    }
}
struct ParametersVisitor<'a, T: XapiRoot> {
    decoder: &'a mut T::Decoder,
    limits: crate::Limits,
}
impl<'de, T: XapiRoot> Visitor<'de> for ParametersVisitor<'_, T> {
    type Value = ();
    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("XAPI parameters object or array")
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> std::result::Result<(), A::Error> {
        let mut seen = HashSet::new();
        while let Some(id) = map.next_key::<String>()? {
            if !seen.insert(id.clone()) {
                return Err(de::Error::custom("duplicate JSON object key"));
            }
            let raw: Box<RawValue> = map.next_value()?;
            let value = raw_scalar(&raw, self.limits).map_err(de::Error::custom)?;
            drop(raw);
            let value = optimize_for::<T>(value, parameter_wire::<T>(&id), self.limits)
                .map_err(de::Error::custom)?;
            self.decoder
                .parameter(&id, &value)
                .map_err(de::Error::custom)?;
        }
        Ok(())
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> std::result::Result<(), A::Error> {
        while seq
            .next_element_seed(ParameterItemSeed::<T> {
                decoder: self.decoder,
                limits: self.limits,
            })?
            .is_some()
        {}
        Ok(())
    }
}
struct ParameterItemSeed<'a, T: XapiRoot> {
    decoder: &'a mut T::Decoder,
    limits: crate::Limits,
}
impl<'de, T: XapiRoot> DeserializeSeed<'de> for ParameterItemSeed<'_, T> {
    type Value = ();
    fn deserialize<E: de::Deserializer<'de>>(
        self,
        deserializer: E,
    ) -> std::result::Result<(), E::Error> {
        deserializer.deserialize_map(ParameterItemVisitor::<T> {
            decoder: self.decoder,
            limits: self.limits,
        })
    }
}
struct ParameterItemVisitor<'a, T: XapiRoot> {
    decoder: &'a mut T::Decoder,
    limits: crate::Limits,
}
impl<'de, T: XapiRoot> Visitor<'de> for ParameterItemVisitor<'_, T> {
    type Value = ();
    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("an XAPI parameter")
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> std::result::Result<(), A::Error> {
        let mut id = None;
        let mut value = None;
        let mut seen = HashSet::new();
        while let Some(key) = map.next_key::<String>()? {
            let normalized = key.to_ascii_lowercase();
            if !seen.insert(normalized.clone()) {
                return Err(de::Error::custom("duplicate JSON object key"));
            }
            match normalized.as_str() {
                "id" => id = Some(map.next_value::<String>()?),
                "value" | "val" => value = Some(map.next_value::<Box<RawValue>>()?),
                _ => {
                    let _: IgnoredAny = map.next_value()?;
                }
            }
        }
        let id = id.ok_or_else(|| de::Error::custom("parameter id is required"))?;
        let value = value
            .map(|raw| raw_scalar(&raw, self.limits).map_err(de::Error::custom))
            .transpose()?
            .unwrap_or(XValue::Missing);
        let value = optimize_for::<T>(value, parameter_wire::<T>(&id), self.limits)
            .map_err(de::Error::custom)?;
        self.decoder
            .parameter(&id, &value)
            .map_err(de::Error::custom)
    }
}

struct DatasetsSeed<'a, T: XapiRoot> {
    decoder: &'a mut T::Decoder,
    limits: crate::Limits,
}
impl<'de, T: XapiRoot> DeserializeSeed<'de> for DatasetsSeed<'_, T> {
    type Value = ();
    fn deserialize<E: de::Deserializer<'de>>(
        self,
        deserializer: E,
    ) -> std::result::Result<(), E::Error> {
        deserializer.deserialize_seq(DatasetsVisitor::<T> {
            decoder: self.decoder,
            limits: self.limits,
            count: 0,
        })
    }
}
struct DatasetsVisitor<'a, T: XapiRoot> {
    decoder: &'a mut T::Decoder,
    limits: crate::Limits,
    count: usize,
}
impl<'de, T: XapiRoot> Visitor<'de> for DatasetsVisitor<'_, T> {
    type Value = ();
    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("an XAPI datasets array")
    }
    fn visit_seq<A: SeqAccess<'de>>(mut self, mut seq: A) -> std::result::Result<(), A::Error> {
        while seq
            .next_element_seed(DatasetSeed::<T> {
                decoder: self.decoder,
                limits: self.limits,
            })?
            .is_some()
        {
            self.count += 1;
            if self.count > self.limits.max_datasets {
                return Err(de::Error::custom("limit exceeded: max_datasets"));
            }
        }
        Ok(())
    }
}
struct DatasetSeed<'a, T: XapiRoot> {
    decoder: &'a mut T::Decoder,
    limits: crate::Limits,
}
impl<'de, T: XapiRoot> DeserializeSeed<'de> for DatasetSeed<'_, T> {
    type Value = ();
    fn deserialize<E: de::Deserializer<'de>>(
        self,
        deserializer: E,
    ) -> std::result::Result<(), E::Error> {
        deserializer.deserialize_map(DatasetVisitor::<T> {
            decoder: self.decoder,
            limits: self.limits,
            id: None,
            begun: false,
            columns: Vec::new(),
            const_columns: Vec::new(),
            rows: Vec::new(),
        })
    }
}
struct DatasetVisitor<'a, T: XapiRoot> {
    decoder: &'a mut T::Decoder,
    limits: crate::Limits,
    id: Option<String>,
    begun: bool,
    columns: Vec<(String, WireType)>,
    const_columns: Vec<(String, WireType, XValue)>,
    rows: Vec<RawRow>,
}
impl<'de, T: XapiRoot> Visitor<'de> for DatasetVisitor<'_, T> {
    type Value = ();
    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("an XAPI dataset object")
    }
    fn visit_map<A: MapAccess<'de>>(mut self, mut map: A) -> std::result::Result<(), A::Error> {
        let mut seen = HashSet::new();
        while let Some(key) = map.next_key::<String>()? {
            let normalized = key.to_ascii_lowercase();
            if !seen.insert(normalized.clone()) {
                return Err(de::Error::custom("duplicate JSON object key"));
            }
            match normalized.as_str() {
                "id" => {
                    let id = map.next_value::<String>()?;
                    self.decoder.dataset_begin(&id).map_err(de::Error::custom)?;
                    self.begun = true;
                    self.id = Some(id);
                }
                "columninfo" | "columns" => {
                    let info = map.next_value_seed(ColumnInfoSeed {
                        limits: self.limits,
                    })?;
                    self.columns = info.columns;
                    self.const_columns = info.const_columns;
                }
                "rows" => {
                    if let Some(id) = self.id.clone() {
                        map.next_value_seed(RowsDispatchSeed::<T> {
                            decoder: self.decoder,
                            dataset: id,
                            columns: self.columns.clone(),
                            has_blob: self.columns.iter().any(|(_, wire)| *wire == WireType::Blob)
                                || T::dataset_has_blob(self.id.as_deref().unwrap_or_default()),
                            limits: self.limits,
                        })?;
                    } else {
                        self.rows = map.next_value_seed(RowsSeed {
                            limits: self.limits,
                        })?;
                    }
                }
                _ => {
                    let _: IgnoredAny = map.next_value()?;
                }
            }
        }
        let id = self
            .id
            .ok_or_else(|| de::Error::custom("dataset id is required"))?;
        if !self.begun {
            self.decoder.dataset_begin(&id).map_err(de::Error::custom)?;
        }
        let mut all_columns = self.columns.clone();
        all_columns.extend(
            self.const_columns
                .iter()
                .map(|(id, wire, _)| (id.clone(), *wire)),
        );
        self.decoder
            .dataset_columns(&id, &all_columns)
            .map_err(de::Error::custom)?;
        for (column, wire, value) in &self.const_columns {
            self.decoder
                .dataset_const_column(&id, column, *wire, value)
                .map_err(de::Error::custom)?;
        }
        let has_blob = self.columns.iter().any(|(_, wire)| *wire == WireType::Blob)
            || T::dataset_has_blob(&id);
        for row in self.rows {
            let row = optimize_row::<T>(&id, row, &self.columns, has_blob, self.limits)
                .map_err(de::Error::custom)?;
            self.decoder
                .dataset_row(&id, row)
                .map_err(de::Error::custom)?;
        }
        Ok(())
    }
}
struct ColumnInfoSeed {
    limits: crate::Limits,
}
struct ColumnInfoData {
    columns: Vec<(String, WireType)>,
    const_columns: Vec<(String, WireType, XValue)>,
}
impl<'de> DeserializeSeed<'de> for ColumnInfoSeed {
    type Value = ColumnInfoData;
    fn deserialize<E: de::Deserializer<'de>>(
        self,
        deserializer: E,
    ) -> std::result::Result<Self::Value, E::Error> {
        deserializer.deserialize_any(ColumnInfoVisitor {
            limits: self.limits,
            count: 0,
        })
    }
}
struct ColumnInfoVisitor {
    limits: crate::Limits,
    count: usize,
}
impl<'de> Visitor<'de> for ColumnInfoVisitor {
    type Value = ColumnInfoData;
    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("an XAPI column info array or object")
    }
    fn visit_seq<A: SeqAccess<'de>>(
        mut self,
        mut seq: A,
    ) -> std::result::Result<ColumnInfoData, A::Error> {
        let mut columns = Vec::new();
        let mut seen = HashSet::new();
        while let Some(column) = seq.next_element_seed(ColumnItemSeed)? {
            self.count += 1;
            if self.count > self.limits.max_columns {
                return Err(de::Error::custom("limit exceeded: max_columns"));
            }
            if !seen.insert(column.0.clone()) {
                return Err(de::Error::custom("duplicate column id"));
            }
            columns.push(column);
        }
        Ok(ColumnInfoData {
            columns,
            const_columns: Vec::new(),
        })
    }

    fn visit_map<A: MapAccess<'de>>(
        self,
        mut map: A,
    ) -> std::result::Result<ColumnInfoData, A::Error> {
        let mut columns = Vec::new();
        let mut const_columns = Vec::new();
        let mut seen_keys = HashSet::new();
        while let Some(key) = map.next_key::<String>()? {
            let normalized = key.to_ascii_lowercase();
            if !seen_keys.insert(normalized.clone()) {
                return Err(de::Error::custom("duplicate JSON object key"));
            }
            match normalized.as_str() {
                "column" | "columns" => {
                    columns = map.next_value_seed(ColumnArraySeed {
                        limits: self.limits,
                    })?;
                }
                "constcolumn" | "constcolumns" => {
                    const_columns = map.next_value_seed(ConstColumnArraySeed {
                        limits: self.limits,
                    })?;
                }
                _ => {
                    let _: IgnoredAny = map.next_value()?;
                }
            }
        }
        let mut ids = HashSet::new();
        if columns.len().saturating_add(const_columns.len()) > self.limits.max_columns {
            return Err(de::Error::custom("limit exceeded: max_columns"));
        }
        for (id, _) in &columns {
            if !ids.insert(id) {
                return Err(de::Error::custom("duplicate column id"));
            }
        }
        for (id, _, _) in &const_columns {
            if !ids.insert(id) {
                return Err(de::Error::custom("duplicate column id"));
            }
        }
        Ok(ColumnInfoData {
            columns,
            const_columns,
        })
    }
}

struct ColumnArraySeed {
    limits: crate::Limits,
}
impl<'de> DeserializeSeed<'de> for ColumnArraySeed {
    type Value = Vec<(String, WireType)>;
    fn deserialize<E: de::Deserializer<'de>>(
        self,
        deserializer: E,
    ) -> std::result::Result<Self::Value, E::Error> {
        deserializer.deserialize_seq(ColumnArrayVisitor {
            limits: self.limits,
            count: 0,
        })
    }
}
struct ColumnArrayVisitor {
    limits: crate::Limits,
    count: usize,
}
impl<'de> Visitor<'de> for ColumnArrayVisitor {
    type Value = Vec<(String, WireType)>;
    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("an XAPI column array")
    }
    fn visit_seq<A: SeqAccess<'de>>(
        mut self,
        mut seq: A,
    ) -> std::result::Result<Self::Value, A::Error> {
        let mut columns = Vec::new();
        let mut seen = HashSet::new();
        while let Some(column) = seq.next_element_seed(ColumnItemSeed)? {
            self.count += 1;
            if self.count > self.limits.max_columns {
                return Err(de::Error::custom("limit exceeded: max_columns"));
            }
            if !seen.insert(column.0.clone()) {
                return Err(de::Error::custom("duplicate column id"));
            }
            columns.push(column);
        }
        Ok(columns)
    }
}

struct ConstColumnArraySeed {
    limits: crate::Limits,
}
impl<'de> DeserializeSeed<'de> for ConstColumnArraySeed {
    type Value = Vec<(String, WireType, XValue)>;
    fn deserialize<E: de::Deserializer<'de>>(
        self,
        deserializer: E,
    ) -> std::result::Result<Self::Value, E::Error> {
        deserializer.deserialize_seq(ConstColumnArrayVisitor {
            limits: self.limits,
            count: 0,
        })
    }
}
struct ConstColumnArrayVisitor {
    limits: crate::Limits,
    count: usize,
}
impl<'de> Visitor<'de> for ConstColumnArrayVisitor {
    type Value = Vec<(String, WireType, XValue)>;
    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("an XAPI constant column array")
    }
    fn visit_seq<A: SeqAccess<'de>>(
        mut self,
        mut seq: A,
    ) -> std::result::Result<Self::Value, A::Error> {
        let mut columns = Vec::new();
        let mut seen = HashSet::new();
        while let Some(column) = seq.next_element_seed(ConstColumnItemSeed {
            limits: self.limits,
        })? {
            self.count += 1;
            if self.count > self.limits.max_columns {
                return Err(de::Error::custom("limit exceeded: max_columns"));
            }
            if !seen.insert(column.0.clone()) {
                return Err(de::Error::custom("duplicate column id"));
            }
            columns.push(column);
        }
        Ok(columns)
    }
}

struct ConstColumnItemSeed {
    limits: crate::Limits,
}
impl<'de> DeserializeSeed<'de> for ConstColumnItemSeed {
    type Value = (String, WireType, XValue);
    fn deserialize<E: de::Deserializer<'de>>(
        self,
        deserializer: E,
    ) -> std::result::Result<Self::Value, E::Error> {
        deserializer.deserialize_map(ConstColumnItemVisitor {
            limits: self.limits,
        })
    }
}
struct ConstColumnItemVisitor {
    limits: crate::Limits,
}
impl<'de> Visitor<'de> for ConstColumnItemVisitor {
    type Value = (String, WireType, XValue);
    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("an XAPI constant column descriptor")
    }
    fn visit_map<A: MapAccess<'de>>(
        self,
        mut map: A,
    ) -> std::result::Result<Self::Value, A::Error> {
        let mut seen = HashSet::new();
        let mut id = None;
        let mut wire = None;
        let mut raw_value = None;
        while let Some(key) = map.next_key::<String>()? {
            let normalized = key.to_ascii_lowercase();
            if !seen.insert(normalized.clone()) {
                return Err(de::Error::custom("duplicate JSON object key"));
            }
            match normalized.as_str() {
                "id" => id = Some(map.next_value::<String>()?),
                "type" => {
                    let value = map.next_value::<String>()?;
                    wire = Some(parse_wire(Some(&value)).map_err(de::Error::custom)?);
                }
                "value" => {
                    let raw: Box<RawValue> = map.next_value()?;
                    raw_value = Some(raw_scalar(&raw, self.limits).map_err(de::Error::custom)?);
                }
                _ => {
                    let _: IgnoredAny = map.next_value()?;
                }
            }
        }
        Ok((
            id.ok_or_else(|| de::Error::custom("column id is required"))?,
            wire.unwrap_or(WireType::String),
            raw_value.unwrap_or(XValue::Missing),
        ))
    }
}

struct ColumnItemSeed;

impl<'de> DeserializeSeed<'de> for ColumnItemSeed {
    type Value = (String, WireType);

    fn deserialize<E: de::Deserializer<'de>>(
        self,
        deserializer: E,
    ) -> std::result::Result<Self::Value, E::Error> {
        deserializer.deserialize_map(ColumnItemVisitor)
    }
}

struct ColumnItemVisitor;

impl<'de> Visitor<'de> for ColumnItemVisitor {
    type Value = (String, WireType);

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("an XAPI column descriptor")
    }

    fn visit_map<A: MapAccess<'de>>(
        self,
        mut map: A,
    ) -> std::result::Result<Self::Value, A::Error> {
        let mut seen = HashSet::new();
        let mut id = None;
        let mut wire = None;
        while let Some(key) = map.next_key::<String>()? {
            if !seen.insert(key.clone()) {
                return Err(de::Error::custom("duplicate JSON object key"));
            }
            match key.to_ascii_lowercase().as_str() {
                "id" => id = Some(map.next_value::<String>()?),
                "type" => {
                    let value = map.next_value::<String>()?;
                    wire = Some(parse_wire(Some(&value)).map_err(de::Error::custom)?);
                }
                _ => {
                    let _: IgnoredAny = map.next_value()?;
                }
            }
        }
        Ok((
            id.ok_or_else(|| de::Error::custom("column id is required"))?,
            wire.unwrap_or(WireType::String),
        ))
    }
}

struct RowsSeed {
    limits: crate::Limits,
}

struct RowsDispatchSeed<'a, T: XapiRoot> {
    decoder: &'a mut T::Decoder,
    dataset: String,
    columns: Vec<(String, WireType)>,
    has_blob: bool,
    limits: crate::Limits,
}

impl<'de, T: XapiRoot> DeserializeSeed<'de> for RowsDispatchSeed<'_, T> {
    type Value = ();

    fn deserialize<E: de::Deserializer<'de>>(
        self,
        deserializer: E,
    ) -> std::result::Result<(), E::Error> {
        deserializer.deserialize_seq(RowsDispatchVisitor::<T> {
            decoder: self.decoder,
            dataset: self.dataset,
            columns: self.columns,
            has_blob: self.has_blob,
            limits: self.limits,
            count: 0,
            pending: None,
        })
    }
}

struct RowsDispatchVisitor<'a, T: XapiRoot> {
    decoder: &'a mut T::Decoder,
    dataset: String,
    columns: Vec<(String, WireType)>,
    has_blob: bool,
    limits: crate::Limits,
    count: usize,
    pending: Option<RawRow>,
}

impl<'de, T: XapiRoot> Visitor<'de> for RowsDispatchVisitor<'_, T> {
    type Value = ();

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("an XAPI rows array")
    }

    fn visit_seq<A: SeqAccess<'de>>(mut self, mut seq: A) -> std::result::Result<(), A::Error> {
        while let Some(row) = seq.next_element_seed(RowSeed {
            limits: self.limits,
        })? {
            let row = optimize_row::<T>(
                &self.dataset,
                row,
                &self.columns,
                self.has_blob,
                self.limits,
            )
            .map_err(de::Error::custom)?;
            if row.row_type == RowType::Original {
                let Some(current) = self.pending.as_mut() else {
                    return Err(de::Error::custom(
                        "original row must follow a materialized row",
                    ));
                };
                if current.original.is_some() {
                    return Err(de::Error::custom("row has duplicate original values"));
                }
                current.original = Some(row.values);
            } else {
                if let Some(current) = self.pending.take() {
                    self.decoder
                        .dataset_row(&self.dataset, current)
                        .map_err(de::Error::custom)?;
                }
                self.count += 1;
                if self.count > self.limits.max_rows {
                    return Err(de::Error::custom("limit exceeded: max_rows"));
                }
                self.pending = Some(row);
            }
        }
        if let Some(current) = self.pending.take() {
            self.decoder
                .dataset_row(&self.dataset, current)
                .map_err(de::Error::custom)?;
        }
        Ok(())
    }
}

impl<'de> DeserializeSeed<'de> for RowsSeed {
    type Value = Vec<RawRow>;
    fn deserialize<E: de::Deserializer<'de>>(
        self,
        deserializer: E,
    ) -> std::result::Result<Self::Value, E::Error> {
        deserializer.deserialize_seq(RowsVisitor {
            limits: self.limits,
            count: 0,
            pending: None,
        })
    }
}
struct RowsVisitor {
    limits: crate::Limits,
    count: usize,
    pending: Option<RawRow>,
}
impl<'de> Visitor<'de> for RowsVisitor {
    type Value = Vec<RawRow>;
    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("an XAPI rows array")
    }
    fn visit_seq<A: SeqAccess<'de>>(
        mut self,
        mut seq: A,
    ) -> std::result::Result<Vec<RawRow>, A::Error> {
        let mut rows = Vec::new();
        while let Some(row) = seq.next_element_seed(RowSeed {
            limits: self.limits,
        })? {
            if row.row_type == RowType::Original {
                let Some(current) = self.pending.as_mut() else {
                    return Err(de::Error::custom(
                        "original row must follow a materialized row",
                    ));
                };
                if current.original.is_some() {
                    return Err(de::Error::custom("row has duplicate original values"));
                }
                current.original = Some(row.values);
            } else {
                if let Some(current) = self.pending.take() {
                    rows.push(current);
                }
                self.count += 1;
                if self.count > self.limits.max_rows {
                    return Err(de::Error::custom("limit exceeded: max_rows"));
                }
                self.pending = Some(row);
            }
        }
        if let Some(current) = self.pending.take() {
            rows.push(current);
        }
        Ok(rows)
    }
}
struct RowSeed {
    limits: crate::Limits,
}
impl<'de> DeserializeSeed<'de> for RowSeed {
    type Value = RawRow;
    fn deserialize<E: de::Deserializer<'de>>(
        self,
        deserializer: E,
    ) -> std::result::Result<RawRow, E::Error> {
        deserializer.deserialize_map(RowVisitor {
            limits: self.limits,
            kind: RowType::Normal,
            values: Vec::new(),
            original: None,
        })
    }
}
struct RowVisitor {
    limits: crate::Limits,
    kind: RowType,
    values: Vec<(String, XValue)>,
    original: Option<Vec<(String, XValue)>>,
}
impl<'de> Visitor<'de> for RowVisitor {
    type Value = RawRow;
    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("an XAPI row object")
    }
    fn visit_map<A: MapAccess<'de>>(mut self, mut map: A) -> std::result::Result<RawRow, A::Error> {
        let mut seen_insert = false;
        let mut seen_update = false;
        let mut seen_delete = false;
        let mut seen_original = false;
        while let Some(key) = map.next_key::<String>()? {
            let duplicate = match key.as_str() {
                "_RowType_" => std::mem::replace(&mut seen_update, true),
                "I" => std::mem::replace(&mut seen_insert, true),
                "U" => std::mem::replace(&mut seen_update, true),
                "D" => std::mem::replace(&mut seen_delete, true),
                "O" => std::mem::replace(&mut seen_original, true),
                _ => self.values.iter().any(|(id, _)| id == &key),
            };
            if duplicate {
                return Err(de::Error::custom("duplicate JSON object key"));
            }
            match key.as_str() {
                "_RowType_" => {
                    let row_type = map.next_value::<String>()?;
                    self.kind = match row_type.to_ascii_uppercase().as_str() {
                        "N" => RowType::Normal,
                        "I" => RowType::Insert,
                        "U" => RowType::Update,
                        "D" => RowType::Delete,
                        "O" => RowType::Original,
                        _ => return Err(de::Error::custom("invalid JSON row type")),
                    };
                }
                "I" => {
                    self.kind = RowType::Insert;
                    self.values = map.next_value_seed(RowValuesSeed {
                        limits: self.limits,
                    })?;
                }
                "U" => {
                    self.kind = RowType::Update;
                    self.values = map.next_value_seed(RowValuesSeed {
                        limits: self.limits,
                    })?;
                }
                "D" => {
                    self.kind = RowType::Delete;
                    self.values = map.next_value_seed(RowValuesSeed {
                        limits: self.limits,
                    })?;
                }
                "O" => {
                    self.original = Some(map.next_value_seed(RowValuesSeed {
                        limits: self.limits,
                    })?)
                }
                _ => {
                    let raw: Box<RawValue> = map.next_value()?;
                    self.values.push((
                        key,
                        raw_scalar(&raw, self.limits).map_err(de::Error::custom)?,
                    ));
                }
            }
        }
        Ok(RawRow {
            row_type: self.kind,
            values: self.values,
            original: self.original,
        })
    }
}
struct RowValuesSeed {
    limits: crate::Limits,
}
impl<'de> DeserializeSeed<'de> for RowValuesSeed {
    type Value = Vec<(String, XValue)>;
    fn deserialize<E: de::Deserializer<'de>>(
        self,
        deserializer: E,
    ) -> std::result::Result<Self::Value, E::Error> {
        deserializer.deserialize_map(RowValuesVisitor {
            limits: self.limits,
        })
    }
}
struct RowValuesVisitor {
    limits: crate::Limits,
}
impl<'de> Visitor<'de> for RowValuesVisitor {
    type Value = Vec<(String, XValue)>;
    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("an XAPI row values object")
    }
    fn visit_map<A: MapAccess<'de>>(
        self,
        mut map: A,
    ) -> std::result::Result<Self::Value, A::Error> {
        let mut values = Vec::new();
        while let Some(key) = map.next_key::<String>()? {
            if values.iter().any(|(id, _)| id == &key) {
                return Err(de::Error::custom("duplicate JSON object key"));
            }
            let raw: Box<RawValue> = map.next_value()?;
            values.push((
                key,
                raw_scalar(&raw, self.limits).map_err(de::Error::custom)?,
            ));
        }
        Ok(values)
    }
}

fn raw_scalar(raw: &RawValue, limits: crate::Limits) -> Result<XValue> {
    if raw.get().len() > limits.max_scalar_bytes {
        return Err(Error::from(DecodeError::Limit("max_scalar_bytes")));
    }
    scalar(raw.get())
}

fn parameter_wire<T: XapiRoot>(id: &str) -> Option<WireType> {
    T::schema()
        .fields
        .iter()
        .find(|field| field.kind == crate::RootFieldKind::Parameter && field.id == id)
        .map(|field| field.wire)
}

fn optimize_row<T: XapiRoot>(
    dataset: &str,
    mut row: RawRow,
    columns: &[(String, WireType)],
    has_blob: bool,
    limits: crate::Limits,
) -> Result<RawRow> {
    if !has_blob {
        return Ok(row);
    }
    for (id, value) in &mut row.values {
        let wire = columns
            .iter()
            .find(|(column, _)| column == id)
            .map(|(_, wire)| *wire)
            .or_else(|| T::dataset_wire(dataset, id));
        let current = std::mem::replace(value, XValue::Missing);
        *value = optimize_for::<T>(current, wire, limits)?;
    }
    if let Some(original) = row.original.as_mut() {
        for (id, value) in original {
            let wire = columns
                .iter()
                .find(|(column, _)| column == id)
                .map(|(_, wire)| *wire)
                .or_else(|| T::dataset_wire(dataset, id));
            let current = std::mem::replace(value, XValue::Missing);
            *value = optimize_for::<T>(current, wire, limits)?;
        }
    }
    Ok(row)
}

pub fn decode_raw<R: Read>(reader: R, limits: crate::Limits) -> Result<RawDocument> {
    let mut limited = reader.take(limits.max_document_bytes.saturating_add(1));
    let object: BTreeMap<String, Box<RawValue>> =
        serde_json::from_reader(&mut limited).map_err(json_error)?;
    if limited.limit() == 0 {
        return Err(Error::from(DecodeError::Limit("max_document_bytes")));
    }
    let mut document = RawDocument::default();
    if let Some(raw) = find(&object, "Parameters") {
        document.parameters = parameters(raw)?;
    }
    if let Some(raw) = find(&object, "Datasets") {
        document.datasets = datasets(raw, limits)?;
    }
    Ok(document)
}

fn parameters(raw: &RawValue) -> Result<Vec<(String, XValue)>> {
    if raw.get().trim_start().starts_with('{') {
        let object: BTreeMap<String, Box<RawValue>> =
            serde_json::from_str(raw.get()).map_err(json_error)?;
        return object
            .into_iter()
            .map(|(id, value)| Ok((id, scalar(value.get())?)))
            .collect();
    }
    let items: Vec<Box<RawValue>> = serde_json::from_str(raw.get()).map_err(json_error)?;
    items
        .into_iter()
        .map(|item| {
            let object: BTreeMap<String, Box<RawValue>> =
                serde_json::from_str(item.get()).map_err(json_error)?;
            let id = string_field(&object, "id")?
                .ok_or_else(|| Error::from(DecodeError::message("parameter id is required")))?;
            let value = find(&object, "value")
                .or_else(|| find(&object, "val"))
                .map(|value| scalar(value.get()))
                .transpose()?
                .unwrap_or(XValue::Null);
            Ok((id, value))
        })
        .collect()
}

type ColumnInfo = (Vec<(String, WireType)>, Vec<(String, WireType, XValue)>);
fn column_info(raw: &RawValue, limits: crate::Limits) -> Result<ColumnInfo> {
    if raw.get().trim_start().starts_with('[') {
        return Ok((columns(raw, limits)?, Vec::new()));
    }
    let object: BTreeMap<String, Box<RawValue>> =
        serde_json::from_str(raw.get()).map_err(json_error)?;
    let columns = find(&object, "Column")
        .or_else(|| find(&object, "columns"))
        .map(|raw| columns(raw, limits))
        .transpose()?
        .unwrap_or_default();
    let constants = find(&object, "ConstColumn")
        .or_else(|| find(&object, "constColumns"))
        .map(|raw| {
            let items: Vec<Box<RawValue>> = serde_json::from_str(raw.get()).map_err(json_error)?;
            items
                .into_iter()
                .map(|item| {
                    let object: BTreeMap<String, Box<RawValue>> =
                        serde_json::from_str(item.get()).map_err(json_error)?;
                    let id = string_field(&object, "id")?.ok_or_else(|| {
                        Error::from(DecodeError::message("constant column id is required"))
                    })?;
                    let wire = parse_wire(string_field(&object, "type")?.as_deref())?;
                    let value = find(&object, "value")
                        .or_else(|| find(&object, "val"))
                        .map(|value| scalar(value.get()))
                        .transpose()?
                        .unwrap_or(XValue::Missing);
                    Ok::<(String, WireType, XValue), Error>((id, wire, value))
                })
                .collect::<Result<Vec<_>>>()
        })
        .transpose()?
        .unwrap_or_default();
    Ok((columns, constants))
}

fn datasets(raw: &RawValue, limits: crate::Limits) -> Result<Vec<RawDataset>> {
    let items: Vec<Box<RawValue>> = serde_json::from_str(raw.get()).map_err(json_error)?;
    items
        .into_iter()
        .map(|item| {
            let object: BTreeMap<String, Box<RawValue>> =
                serde_json::from_str(item.get()).map_err(json_error)?;
            let id = string_field(&object, "id")?
                .ok_or_else(|| Error::from(DecodeError::message("dataset id is required")))?;
            let (columns, const_columns) =
                match find(&object, "ColumnInfo").or_else(|| find(&object, "columns")) {
                    Some(raw_columns) => column_info(raw_columns, limits)?,
                    None => (Vec::new(), Vec::new()),
                };
            let rows = match find(&object, "Rows").or_else(|| find(&object, "rows")) {
                Some(raw_rows) => rows(raw_rows)?,
                None => Vec::new(),
            };
            Ok(RawDataset {
                id,
                columns,
                column_sizes: Vec::new(),
                column_meta: Vec::new(),
                const_columns,
                const_column_sizes: Vec::new(),
                rows,
            })
        })
        .collect()
}

fn columns(raw: &RawValue, limits: crate::Limits) -> Result<Vec<(String, WireType)>> {
    let items: Vec<Box<RawValue>> = serde_json::from_str(raw.get()).map_err(json_error)?;
    if items.len() > limits.max_columns {
        return Err(Error::from(DecodeError::Limit("max_columns")));
    }
    items
        .into_iter()
        .map(|item| {
            let object: BTreeMap<String, Box<RawValue>> =
                serde_json::from_str(item.get()).map_err(json_error)?;
            let id = string_field(&object, "id")?
                .ok_or_else(|| Error::from(DecodeError::message("column id is required")))?;
            let wire = parse_wire(string_field(&object, "type")?.as_deref())?;
            Ok((id, wire))
        })
        .collect()
}

fn parse_wire(value: Option<&str>) -> Result<WireType> {
    match value.unwrap_or("STRING").to_ascii_uppercase().as_str() {
        "STRING" => Ok(WireType::String),
        "SHORT" | "USHORT" | "INT" | "UINT" => Ok(WireType::Int),
        "LONG" | "ULONG" => Ok(WireType::Long),
        "FLOAT" => Ok(WireType::Float),
        "DOUBLE" => Ok(WireType::Double),
        "DECIMAL" => Ok(WireType::Decimal),
        "BIGDECIMAL" => Ok(WireType::BigDecimal),
        "DATE" => Ok(WireType::Date),
        "TIME" => Ok(WireType::Time),
        "DATETIME" => Ok(WireType::DateTime),
        "BLOB" | "FILE" => Ok(WireType::Blob),
        "BOOLEAN" | "BOOL" => Ok(WireType::Boolean),
        _ => Ok(WireType::String),
    }
}

fn rows(raw: &RawValue) -> Result<Vec<RawRow>> {
    let items: Vec<Box<RawValue>> = serde_json::from_str(raw.get()).map_err(json_error)?;
    let mut rows: Vec<RawRow> = Vec::with_capacity(items.len());
    for item in items {
        let object: BTreeMap<String, Box<RawValue>> =
            serde_json::from_str(item.get()).map_err(json_error)?;
        let marker = string_field(&object, "_RowType_")?;
        match marker.as_deref() {
            Some("O") => {
                let original = object_values_without(object, "_RowType_")?;
                let Some(previous) = rows.last_mut() else {
                    return Err(Error::from(DecodeError::message(
                        "original row must follow a materialized row",
                    )));
                };
                if previous.original.replace(original).is_some() {
                    return Err(Error::from(DecodeError::message(
                        "row has duplicate original values",
                    )));
                }
            }
            Some("I") | Some("U") | Some("D") | Some("N") => {
                let row_type = match marker.as_deref().unwrap_or("N") {
                    "I" => RowType::Insert,
                    "U" => RowType::Update,
                    "D" => RowType::Delete,
                    _ => RowType::Normal,
                };
                rows.push(RawRow {
                    row_type,
                    values: object_values_without(object, "_RowType_")?,
                    original: None,
                });
            }
            Some(other) => {
                return Err(Error::from(DecodeError::message(format!(
                    "unsupported JSON row type: {other}",
                ))));
            }
            None => {
                let (row_type, values, original) = if let Some((kind, raw)) = [
                    ("I", RowType::Insert),
                    ("U", RowType::Update),
                    ("D", RowType::Delete),
                ]
                .iter()
                .find_map(|(key, kind)| find(&object, key).map(|raw| (*kind, raw)))
                {
                    (
                        kind,
                        object_values(raw)?,
                        find(&object, "O").map(object_values).transpose()?,
                    )
                } else {
                    (RowType::Normal, object_values_map(object)?, None)
                };
                rows.push(RawRow {
                    row_type,
                    values,
                    original,
                });
            }
        }
    }
    Ok(rows)
}
fn object_values(raw: &RawValue) -> Result<Vec<(String, XValue)>> {
    let object: BTreeMap<String, Box<RawValue>> =
        serde_json::from_str(raw.get()).map_err(json_error)?;
    object_values_map(object)
}

fn object_values_without(
    object: BTreeMap<String, Box<RawValue>>,
    excluded: &str,
) -> Result<Vec<(String, XValue)>> {
    object_values_map(
        object
            .into_iter()
            .filter(|(id, _)| !id.eq_ignore_ascii_case(excluded))
            .collect(),
    )
}

fn object_values_map(object: BTreeMap<String, Box<RawValue>>) -> Result<Vec<(String, XValue)>> {
    object
        .into_iter()
        .map(|(id, value)| Ok((id, scalar(value.get())?)))
        .collect()
}

fn scalar(value: &str) -> Result<XValue> {
    if value == "null" {
        return Ok(XValue::Null);
    }
    if value.starts_with('"') {
        return serde_json::from_str::<String>(value)
            .map(XValue::Text)
            .map_err(json_error);
    }
    if value == "true" || value == "false" || value.parse::<serde_json::Number>().is_ok() {
        return Ok(XValue::Text(value.to_owned()));
    }
    Err(Error::from(DecodeError::message(
        "JSON scalar must be string, number, boolean, or null",
    )))
}

fn find<'a>(object: &'a BTreeMap<String, Box<RawValue>>, key: &str) -> Option<&'a RawValue> {
    object
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(key))
        .map(|(_, value)| value.as_ref())
}
fn string_field(object: &BTreeMap<String, Box<RawValue>>, key: &str) -> Result<Option<String>> {
    find(object, key)
        .map(|raw| serde_json::from_str(raw.get()).map_err(json_error))
        .transpose()
}
fn json_error(error: serde_json::Error) -> Error {
    Error::from(DecodeError::message(format!("invalid JSON: {error}")))
}

pub fn encode<T: XapiRoot, W: Write>(output: W, value: &T) -> Result<()> {
    let document = value.encode_raw()?;
    let mut serializer = serde_json::Serializer::new(output);
    let mut root = serializer.serialize_map(Some(2)).map_err(io_error)?;
    let mut parameters = BTreeMap::new();
    for (id, value) in &document.parameters {
        parameters.insert(id.clone(), json_value(value, None).map_err(Error::from)?);
    }
    root.serialize_entry("Parameters", &parameters)
        .map_err(io_error)?;
    let mut datasets = Vec::new();
    for dataset in &document.datasets {
        let mut object = BTreeMap::new();
        object.insert("id".into(), raw_string(&dataset.id).map_err(Error::from)?);
        let mut columns = Vec::new();
        for (id, wire) in &dataset.columns {
            let mut column = BTreeMap::new();
            column.insert("id".into(), raw_string(id).map_err(Error::from)?);
            column.insert(
                "type".into(),
                raw_string(&wire.to_string()).map_err(Error::from)?,
            );
            columns.push(raw_object(column).map_err(Error::from)?);
        }
        if dataset.const_columns.is_empty() {
            object.insert(
                "ColumnInfo".into(),
                raw_array(columns).map_err(Error::from)?,
            );
        } else {
            let mut const_columns = Vec::new();
            for (id, wire, value) in &dataset.const_columns {
                let mut column = BTreeMap::new();
                column.insert("id".into(), raw_string(id).map_err(Error::from)?);
                column.insert(
                    "type".into(),
                    raw_string(&wire.to_string()).map_err(Error::from)?,
                );
                column.insert(
                    "value".into(),
                    json_value(value, Some(*wire)).map_err(Error::from)?,
                );
                const_columns.push(raw_object(column).map_err(Error::from)?);
            }
            let mut info = BTreeMap::new();
            info.insert("Column".into(), raw_array(columns).map_err(Error::from)?);
            info.insert(
                "ConstColumn".into(),
                raw_array(const_columns).map_err(Error::from)?,
            );
            object.insert("ColumnInfo".into(), raw_object(info).map_err(Error::from)?);
        }
        let mut rows = Vec::new();
        for row in &dataset.rows {
            let mut raw_row = BTreeMap::new();
            for (id, raw) in &row.values {
                let wire = dataset
                    .columns
                    .iter()
                    .find(|(name, _)| name == id)
                    .map(|(_, wire)| *wire);
                if !matches!(raw, XValue::Missing) {
                    raw_row.insert(id.clone(), json_value(raw, wire).map_err(Error::from)?);
                }
            }
            if row.row_type == RowType::Normal && row.original.is_none() {
                rows.push(raw_object(raw_row).map_err(Error::from)?);
            } else {
                let key = match row.row_type {
                    RowType::Insert => "I",
                    RowType::Update => "U",
                    RowType::Delete => "D",
                    RowType::Normal | RowType::Original => "U",
                };
                let mut wrapper = BTreeMap::new();
                wrapper.insert(key.into(), raw_object(raw_row).map_err(Error::from)?);
                if let Some(original) = &row.original {
                    let mut original_row = BTreeMap::new();
                    for (id, raw) in original {
                        if !matches!(raw, XValue::Missing) {
                            original_row.insert(
                                id.clone(),
                                json_value(
                                    raw,
                                    dataset
                                        .columns
                                        .iter()
                                        .find(|(name, _)| name == id)
                                        .map(|(_, wire)| *wire),
                                )
                                .map_err(Error::from)?,
                            );
                        }
                    }
                    wrapper.insert("O".into(), raw_object(original_row).map_err(Error::from)?);
                }
                rows.push(raw_object(wrapper).map_err(Error::from)?);
            }
        }
        object.insert("Rows".into(), raw_array(rows).map_err(Error::from)?);
        datasets.push(raw_object(object).map_err(Error::from)?);
    }
    root.serialize_entry("Datasets", &datasets)
        .map_err(io_error)?;
    SerializeMap::end(root).map_err(io_error)
}

pub fn encode_stream<T: XapiRoot, W: Write>(output: W, value: &T) -> Result<()> {
    let mut sink = JsonSink::new(output);
    value.encode_stream(&mut sink)
}

/// Encodes a runtime-schema document using the public Nexacro JSON shape.
pub fn encode_raw<W: Write>(mut output: W, document: &crate::RawDocument) -> Result<()> {
    use serde_json::{Map, Value};
    let mut root = Map::new();
    if !document.parameters.is_empty() {
        root.insert(
            "Parameters".into(),
            Value::Array(
                document
                    .parameters
                    .iter()
                    .enumerate()
                    .map(|(index, (id, raw))| {
                        let mut item = Map::new();
                        item.insert("id".into(), Value::String(id.clone()));
                        let wire_name = document
                            .parameter_wires
                            .get(index)
                            .and_then(|wire| wire.as_deref());
                        let wire = wire_name.and_then(|wire| parse_wire(Some(wire)).ok());
                        item.insert(
                            "type".into(),
                            Value::String(
                                wire.map(json_wire_name).unwrap_or_else(|| "string".into()),
                            ),
                        );
                        let value = match (wire_name, raw) {
                            (Some("UINT" | "USHORT"), XValue::Text(value))
                                if value.parse::<i32>().is_err() =>
                            {
                                XValue::Text("0".into())
                            }
                            (Some("ULONG" | "LONG"), XValue::Text(value))
                                if value.parse::<i64>().is_err() =>
                            {
                                XValue::Text("0".into())
                            }
                            _ => raw.clone(),
                        };
                        item.insert("value".into(), raw_json(&value, wire));
                        Value::Object(item)
                    })
                    .collect(),
            ),
        );
    }
    let parameters = root.remove("Parameters");
    if !document.datasets.is_empty() {
        root.insert(
            "Datasets".into(),
            Value::Array(
                document
                    .datasets
                    .iter()
                    .map(|dataset| {
                        let mut item = Map::new();
                        let mut info = Map::new();
                        let columns: Vec<Value> = dataset
                            .columns
                            .iter()
                            .enumerate()
                            .map(|(index, (id, wire))| {
                                let mut column = Map::new();
                                let size = dataset
                                    .column_sizes
                                    .get(index)
                                    .and_then(Option::as_ref)
                                    .cloned()
                                    .unwrap_or_else(|| json_default_size(*wire).to_owned());
                                column.insert("size".into(), Value::String(size));
                                column.insert("id".into(), Value::String(id.clone()));
                                column.insert("type".into(), Value::String(json_wire_name(*wire)));
                                Value::Object(column)
                            })
                            .collect();
                        let const_columns: Vec<Value> = dataset
                            .const_columns
                            .iter()
                            .enumerate()
                            .map(|(index, (id, wire, raw))| {
                                let mut column = Map::new();
                                let size = dataset
                                    .const_column_sizes
                                    .get(index)
                                    .and_then(Option::as_ref)
                                    .cloned()
                                    .unwrap_or_else(|| json_default_size(*wire).to_owned());
                                column.insert("size".into(), Value::String(size));
                                column.insert("id".into(), Value::String(id.clone()));
                                column.insert("type".into(), Value::String(json_wire_name(*wire)));
                                column.insert("value".into(), raw_json(raw, Some(*wire)));
                                Value::Object(column)
                            })
                            .collect();
                        if !const_columns.is_empty() {
                            info.insert("ConstColumn".into(), Value::Array(const_columns));
                        }
                        if !columns.is_empty() {
                            info.insert("Column".into(), Value::Array(columns));
                        }
                        if !info.is_empty() {
                            item.insert("ColumnInfo".into(), Value::Object(info));
                        }
                        item.insert("id".into(), Value::String(dataset.id.clone()));
                        if !dataset.rows.is_empty() {
                            item.insert(
                                "Rows".into(),
                                Value::Array(
                                    dataset
                                        .rows
                                        .iter()
                                        .map(|row| {
                                            let mut values = Map::new();
                                            for (id, raw) in &row.values {
                                                let wire = dataset
                                                    .columns
                                                    .iter()
                                                    .find(|(name, _)| name == id)
                                                    .map(|(_, wire)| *wire);
                                                match raw {
                                                    XValue::Missing => {
                                                        if wire == Some(WireType::Boolean) {
                                                            values.insert(
                                                                id.clone(),
                                                                Value::String("0".into()),
                                                            );
                                                        }
                                                    }
                                                    XValue::Null => {}
                                                    _ => {
                                                        values.insert(
                                                            id.clone(),
                                                            raw_json(raw, wire),
                                                        );
                                                    }
                                                }
                                            }
                                            for (id, wire) in &dataset.columns {
                                                if *wire == WireType::Boolean
                                                    && !values.contains_key(id)
                                                {
                                                    values.insert(
                                                        id.clone(),
                                                        Value::String("0".into()),
                                                    );
                                                }
                                            }
                                            let mut result = values;
                                            result.insert(
                                                "_RowType_".into(),
                                                Value::String(row_name(row.row_type).into()),
                                            );
                                            let mut entries =
                                                result.into_iter().collect::<Vec<_>>();
                                            entries.sort_by_key(|(id, _)| {
                                                (
                                                    java_json_bucket(id),
                                                    dataset
                                                        .columns
                                                        .iter()
                                                        .position(|(column, _)| column == id)
                                                        .unwrap_or(usize::MAX),
                                                )
                                            });
                                            Value::Object(
                                                entries.into_iter().collect::<Map<_, _>>(),
                                            )
                                        })
                                        .collect(),
                                ),
                            );
                        }
                        Value::Object(item)
                    })
                    .collect(),
            ),
        );
    }
    if let Some(parameters) = parameters {
        root.insert("Parameters".into(), parameters);
    }
    root.insert("version".into(), Value::String("1.0".into()));
    let encoded = serde_json::to_vec(&Value::Object(root))
        .map_err(|error| Error::from(EncodeError::Message(error.to_string())))?;
    let encoded = String::from_utf8(encoded)
        .map_err(|error| Error::from(EncodeError::Message(error.to_string())))?
        .replace("\\\\/", "\\/");
    output.write_all(encoded.as_bytes()).map_err(Error::from)
}

fn row_name(row: RowType) -> &'static str {
    match row {
        RowType::Normal => "N",
        RowType::Insert | RowType::Update | RowType::Delete | RowType::Original => "N",
    }
}
fn java_json_bucket(value: &str) -> u32 {
    let hash = value.bytes().fold(0u32, |hash, byte| {
        hash.wrapping_mul(31).wrapping_add(byte as u32)
    });
    (hash ^ (hash >> 16)) & 15
}
fn raw_json(raw: &XValue, wire: Option<WireType>) -> serde_json::Value {
    match raw {
        XValue::Missing => serde_json::Value::Null,
        XValue::Null => match wire {
            Some(WireType::Int | WireType::Long | WireType::Boolean) => {
                serde_json::Value::String("0".into())
            }
            Some(WireType::Float | WireType::Double) => serde_json::Value::String("0.0".into()),
            Some(WireType::Decimal | WireType::BigDecimal) => serde_json::Value::String("0".into()),
            _ => serde_json::Value::Null,
        },
        XValue::Text(value) if matches!(wire, Some(WireType::Int | WireType::Long)) => {
            serde_json::Value::String(normalize_integer(value))
        }
        XValue::Text(value) if matches!(wire, Some(WireType::Float | WireType::Double)) => {
            serde_json::Value::String(normalize_float_text(value))
        }
        XValue::Text(value) if matches!(wire, Some(WireType::Decimal | WireType::BigDecimal)) => {
            serde_json::Value::String(normalize_decimal(value))
        }
        XValue::Text(value) if matches!(wire, Some(WireType::Boolean)) => {
            serde_json::Value::String(if source_boolean(value) { "1" } else { "0" }.into())
        }
        XValue::Text(value) if matches!(wire, Some(WireType::Blob)) => {
            serde_json::Value::String(value.replace('/', "\\/"))
        }
        XValue::Text(value) => serde_json::Value::String(value.clone()),
        XValue::Bytes(value) => serde_json::Value::String(
            base64::engine::general_purpose::STANDARD
                .encode(value)
                .replace('/', "\\/"),
        ),
    }
}

fn json_default_size(wire: WireType) -> &'static str {
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

fn json_wire_name(wire: WireType) -> String {
    match wire {
        WireType::Boolean => "int",
        WireType::Long | WireType::Decimal | WireType::BigDecimal => "bigdecimal",
        WireType::Double => "float",
        WireType::Blob => "blob",
        _ => return wire.to_string().to_ascii_lowercase(),
    }
    .into()
}
fn json_number(value: &str) -> serde_json::Value {
    serde_json::from_str(value).unwrap_or(serde_json::Value::Null)
}
fn source_boolean(value: &str) -> bool {
    matches!(
        value,
        "true" | "True" | "TRUE" | "yes" | "Yes" | "YES" | "y" | "Y" | "on" | "On" | "ON" | "1"
    )
}

fn normalize_float_text(value: &str) -> String {
    let value = value.trim().replace(',', "");
    if matches!(
        value.as_str(),
        "NaN" | "Infinity" | "+Infinity" | "-Infinity"
    ) {
        return value;
    }
    let parsed = value.parse::<f64>().unwrap_or(0.0);
    let mut text = parsed.to_string();
    if !text.contains('.') && !text.contains('e') && !text.contains('E') {
        text.push_str(".0");
    }
    text
}
fn normalize_integer(value: &str) -> String {
    let value = value.trim().replace(',', "");
    if let Some(hex) = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
    {
        return i64::from_str_radix(hex, 16).unwrap_or(0).to_string();
    }
    value
        .parse::<f64>()
        .map(|v| (v.trunc() as i64).to_string())
        .unwrap_or_else(|_| "0".into())
}
fn normalize_float_json(value: &str) -> serde_json::Value {
    let value = value.trim().replace(',', "");
    if matches!(
        value.as_str(),
        "NaN" | "Infinity" | "+Infinity" | "-Infinity"
    ) {
        return serde_json::Value::Null;
    }
    let parsed = value.parse::<f64>().unwrap_or(0.0);
    let mut text = parsed.to_string();
    if !text.contains('.') && !text.contains('e') && !text.contains('E') {
        text.push_str(".0");
    }
    json_number(&text)
}
fn normalize_decimal(value: &str) -> String {
    let mut value = value.trim().replace(',', "");
    let (sign, body) = if let Some(rest) = value.strip_prefix('+') {
        ("", rest)
    } else if let Some(rest) = value.strip_prefix('-') {
        ("-", rest)
    } else {
        ("", value.as_str())
    };
    value = body.to_owned();
    let (mantissa, exponent) = value
        .split_once(['e', 'E'])
        .map_or((value.as_str(), ""), |(m, e)| (m, e));
    let (integer, fraction) = mantissa
        .split_once('.')
        .map_or((mantissa, None), |(i, f)| (i, Some(f)));
    let integer = integer.trim_start_matches('0');
    let integer = if integer.is_empty() { "0" } else { integer };
    match fraction {
        Some(f) => format!("{sign}{integer}.{f}{exponent}"),
        None => format!("{sign}{integer}{exponent}"),
    }
}
fn restore_decimal_lexicals(encoded: &mut Vec<u8>, document: &crate::RawDocument) {
    let mut from = 0;
    for (index, (_, raw)) in document.parameters.iter().enumerate() {
        if document
            .parameter_wires
            .get(index)
            .and_then(|wire| wire.as_deref())
            .is_some_and(|wire| wire.eq_ignore_ascii_case("BIGDECIMAL"))
        {
            if let XValue::Text(value) = raw {
                let normalized = normalize_decimal(value);
                let numeric = json_number(&normalized).to_string();
                let needle = format!("\"value\":{numeric}").into_bytes();
                let replacement = format!("\"value\":{normalized}").into_bytes();
                if let Some(position) = encoded[from..]
                    .windows(needle.len())
                    .position(|window| window == needle)
                {
                    let position = from + position;
                    encoded.splice(position..position + needle.len(), replacement);
                    from = position;
                }
            }
        }
    }
}

struct JsonSink<W: Write> {
    writer: W,
    parameters_open: bool,
    datasets_open: bool,
    dataset_open: bool,
    first_parameter: bool,
    first_dataset: bool,
    first_row: bool,
    columns: &'static [crate::FieldSchema],
}

impl<W: Write> JsonSink<W> {
    fn new(writer: W) -> Self {
        Self {
            writer,
            parameters_open: false,
            datasets_open: false,
            dataset_open: false,
            first_parameter: true,
            first_dataset: true,
            first_row: true,
            columns: &[],
        }
    }

    fn bytes(&mut self, bytes: &[u8]) -> Result<()> {
        self.writer
            .write_all(bytes)
            .map_err(|error| Error::from(EncodeError::Message(error.to_string())))
    }

    fn comma(&mut self, first: &mut bool) -> Result<()> {
        if *first {
            *first = false;
            Ok(())
        } else {
            self.bytes(b",")
        }
    }

    fn string(&mut self, value: &str) -> Result<()> {
        serde_json::to_writer(&mut self.writer, value).map_err(io_error)
    }

    fn raw(&mut self, value: &XValue, wire: Option<WireType>) -> Result<()> {
        match value {
            XValue::Missing | XValue::Null => self.bytes(b"null"),
            XValue::Text(value)
                if matches!(
                    wire,
                    Some(
                        WireType::Int
                            | WireType::Long
                            | WireType::Float
                            | WireType::Double
                            | WireType::Decimal
                            | WireType::BigDecimal
                            | WireType::Boolean
                    )
                ) =>
            {
                self.bytes(value.as_bytes())
            }
            XValue::Text(value) => self.string(value),
            XValue::Bytes(value) => {
                self.bytes(b"\"")?;
                write_base64(&mut self.writer, value)?;
                self.bytes(b"\"")
            }
        }
    }

    fn field_wire(&self, id: &str) -> Option<WireType> {
        self.columns
            .iter()
            .find(|field| field.id == id)
            .map(|field| field.wire)
    }

    fn values(&mut self, values: &[(String, XValue)]) -> Result<()> {
        self.bytes(b"{")?;
        self.values_inner(values)?;
        self.bytes(b"}")
    }

    fn values_inner(&mut self, values: &[(String, XValue)]) -> Result<()> {
        let mut first = true;
        for (id, value) in values {
            if matches!(value, XValue::Missing) {
                continue;
            }
            self.comma(&mut first)?;
            self.string(id)?;
            self.bytes(b":")?;
            self.raw(value, self.field_wire(id))?;
        }
        Ok(())
    }
}

fn write_base64<W: Write>(writer: &mut W, value: &[u8]) -> Result<()> {
    const INPUT_CHUNK: usize = 3 * 1024;
    let mut rest = value;
    let mut encoded = [0_u8; 4096];
    while rest.len() > INPUT_CHUNK {
        let (chunk, next) = rest.split_at(INPUT_CHUNK);
        let length = STANDARD
            .encode_slice(chunk, &mut encoded)
            .map_err(|error| Error::from(EncodeError::Message(error.to_string())))?;
        writer
            .write_all(&encoded[..length])
            .map_err(|error| Error::from(EncodeError::Message(error.to_string())))?;
        rest = next;
    }
    let length = STANDARD
        .encode_slice(rest, &mut encoded)
        .map_err(|error| Error::from(EncodeError::Message(error.to_string())))?;
    writer
        .write_all(&encoded[..length])
        .map_err(|error| Error::from(EncodeError::Message(error.to_string())))
}

impl<W: Write> XapiEncodeSink for JsonSink<W> {
    fn begin(&mut self) -> Result<()> {
        self.bytes(b"{\"version\":\"1.0\"")
    }

    fn parameter(&mut self, id: &str, value: XValue, wire: WireType) -> Result<()> {
        if self.datasets_open {
            return Err(Error::from(EncodeError::Message(
                "parameter emitted after dataset".into(),
            )));
        }
        if !self.parameters_open {
            self.bytes(br#","Parameters":["#)?;
            self.parameters_open = true;
        }
        let mut first = self.first_parameter;
        self.comma(&mut first)?;
        self.first_parameter = first;
        self.bytes(b"{\"id\":")?;
        self.string(id)?;
        self.bytes(b",\"value\":")?;
        self.raw(&value, Some(wire))?;
        self.bytes(b"}")
    }

    fn dataset_begin(&mut self, id: &str, columns: &'static [crate::FieldSchema]) -> Result<()> {
        if self.parameters_open {
            self.bytes(b"]")?;
            self.parameters_open = false;
        }
        if !self.datasets_open {
            self.bytes(b",\"Datasets\":[")?;
            self.datasets_open = true;
        }
        let mut first = self.first_dataset;
        self.comma(&mut first)?;
        self.first_dataset = first;
        self.bytes(br#"{"id":"#)?;
        self.string(id)?;
        self.bytes(br#","ColumnInfo":{"#)?;
        let has_const = columns.iter().any(|field| field.const_value.is_some());
        if has_const {
            self.bytes(br#""ConstColumn":["#)?;
            let mut first_const = true;
            for field in columns.iter().filter(|field| field.const_value.is_some()) {
                if !first_const {
                    self.bytes(b",")?;
                }
                first_const = false;
                self.bytes(br#"{"id":"#)?;
                self.string(field.id)?;
                self.bytes(br#","size":"#)?;
                self.bytes(field.size.unwrap_or(0).to_string().as_bytes())?;
                self.bytes(br#","type":"#)?;
                self.string(&field.wire.to_string())?;
                self.bytes(b",\"value\":")?;
                self.raw(
                    &XValue::Text(field.const_value.unwrap_or_default().to_owned()),
                    Some(field.wire),
                )?;
                self.bytes(b"}")?;
            }
            self.bytes(b"],")?;
        }
        self.bytes(br#""Column":["#)?;
        let mut first_column = true;
        for field in columns.iter().filter(|field| field.const_value.is_none()) {
            if !first_column {
                self.bytes(b",")?;
            }
            first_column = false;
            self.bytes(br#"{"id":"#)?;
            self.string(field.id)?;
            self.bytes(br#","size":"#)?;
            if let Some(size) = field.size {
                self.bytes(size.to_string().as_bytes())?;
            } else {
                self.bytes(b"0")?;
            }
            self.bytes(br#","type":"#)?;
            self.string(&field.wire.to_string())?;
            self.bytes(b"}")?;
        }
        self.bytes(br#"]},"Rows":["#)?;
        self.columns = columns;
        self.dataset_open = true;
        self.first_row = true;
        Ok(())
    }

    fn row(&mut self, row: RawRow) -> Result<()> {
        if !self.dataset_open {
            return Err(Error::from(EncodeError::Message(
                "row emitted outside dataset".into(),
            )));
        }
        let mut first = self.first_row;
        self.comma(&mut first)?;
        self.first_row = first;
        self.bytes(b"{")?;
        self.string("_RowType_")?;
        self.bytes(b":")?;
        self.string(match row.row_type {
            RowType::Normal => "N",
            RowType::Insert => "I",
            RowType::Update => "U",
            RowType::Delete => "D",
            RowType::Original => "O",
        })?;
        if row
            .values
            .iter()
            .any(|(_, value)| !matches!(value, XValue::Missing))
        {
            self.bytes(b",")?;
            self.values_inner(&row.values)?;
        }
        self.bytes(b"}")?;
        if let Some(original) = row.original.as_ref() {
            let mut first = self.first_row;
            self.comma(&mut first)?;
            self.first_row = first;
            self.bytes(b"{\"_RowType_\":\"O\"")?;
            if original
                .iter()
                .any(|(_, value)| !matches!(value, XValue::Missing))
            {
                self.bytes(b",")?;
                self.values_inner(original)?;
            }
            self.bytes(b"}")?;
        }
        Ok(())
    }

    fn dataset_end(&mut self) -> Result<()> {
        if !self.dataset_open {
            return Err(Error::from(EncodeError::Message(
                "dataset ended before it began".into(),
            )));
        }
        self.bytes(b"]}")?;
        self.dataset_open = false;
        Ok(())
    }

    fn end(&mut self) -> Result<()> {
        if self.dataset_open {
            return Err(Error::from(EncodeError::Message(
                "dataset was not ended".into(),
            )));
        }
        if self.parameters_open {
            self.bytes(b"]")?;
        }
        if self.datasets_open {
            self.bytes(b"]")?;
        }
        self.bytes(b"}")
    }
}

fn optimize_for<T: XapiRoot>(
    raw: XValue,
    wire: Option<WireType>,
    limits: crate::Limits,
) -> Result<XValue> {
    if T::schema().fields.is_empty() {
        Ok(raw)
    } else {
        optimize_raw(raw, wire, limits)
    }
}

fn json_value(
    value: &XValue,
    wire: Option<WireType>,
) -> std::result::Result<Box<RawValue>, EncodeError> {
    let text = match value {
        XValue::Missing | XValue::Null => "null".to_owned(),
        XValue::Text(value)
            if matches!(
                wire,
                Some(
                    WireType::Int
                        | WireType::Long
                        | WireType::Float
                        | WireType::Double
                        | WireType::Decimal
                        | WireType::BigDecimal
                        | WireType::Boolean
                )
            ) && serde_json::from_str::<serde_json::Value>(value).is_ok() =>
        {
            value.clone()
        }
        XValue::Text(value) => {
            serde_json::to_string(value).map_err(|e| EncodeError::Message(e.to_string()))?
        }
        XValue::Bytes(value) => serde_json::to_string(&Blob::new(value.clone()).to_base64())
            .map_err(|e| EncodeError::Message(e.to_string()))?,
    };
    RawValue::from_string(text).map_err(|e| EncodeError::Message(e.to_string()))
}
fn raw_string(value: &str) -> std::result::Result<Box<RawValue>, EncodeError> {
    RawValue::from_string(
        serde_json::to_string(value).map_err(|e| EncodeError::Message(e.to_string()))?,
    )
    .map_err(|e| EncodeError::Message(e.to_string()))
}
fn raw_object(
    value: BTreeMap<String, Box<RawValue>>,
) -> std::result::Result<Box<RawValue>, EncodeError> {
    RawValue::from_string(
        serde_json::to_string(&value).map_err(|e| EncodeError::Message(e.to_string()))?,
    )
    .map_err(|e| EncodeError::Message(e.to_string()))
}
fn raw_array(value: Vec<Box<RawValue>>) -> std::result::Result<Box<RawValue>, EncodeError> {
    RawValue::from_string(
        serde_json::to_string(&value).map_err(|e| EncodeError::Message(e.to_string()))?,
    )
    .map_err(|e| EncodeError::Message(e.to_string()))
}
fn io_error(error: serde_json::Error) -> Error {
    Error::from(EncodeError::Message(error.to_string()))
}
