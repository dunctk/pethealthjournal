# Roadmap

Pet Health Journal is a source-available, self-hostable pet health record with MCP-first conversational AI and a proposed optional proactive monitoring service.

## Future possibilities (not yet shipped)

- Better ChatGPT/Claude MCP onboarding and structured, model-free record writes
- Evidence-backed on-demand deterministic health trends with safe interpretations
- Optional proactive AI: opt-in and metered on paid hosted plans, or operator-funded BYOK for self-hosted users
- Opt-in scheduled medication and vaccination reminders
- Caregiver and pet-sitter collaboration with least-privilege roles
- Vet-ready summaries and richer longitudinal visualization
- A managed hosted service with optional subscriptions

Core history, safety, household scoping and record portability remain design priorities. No listed roadmap item is a commitment, and there is currently no Stripe checkout or paid-feature enforcement. The self-hosted application defaults to `DEPLOYMENT_MODE=self_hosted` and requires no paid subscription.

`record_observation` is now a no-LLM MCP path for ordinary owner notes. The legacy in-app AI remains. `PROACTIVE_AI_ENABLED` is a reserved configuration guard only; there is no proactive worker, notification system or paid entitlement enforcement yet.

See [README.md](README.md) and [docs/ai-architecture.md](docs/ai-architecture.md) for current behavior and future plans.
