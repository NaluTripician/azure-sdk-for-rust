# Skill: Decode an Azure diagnostics binary blob

## Name

`decode-azure-diagnostics-blob`

## When to use

Use this skill when you are handed an Azure SDK diagnostics **binary blob** (the `AZD1` format —
it starts with the ASCII bytes `AZD1`) and you need to read it as human-/agent-readable JSON. These
blobs show up in: captured-on-error diagnostics, support bundles, log attachments, or test output.

You do **not** need this skill for diagnostics that are already JSON.

## What it does

Runs the `diag-decode` tool, which:

1. Reads the blob (from a file path, or from stdin).
2. Verifies the `AZD1` magic + version, inflating DEFLATE if the compressed flag is set.
3. Prints the decoded span tree as pretty JSON — a flat node list where each node carries
   `parent` (index link), `kind`, `start_ns`, `duration_ns`, `status`, and `attrs`.

## How to invoke

```bash
# From a file:
diag-decode ./diagnostics.azd1

# From stdin (e.g. piped from a base64 decode):
base64 -d blob.b64 | diag-decode
```

In this repository the tool is built from the spike crate:

```bash
cargo run -p azure_core_diag_combo1 --bin diag-decode -- <blob-file>
```

## Interpreting the output

- The node with `"parent": null` is the **operation** root. Read `attrs` for `az.operation`,
  `az.outcome`, `az.attempt_count`.
- Nodes whose `kind` is `1` are **attempts** — check `status`, `az.service_request_id` (the
  service-sourced `x-ms-request-id`), `az.request_charge`, and `az.error_kind` on failures.
- Nodes whose `kind` is `3` are **routing / fan-out** children — read `az.plan_node_id` and
  `az.feed_range`.
- Reconstruct hierarchy by following each node's `parent` index.

## Failure modes

- `not an AZD1 diagnostics blob` — the input is not this format (wrong file, or already JSON).
- `unsupported AZD1 version N` — the blob is newer than this decoder; upgrade the tool.
- `unexpected end of blob` / `malformed blob` — the input is truncated or corrupted.
