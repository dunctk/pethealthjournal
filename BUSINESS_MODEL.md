# Pet Health Journal: freemium and monetization strategy

_Status: proposed strategy, not implemented. Updated: 2026-11-04._

## Thesis

**Make the ChatGPT/MCP experience genuinely useful for free. Charge for reliable, ongoing pet-care coordination rather than for AI-generated answers.**

ChatGPT supplies conversational reasoning and interpretation. Pet Health Journal supplies persistent, household-scoped structured data, accessible history, reminders, permissions, reporting and workflows that continue working when ChatGPT is closed.

The long-term differentiation is the **longitudinal, trustworthy pet record** plus the services and people coordinated around it, not proprietary AI inference. The free ChatGPT plugin is an acquisition and activation channel, not a trial that becomes unusable.

Naming: **Pet Health Journal** is the working product brand; `pethealthjournal.com` is a candidate domain, not confirmed purchased.

## Proposed tiers (validate before shipping)

| Capability | Free | Plus |
| --- | --- | --- |
| Price | $0 | **Test** $4.99/month or $39/year |
| Pets | 2 | Unlimited (subject to abuse controls) |
| Structured observations and pet timelines | Yes | Yes |
| ChatGPT/MCP read and write access to own records | Yes | Yes |
| Weight, medication and basic health-history logging | Yes | Yes |
| Basic medical-record access and portable export | Yes | Yes |
| Scheduled medication and refill reminders | No | Yes |
| Vaccination/appointment reminders and calendar sync | No | Yes |
| Household collaboration and pet-sitter access | No | Yes |
| Dose confirmations and care handovers | No | Yes |
| Vet-ready formatted PDF reports | No | Yes |
| Rich visual trends and longer-running comparisons | Basic | Advanced |
| Private photo/document storage | Small allowance | Larger allowance |
| Enhanced veterinarian sharing workflows | Basic, safe expiring links | Additional convenience and coordination |

These are **product hypotheses**, not live entitlement rules. Confirm storage allowances, delivery costs, subscription billing, geographic currency and tax treatment before launch. Any Plus entitlement must be enforced server-side consistently across web UI and MCP, without disrupting basic record access.

### What should remain free

- Viewing and logging one's own pet health history; a useful free-tier ChatGPT integration.
- Basic pet/medication/weight tracking and ordinary record export in a usable, portable format.
- Access to and export of already-stored records if the user downgrades or cancels.
- Essential account, privacy and safety controls. Never hold existing pet health records hostage behind a subscription.
- Existing basic revocable, expiring vet-share links should not be removed or made unsafe for monetization.

If a free user exceeds a pet or storage allowance, preserve read and export access; only restrict creating additional pets or storing more uploads where necessary. Define a clear grace period and never silently delete data.

## Why users pay

### 1. Reminders that function without an active ChatGPT session
Medication times, dose confirmations, low-stock/refill alerts, vaccination due dates and appointment prompts. This requires a reliable scheduler, opt-in delivery preferences, timezones, retries, and clear failure handling. Notifications should not claim a dose was given merely because a reminder was sent. Make alert delivery and limitations explicit; it is not an emergency monitoring system.

### 2. Shared care coordination
Invite a spouse, family member, or sitter into a household with limited roles. Let authorized people confirm medication administration, see the relevant handover instructions, and track who did what and when. Avoid exposing unrelated records to sitters. Add audit history and easy access revocation.

### 3. Vet-ready summaries
Printable/downloadable reports with dated symptoms, medication changes, weight charts, relevant lab results, and user-selected photos. Provide read-only, expiring and revocable sharing links. Reports should identify their source data, preserve chronological accuracy, and mark uncertainty or missing observations rather than asserting diagnosis. Export and basic history remain available free.

### 4. Multi-pet households and richer storage
Unlimited pet profiles and higher photo/document limits are natural upgrades for households with several animals or complex chronic-care records. Storage costs should be measured, not hidden behind unlimited marketing claims.

## Recommended first paid release

Prioritize **reliable medication reminders** and **household/sitter collaboration**. They solve recurring jobs outside the chat interface and create an ongoing reason to subscribe. Follow with vet-ready PDFs, vaccination/refill workflows, richer charts and larger storage allowances.

The initial free experience must be immediately valuable: create a pet, record an observation via ChatGPT, retrieve history, and see it in the web timeline. Avoid paywalling that 'aha' moment.

## Growth funnel

1. **Discovery:** users find Pet Health Journal via the ChatGPT plugin and search-led pages about pet records, pet medication logs, pet vaccination records, cat medical records, etc. ChatGPT plugin listing/discovery performance is an unproven channel assumption to measure.
2. **Activation:** user connects OAuth, adds a pet, saves a genuine health event and retrieves its history through ChatGPT.
3. **Retention:** users record repeat events and see a useful longitudinal history.
4. **Upgrade trigger:** they need reliable scheduled reminders, multiple caregivers, a vet-ready report, additional pets, or greater storage.
5. **Conversion:** contextual prompts in web UI and appropriate MCP responses explain which concrete workflow Plus enables. Do not obstruct ordinary chat or solicit unnecessary payment information through tools.
6. **Referral:** optionally share a vet report or invite a caregiver. Sharing must be consensual and least-privilege, not a viral leak of medical history.

**Example free request:** 'How has my cat's weight changed over the last three months?' ChatGPT can reason over the available dated weights.

**Example Plus request:** 'Remind us twice daily, let both caregivers confirm each dose, alert us when medication is running low, and generate a vet report each month.' The paid value is dependable workflow infrastructure, not the language model's answer.

## Product architecture / guardrails

- Keep OAuth, MCP tools and all data household-scoped; never assume the ChatGPT client is an entitlement boundary.
- The existing Rust application is already built around multi-pet histories, reversible structured observations, private photos, blood-test imports, and expiring/revocable read-only vet links. Extend those rather than replacing the working free path.
- Model suggestions are non-authoritative context, not diagnoses. Never allow an LLM to set authoritative timestamps, prescribe treatment, or confirm doses on behalf of people without explicit verified events.
- For reminders: define per-pet prescriptions, timezone-aware schedules, delivery method, acknowledgement, missed-dose states, opt-in consent, and observability. Avoid dangerous interpretations of 'missed'; a person may have administered a medication without recording it.
- For collaboration: distinct users, invitation expiry, least-privilege sitter roles, per-pet permissions if needed, audit logs and revocation.
- For reporting: attribute claims to recorded events, preserve original wording, show dates and relevant measurement units, protect generated links.
- Separate **authentication**, **household permissions**, **subscription entitlements** and **MCP authorization**. Billing state changes must not compromise data access/export.
- Protect sensitive owner contact details and pet records, use access controls, durable backups, retention/deletion policies and secure delivery of reminders.
- Measure provider/API costs carefully for optional OCR, vision, storage, delivery and PDF generation. Do not silently bundle uncapped costly inference into free access; ChatGPT model intelligence may be supplied by the client, but backend features still have costs.
- Verify platform-specific ChatGPT plugin billing/link rules before implementing upgrade flows; do not assume in-chat purchases are supported.

## Validation plan

Before committing to the proposed prices or limits:

1. Interview owners of pets on daily medications and multi-pet households about reminders, sitter coordination and vet visits.
2. Run a simple landing-page pricing test: free vs $4.99/month or $39/year Plus, and ask which single feature drives purchase.
3. Instrument **install/discovery → OAuth connection → first pet → first recorded event → first history retrieval → day-7/day-30 repeat use → paywall view → upgrade**.
4. Track reminders configured, reminder delivery reliability, missed/confirmed doses, caregiver invitations accepted, report downloads and cancellations.
5. Monitor variable costs per free and paying household, support volume, abuse, data export requests and churn.
6. Consider a veterinary practice portal only after interviews establish how clinics actually receive and review client-supplied history; defer B2B monetization from the first launch.

## Suggested implementation order

- [ ] Confirm branding and registrar availability independently; do not assume the domain is purchased.
- [ ] Keep the existing free pet-history and OAuth/MCP experience frictionless.
- [ ] Instrument core activation and retained use, with privacy-conscious event properties.
- [ ] Add server-side subscriptions and feature entitlements with cancel/downgrade safety.
- [ ] Build opt-in, reliable medication reminder scheduling and delivery.
- [ ] Build caregiver invitations, roles, per-pet access (where needed) and dose acknowledgements.
- [ ] Add upgrades and plan management in web UI; expose clear, non-blocking Plus explanations in MCP.
- [ ] Add rich vet-ready PDF exports and additional storage tiers.
- [ ] Test prices and limits against real user conversion, retention and costs.

## SEO context (US English, Google Ads Keyword Planner estimates)

These are search-demand signals for content planning, **not** organic ranking difficulty or traffic forecasts:

| Query | Approx. monthly US searches |
| --- | ---: |
| pet medical records | 70 |
| pet medical records app | 30 |
| cat medical records | 20 |
| dog medical records | 70 |
| pet health records | 90 |
| pet vaccination records | 480 |

A content strategy can use individual information and product pages for these needs. Don't interpret high keyword search demand as proof that users will pay for a SaaS product; many vaccination-record queries seek a printable document or an existing provider's records.

## Explicit non-goals at launch

- Selling generic AI chat completions as the premium feature.
- Paywalling a pet's existing history or basic export.
- Clinical diagnosis, medication prescribing, or emergency alert guarantees.
- Building a full veterinarian SaaS/portal before consumer demand is validated.
