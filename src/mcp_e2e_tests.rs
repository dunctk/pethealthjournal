//! End-to-end MCP test: drives the real router over HTTP-shaped requests
//! with personal API tokens, exercising every listed tool (except those that
//! need external providers) and asserting protocol invariants that ChatGPT
//! enforces, such as `structuredContent` always being a JSON object.

use crate::{AppState, agent, db, web};
use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode, header},
};
use sea_orm::ConnectionTrait;
use serde_json::{Value, json};
use std::{collections::BTreeSet, sync::Arc};
use tower::ServiceExt;

/// Tools that call paid external providers (LLM, Mistral OCR) and are not
/// exercised here. Every other listed tool must be called by this test.
const NEEDS_EXTERNAL_PROVIDER: &[&str] = &[
    "record_health_event",
    "upload_blood_test",
    "import_blood_tests",
];

struct Harness {
    app: Router,
    state: AppState,
    called: BTreeSet<String>,
    next_id: i64,
}

impl Harness {
    async fn new(root: &std::path::Path) -> Self {
        let config = crate::ocr::tests::test_config(root);
        let database = db::connect("sqlite::memory:").await.unwrap();
        db::migrate(&database).await.unwrap();
        let state = AppState {
            db: database,
            agent: Arc::new(agent::CaptureAgent::new(&config)),
            chat: Arc::new(agent::ChatAgent::new(&config)),
            mailer: Arc::new(crate::email::Mailer::Disabled),
            files: Arc::new(crate::storage::FileStore::from_config(&config).unwrap()),
            config: Arc::new(config),
        };
        Self {
            app: web::router(state.clone()),
            state,
            called: BTreeSet::new(),
            next_id: 1,
        }
    }

    async fn token(&self, email: &str) -> String {
        let user = db::create_account(&self.state.db, email, "Tester", "unused-hash")
            .await
            .unwrap();
        db::create_api_token(&self.state.db, user.id, user.household_id, "e2e")
            .await
            .unwrap()
            .1
    }

    async fn get(&self, path: &str) -> (StatusCode, String) {
        let response = self
            .app
            .clone()
            .oneshot(Request::get(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let body = to_bytes(response.into_body(), 5_000_000).await.unwrap();
        (status, String::from_utf8_lossy(&body).into_owned())
    }

    async fn rpc(&mut self, token: &str, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let body = json!({"jsonrpc":"2.0","id":self.next_id,"method":method,"params":params});
        let response = self
            .app
            .clone()
            .oneshot(
                Request::post("/mcp")
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::HOST, "app.test")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        // Unauthenticated calls are 401 by design; everything else is 200.
        let expected = if token.starts_with("phj_not") {
            StatusCode::UNAUTHORIZED
        } else {
            StatusCode::OK
        };
        assert_eq!(response.status(), expected, "{method} HTTP status");
        let bytes = to_bytes(response.into_body(), 5_000_000).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    /// Call a tool and require success; returns `structuredContent`.
    async fn ok(&mut self, token: &str, tool: &str, args: Value) -> Value {
        let response = self.call(token, tool, args).await;
        let result = response
            .get("result")
            .unwrap_or_else(|| panic!("{tool} failed: {response}"));
        let structured = &result["structuredContent"];
        assert!(
            structured.is_object(),
            "{tool}: structuredContent must be an object, got {structured}"
        );
        assert_eq!(result["isError"], false);
        let text = result["content"][0]["text"].as_str().unwrap();
        assert_eq!(&serde_json::from_str::<Value>(text).unwrap(), structured);
        structured.clone()
    }

    /// Call a tool and require a JSON-RPC error; returns its message.
    async fn err(&mut self, token: &str, tool: &str, args: Value) -> String {
        let response = self.call(token, tool, args).await;
        response["error"]["message"]
            .as_str()
            .unwrap_or_else(|| panic!("{tool} should have failed: {response}"))
            .to_owned()
    }

    async fn call(&mut self, token: &str, tool: &str, args: Value) -> Value {
        self.called.insert(tool.to_owned());
        self.rpc(
            token,
            "tools/call",
            json!({"name": tool, "arguments": args}),
        )
        .await
    }
}

#[tokio::test]
async fn every_mcp_tool_works_end_to_end_with_household_isolation() {
    let root = tempfile::tempdir().unwrap();
    let mut h = Harness::new(root.path()).await;
    let owner = h.token("owner@example.test").await;
    let stranger = h.token("stranger@example.test").await;

    // Protocol basics.
    let init = h
        .rpc(
            &owner,
            "initialize",
            json!({"protocolVersion":"2025-06-18"}),
        )
        .await;
    assert_eq!(init["result"]["protocolVersion"], "2025-06-18");
    let listed = h.rpc(&owner, "tools/list", json!({})).await;
    let tools: BTreeSet<String> = listed["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| {
            assert!(tool["inputSchema"].is_object(), "{tool}");
            tool["name"].as_str().unwrap().to_owned()
        })
        .collect();
    let bad = h.rpc("phj_not_a_token", "tools/list", json!({})).await;
    assert_eq!(bad["error"]["code"], -32001);

    // Pets.
    let pet = h
        .ok(
            &owner,
            "add_pet",
            json!({"name":"Testy","species":"Cat","weight_kg":4.1}),
        )
        .await;
    let pet_id = pet["pet_id"].as_i64().unwrap();
    let pets = h.ok(&owner, "list_pets", json!({})).await;
    assert_eq!(pets["items"][0]["name"], "Testy");
    let updated = h
        .ok(
            &owner,
            "update_pet",
            json!({"pet_id":pet_id,"breed":"Tabby","date_of_birth":"2019-04-01","microchip_number":"009123456789012"}),
        )
        .await;
    assert_eq!(updated["pet"]["breed"], "Tabby");
    assert_eq!(updated["pet"]["microchip_number"], "009123456789012");
    let pets = h.ok(&owner, "list_pets", json!({})).await;
    assert_eq!(pets["items"][0]["microchip_number"], "009123456789012");
    let invalid = h
        .err(
            &owner,
            "update_pet",
            json!({"pet_id":pet_id,"microchip_number":123456789012345_i64}),
        )
        .await;
    assert!(invalid.contains("must be a string"));
    let invalid = h
        .err(
            &owner,
            "update_pet",
            json!({"pet_id":pet_id,"microchip_number":"123XYZ"}),
        )
        .await;
    assert!(invalid.contains("9 to 20 digits"));
    let second_id = h
        .ok(&owner, "add_pet", json!({"name":"Other","species":"Cat"}))
        .await["pet_id"]
        .as_i64()
        .unwrap();
    let duplicate = h
        .err(
            &owner,
            "update_pet",
            json!({"pet_id":second_id,"microchip_number":"009123456789012"}),
        )
        .await;
    assert!(duplicate.contains("microchip number"));
    h.ok(
        &owner,
        "update_pet",
        json!({"pet_id":pet_id,"microchip_number":""}),
    )
    .await;
    let updated = h
        .ok(
            &owner,
            "update_pet",
            json!({"pet_id":pet_id,"microchip_number":"009123456789012"}),
        )
        .await;
    assert_eq!(updated["pet"]["microchip_number"], "009123456789012");
    h.err(&owner, "update_pet", json!({"pet_id":pet_id})).await;

    // Private passport scans: storage and tenant isolation.
    use base64::Engine as _;
    let encoded = base64::engine::general_purpose::STANDARD.encode(b"%PDF-1.4\n%%EOF");
    let saved = h
        .ok(
            &owner,
            "add_pet_passport",
            json!({"pet_id":pet_id,"filename":"Identity.pdf","label":"Identity","content_base64":encoded}),
        )
        .await;
    let passport_id = saved["passport_id"].as_i64().unwrap();
    let docs = h
        .ok(&owner, "list_pet_passports", json!({"pet_id":pet_id}))
        .await;
    assert_eq!(docs["items"][0]["passport_id"], passport_id);
    assert_eq!(docs["items"][0]["label"], "Identity");
    assert_eq!(docs["items"][0]["authentication_required"], true);
    assert!(
        h.err(&stranger, "list_pet_passports", json!({"pet_id":pet_id}))
            .await
            .contains("not in this household")
    );
    assert!(
        h.err(
            &stranger,
            "add_pet_passport",
            json!({"pet_id":pet_id,"content_base64":encoded})
        )
        .await
        .contains("not in this household")
    );
    assert!(h.err(&owner, "add_pet_passport", json!({"pet_id":pet_id,"content_base64":base64::engine::general_purpose::STANDARD.encode(b"bad document")})).await.contains("PDF, JPEG, PNG or WebP"));
    assert!(
        h.err(
            &owner,
            "delete_pet_passport",
            json!({"passport_id":passport_id,"confirm":false})
        )
        .await
        .contains("confirm")
    );
    assert!(
        h.err(
            &stranger,
            "delete_pet_passport",
            json!({"passport_id":passport_id,"confirm":true})
        )
        .await
        .contains("not in this household")
    );
    let removed = h
        .ok(
            &owner,
            "delete_pet_passport",
            json!({"passport_id":passport_id,"confirm":true}),
        )
        .await;
    assert_eq!(removed["deleted"], true);
    assert_eq!(
        h.ok(&owner, "list_pet_passports", json!({"pet_id":pet_id}))
            .await["items"]
            .as_array()
            .unwrap()
            .len(),
        0
    );
    h.ok(
        &owner,
        "add_pet_passport",
        json!({"pet_id":pet_id,"filename":"Rabies.pdf","content_base64":encoded}),
    )
    .await;

    // Isolation: another household sees nothing and cannot act.
    let theirs = h.ok(&stranger, "list_pets", json!({})).await;
    assert_eq!(theirs["items"].as_array().unwrap().len(), 0);
    h.err(&stranger, "get_pet_timeline", json!({"pet_id":pet_id}))
        .await;
    h.err(
        &stranger,
        "record_weight",
        json!({"pet_id":pet_id,"weight_kg":1.0,"measured_at":"2026-01-01"}),
    )
    .await;

    // Weights.
    let weight = h
        .ok(
            &owner,
            "record_weight",
            json!({"pet_id":pet_id,"weight_kg":4.3,"measured_at":"2026-11-01"}),
        )
        .await;
    let weights = h
        .ok(&owner, "get_weight_history", json!({"pet_id":pet_id}))
        .await;
    assert!(weights["items"].as_array().unwrap().len() >= 2);
    h.err(
        &stranger,
        "delete_weight",
        json!({"weight_id":weight["weight_id"]}),
    )
    .await;
    h.ok(
        &owner,
        "delete_weight",
        json!({"weight_id":weight["weight_id"]}),
    )
    .await;

    // Timeline events.
    h.ok(
        &owner,
        "record_observation",
        json!({"pet_id":pet_id,"message":"Testy seems a bit quiet today"}),
    )
    .await;
    let symptom = h.ok(&owner, "record_symptom_event", json!({"pet_id":pet_id,"message":"Testy vomited once after breakfast","symptom":"vomiting","minutes_ago":30})).await;
    let timeline = h
        .ok(&owner, "get_pet_timeline", json!({"pet_id":pet_id}))
        .await;
    assert!(timeline["items"].as_array().unwrap().len() >= 2);
    h.ok(
        &owner,
        "undo_health_event",
        json!({"event_id":symptom["event_id"]}),
    )
    .await;
    h.ok(&owner, "get_health_context", json!({"concept":"vomiting"}))
        .await;

    // Medication lifecycle.
    let rx = h.ok(&owner, "record_prescription", json!({"pet_id":pet_id,"name":"Semintra","concentration_value":4.0,"concentration_unit":"mg/ml","dose_value":0.35,"dose_unit":"ml","frequency":"once daily","route":"oral","message":"Testy is on Semintra 0.35 ml daily"})).await;
    let rx_id = rx["prescription_id"].as_i64().unwrap();
    let dose = h.ok(&owner, "record_medication", json!({"pet_id":pet_id,"name":"Cerenia","dose_value":1.0,"dose_unit":"tablet","status":"given","message":"Gave Cerenia"})).await;
    let adherence = h.ok(&owner, "record_medication_adherence", json!({"pet_id":pet_id,"prescription_id":rx_id,"status":"given","message":"Gave Semintra"})).await;
    h.ok(
        &owner,
        "undo_medication_entry",
        json!({"entry_type":"dose","id":dose["medication_id"]}),
    )
    .await;
    h.ok(
        &owner,
        "undo_medication_entry",
        json!({"entry_type":"adherence","id":adherence["adherence_id"]}),
    )
    .await;
    let unconfirmed = h.err(&owner, "update_prescription", json!({"pet_id":pet_id,"prescription_id":rx_id,"action":"change_dose","dose_value":0.5,"message":"Vet upped it","confirm":false})).await;
    assert!(unconfirmed.contains("confirm"));
    let change = h.ok(&owner, "update_prescription", json!({"pet_id":pet_id,"prescription_id":rx_id,"action":"change_dose","dose_value":0.5,"message":"Vet upped it to 0.5 ml","confirm":true})).await;
    assert_eq!(change["new_status"], "active");
    let plan = h
        .ok(&owner, "get_medication_plan", json!({"pet_id":pet_id}))
        .await;
    assert!(plan.to_string().contains("0.5"));
    h.ok(&owner, "get_clinical_timeline", json!({"pet_id":pet_id}))
        .await;

    // Blood tests from two labs in different units; insights must line them up.
    let unconfirmed = h.err(&owner, "record_blood_test", json!({"pet_id":pet_id,"test_date":"2026-09-01","results":[{"test_name":"CREA","value":"2.0","unit":"mg/dL"}],"confirm":false})).await;
    assert!(unconfirmed.contains("confirm"));
    let first = h.ok(&owner, "record_blood_test", json!({"pet_id":pet_id,"test_date":"2026-09-01","lab_name":"US Lab","results":[{"test_name":"CREA","value":"2.0","unit":"mg/dL","reference_range":"0.8 - 2.4"}],"confirm":true})).await;
    assert_eq!(first["original_report"]["attached"], false);
    h.ok(&owner, "record_blood_test", json!({"pet_id":pet_id,"test_date":"2026-10-01","lab_name":"EU Lab","results":[{"test_name":"Creatinina","value":"194,5","unit":"µmol/L","flag":"high"}],"confirm":true})).await;
    // A disallowed file host never blocks saving the values.
    let foreign = h.ok(&owner, "record_blood_test", json!({"pet_id":pet_id,"test_date":"2026-10-15","results":[{"test_name":"ALT","value":"80","unit":"U/L"}],"confirm":true,"report_file":{"download_url":"https://example.com/report.png","file_id":"file_1"}})).await;
    assert_eq!(foreign["original_report"]["attached"], false);
    assert!(
        foreign["original_report"]["reason"]
            .as_str()
            .unwrap()
            .contains("not allowed")
    );
    let attach = h.err(&owner, "attach_blood_test_original", json!({"report_id":foreign["report_id"],"report_file":{"download_url":"https://evil.blob.core.windows.net/x.pdf"}})).await;
    assert!(attach.contains("not allowed"), "{attach}");
    h.err(&stranger, "attach_blood_test_original", json!({"report_id":foreign["report_id"],"report_file":{"download_url":"https://files.oaiusercontent.com/x.pdf"}})).await;
    let again = h.ok(&owner, "record_blood_test", json!({"pet_id":pet_id,"test_date":"2026-10-15","results":[{"test_name":"ALT/GPT","value":"81","unit":"UI/l"}],"confirm":true})).await;
    assert_eq!(
        again["other_reports_same_date"],
        json!([foreign["report_id"]])
    );
    let history = h
        .ok(&owner, "get_blood_test_history", json!({"pet_id":pet_id}))
        .await;
    assert!(history.to_string().contains("\"analyte\":\"creatinine\""));
    let insights = h
        .ok(&owner, "get_health_insights", json!({"pet_id":pet_id}))
        .await;
    assert!(
        insights
            .to_string()
            .contains("Creatinine changed from 176.8 to 194.5"),
        "{insights}"
    );
    h.err(
        &stranger,
        "delete_blood_test",
        json!({"report_id":first["report_id"]}),
    )
    .await;
    h.ok(
        &owner,
        "delete_blood_test",
        json!({"report_id":first["report_id"]}),
    )
    .await;

    // Vet sharing: link works, then stops working once revoked.
    h.err(
        &owner,
        "create_vet_share",
        json!({"pet_id":pet_id,"label":"Dr Who","confirm":false}),
    )
    .await;
    let share = h
        .ok(
            &owner,
            "create_vet_share",
            json!({"pet_id":pet_id,"label":"Dr Who, TARDIS Vets","days":7,"confirm":true}),
        )
        .await;
    let url = share["url"].as_str().unwrap();
    assert!(url.starts_with("http://app.test/share/"), "{url}");
    let path = url.trim_start_matches("http://app.test");
    let (share_status, share_body) = h.get(path).await;
    assert_eq!(share_status, StatusCode::OK);
    assert!(share_body.contains("id=\"clinical-timeline\""));
    assert!(share_body.contains("id=\"blood-markers\""));
    assert!(share_body.contains("vet-timeline-entry"));
    assert!(
        share_body.contains("vet-chart"),
        "blood markers should render an SVG chart"
    );
    assert!(
        share_body.contains("Creatinine"),
        "lab analyte aliases should be unified"
    );
    assert!(share_body.contains("Prescription history"));
    assert!(
        !share_body.contains("009123456789012"),
        "private microchip must not be exposed"
    );
    assert!(
        !share_body.contains("href=\"/passports/"),
        "passports must not be shared"
    );
    let listed_shares = h.ok(&owner, "list_vet_shares", json!({})).await;
    assert_eq!(listed_shares["shares"][0]["status"], "active");
    assert!(
        !listed_shares
            .to_string()
            .contains(path.trim_start_matches("/share/"))
    );
    h.err(
        &stranger,
        "revoke_vet_share",
        json!({"share_id":share["share_id"]}),
    )
    .await;
    h.ok(
        &owner,
        "revoke_vet_share",
        json!({"share_id":share["share_id"]}),
    )
    .await;
    assert_ne!(h.get(path).await.0, StatusCode::OK);

    // An owner can investigate unresolved symptoms without the assistant diagnosing.
    let case_opened = h
        .ok(
            &owner,
            "open_clinical_case",
            json!({
                "pet_id":pet_id,"title":"Persistent upper respiratory signs",
                "presenting_concern":"Intermittent noisy breathing reported by owner"
            }),
        )
        .await;
    let case_id = case_opened["case_id"].as_i64().unwrap();
    let cases = h
        .ok(&owner, "list_clinical_cases", json!({"pet_id":pet_id}))
        .await;
    assert_eq!(cases["investigations"][0]["id"], case_id);
    h.err(&stranger, "get_clinical_case", json!({"case_id":case_id}))
        .await;
    let observation = h
        .ok(
            &owner,
            "add_clinical_case_update",
            json!({
                "case_id":case_id,"kind":"observation","original_wording":"Occasional wheezing",
                "source":"owner_reported"
            }),
        )
        .await;
    assert!(observation["case_update_id"].as_i64().unwrap() > 0);
    let hypothesis=h.ok(&owner,"add_clinical_hypothesis",json!({
        "case_id":case_id,"label":"Could a medication contribute?","reported_by":"owner_question"
    })).await;
    let hypothesis_id = hypothesis["hypothesis_id"].as_i64().unwrap();
    let refused = h
        .err(
            &owner,
            "review_clinical_hypothesis",
            json!({
                "case_id":case_id,"hypothesis_id":hypothesis_id,"status":"vet_supported",
                "rationale":"No vet assessment","reported_by":"owner_question","confirm":true
            }),
        )
        .await;
    assert!(refused.contains("veterinary assessment"));
    h.ok(
        &owner,
        "review_clinical_hypothesis",
        json!({
            "case_id":case_id,"hypothesis_id":hypothesis_id,"status":"under_review",
            "rationale":"Ask the vet","reported_by":"owner_question","confirm":true
        }),
    )
    .await;
    let detail = h
        .ok(&owner, "get_clinical_case", json!({"case_id":case_id}))
        .await;
    assert_eq!(
        detail["updates"][0]["original_wording"],
        "Occasional wheezing"
    );
    let brief = h
        .ok(&owner, "get_vet_case_brief", json!({"case_id":case_id}))
        .await;
    assert_eq!(brief["case"]["investigation"]["id"], case_id);
    h.ok(
        &owner,
        "set_clinical_case_status",
        json!({
            "case_id":case_id,"status":"monitoring","confirm":true
        }),
    )
    .await;
    // No case data should be accessible from an unrelated household.
    h.err(&stranger, "list_clinical_cases", json!({"pet_id":pet_id}))
        .await;
    h.err(&stranger, "get_vet_case_brief", json!({"case_id":case_id}))
        .await;

    // A new share shows investigation entries in the same chronological timeline.
    let case_share = h
        .ok(
            &owner,
            "create_vet_share",
            json!({"pet_id":pet_id,"label":"Clinical case review","days":1,"confirm":true}),
        )
        .await;
    let case_url = case_share["url"].as_str().unwrap();
    let case_path = case_url.trim_start_matches("http://app.test");
    let (case_status, case_body) = h.get(case_path).await;
    assert_eq!(case_status, StatusCode::OK);
    assert!(case_body.contains("Persistent upper respiratory signs"));
    assert!(case_body.contains("Occasional wheezing"));
    assert!(case_body.contains(&format!("case-{case_id}-update-")));
    h.ok(
        &owner,
        "revoke_vet_share",
        json!({"share_id":case_share["share_id"]}),
    )
    .await;
    assert_ne!(h.get(case_path).await.0, StatusCode::OK);

    // Deleting a pet requires the exact name, then removes everything.
    let refused = h
        .err(
            &owner,
            "delete_pet",
            json!({"pet_id":pet_id,"confirm_name":"testy","confirm":true}),
        )
        .await;
    assert!(refused.contains("Testy"));
    let deleted = h
        .ok(
            &owner,
            "delete_pet",
            json!({"pet_id":pet_id,"confirm_name":"Testy","confirm":true}),
        )
        .await;
    assert!(deleted["removed"]["lab_reports"].as_u64().unwrap() >= 2);
    assert_eq!(deleted["removed"]["pet_passports"], 1);
    h.ok(
        &owner,
        "delete_pet",
        json!({"pet_id":second_id,"confirm_name":"Other","confirm":true}),
    )
    .await;
    let pets = h.ok(&owner, "list_pets", json!({})).await;
    assert_eq!(pets["items"].as_array().unwrap().len(), 0);

    // Every listed tool was exercised, except provider-backed ones.
    let missing: Vec<_> = tools
        .iter()
        .filter(|tool| {
            !h.called.contains(*tool) && !NEEDS_EXTERNAL_PROVIDER.contains(&tool.as_str())
        })
        .collect();
    assert!(
        missing.is_empty(),
        "tools not covered by the e2e test: {missing:?}"
    );

    // Observability: calls were logged without arguments.
    let logged = h
        .state
        .db
        .query_one(sea_orm::Statement::from_string(
            sea_orm::DbBackend::Sqlite,
            "SELECT COUNT(*) AS n, SUM(ok=0) AS failed FROM mcp_tool_calls".to_owned(),
        ))
        .await
        .unwrap()
        .unwrap();
    let n: i64 = logged.try_get("", "n").unwrap();
    let failed: i64 = logged.try_get("", "failed").unwrap();
    assert!(n > 40 && failed > 5, "logged {n} calls, {failed} failed");

    // Build identity endpoint.
    let (status, body) = h.get("/version").await;
    assert_eq!(status, StatusCode::OK);
    assert!(serde_json::from_str::<Value>(&body).unwrap()["commit"].is_string());
}
