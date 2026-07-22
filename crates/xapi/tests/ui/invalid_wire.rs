use xapi::XapiRow;

#[derive(XapiRow)]
struct Row {
    #[xapi(col = "id", wire = "NOT_A_WIRE")]
    id: i32,
}

fn main() {}
