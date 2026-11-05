# MCP-first AI and proactive analysis

_Status: architectural direction, 2026-11-05. Background monitoring, paid entitlements, insights and notifications are not implemented._

## Product boundary

ChatGPT, Claude, and other MCP clients provide the primary conversational AI: interpreting ordinary language, asking follow-ups, explaining health records and calling tools. Pet Health Journal is the authoritative record layer: ownership and household permissions, original wording, structured data, safe timestamps, timeline, audits and export. Manual web forms remain available.

**Avoid double inference:** `record_weight`, `record_symptom_event`, `record_medication`, `record_prescription` and other structured MCP tools should be used whenever appropriate. `record_observation(pet_id, message)` stores a neutral owner note without the backend LLM. The legacy `record_health_event(message)` may invoke a second server-side model and remains only for compatibility until existing workflows can be migrated safely.

Specialised vision alignment/photo comparison and OCR are optional integrations with separate provider costs and data handling.

## Target cost and entitlement matrix (not shipped)

| Capability | Hosted Free | Hosted Plus | Self-hosted |
| --- | --- | --- | --- |
| Records, manual entry, MCP read/write and export | Yes | Yes | Yes |
| ChatGPT/Claude conversational reasoning | Provided by the client | Provided by the client | Provided by the client |
| On-demand deterministic trend calculations | May be included with limits | Included | Local |
| Background LLM pattern recognition | **No** | Opt-in, per-household paid entitlement and spend cap | Optional BYOK and own expenses |
| Scheduled notifications | No unlimited free promise | Opt-in, rate-limited | Operator-managed |

The managed/free plan must not silently run a backend LLM upon record changes or routine scans. Existing in-app chat/capture and OCR/vision integrations still have potential provider costs; this policy is a future engineering direction, not a claim that all current backend inference is removed.

## Proactive monitoring pipeline to build

1. Trigger from new/changed records or scheduled opt-in batches, scoped to one household and pet.
2. Compute objective, deterministic changes: weight deltas, symptom-event frequency, medication-adherence gaps and lab-value changes. Document minimum sample sizes, missing values and comparison windows. Validate clinically relevant rules with qualified veterinary input before deployment.
3. Persist evidence-backed findings with source IDs, versioned rule, calculation, time window, uncertainty, review/dismissal state, and a stable deduplication key.
4. **Before model invocation**, enforce explicit consent and cost eligibility: self-hosted with operator-supplied provider key, or managed with a verified Plus **household**, remaining budget and rate-limit allowance. `DEPLOYMENT_MODE=managed` alone is not enough.
5. Optionally generate cautious natural-language summaries (no diagnosis or medication instructions), then notify with opt-in delivery and reliable retries.
6. Expose persisted findings to the web and MCP; don't advertise `get_health_insights` until it actually returns real findings. Provide opt-out and deletion controls.

## Current config: guard only

`PROACTIVE_AI_ENABLED` defaults to `false`. It is a **reserved startup guard only**, not an operational monitoring worker. Setting it true today does not generate alerts or summaries.

- Self-hosted: a requested true setting requires the operator's `OPENROUTER_API_KEY` or `LLM_API_KEY`. The operator pays any provider costs, but no worker exists yet.
- Managed: requested true is rejected even with API credentials until scheduled processing, per-household paid entitlements and budgets have been implemented.

## Engineering sequence

1. MCP-first docs and neutral, no-LLM note tool.
2. Improve MCP onboarding, structured writes and optional provider cost measurement.
3. Build deterministic insights with source-provenance tests.
4. Add consent, scheduling, per-household Plus gates, BYOK, budgets and deduplication.
5. Enable bounded background AI and notification delivery with monitoring; de-emphasize duplicate in-app chat without breaking record access.
