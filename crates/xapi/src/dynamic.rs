//! Runtime-schema bridge used by protocol tooling such as conformance runners.
//! Handwritten application DTOs should use `XapiRoot`/`XapiRow` instead.

use std::io::{Read, Write};

use crate::{
    DecodeError, Error, Format, Limits, RawDataset, RawDocument, RawRow, RawValue, Result,
    WireType, XapiEncodeSink, XapiRoot, XapiRootDecoder,
};

pub type Document = RawDocument;

pub fn decode<R: Read>(
    reader: R,
    format: Format,
    limits: Limits,
    strict: bool,
) -> Result<Document> {
    crate::from_reader_with_options(reader, format, limits, strict)
}

pub fn encode<W: Write>(writer: W, document: &Document, format: Format) -> Result<()> {
    match format {
        Format::XPlatformXml4000 | Format::NexacroXml4000 => crate::xml::encode(writer, document),
        Format::NexacroJson10 => crate::json::encode(writer, document),
    }
}

#[derive(Default)]
pub struct Decoder {
    document: RawDocument,
    current: Option<usize>,
}

impl XapiRootDecoder<RawDocument> for Decoder {
    fn parameter(&mut self, id: &str, value: &RawValue) -> Result<()> {
        self.document
            .parameters
            .push((id.to_owned(), value.clone()));
        Ok(())
    }

    fn dataset_begin(&mut self, dataset: &str) -> Result<()> {
        self.document.datasets.push(RawDataset {
            id: dataset.to_owned(),
            columns: Vec::new(),
            const_columns: Vec::new(),
            rows: Vec::new(),
        });
        self.current = Some(self.document.datasets.len() - 1);
        Ok(())
    }

    fn dataset_columns(&mut self, _dataset: &str, columns: &[(String, WireType)]) -> Result<()> {
        if let Some(index) = self.current {
            let constants = self.document.datasets[index]
                .const_columns
                .iter()
                .map(|(id, _, _)| id.as_str())
                .collect::<std::collections::HashSet<_>>();
            self.document.datasets[index].columns = columns
                .iter()
                .filter(|(id, _)| !constants.contains(id.as_str()))
                .cloned()
                .collect();
        }
        Ok(())
    }

    fn dataset_const_column(
        &mut self,
        _dataset: &str,
        id: &str,
        wire: WireType,
        value: &RawValue,
    ) -> Result<()> {
        if let Some(index) = self.current {
            self.document.datasets[index]
                .const_columns
                .push((id.to_owned(), wire, value.clone()));
            self.document.datasets[index]
                .columns
                .retain(|(column, _)| column != id);
        }
        Ok(())
    }

    fn dataset_row(&mut self, _dataset: &str, row: RawRow) -> Result<()> {
        let Some(index) = self.current else {
            return Err(Error::from(DecodeError::message(
                "row received before dataset",
            )));
        };
        self.document.datasets[index].rows.push(row);
        Ok(())
    }

    fn finish(self) -> Result<RawDocument> {
        Ok(self.document)
    }
}

impl XapiRoot for RawDocument {
    type Decoder = Decoder;

    fn schema() -> &'static crate::RootSchema<Self> {
        static FIELDS: &[crate::RootFieldSchema] = &[];
        static SCHEMA: crate::RootSchema<RawDocument> = crate::RootSchema {
            fields: FIELDS,
            marker: std::marker::PhantomData,
        };
        &SCHEMA
    }

    fn decoder(_strict: bool, _limits: Limits) -> Self::Decoder {
        Decoder::default()
    }

    fn decode_raw(document: RawDocument) -> Result<Self> {
        Ok(document)
    }

    fn encode_raw(&self) -> Result<RawDocument> {
        Ok(self.clone())
    }

    fn encode_stream(&self, _sink: &mut dyn XapiEncodeSink) -> Result<()> {
        Err(Error::from(DecodeError::message(
            "use xapi::dynamic::encode for runtime-schema documents",
        )))
    }
}
