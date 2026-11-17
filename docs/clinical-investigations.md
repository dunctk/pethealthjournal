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

The printable case brief intentionally labels recent general health events separately from case-specific entries; they may be unrelated to the presenting problem.


## Interactive veterinary share viewer (read-only)

The existing short-lived /share/{token} link now presents:

- Clinical investigations and their hypothesis status (labelled as owner-reported, not veterinary-authenticated), with navigation to individual case updates.
- A unified reverse-chronological timeline of health events, structured symptom detail, doses administered, weight measurements, laboratory reports and investigation notes. Each event can be expanded via a keyboard-operable native HTML details/summary control. The page includes up to the newest 400 ordinary record entries plus case updates; case updates with unknown observation dates are labelled with their recorded date rather than presented as a confirmed onset.
- Laboratory charts grouped using the existing analyte name/unit normalizer. Known values from different labs are converted at read time into canonical units without modifying the printed values. Unknown/incompatible units remain separate. Missing dates and non-numeric results are listed in accessible tables without false chart points. Labs retain their original per-report reference ranges and flags: the charts do not claim a single universal normal range.
- The current and historical prescription plan as a distinct section, never conflated with individual administrations.

**Privacy boundary:** The existing share-token resolver runs on every request, so revoked and expired links deny all sections. The route is read-only and uses no new public data endpoints, external charting scripts, image uploads, or model calls. The page doesn't expose private passport images, microchip IDs, account details, or original lab-file download URLs. Existing no-store, no-referrer and strict CSP headers apply.

Render validation: compare at desktop and mobile widths, expand each clinical row and marker, inspect graph point details and tables, and confirm revoked links return not found. The server-side charts are informational only; the veterinarian should confirm clinical interpretation from the source report.

Chart points have tooltips for pointer users, and every underlying measurement is available in the table for keyboard and assistive-technology users. Lab-report links target the expandable timeline report details.
