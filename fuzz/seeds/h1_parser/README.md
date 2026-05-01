# H1 parser fuzz corpus seeds

Hand-curated seeds for the `h1_parser` cargo-fuzz target. Each file
is one input the parser will see verbatim. The fuzzer mutates these
to cover the surrounding state space.

| File | What it covers |
|------|----------------|
| `get_simple` | Smallest valid GET / HTTP/1.1 request. |
| `get_with_headers` | Multiple well-formed headers — Connection, User-Agent, Accept. |
| `post_with_body` | POST with Content-Length and body bytes. |
| `pipelined_minor` | Two back-to-back requests in one buffer (HTTP/1.0 + HTTP/1.1). |
| `unknown_method` | Method tokens the parser must accept syntactically (`BREW`). |
| `bogus_version` | Major/minor outside the supported range (`HTTP/9.9`). |
| `truncated` | Cut off in the middle of the request line. |
| `leading_blank_lines` | Spec-allowed leading CRLFs before the request line. |
| `header_no_space` | Header value with no space after the colon. |
| `header_empty_value` | Header with an empty value. |
| `get_binary_path` | Non-ASCII / control bytes in the request-target. |
| `very_long_path` | 2 KB request-target — exercises buffer-size paths. |

To run with this seed corpus:

```bash
cargo fuzz run h1_parser fuzz/seeds/h1_parser -- -max_total_time=60
```

The runtime corpus that libFuzzer mutates lives at
`fuzz/corpus/h1_parser/` and is gitignored — these seeds are the
durable starting point.
