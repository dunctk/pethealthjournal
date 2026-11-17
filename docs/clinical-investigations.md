# Clinical investigations — owner-led case files

Clinical investigations are problem-oriented records alongside the existing chronological pet record. They are useful when symptoms persist, treatment does not fully resolve the problem, or an owner wants an evidence-led discussion with a veterinarian.

## Model and provenance

- A pet can have multiple cases, each with a presenting concern and state (open / monitoring / resolved).
- Updates are chronological observations, treatment courses, responses, owner-relayed vet findings, tests or questions. Original words are preserved. Observed date is optional and must never be invented; recorded time is server-owned.
- An update may reference a same-pet active health event. It never changes or double-counts that event.
- Possible explanations are hypotheses, always initially unreviewed. Vet-supported or vet-ruled-out labels can only be recorded when the owner explicitly relays a veterinary assessment; this is not independent vet authentication.
- Never infer a diagnosis, causal medication reaction, or recommend medication changes. An assistant can suggest questions with clear uncertainty and sources.
- Household scoping, audited changes and cascade deletion of case data are mandatory.
- No new background inference or monitoring.

## MCP workflow

1. list_pets → list_clinical_cases → open_clinical_case if needed.
2. record_symptom_event includes respiratory categories: wheezing, nasal congestion/discharge, eye discharge, coughing and labored breathing.
3. add_clinical_case_update records exact observations, treatment attempts and response, vet discussion, tests and questions; link existing health event when appropriate.
4. add_clinical_hypothesis stores an unreviewed differential question, not a diagnosis.
5. review_clinical_hypothesis requires explicit user approval and a source attribution. A vet status requires the owner to relay that assessment.
6. get_vet_case_brief includes case-specific updates, hypotheses, and separately labelled prescription, lab and recent-event context. The web UI prints this brief. Existing expiring, revocable read-only vet shares display cases.

Do not invent onset dates, antibiotics or dosage history, veterinary conclusions, or causality. Mark unknown values unknown.

## Future additions

- Structured treatment trials with start/end, dose reference, adherence and standardized response, connected to actual prescription records.
- Case-specific timeline selection and evidence-backed symptom trends with uncertainty.
- Attachments and veterinary-verified author identities, where genuinely verified.
- Native PDF export rather than browser print.

## Verification

Run cargo fmt --check, cargo check --all-targets, cargo test, and review owner form and vet share at mobile/desktop sizes. MCP e2e exercises isolation and confirmation paths.
