# Fork notes

This fork tracks `anomalyco/hex` `dev` and keeps its own changes small so
upstream merges stay cheap. Prefer upstream behavior over fork code.

## Changes against upstream

- `scripts/install-local.sh`: local build signed with the self-signed
  "Hex Local Build" certificate (`HEX_LOCAL_CODESIGN_IDENTITY` in
  `scripts/build-app.sh`), no Developer ID, updates off.
- Model catalog: Parakeet v3 Primeline (German fine-tune) as the most accurate
  German choice.
- `src/system_load.rs`: slow dictation inference is logged with GPU
  utilization and memory pressure.
- Sidebar: Voice Action, Commands, and Meetings panes are hidden until the
  feature is turned on (`hex settings`).

## Local setup

- Models live on the external SSD: `~/Library/Application Support/hex/models`
  is a symlink to the model folder there.
- Build prerequisites: `uv venv .venv && uv pip install --python .venv/bin/python moonshine-voice==0.0.71`
  (only the dylibs are bundled), `bun install` in `sdk` and `sdk/commands`.
