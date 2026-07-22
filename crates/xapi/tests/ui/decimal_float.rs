use xapi::XapiRow;

#[derive(XapiRow)]
struct BadDecimal {
    #[xapi(col = "amount", wire = "DECIMAL")]
    amount: f64,
}

fn main() {}
