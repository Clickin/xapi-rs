# Performance notes

The XML reader consumes `Read` events and sends each completed row to the
derive-generated root decoder; it does not build a raw document. Generated row
decoders scan each row once and dispatch columns through a static `match`, so
they do not perform per-field linear lookup. The JSON reader uses a
`serde_json::Deserializer` visitor and keeps scalar tokens lexical until DTO
conversion. When the usual dataset `id` precedes `Rows`, rows are dispatched
immediately; only the compatibility case where `Rows` precedes `id` needs a
per-dataset buffer. The complete document is never materialized.

XML encoding also uses the generated sink path: parameters, static
`ColumnInfo`, and each row are written directly to `Write`. JSON encoding now
uses the same generated sink path instead of materializing a raw document.
Blob fields reach that sink as bytes and are base64-encoded in bounded output
chunks rather than first materializing a complete base64 string.

No release performance claim is made. The current benchmark covers 10k and
100k rows plus 1 MiB BLOBs; allocation counts and peak RSS still need a
dedicated profiler run before making a memory budget claim. Unsafe code,
custom allocators, and parser-specific pooling are intentionally not used.

The initial 10k/100k-row smoke benchmark is runnable with:

```text
cargo bench --bench codec
```

The 32 MiB BLOB benchmark is separate because it intentionally allocates a
large fixture and uses ten samples:

```text
cargo bench --bench blob
```

These benchmarks use a small standard-library harness and report one
measurement per case; they intentionally avoid adding a benchmark framework
to the runtime dependency graph.

On the development machine used for this checkpoint, the observed median was
about 7.2 ms (XML) and 12.2 ms (JSON) to decode 10k rows, and 118 ms (XML)
and 130 ms (JSON) for 100k rows. Encoding 10k rows measured about 11.3 ms
(XML) and 2.0 ms (JSON). A 1 MiB BLOB measured about 1.6 ms/23.6 ms to decode
and 10.3 ms/2.8 ms to encode (XML/JSON respectively). These are development
measurements, not release targets; allocation counts and peak memory still
need dedicated profiling.

The short 32 MiB BLOB run (`10` samples, `0.5 s` measurement target) measured
about 40.4 ms (XML decode), 211.6 ms (JSON decode), 89.3 ms (XML encode), and
25.1 ms (JSON encode) after the owned-row decode path was enabled. These
numbers are directional because the run was shortened for local feedback.

A one-shot release profile is available as the ignored test
`tests::large_blob_round_trip_for_rss_profile`. On the development arm64
macOS machine it completed both XML and JSON round-trips with a peak RSS of
about 282 MiB (`/usr/bin/time -l`); this includes the test harness and both
formats, so it is a directional upper bound rather than a hard allocation
budget.
