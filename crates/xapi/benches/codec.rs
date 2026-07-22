use std::{hint::black_box, time::Instant};

use xapi::{from_slice, to_vec, Blob, Format, XapiRow};

#[derive(XapiRow)]
struct Row {
    #[xapi(col = "id", wire = "INT")]
    id: i32,
    #[xapi(col = "name", wire = "STRING")]
    name: String,
}

#[derive(xapi::XapiRoot)]
struct Response {
    #[xapi(dataset = "rows")]
    rows: Vec<Row>,
}

#[derive(XapiRow)]
struct BlobRow {
    #[xapi(col = "payload", wire = "BLOB")]
    payload: Blob,
}

#[derive(xapi::XapiRoot)]
struct BlobResponse {
    #[xapi(dataset = "blobs")]
    blobs: Vec<BlobRow>,
}

fn xml(rows: usize) -> Vec<u8> {
    let mut output = String::from("<Root><Datasets><Dataset id=\"rows\"><Rows>");
    for id in 0..rows {
        output.push_str(&format!(
            "<Row><Col id=\"id\">{id}</Col><Col id=\"name\">user{id}</Col></Row>"
        ));
    }
    output.push_str("</Rows></Dataset></Datasets></Root>");
    output.into_bytes()
}

fn json(rows: usize) -> Vec<u8> {
    let values = (0..rows)
        .map(|id| format!("{{\"id\":{id},\"name\":\"user{id}\"}}"))
        .collect::<Vec<_>>()
        .join(",");
    format!("{{\"Datasets\":[{{\"id\":\"rows\",\"Rows\":[{values}]}}]}}").into_bytes()
}

fn blob_xml(size: usize) -> Vec<u8> {
    let payload = Blob::new((0..size).map(|value| value as u8).collect()).to_base64();
    format!(
        "<Root><Datasets><Dataset id=\"blobs\"><Rows><Row><Col id=\"payload\">{payload}</Col></Row></Rows></Dataset></Datasets></Root>"
    )
    .into_bytes()
}

fn blob_json(size: usize) -> Vec<u8> {
    let payload = Blob::new((0..size).map(|value| value as u8).collect()).to_base64();
    format!("{{\"Datasets\":[{{\"id\":\"blobs\",\"Rows\":[{{\"payload\":\"{payload}\"}}]}}]}}")
        .into_bytes()
}

fn measure(name: &str, f: impl FnOnce() -> usize) {
    let start = Instant::now();
    let bytes = black_box(f());
    println!("{name}: {:?}, output={bytes}", start.elapsed());
}

fn main() {
    let xml_10k = xml(10_000);
    let json_10k = json(10_000);
    let xml_100k = xml(100_000);
    let json_100k = json(100_000);
    let xml_value: Response = from_slice(&xml_10k, Format::NexacroXml4000).unwrap();
    let json_value: Response = from_slice(&json_10k, Format::NexacroJson10).unwrap();
    let blob_xml_1mb = blob_xml(1 << 20);
    let blob_json_1mb = blob_json(1 << 20);
    let blob_value = BlobResponse {
        blobs: vec![BlobRow {
            payload: Blob::new((0..(1 << 20)).map(|value| value as u8).collect()),
        }],
    };

    measure("xml_decode_10k_rows", || {
        from_slice::<Response>(&xml_10k, Format::NexacroXml4000)
            .unwrap()
            .rows
            .len()
    });
    measure("json_decode_10k_rows", || {
        from_slice::<Response>(&json_10k, Format::NexacroJson10)
            .unwrap()
            .rows
            .len()
    });
    measure("xml_decode_100k_rows", || {
        from_slice::<Response>(&xml_100k, Format::NexacroXml4000)
            .unwrap()
            .rows
            .len()
    });
    measure("json_decode_100k_rows", || {
        from_slice::<Response>(&json_100k, Format::NexacroJson10)
            .unwrap()
            .rows
            .len()
    });
    measure("xml_encode_10k_rows", || {
        to_vec(&xml_value, Format::NexacroXml4000).unwrap().len()
    });
    measure("json_encode_10k_rows", || {
        to_vec(&json_value, Format::NexacroJson10).unwrap().len()
    });
    measure("xml_decode_1mb_blob", || {
        from_slice::<BlobResponse>(&blob_xml_1mb, Format::NexacroXml4000)
            .unwrap()
            .blobs[0]
            .payload
            .as_bytes()
            .len()
    });
    measure("json_decode_1mb_blob", || {
        from_slice::<BlobResponse>(&blob_json_1mb, Format::NexacroJson10)
            .unwrap()
            .blobs[0]
            .payload
            .as_bytes()
            .len()
    });
    measure("xml_encode_1mb_blob", || {
        to_vec(&blob_value, Format::NexacroXml4000).unwrap().len()
    });
    measure("json_encode_1mb_blob", || {
        to_vec(&blob_value, Format::NexacroJson10).unwrap().len()
    });
}
