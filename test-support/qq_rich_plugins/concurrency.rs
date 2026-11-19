use super::*;

#[tokio::test]
async fn early_click_waits_for_send_result_before_becoming_eligible() {
    for status in [StatusCode::OK, StatusCode::BAD_REQUEST] {
        let mut run = Harness::new(Arc::new(QqMarkdownPagesPlugin::default()), false).await;
        run.observed
            .active_first_page
            .store(true, Ordering::Relaxed);
        let (release, pause) = oneshot::channel();
        *run.api.pause_next_send.lock().unwrap() = Some(pause);

        // Start an active C2C send from another conversation, so its HTTP response
        // cannot block the runtime's C2C partition from receiving the click.
        run.submit(
            "origin",
            command_event("origin", group("other-partition")),
            0,
        )
        .await;
        timeout(WAIT, run.api.accepted.notified()).await.unwrap();
        let token = run.last_token();
        run.submit("early", click(&token, "early-ack", "reader", c2c()), 0)
            .await;
        assert!(
            timeout(Duration::from_millis(150), run.handled.recv())
                .await
                .is_err()
        );
        release.send(status).unwrap();

        let mut handled = Vec::new();
        for _ in 0..2 {
            handled.push(timeout(WAIT, run.handled.recv()).await.unwrap().unwrap());
        }
        handled.sort_unstable();
        assert_eq!(handled, ["early", "origin"]);
        let calls = run.requests();
        if status.is_success() {
            assert_eq!(calls.len(), 3);
            assert_eq!(
                calls[1],
                ("interactions/early-ack".to_owned(), json!({"code":0}))
            );
        } else {
            assert_eq!(calls.len(), 1);
        }
        run.stop().await;
    }
}

#[tokio::test]
async fn overlapping_acks_keep_their_own_followup_in_both_backends() {
    for plugin in [
        Arc::new(QqMarkdownPagesPlugin::default()) as Arc<dyn StaticPlugin>,
        wasm_pager().await,
    ] {
        let mut run = Harness::new(plugin, false).await;
        let second = MessageTarget::Private {
            user_id: "reader-2".to_owned(),
        };
        run.command("first", c2c()).await;
        let first_token = run.last_token();
        run.command("second", second.clone()).await;
        let second_token = run.last_token();

        let (release, paused) = oneshot::channel();
        *run.api.pause_next_ack.lock().unwrap() = Some(paused);
        run.submit(
            "first-click",
            click(&first_token, "first-ack", "reader", c2c()),
            0,
        )
        .await;
        timeout(WAIT, run.api.ack_started.notified()).await.unwrap();
        run.deliver(
            "second-click",
            click(&second_token, "second-ack", "reader-2", second),
            0,
        )
        .await;

        release.send(()).unwrap();
        assert_eq!(
            timeout(WAIT, run.handled.recv()).await.unwrap().as_deref(),
            Some("first-click")
        );
        let calls = run.requests();
        let paths: Vec<_> = calls.iter().map(|(path, _)| path.as_str()).collect();
        assert_eq!(
            paths,
            [
                "users/reader",
                "users/reader-2",
                "interactions/first-ack",
                "interactions/second-ack",
                "users/reader-2",
                "users/reader",
            ]
        );
        assert!(calls[4].1.get("msg_id").is_none());
        assert!(calls[5].1.get("msg_id").is_none());
        run.stop().await;
    }
}

#[tokio::test]
async fn slow_button_delivery_expires_without_retry_and_releases_the_conversation() {
    let mut run = Harness::new(Arc::new(QqMarkdownPagesPlugin::default()), false).await;
    run.command("original", c2c()).await;
    let token = run.last_token();
    run.observed.slow_click.store(true, Ordering::Relaxed);
    timeout(
        Duration::from_secs(2),
        run.deliver("slow-click", click(&token, "slow-ack", "reader", c2c()), 4),
    )
    .await
    .expect("the callback deadline should release the conversation");
    assert_eq!(run.observed.button_invocations.load(Ordering::Relaxed), 1);
    assert_eq!(run.requests().len(), 1);
    run.command("new-page", c2c()).await;
    assert_eq!(run.requests().len(), 2);
    run.stop().await;
}

pub(super) fn test_manifest(mut manifest: PluginManifest) -> PluginManifest {
    manifest.runtime.mode = RuntimeMode::Partitioned;
    manifest.runtime.max_concurrency = 2;
    manifest
        .permissions
        .actions
        .extend(["message.send".to_owned(), "media.create".to_owned()]);
    manifest.permissions.http.push(plugin_api::HttpPermission {
        host: "example.com".to_owned(),
        port: 443,
        methods: std::collections::BTreeSet::from(["GET".to_owned()]),
        path_prefixes: std::collections::BTreeSet::from(["/".to_owned()]),
        credential: None,
    });
    manifest
}

pub(super) fn forbidden_click_commands() -> [PluginCommand; 2] {
    [
        PluginCommand {
            command_id: "image".to_owned(),
            kind: "media.create".to_owned(),
            idempotency_key: None,
            deadline_ms: None,
            payload: json!({"mime_type":"image/png","data_base64":"invalid"}),
        },
        PluginCommand {
            command_id: "slow-http".to_owned(),
            kind: "http.request".to_owned(),
            idempotency_key: None,
            deadline_ms: None,
            payload: json!({"method":"GET","url":"https://example.com/"}),
        },
    ]
}

#[derive(Default)]
pub(super) struct SlowHttp(AtomicUsize);

#[async_trait]
impl plugin_host::HttpExecutor for SlowHttp {
    async fn execute(
        &self,
        _: &[plugin_api::HttpPermission],
        _: &std::collections::BTreeSet<String>,
        _: &plugin_api::HttpRequest,
    ) -> Result<plugin_api::HttpResponse, plugin_host::HttpExecutionError> {
        self.0.fetch_add(1, Ordering::Relaxed);
        std::future::pending().await
    }
}

#[tokio::test]
async fn an_initial_click_cannot_run_slow_http_before_ack_or_block_the_next_event() {
    let mut run = Harness::new(Arc::new(QqMarkdownPagesPlugin::default()), false).await;
    run.command("original", c2c()).await;
    let token = run.last_token();
    run.observed.slow_command.store(true, Ordering::Relaxed);
    timeout(
        Duration::from_secs(3),
        run.deliver(
            "slow-command-click",
            click(&token, "timely-ack", "reader", c2c()),
            3,
        ),
    )
    .await
    .expect("the click should reach ACK without waiting for HTTP");
    assert_eq!(run.slow_http.0.load(Ordering::Relaxed), 0);
    {
        let receipts = run.observed.receipts.lock().unwrap();
        assert_eq!(receipts[1].kind, "media.create");
        assert_eq!(receipts[1].status, ActionStatus::Denied);
        assert_eq!(receipts[2].kind, "http.request");
        assert_eq!(receipts[2].status, ActionStatus::Denied);
        assert_eq!(receipts[3].kind, QQ_BUTTON_ACK);
        assert_eq!(receipts[3].status, ActionStatus::Succeeded);
    }
    run.command("following-message", c2c()).await;
    assert_eq!(run.requests().len(), 4);
    run.stop().await;
}
