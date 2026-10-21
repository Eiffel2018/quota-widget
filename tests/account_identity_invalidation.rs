use quota_widget::model::QuotaModel;
use serde_json::json;

#[test]
fn explicit_account_change_does_not_retain_old_identity_when_next_payload_omits_it() {
    let mut model = QuotaModel::new("synthetic-session");
    model
        .ingest_json(
            &json!({"accountId":"synthetic-account-a","rateLimits":{
        "primary":{"usedPercent":10,"windowDurationMins":10080,"resetsAt":100000}}}),
            100,
        )
        .unwrap();
    assert_eq!(model.account_id(), Some("synthetic-account-a"));
    model.account_changed();
    assert_eq!(model.account_id(), None);
    model
        .ingest_json(
            &json!({"rateLimits":{
        "primary":{"usedPercent":80,"windowDurationMins":10080,"resetsAt":100000}}}),
            110,
        )
        .unwrap();
    assert_eq!(model.account_id(), None);
    assert!(
        model.history.is_empty(),
        "account transition cannot invent a 70-point drop"
    );
    model
        .ingest_json(
            &json!({"accountId":"synthetic-account-b","rateLimits":{
        "primary":{"usedPercent":81,"windowDurationMins":10080,"resetsAt":100000}}}),
            120,
        )
        .unwrap();
    assert_eq!(model.account_id(), Some("synthetic-account-b"));
}
