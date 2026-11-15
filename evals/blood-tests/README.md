# Blood-test transcription evals

Measures how accurately an MCP client (ChatGPT, Claude) transcribes a photo of
a lab report into `record_blood_test`. The server cannot see the photo, so the
loop is: you run the prompt in the client, then this script compares what was
saved against a hand-checked answer key.

**Never commit real reports or answer keys.** `cases/` is gitignored except
`example.json`. Use anonymised reports from a test household.

## Add a case

1. Put the photo/PDF somewhere private (not in this repo).
2. Copy `cases/example.json` to `cases/<name>.json` and type the correct
   values from the paper: `test_date`, and every analyte's `test_name`,
   `value`, `unit`, `reference_range`, `flag` as printed.
3. Cover the hard cases: US vs EU date order, comma decimals, `<`/`>` values,
   multi-page reports, blurry or angled photos, Spanish/French labels.

## Run

1. In ChatGPT (or Claude), in a fresh chat with the connector enabled, attach
   the photo and say: `Save this blood test to Pet Health Journal for <pet>.`
   Approve the table it shows.
2. Score it:

```bash
PHJ_TOKEN=phj_... python3 evals/blood-tests/score.py --pet-id 3 evals/blood-tests/cases/<name>.json
```

Mint the token at `/account/api-tokens` for the test household. The script
reads with `get_blood_test_history` only; it never writes. Use
`--url http://localhost:3000/mcp` against a local server.

The report shows per-field accuracy, missing and extra analytes, and exits
non-zero if any value, unit or the date is wrong. Track the totals over time
when you change tool descriptions.
