# ADR 0001: XML parser selection

Status: accepted

The XML codec uses `quick-xml`'s event reader/writer. It was selected because
it provides the required pull-based events, CDATA handling, malformed-input
errors, and bounded `Read`/`Write` integration without introducing a DOM.

The implementation additionally rejects DTD/DOCTYPE input, applies depth and
size limits, and has regression coverage for numeric references, control
entities, CDATA, NUL values, reordered columns, and empty datasets. Parser
selection is based on these correctness properties as well as throughput.
