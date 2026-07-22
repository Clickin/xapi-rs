//! A small, streaming-oriented foundation for XAPI Dataset codecs.
//!
//! The protocol codecs are profile-specific; this crate keeps the wire model
//! and scalar conversion rules shared by XML and JSON implementations.

extern crate self as xapi;

mod error;
mod json;
mod scalar;
mod xml;

pub mod dynamic;

pub use error::{DecodeError, EncodeError, Error, Path, Result};
pub use scalar::{
    Blob, DateTime, Decimal, LexicalValue, Limits, Presence, Time, WireType, XapiDate, XapiField,
    XapiScalar,
};

/// Exact lexical decimal type used for both DECIMAL and BIGDECIMAL fields.
pub type BigDecimal = Decimal;
pub type XapiTime = Time;
pub type XapiDateTime = DateTime;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RawValue {
    Missing,
    Null,
    Text(String),
    Bytes(Vec<u8>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawDataset {
    pub id: String,
    pub columns: Vec<(String, WireType)>,
    pub const_columns: Vec<(String, WireType, RawValue)>,
    pub rows: Vec<RawRow>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawRow {
    pub row_type: RowType,
    pub values: Vec<(String, RawValue)>,
    pub original: Option<Vec<(String, RawValue)>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RawDocument {
    pub parameters: Vec<(String, RawValue)>,
    pub datasets: Vec<RawDataset>,
}

pub fn raw_get<'a>(row: &'a [(String, RawValue)], id: &str) -> &'a RawValue {
    row.iter()
        .find(|(column, _)| column == id)
        .map(|(_, value)| value)
        .unwrap_or(&RawValue::Missing)
}

#[doc(hidden)]
pub fn raw_take(row: &mut Vec<(String, RawValue)>, id: &str) -> RawValue {
    row.iter()
        .position(|(column, _)| column == id)
        .map(|index| row.swap_remove(index).1)
        .unwrap_or(RawValue::Missing)
}

pub(crate) fn optimize_raw(
    value: RawValue,
    wire: Option<WireType>,
    limits: Limits,
) -> Result<RawValue> {
    if wire == Some(WireType::Blob) {
        if let RawValue::Text(value) = value {
            return Ok(RawValue::Bytes(scalar::decode_base64_in_place(
                value,
                limits.max_blob_bytes,
            )?));
        }
    }
    Ok(value)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FieldSchema {
    pub id: &'static str,
    pub wire: WireType,
    pub size: Option<usize>,
    pub rust_type: &'static str,
    pub const_value: Option<&'static str>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RootFieldKind {
    Parameter,
    Dataset,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RootFieldSchema {
    pub id: &'static str,
    pub kind: RootFieldKind,
    pub wire: WireType,
    pub rust_type: &'static str,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RowSchema<T> {
    pub fields: &'static [FieldSchema],
    pub marker: std::marker::PhantomData<fn() -> T>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RootSchema<T> {
    pub fields: &'static [RootFieldSchema],
    pub marker: std::marker::PhantomData<fn() -> T>,
}

#[cfg(feature = "derive")]
pub use xapi_derive::{XapiRoot, XapiRow};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    XPlatformXml4000,
    NexacroXml4000,
    NexacroJson10,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContentType {
    pub media_type: String,
    pub charset: Option<String>,
}

impl ContentType {
    pub fn parse(value: &str) -> Result<Self> {
        let mut parts = value.split(';');
        let media_type = parts.next().unwrap_or_default().trim().to_ascii_lowercase();
        if !matches!(
            media_type.as_str(),
            "application/xml" | "text/xml" | "application/json"
        ) {
            return Err(Error::from(DecodeError::message(
                "unsupported Content-Type",
            )));
        }
        let mut charset = None;
        for parameter in parts {
            let mut pair = parameter.trim().splitn(2, '=');
            let key = pair.next().unwrap_or_default().trim();
            let value = pair
                .next()
                .map(str::trim)
                .map(|value| value.trim_matches('"').to_owned());
            if key.eq_ignore_ascii_case("charset") {
                charset = value;
            }
        }
        Ok(Self {
            media_type,
            charset,
        })
    }

    pub fn format(&self) -> Format {
        if self.media_type == "application/json" {
            Format::NexacroJson10
        } else {
            Format::NexacroXml4000
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowType {
    Normal,
    Insert,
    Update,
    Delete,
    /// Nexacro JSON's standalone `_RowType_: "O"` original-row marker.
    /// It is consumed by the dataset decoder and is never exposed as a DTO row.
    Original,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tracked<T> {
    pub row_type: RowType,
    pub current: T,
    pub original: Option<Box<T>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Codec {
    format: Format,
    strict: bool,
    limits: Limits,
}

impl Codec {
    pub fn builder() -> CodecBuilder {
        CodecBuilder::default()
    }

    pub fn format(&self) -> Format {
        self.format
    }
    pub fn strict(&self) -> bool {
        self.strict
    }
    pub fn limits(&self) -> Limits {
        self.limits
    }

    pub fn decode<T: XapiRoot, R: std::io::Read>(&self, reader: R) -> Result<T> {
        from_reader_with_options(reader, self.format, self.limits, self.strict)
    }

    pub fn encode<T: XapiRoot, W: std::io::Write>(&self, writer: W, value: &T) -> Result<()> {
        to_writer(writer, value, self.format)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CodecBuilder {
    format: Option<Format>,
    strict: bool,
    limits: Limits,
}

impl CodecBuilder {
    pub fn format(mut self, format: Format) -> Self {
        self.format = Some(format);
        self
    }
    pub fn strict(mut self, strict: bool) -> Self {
        self.strict = strict;
        self
    }
    pub fn limits(mut self, limits: Limits) -> Self {
        self.limits = limits;
        self
    }

    pub fn build(self) -> Result<Codec> {
        Ok(Codec {
            format: self
                .format
                .ok_or_else(|| DecodeError::message("format is required"))?,
            strict: self.strict,
            limits: self.limits,
        })
    }
}

/// Root and row traits are intentionally small; codecs consume generated
/// metadata without requiring reflection or a dynamic value tree.
pub trait XapiRoot: Sized + Send + Sync + 'static {
    type Decoder: XapiRootDecoder<Self>;
    fn schema() -> &'static RootSchema<Self>;
    fn dataset_wire(dataset: &str, column: &str) -> Option<WireType> {
        let _ = (dataset, column);
        None
    }
    fn dataset_has_blob(dataset: &str) -> bool {
        let _ = dataset;
        false
    }
    fn decoder(strict: bool, limits: Limits) -> Self::Decoder;
    fn decode_raw(document: RawDocument) -> Result<Self>;
    fn encode_raw(&self) -> Result<RawDocument>;
    fn encode_stream(&self, sink: &mut dyn XapiEncodeSink) -> Result<()>;
}

pub trait XapiEncodeSink {
    fn begin(&mut self) -> Result<()>;
    fn parameter(&mut self, id: &str, value: RawValue, wire: WireType) -> Result<()>;
    fn dataset_begin(&mut self, id: &str, columns: &'static [FieldSchema]) -> Result<()>;
    fn row(&mut self, row: RawRow) -> Result<()>;
    fn dataset_end(&mut self) -> Result<()>;
    fn end(&mut self) -> Result<()>;
}

pub trait XapiRootDecoder<T>: Sized {
    fn unknown_root_member(&mut self, id: &str) -> Result<()> {
        let _ = id;
        Ok(())
    }
    fn parameter(&mut self, id: &str, value: &RawValue) -> Result<()>;
    fn dataset_begin(&mut self, dataset: &str) -> Result<()> {
        let _ = dataset;
        Ok(())
    }
    fn dataset_columns(&mut self, dataset: &str, columns: &[(String, WireType)]) -> Result<()> {
        let _ = (dataset, columns);
        Ok(())
    }
    fn dataset_const_column(
        &mut self,
        dataset: &str,
        id: &str,
        wire: WireType,
        value: &RawValue,
    ) -> Result<()> {
        let _ = (dataset, id, wire, value);
        Ok(())
    }
    fn dataset_row(&mut self, dataset: &str, row: RawRow) -> Result<()>;
    fn finish(self) -> Result<T>;
}

pub trait XapiRow: Sized + Send + Sync {
    fn schema() -> &'static RowSchema<Self>;
    fn decode_raw(row: &[(String, RawValue)]) -> Result<Self>;
    fn decode_raw_with_limits(row: &[(String, RawValue)], limits: Limits) -> Result<Self> {
        let _ = limits;
        Self::decode_raw(row)
    }
    fn decode_raw_with_options(
        row: &[(String, RawValue)],
        strict: bool,
        limits: Limits,
    ) -> Result<Self> {
        let _ = strict;
        Self::decode_raw_with_limits(row, limits)
    }
    fn decode_raw_owned(
        row: Vec<(String, RawValue)>,
        strict: bool,
        limits: Limits,
    ) -> Result<Self> {
        Self::decode_raw_with_options(&row, strict, limits)
    }
    fn encode_raw(&self) -> Result<Vec<(String, RawValue)>>;
    fn validate_const_column(id: &str, value: &RawValue, limits: Limits) -> Result<()> {
        let _ = (id, value, limits);
        Ok(())
    }
}

pub fn from_reader<T, R>(reader: R, format: Format) -> Result<T>
where
    T: XapiRoot,
    R: std::io::Read,
{
    from_reader_with_options(reader, format, Limits::default(), false)
}

pub fn from_reader_with_limits<T, R>(reader: R, format: Format, limits: Limits) -> Result<T>
where
    T: XapiRoot,
    R: std::io::Read,
{
    from_reader_with_options(reader, format, limits, false)
}

pub fn from_reader_with_options<T, R>(
    reader: R,
    format: Format,
    limits: Limits,
    strict: bool,
) -> Result<T>
where
    T: XapiRoot,
    R: std::io::Read,
{
    match format {
        Format::XPlatformXml4000 | Format::NexacroXml4000 => {
            xml::decode_stream(reader, limits, strict)
        }
        Format::NexacroJson10 => json::decode_stream(reader, limits, strict),
    }
}

#[allow(dead_code)]
fn validate_limits(document: &RawDocument, limits: Limits) -> Result<()> {
    if document.datasets.len() > limits.max_datasets {
        return Err(Error::from(DecodeError::Limit("max_datasets")));
    }
    for dataset in &document.datasets {
        if dataset.rows.len() > limits.max_rows {
            return Err(Error::from(DecodeError::Limit("max_rows")));
        }
        if dataset.columns.len() > limits.max_columns {
            return Err(Error::from(DecodeError::Limit("max_columns")));
        }
        for row in &dataset.rows {
            for (_, value) in row.values.iter().chain(row.original.iter().flatten()) {
                if let RawValue::Text(value) = value {
                    if value.len() > limits.max_scalar_bytes {
                        return Err(Error::from(DecodeError::Limit("max_scalar_bytes")));
                    }
                }
            }
        }
    }
    for (_, value) in &document.parameters {
        if let RawValue::Text(value) = value {
            if value.len() > limits.max_scalar_bytes {
                return Err(Error::from(DecodeError::Limit("max_scalar_bytes")));
            }
        }
    }
    Ok(())
}

pub fn from_slice<T: XapiRoot>(bytes: &[u8], format: Format) -> Result<T> {
    from_reader(bytes, format)
}

pub fn to_writer<T, W>(writer: W, value: &T, format: Format) -> Result<()>
where
    T: XapiRoot,
    W: std::io::Write,
{
    match format {
        Format::XPlatformXml4000 | Format::NexacroXml4000 => {
            xml::encode_stream(writer, value, format)
        }
        Format::NexacroJson10 => json::encode_stream(writer, value),
    }
}

pub fn to_vec<T: XapiRoot>(value: &T, format: Format) -> Result<Vec<u8>> {
    let mut output = Vec::new();
    to_writer(&mut output, value, format)?;
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[allow(dead_code)]
    #[derive(XapiRow)]
    struct UserRow {
        #[xapi(col = "id", wire = "INT", size = 10)]
        id: i32,
        #[xapi(col = "name", wire = "STRING", size = 100)]
        name: String,
    }

    #[allow(dead_code)]
    #[derive(XapiRow)]
    struct ConstRow {
        #[xapi(const_col = "region", wire = "STRING", value = "KR")]
        region: String,
    }

    #[allow(dead_code)]
    #[derive(XapiRoot)]
    struct ConstResponse {
        #[xapi(dataset = "users")]
        users: Vec<ConstRow>,
    }

    #[allow(dead_code)]
    #[derive(XapiRow)]
    struct BlobRow {
        #[xapi(col = "payload", wire = "BLOB")]
        payload: Blob,
    }

    #[allow(dead_code)]
    #[derive(XapiRoot)]
    struct BlobResponse {
        #[xapi(dataset = "blobs")]
        blobs: Vec<BlobRow>,
    }

    #[allow(dead_code)]
    #[derive(XapiRoot)]
    struct TrackedBlobResponse {
        #[xapi(dataset = "blobs")]
        blobs: Vec<Tracked<BlobRow>>,
    }

    #[allow(dead_code)]
    #[derive(XapiRoot)]
    struct BlobParameterResponse {
        #[xapi(param = "Payload", wire = "BLOB")]
        payload: Vec<u8>,
    }

    #[allow(dead_code)]
    #[derive(XapiRow)]
    struct ConstBlobRow {
        #[xapi(const_col = "payload", wire = "BLOB", value = "AAEC")]
        payload: Blob,
    }

    #[allow(dead_code)]
    #[derive(XapiRow)]
    struct OptionalBytesRow {
        #[xapi(col = "payload", wire = "BLOB")]
        payload: Option<Vec<u8>>,
    }

    #[allow(dead_code)]
    #[derive(XapiRoot)]
    struct OptionalBytesResponse {
        #[xapi(dataset = "blobs")]
        blobs: Vec<OptionalBytesRow>,
    }

    #[allow(dead_code)]
    #[derive(Debug, PartialEq, XapiRow)]
    struct PresenceRow {
        #[xapi(col = "value", wire = "STRING")]
        value: Presence<String>,
    }

    #[allow(dead_code)]
    #[derive(XapiRoot)]
    struct PresenceResponse {
        #[xapi(dataset = "values")]
        values: Vec<PresenceRow>,
    }

    #[allow(dead_code)]
    #[derive(XapiRow)]
    struct AmountRow {
        #[xapi(col = "amount", wire = "BIGDECIMAL")]
        amount: BigDecimal,
    }

    #[allow(dead_code)]
    #[derive(XapiRoot)]
    struct AmountResponse {
        #[xapi(dataset = "amounts")]
        amounts: Vec<AmountRow>,
    }

    #[allow(dead_code)]
    #[derive(XapiRoot)]
    struct Response {
        #[xapi(param = "ErrorCode", wire = "INT")]
        code: i32,
        #[xapi(dataset = "users")]
        users: Vec<UserRow>,
    }

    #[allow(dead_code)]
    #[derive(XapiRoot)]
    struct TrackedResponse {
        #[xapi(dataset = "users")]
        users: Vec<Tracked<UserRow>>,
    }

    #[test]
    fn codec_is_send_sync_and_requires_format() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Codec>();
        assert!(Codec::builder().build().is_err());
    }

    #[test]
    fn derive_emits_static_schema() {
        assert_eq!(UserRow::schema().fields[0].id, "id");
        assert_eq!(UserRow::schema().fields[0].wire, WireType::Int);
        assert_eq!(Response::schema().fields[1].id, "users");
    }

    #[test]
    fn xml_round_trip_is_column_order_independent() {
        let input = include_bytes!("../../../testdata/nexacro-xml/basic.xml");
        let response: Response = from_slice(input, Format::NexacroXml4000).unwrap();
        assert_eq!(response.code, 7);
        assert_eq!(response.users[0].id, 42);
        assert_eq!(response.users[0].name, "Ada");
        let encoded = to_vec(&response, Format::NexacroXml4000).unwrap();
        let again: Response = from_slice(&encoded, Format::XPlatformXml4000).unwrap();
        assert_eq!(again.users[0].name, "Ada");
    }

    #[test]
    fn reader_api_decodes_from_short_reads() {
        struct OneByte<'a> {
            input: &'a [u8],
            position: usize,
        }
        impl std::io::Read for OneByte<'_> {
            fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
                if self.position == self.input.len() {
                    return Ok(0);
                }
                output[0] = self.input[self.position];
                self.position += 1;
                Ok(1)
            }
        }

        for (input, format) in [
            (
                include_bytes!("../../../testdata/nexacro-xml/basic.xml").as_slice(),
                Format::NexacroXml4000,
            ),
            (
                include_bytes!("../../../testdata/nexacro-json/basic.json").as_slice(),
                Format::NexacroJson10,
            ),
        ] {
            let response: Response = from_reader(OneByte { input, position: 0 }, format).unwrap();
            assert_eq!(response.users[0].name, "Ada");
        }
    }

    #[test]
    fn public_profile_and_invalid_fixtures_are_covered() {
        let xplatform = include_bytes!("../../../testdata/xplatform-xml/basic.xml");
        let response: Response = from_slice(xplatform, Format::XPlatformXml4000).unwrap();
        assert_eq!(response.users[0].id, 42);

        let invalid_xml = include_bytes!("../../../testdata/invalid/malformed.xml");
        assert!(from_slice::<Response>(invalid_xml, Format::NexacroXml4000).is_err());
        let invalid_json = include_bytes!("../../../testdata/invalid/malformed.json");
        assert!(from_slice::<Response>(invalid_json, Format::NexacroJson10).is_err());
    }

    #[test]
    fn canonical_vendor_fixtures_decode() {
        let xml = include_bytes!("../../../testdata/xplatform-xml/direct-dataset.xml");
        let response: Response = from_slice(xml, Format::XPlatformXml4000).unwrap();
        assert_eq!(response.users[0].name, "Ada");

        let json = include_bytes!("../../../testdata/nexacro-json/canonical.json");
        let response: TrackedResponse = from_slice(json, Format::NexacroJson10).unwrap();
        assert_eq!(response.users[0].row_type, RowType::Update);
        assert_eq!(response.users[0].current.id, 42);
        assert_eq!(response.users[0].original.as_ref().unwrap().id, 41);
    }

    #[test]
    fn decimal_lexical_scale_survives_xml_and_json_round_trip() {
        let xml = br#"<Root><Datasets><Dataset id="amounts"><Rows><Row><Col id="amount">65.7700</Col></Row></Rows></Dataset></Datasets></Root>"#;
        let json = br#"{"Datasets":[{"id":"amounts","Rows":[{"amount":65.7700}]}]}"#;
        for (input, format) in [
            (xml.as_slice(), Format::NexacroXml4000),
            (json.as_slice(), Format::NexacroJson10),
        ] {
            let response: AmountResponse = from_slice(input, format).unwrap();
            assert_eq!(response.amounts[0].amount.as_str(), "65.7700");
            let encoded = to_vec(&response, format).unwrap();
            let round_trip: AmountResponse = from_slice(&encoded, format).unwrap();
            assert_eq!(round_trip.amounts[0].amount.as_str(), "65.7700");
        }
    }

    #[test]
    fn json_round_trip_preserves_numeric_lexical_input_until_scalar_decode() {
        let input = include_bytes!("../../../testdata/nexacro-json/basic.json");
        let response: Response = from_slice(input, Format::NexacroJson10).unwrap();
        assert_eq!(response.code, 7);
        assert_eq!(response.users[0].id, 42);
        let encoded = to_vec(&response, Format::NexacroJson10).unwrap();
        let again: Response = from_slice(&encoded, Format::NexacroJson10).unwrap();
        assert_eq!(again.users[0].name, "Ada");
    }

    #[test]
    fn tracked_rows_decode_row_type_and_org_row() {
        let input = br#"<Root><Datasets><Dataset id="users"><ColumnInfo/><Rows><Row type="U"><Col id="id">42</Col><Col id="name">Ada</Col><OrgRow><Col id="id">41</Col><Col id="name">Old</Col></OrgRow></Row></Rows></Dataset></Datasets></Root>"#;
        let response: TrackedResponse = from_slice(input, Format::NexacroXml4000).unwrap();
        assert_eq!(response.users[0].row_type, RowType::Update);
        assert_eq!(response.users[0].current.id, 42);
        assert_eq!(response.users[0].original.as_ref().unwrap().name, "Old");
        let encoded = to_vec(&response, Format::NexacroXml4000).unwrap();
        assert!(String::from_utf8(encoded).unwrap().contains("OrgRow"));
    }

    #[test]
    fn self_closing_org_row_does_not_capture_following_columns() {
        let input = br#"<Root><Datasets><Dataset id="users"><Rows><Row type="U"><Col id="id">42</Col><OrgRow/><Col id="name">Ada</Col></Row></Rows></Dataset></Datasets></Root>"#;
        let response: TrackedResponse = from_slice(input, Format::NexacroXml4000).unwrap();
        assert_eq!(response.users[0].current.name, "Ada");
        assert!(response.users[0].original.is_none());
    }

    #[test]
    fn content_type_parameters_are_typed_and_limits_are_enforced() {
        let content_type = ContentType::parse("Application/JSON; charset=\"UTF-8\"").unwrap();
        assert_eq!(content_type.format(), Format::NexacroJson10);
        assert_eq!(content_type.charset.as_deref(), Some("UTF-8"));
        let input = include_bytes!("../../../testdata/nexacro-xml/basic.xml");
        let limits = Limits {
            max_rows: 0,
            ..Limits::default()
        };
        assert!(from_reader_with_limits::<Response, _>(
            input.as_slice(),
            Format::NexacroXml4000,
            limits
        )
        .is_err());

        let no_datasets = Limits {
            max_datasets: 0,
            ..Limits::default()
        };
        assert!(from_reader_with_limits::<Response, _>(
            input.as_slice(),
            Format::NexacroXml4000,
            no_datasets
        )
        .is_err());

        let no_columns = Limits {
            max_columns: 0,
            ..Limits::default()
        };
        assert!(from_reader_with_limits::<Response, _>(
            input.as_slice(),
            Format::NexacroXml4000,
            no_columns
        )
        .is_err());

        let parameter_attribute = br#"<Root><Parameter id="ErrorCode" value="123"/></Root>"#;
        let scalar_limit = Limits {
            max_scalar_bytes: 1,
            ..Limits::default()
        };
        assert!(from_reader_with_options::<Response, _>(
            parameter_attribute.as_slice(),
            Format::NexacroXml4000,
            scalar_limit,
            false
        )
        .is_err());

        let const_attribute = br#"<Root><Dataset id="users"><ColumnInfo><ConstColumn id="region" type="STRING" value="KR"/></ColumnInfo></Dataset></Root>"#;
        assert!(from_reader_with_options::<ConstResponse, _>(
            const_attribute.as_slice(),
            Format::NexacroXml4000,
            scalar_limit,
            false
        )
        .is_err());
    }

    #[test]
    fn json_u_o_rows_map_to_tracked_model() {
        let input = br#"{"Datasets":[{"id":"users","Rows":[{"U":{"id":42,"name":"Ada"},"O":{"id":41,"name":"Old"}}]}]}"#;
        let response: TrackedResponse = from_slice(input, Format::NexacroJson10).unwrap();
        assert_eq!(response.users[0].row_type, RowType::Update);
        assert_eq!(response.users[0].original.as_ref().unwrap().id, 41);
        let encoded = to_vec(&response, Format::NexacroJson10).unwrap();
        assert!(String::from_utf8(encoded).unwrap().contains("\"O\""));
    }

    #[test]
    fn json_reordered_dataset_keys_still_decode() {
        let input = br#"{"Parameters":{"ErrorCode":0},"Datasets":[{"Rows":[{"id":42,"name":"Ada"}],"id":"users"}]}"#;
        let response: Response = from_slice(input, Format::NexacroJson10).unwrap();
        assert_eq!(response.users[0].id, 42);
        assert_eq!(response.users[0].name, "Ada");
    }

    #[test]
    fn scalar_errors_include_dto_path() {
        let input = br#"<Root><Parameters><Parameter id="ErrorCode">not-an-int</Parameter></Parameters></Root>"#;
        let error = match from_slice::<Response>(input, Format::NexacroXml4000) {
            Ok(_) => panic!("invalid scalar unexpectedly decoded"),
            Err(error) => error.to_string(),
        };
        assert!(error.contains("Response.ErrorCode"), "{error}");
    }

    #[test]
    fn strict_codec_rejects_unknown_root_members() {
        let input =
            br#"<Root><Parameters><Parameter id="Unexpected">1</Parameter></Parameters></Root>"#;
        let codec = Codec::builder()
            .format(Format::NexacroXml4000)
            .strict(true)
            .build()
            .unwrap();
        assert!(codec.decode::<Response, _>(input.as_slice()).is_err());

        let xml = br#"<Root><Unexpected><Value>1</Value></Unexpected><Parameters><Parameter id="ErrorCode">0</Parameter></Parameters></Root>"#;
        assert!(codec.decode::<Response, _>(xml.as_slice()).is_err());

        let json = br#"{"Unexpected":1,"Parameters":{"ErrorCode":0}}"#;
        let codec = Codec::builder()
            .format(Format::NexacroJson10)
            .strict(true)
            .build()
            .unwrap();
        assert!(codec.decode::<Response, _>(json.as_slice()).is_err());
    }

    #[test]
    fn strict_codec_rejects_unknown_columns() {
        let input = br#"<Root><Parameters><Parameter id="ErrorCode">0</Parameter></Parameters><Datasets><Dataset id="users"><Rows><Row><Col id="id">1</Col><Col id="name">Ada</Col><Col id="extra">ignored</Col></Row></Rows></Dataset></Datasets></Root>"#;
        let codec = Codec::builder()
            .format(Format::NexacroXml4000)
            .strict(true)
            .build()
            .unwrap();
        assert!(codec.decode::<Response, _>(input.as_slice()).is_err());
    }

    #[test]
    fn strict_codec_rejects_unknown_columns_in_empty_datasets() {
        let codec = Codec::builder()
            .format(Format::NexacroXml4000)
            .strict(true)
            .build()
            .unwrap();
        let xml = br#"<Root><Datasets><Dataset id="users"><ColumnInfo><Column id="extra" type="STRING"/></ColumnInfo><Rows/></Dataset></Datasets></Root>"#;
        assert!(codec.decode::<Response, _>(xml.as_slice()).is_err());

        let json = br#"{"Datasets":[{"id":"users","ColumnInfo":[{"id":"extra","type":"STRING"}],"Rows":[] }]}"#;
        let codec = Codec::builder()
            .format(Format::NexacroJson10)
            .strict(true)
            .build()
            .unwrap();
        assert!(codec.decode::<Response, _>(json.as_slice()).is_err());

        let too_many_columns = br#"{"Datasets":[{"id":"users","ColumnInfo":{"Column":[{"id":"id"}],"ConstColumn":[{"id":"region","value":"KR"}]},"Rows":[]}]}"#;
        let limits = Limits {
            max_columns: 1,
            ..Limits::default()
        };
        assert!(from_reader_with_options::<ConstResponse, _>(
            too_many_columns.as_slice(),
            Format::NexacroJson10,
            limits,
            false
        )
        .is_err());

        let unknown_const = br#"<Root><Dataset id="users"><ColumnInfo><ConstColumn id="extra" type="STRING" value="ignored"/></ColumnInfo><Rows/></Dataset></Root>"#;
        assert!(from_slice::<ConstResponse>(unknown_const, Format::NexacroXml4000).is_ok());
    }

    #[test]
    fn strict_codec_rejects_missing_dataset_but_accepts_empty_dataset() {
        let codec = Codec::builder()
            .format(Format::NexacroXml4000)
            .strict(true)
            .build()
            .unwrap();
        let missing =
            br#"<Root><Parameters><Parameter id="ErrorCode">0</Parameter></Parameters></Root>"#;
        assert!(codec.decode::<Response, _>(missing.as_slice()).is_err());

        let empty = br#"<Root><Parameters><Parameter id="ErrorCode">0</Parameter></Parameters><Datasets><Dataset id="users"/></Datasets></Root>"#;
        assert!(codec.decode::<Response, _>(empty.as_slice()).is_ok());
    }

    #[test]
    fn json_depth_limit_applies_to_unknown_nested_values() {
        let input = br#"{"unknown":{"nested":{"value":1}},"Parameters":{"ErrorCode":0}}"#;
        let limits = Limits {
            max_depth: 2,
            ..Limits::default()
        };
        assert!(from_reader_with_options::<Response, _>(
            input.as_slice(),
            Format::NexacroJson10,
            limits,
            false
        )
        .is_err());
    }

    #[test]
    fn json_duplicate_object_keys_are_rejected() {
        let duplicate_root = br#"{"Parameters":{"ErrorCode":0},"Parameters":{"ErrorCode":1}}"#;
        assert!(from_slice::<Response>(duplicate_root, Format::NexacroJson10).is_err());
        let duplicate_row = br#"{"Parameters":{"ErrorCode":0},"Datasets":[{"id":"users","Rows":[{"id":1,"id":2,"name":"Ada"}]}]}"#;
        assert!(from_slice::<Response>(duplicate_row, Format::NexacroJson10).is_err());
        let duplicate_column =
            br#"{"Datasets":[{"id":"users","ColumnInfo":[{"id":"id"},{"id":"id"}],"Rows":[] }]}"#;
        assert!(from_slice::<Response>(duplicate_column, Format::NexacroJson10).is_err());
        let duplicate_xml_column = br#"<Root><Datasets><Dataset id="users"><ColumnInfo><Column id="id"/><Column id="id"/></ColumnInfo><Rows/></Dataset></Datasets></Root>"#;
        assert!(from_slice::<Response>(duplicate_xml_column, Format::NexacroXml4000).is_err());
    }

    #[test]
    fn row_errors_include_dataset_and_row_index() {
        let input = br#"<Root><Parameters><Parameter id="ErrorCode">0</Parameter></Parameters><Datasets><Dataset id="users"><Rows><Row><Col id="id">not-an-int</Col><Col id="name">Ada</Col></Row></Rows></Dataset></Datasets></Root>"#;
        let error = match from_slice::<Response>(input, Format::NexacroXml4000) {
            Ok(_) => panic!("invalid row unexpectedly decoded"),
            Err(error) => error.to_string(),
        };
        assert!(error.contains("Response.users[0].UserRow.id"), "{error}");
    }

    #[test]
    fn malformed_xml_and_json_inputs_do_not_panic() {
        let mut state = 0x9e3779b9_u32;
        for length in 0..256 {
            let mut bytes = Vec::with_capacity(length);
            for _ in 0..length {
                state = state.wrapping_mul(1664525).wrapping_add(1013904223);
                bytes.push((state >> 24) as u8);
            }
            for format in [Format::NexacroXml4000, Format::NexacroJson10] {
                let result = std::panic::catch_unwind(|| {
                    let _ = from_slice::<Response>(&bytes, format);
                });
                assert!(result.is_ok(), "parser panicked for {format:?}");
            }
        }
    }

    #[test]
    fn empty_dataset_writes_static_column_info_without_a_row() {
        let value = Response {
            code: 0,
            users: Vec::new(),
        };
        let output = String::from_utf8(to_vec(&value, Format::NexacroXml4000).unwrap()).unwrap();
        assert!(output.contains("ColumnInfo"));
        assert!(output.contains("id"));
        assert!(output.contains("name"));
    }

    #[test]
    fn xml_control_entities_are_single_escaped_and_round_trip() {
        let value = Response {
            code: 0,
            users: vec![UserRow {
                id: 1,
                name: "before\u{1}&after".into(),
            }],
        };
        let encoded = String::from_utf8(to_vec(&value, Format::NexacroXml4000).unwrap()).unwrap();
        assert!(encoded.contains("before&#1;&amp;after"), "{encoded}");
        assert!(!encoded.contains("&amp;#1;"), "{encoded}");
        let decoded: Response = from_slice(encoded.as_bytes(), Format::NexacroXml4000).unwrap();
        assert_eq!(decoded.users[0].name, "before\u{1}&after");
    }

    #[test]
    fn xml_literal_entity_text_is_not_decoded_twice() {
        let input = br#"<Root><Parameters><Parameter id="ErrorCode">0</Parameter></Parameters><Datasets><Dataset id="users"><Rows><Row><Col id="id">1</Col><Col id="name">&amp;#1;</Col></Row></Rows></Dataset></Datasets></Root>"#;
        let response: Response = from_slice(input, Format::NexacroXml4000).unwrap();
        assert_eq!(response.users[0].name, "&#1;");
    }

    #[test]
    fn xml_rejects_nul_values() {
        let value = Response {
            code: 0,
            users: vec![UserRow {
                id: 1,
                name: "bad\0value".into(),
            }],
        };
        assert!(to_vec(&value, Format::NexacroXml4000).is_err());
    }

    #[test]
    fn xml_cdata_is_literal_and_malformed_entities_fail() {
        let cdata = br#"<Root><Parameters><Parameter id="ErrorCode">0</Parameter></Parameters><Datasets><Dataset id="users"><Rows><Row><Col id="id">1</Col><Col id="name"><![CDATA[<tag>&amp;]]></Col></Row></Rows></Dataset></Datasets></Root>"#;
        let response: Response = from_slice(cdata, Format::NexacroXml4000).unwrap();
        assert_eq!(response.users[0].name, "<tag>&amp;");

        let malformed = br#"<Root><Parameters><Parameter id="ErrorCode">0</Parameter></Parameters><Datasets><Dataset id="users"><Rows><Row><Col id="id">1</Col><Col id="name">&unknown;</Col></Row></Rows></Dataset></Datasets></Root>"#;
        assert!(from_slice::<Response>(malformed, Format::NexacroXml4000).is_err());

        let doctype = br#"<!DOCTYPE Root [<!ENTITY x "blocked">]><Root><Parameters><Parameter id="ErrorCode">0</Parameter></Parameters></Root>"#;
        assert!(from_slice::<Response>(doctype, Format::NexacroXml4000).is_err());
    }

    #[test]
    fn xml_invalid_cdata_utf8_is_rejected() {
        let input = b"<Root><Datasets><Dataset id=\"users\"><Rows><Row><Col id=\"id\">1</Col><Col id=\"name\"><![CDATA[\xFF]]></Col></Row></Rows></Dataset></Datasets></Root>";
        assert!(from_slice::<Response>(input, Format::NexacroXml4000).is_err());
    }

    #[test]
    fn const_column_is_validated_by_the_derived_row_decoder() {
        let row = ConstRow::decode_raw(&[("region".into(), RawValue::Text("KR".into()))]).unwrap();
        assert_eq!(row.region, "KR");
        assert!(ConstRow::decode_raw(&[("region".into(), RawValue::Text("US".into()),)]).is_err());
    }

    #[test]
    fn const_columns_use_metadata_and_are_not_repeated_in_rows() {
        let xml = br#"<Root><Dataset id="users"><ColumnInfo><ConstColumn id="region" size="2" type="STRING" value="KR"/></ColumnInfo><Rows><Row/></Rows></Dataset></Root>"#;
        let response: ConstResponse = from_slice(xml, Format::NexacroXml4000).unwrap();
        assert_eq!(response.users[0].region, "KR");
        let encoded =
            String::from_utf8(to_vec(&response, Format::NexacroXml4000).unwrap()).unwrap();
        assert!(encoded.contains("<ConstColumn"));
        assert!(
            !encoded.contains("<Col id=\"region\">") && !encoded.contains("<Col id=\"region\"/>")
        );

        let json = br#"{"version":"1.0","Datasets":[{"id":"users","ColumnInfo":{"ConstColumn":[{"id":"region","type":"STRING","size":2,"value":"KR"}],"Column":[]},"Rows":[{"_RowType_":"N"}]}]}"#;
        let response: ConstResponse = from_slice(json, Format::NexacroJson10).unwrap();
        assert_eq!(response.users[0].region, "KR");
        let encoded = to_vec(&response, Format::NexacroJson10).unwrap();
        let round_trip: ConstResponse = from_slice(&encoded, Format::NexacroJson10).unwrap();
        assert_eq!(round_trip.users[0].region, "KR");
    }

    #[test]
    fn blob_limits_reach_generated_xml_and_json_decoders() {
        let limits = Limits {
            max_blob_bytes: 2,
            ..Limits::default()
        };
        let xml = br#"<Root><Datasets><Dataset id="blobs"><Rows><Row><Col id="payload">AAEC</Col></Row></Rows></Dataset></Datasets></Root>"#;
        assert!(from_reader_with_options::<BlobResponse, _>(
            xml.as_slice(),
            Format::NexacroXml4000,
            limits,
            false
        )
        .is_err());
        let json = br#"{"Datasets":[{"id":"blobs","Rows":[{"payload":"AAEC"}]}]}"#;
        assert!(from_reader_with_options::<BlobResponse, _>(
            json.as_slice(),
            Format::NexacroJson10,
            limits,
            false
        )
        .is_err());
    }

    #[test]
    fn blob_round_trip_uses_the_same_wire_path_in_xml_and_json() {
        let value = BlobResponse {
            blobs: vec![BlobRow {
                payload: Blob::new((0..(3 * 1024 + 2)).map(|value| value as u8).collect()),
            }],
        };
        let expected: Vec<u8> = (0..(3 * 1024 + 2)).map(|value| value as u8).collect();
        for format in [Format::NexacroXml4000, Format::NexacroJson10] {
            let encoded = to_vec(&value, format).unwrap();
            let decoded: BlobResponse = from_slice(&encoded, format).unwrap();
            assert_eq!(decoded.blobs[0].payload.as_bytes(), expected);
        }
    }

    #[test]
    fn blob_parameters_and_const_columns_use_the_shared_base64_path() {
        let xml =
            br#"<Root><Parameters><Parameter id="Payload">AAEC</Parameter></Parameters></Root>"#;
        let json = br#"{"Parameters":{"Payload":"AAEC"}}"#;
        for (input, format) in [
            (xml.as_slice(), Format::NexacroXml4000),
            (json.as_slice(), Format::NexacroJson10),
        ] {
            let response: BlobParameterResponse = from_slice(input, format).unwrap();
            assert_eq!(response.payload, [0, 1, 2]);
        }

        let row =
            ConstBlobRow::decode_raw(&[("payload".into(), RawValue::Text("AAEC".into()))]).unwrap();
        assert_eq!(row.payload.as_bytes(), &[0, 1, 2]);
        assert!(
            ConstBlobRow::decode_raw(&[("payload".into(), RawValue::Text("AQID".into()),)])
                .is_err()
        );
    }

    #[test]
    fn blob_org_rows_and_boundary_lengths_use_the_shared_path() {
        for length in 0..=3 {
            let value = Blob::new((0..length).map(|byte| byte as u8).collect());
            let encoded = value.to_base64();
            assert_eq!(Blob::from_base64(&encoded, 3).unwrap(), value);
        }
        assert!(Blob::from_base64("A A==", 3).is_err());
        assert!(Blob::from_base64("TQ=", 3).is_err());
        assert!(Blob::from_base64("T$==", 3).is_err());
        assert!(Blob::from_base64("AAEC", 2).is_err());

        let xml = br#"<Root><Datasets><Dataset id="blobs"><Rows><Row type="U"><Col id="payload">AAEC</Col><OrgRow><Col id="payload">AQID</Col></OrgRow></Row></Rows></Dataset></Datasets></Root>"#;
        let json = br#"{"Datasets":[{"id":"blobs","Rows":[{"U":{"payload":"AAEC"},"O":{"payload":"AQID"}}]}]}"#;
        for (input, format) in [
            (xml.as_slice(), Format::NexacroXml4000),
            (json.as_slice(), Format::NexacroJson10),
        ] {
            let response: TrackedBlobResponse = from_slice(input, format).unwrap();
            assert_eq!(response.blobs[0].current.payload.as_bytes(), &[0, 1, 2]);
            assert_eq!(
                response.blobs[0]
                    .original
                    .as_ref()
                    .unwrap()
                    .payload
                    .as_bytes(),
                &[1, 2, 3]
            );
        }
    }

    #[test]
    #[ignore = "manual 32 MiB peak-RSS profile"]
    fn large_blob_round_trip_for_rss_profile() {
        let value = BlobResponse {
            blobs: vec![BlobRow {
                payload: Blob::new((0..(32 << 20)).map(|byte| byte as u8).collect()),
            }],
        };
        for format in [Format::NexacroXml4000, Format::NexacroJson10] {
            let encoded = to_vec(&value, format).unwrap();
            let decoded: BlobResponse = from_slice(&encoded, format).unwrap();
            assert_eq!(
                decoded.blobs[0].payload.as_bytes(),
                value.blobs[0].payload.as_bytes()
            );
        }
    }

    #[test]
    fn optional_vec_bytes_matches_the_documented_dto_shape() {
        let input = br#"<Root><Datasets><Dataset id="blobs"><Rows><Row><Col id="payload">AAEC</Col></Row><Row/></Rows></Dataset></Datasets></Root>"#;
        let response: OptionalBytesResponse = from_slice(input, Format::NexacroXml4000).unwrap();
        assert_eq!(response.blobs[0].payload.as_deref(), Some(&[0, 1, 2][..]));
        assert_eq!(response.blobs[1].payload, None);
    }

    #[test]
    fn xml_explicit_null_between_start_and_end_tags_is_preserved() {
        let input = br#"<Root><Datasets><Dataset id="blobs"><Rows><Row><Col id="payload" null="true"></Col></Row></Rows></Dataset></Datasets></Root>"#;
        let response: OptionalBytesResponse = from_slice(input, Format::NexacroXml4000).unwrap();
        assert_eq!(response.blobs[0].payload, None);
    }

    #[test]
    fn blob_errors_do_not_echo_the_input() {
        let secret = "not-a-secret-looking-base64-value";
        let error = Blob::from_base64(secret, 32).unwrap_err().to_string();
        assert!(!error.contains(secret));
    }

    #[test]
    fn presence_preserves_missing_null_and_empty_across_formats() {
        let xml = br#"<Root><Datasets><Dataset id="values"><Rows><Row/><Row><Col id="value" null="true"/></Row><Row><Col id="value"></Col></Row></Rows></Dataset></Datasets></Root>"#;
        let json = br#"{"Datasets":[{"id":"values","Rows":[{}, {"value":null}, {"value":""}]}]}"#;
        for (input, format) in [
            (xml.as_slice(), Format::NexacroXml4000),
            (json.as_slice(), Format::NexacroJson10),
        ] {
            let response: PresenceResponse = from_slice(input, format).unwrap();
            assert_eq!(response.values[0].value, Presence::Missing);
            assert_eq!(response.values[1].value, Presence::Null);
            assert_eq!(response.values[2].value, Presence::Value(String::new()));
            let encoded = to_vec(&response, format).unwrap();
            let round_trip: PresenceResponse = from_slice(&encoded, format).unwrap();
            assert_eq!(round_trip.values, response.values);
        }
    }
}
