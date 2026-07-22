use xapi::XapiRow;

#[derive(XapiRow)]
struct Row {
    #[xapi(col = "id", wire = "INT", size = 0)]
    id: i32,
}

fn main() {}
