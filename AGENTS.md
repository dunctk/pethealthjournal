# Pet Health Agent Guide

## Stack

- Rust + Axum
- Askama server-rendered HTML
- HTMX bounded fragment updates
- SeaORM over SQLite in WAL mode
- Rig for optional model-backed event extraction

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

