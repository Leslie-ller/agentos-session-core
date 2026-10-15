use super::*;
use crate::db::service::conversation_handoff;
use crate::models::message::MessageTurn;

impl ConnectionManager {
    pub(crate) async fn lock_conversation(&self, id: i32) -> tokio::sync::OwnedMutexGuard<()> {
        let lock = {
            let mut locks = self.conversation_locks.lock().await;
            locks
                .entry(id)
                .or_insert_with(|| Arc::new(Mutex::new(())))
                .clone()
        };
        lock.lock_owned().await
    }

    pub(super) async fn prepare_handoff_prompt(
        &self,
        db: &sea_orm::DatabaseConnection,
        conn_id: &str,
        logical_id: Option<i32>,
    ) -> Result<Option<String>, AcpError> {
        let Some(id) = logical_id else {
            return Ok(None);
        };
        let state = self
            .get_state(conn_id)
            .await
            .ok_or_else(|| AcpError::ConnectionNotFound(conn_id.into()))?;
        let (agent, external_id, retired) = {
            let s = state.read().await;
            (s.agent_type, s.external_id.clone(), s.retired_for_handoff)
        };
        let summary = conversation_service::get_by_id(db, id)
            .await
            .map_err(|e| AcpError::protocol(e.to_string()))?;
        if retired || summary.agent_type != agent {
            return Err(AcpError::protocol(
                "Conversation switched to another agent; reload before sending",
            ));
        }
        let Some(handoff) = conversation_handoff::load(db, id)
            .await
            .map_err(|e| AcpError::protocol(e.to_string()))?
        else {
            return Ok(None);
        };
        let sid =
            external_id.ok_or_else(|| AcpError::protocol("Target agent session is not ready"))?;
        if (handoff.is_retired(agent, &sid) && summary.external_id.as_ref() != Some(&sid))
            || summary
                .external_id
                .as_ref()
                .is_some_and(|bound| bound != &sid)
        {
            return Err(AcpError::protocol(
                "This native session no longer owns the conversation; reload",
            ));
        }
        let turns = read_native_turns(agent, sid).await?;
        // Read the native transcript rather than flipping a database 'sent'
        // flag before delivery. A crash between enqueue and persistence cannot
        // lose the seed. If the adapter hasn't flushed yet, repeating historical
        // context is harmless; it never repeats the user's new instruction.
        Ok((!handoff.context_was_recorded(&turns)).then_some(handoff.context_prefix))
    }

    pub async fn switch_conversation_agent(
        &self,
        db: &AppDatabase,
        emitter: EventEmitter,
        id: i32,
        expected_agent: AgentType,
        expected_session_id: Option<String>,
        target: AgentType,
    ) -> Result<(), AcpError> {
        crate::acp::session_profiles::ensure_supported(target)?;
        if !conversation_handoff::supports_handoff(expected_agent)
            || !conversation_handoff::supports_handoff(target)
            || target == expected_agent
        {
            return Err(AcpError::protocol(
                "This prototype supports Codex, Claude Code, Antigravity and Devin",
            ));
        }
        let manager = self.clone_ref();
        let db = db.conn.clone();
        // As with native fork, shield commit + runtime retirement from a web
        // client dropping its HTTP request. No prompts are sent by this task.
        tokio::spawn(async move {
            // Outside the conversation lock: the card is an ordinary turn whose
            // lifecycle events take that lock too.
            let card = manager
                .prepare_handoff_card(&db, id, expected_agent, expected_session_id.as_deref(), target)
                .await;
            let _guard = manager.lock_conversation(id).await;
            let summary = conversation_service::get_by_id(&db, id).await
                .map_err(|e| AcpError::protocol(e.to_string()))?;
            if summary.agent_type != expected_agent || summary.external_id != expected_session_id {
                return Err(AcpError::ConversationChanged("Conversation changed; reload before switching".into()));
            }
            if summary.parent_id.is_some() || summary.child_count > 0
                || !matches!(summary.kind, crate::db::entities::conversation::ConversationKind::Chat
                    | crate::db::entities::conversation::ConversationKind::Regular) {
                return Err(AcpError::protocol("Switching conversations with delegated work is not supported yet"));
            }

            let connections: Vec<_> = {
                let all = manager.connections.lock().await;
                all.iter().map(|(id, c)| (id.clone(), c.state.clone(), c.prompt_lock.clone())).collect()
            };
            let mut retiring = Vec::new();
            let mut prompt_guards = Vec::new();
            for (conn_id, state, lock) in connections {
                let belongs = {
                    let s = state.read().await;
                    s.conversation_id == Some(id) || (s.agent_type == expected_agent
                        && expected_session_id.is_some() && s.external_id == expected_session_id)
                };
                if !belongs { continue }
                prompt_guards.push(lock.lock_owned().await);
                {
                    let s = state.read().await;
                    if s.turn_in_flight || s.status == ConnectionStatus::Prompting
                        || s.status == ConnectionStatus::Connecting
                        || s.pending_permission.is_some() || s.pending_question.is_some()
                        || s.pending_plan_approval.is_some()
                        || s.has_active_background_work(chrono::Utc::now()) {
                        return Err(AcpError::TurnInProgress);
                    }
                }
                retiring.push((conn_id, state));
            }
            // A persisted in-progress row with no live runtime may be a stale
            // crash, but must not be silently treated as a completed segment.
            if summary.status == "in_progress" {
                return Err(AcpError::protocol("Wait for the current turn to finish before switching"));
            }
            if let Some(sid) = &summary.external_id {
                if read_native_turns(expected_agent, sid.clone()).await?.is_empty() {
                    return Err(AcpError::protocol("The current transcript is not available yet; nothing was switched"));
                }
            }
            let (detail, _) = crate::commands::conversations::get_folder_conversation_core(&db, id)
                .await.map_err(|e| AcpError::protocol(e.to_string()))?;
            if detail.summary.agent_type != expected_agent || detail.summary.external_id != expected_session_id {
                return Err(AcpError::ConversationChanged("Conversation changed while waiting to switch; reload and try again".into()));
            }
            // Refuse to archive a transcript that has not caught up with the
            // last reply observed over ACP. Do not silently lose its tail.
            for (_, state) in &retiring {
                if let Some(last) = &state.read().await.last_assistant_text {
                    let persisted = detail.turns.iter().filter(|t| matches!(t.role,
                        crate::models::message::TurnRole::Assistant))
                        .flat_map(|t| t.blocks.iter()).filter_map(|b| match b {
                            crate::models::message::ContentBlock::Text { text } => Some(text.as_str()),
                            _ => None,
                        }).collect::<Vec<_>>().join("\n");
                    if !last.trim().is_empty() && !persisted.contains(last.trim()) {
                        return Err(AcpError::protocol("Waiting for the native transcript to finish saving; try switching again"));
                    }
                }
            }
            let native_turn_count = match &summary.external_id {
                Some(sid) => read_native_turns(expected_agent, sid.clone()).await?.len(),
                None => 0,
            };
            conversation_handoff::save_switch_with_card(&db, &detail.summary, target, detail.turns, Some(native_turn_count), card)
                .await.map_err(|e| match e {
                    crate::db::error::DbError::Conflict(message) => AcpError::ConversationChanged(message),
                    other => AcpError::protocol(other.to_string()),
                })?;
            for (_, state) in &retiring {
                let mut s = state.write().await;
                s.retired_for_handoff = true;
                s.conversation_id = None;
            }
            // Park the old runtime without killing its process. Only the
            // database-selected runtime may send, even from another client.
            let selected = conversation_service::get_by_id(&db, id).await
                .map_err(|e| AcpError::protocol(e.to_string()))?;
            let states: Vec<_> = manager.connections.lock().await.values()
                .map(|c| c.state.clone()).collect();
            for state in states {
                let mut s = state.write().await;
                if s.agent_type == target && selected.external_id.is_some()
                    && s.external_id == selected.external_id {
                    s.retired_for_handoff = false;
                    s.conversation_id = Some(id);
                    s.last_activity_at = chrono::Utc::now();
                }
            }
            crate::commands::conversations::emit_conversation_upsert(&emitter, &db, id).await;
            Ok(())
        }).await.map_err(|e| AcpError::protocol(format!("Agent switch failed: {e}")))?
    }
}

impl ConnectionManager {
    /// Ask the outgoing agent for a handoff card when the history is too long
    /// to travel verbatim. Any failure means no card: the switch then uses the
    /// verbatim seed, exactly as before.
    async fn prepare_handoff_card(
        &self,
        db: &sea_orm::DatabaseConnection,
        id: i32,
        agent: AgentType,
        session_id: Option<&str>,
        target: AgentType,
    ) -> Option<conversation_handoff::HandoffCard> {
        let (detail, _) = crate::commands::conversations::get_folder_conversation_core(db, id)
            .await
            .ok()?;
        if !conversation_handoff::needs_card(db, id, target, &detail.turns)
            .await
            .ok()?
        {
            return None;
        }
        let mut owners = Vec::new();
        for (conn_id, state) in self
            .connections
            .lock()
            .await
            .iter()
            .map(|(k, c)| (k.clone(), c.state.clone()))
            .collect::<Vec<_>>()
        {
            let s = state.read().await;
            if s.conversation_id == Some(id)
                && s.agent_type == agent
                && s.external_id.as_deref() == session_id
                && !s.retired_for_handoff
            {
                owners.push(conn_id);
            }
        }
        let [conn_id] = owners.as_slice() else {
            tracing::info!(conversation_id = id, "[ACP] no single live runtime to write a handoff card");
            return None;
        };
        let text = self.request_handoff_card(db, conn_id).await;
        if text.is_none() {
            tracing::warn!(conversation_id = id, "[ACP] handoff card unavailable; sending verbatim history");
        }
        text.map(|text| conversation_handoff::HandoffCard { author: agent, text })
    }

    async fn request_handoff_card(
        &self,
        db: &sea_orm::DatabaseConnection,
        conn_id: &str,
    ) -> Option<String> {
        const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(180);
        let state = self.get_state(conn_id).await?;
        let (before, previous_text) = {
            let s = state.read().await;
            (s.turns_completed, s.last_assistant_text.clone())
        };
        self.send_prompt_inner(
            conn_id,
            vec![PromptInputBlock::Text { text: conversation_handoff::card_request() }],
            None,
        )
        .await
        .ok()?;
        let deadline = tokio::time::Instant::now() + TIMEOUT;
        let text = loop {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            let Some(state) = self.get_state(conn_id).await else {
                return None;
            };
            let mut s = state.write().await;
            if s.turns_completed != before {
                // The card is plumbing: keep the transcript catch-up check
                // anchored to the last real reply.
                break std::mem::replace(&mut s.last_assistant_text, previous_text);
            }
            drop(s);
            if tokio::time::Instant::now() >= deadline {
                let _ = self.cancel(db, conn_id).await;
                return None;
            }
        };
        text.filter(|t| !t.trim().is_empty())
    }
}

async fn read_native_turns(agent: AgentType, sid: String) -> Result<Vec<MessageTurn>, AcpError> {
    tokio::task::spawn_blocking(move || {
        match crate::parsers::build_agent_parser(agent).get_conversation(&sid) {
            Ok(detail) => Ok(detail.turns),
            Err(crate::parsers::ParseError::ConversationNotFound(_)) => Ok(Vec::new()),
            Err(e) => Err(AcpError::protocol(format!(
                "Cannot read native session for handoff: {e}"
            ))),
        }
    })
    .await
    .map_err(|e| AcpError::protocol(e.to_string()))?
}
