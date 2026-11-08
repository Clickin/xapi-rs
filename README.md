# xapi-rs

Rust runtime for XPLATFORM Dataset XML 4000, Nexacro Dataset XML 4000, and
Nexacro Dataset JSON 1.0, SSV, and binary 5000 profiles.

The workspace contains shared scalar/metadata types, derive macros, a
row-streaming XML codec, a lexical JSON codec, and an optional `xapi-http`
integration crate for the common
Parameters/Datasets/Rows shape. Enable its optional `tokio` feature for a
backpressure-preserving async-to-sync streaming bridge with cancellation and
timeout support. Enable the `zlib` feature when PlatformZlib FFI support is
needed. Profile-specific compatibility remains
documented in `docs/protocol-compatibility.md`.

Use `Vec<Tracked<Row>>` when a dataset carries RowType or an original row;
`XapiRow` deliberately rejects `row_type`/`org_row` field annotations so row
metadata cannot be silently treated as ordinary columns.

```rust
use xapi::{Decimal, XapiScalar, LexicalValue, WireType};

let amount = Decimal::decode_lexical(LexicalValue::Borrowed("65.7700"), WireType::BigDecimal)?;
assert_eq!(amount.as_str(), "65.7700");
# Ok::<(), xapi::DecodeError>(())
```

Run the checks with `cargo test-serial` and
`cargo clippy --workspace --all-targets -- -D warnings`.

`cargo test-serial` limits both Cargo build jobs and test threads to one. Cargo
still keeps unit and integration targets as separate binaries, but they run
with bounded parallelism; merging targets across workspace crates would lose
their package boundaries.
