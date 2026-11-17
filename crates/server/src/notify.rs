//! Notification policy (decision 16): the server decides who is notified; clients decide how
//! to present it. Runs as a job after a message commits, and re-checks access before sending.
use std::collections::HashSet;

use proto::events::UserEventBody;
use proto::model::{
    ConversationKind, Entity, Message, Notification, NotifyLevel, NotifyReason, Preferences,
};
use uuid::Uuid;

use crate::sqlx;
use crate::{AppState, conversations};

pub const JOB: &str = "notify";

/// What the policy needs to know about a message.
pub struct MessageFacts<'a> {
    pub author: Uuid,
    pub kind: ConversationKind,
    pub conversation_id: Uuid,
    pub body: &'a str,
    pub entities: &'a [Entity],
    /// Root author and everyone who replied, when the message is a thread reply.
    pub thread_participants: Option<&'a HashSet<Uuid>>,
}

/// Decides whether `recipient` is notified, and why. Pure, so it is unit-tested directly.
pub fn decide(m: &MessageFacts, recipient: Uuid, prefs: &Preferences) -> Option<NotifyReason> {
    if recipient == m.author {
        return None;
    }
    let mentioned = m
        .entities
        .iter()
        .any(|e| matches!(e, Entity::Mention { user_id, .. } if *user_id == recipient));
    if mentioned {
        return Some(NotifyReason::Mention);
    }
    if prefs.muted.contains(&m.conversation_id) {
        return None;
    }
    if matches!(m.kind, ConversationKind::Dm | ConversationKind::GroupDm) {
        return Some(NotifyReason::DirectMessage);
    }
    if let Some(p) = m.thread_participants
        && p.contains(&recipient)
    {
        return Some(NotifyReason::ThreadReply);
    }
    let lower = m.body.to_lowercase();
    let words: HashSet<&str> = lower
        .split(|c: char| !c.is_alphanumeric() && c != '_' && c != '-')
        .filter(|w| !w.is_empty())
        .collect();
    if prefs
        .keywords
        .iter()
        .any(|k| words.contains(k.trim().to_lowercase().as_str()))
    {
        return Some(NotifyReason::Keyword);
    }
    if prefs.channel_level == NotifyLevel::All {
        return Some(NotifyReason::AllMessages);
    }
    None
}

pub async fn enqueue(conn: &mut sqlx::PgConnection, message: &Message) -> sqlx::Result<()> {
    crate::jobs::enqueue(conn, JOB, serde_json::json!({ "message_id": message.id }))
        .await
        .map(|_| ())
}

fn preview(body: &str) -> String {
    let text = proto::markdown::plain_text(body);
    match text.char_indices().nth(140) {
        Some((i, _)) => format!("{}…", &text[..i]),
        None => text,
    }
}

/// The notify job: decide recipients, re-check their access now, and emit each notification
/// once per message and user.
pub async fn run(state: &AppState, payload: &serde_json::Value) -> anyhow::Result<()> {
    let id: Uuid = serde_json::from_value(payload["message_id"].clone())?;
    let mut conn = state.pool.acquire().await?;
    let message = match crate::messages::load_one(&mut conn, id).await {
        Ok(m) => m,
        Err(crate::error::ApiError::NotFound) => return Ok(()),
        Err(e) => return Err(anyhow::anyhow!("{e}")),
    };
    if message.deleted {
        return Ok(());
    }
    let conv = conversations::load(&mut conn, message.conversation_id)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let participants: Option<HashSet<Uuid>> = match message.thread_root_id {
        Some(root) => Some(
            sqlx::query_scalar::<_, Uuid>(
                "SELECT author_id FROM messages WHERE id = $1 OR thread_root_id = $1",
            )
            .bind(root)
            .fetch_all(&mut *conn)
            .await?
            .into_iter()
            .collect(),
        ),
        None => None,
    };
    let recipients: Vec<(Uuid, serde_json::Value)> = sqlx::query_as(
        "SELECT DISTINCT u.id, u.preferences FROM access_intervals ai JOIN users u ON u.id = ai.user_id
          WHERE ai.conversation_id = $1 AND ai.left_seq IS NULL AND u.id <> $2",
    )
    .bind(message.conversation_id)
    .bind(message.author_id)
    .fetch_all(&mut *conn)
    .await?;
    let author_name: String = sqlx::query_scalar(
        "SELECT display_name FROM workspace_members WHERE workspace_id = $1 AND user_id = $2",
    )
    .bind(message.workspace_id)
    .bind(message.author_id)
    .fetch_optional(&mut *conn)
    .await?
    .unwrap_or_else(|| "Someone".into());
    let facts = MessageFacts {
        author: message.author_id,
        kind: conv.kind,
        conversation_id: conv.id,
        body: &message.body,
        entities: &message.entities,
        thread_participants: participants.as_ref(),
    };
    drop(conn);
    for (user, prefs) in recipients {
        let prefs: Preferences = serde_json::from_value(prefs).unwrap_or_default();
        let Some(reason) = decide(&facts, user, &prefs) else {
            continue;
        };
        let mut tx = state.pool.begin().await?;
        // Access may have changed since the message was sent; the check fails toward deny.
        if !conversations::can_read_now(&mut tx, message.conversation_id, user).await {
            continue;
        }
        let fresh = sqlx::query(
            "INSERT INTO notify_sent (user_id, message_id) VALUES ($1, $2) ON CONFLICT DO NOTHING",
        )
        .bind(user)
        .bind(message.id)
        .execute(&mut *tx)
        .await?
        .rows_affected()
            == 1;
        if !fresh {
            continue;
        }
        crate::sync::append_user(
            &mut tx,
            user,
            vec![UserEventBody::Notify {
                notification: Notification {
                    message_id: message.id,
                    workspace_id: message.workspace_id,
                    conversation_id: message.conversation_id,
                    author_id: message.author_id,
                    author_name: author_name.clone(),
                    reason,
                    preview: preview(&message.body),
                },
            }],
        )
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
        tx.commit().await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts<'a>(
        kind: ConversationKind,
        body: &'a str,
        entities: &'a [Entity],
    ) -> MessageFacts<'a> {
        MessageFacts {
            author: Uuid::from_u128(1),
            kind,
            conversation_id: Uuid::from_u128(100),
            body,
            entities,
            thread_participants: None,
        }
    }

    const BOB: Uuid = Uuid::from_u128(2);

    #[test]
    fn author_is_never_notified() {
        let f = facts(ConversationKind::Dm, "hi", &[]);
        assert_eq!(
            decide(&f, Uuid::from_u128(1), &Preferences::default()),
            None
        );
    }

    #[test]
    fn mentions_notify_even_when_muted() {
        let e = [Entity::Mention {
            user_id: BOB,
            start: 0,
            end: 4,
        }];
        let f = facts(ConversationKind::Public, "@bob", &e);
        let muted = Preferences {
            muted: vec![Uuid::from_u128(100)],
            ..Default::default()
        };
        assert_eq!(decide(&f, BOB, &muted), Some(NotifyReason::Mention));
    }

    #[test]
    fn dms_notify_unless_muted() {
        let f = facts(ConversationKind::GroupDm, "hey", &[]);
        assert_eq!(
            decide(&f, BOB, &Preferences::default()),
            Some(NotifyReason::DirectMessage)
        );
        let muted = Preferences {
            muted: vec![Uuid::from_u128(100)],
            ..Default::default()
        };
        assert_eq!(decide(&f, BOB, &muted), None);
    }

    #[test]
    fn channels_default_to_mentions_only() {
        let f = facts(ConversationKind::Public, "deploy is done", &[]);
        assert_eq!(decide(&f, BOB, &Preferences::default()), None);
        let all = Preferences {
            channel_level: NotifyLevel::All,
            ..Default::default()
        };
        assert_eq!(decide(&f, BOB, &all), Some(NotifyReason::AllMessages));
        let nothing = Preferences {
            channel_level: NotifyLevel::Nothing,
            ..Default::default()
        };
        assert_eq!(decide(&f, BOB, &nothing), None);
    }

    #[test]
    fn keywords_match_whole_words_case_insensitively() {
        let prefs = Preferences {
            keywords: vec!["Deploy".into()],
            ..Default::default()
        };
        assert_eq!(
            decide(
                &facts(ConversationKind::Public, "the DEPLOY failed", &[]),
                BOB,
                &prefs
            ),
            Some(NotifyReason::Keyword)
        );
        assert_eq!(
            decide(
                &facts(ConversationKind::Public, "redeployed", &[]),
                BOB,
                &prefs
            ),
            None
        );
    }

    #[test]
    fn thread_participants_hear_replies() {
        let p: HashSet<Uuid> = [BOB].into_iter().collect();
        let mut f = facts(ConversationKind::Public, "reply", &[]);
        f.thread_participants = Some(&p);
        assert_eq!(
            decide(&f, BOB, &Preferences::default()),
            Some(NotifyReason::ThreadReply)
        );
        assert_eq!(
            decide(&f, Uuid::from_u128(3), &Preferences::default()),
            None
        );
    }

    #[test]
    fn previews_are_plain_text_and_bounded() {
        assert_eq!(preview("**hi** <b>x</b>"), "hi <b>x</b>");
        assert_eq!(preview(&"a".repeat(500)).chars().count(), 141);
    }
}
