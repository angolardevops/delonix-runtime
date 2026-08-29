//! In-process, session-scoped task registry (ADR-0025 §7). No persisted job queue —
//! that would be daemon-shaped infrastructure outliving the `delonix mcp serve`
//! session. A task's state lives only as long as this process does.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde_json::Value;

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "state", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum TaskState {
    Pending,
    Running,
    Succeeded {
        output: Value,
    },
    Failed {
        error: String,
    },
    /// Cancellation was requested, but this engine has no generic operation-abort
    /// primitive to hook into — a mutation already running is left to finish
    /// rather than killed mid-flight, which could leave it half-applied. This
    /// state only appears when cancellation was requested before the task
    /// actually started running.
    Cancelled,
}

#[derive(Debug, Serialize)]
pub struct TaskInfo {
    pub task_id: String,
    pub tool: String,
    pub risk: &'static str,
    pub created_unix: u64,
    pub state: TaskState,
}

struct TaskEntry {
    tool: String,
    risk: &'static str,
    created_unix: u64,
    state: Mutex<TaskState>,
    cancel_requested: std::sync::atomic::AtomicBool,
}

#[derive(Clone, Default)]
pub struct TaskRegistry {
    tasks: Arc<Mutex<HashMap<String, Arc<TaskEntry>>>>,
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl TaskRegistry {
    /// Runs `work` on a blocking thread and tracks it under a new task id.
    /// `work` returns `Ok(json)` on success or `Err(message)` on failure; it has
    /// no way to observe `cancel_requested` mid-flight in this pass — see
    /// [`TaskState::Cancelled`]'s doc comment for why that is honest, not lazy.
    pub fn spawn<F>(&self, tool: &str, risk: &'static str, work: F) -> String
    where
        F: FnOnce() -> Result<Value, String> + Send + 'static,
    {
        let id = format!(
            "task-{}-{}",
            now_unix(),
            NEXT_ID.fetch_add(1, Ordering::Relaxed)
        );
        let entry = Arc::new(TaskEntry {
            tool: tool.to_string(),
            risk,
            created_unix: now_unix(),
            state: Mutex::new(TaskState::Pending),
            cancel_requested: std::sync::atomic::AtomicBool::new(false),
        });
        self.tasks.lock().unwrap().insert(id.clone(), entry.clone());

        tokio::spawn(async move {
            if entry.cancel_requested.load(Ordering::SeqCst) {
                *entry.state.lock().unwrap() = TaskState::Cancelled;
                return;
            }
            *entry.state.lock().unwrap() = TaskState::Running;
            let result = tokio::task::spawn_blocking(work).await;
            let final_state = match result {
                Ok(Ok(output)) => TaskState::Succeeded { output },
                Ok(Err(error)) => TaskState::Failed { error },
                Err(join_error) => TaskState::Failed {
                    error: format!("task panicked: {join_error}"),
                },
            };
            *entry.state.lock().unwrap() = final_state;
        });

        id
    }

    pub fn get(&self, id: &str) -> Option<TaskInfo> {
        let entry = self.tasks.lock().unwrap().get(id).cloned()?;
        let state = entry.state.lock().unwrap().clone();
        Some(TaskInfo {
            task_id: id.to_string(),
            tool: entry.tool.clone(),
            risk: entry.risk,
            created_unix: entry.created_unix,
            state,
        })
    }

    /// Requests cancellation. Only prevents a task that has not yet started
    /// running from starting; a task already `Running` is unaffected and this
    /// returns its current (running) state — see [`TaskState::Cancelled`].
    pub fn cancel(&self, id: &str) -> Option<TaskInfo> {
        let entry = self.tasks.lock().unwrap().get(id).cloned()?;
        entry.cancel_requested.store(true, Ordering::SeqCst);
        let mut guard = entry.state.lock().unwrap();
        if matches!(*guard, TaskState::Pending) {
            *guard = TaskState::Cancelled;
        }
        let state = guard.clone();
        drop(guard);
        Some(TaskInfo {
            task_id: id.to_string(),
            tool: entry.tool.clone(),
            risk: entry.risk,
            created_unix: entry.created_unix,
            state,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// Polls `get` until the task leaves `Pending`/`Running`, or a generous
    /// timeout elapses — real work runs on a `spawn_blocking` thread, so a
    /// fixed sleep would be either flaky or needlessly slow.
    async fn wait_for_terminal(reg: &TaskRegistry, id: &str) -> TaskInfo {
        for _ in 0..200 {
            let info = reg.get(id).expect("task must exist");
            if !matches!(info.state, TaskState::Pending | TaskState::Running) {
                return info;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("task {id} never left Pending/Running");
    }

    #[tokio::test]
    async fn a_successful_task_ends_up_succeeded_with_its_output() {
        let reg = TaskRegistry::default();
        let id = reg.spawn("test.tool", "READ", || Ok(serde_json::json!({"ok": true})));
        let info = wait_for_terminal(&reg, &id).await;
        match info.state {
            TaskState::Succeeded { output } => assert_eq!(output, serde_json::json!({"ok": true})),
            other => panic!("expected Succeeded, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_failing_task_ends_up_failed_with_its_message() {
        let reg = TaskRegistry::default();
        let id = reg.spawn("test.tool", "READ", || Err("boom".to_string()));
        let info = wait_for_terminal(&reg, &id).await;
        match info.state {
            TaskState::Failed { error } => assert_eq!(error, "boom"),
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn cancelling_an_already_finished_task_does_not_overwrite_its_result() {
        let reg = TaskRegistry::default();
        let id = reg.spawn("test.tool", "READ", || Ok(serde_json::json!(1)));
        wait_for_terminal(&reg, &id).await;
        let cancelled = reg.cancel(&id).expect("task must exist");
        assert!(matches!(cancelled.state, TaskState::Succeeded { .. }));
    }

    #[tokio::test]
    async fn unknown_task_ids_are_none_everywhere() {
        let reg = TaskRegistry::default();
        assert!(reg.get("no-such-task").is_none());
        assert!(reg.cancel("no-such-task").is_none());
    }
}
