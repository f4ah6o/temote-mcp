# macOS pinned Git identity inspection

Status: doing

CI run 35709691217, job 106687000070 observed seven failures during
pre-approval `pin_git_repository` identity inspection. macOS canonicalized
`/dev/fd/N` as `/dev/fd/repo` instead of the canonical workspace.

Use the descriptor-derived macOS path only for initial identity inspection,
with descriptor and candidate device/inode validation before and after
inspection.

Do not weaken descriptor-backed post-approval Git execution or its safety
checks. Linux and macOS CI are required.

Post-approval runtime behavior is NOT VERIFIED until CI passes.

No deployment.
