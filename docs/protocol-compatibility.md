# Protocol compatibility

The supported profiles are `XPlatformXml4000`, `NexacroXml4000`, and
`NexacroJson10`. The compatibility matrix will be expanded as public fixtures
are added; unverified behavior is not silently treated as compatible.

| Feature | XPlatform XML 4000 | Nexacro XML 4000 | Nexacro JSON 1.0 |
|---|---|---|---|
| Root/parameters/datasets | verified (`xmlns`, `ver`, direct `Dataset`) | verified (`xmlns`, `version`, direct `Dataset`) | verified (`version`, array `Parameters`/`Datasets`) |
| Typed scalar lexical preservation | basic | basic | basic |
| Exact decimal wrapper | basic | basic | basic |
| Base64 BLOB | basic scalar path | basic scalar path | basic scalar path |
| RowType/original row | basic | basic | basic |
| Missing/null/empty (`Option`/`Presence`) | verified | verified | verified |
| Streaming bounded DTO construction | verified common shape | verified common shape | verified, including reordered dataset keys |
| `ColumnInfo` / `ConstColumn` metadata | verified | verified | verified |
| JSON duplicate-key/depth policy | n/a | n/a | rejected / limited |

The tested vendor shapes are kept as clean-room fixtures in
`testdata/xplatform-xml/direct-dataset.xml` and
`testdata/nexacro-json/canonical.json`. They cover `Parameters`, direct and
wrapped datasets, typed `ColumnInfo`, `ConstColumn`, `Rows`, `_RowType_`, and
separate original (`O`) rows. Vendor-specific aliases and profile variations
without fixtures remain unverified.
