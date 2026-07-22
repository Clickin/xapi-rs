use xapi::XapiRow;

#[derive(XapiRow)]
struct BadRowMetadata {
    #[xapi(row_type)]
    state: xapi::RowType,
    #[xapi(col = "id", wire = "INT")]
    id: i32,
}

fn main() {}
