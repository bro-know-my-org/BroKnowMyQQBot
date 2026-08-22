use qqbot_protocol::{
    C2cStreamMessageRequest, C2cStreamMessageResponse, DecimalBytes, MediaFileType,
    MediaUploadFinalizeRequest, MediaUploadRequest, MediaUploadResponse,
    MediaUploadValidationError, StreamContentType, StreamInputMode, StreamInputState,
    StreamUploadValidationError, UploadPartFinishRequest, UploadPrepareRequest,
    UploadPrepareResponse,
};
use serde_json::json;

fn first_stream_request() -> C2cStreamMessageRequest {
    C2cStreamMessageRequest {
        input_mode: StreamInputMode::append(),
        input_state: StreamInputState::GENERATING,
        index: 0,
        content_type: StreamContentType::markdown(),
        content_raw: "第一片".to_owned(),
        event_id: None,
        msg_id: Some("message-id".to_owned()),
        stream_msg_id: None,
        msg_seq: Some(1),
        is_wakeup: false,
    }
}

fn prepare_request() -> UploadPrepareRequest {
    UploadPrepareRequest {
        file_type: MediaFileType::VIDEO,
        file_size: DecimalBytes::new("file_size", "10").unwrap(),
        file_name: "video.mp4".to_owned(),
        md5: "a".repeat(32),
        sha1: "b".repeat(40),
        md5_10m: "c".repeat(32),
    }
}

fn prepare_response() -> UploadPrepareResponse {
    serde_json::from_value(json!({
        "upload_id":"upload-capability-id","block_size":"5",
        "parts":[
            {"index":0,"presigned_url":"https://upload.example/part-0?signature=capability-secret","block_size":"5","part_secret":"part-capability-secret"},
            {"index":1,"presigned_url":"https://upload.example/part-1","block_size":"5"}
        ],
        "upload_config":{"concurrency":1,"retry_timeout":300,"retry_delay":1,"config_secret":"config-capability-secret"},
        "extension_secret":"response-capability-secret"
    }))
    .unwrap()
}

#[test]
fn validates_stream_fragments_and_wire_values() {
    let first = first_stream_request();
    first.validate().unwrap();
    assert_eq!(
        serde_json::to_value(&first).unwrap(),
        json!({
            "input_mode":"append","input_state":1,"index":0,
            "content_type":"markdown","content_raw":"第一片",
            "msg_id":"message-id","msg_seq":1,"is_wakeup":false
        })
    );

    let mut next = first.clone();
    next.index = 1;
    next.stream_msg_id = Some("stream-id".to_owned());
    next.input_mode = StreamInputMode::replace();
    next.input_state = StreamInputState::FINISHED;
    next.validate().unwrap();
    assert_eq!(
        serde_json::to_value(&next).unwrap(),
        json!({
            "input_mode":"replace","input_state":10,"index":1,
            "content_type":"markdown","content_raw":"第一片",
            "msg_id":"message-id","stream_msg_id":"stream-id",
            "msg_seq":1,"is_wakeup":false
        })
    );

    let event_reply = C2cStreamMessageRequest {
        event_id: Some("event-id".to_owned()),
        msg_id: None,
        ..first.clone()
    };
    event_reply.validate().unwrap();
    let event_wire = serde_json::to_value(event_reply).unwrap();
    assert_eq!(event_wire["event_id"], "event-id");
    assert!(event_wire.get("msg_id").is_none());

    let missing_reply = C2cStreamMessageRequest {
        msg_id: None,
        ..first.clone()
    };
    assert_eq!(
        missing_reply.validate().unwrap_err(),
        StreamUploadValidationError::MissingReplyReference
    );
    let conflicting_reply = C2cStreamMessageRequest {
        event_id: Some("event-id".to_owned()),
        ..first.clone()
    };
    assert_eq!(
        conflicting_reply.validate().unwrap_err(),
        StreamUploadValidationError::ConflictingReplyReference
    );
    for invalid_sequence in [
        C2cStreamMessageRequest {
            stream_msg_id: Some("stream-id".to_owned()),
            ..first.clone()
        },
        C2cStreamMessageRequest {
            index: 1,
            ..first.clone()
        },
    ] {
        assert_eq!(
            invalid_sequence.validate().unwrap_err(),
            StreamUploadValidationError::InvalidStreamSequence
        );
    }

    let wakeup = C2cStreamMessageRequest {
        msg_id: None,
        is_wakeup: true,
        ..first.clone()
    };
    wakeup.validate().unwrap();

    let mut unknown_mode = first.clone();
    unknown_mode.input_mode = StreamInputMode::new("future");
    assert_eq!(
        unknown_mode.validate().unwrap_err(),
        StreamUploadValidationError::InvalidStreamMode {
            value: "future".to_owned()
        }
    );

    let mut unknown_state = first.clone();
    unknown_state.input_state = StreamInputState::new(99);
    assert_eq!(
        unknown_state.validate().unwrap_err(),
        StreamUploadValidationError::InvalidStreamState { value: 99 }
    );

    let mut unknown_content = first;
    unknown_content.content_type = StreamContentType::new("future");
    assert_eq!(
        unknown_content.validate().unwrap_err(),
        StreamUploadValidationError::InvalidContentType {
            value: "future".to_owned()
        }
    );
}

#[test]
fn validates_stream_response_and_optional_remaining_length() {
    let mut response: C2cStreamMessageResponse = serde_json::from_value(json!({
        "id":"stream-id","timestamp":"2026-08-22T10:00:00+08:00",
        "ext_info":{"ref_idx":"1"},"future":true
    }))
    .unwrap();
    response.validate().unwrap();
    assert_eq!(response.remain_msg_len, None);

    response.extra.insert("id".to_owned(), json!("conflict"));
    response
        .extra
        .insert("timestamp".to_owned(), json!("not-time"));
    response
        .extra
        .insert("ext_info".to_owned(), json!({"conflict":true}));
    response
        .extra
        .insert("remain_msg_len".to_owned(), json!(99));
    let serialized = serde_json::to_string(&response).unwrap();
    for field in ["id", "timestamp", "ext_info"] {
        assert_eq!(serialized.matches(&format!("\"{field}\":")).count(), 1);
    }
    assert!(!serialized.contains("\"remain_msg_len\":"));
    let wire: serde_json::Value = serde_json::from_str(&serialized).unwrap();
    assert_eq!(wire["id"], "stream-id");
    assert_eq!(wire["timestamp"], "2026-08-22T10:00:00+08:00");
    assert_eq!(wire["ext_info"], json!({"ref_idx":"1"}));

    let invalid: C2cStreamMessageResponse = serde_json::from_value(json!({
        "id":"stream-id","timestamp":"not-time"
    }))
    .unwrap();
    assert_eq!(
        invalid.validate().unwrap_err(),
        StreamUploadValidationError::InvalidTimestamp { field: "timestamp" }
    );
}

#[test]
fn validates_prepare_and_part_finish_wire_contracts() {
    let request = prepare_request();
    request.validate().unwrap();
    let wire = serde_json::to_value(&request).unwrap();
    assert_eq!(wire["file_size"], "10");
    assert!(wire["file_size"].is_string());

    for value in ["", "0", "+1", "１２３", "18446744073709551616"] {
        assert!(DecimalBytes::new("file_size", value).is_err());
    }
    assert!(serde_json::from_value::<DecimalBytes>(json!(11)).is_err());

    let mut invalid_digest = request.clone();
    invalid_digest.sha1 = "z".repeat(40);
    assert_eq!(
        invalid_digest.validate().unwrap_err(),
        StreamUploadValidationError::InvalidDigest { field: "sha1" }
    );

    let finish = UploadPartFinishRequest {
        upload_id: "upload-id".to_owned(),
        part_index: 0,
        block_size: DecimalBytes::new("block_size", "5").unwrap(),
        md5: "d".repeat(32),
    };
    finish.validate().unwrap();
    assert_eq!(serde_json::to_value(finish).unwrap()["block_size"], "5");
}

#[test]
fn upload_prepare_response_filters_reserved_extra_keys() {
    let mut response = prepare_response();
    response
        .extra
        .insert("upload_id".to_owned(), json!("conflicting-upload-id"));
    response.extra.insert("block_size".to_owned(), json!("1"));
    response.extra.insert("parts".to_owned(), json!([]));
    response.extra.insert("upload_config".to_owned(), json!({}));
    response.parts[0].extra.insert("index".to_owned(), json!(9));
    response.parts[0].extra.insert(
        "presigned_url".to_owned(),
        json!("https://attacker.example"),
    );
    response.parts[0]
        .extra
        .insert("block_size".to_owned(), json!("1"));
    response
        .upload_config
        .extra
        .insert("concurrency".to_owned(), json!(9));
    response
        .upload_config
        .extra
        .insert("retry_timeout".to_owned(), json!(9));
    response
        .upload_config
        .extra
        .insert("retry_delay".to_owned(), json!(9));
    let serialized = serde_json::to_string(&response).unwrap();
    assert_eq!(serialized.matches("\"upload_id\":").count(), 1);
    assert_eq!(serialized.matches("\"parts\":").count(), 1);
    assert_eq!(serialized.matches("\"upload_config\":").count(), 1);
    assert_eq!(serialized.matches("\"block_size\":").count(), 3);
    assert_eq!(serialized.matches("\"index\":").count(), 2);
    assert_eq!(serialized.matches("\"presigned_url\":").count(), 2);
    for field in ["concurrency", "retry_timeout", "retry_delay"] {
        assert_eq!(serialized.matches(&format!("\"{field}\":")).count(), 1);
    }
    let wire: serde_json::Value = serde_json::from_str(&serialized).unwrap();
    assert_eq!(wire["upload_id"], "upload-capability-id");
    assert_eq!(wire["block_size"], "5");
    assert_eq!(wire["parts"][0]["index"], 0);
    assert_eq!(wire["upload_config"]["concurrency"], 1);
}

#[test]
fn validates_prepare_response_parts_and_chunk_finalize_request() {
    let response = prepare_response();
    response.validate_for_request(&prepare_request()).unwrap();
    assert_eq!(response.parts[1].block_size.value(), 5);
    let debug = format!("{response:?}");
    for secret in [
        "upload-capability-id",
        "capability-secret",
        "part-capability-secret",
        "response-capability-secret",
        "config-capability-secret",
    ] {
        assert!(!debug.contains(secret));
    }
    for part in &response.parts {
        assert!(!debug.contains(part.presigned_url.as_str()));
    }

    let mut duplicate = response.clone();
    duplicate.parts[1].index = 0;
    assert_eq!(
        duplicate.validate().unwrap_err(),
        StreamUploadValidationError::DuplicatePartIndex { index: 0 }
    );

    for indexes in [[1, 2], [0, 2]] {
        let mut non_contiguous = response.clone();
        non_contiguous.parts[0].index = indexes[0];
        non_contiguous.parts[1].index = indexes[1];
        assert_eq!(
            non_contiguous.validate().unwrap_err(),
            StreamUploadValidationError::InvalidPartSequence
        );
    }

    let mut empty = response.clone();
    empty.parts.clear();
    assert_eq!(
        empty.validate().unwrap_err(),
        StreamUploadValidationError::EmptyParts
    );

    let mut zero_concurrency = response.clone();
    zero_concurrency.upload_config.concurrency = 0;
    assert_eq!(
        zero_concurrency.validate().unwrap_err(),
        StreamUploadValidationError::ZeroConcurrency
    );

    let mut padded_response_upload_id = response.clone();
    padded_response_upload_id.upload_id = " upload-id".to_owned();
    assert_eq!(
        padded_response_upload_id.validate().unwrap_err(),
        StreamUploadValidationError::InvalidUploadId
    );

    let mut invalid_url = response.clone();
    invalid_url.parts[0].presigned_url = "file:///tmp/part".to_owned();
    assert_eq!(
        invalid_url.validate().unwrap_err(),
        StreamUploadValidationError::InvalidPresignedUrl
    );

    let mut insecure_url = response.clone();
    insecure_url.parts[0].presigned_url = "http://upload.example/part-0".to_owned();
    assert_eq!(
        insecure_url.validate().unwrap_err(),
        StreamUploadValidationError::InvalidPresignedUrl
    );

    for value in [
        "https://user:password@upload.example/part-0",
        "https://@upload.example/part-0",
        "https:\n//user:secret@upload.example/part-0",
        "https:\\user:secret@upload.example/part-0",
    ] {
        let mut userinfo_url = response.clone();
        userinfo_url.parts[0].presigned_url = value.to_owned();
        assert_eq!(
            userinfo_url.validate().unwrap_err(),
            StreamUploadValidationError::InvalidPresignedUrl
        );
    }

    let mut fragment_url = response;
    fragment_url.parts[0].presigned_url = "https://upload.example/part-0#secret".to_owned();
    assert_eq!(
        fragment_url.validate().unwrap_err(),
        StreamUploadValidationError::InvalidPresignedUrl
    );
}

#[test]
fn rejects_inconsistent_prepare_response_part_sizes() {
    let response = prepare_response();
    let mut short_regular_part = response.clone();
    short_regular_part.parts[0].block_size = DecimalBytes::new("block_size", "4").unwrap();
    assert_eq!(
        short_regular_part.validate().unwrap_err(),
        StreamUploadValidationError::InvalidPartBlockSize {
            index: 0,
            expected_max: 5,
            actual: 4,
        }
    );
    let mut oversized_final_part = response;
    oversized_final_part.parts[1].block_size = DecimalBytes::new("block_size", "6").unwrap();
    assert_eq!(
        oversized_final_part.validate().unwrap_err(),
        StreamUploadValidationError::InvalidPartBlockSize {
            index: 1,
            expected_max: 5,
            actual: 6,
        }
    );
    let mut short_final_part = prepare_response();
    short_final_part.parts[1].block_size = DecimalBytes::new("block_size", "1").unwrap();
    short_final_part.validate().unwrap();
    assert!(
        serde_json::from_value::<UploadPrepareResponse>(json!({
            "upload_id":"upload-id","block_size":"5",
            "parts":[{"index":0,"presigned_url":"https://upload.example/part-0","block_size":"0"}],
            "upload_config":{"concurrency":1,"retry_timeout":300,"retry_delay":1}
        }))
        .is_err()
    );
}

#[test]
fn rejects_prepare_part_plans_with_the_wrong_total_size() {
    let response = prepare_response();
    for (file_size, expected, actual) in [("11", 11, 10), ("9", 9, 10)] {
        let mut request = prepare_request();
        request.file_size = DecimalBytes::new("file_size", file_size).unwrap();
        assert_eq!(
            response.validate_for_request(&request).unwrap_err(),
            StreamUploadValidationError::PartPlanSizeMismatch { expected, actual }
        );
    }
}

#[test]
fn rejects_prepare_part_plan_size_overflow() {
    let response: UploadPrepareResponse = serde_json::from_value(json!({
        "upload_id":"upload-id","block_size":"18446744073709551615",
        "parts":[
            {"index":0,"presigned_url":"https://upload.example/part-0","block_size":"18446744073709551615"},
            {"index":1,"presigned_url":"https://upload.example/part-1","block_size":"18446744073709551615"}
        ],
        "upload_config":{"concurrency":1,"retry_timeout":300,"retry_delay":1}
    }))
    .unwrap();
    let mut request = prepare_request();
    request.file_size = DecimalBytes::new("file_size", "18446744073709551615").unwrap();

    assert_eq!(
        response.validate_for_request(&request).unwrap_err(),
        StreamUploadValidationError::PartPlanSizeOverflow
    );
}

#[test]
fn media_upload_response_preserves_flattened_compatibility_fields() {
    let response: MediaUploadResponse = serde_json::from_value(json!({
        "file_uuid":"file-uuid",
        "file_info":"file-info",
        "ttl":3600,
        "id":"message-id",
        "raw_url":"https://cdn.example/media",
        "future_field":true
    }))
    .unwrap();

    assert_eq!(response.file_uuid, "file-uuid");
    assert_eq!(response.file_info, "file-info");
    assert_eq!(response.ttl, Some(3600));
    assert_eq!(response.message_id(), Some("message-id"));
    assert_eq!(response.raw_url(), Some("https://cdn.example/media"));
    assert_eq!(response.extra.get("future_field"), Some(&json!(true)));

    let mut colliding = response.clone();
    colliding
        .extra
        .insert("file_uuid".to_owned(), json!("conflicting-file-uuid"));
    colliding
        .extra
        .insert("file_info".to_owned(), json!("conflicting-file-info"));
    colliding.extra.insert("ttl".to_owned(), json!(1));
    let collision_wire = serde_json::to_string(&colliding).unwrap();
    assert_eq!(collision_wire.matches("\"file_uuid\":").count(), 1);
    assert_eq!(collision_wire.matches("\"file_info\":").count(), 1);
    assert_eq!(collision_wire.matches("\"ttl\":").count(), 1);
    let collision_value: serde_json::Value = serde_json::from_str(&collision_wire).unwrap();
    assert_eq!(collision_value["file_uuid"], "file-uuid");
    assert_eq!(collision_value["file_info"], "file-info");
    assert_eq!(collision_value["ttl"], 3600);

    let serialized = serde_json::to_string(&response).unwrap();
    assert_eq!(serialized.matches("\"id\":").count(), 1);
    assert_eq!(serialized.matches("\"raw_url\":").count(), 1);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&serialized).unwrap(),
        json!({
            "file_uuid":"file-uuid",
            "file_info":"file-info",
            "ttl":3600,
            "id":"message-id",
            "raw_url":"https://cdn.example/media",
            "future_field":true
        })
    );
}

#[test]
fn validates_finalize_and_legacy_url_requests() {
    let mut finalize = MediaUploadFinalizeRequest::new(
        MediaFileType::VIDEO,
        "upload-id",
        Some("video.mp4".to_owned()),
    );
    finalize.srv_send_msg = true;
    finalize.validate().unwrap();
    assert!(!format!("{finalize:?}").contains("upload-id"));
    assert_eq!(
        serde_json::to_value(finalize).unwrap(),
        json!({
            "file_type":2,"upload_id":"upload-id","file_name":"video.mp4",
            "srv_send_msg":true
        })
    );

    let padded_upload_id = MediaUploadFinalizeRequest::new(
        MediaFileType::VIDEO,
        " upload-id",
        Some("video.mp4".to_owned()),
    );
    assert_eq!(
        padded_upload_id.validate().unwrap_err(),
        StreamUploadValidationError::InvalidUploadId
    );
    let padded_finish = UploadPartFinishRequest {
        upload_id: "upload-id ".to_owned(),
        part_index: 0,
        block_size: DecimalBytes::new("block_size", "5").unwrap(),
        md5: "d".repeat(32),
    };
    assert_eq!(
        padded_finish.validate().unwrap_err(),
        StreamUploadValidationError::InvalidUploadId
    );
    assert!(!format!("{padded_finish:?}").contains("upload-id"));
    let empty_upload_id = MediaUploadFinalizeRequest::new(MediaFileType::VIDEO, "", None);
    assert_eq!(
        empty_upload_id.validate().unwrap_err(),
        StreamUploadValidationError::EmptyField { field: "upload_id" }
    );

    let url_upload =
        MediaUploadRequest::from_url(MediaFileType::VIDEO, "https://example.com/video.mp4");
    url_upload.validate().unwrap();
    assert_eq!(
        serde_json::to_value(url_upload).unwrap(),
        json!({
            "file_type":2,"url":"https://example.com/video.mp4","srv_send_msg":false
        })
    );

    for (invalid_url, expected) in [
        (
            "https://example.com/video.mp4#secret",
            MediaUploadValidationError::InvalidUrlFragment,
        ),
        (
            "https://user:password@example.com/video.mp4",
            MediaUploadValidationError::InvalidUrlCredentials,
        ),
        (
            "https://@example.com/video.mp4",
            MediaUploadValidationError::InvalidUrlCredentials,
        ),
        (
            "https:\n//user:secret@example.com/video.mp4",
            MediaUploadValidationError::InvalidUrlCredentials,
        ),
        (
            "https:\\user:secret@example.com/video.mp4",
            MediaUploadValidationError::InvalidUrlCredentials,
        ),
        (
            "file:///tmp/video.mp4",
            MediaUploadValidationError::InvalidUrlScheme,
        ),
    ] {
        let request = MediaUploadRequest::from_url(MediaFileType::VIDEO, invalid_url);
        assert_eq!(request.validate().unwrap_err(), expected);
    }
}
