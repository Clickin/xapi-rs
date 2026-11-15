use std::{collections::HashSet, io::Read};

use serde::de::{self, DeserializeSeed, Deserializer, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde_json::value::RawValue;
use std::fmt;

use crate::{
    optimize_raw, DecodeError, Error, RawRow, RawValue as XValue, Result, RowType, WireType,
    XapiRoot, XapiRootDecoder,
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
                // The drivers read size as a JSON string and reject a number.
                "size" => {
                    let _: String = map.next_value()?;
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
                // The drivers read size as a JSON string and reject a number.
                "size" => {
                    let _: String = map.next_value()?;
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

fn json_error(error: serde_json::Error) -> Error {
    Error::from(DecodeError::message(format!("invalid JSON: {error}")))
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
