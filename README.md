# Pet Health

Pet Health is a source-available, Elastic-2.0-licensed, self-hostable health timeline for multi-pet households. It uses Rust, Axum, Askama, HTMX, SeaORM, SQLite/WAL, and Rig for optional LLM-backed structured capture.

The first complete workflow is intentionally direct:

> An owner can write “Milo vomited just now”, immediately see a timestamped structured event on Milo's timeline, and undo it.

**MCP-first AI direction:** ChatGPT, Claude and other MCP clients are the primary conversational interface; Pet Health Journal owns the structured records, household permissions, audit trail and manual data entry. Existing in-app chat and model-assisted free-text entry remain for compatibility, but shouldn't duplicate the client's reasoning.

The application also creates expiring, revocable, read-only vet links. Share tokens are stored only as SHA-256 hashes and are displayed to the owner once.

## Proactive AI and model costs

**Roadmap only:** proactive monitoring is not running today. Future deterministic checks can calculate weight changes, event frequencies and similar measurable patterns without an LLM. Scheduled AI summaries would be optional, bounded and evidence-backed, never a diagnosis or emergency-monitoring guarantee.

- **Hosted Free:** no automatic recurring LLM scans. ChatGPT/Claude provide conversational reasoning as the client.
- **Hosted Plus (proposed):** opt-in proactive AI only with server-enforced *per-household* entitlements, monthly budgets, consent, scheduling and deduplicated alerts.
- **Self-hosted:** local deterministic checks, plus an optional future AI worker using the operator's own model key and budget.

`PROACTIVE_AI_ENABLED` defaults to `false`. It is currently **a startup guard, not a working analysis scheduler**. True is allowed only on self-hosted deployments with a configured model key; managed mode rejects true until paid entitlement and worker support exist. Turning it on today does **not** generate insights or notifications.

Existing optional OCR, vision, in-app chat and generic record extraction may still incur backend provider costs. See [AI architecture](docs/ai-architecture.md).

## Appearance photos

Each pet has a private **Photos** tab for quick appearance tracking. On a phone, **Take photo** opens the camera and automatically uploads the selected shot. Files are stored beneath `PET_PHOTOS_DIR` in household- and pet-scoped directories and are only served back through authenticated, household-scoped routes.

When `OPENROUTER_API_KEY` or `LLM_API_KEY` is configured, the app asks the configured vision-capable model to locate both eyes in the background. The browser then uses those normalized eye coordinates to rotate, scale, and translate each image onto the same canvas, making it easier to step through changes without the face jumping around between frames. If the eyes cannot be found, the original framing is still shown.

For two-date comparisons, the owner can select any two photos and request a cautious AI visual comparison. The prompt explicitly separates visible changes from pose, distance, expression, fur, and lighting differences and does not ask the model to diagnose a condition or attribute a change to medication. Image inputs are sent as private base64 data rather than public photo URLs.

## Weights and blood tests

The console keeps a dated weight history for each pet. Blood-test files are stored under a private household directory beneath `BLOOD_TESTS_DIR` (local default: `./example_blood_tests`; production compose default: `/persistent/blood_tests`). The web console can upload a PDF or image directly, and Syncthing can place files in the matching household directory. The owner then chooses **Import new tests**, or calls the MCP `upload_blood_test` / `import_blood_tests` tools.

Imports use Mistral OCR 4 (`mistral-ocr-4-0`) with block and table extraction. The OCR text is stored alongside parsed test name, value, unit, reference range, flag, and test date. Spanish and English labels are accepted, and the original OCR text remains available for review when a row cannot be parsed. Set `MISTRAL_API_KEY` in the environment or local `.env`; never commit that file.

## MCP for ChatGPT, Claude and other clients

Pet Health exposes an authenticated MCP endpoint at `/mcp`. MCP clients can use OAuth with S256 PKCE, discovered from the standard `/.well-known/oauth-protected-resource` and `/.well-known/oauth-authorization-server` endpoints. Existing Pet Health session tokens also remain supported as revocable bearer tokens. Every tool call is scoped to that account's household.

The tools support record access and structured updates. Prefer specific MCP tools such as `record_weight`, `record_symptom_event`, `record_medication` and `record_prescription`. For neutral notes, `record_observation(pet_id, message)` preserves the owner's wording **without another backend LLM call**. The legacy `record_health_event(message)` may invoke the backend model and remains available for backwards compatibility. Write tools are explicit, preserve the user's original wording, and let the server choose timestamps.

For Claude Code, add the endpoint and let Claude complete the OAuth consent flow:

```bash
claude mcp add --transport http pethealth https://your-host/mcp
```

For Codex, add the same URL as a remote MCP server. The client can discover the OAuth endpoints and ask the user to approve access. Keep write actions enabled only for users who should be able to change the household record.

If the client is running over SSH without a display, use the device-code fallback: the client posts its PKCE challenge to `/oauth/device`, prints the returned `user_code`, and polls `/oauth/token` with the returned `device_code`. Open the returned `verification_uri` on any phone or computer, enter the code, sign in, and approve. The code expires after 10 minutes and can be used once.

## Run locally

```bash
cp .env.example .env
set -a
source .env
set +a
cargo run
```

Open `http://localhost:3000`. On a new database, `APP_USERNAME` and `APP_PASSWORD` create the initial owner account for the default household. They are bootstrap credentials only; after the account exists, password changes happen from **Account settings** and revoke every active session.

Additional owners can register with an email address. Each registration creates a separate household, and all pet, event, share, and audit queries remain household-scoped. Passwords use salted Argon2 hashes. Browser sessions are random, stored server-side as SHA-256 token hashes, expire after 30 days, and use `HttpOnly`, `SameSite=Lax`, and production-only `Secure` cookies.

## Deployment modes and billing

`DEPLOYMENT_MODE` defaults to `self_hosted` if unset, including when `PRODUCTION=true`. This differs from `PRODUCTION`, which only controls production storage and security checks.

| Setting | Purpose |
| --- | --- |
| `DEPLOYMENT_MODE=self_hosted` | Default. No subscription restrictions, Stripe initialization, hosted licensing service, or Stripe credentials required. All currently implemented local application features remain available. |
| `DEPLOYMENT_MODE=managed` | Operator-hosted SaaS deployment. Prepares the application configuration for future billing and server-side Free/Plus entitlements. |
| `STRIPE_SECRET_KEY`, `STRIPE_WEBHOOK_SECRET` | Optional in managed mode, but must be supplied together. Ignored in self-hosted mode. Never commit real values. |

Example self-hosted deployment:

```env
DEPLOYMENT_MODE=self_hosted
# Stripe variables are unnecessary.
```

Managed SaaS configuration, once you are ready to integrate payments:

```env
DEPLOYMENT_MODE=managed
STRIPE_SECRET_KEY=...
STRIPE_WEBHOOK_SECRET=...
```

**Important:** These are configuration switches, not a complete payment implementation. This revision does **not** create checkout sessions, process Stripe webhooks, persist subscription state, enforce Free/Plus limits, or add premium features. Managed mode without Stripe credentials currently runs existing features without a paywall. When those features are built, entitlements must be enforced server-side across both web and MCP; self-hosted deployments must remain independent of the operator's payment infrastructure.

## Licensing

Current revisions are offered under [Elastic License 2.0](LICENSE), which allows source access, modification and self-hosting but restricts providing substantial software functionality as a hosted or managed service to third parties. This is **source-available**, not OSI-approved open source. Prior revisions released under AGPL-3.0 remain available under their original terms; this change does not revoke those existing grants. Third-party components retain their own licenses.

## Production data

When `PRODUCTION=true`, the database location is not configurable: it resolves to:

```text
/persistent/pethealth.sqlite
```

The process refuses to start if `/persistent` is absent or the production password is unchanged. Mount durable local storage at `/persistent`; SQLite runs in WAL mode with foreign keys, a busy timeout, and `synchronous=FULL`.

The checked-in `compose.production.yml` is the Coolify production definition. It deliberately has no host port mapping, pulls the public `ghcr.io/dunctk/pethealth:mvp` image, and declares the durable volume explicitly:

```bash
docker build -t pethealth:mvp .
APP_USERNAME=owner APP_PASSWORD='replace-me' DEPLOYMENT_MODE=self_hosted docker compose -f compose.production.yml up -d
```

In Coolify, route the `pethealth` service to `https://your-host:3000`. Keep the bootstrap `APP_PASSWORD` in Coolify's protected environment settings rather than adding it to the compose file. Back up the named volume containing `/persistent/pethealth.sqlite` together with its WAL files.

## GitHub Actions image publishing

The `main` branch workflow runs the Rust checks and publishes `ghcr.io/dunctk/pethealth:mvp` to GitHub Container Registry using the built-in `GITHUB_TOKEN`. No repository secrets are needed.

After the first successful publish, open the package settings on GitHub and set the `pethealth` container package visibility to **Public**. Coolify pulls the image without credentials.

## Optional Rig agent

When configured, Rig is the first pass for every natural-language capture, so the record box can structure free-form observations instead of relying on fixed phrases. A small deterministic parser remains as a safe fallback for known phrases when the provider is unavailable or omitted:

```text
OPENROUTER_API_KEY=...
# Optional overrides:
# LLM_BASE_URL=https://openrouter.ai/api/v1
# LLM_MODEL=openai/gpt-5.6-sol
```

`OPENROUTER_API_KEY` is the preferred provider-specific name; `LLM_API_KEY` remains accepted for backwards compatibility. The base URL defaults to OpenRouter and the model defaults to `openai/gpt-5.6-sol`, so neither override is required. The production compose file passes both key names through to the container. The capture box also uses the selected pet when a note says “she”, “he”, or “they” without repeating the pet's name. A note saying no medication was given and appetite was reasonable is saved as a care update and marks the pet's active prescriptions as missed for that date.

The model proposes a typed event only. Rust resolves the pet, validates the proposal, chooses the timestamp, and performs the tenant-scoped transaction.

## Verification

```bash
cargo fmt --check
cargo check --all-targets
cargo test
docker build -t pethealth:mvp .
```
