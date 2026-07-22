use xapi::XapiRow;

#[derive(XapiRow)]
struct Row {
    #[xapi(col = "id", wire = "INT")]
    first: i32,
    #[xapi(col = "id", wire = "INT")]
    second: i32,
}

fn main() {}
