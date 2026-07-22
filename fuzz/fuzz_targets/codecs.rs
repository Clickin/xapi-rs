#![no_main]

use libfuzzer_sys::fuzz_target;
use xapi::{from_slice, Format, XapiRoot, XapiRow};

#[derive(XapiRow)]
struct Row {
    #[xapi(col = "id", wire = "INT")]
    id: i32,
    #[xapi(col = "name", wire = "STRING")]
    name: String,
}

#[derive(XapiRoot)]
struct Response {
    #[xapi(param = "ErrorCode", wire = "INT")]
    code: i32,
    #[xapi(dataset = "rows")]
    rows: Vec<Row>,
}

fuzz_target!(|input: &[u8]| {
    let _ = from_slice::<Response>(input, Format::NexacroXml4000);
    let _ = from_slice::<Response>(input, Format::NexacroJson10);
});
