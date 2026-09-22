# macOS non-UTF-8 reservation fixture

Status: done — verified at `a3591d7`.

- Source evidence: macOS CI job `106680747242`, run `35707772355`.
- Failure: `non_utf8_reservation_identities_do_not_collide` failed at
  `create_dir_all` with OS error 92, Illegal byte sequence.
- Split the test into a pure cross-Unix raw-byte identity and namespace
  proof, plus a Linux-only filesystem-lock acceptance test.
- The test-only split preserves the raw-byte and namespace guarantees.
- No production code changes.
- Tests PASS: cross-Unix identity/namespace proof in macOS CI job `106687000070`, and physical Linux lock acceptance in the all-features Linux suite of job `106687000096`, run `35709691217`.
- Review evidence: `docs/evaluations/20260922-interrupted-opencode-recovery-review.md`.
- The observed macOS filesystem rejected invalid-byte filenames; that is not a
  failure of the reservation hash.
