//! Regression coverage for logical conversations across native agent boundaries.
use codeg_lib::acp::{error::AcpError, manager::ConnectionManager, types::PromptInputBlock};
use codeg_lib::db::service::conversation_handoff::*;
use codeg_lib::db::{
    entities::conversation::ConversationStatus,
    service::{conversation_handoff, conversation_service},
    AppDatabase,
};
use codeg_lib::models::{
    message::{ContentBlock, MessageTurn},
    AgentType,
};
use codeg_lib::web::event_bridge::EventEmitter;

async fn rejected_prompt(
    db: &AppDatabase,
    manager: &ConnectionManager,
    connection: &str,
    id: i32,
) -> String {
    let folder = conversation_service::get_by_id(&db.conn, id)
        .await
        .unwrap()
        .folder_id;
    manager
        .send_prompt_linked_with_message_id(
            db,
            connection,
            vec![PromptInputBlock::Text {
                text: "must not be sent".into(),
            }],
            Some(folder),
            Some(id),
            None,
            None,
        )
        .await
        .unwrap_err()
        .to_string()
}

mod history_tests {
    use super::*;
    use codeg_lib::db::{service::conversation_service, test_helpers};

    fn turn(role: &str, text: &str) -> MessageTurn {
        serde_json::from_value(serde_json::json!({
            "id": "turn-0", "role": role, "blocks": [{"type": "text", "text": text}],
            "timestamp": "2026-10-05T00:00:00Z"
        }))
        .unwrap()
    }

    #[tokio::test]
    async fn retained_native_session_boundary_avoids_duplicate_history_on_return() {
        let db = test_helpers::fresh_in_memory_db().await;
        let folder = test_helpers::seed_folder(&db, "/workspace").await;
        let conv = conversation_service::create(&db.conn, folder, AgentType::Codex, None, None).await.unwrap();
        conversation_service::bind_external_id(&db.conn, conv.id, "a1", &[]).await.unwrap();
        let a = conversation_service::get_by_id(&db.conn, conv.id).await.unwrap();
        let original = vec![turn("user", "A secret ORCHID"), turn("assistant", "A result")];
        save_switch_with_boundary(&db.conn, &a, AgentType::Custom("devin"), original.clone(), Some(2)).await.unwrap();
        conversation_service::bind_external_id(&db.conn, conv.id, "b1", &[]).await.unwrap();
        let b = conversation_service::get_by_id(&db.conn, conv.id).await.unwrap();
        let mut archive = original.clone(); archive.push(turn("assistant", "B new result"));
        save_switch_with_boundary(&db.conn, &b, AgentType::Codex, archive, Some(1)).await.unwrap();
        let restored = conversation_service::get_by_id(&db.conn, conv.id).await.unwrap();
        assert_eq!(restored.external_id.as_deref(), Some("a1"));
        let h = load(&db.conn, conv.id).await.unwrap().unwrap();
        assert_eq!(h.active_native_turn_count, 2);
        assert_eq!(h.merge(original.clone()).len(), 3);
        let mut resumed = original;
        resumed.push(turn("user", &format!("{}Continue A", h.context_prefix)));
        resumed.push(turn("assistant", "A continues with B knowledge"));
        let merged = h.merge(resumed);
        assert_eq!(merged.len(), 5);
        assert!(matches!(&merged[3].blocks[0], ContentBlock::Text{text} if text == "Continue A"));
        assert!(h.context_prefix.contains("B new result"));
    }

    #[tokio::test]
    async fn round_trip_switch_and_restart_preserves_one_conversation() {
        let dir = tempfile::tempdir().unwrap();
        let db = test_helpers::fresh_disk_db(dir.path()).await;
        let folder = test_helpers::seed_folder(&db, "/tmp/codeg-handoff-test").await;
        let conv = conversation_service::create(
            &db.conn,
            folder,
            AgentType::Codex,
            Some("My conversation".into()),
            None,
        )
        .await
        .unwrap();
        conversation_service::bind_external_id(&db.conn, conv.id, "a1", &[])
            .await
            .unwrap();
        let source = conversation_service::get_by_id(&db.conn, conv.id)
            .await
            .unwrap();
        save_switch(
            &db.conn,
            &source,
            AgentType::Antigravity,
            vec![turn("user", "Remember ORCHID-742")],
        )
        .await
        .unwrap();

        // Reopen the actual on-disk SQLite database, not just the same object.
        let reopened = sea_orm::Database::connect(format!(
            "sqlite:{}",
            dir.path().join("source.db").display()
        ))
        .await
        .unwrap();
        let b = conversation_service::get_by_id(&reopened, conv.id)
            .await
            .unwrap();
        assert_eq!(b.agent_type, AgentType::Antigravity);
        assert!(b.external_id.is_none());
        // Legacy archives had no segment revision on their retired sessions.
        use sea_orm::{ConnectionTrait, DbBackend, Statement};
        reopened.execute(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "UPDATE conversation_handoff SET retired_sessions_json = ? WHERE conversation_id = ?",
            [r#"[{"agent":"codex","session_id":"a1"}]"#.into(), conv.id.into()],
        )).await.unwrap();
        let handoff = load(&reopened, conv.id).await.unwrap().unwrap();
        assert_eq!(handoff.retired_sessions[0].revision, Some(0));
        assert!(handoff.context_prefix.contains("ORCHID-742"));
        assert!(handoff.is_retired(AgentType::Codex, "a1"));
        let prompt = format!("{}What did I say?", handoff.context_prefix);
        let native = vec![turn("user", &prompt), turn("assistant", "ORCHID-742")];
        assert!(handoff.context_was_recorded(&native));
        let merged = handoff.merge(native);
        assert_eq!(merged.len(), 3);
        assert_ne!(merged[0].id, merged[1].id);
        assert!(
            matches!(&merged[1].blocks[0], ContentBlock::Text { text } if text == "What did I say?")
        );

        conversation_service::bind_external_id(&reopened, conv.id, "b1", &[])
            .await
            .unwrap();
        let source = conversation_service::get_by_id(&reopened, conv.id)
            .await
            .unwrap();
        save_switch(&reopened, &source, AgentType::Codex, merged)
            .await
            .unwrap();
        let a = conversation_service::get_by_id(&reopened, conv.id)
            .await
            .unwrap();
        assert_eq!(a.id, conv.id);
        assert_eq!(a.agent_type, AgentType::Codex);
        assert!(a.external_id.is_none());
        let second = load(&reopened, conv.id).await.unwrap().unwrap();
        assert_eq!(second.revision, 2);
        assert_eq!(second.archive.len(), 3);
        assert_eq!(second.retired_sessions.len(), 2);
        assert_eq!(second.retired_sessions[0].revision, Some(0));
        assert_eq!(second.retired_sessions[1].revision, Some(1));
        assert!(!second.context_prefix.contains(&handoff.context_prefix));
    }

    #[tokio::test]
    async fn stale_switch_rolls_back_archive_and_binding() {
        let db = test_helpers::fresh_in_memory_db().await;
        let folder = test_helpers::seed_folder(&db, "/tmp/codeg-handoff-test").await;
        let conv = conversation_service::create(&db.conn, folder, AgentType::Codex, None, None)
            .await
            .unwrap();
        let stale = conversation_service::get_by_id(&db.conn, conv.id)
            .await
            .unwrap();
        save_switch(
            &db.conn,
            &stale,
            AgentType::Antigravity,
            vec![turn("user", "saved")],
        )
        .await
        .unwrap();
        assert!(save_switch(
            &db.conn,
            &stale,
            AgentType::Antigravity,
            vec![turn("user", "must not replace")]
        )
        .await
        .is_err());
        let error = ConnectionManager::new().switch_conversation_agent(
            &db, EventEmitter::Noop, conv.id, AgentType::Codex, None,
            AgentType::Antigravity,
        ).await.unwrap_err();
        assert!(matches!(error, AcpError::ConversationChanged(_)));
        let h = load(&db.conn, conv.id).await.unwrap().unwrap();
        assert_eq!(h.revision, 1);
        assert!(h.context_prefix.contains("saved"));
        assert!(!h.context_prefix.contains("must not replace"));
    }

    #[test]
    fn oversized_context_is_rejected_without_silent_truncation() {
        assert!(context_prefix(&[turn("user", &"界".repeat(120_000))]).is_err());
    }

    #[test]
    fn adapter_whitespace_normalization_does_not_duplicate_the_seed() {
        let h = Handoff {
            revision: 1,
            archive: vec![],
            retired_sessions: vec![],
            active_native_turn_count: 0,
            context_prefix: context_prefix(&[turn("user", "remember this")]).unwrap(),
        };
        let normalized = turn("user", h.context_prefix.trim());
        assert!(h.context_was_recorded(std::slice::from_ref(&normalized)));
        assert!(h.merge(vec![normalized]).is_empty());
    }

    #[test]
    fn transferred_context_does_not_strip_indentation_from_the_new_message() {
        let h = Handoff {
            revision: 1,
            archive: vec![],
            retired_sessions: vec![],
            active_native_turn_count: 0,
            context_prefix: context_prefix(&[turn("user", "previous")]).unwrap(),
        };
        let message = format!("{}    indented code", h.context_prefix);
        let merged = h.merge(vec![turn("user", &message)]);
        assert!(
            matches!(&merged[0].blocks[0], ContentBlock::Text { text } if text == "    indented code")
        );
    }

    #[test]
    fn reasoning_and_image_bytes_do_not_travel_to_another_agent() {
        let mut input = turn("assistant", "Public answer");
        input.blocks.push(ContentBlock::Thinking {
            text: "private reasoning".into(),
        });
        input.blocks.push(ContentBlock::Image {
            data: "secret-binary-payload".into(),
            mime_type: "image/png".into(),
            uri: None,
        });
        let context = context_prefix(&[input]).unwrap();
        assert!(context.contains("Public answer"));
        assert!(!context.contains("private reasoning"));
        assert!(!context.contains("secret-binary-payload"));
        assert!(context.contains("Image bytes not transferred"));
    }

    #[test]
    fn only_exact_transfer_prefix_is_removed() {
        let h = Handoff {
            revision: 1,
            archive: vec![],
            retired_sessions: vec![],
            active_native_turn_count: 0,
            context_prefix: "exact private nonce prefix\n".into(),
        };
        let ordinary = turn("user", "[Codeg context transfer other] keep this text");
        assert!(!h.context_was_recorded(std::slice::from_ref(&ordinary)));
        let merged = h.merge(vec![ordinary]);
        assert!(
            matches!(&merged[0].blocks[0], ContentBlock::Text { text } if text.ends_with("keep this text"))
        );
    }
}

mod runtime_tests {
    use super::*;
    use codeg_lib::db::test_helpers;

    async fn fixture() -> (AppDatabase, ConnectionManager, i32) {
        let db = test_helpers::fresh_in_memory_db().await;
        let fid = test_helpers::seed_folder(&db, "/tmp/codeg-handoff-test").await;
        let conv = conversation_service::create(&db.conn, fid, AgentType::Codex, None, None)
            .await
            .unwrap();
        conversation_service::update_status(&db.conn, conv.id, ConversationStatus::PendingReview)
            .await
            .unwrap();
        let manager = ConnectionManager::new();
        manager
            .insert_test_connection("old", AgentType::Codex, None, EventEmitter::Noop)
            .await;
        manager
            .get_state("old")
            .await
            .unwrap()
            .write()
            .await
            .conversation_id = Some(conv.id);
        (db, manager, conv.id)
    }

    #[tokio::test]
    async fn busy_switch_changes_nothing() {
        let (db, manager, id) = fixture().await;
        manager
            .get_state("old")
            .await
            .unwrap()
            .write()
            .await
            .turn_in_flight = true;
        let error = manager
            .switch_conversation_agent(
                &db,
                EventEmitter::Noop,
                id,
                AgentType::Codex,
                None,
                AgentType::Antigravity,
            )
            .await
            .unwrap_err();
        assert!(matches!(error, AcpError::TurnInProgress));
        assert!(conversation_handoff::load(&db.conn, id)
            .await
            .unwrap()
            .is_none());
        assert_eq!(
            conversation_service::get_by_id(&db.conn, id)
                .await
                .unwrap()
                .agent_type,
            AgentType::Codex
        );
        assert!(manager.get_state("old").await.is_some());
    }

    #[tokio::test]
    async fn successful_switch_retires_old_runtime_and_rejects_stale_sender() {
        let (db, manager, id) = fixture().await;
        let old = manager.get_state("old").await.unwrap();
        manager
            .switch_conversation_agent(
                &db,
                EventEmitter::Noop,
                id,
                AgentType::Codex,
                None,
                AgentType::Antigravity,
            )
            .await
            .unwrap();
        assert!(old.read().await.retired_for_handoff);
        assert!(manager.get_state("old").await.is_some());
        manager
            .insert_test_connection("stale", AgentType::Codex, None, EventEmitter::Noop)
            .await;
        assert!(rejected_prompt(&db, &manager, "stale", id)
            .await
            .contains("switched to another agent"));
        assert_eq!(
            conversation_handoff::load(&db.conn, id)
                .await
                .unwrap()
                .unwrap()
                .revision,
            1
        );
    }

    #[tokio::test]
    async fn concurrent_switches_commit_only_once() {
        let (db, manager, id) = fixture().await;
        let (a, b) = tokio::join!(
            manager.switch_conversation_agent(
                &db,
                EventEmitter::Noop,
                id,
                AgentType::Codex,
                None,
                AgentType::Antigravity
            ),
            manager.switch_conversation_agent(
                &db,
                EventEmitter::Noop,
                id,
                AgentType::Codex,
                None,
                AgentType::Antigravity
            ),
        );
        assert_ne!(a.is_ok(), b.is_ok());
        assert_eq!(
            conversation_handoff::load(&db.conn, id)
                .await
                .unwrap()
                .unwrap()
                .revision,
            1
        );
    }

    #[tokio::test]
    async fn switching_back_cannot_reuse_a_retired_native_session() {
        let (db, manager, id) = fixture().await;
        // Exercise the persistent check even when the old in-memory retirement
        // flag is gone, as it would be after restarting the server.
        conversation_service::bind_external_id(&db.conn, id, "a1", &[])
            .await
            .unwrap();
        let a = conversation_service::get_by_id(&db.conn, id).await.unwrap();
        conversation_handoff::save_switch(&db.conn, &a, AgentType::Antigravity, vec![])
            .await
            .unwrap();
        let b = conversation_service::get_by_id(&db.conn, id).await.unwrap();
        conversation_handoff::save_switch(&db.conn, &b, AgentType::Codex, vec![])
            .await
            .unwrap();
        manager
            .get_state("old")
            .await
            .unwrap()
            .write()
            .await
            .external_id = Some("a1".into());
        assert!(rejected_prompt(&db, &manager, "old", id)
            .await
            .contains("no longer owns"));
        assert_eq!(
            conversation_handoff::retired_owner(&db.conn, AgentType::Codex, "a1")
                .await
                .unwrap(),
            Some(id)
        );
    }
}

mod three_agent_history {
    use super::*;
    use codeg_lib::db::test_helpers;

    fn text_turn(role: &str, text: &str) -> MessageTurn {
        serde_json::from_value(serde_json::json!({
            "id":format!("native-{role}"), "role":role,
            "timestamp":"2026-10-05T00:00:00Z",
            "blocks":[{"type":"text","text":text}]
        })).unwrap()
    }

    #[tokio::test]
    async fn three_agents_tool_failure_correction_and_restart_keep_order_and_unique_turn_ids() {
        let dir=tempfile::tempdir().unwrap();
        let db=test_helpers::fresh_disk_db(dir.path()).await;
        let folder=test_helpers::seed_folder(&db,"/tmp/three-agent-test").await;
        let c=conversation_service::create(&db.conn,folder,AgentType::Codex,Some("Ledger".into()),None).await.unwrap();
        let devin=AgentType::custom("devin").unwrap();
        assert!(supports_handoff(devin));
        assert!(!supports_handoff(AgentType::custom("untested").unwrap()));
        let mut history=vec![text_turn("user","Budget is 137; only half-up decimal rounding is allowed.")];
        let tool:MessageTurn=serde_json::from_value(serde_json::json!({
            "id":"tool-turn","role":"assistant","timestamp":"2026-10-05T00:00:01Z",
            "blocks":[{"type":"tool_result","tool_use_id":"exec1","output_preview":"FAIL duplicate id accepted; exit 1","is_error":true}]
        })).unwrap();
        history.push(tool);
        for (index,(agent,target)) in [(AgentType::Codex,devin),(devin,AgentType::Antigravity),(AgentType::Antigravity,AgentType::Codex),(AgentType::Codex,devin)].into_iter().enumerate() {
            let sid=format!("segment-{index}");
            conversation_service::bind_external_id(&db.conn,c.id,&sid,&[]).await.unwrap();
            let before=conversation_service::get_by_id(&db.conn,c.id).await.unwrap();
            assert_eq!(before.agent_type,agent);
            save_switch(&db.conn,&before,target,history.clone()).await.unwrap();
            let reopened=sea_orm::Database::connect(format!("sqlite:{}",dir.path().join("source.db").display())).await.unwrap();
            let h=load(&reopened,c.id).await.unwrap().unwrap();
            assert_eq!(h.revision,index as i32+1);
            assert!(h.is_retired(agent,&sid));
            assert_eq!(retired_owner(&reopened,agent,&sid).await.unwrap(),Some(c.id));
            assert!(h.context_prefix.contains("exit 1"));
            let request=format!("step {index}: budget is now 149; don't restore 137");
            history=h.merge(vec![text_turn("user",&format!("{}{}",h.context_prefix,request)),text_turn("assistant",&format!("step {index}: duplicate rejected; checks pass"))]);
            assert_eq!(history.len(),4+index*2);
            let ids:std::collections::HashSet<_>=history.iter().map(|t|t.id.clone()).collect();
            assert_eq!(ids.len(),history.len());
            assert!(!format!("{history:?}").contains("CODEG_HANDOFF"));
            assert_eq!(conversation_service::get_by_id(&reopened,c.id).await.unwrap().title.as_deref(),Some("Ledger"));
        }
    }
}

#[test]
fn large_repeated_edit_snapshots_keep_changed_bytes_without_losing_tool_output() {
    let old=format!("{}OLD RULE\n{}", "unchanged prefix line abcdefghijklmnopqrstuvwxyz\n".repeat(800), "unchanged suffix line abcdefghijklmnopqrstuvwxyz\n".repeat(800));
    let new=old.replace("OLD RULE", "NEW RULE");
    let diff=format!("--- ledger.py\n+++ ledger.py\n{}{}",old.split_inclusive('\n').map(|l|format!("-{l}")).collect::<String>(),new.split_inclusive('\n').map(|l|format!("+{l}")).collect::<String>());
    let turn:MessageTurn=serde_json::from_value(serde_json::json!({
        "id":"edits","role":"assistant","timestamp":"2026-10-05T00:00:00Z","blocks":[
            {"type":"tool_use","id":"edit1","tool_name":"Edit","input_preview":serde_json::json!({"file_path":"ledger.py","old_string":old,"new_string":new}).to_string(),"status":"completed"},
            {"type":"tool_result","tool_use_id":"edit1","output_preview":diff,"is_error":false},
            {"type":"tool_result","tool_use_id":"exec1","output_preview":"ASSERTION FAILED: TOOL-123; exit 1","is_error":true}
        ]
    })).unwrap();
    let archived=serde_json::to_string(&turn).unwrap();
    let seed=context_prefix(&[turn]).unwrap();
    assert!(archived.len()>250_000);
    assert!(seed.len()<5_000);
    assert!(seed.contains("OLD RULE"));assert!(seed.contains("NEW RULE"));
    assert!(seed.contains("\\\"start_line\\\":801") || seed.contains("\"start_line\":801"));
    assert!(seed.contains("ASSERTION FAILED: TOOL-123; exit 1"));
}

#[test]
fn oversized_tools_transfer_losslessly_and_cleanup_uncommitted_files() {
    let root = tempfile::tempdir().unwrap();
    let payload = "原始代码：中文与特殊字符\\\"\n".repeat(10_000);
    let turn: MessageTurn = serde_json::from_value(serde_json::json!({
        "id":"large","role":"assistant","timestamp":"2026-10-05T00:00:00Z",
        "blocks":[
            {"type":"text","text":"Preserve this decision verbatim."},
            {"type":"tool_use","tool_use_id":"write","tool_name":"Write","input_preview":payload},
            {"type":"tool_result","tool_use_id":"write","output_preview":payload,"is_error":true}
        ]
    })).unwrap();
    let before = serde_json::to_string(&turn).unwrap();
    let (prefix, guard) = context_with_tool_files(&[turn.clone()], root.path()).unwrap();
    let guard = guard.unwrap();
    let path = guard.path().to_path_buf();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o700);
    }
    assert!(prefix.len() < 10_000);
    assert!(prefix.contains("Preserve this decision verbatim."));
    assert!(prefix.contains("stored_tool_payload"));
    assert!(prefix.contains("\"is_error\":true"));
    for name in ["turn-0-block-1.txt", "turn-0-block-2.txt"] {
        assert_eq!(std::fs::read_to_string(path.join(name)).unwrap(), payload);
        assert!(prefix.contains(name));
    }
    assert_eq!(serde_json::to_string(&turn).unwrap(), before);
    drop(guard);
    assert!(!path.exists());
}

#[test]
fn oversized_user_text_is_not_silently_truncated_and_fallback_cleans_up() {
    let root = tempfile::tempdir().unwrap();
    let turn: MessageTurn = serde_json::from_value(serde_json::json!({
        "id":"large","role":"user","timestamp":"2026-10-05T00:00:00Z",
        "blocks":[{"type":"text","text":"界".repeat(120_000)}]
    })).unwrap();
    assert!(context_with_tool_files(&[turn], root.path()).is_err());
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
}
