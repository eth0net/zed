use db::kvp::KeyValueStore;
use gpui::{App, AppContext as _, Context, Subscription, Task, WindowId};
use util::ResultExt;

pub struct Session {
    session_id: String,
    old_session_ids: Vec<String>,
    old_window_ids: Option<Vec<WindowId>>,
}

const SESSION_ID_KEY: &str = "session_id";
const SESSION_WINDOW_STACK_KEY: &str = "session_window_stack";
// Sessions whose windows haven't all been restored yet, newest first. A launch
// killed mid-restore leaves windows under both its own id and the ones it was
// restoring from.
const UNRESTORED_SESSION_IDS_KEY: &str = "unrestored_session_ids";

impl Session {
    pub async fn new(session_id: String, db: KeyValueStore) -> Self {
        let old_session_id = db.read_kvp(SESSION_ID_KEY).ok().flatten();
        let unrestored_session_ids = db
            .read_kvp(UNRESTORED_SESSION_IDS_KEY)
            .ok()
            .flatten()
            .and_then(|json| serde_json::from_str::<Vec<String>>(&json).log_err())
            .unwrap_or_default();

        let mut old_session_ids = Vec::new();
        for id in old_session_id.into_iter().chain(unrestored_session_ids) {
            if !old_session_ids.contains(&id) {
                old_session_ids.push(id);
            }
        }

        // Record the old ids before replacing the current one, so a crash between
        // the two writes still leaves them reachable.
        if !old_session_ids.is_empty()
            && let Some(json) = serde_json::to_string(&old_session_ids).log_err()
        {
            db.write_kvp(UNRESTORED_SESSION_IDS_KEY.to_string(), json)
                .await
                .log_err();
        }

        db.write_kvp(SESSION_ID_KEY.to_string(), session_id.clone())
            .await
            .log_err();

        let old_window_ids = db
            .read_kvp(SESSION_WINDOW_STACK_KEY)
            .ok()
            .flatten()
            .and_then(|json| serde_json::from_str::<Vec<u64>>(&json).ok())
            .map(|vec: Vec<u64>| {
                vec.into_iter()
                    .map(WindowId::from)
                    .collect::<Vec<WindowId>>()
            });

        Self {
            session_id,
            old_session_ids,
            old_window_ids,
        }
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn test() -> Self {
        Self {
            session_id: uuid::Uuid::new_v4().to_string(),
            old_session_ids: Vec::new(),
            old_window_ids: None,
        }
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn test_with_old_session(old_session_id: String) -> Self {
        Self {
            session_id: uuid::Uuid::new_v4().to_string(),
            old_session_ids: vec![old_session_id],
            old_window_ids: None,
        }
    }

    pub fn id(&self) -> &str {
        &self.session_id
    }
}

pub struct AppSession {
    session: Session,
    _serialization_task: Task<()>,
    _subscriptions: Vec<Subscription>,
}

impl AppSession {
    pub fn new(session: Session, cx: &Context<Self>) -> Self {
        let _subscriptions = vec![cx.on_app_quit(Self::app_will_quit)];

        let _serialization_task = if cfg!(not(any(test, feature = "test-support"))) {
            let db = KeyValueStore::global(cx);
            cx.spawn(async move |_, cx| {
                // Disabled in tests: the infinite loop bypasses "parking forbidden" checks,
                // causing tests to hang instead of panicking.
                {
                    let mut current_window_stack = Vec::new();
                    loop {
                        if let Some(windows) = cx.update(|cx| window_stack(cx))
                            && !windows.is_empty()
                            && windows != current_window_stack
                        {
                            store_window_stack(db.clone(), &windows).await;
                            current_window_stack = windows;
                        }

                        cx.background_executor()
                            .timer(std::time::Duration::from_millis(500))
                            .await;
                    }
                }
            })
        } else {
            Task::ready(())
        };

        Self {
            session,
            _subscriptions,
            _serialization_task,
        }
    }

    fn app_will_quit(&mut self, cx: &mut Context<Self>) -> Task<()> {
        if let Some(window_stack) = window_stack(cx)
            && !window_stack.is_empty()
        {
            let db = KeyValueStore::global(cx);
            cx.background_spawn(async move { store_window_stack(db, &window_stack).await })
        } else {
            Task::ready(())
        }
    }

    pub fn id(&self) -> &str {
        self.session.id()
    }

    pub fn last_session_id(&self) -> Option<&str> {
        self.session.old_session_ids.first().map(String::as_str)
    }

    /// The last session, then any earlier sessions whose restore was interrupted.
    pub fn last_session_ids(&self) -> &[String] {
        &self.session.old_session_ids
    }

    /// Call once the last session has been restored, so the next launch no longer
    /// restores from the sessions before this one.
    pub fn restore_finished(&self, cx: &App) -> Task<()> {
        let db = KeyValueStore::global(cx);
        cx.background_spawn(async move {
            db.delete_kvp(UNRESTORED_SESSION_IDS_KEY.to_string())
                .await
                .log_err();
        })
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn replace_session_for_test(&mut self, session: Session) {
        self.session = session;
    }

    pub fn last_session_window_stack(&self) -> Option<Vec<WindowId>> {
        self.session.old_window_ids.clone()
    }
}

fn window_stack(cx: &App) -> Option<Vec<u64>> {
    Some(
        cx.window_stack()?
            .into_iter()
            .map(|window| window.window_id().as_u64())
            .collect(),
    )
}

async fn store_window_stack(db: KeyValueStore, windows: &[u64]) {
    if let Ok(window_ids_json) = serde_json::to_string(windows) {
        db.write_kvp(SESSION_WINDOW_STACK_KEY.to_string(), window_ids_json)
            .await
            .log_err();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    async fn test_interrupted_restore_keeps_earlier_sessions(cx: &mut TestAppContext) {
        let app_db = db::AppDatabase::test_new();
        let db = KeyValueStore::from_app_db(&app_db);
        cx.update(|cx| cx.set_global(app_db));

        let launch = |id: &str| Session::new(id.to_string(), db.clone());

        assert!(launch("first").await.old_session_ids.is_empty());
        // Killed before "first" was restored.
        assert_eq!(launch("second").await.old_session_ids, ["first"]);
        let third = launch("third").await;
        assert_eq!(third.old_session_ids, ["second", "first"]);

        let app_session = cx.new(|cx| AppSession::new(third, cx));
        cx.update(|cx| app_session.read(cx).restore_finished(cx))
            .await;

        assert_eq!(launch("fourth").await.old_session_ids, ["third"]);
    }
}
