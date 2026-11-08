//! Secret handoffs. Secret-bearing JSON errors are deliberately generic.
use super::tasks::{insert_activity, task_by_ref, task_detail};
use super::{now_millis, require_human, ApiError, Auth};
use crate::{
    adapter::openchamber::{PromptDelivery, SEND_TIMEOUT},
    auth::{constant_time_eq, Scope},
    ids,
    secret_memory::StoreError,
    server::AppState,
};
use axum::{
    extract::{rejection::JsonRejection, Path, State},
    Json,
};
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::time::{Duration, Instant};

const REQUEST_MS: i64 = 24 * 60 * 60 * 1000;
const VALUE_MS: i64 = 15 * 60 * 1000;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Create {
    label: String,
    purpose: String,
    id: Option<String>,
    capability: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Submit {
    value: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Consume {
    capability: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Decline {
    explanation: Option<String>,
}

fn body<T>(body: Result<Json<T>, JsonRejection>) -> Result<T, ApiError> {
    body.map(|Json(v)| v)
        .map_err(|e| ApiError::rejection(e.status(), "invalid request body"))
}
fn store_error(error: StoreError) -> ApiError {
    match error {
        StoreError::Capacity => ApiError::service_unavailable(
            "capacity",
            "secret handoff capacity reached; retry later",
        ),
        StoreError::InvalidValue => {
            ApiError::validation("value must be non-empty UTF-8, at most 16 KiB, without NUL")
        }
        _ => ApiError::conflict("secret handoff unavailable"),
    }
}

#[derive(Serialize)]
pub(crate) struct Metadata {
    id: String,
    task_id: String,
    label: String,
    purpose: String,
    state: String,
    created_at: i64,
    deadline: i64,
    value_deadline: Option<i64>,
    explanation: Option<String>,
    delivery_status: Option<String>,
    delivery_error: Option<String>,
    invalidation_reason: Option<String>,
}
fn metadata(row: &rusqlite::Row<'_>) -> rusqlite::Result<Metadata> {
    Ok(Metadata {
        id: row.get(0)?,
        task_id: row.get(1)?,
        label: row.get(2)?,
        purpose: row.get(3)?,
        state: row.get(4)?,
        created_at: row.get(5)?,
        deadline: row.get(6)?,
        value_deadline: row.get(7)?,
        explanation: row.get(8)?,
        delivery_status: row.get(9)?,
        invalidation_reason: row.get(10)?,
        delivery_error: row.get(11)?,
    })
}
const COLUMNS: &str = "id, task_id, label, purpose, state, created_at, deadline, value_deadline, explanation, delivery_status, invalidation_reason, delivery_error";

pub async fn create(
    State(state): State<AppState>,
    Auth(identity): Auth,
    Path(reference): Path<String>,
    payload: Result<Json<Create>, JsonRejection>,
) -> Result<Json<Value>, ApiError> {
    if identity.scope != Scope::Agent {
        return Err(ApiError::forbidden("agent token required"));
    }
    let token = identity
        .token_id
        .clone()
        .ok_or_else(|| ApiError::forbidden("agent token required"))?;
    if state.openchamber().is_none() {
        return Err(ApiError::service_unavailable(
            "integration_disabled",
            "OpenChamber integration is disabled",
        ));
    }
    let payload = body(payload)?;
    if payload.label.trim().is_empty()
        || payload.label.chars().count() > 120
        || payload.purpose.chars().count() > 2000
    {
        return Err(ApiError::validation("invalid label or purpose"));
    }
    // The CLI provisions its private file before creating a visible blocker.
    // A lost response then cannot strand an unidentifiable request. Direct API
    // callers can still request a server-generated pair.
    let (id, capability) = match (&payload.id, &payload.capability) {
        (Some(id), Some(capability))
            if ids::is_uuid(id)
                && capability.len() == 43
                && ids::decode_base64_url(capability).is_some_and(|bytes| bytes.len() == 32) =>
        {
            (id.clone(), capability.clone())
        }
        (None, None) => (
            ids::uuid_v4()?,
            ids::base64_url(&ids::random_bytes::<32>()?),
        ),
        _ => return Err(ApiError::validation("invalid private request provisioning")),
    };
    let verifier = ids::sha256_hex(&capability);
    let request_id = id.clone();
    let published_id = id.clone();
    let memory = state.secrets.clone();
    let actor = identity.actor().to_owned();
    let outcome = state.db.call(move |db| {
        let tx = db.transaction()?;
        let duplicate: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM secret_requests WHERE id=?1)",
            [&request_id], |row| row.get(0),
        )?;
        if duplicate {
            return Ok(Err(ApiError::conflict("request ID already exists")));
        }
        let token_live: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM tokens WHERE id=?1 AND scope='agent' AND revoked_at IS NULL)",
            [&token], |row| row.get(0),
        )?;
        if !token_live {
            return Ok(Err(ApiError::forbidden("agent token is revoked")));
        }
        let Some(task) = task_by_ref(&tx, &reference)? else {
            return Ok(Err(ApiError::not_found("task not found")));
        };
        if task.state != "active" || task.session_id.is_none() {
            return Ok(Err(ApiError::conflict("active card and linked session required")));
        }
        if task.status != "in_progress" {
            if task.status != "blocker" {
                return Ok(Err(ApiError::conflict("card must be in progress")));
            }
            if !replacement_allowed(&tx, &task, &token)? {
                return Ok(Err(ApiError::conflict("existing blocker or live handoff")));
            }
        }
        let live: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM secret_requests WHERE task_id=?1 AND state IN ('waiting','ready'))",
            [&task.id], |row| row.get(0),
        )?;
        if live {
            return Ok(Err(ApiError::conflict("live handoff already exists")));
        }
        let now = now_millis();
        let instant = Instant::now();
        if let Err(error) = memory.reserve(&request_id, instant + Duration::from_millis(REQUEST_MS as u64), instant) {
            return Ok(Err(store_error(error)));
        }
        let result = (|| -> crate::error::Result<()> {
            if task.status == "in_progress" {
                tx.execute(
                    "UPDATE tasks SET status='blocker',blocked_from='in_progress',blocked_reason=?1,updated_at=?2 WHERE id=?3",
                    params![payload.label, now, task.id],
                )?;
                insert_activity(&tx, &task.id, &actor, "blocked", &json!({"reason":payload.label}))?;
            }
            tx.execute(
                "INSERT INTO secret_requests
                 (id,task_id,token_id,session_id,session_directory,label,purpose,state,verifier,created_at,deadline)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,'waiting',?8,?9,?10)",
                params![request_id,task.id,token,task.session_id,task.session_directory,payload.label,payload.purpose,verifier,now,now+REQUEST_MS],
            )?;
            insert_activity(&tx, &task.id, &actor, "secret_requested", &json!({"request_id":request_id}))?;
            tx.commit()?;
            Ok(())
        })();
        if result.is_err() {
            memory.remove(&request_id);
        }
        result?;
        Ok(Ok(task.id))
    }).await?;
    let task_id = outcome?;
    publish_change(&state, &task_id, &published_id).await?;
    Ok(Json(json!({"id":id,"capability":capability})))
}

/// Only the latest terminal request can justify asking again while blocked.
/// Any intervening ordinary blocker question or session change closes this
/// exception, even if the visible session later changes back to the old one.
fn replacement_allowed(
    db: &rusqlite::Connection,
    task: &super::model::Task,
    token: &str,
) -> crate::error::Result<bool> {
    Ok(db.query_row(
        "SELECT EXISTS(
           SELECT 1 FROM secret_requests r
           WHERE r.task_id=?1
             AND r.rowid=(SELECT MAX(rowid) FROM secret_requests WHERE task_id=?1)
             AND r.token_id=?2 AND r.session_id=?3 AND r.session_directory IS ?4
             AND r.state NOT IN ('waiting','ready')
             AND r.invalidation_reason IS NOT 'session_changed'
             AND NOT EXISTS(
               SELECT 1 FROM activity a WHERE a.task_id=?1
                 AND a.rowid > COALESCE((
                   SELECT rowid FROM activity WHERE task_id=?1 AND kind='secret_requested'
                     AND json_extract(data,'$.request_id')=r.id ORDER BY rowid DESC LIMIT 1
                 ),0)
                 AND (a.kind='blocked' OR (a.kind='moved' AND json_type(a.data,'$.reason') IS NOT NULL))
             )
         )",
        params![task.id,token,task.session_id,task.session_directory],
        |row| row.get(0),
    )?)
}

pub async fn list(
    State(state): State<AppState>,
    _auth: Auth,
    Path(reference): Path<String>,
) -> Result<Json<Vec<Metadata>>, ApiError> {
    let data = state
        .db
        .call(move |db| {
            let Some(task) = task_by_ref(db, &reference)? else {
                return Ok(None);
            };
            let mut stmt = db.prepare(&format!(
                "SELECT {COLUMNS} FROM secret_requests WHERE task_id=?1 ORDER BY rowid DESC"
            ))?;
            let rows = stmt
                .query_map([task.id], metadata)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(Some(rows))
        })
        .await?;
    Ok(Json(
        data.ok_or_else(|| ApiError::not_found("task not found"))?,
    ))
}
pub async fn status(
    State(state): State<AppState>,
    _auth: Auth,
    Path(id): Path<String>,
) -> Result<Json<Metadata>, ApiError> {
    let result = state
        .db
        .call(move |db| {
            Ok(db
                .query_row(
                    &format!("SELECT {COLUMNS} FROM secret_requests WHERE id=?1"),
                    [id],
                    metadata,
                )
                .optional()?)
        })
        .await?;
    Ok(Json(
        result.ok_or_else(|| ApiError::not_found("request not found"))?,
    ))
}

// Validity is checked again inside the serialized connection for every operation.
type LiveRequest = (String, String, Option<String>, String, i64);
fn live(db: &rusqlite::Connection, id: &str) -> crate::error::Result<Option<LiveRequest>> {
    Ok(db.query_row("SELECT r.state,r.token_id,r.verifier,r.task_id,r.deadline FROM secret_requests r JOIN tasks t ON t.id=r.task_id JOIN tokens k ON k.id=r.token_id WHERE r.id=?1 AND t.state='active' AND t.status='blocker' AND t.session_id=r.session_id AND t.session_directory IS r.session_directory AND k.revoked_at IS NULL AND r.deadline>?2 AND (r.value_deadline IS NULL OR r.value_deadline>?2)", params![id,now_millis()], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?))).optional()?)
}
pub async fn submit(
    State(state): State<AppState>,
    Auth(identity): Auth,
    Path(id): Path<String>,
    payload: Result<Json<Submit>, JsonRejection>,
) -> Result<Json<Value>, ApiError> {
    require_human(&identity)?;
    let payload = body(payload)?;
    let memory = state.secrets.clone();
    let request_id = id.clone();
    let actor = identity.actor().to_owned();
    let outcome = state
        .db
        .call(move |db| {
            let tx = db.transaction()?;
            let Some((status, _, _, task_id, deadline)) = live(&tx, &id)? else {
                memory.remove(&id);
                return Ok(Err(ApiError::conflict("handoff unavailable")));
            };
            if status != "waiting" {
                return Ok(Err(ApiError::conflict("handoff not waiting")));
            }
            let now = now_millis();
            let until = (now + VALUE_MS).min(deadline);
            let instant = Instant::now();
            if let Err(e) = memory.submit(
                &id,
                payload.value,
                instant + Duration::from_millis((until - now) as u64),
                instant,
            ) {
                return Ok(Err(store_error(e)));
            }
            let saved = (|| -> crate::error::Result<()> {
                tx.execute(
                    "UPDATE secret_requests SET state='ready', value_deadline=?1 WHERE id=?2",
                    params![until, id],
                )?;
                prepare_notification(&tx, &id, &task_id, "ready")?;
                insert_activity(
                    &tx,
                    &task_id,
                    &actor,
                    "secret_submitted",
                    &json!({"request_id": id}),
                )?;
                tx.commit()?;
                Ok(())
            })();
            if let Err(e) = saved {
                memory.remove(&id);
                return Err(e);
            }
            Ok(Ok(()))
        })
        .await?;
    outcome?;
    publish_request(&state, &request_id).await?;
    let delivery = notify_after_accept(&state, &request_id, identity.actor()).await;
    Ok(Json(
        json!({"status":"ready", "delivery_status":delivery.status, "delivery_error":delivery.error}),
    ))
}
pub async fn consume(
    State(state): State<AppState>,
    Auth(identity): Auth,
    Path(id): Path<String>,
    payload: Result<Json<Consume>, JsonRejection>,
) -> Result<Json<Value>, ApiError> {
    if identity.scope != Scope::Agent {
        return Err(ApiError::forbidden("agent token required"));
    }
    let token = identity.token_id.unwrap_or_default();
    let actor = identity.name.clone();
    let capability = body(payload)?.capability;
    let memory = state.secrets.clone();
    let request_id = id.clone();
    let outcome = state
        .db
        .call(move |db| {
            let Some((status, owner, verifier, task_id, _)) = live(db, &id)? else {
                memory.remove(&id);
                return Ok(Err(ApiError::conflict("handoff unavailable")));
            };
            let hash = ids::sha256_hex(&capability);
            if owner != token
                || !verifier
                    .as_deref()
                    .is_some_and(|v| constant_time_eq(v.as_bytes(), hash.as_bytes()))
            {
                return Ok(Err(ApiError::forbidden("invalid handoff authorization")));
            }
            if status != "ready" {
                return Ok(Err(ApiError::conflict("handoff not ready")));
            }
            let tx = db.transaction()?;
            tx.execute(
                "UPDATE secret_requests SET state='consumed',verifier=NULL WHERE id=?1",
                [&id],
            )?;
            insert_activity(
                &tx,
                &task_id,
                &actor,
                "secret_consumed",
                &json!({"request_id":id}),
            )?;
            tx.commit()?;
            let secret = match memory.take(&id, Instant::now()) {
                Ok(s) => s,
                Err(e) => return Ok(Err(store_error(e))),
            };
            Ok(Ok(secret))
        })
        .await?;
    let secret = outcome?;
    publish_request(&state, &request_id).await?;
    let value = std::str::from_utf8(secret.as_bytes())
        .map_err(|_| ApiError::internal("invalid stored value"))?;
    Ok(Json(json!({"value":value})))
}
pub async fn cancel(
    State(state): State<AppState>,
    Auth(identity): Auth,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let memory = state.secrets.clone();
    let request_id = id.clone();
    let outcome = state
        .db
        .call(move |db| {
            let Some((status, owner, _, task_id, _)) = live(db, &id)? else {
                memory.remove(&id);
                return Ok(Err(ApiError::conflict("handoff unavailable")));
            };
            if !identity.is_human() && identity.token_id.as_deref() != Some(&owner) {
                return Ok(Err(ApiError::forbidden("original agent token required")));
            }
            if status != "waiting" && status != "ready" {
                return Ok(Err(ApiError::conflict("handoff already closed")));
            }
            let tx = db.transaction()?;
            tx.execute(
                "UPDATE secret_requests SET state='cancelled',verifier=NULL WHERE id=?1",
                [&id],
            )?;
            insert_activity(
                &tx,
                &task_id,
                identity.actor(),
                "secret_cancelled",
                &json!({"request_id":id}),
            )?;
            tx.commit()?;
            memory.remove(&id);
            Ok(Ok(()))
        })
        .await?;
    outcome?;
    publish_request(&state, &request_id).await?;
    Ok(Json(json!({"status":"cancelled"})))
}
pub async fn decline(
    State(state): State<AppState>,
    Auth(identity): Auth,
    Path(id): Path<String>,
    payload: Result<Json<Decline>, JsonRejection>,
) -> Result<Json<Value>, ApiError> {
    require_human(&identity)?;
    let payload = body(payload)?;
    if payload
        .explanation
        .as_ref()
        .is_some_and(|s| s.chars().count() > 2000)
    {
        return Err(ApiError::validation("explanation too long"));
    }
    let memory = state.secrets.clone();
    let request_id = id.clone();
    let actor = identity.actor().to_owned();
    let outcome=state.db.call(move |db| {
        let tx = db.transaction()?;
        let Some((status,_,_,task_id,_))=live(&tx,&id)? else { memory.remove(&id); return Ok(Err(ApiError::conflict("handoff unavailable"))); };
        if status!="waiting" { return Ok(Err(ApiError::conflict("handoff not waiting"))); }
        tx.execute("UPDATE secret_requests SET state='declined',verifier=NULL,explanation=?1 WHERE id=?2",params![payload.explanation,id])?;
        prepare_notification(&tx, &id, &task_id, "declined")?;
        insert_activity(&tx, &task_id, &actor, "secret_declined", &json!({"request_id":id}))?;
        tx.commit()?;
        memory.remove(&id); Ok(Ok(()))
    }).await?;
    outcome?;
    publish_request(&state, &request_id).await?;
    let delivery = notify_after_accept(&state, &request_id, identity.actor()).await;
    Ok(Json(
        json!({"status":"declined", "delivery_status":delivery.status, "delivery_error":delivery.error}),
    ))
}
fn prepare_notification(
    db: &rusqlite::Connection,
    id: &str,
    task_id: &str,
    outcome: &str,
) -> crate::error::Result<()> {
    let reference = task_by_ref(db, task_id)?
        .ok_or_else(|| crate::error::Error::msg("secret request task missing"))?
        .reference;
    // Fixed, non-secret text only: even the human's decline explanation stays
    // on the card. Never use the ordinary comment/reply persistence path.
    let prompt = format!(
        "Secret handoff on {reference} (request {id}): {outcome}.\n\
         Inspect with `kanso secret status {id}`. Use your private local capability \
         with `kanso secret exec` if ready; never print a credential.\n\
         This is metadata only. Ignore duplicate notifications for this request \
         and outcome. Continue and move the card out of blocker when unblocked."
    );
    db.execute(
        "UPDATE secret_requests SET delivery_status='unknown', delivery_error=NULL, delivery_prompt=?1 WHERE id=?2",
        params![prompt, id],
    )?;
    Ok(())
}

#[derive(Serialize)]
pub struct Delivery {
    status: &'static str,
    error: Option<&'static str>,
}

struct Notification {
    task_id: String,
    session: String,
    directory: Option<String>,
    prompt: String,
}

/// Retry only an already stored, reference-only notification. This never
/// resubmits a value or extends its deadlines.
pub async fn retry(
    State(state): State<AppState>,
    Auth(identity): Auth,
    Path(id): Path<String>,
) -> Result<Json<Delivery>, ApiError> {
    require_human(&identity)?;
    Ok(Json(notify(&state, &id, identity.actor()).await?))
}

async fn notify(state: &AppState, id: &str, actor: &str) -> Result<Delivery, ApiError> {
    let key = format!("secret:{id}");
    if !state.reserve_dispatch(&key) {
        return Err(ApiError::conflict("notification already in flight"));
    }
    // Own the reservation in the spawned task so a disconnected browser cannot
    // abandon delivery bookkeeping or allow a concurrent duplicate send.
    let worker_state = state.clone();
    let worker_id = id.to_owned();
    let actor = actor.to_owned();
    let worker = tokio::spawn(async move {
        let result = deliver_notification(&worker_state, &worker_id, &actor).await;
        worker_state.release_dispatch(&key);
        result
    });
    worker
        .await
        .map_err(|_| ApiError::internal("notification worker failed"))?
}

async fn notify_after_accept(state: &AppState, id: &str, actor: &str) -> Delivery {
    // Submission/decline has already committed. A cancelled handoff, upstream
    // outage or bookkeeping failure must not make the UI resubmit the value.
    notify(state, id, actor).await.unwrap_or(Delivery {
        status: "unknown",
        error: Some("Notification unavailable; inspect the request status before retrying."),
    })
}

async fn deliver_notification(
    state: &AppState,
    id: &str,
    actor: &str,
) -> Result<Delivery, ApiError> {
    let request_id = id.to_owned();
    let notification = state.db.call(move |db| {
        // A decline is terminal for retrieval but can still notify the original
        // session until its request deadline. Consumed/cancelled requests cannot.
        let row = db.query_row(
            "SELECT r.task_id,r.session_id,r.session_directory,r.delivery_prompt,r.delivery_status
             FROM secret_requests r JOIN tasks t ON t.id=r.task_id JOIN tokens k ON k.id=r.token_id
             WHERE r.id=?1 AND r.state IN ('ready','declined') AND r.deadline>?2
               AND (r.value_deadline IS NULL OR r.value_deadline>?2)
               AND t.state='active' AND t.status='blocker'
               AND t.session_id=r.session_id AND t.session_directory IS r.session_directory
               AND k.revoked_at IS NULL AND r.invalidation_reason IS NULL
               AND NOT EXISTS(SELECT 1 FROM secret_requests newer WHERE newer.task_id=r.task_id AND newer.rowid>r.rowid)",
            params![request_id, now_millis()],
            |row| Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?,row.get::<_,Option<String>>(2)?,row.get::<_,Option<String>>(3)?,row.get::<_,Option<String>>(4)?)),
        ).optional()?;
        let Some((task_id,session,directory,Some(prompt),status)) = row else {
            return Ok(Err(ApiError::conflict("notification unavailable")));
        };
        if status.as_deref()==Some("sent") {
            return Ok(Err(ApiError::conflict_code("already_sent", "notification already delivered")));
        }
        db.execute("UPDATE secret_requests SET delivery_status='unknown',delivery_error=NULL WHERE id=?1", [&request_id])?;
        Ok(Ok(Notification { task_id, session, directory, prompt }))
    }).await??;

    let outcome = if let Some(adapter) = state.openchamber() {
        adapter
            .send_prompt(
                &notification.session,
                notification.directory.as_deref().unwrap_or_default(),
                &notification.prompt,
                SEND_TIMEOUT,
            )
            .await
    } else {
        PromptDelivery::Failed(String::new())
    };
    // Never store, log or echo upstream error bodies: they are untrusted and
    // may echo unrelated credentials. Classify with constant, safe messages.
    let delivery = match outcome {
        PromptDelivery::Sent => Delivery {
            status: "sent",
            error: None,
        },
        PromptDelivery::Failed(_) => Delivery {
            status: "failed",
            error: Some("Notification was not delivered."),
        },
        PromptDelivery::Unknown(_) => Delivery {
            status: "unknown",
            error: Some("Delivery is uncertain; retry may send a duplicate notification."),
        },
    };
    let status = delivery.status;
    let error = delivery.error;
    let request_id = id.to_owned();
    let actor = actor.to_owned();
    let task_id = notification.task_id.clone();
    state.db.call(move |db| {
        let tx = db.transaction()?;
        tx.execute("UPDATE secret_requests SET delivery_status=?1,delivery_error=?2 WHERE id=?3", params![status,error,request_id])?;
        if status=="sent" {
            tx.execute("UPDATE tasks SET session_status='running',attention=0,updated_at=?1 WHERE id=?2 AND state='active' AND status='blocker' AND session_id=?3 AND session_directory IS ?4", params![now_millis(),task_id,notification.session,notification.directory])?;
        }
        insert_activity(&tx, &task_id, &actor, "secret_notification", &json!({"request_id":request_id,"delivery_status":status}))?;
        tx.commit()?;
        Ok(())
    }).await?;
    publish_change(state, &notification.task_id, id).await?;
    Ok(delivery)
}

async fn publish_request(state: &AppState, id: &str) -> Result<(), ApiError> {
    let request_id = id.to_owned();
    let task_id = state
        .db
        .call(move |db| {
            Ok(db
                .query_row(
                    "SELECT task_id FROM secret_requests WHERE id=?1",
                    [request_id],
                    |row| row.get::<_, String>(0),
                )
                .optional()?)
        })
        .await?;
    if let Some(task_id) = task_id {
        publish_change(state, &task_id, id).await?;
    }
    Ok(())
}

async fn publish_change(state: &AppState, task_id: &str, id: &str) -> Result<(), ApiError> {
    let task_key = task_id.to_owned();
    let task = state
        .db
        .call(move |db| {
            task_by_ref(db, &task_key)?
                .map(|task| task_detail(db, task).map(|detail| detail.task))
                .transpose()
        })
        .await?;
    state.publish("secret.updated", json!({"task_id":task_id,"request_id":id}));
    if let Some(task) = task {
        state.publish("task.updated", json!({"task":task}));
    }
    Ok(())
}

pub fn spawn_cleanup(state: AppState) {
    tokio::spawn(async move {
        loop {
            // Schedule the actual nearest deadline, not a one-second polling
            // interval. A newly created or shortened deadline wakes the loop.
            let changed = state.secrets.changed();
            tokio::pin!(changed);
            if let Some(deadline) = state.secrets.next_deadline() {
                tokio::select! {
                    () = tokio::time::sleep_until(deadline.into()) => {},
                    () = &mut changed => {},
                }
            } else {
                changed.await;
            }
            if let Err(error) = state.db.call(|_| Ok(())).await {
                // Database errors cannot contain secret values because no
                // value is ever used in SQL. Still keep this path generic.
                let _ = error;
                tracing::error!("secret cleanup failed");
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            for (id, task_id) in state.db.take_secret_changes() {
                if publish_change(&state, &task_id, &id).await.is_err() {
                    tracing::error!("secret status publication failed");
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{auth::AuthConfig, db::Database, test_support::TempDir};

    #[tokio::test]
    async fn deadline_worker_reschedules_and_publishes_expiry_without_traffic() {
        let dir = TempDir::new();
        let db = Database::open(&dir.path().join("kanso.db")).unwrap();
        let auth = AuthConfig::load("test-password", None, dir.path()).unwrap();
        let state = AppState::new(db, dir.path().to_path_buf(), auth);
        let memory = state.secrets.clone();
        state.db.call(move |db| {
            let now = now_millis();
            db.execute("INSERT INTO projects (id,key,name,position,created_at,updated_at) VALUES ('project','EXP','Expiry',0,?1,?1)", [now])?;
            db.execute("INSERT INTO tasks (id,project_id,number,title,status,rank,session_id,created_at,updated_at) VALUES ('00000000-0000-4000-8000-000000000001','project',1,'Expiry','blocker',0,'session',?1,?1)", [now])?;
            db.execute("INSERT INTO tokens (id,name,scope,hash,created_at) VALUES ('token','agent','agent','hash',?1)", [now])?;
            db.execute("INSERT INTO secret_requests (id,task_id,token_id,session_id,label,purpose,state,verifier,created_at,deadline) VALUES ('request','00000000-0000-4000-8000-000000000001','token','session','Expiry','test','waiting','verifier',?1,?2)", params![now,now+3_600_000])?;
            memory.reserve("request", Instant::now()+Duration::from_secs(3600), Instant::now()).unwrap();
            Ok(())
        }).await.unwrap();
        let mut events = state.events.subscribe();
        spawn_cleanup(state.clone());
        let memory = state.secrets.clone();
        state
            .db
            .call(move |db| {
                let now = now_millis();
                db.execute(
                    "UPDATE secret_requests SET state='ready',value_deadline=?1 WHERE id='request'",
                    [now + 80],
                )?;
                memory
                    .submit(
                        "request",
                        "short-lived-sentinel".into(),
                        Instant::now() + Duration::from_millis(80),
                        Instant::now(),
                    )
                    .unwrap();
                Ok(())
            })
            .await
            .unwrap();
        // No traffic/polling to cause cleanup: only the scheduled deadline.
        let event = tokio::time::timeout(Duration::from_secs(2), events.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(event.kind, "secret.updated");
        assert!(!event.data.to_string().contains("short-lived-sentinel"));
        assert!(matches!(
            state.secrets.take("request", Instant::now()),
            Err(StoreError::NotFound)
        ));
        let status = state
            .db
            .call(|db| {
                Ok(db.query_row(
                    "SELECT state FROM secret_requests WHERE id='request'",
                    [],
                    |row| row.get::<_, String>(0),
                )?)
            })
            .await
            .unwrap();
        assert_eq!(status, "expired");
    }
}
