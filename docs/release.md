# Release checklist

The crates are published in dependency order because `xapi` depends on the
optional `xapi-derive` crate and `xapi-http` depends on `xapi`:

```text
cargo publish -p xapi-derive
cargo publish -p xapi
cargo publish -p xapi-http
```

The `Release` workflow performs the same sequence when a `v*` tag is pushed.
Configure the repository secret `CARGO_REGISTRY_TOKEN` with a crates.io API
token before pushing a release tag. The workflow also creates the GitHub
Release and generates its notes.

Before publishing, run the workspace quality gates from CI and package each
crate with `cargo package --allow-dirty`. A dependent crate's
package verification requires its predecessor version to be available in the
registry; this cannot be proven offline from a fresh workspace. The CI
job verifies `xapi-derive` directly and prepares all workspace archives with
`--no-verify` for this reason. The CI license-security job runs advisory,
license, source, and dependency-policy checks from `deny.toml`.
