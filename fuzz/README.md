# Fuzzing

The wire decoder has a [cargo-fuzz](https://github.com/rust-fuzz/cargo-fuzz) target.
It is not part of CI (it needs nightly and libFuzzer).

    cargo install cargo-fuzz
    cd fuzz
    cargo +nightly fuzz run decode

Seed corpus files live in `fuzz/corpus/decode/` and crashes land in
`fuzz/artifacts/decode/`; neither is committed.
