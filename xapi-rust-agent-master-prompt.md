# Rust XAPI Dataset Protocol Runtime — Agent Master Prompt

## 1. 프로젝트 목표

TOBEsoft XPLATFORM 및 Nexacro Dataset 통신 형식을 Rust에서 타입 안전하고 고성능으로 처리하는 독립 라이브러리를 개발하라.

지원 형식:

- XPLATFORM Dataset XML 4000
- Nexacro Dataset XML 4000
- Nexacro Dataset JSON 1.0

개발자는 DTO를 직접 Rust struct로 작성한다.

```rust
#[derive(XapiRoot)]
struct SearchUsersResponse {
    #[xapi(param = "ErrorCode", wire = "INT")]
    error_code: i32,

    #[xapi(param = "ErrorMsg", wire = "STRING")]
    error_msg: String,

    #[xapi(dataset = "users")]
    users: Vec<UserRow>,
}

#[derive(XapiRow)]
struct UserRow {
    #[xapi(col = "id", wire = "INT", size = 10)]
    id: i32,

    #[xapi(col = "name", wire = "STRING", size = 100)]
    name: String,

    #[xapi(col = "balance", wire = "BIGDECIMAL", size = 31)]
    balance: BigDecimal,

    #[xapi(col = "photo", wire = "BLOB")]
    photo: Option<Vec<u8>>,
}
```

Derive macro는 사용자가 작성한 DTO에 trait implementation과 정적 metadata를 추가한다.

외부 generator 없이도 runtime이 완전히 동작해야 한다.

핵심 목표:

1. 업무 코드가 구체적인 Rust DTO만 사용한다.
2. string column lookup과 dynamic map을 일반 API에서 제거한다.
3. XML과 JSON이 동일한 DTO schema를 공유한다.
4. derive metadata로 reflection 없는 dispatch를 구현한다.
5. decimal precision과 lexical value를 보존한다.
6. 대규모 Dataset과 BLOB을 streaming 처리한다.
7. 라이브러리가 애플리케이션 병목이 되지 않게 한다.
8. malformed input에서 panic하지 않는다.
9. 공개 문서 기반 clean-room 구현으로 만든다.

이 repository는 Rust 전용 독립 repository다.

Runtime과 proc macro를 분리하기 위한 Cargo workspace는 허용한다. 플랫폼 중립 IDL은 필수 dependency가 아니다.

---

## 2. 참조 자료

조사 대상:

- `https://github.com/Clickin/xapi-js`
- `https://github.com/Clickin/xplatform-xml`
- TOBEsoft 공개 문서
- 실제 또는 비식별화 fixture

금지:

- TOBEsoft 상용 Java/C# source/binary 복제
- vendor 내부 API 재현
- 검증되지 않은 protocol 추측
- TypeScript/JavaScript 한계를 Rust로 그대로 가져오기
- 기존 precision loss나 global state 복제

작성:

```text
docs/clean-room.md
docs/protocol-compatibility.md
docs/performance.md
docs/adr/
```

---

## 3. 기본 사용 방식

Decode:

```rust
let response: SearchUsersResponse =
    xapi::from_reader(reader, Format::NexacroJson10)?;
```

Encode:

```rust
xapi::to_writer(
    writer,
    &response,
    Format::XPlatformXml4000,
)?;
```

Codec:

```rust
let codec = Codec::builder()
    .format(Format::NexacroJson10)
    .strict(true)
    .limits(Limits::default())
    .build()?;

let response: SearchUsersResponse = codec.decode(reader)?;
```

일반 문서에서는 다음을 권장하지 않는다.

```rust
dataset.get(row, "column")
serde_json::Value
HashMap<String, Value>
downcast_ref
```

Dynamic API가 필요하면 `xapi::dynamic`에 격리한다.

---

## 4. 권장 Cargo workspace

```text
/
├─ crates/
│  ├─ xapi/
│  ├─ xapi-derive/
│  └─ xapi-http/
├─ testdata/
│  ├─ xplatform-xml/
│  ├─ nexacro-xml/
│  ├─ nexacro-json/
│  ├─ invalid/
│  └─ expected/
├─ docs/
│  ├─ protocol-compatibility.md
│  ├─ clean-room.md
│  ├─ performance.md
│  └─ adr/
├─ Cargo.toml
└─ README.md
```

책임:

- `xapi`: core schema/profile/scalar/XML/JSON
- `xapi-derive`: handwritten DTO derive macro
- `xapi-http`: optional HTTP integration

Proc macro 외에는 불필요하게 crate를 세분화하지 않는다.

---

## 5. 지원 protocol model

지원:

- Root
- Parameters / Parameter
- Datasets / Dataset
- ColumnInfo
- Column / ConstColumn
- Rows / Row
- RowType
- OrgRow
- JSON O row
- missing
- empty
- explicit null
- control-character entity
- BLOB
- profiles
- Content-Type/charset

Wire type:

```text
STRING
INT
LONG
FLOAT
DOUBLE
DECIMAL
BIGDECIMAL
DATE
TIME
DATETIME
BLOB
BOOLEAN
```

Profile별 실제 지원 범위를 compatibility matrix에 기록한다.

---

## 6. 필수 설계 원칙

### 6.1 Lexical value를 먼저 보존한다

```text
XML text / JSON lexical token
            ↓
LexicalValue
            ↓
XapiScalar
            ↓
DTO field
```

XML `ColumnInfo.type`으로 먼저 Rust primitive로 coercion하지 않는다.

예:

```xml
<Column id="amount" type="DECIMAL"/>
<Col id="amount">65.7700</Col>
```

DTO가 `String`이면 `"65.7700"`을 보존한다. `BigDecimal`이면 정확히 변환한다. 중간 `f64`는 금지한다.

권장 abstraction:

```rust
pub enum LexicalValue<'a> {
    Missing,
    Null,
    Borrowed(&'a str),
    Owned(String),
    Bytes(Cow<'a, [u8]>),
}
```

실제 API는 lifetime/ergonomics에 맞게 개선할 수 있다. Borrowed representation을 우선 검토한다.

### 6.2 Parameter, ConstColumn, Col은 동일 scalar path를 사용한다

```rust
pub trait XapiScalar: Sized {
    fn decode_lexical(
        value: LexicalValue<'_>,
        wire_type: WireType,
    ) -> Result<Self, DecodeError>;

    fn encode_lexical<W: ScalarWriter>(
        &self,
        wire_type: WireType,
        writer: &mut W,
    ) -> Result<(), EncodeError>;
}
```

다음 위치가 같은 logic을 사용한다.

- Parameter
- ConstColumn
- Row Col
- OrgRow Col
- JSON property

위치별 `to_string` 구현을 만들지 않는다.

### 6.3 DECIMAL/BIGDECIMAL은 exact type이다

Core는 lexical decimal wrapper를 제공한다.

Optional feature:

```text
bigdecimal
rust_decimal
```

특정 decimal crate를 필수 dependency로 강제하지 않는다.

`BIGDECIMAL` + `f64` derive mapping은 기본 compile error 또는 strict incompatibility로 처리한다. 명시적 precision-loss opt-in 없이 허용하지 않는다.

### 6.4 BLOB은 base64다

```rust
struct AttachmentRow {
    #[xapi(col = "content", wire = "BLOB")]
    content: Vec<u8>,
}
```

지원:

- XML base64 text
- JSON base64 string
- standard base64 encode
- zero length
- invalid alphabet/padding
- whitespace policy
- 최대 크기
- Parameter/ConstColumn/Row/OrgRow

목표:

```text
BLOB bytes
 → incremental base64
 → XML/JSON writer
```

다음을 동시에 만들지 않는다.

```text
Vec<u8>
+ 완성된 base64 String
+ 완성된 XML String
```

선택적 advanced API:

```rust
pub trait BlobSource {
    fn len(&self) -> Option<u64>;
    fn reader(&self) -> Result<Box<dyn Read + '_>, Error>;
}
```

필수 fixture:

- 0/1/2/3 bytes
- padding
- 1MB/32MB
- invalid alphabet/padding
- whitespace
- oversized
- 모든 scalar 위치

### 6.5 ASCII 제어문자 entity를 지원한다

Encode 대상:

```text
0x01–0x08
0x0B–0x0C
0x0E–0x1F
```

유지:

```text
TAB
LF
CR
SPACE
```

NUL은 오류다.

Decode는 decimal numeric reference를 정확히 한 번만 복원한다.

`&amp;#1;`을 control character로 double-decode하지 않는다.

반복 replace chain을 피하고 XML event parser 또는 single-pass compatibility decoder를 사용한다.

테스트:

- `&`
- `&amp;`
- literal `&#1;`
- actual control char
- `&amp;#1;`
- malformed numeric entity
- CDATA
- attribute
- Unicode 혼합
- NUL

### 6.6 Missing/null/empty를 구분한다

기본:

```rust
Option<String>
```

- `None`: missing
- `Some("")`: empty

Explicit null까지 구분할 필요가 있으면:

```rust
pub enum Presence<T> {
    Missing,
    Null,
    Value(T),
}
```

다음 상태를 합치지 않는다.

- property missing
- JSON null
- XML Col missing
- empty Col
- empty string
- zero
- false
- empty BLOB

### 6.7 Column 순서를 신뢰하지 않는다

Derived metadata는 static field decoder를 생성한다.

Dataset 시작 시:

```text
wire column ID → generated field decoder slot
```

Row마다 금지:

- HashMap 생성
- field-name allocation
- dynamic Any
- linear attribute search
- schema clone

가능하면 macro가 작은 static match 또는 효율적인 dispatch를 생성한다.

### 6.8 Empty Dataset에도 ColumnInfo를 출력한다

`Vec<T>`가 비어 있어도 `T: XapiRow` static schema로 ColumnInfo를 생성한다.

첫 row를 보고 schema를 추론하지 않는다.

권장:

- String은 STRING inference 검토 가능
- DECIMAL/BLOB/DATE는 명시 필수
- 모호한 type은 compile error

### 6.9 RowType/original row를 typed model로 제공한다

```rust
pub enum RowType {
    Normal,
    Insert,
    Update,
    Delete,
}

pub struct Tracked<T> {
    pub row_type: RowType,
    pub current: T,
    pub original: Option<Box<T>>,
}
```

Mapping:

```text
XML: update Row + OrgRow
JSON: U row + O row
```

JSON decoder는 직전 U row만 유지하는 streaming state machine을 우선 검토한다. O row를 위해 전체 Dataset을 보관하지 않는다.

### 6.10 날짜/시간은 timezone 없는 type이다

```rust
pub struct XapiDate {
    // YYYYMMDD
}

pub struct XapiTime {
    // HHMMSS + confirmed precision
}

pub struct XapiDateTime {
    // local datetime without timezone
}
```

Optional feature:

```text
time
chrono
```

요구:

- timezone shift 없음
- lexical round-trip
- leap year
- invalid date
- fractional precision
- unsupported representation error

기존 TypeScript parser의 길이 가정을 검증 없이 복사하지 않는다.

### 6.11 Global mutable configuration 금지

`static mut`, process-global profile mutation, global lock 기반 current options를 금지한다.

```rust
let codec = Codec::builder()
    .format(Format::NexacroJson10)
    .strict(true)
    .build()?;
```

Codec은 가능하면 `Send + Sync`여야 하며 concurrent test를 작성한다.

### 6.12 Content-Type parameter 처리

지원:

```text
application/xml
application/xml; charset=UTF-8
text/xml
text/xml; charset=utf-8
application/json
application/json; charset=UTF-8
```

Typed header parser를 사용하고 완전 일치 문자열 비교를 하지 않는다.

### 6.13 End-to-end streaming

필수 API:

```rust
pub fn from_reader<T, R>(
    reader: R,
    format: Format,
) -> Result<T, Error>
where
    T: XapiRoot,
    R: Read;

pub fn to_writer<T, W>(
    writer: W,
    value: &T,
    format: Format,
) -> Result<(), Error>
where
    T: XapiRoot,
    W: Write;
```

편의 API:

```text
from_slice
to_vec
```

금지:

- 전체 XML String
- generic DOM
- 전체 `serde_json::Value`
- dynamic Dataset 전체 복제
- 모든 Reader에 `read_to_end`
- BLOB double buffering
- 모든 scalar String allocation
- row마다 map
- output 전체 String 생성

Decode:

```text
Read
 → event parser
 → generated field dispatch
 → DTO
```

Encode:

```text
DTO
 → static schema
 → event writer
 → Write
```

Async API는 별도 bridge로 제공한다. Sync core를 특정 runtime에 종속시키지 않는다.

### 6.14 XML library 선택은 자유지만 검증한다

`quick-xml`을 우선 검토할 수 있으나 필수는 아니다.

비교 기준:

- streaming pull/events
- namespace
- CDATA
- entity/numeric reference
- borrowed text
- allocation
- malformed error
- maintenance/license/fuzzing
- throughput

ADR:

```text
docs/adr/xxxx-xml-parser-selection.md
```

모든 후보는 동일 fixture를 통과해야 한다. Correctness 실패 parser는 빠르더라도 탈락한다.

Custom parser는 최후 수단이다.

조건:

- 기존 library의 구체적 한계
- XAPI vocabulary scope
- fuzzing
- panic-free
- entity/CDATA/control tests
- benchmark 이득
- safe implementation 우선

### 6.15 성능은 first-class requirement다

측정:

- MB/s
- rows/s
- allocated bytes
- allocation count
- peak heap/RSS
- borrowed/owned ratio
- clone 수
- parser-only
- parser+DTO
- encode/base64
- schema metadata
- feature별 영향

Release blocker:

- 전체 document duplication
- 모든 scalar allocation
- BLOB double buffering
- row마다 map/column-name allocation
- 100k rows의 과도한 memory
- malformed panic
- unsafe invariant 부재
- cancellation 무시

근거 없이 금지:

- unsafe
- unchecked UTF-8
- raw pointer
- custom allocator
- architecture-specific SIMD
- handwritten parser
- complex pooling

Unsafe가 필요하면:

- safe fallback
- benchmark
- safety invariant
- `SAFETY` comment
- Miri
- fuzz
- platform 범위

### 6.16 Path-aware error

예:

```text
SearchUsersResponse.users[17].balance:
cannot decode "12A.30" as BIGDECIMAL
```

포함:

- DTO type
- Dataset ID
- row index
- field/column ID
- wire/target type
- source chain

민감 데이터와 전체 BLOB을 출력하지 않는다.

### 6.17 Security limits

```rust
pub struct Limits {
    pub max_document_bytes: u64,
    pub max_depth: usize,
    pub max_datasets: usize,
    pub max_columns: usize,
    pub max_rows: usize,
    pub max_scalar_bytes: usize,
    pub max_blob_bytes: usize,
}
```

XML:

- DTD/external entity 거부
- depth/entity expansion
- malformed reference
- duplicate attributes

JSON:

- depth/string size/number length
- duplicate key policy
- row count

---

## 7. Core trait과 derive

초안:

```rust
pub trait XapiRoot: Sized {
    fn schema() -> &'static RootSchema<Self>;
}

pub trait XapiRow: Sized {
    fn schema() -> &'static RowSchema<Self>;
}

pub trait XapiScalar: Sized {
    fn decode_lexical(
        value: LexicalValue<'_>,
        wire_type: WireType,
    ) -> Result<Self, DecodeError>;

    fn encode_lexical<W: ScalarWriter>(
        &self,
        wire_type: WireType,
        writer: &mut W,
    ) -> Result<(), EncodeError>;
}
```

실제 API는 compiler ergonomics/performance에 맞게 개선한다.

지원 derive:

```rust
#[derive(XapiRoot)]
#[derive(XapiRow)]
```

Root attributes:

```rust
#[xapi(param = "ErrorCode", wire = "INT")]
#[xapi(dataset = "users")]
```

Row attributes:

```rust
#[xapi(col = "id", wire = "INT", size = 10)]
#[xapi(const_col = "region", wire = "STRING", value = "KR")]
#[xapi(row_type)]
#[xapi(org_row)]
```

Compile-time validation:

- conflicting attributes
- empty ID
- duplicate Parameter/Dataset/Column
- ConstColumn collision
- unsupported wire
- invalid size
- invalid Dataset collection
- row가 XapiRow 미구현
- OrgRow type mismatch
- RowType field mismatch
- BLOB type mismatch
- BIGDECIMAL + f64
- invalid recursion
- unmapped field policy

`trybuild` compile-fail tests를 작성한다.

Macro panic을 사용자에게 노출하지 않고 field span에 진단한다.

---

## 8. Profile 설계

```rust
pub enum Format {
    XPlatformXml4000,
    NexacroXml4000,
    NexacroJson10,
}
```

Profile 정책:

- format
- namespace/version/version attribute
- Parameter encoding
- Dataset wrapper
- RowType/original row
- null/date/time/BLOB
- Content-Type
- compatibility variation

Codec options는 immutable하다.

---

## 9. 구현 단계

### Phase R1 — 조사/workspace

조사:

- xapi-js wire model/custom parser/schema
- global mutable options
- numeric precision 한계
- ConstColumn/column ordering 문제
- Java processor/lexical preservation/framework 시행착오
- 공식 XML/JSON 문서
- fixture

작성:

```text
docs/protocol-compatibility.md
docs/clean-room.md
```

완료:

- Cargo workspace
- skeleton traits/profile
- compile tests
- `cargo test --workspace`

### Phase R2 — Core schema/scalar

구현:

- WireType/LexicalValue
- scalar traits
- Decimal
- Date/Time/DateTime
- BLOB
- Presence
- Limits
- RowType/Tracked
- errors/profile
- common scalar path

테스트:

- overflow
- exact decimal
- date
- BLOB
- missing/null/empty
- control utility
- Send/Sync

### Phase R3 — Derive macro

구현:

- XapiRoot/XapiRow
- static metadata
- field dispatch
- compile-time validation
- generated encoders/decoders

테스트:

- trybuild
- duplicate column
- invalid wire
- BIGDECIMAL+f64
- invalid dataset
- wrong OrgRow
- conflicting attributes
- custom scalar
- optional/tracked

### Phase R4 — XML codec

Event-based reader/writer.

지원:

- namespace/version
- direct Dataset/validated wrapper
- Parameter text/value
- ColumnInfo/ConstColumn
- empty Dataset
- reordered/sparse columns
- RowType/OrgRow
- CDATA/entity/control entity
- NUL rejection
- BLOB/date/time/Unicode
- limits

완료:

- round-trip
- canonical snapshots
- malformed/fuzz
- 10k/100k benchmarks
- memory profile

### Phase R5 — JSON codec

전체 `serde_json::Value` materialization을 기본 구현으로 사용하지 않는다.

검토:

- Visitor
- DeserializeSeed
- streaming deserializer
- arbitrary precision
- lexical number visitor

지원:

- Parameters/Datasets/ColumnInfo/Rows
- N/I/U/D/O
- streaming U/O reconstruction
- missing/null/empty
- BLOB/exact decimal/date/time
- reordered keys
- duplicate key policy

완료:

- round-trip
- U/O
- exact decimal
- 100k rows
- bounded memory

### Phase R6 — Public API

구현:

```text
from_slice
from_reader
to_vec
to_writer
Codec::builder
Codec::decode
Codec::encode
```

Options:

- strict
- unknown column
- missing Dataset
- Limits
- decimal encoding
- JSON null
- compatibility

Dynamic API는 별도 module. Rustdoc examples는 compile되어야 한다.

### Phase R7 — HTTP integration

Core는 HTTP dependency가 없어야 한다.

`xapi-http`에 구현한다.

Reqwest client 또는 framework-neutral helper를 먼저 구현한다. Server framework는 초기 하나만 선택한다. Axum이 자연스러우면 Axum을 사용한다.

요구:

- streaming body
- limits
- Content-Type parameter
- charset
- cancellation/timeout
- non-2xx
- typed errors
- headers/auth
- backpressure
- sync/async bridge 복사 비용 측정

### Phase R8 — Conformance/fuzz/benchmark

Fuzz:

- XML/entity/control
- JSON/scalar/base64
- U/O
- derive attribute parser
- date/time

Benchmark:

- small
- 100/10k/100k rows
- sparse/dense
- 1MB/32MB BLOB
- Unicode/control
- OrgRow/UO
- parser-only
- parser+DTO
- encode/base64
- metadata access

### Phase R9 — Parser 선택/release

Parser 후보 비교. 모든 correctness fixtures 통과가 선행 조건이다.

작성:

```text
docs/adr/xxxx-xml-parser-selection.md
docs/performance.md
```

CI:

```text
cargo test --workspace --all-features
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo fmt --check
cargo doc
trybuild
fuzz smoke
license/security check
```

Unsafe가 있으면 Miri를 추가한다. Crates.io publish dry-run을 수행한다.

---

## 10. Java/TypeScript 교훈 체크리스트

- [ ] Wire type으로 선행 coercion하지 않는가
- [ ] String이 decimal lexical value를 보존하는가
- [ ] BIGDECIMAL이 f64를 거치지 않는가
- [ ] 모든 scalar 위치가 같은 codec을 쓰는가
- [ ] ConstColumn BLOB이 base64인가
- [ ] control entity가 single-pass인가
- [ ] `&amp;#1;` double-decode가 없는가
- [ ] NUL 거부가 있는가
- [ ] missing/empty/null이 구분되는가
- [ ] optional field를 empty로 serialize하지 않는가
- [ ] Column 순서를 신뢰하지 않는가
- [ ] empty Dataset ColumnInfo가 있는가
- [ ] typed RowType/OrgRow가 있는가
- [ ] global mutable option이 없는가
- [ ] Content-Type parameter를 처리하는가
- [ ] XML 전체 DOM이 없는가
- [ ] JSON 전체 Value가 없는가
- [ ] BLOB double buffering이 없는가
- [ ] row마다 map이 없는가
- [ ] malformed input panic이 없는가
- [ ] framework integration이 core와 분리되었는가

---

## 11. 최종 합격 기준

1. Handwritten Rust DTO를 사용한다.
2. XML/JSON이 동일 DTO를 사용한다.
3. Derive가 reflection 없는 binding을 생성한다.
4. Decimal precision이 보존된다.
5. BLOB이 모든 위치에서 base64다.
6. ASCII control entities가 정확하다.
7. Missing/null/empty가 구분된다.
8. Column 순서 독립적이다.
9. Empty Dataset ColumnInfo가 생성된다.
10. RowType/OrgRow가 typed model이다.
11. Streaming Reader/Writer가 기본이다.
12. 100k rows memory가 bounded하다.
13. BLOB double buffering이 없다.
14. Codec이 immutable하고 가능하면 Send+Sync다.
15. Strict/compatibility mode가 분리된다.
16. Security limits가 있다.
17. Parser ADR/benchmark 문서가 있다.
18. test/clippy/fmt/doc이 통과한다.
19. Clean-room 문서가 있다.
20. Crates.io publish 가능하다.

---

## 12. Agent 작업 방식

- Repository를 먼저 읽는다.
- 기존 코드와 중복 구현하지 않는다.
- 각 phase를 작은 리뷰 단위로 유지한다.
- fixture 없는 compatibility 예외를 추가하지 않는다.
- 미확정 protocol을 추측하지 않는다.
- 테스트를 약화하지 않는다.
- benchmark 없는 unsafe를 추가하지 않는다.
- public API 변경 영향과 migration을 보고한다.
- 합리적인 기본 결정을 내려 진행한다.
- 불확실성은 문서화한다.

Phase 보고:

```text
1. 구현 범위
2. 설계 결정
3. 주요 파일
4. 테스트 결과
5. benchmark 결과
6. memory 결과
7. protocol 불확실성
8. 남은 작업
9. 다음 phase
```
