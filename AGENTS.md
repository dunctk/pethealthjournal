# Pet Health Agent Guide

## Stack

- Rust + Axum
- Askama server-rendered HTML
- HTMX bounded fragment updates
- SeaORM over SQLite in WAL mode
- Rig for optional model-backed event extraction

## MCP-first AI and proactive cost policy

- ChatGPT, Claude and other MCP clients handle primary conversations; Rust owns authoritative records and validation. Keep manual web entry accessible.
- Prefer typed MCP writes and the neutral `record_observation` tool (no extra model call). The legacy `record_health_event` may invoke a second LLM, so don't recommend it for every assistant-generated write.
- OCR and photo-vision are separate optional provider integrations with their own costs and privacy considerations.
- Hosted Free must not run background LLM monitoring. Managed Plus will need consent, server-side **household-level** billing entitlement, spend caps, deduplication and a scheduler. The global managed mode does not grant permission.
- Self-hosted proactive AI, when actually implemented, will use the operator's own provider key. `PROACTIVE_AI_ENABLED` defaults off and currently provides **configuration validation only, no worker**.
- Persist source evidence for deterministic insights. Never diagnose, recommend medication changes or promise emergency monitoring.
- See `docs/ai-architecture.md` for the plan; don't advertise planned insights as shipped.

## Product invariants

- Current source releases use Elastic License 2.0 (source-available, not OSI open source); older AGPL revisions retain their original rights.
- `DEPLOYMENT_MODE` defaults to `self_hosted` and must never require Stripe, license servers, or hosted SaaS subscription entitlements for local core use.
- `managed` is a future hosted SaaS integration mode, not evidence of implemented checkout or paywall behavior; payment and entitlements must be server-enforced across both web and MCP when built.
- Commercial strategy belongs in a separate private repository. Never add business strategy, pricing experiments or real production secrets here.
- Every pet, event, share, and audit query must be household-scoped.
- The LLM never receives database write access and never supplies authoritative timestamps.
- Preserve the user's original health-event wording alongside structured fields.
- Vet access must be revocable, expiring, least-privilege, and non-enumerable.
- `PRODUCTION=true` always stores SQLite data at `/persistent/pethealth.sqlite`.
- Never commit real pet health records, database files, credentials, or share tokens.
- AI output and linked knowledge are context, not diagnosis.

## Verification

Run `cargo fmt --check`, `cargo check --all-targets`, and `cargo test`. For UI changes, test the actual HTMX capture and undo flow at desktop and mobile widths.

