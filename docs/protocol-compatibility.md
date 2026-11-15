# Protocol compatibility

The runtime supports XPlatform XML 4000, Nexacro XML 4000, and Nexacro JSON
1.0. The compatibility matrix below records the existing verified codec coverage; the conformance
adapter additionally exposes SSV and binary profiles.

`Format::XPlatformXml4000` writes the root the original XPLATFORM driver writes,
`<Root xmlns="http://www.tobesoft.com/platform/dataset" ver="5000">`, and the
conformance adapter exposes it as `xplatform-xml-5000`. `Format::NexacroXml4000`
writes `<Root xmlns="http://www.nexacroplatform.com/platform/dataset">` without a
version attribute, as the original Nexacro driver does.

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

## Driver output

The encoders (`to_writer`, `to_vec`, `dynamic::encode`) write what the original
XPLATFORM and Nexacro Java drivers write, byte for byte, as checked by the
`driver.*` conformance vectors. The functions are public in `xapi::driver`.

- Layout: XML uses the drivers' tab indentation and always writes
  `<Parameters/>`; XPLATFORM XML writes columns before constant columns, Nexacro
  constant columns first. JSON orders object keys like the drivers' `HashMap`
  and writes every value as a string. SSV and PlatformBinary follow the drivers'
  record and block layout.
- Escaping: XPLATFORM XML writes characters below U+0020 and the space as
  decimal references; Nexacro XML escapes only the predefined entities and
  rejects other control characters. JSON escapes like json-simple (`\/`,
  control characters, U+2000-U+20FF).
- Values go through the drivers' type conversion: BOOLEAN writes `1`/`0`,
  numbers are parsed and printed like Java (`Integer`, `Long`, `Double`,
  `BigDecimal`), dates are read as UTC. A type name is written as the drivers
  write it (`int` for BOOLEAN/SHORT/INT, `bigdecimal` for LONG/DECIMAL, `float`
  for FLOAT/DOUBLE).
- Rows are written as the drivers' saveType ALL: live rows in order, deleted rows
  last, an updated row with its original row (an empty one when there is none).
  A null cell is not written, so null reads back as missing.
- Decoding follows the drivers too: a JSON column `size` must be a string, an XML
  BLOB constant without a `value` attribute is rejected, an SSV constant keeps
  `\x03` as the character, and PlatformBinary reads null tags as null and dates
  as the column's date type.
