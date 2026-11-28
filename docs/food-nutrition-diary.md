# Food & Nutrition Diary

The Food & Nutrition Diary is a longitudinal feeding, appetite, and nutritional record integrated directly into Pet Health Journal alongside weights, medications, symptoms, laboratory reports, clinical investigations, and expiring veterinary shares.

## Architecture & core principles

- **MCP-first, zero server-side LLM calls:** External assistants (ChatGPT, Claude, Codex, and other MCP clients) interpret natural-language owner updates and invoke typed MCP tools. Rust owns validation, deterministic arithmetic, household permissions, audit trails, and historical snapshots. Manual web entry remains first-class.
- **Reusable household food catalogue (`food_items`):** Store commercial wet/dry food, veterinary prescription diets, treats, supplements, plain human foods, and custom preparations once per household. Supports optional serving size/label, energy density, protein, fat, phosphorus, sodium, moisture, completeness (`complete`, `complementary`, `unknown`), confidence (`verified`, `estimated`, `unknown`), and `nutrition_source`.
- **Explicit nutritional bases — never silently mixed:** Supports `per_100g`, `per_100ml`, `per_serving`, and `dry_matter` (per 100 g dry matter). Mass and volume are never silently mixed, universal serving sizes (such as pouch weight) are never assumed, and `dry_matter` nutrition is never silently converted to as-fed grams when `moisture_pct` is absent.
- **Immutable historical snapshots (`food_diary_entries`):** Every feeding record stores a snapshot of the food's nutritional values and source metadata at the time of recording. Updating a catalogue food later never rewrites historical consumption records.
- **Missing data never blocks recording:** Foods with unknown nutrition or unknown consumed amounts can always be logged.
- **Six distinct intake states:** `consumed_all`, `consumed_partial`, `refused`, `consumed_unknown_amount`, `not_offered`, and unlogged days. Unknown intake is never 0 kcal; refused meals do not collapse a day's other meals to 0 kcal; unlogged days are never interpreted as fasting.
- **Coverage-aware deterministic calculations:** Daily summaries split `verified_kcal` vs. `estimated_kcal`, track per-nutrient item coverage, and distinguish `complete_for_logged_entries` from `partial_subtotal`, `refused_only`, `unknown`, and `no_entries`. Rolling 7-day and 30-day averages divide only over logged days with computable data.
- **Optional owner/vet targets with provenance (`pet_nutrition_targets`):** Pet Health Journal never prescribes calorie targets or nutrient limits automatically. Owners may record an explicit owner- or veterinarian-established target with required `source` and `source_note`.

## MCP tools

| Tool | Purpose |
| --- | --- |
| `list_foods` | Search/list reusable foods in the household catalogue. |
| `add_food` | Add a reusable food with optional sourced nutritional profile. |
| `update_food` | Update a catalogue item without altering historical diary snapshots. |
| `record_food_intake` | Atomically record one or multiple foods in a meal, with optional `idempotency_key` retry protection. |
| `list_food_intake` | List active feeding entries with date bounds, status/food filters, and pagination. |
| `update_food_intake` | Correct an active feeding entry and deterministically recompute its nutrition from its snapshot. |
| `delete_food_intake` | Soft-delete (undo) a mistaken feeding entry. |
| `get_food_diary_summary` | Concise daily or date-range totals, verified/estimated split, coverage flags, refusals, and target comparison. |
| `get_nutrition_trends` | 7-day, 30-day, and custom-window trends, food acceptance/refusal rates, and optional chronologically aligned weights, medications, symptoms, and labs. |
| `set_nutrition_target` | Save or clear an explicit owner- or vet-entered daily calorie/nutrient target with provenance. |

`add_clinical_case_update` also accepts an optional `linked_food_entry_id` to attach an active same-pet feeding record as evidence, and `get_vet_case_brief` includes a 30-day nutrition summary.

## Web interface & veterinary sharing

- **Per-pet Food tab (`/app?pet={id}&view=food`):** Mobile-friendly HTMX interface with selected-day intake summary, coverage banner, historical date navigation, one-tap frequent-food logging (`Ate 1`, `Ate ½`, `Refused`), manual meal/refusal capture, inline correction, undo with one-tap restore, 7/30-day trends, a server-rendered SVG Weight vs. Recorded Daily Calorie Intake chart, owner/vet target management, and household catalogue management.
- **Unified timeline & `+ Record` menu:** Active feeding entries appear chronologically on the pet timeline alongside events, weights, medication doses, and labs; `+ Record → Food / meal` provides dialog capture from any tab.
- **Read-only vet share (`/share/{token}#nutrition`):** Authorized, expiring, revocable vet links display the 30-day nutrition and appetite summary, Weight vs. Recorded Calories chart, food acceptance table, and chronological feeding entries without exposing the household food catalogue or private documents.

## Remaining limitations

- No automatic calorie target calculation or medical nutrient prescription.
- No barcode lookup or paid third-party nutrition API integration.
- Dedicated water-intake tracking and automated nutrition-label photo OCR remain future additions (assistants can read packaging photos client-side and call `add_food` with `nutrition_source`).
- Chronological alignment with weights, medications, symptoms, and labs shows temporal proximity only and never establishes causation or diagnosis.
