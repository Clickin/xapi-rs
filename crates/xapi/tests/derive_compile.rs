#[test]
fn derive_rejects_invalid_schema() {
    let tests = trybuild::TestCases::new();
    tests.compile_fail("tests/ui/*.rs");
}
