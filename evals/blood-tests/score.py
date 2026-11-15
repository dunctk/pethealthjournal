#!/usr/bin/env python3
"""Score a saved blood-test transcription against a hand-checked answer key.

Read-only: calls the MCP tool get_blood_test_history. Stdlib only.
"""
import argparse
import json
import os
import sys
import urllib.request


def norm(text):
    if text is None:
        return ""
    return " ".join(str(text).replace("µ", "u").replace("μ", "u").lower().split())


def norm_value(text):
    value = norm(text).replace(" ", "")
    # Treat "1,8" and "1.8" as the same printed number.
    if value.count(",") == 1 and "." not in value:
        value = value.replace(",", ".")
    return value


def fetch_reports(url, token, pet_id):
    body = json.dumps({
        "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": {"name": "get_blood_test_history", "arguments": {"pet_id": pet_id, "limit": 50}},
    }).encode()
    request = urllib.request.Request(url, data=body, headers={
        "Authorization": f"Bearer {token}", "Content-Type": "application/json",
    })
    with urllib.request.urlopen(request, timeout=30) as response:
        payload = json.load(response)
    if "error" in payload:
        sys.exit(f"MCP error: {payload['error']}")
    return payload["result"]["structuredContent"].get("items", [])


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("case", help="answer key JSON (see cases/example.json)")
    parser.add_argument("--pet-id", type=int, required=True)
    parser.add_argument("--report-id", type=int, help="score this report instead of matching by date")
    parser.add_argument("--url", default="https://app.pethealthjournal.com/mcp")
    args = parser.parse_args()
    token = os.environ.get("PHJ_TOKEN") or sys.exit("Set PHJ_TOKEN to a phj_ API token.")

    with open(args.case, encoding="utf-8") as handle:
        expected = json.load(handle)
    reports = fetch_reports(args.url, token, args.pet_id)
    if args.report_id:
        candidates = [r for r in reports if r["id"] == args.report_id]
    else:
        candidates = [r for r in reports if r.get("test_date") == expected["test_date"]] or reports[:1]
    if not candidates:
        sys.exit("No saved report found for this pet.")
    report = candidates[0]

    date_ok = report.get("test_date") == expected["test_date"]
    saved = {norm(r["test_name"]): r for r in report.get("results", [])}
    fields = ["value", "unit", "reference_range", "flag"]
    totals = {field: [0, 0] for field in fields}
    problems = []
    for item in expected["results"]:
        key = norm(item["test_name"])
        got = saved.pop(key, None)
        if got is None:
            problems.append(f"MISSING  {item['test_name']}")
            for field in fields:
                if item.get(field) is not None:
                    totals[field][1] += 1
            continue
        for field in fields:
            want = item.get(field)
            have = got.get("value_text" if field == "value" else field)
            if want is None and have is None:
                continue
            totals[field][1] += 1
            same = norm_value(want) == norm_value(have) if field == "value" else norm(want) == norm(have)
            if same:
                totals[field][0] += 1
            else:
                problems.append(f"WRONG    {item['test_name']}.{field}: expected {want!r}, saved {have!r}")
    for extra in saved.values():
        problems.append(f"EXTRA    {extra['test_name']} = {extra.get('value_text')!r}")

    print(f"Report #{report['id']} ({report.get('source_filename')})")
    print(f"test_date: {'ok' if date_ok else 'WRONG'} (expected {expected['test_date']}, saved {report.get('test_date')})")
    for field, (good, total) in totals.items():
        if total:
            print(f"{field:>16}: {good}/{total} ({good / total:.0%})")
    for line in problems:
        print(line)
    critical = not date_ok or any(
        line.startswith(("MISSING", "EXTRA")) or ".value:" in line or ".unit:" in line for line in problems
    )
    sys.exit(1 if critical else 0)


if __name__ == "__main__":
    main()
