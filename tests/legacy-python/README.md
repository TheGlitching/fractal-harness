# Legacy Python acceptance tests

These tests are the acceptance suite for the original Python implementation,
recovered from git history (`git show bbfd80a^:tests/`). The Python line was
deleted in favour of the single canonical Rust implementation, but these files
record the intended scheduler semantics: escalation (`suspend -> reopen owner ->
resolve(amend|overrule|replan|depends_on)`), budget ledger and exhaustion,
dependency staleness, and the contract/verb terminology in `docs/SPEC.md` §4.

They are **not** run by CI and do not import the current code. They are the
porting reference for the follow-up reliability task that restores those
semantics in Rust.
