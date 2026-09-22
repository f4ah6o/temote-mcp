# macOS non-UTF-8 reservation fixture

Status: doing; pending CI.

- Source evidence: macOS CI job `106680747242`, run `35707772355`.
- Failure: `non_utf8_reservation_identities_do_not_collide` failed at
  `create_dir_all` with OS error 92, Illegal byte sequence.
- Split the test into a pure cross-Unix raw-byte identity and namespace
  proof, plus a Linux-only filesystem-lock acceptance test.
- The test-only split preserves the raw-byte and namespace guarantees.
- No production code changes.
- Tests pending.
- The observed macOS filesystem rejected invalid-byte filenames; that is not a
  failure of the reservation hash.
