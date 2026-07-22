use xapi::XapiRow;

#[derive(XapiRow)]
struct Row {
    #[xapi(col = "payload", wire = "BLOB")]
    payload: String,
}

fn main() {}
