use std::{hint::black_box, time::Instant};

use xapi::{from_reader_with_options, to_vec, Blob, Format, Limits, XapiRow};

#[derive(XapiRow)]
struct BlobRow {
    #[xapi(col = "payload", wire = "BLOB")]
    payload: Blob,
}

#[derive(xapi::XapiRoot)]
struct Response {
    #[xapi(dataset = "blobs")]
    blobs: Vec<BlobRow>,
}

fn payload(size: usize) -> Vec<u8> {
    (0..size).map(|value| value as u8).collect()
}

fn xml(input: &Blob) -> Vec<u8> {
    format!(
        "<Root><Datasets><Dataset id=\"blobs\"><Rows><Row><Col id=\"payload\">{}</Col></Row></Rows></Dataset></Datasets></Root>",
        input.to_base64()
    )
    .into_bytes()
}

fn json(input: &Blob) -> Vec<u8> {
    format!(
        "{{\"Datasets\":[{{\"id\":\"blobs\",\"Rows\":[{{\"payload\":\"{}\"}}]}}]}}",
        input.to_base64()
    )
    .into_bytes()
}

fn measure(name: &str, f: impl FnOnce() -> usize) {
    let start = Instant::now();
    let bytes = black_box(f());
    println!("{name}: {:?}, output={bytes}", start.elapsed());
}

fn main() {
    let input = Blob::new(payload(32 << 20));
    let xml_input = xml(&input);
    let json_input = json(&input);
    let limits = Limits::default();
    let value = Response {
        blobs: vec![BlobRow { payload: input }],
    };

    measure("blob_32mb_xml_decode", || {
        from_reader_with_options::<Response, _>(
            xml_input.as_slice(),
            Format::NexacroXml4000,
            limits,
            false,
        )
        .unwrap()
        .blobs[0]
            .payload
            .as_bytes()
            .len()
    });
    measure("blob_32mb_json_decode", || {
        from_reader_with_options::<Response, _>(
            json_input.as_slice(),
            Format::NexacroJson10,
            limits,
            false,
        )
        .unwrap()
        .blobs[0]
            .payload
            .as_bytes()
            .len()
    });
    measure("blob_32mb_xml_encode", || {
        to_vec(&value, Format::XPlatformXml4000).unwrap().len()
    });
    measure("blob_32mb_json_encode", || {
        to_vec(&value, Format::NexacroJson10).unwrap().len()
    });
}
