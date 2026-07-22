use xapi::XapiRow;

#[derive(XapiRow)]
struct Row {
    #[xapi(col = "amount", wire = "BIGDECIMAL")]
    amount: Option<f64>,
}

fn main() {}
