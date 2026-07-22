# ADR 0002: JSON parser selection

Status: accepted for the initial runtime

The JSON profile uses `serde_json::Deserializer` with custom visitors. Scalar
members are captured as `RawValue` one at a time so numbers reach the shared
lexical scalar path without an intermediate `f64`. Unknown root members are
skipped in compatibility mode, while strict mode rejects unknown root members,
parameters, and datasets through the generated decoder.

The visitor dispatches rows immediately when `id` precedes `Rows`, and buffers
only the reordered-key case where `Rows` precedes the dataset `id`.
The XML profile has no equivalent buffer because its event stream identifies a
Dataset before its Rows block.

The input reader independently tracks JSON string escapes and structural
bracket depth, so `Limits::max_depth` also applies to unknown members. Object
keys are rejected when duplicated rather than silently adopting a last-value
policy.
