//! H1 request-head parser fuzz target.
//!
//! Charter rule (quality gate, item 9): every parser ships with a
//! `cargo-fuzz` target that has run ≥ 1 minute clean.
//!
//! This target drives `httparse::Request::parse` against arbitrary
//! input. httparse is hyper's underlying H1 parser; what's safe for
//! httparse is safe for our ingress.
//!
//! Run locally:
//!
//! ```bash
//! cargo install cargo-fuzz
//! cargo fuzz run h1_parser -- -max_total_time=60
//! ```
//!
//! Or, after a corpus has been seeded:
//!
//! ```bash
//! cargo fuzz run h1_parser fuzz/corpus/h1_parser -- -max_total_time=600
//! ```

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // Same shape as hyper's request-head reader: a fixed-size header
    // array large enough for typical requests. Any panic, OOB read,
    // or other UB inside httparse is caught here.
    let mut headers = [httparse::EMPTY_HEADER; 64];
    let mut req = httparse::Request::new(&mut headers);
    let _ = req.parse(data);
});
