use plugin_api::*;
use serde_json::json;

fn descriptor() -> ManagementDescriptor {
    serde_json::from_value(json!({"schema_version":1,"resources":[{
        "id":"repositories", "title":"仓库别名", "schema":{"type":"object"},
        "columns":[{"key":"alias","title":"别名"}],"operations":["list","create"]
    }]}))
    .unwrap()
}

#[test]
fn descriptor_bounds_and_references() {
    let valid = descriptor();
    valid.validate().unwrap();
    for invalid in ["", "../bad", "with space", &"x".repeat(65)] {
        let mut value = valid.clone();
        value.resources[0].id = invalid.into();
        assert!(value.validate().is_err());
    }
    for reference in [
        "https://example.com/schema",
        "file:///etc/passwd",
        "other.json",
    ] {
        let mut value = valid.clone();
        value.resources[0].schema = json!({"$defs":{"nested":{"$ref":reference}}});
        assert!(value.validate().is_err());
    }
    let mut escaped = valid.clone();
    escaped.resources[0].schema =
        json!({"$ref":"#/%63ustom","custom":{"$ref":"https://example.com"}});
    assert!(escaped.validate().is_err());
    escaped.resources[0].schema =
        json!({"$ref":"#/$defs/node","$defs":{"node":{"$ref":"#/$defs/node"}}});
    escaped.validate().unwrap();
    let mut nested = valid.clone();
    nested.resources[0].schema = json!({"$ref":"#/$defs/n/custom","$defs":{"n":{"$id":"https://example.com/n","custom":{"$ref":"#/hidden"},"hidden":{"$ref":"https://example.com/external"}}}});
    assert!(nested.validate().is_err());
    let mut anchored = valid.clone();
    anchored.resources[0].schema = json!({"$ref":"#node","$defs":{"node":{"$anchor":"node","type":"object"}},"default":{"$anchor":"node","$ref":"https://example.com"}});
    anchored.validate().unwrap();
    let mut literals = valid.clone();
    literals.resources[0].schema = json!({"type":"object","properties":{"$ref":{"type":"string"}},"default":{"$ref":"other.json"},"const":{"$dynamicRef":"https://example.com"}});
    literals.validate().unwrap();
    let mut value = valid.clone();
    value.resources = vec![value.resources[0].clone(); 17];
    assert!(value.validate().is_err());
    let mut value = valid;
    value.resources[0].schema = json!({"description":"x".repeat(65536)});
    assert!(value.validate().is_err());
}

#[test]
fn requests_and_response_bounds_and_write_separation() {
    let mut request: ManagementRequest = serde_json::from_value(json!({
        "resource":"repositories","operation":"list"
    }))
    .unwrap();
    assert_eq!(request.limit, 20);
    request.validate().unwrap();
    request.limit = 101;
    assert!(request.validate().is_err());
    request.limit = 1;
    request.cursor = Some("x".repeat(513));
    assert!(request.validate().is_err());
    request.cursor = None;
    let mut response = ManagementResponse {
        data: json!({"records":[{"id":"A","values":{}}],"next_cursor":null}),
        state_ops: vec![],
    };
    response.validate_for(&request).unwrap();
    response.state_ops.push(StateOp::Delete {
        key: "repos/A".into(),
        expected_revision: Some(0),
    });
    assert!(response.validate_for(&request).is_err());
    request.operation = ManagementOperation::Delete;
    response.validate_for(&request).unwrap();
    response.state_ops = vec![response.state_ops[0].clone(); 257];
    assert!(response.validate().is_err());
    response.state_ops.clear();
    response.data["records"][0]["values"] = json!({"text":"x".repeat(1024*1024)});
    assert!(response.validate().is_err());
    response.data = json!({"records":[{"id":"A","values":null}],"next_cursor":null});
    assert!(response.validate().is_err());
}

#[test]
fn management_error_is_public_and_standard_error() {
    let error = ManagementError::new(ManagementErrorCode::Conflict, "stale");
    let standard: &dyn std::error::Error = &error;
    assert_eq!(standard.to_string(), "stale");
    assert_eq!(error.code, ManagementErrorCode::Conflict);
    assert_eq!(error.message, "stale");
}
