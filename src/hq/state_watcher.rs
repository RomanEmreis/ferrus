use tokio::sync::watch;
use crate::state::{machine::StateData, store};

/// Poll STATE.json every 250 ms; send when content changes.
///
/// Change detection uses `updated_at` as the primary signal — `store::write_state`
/// always stamps it (this is a strict invariant). As a belt-and-suspenders guard,
/// we also compare `state` so a missed timestamp update never silently drops a
/// state transition.
pub async fn watch(tx: watch::Sender<Option<StateData>>) {
    let mut last_ts = None;
    let mut last_task_state = None;
    loop {
        tokio::time::sleep(tokio::time::Duration::from_millis(250)).await;
        if let Ok(state) = store::read_state().await {
            let ts = state.updated_at;
            let task_state = state.state.clone();
            if last_ts != Some(ts) || last_task_state.as_ref() != Some(&task_state) {
                last_ts = Some(ts);
                last_task_state = Some(task_state);
                let _ = tx.send(Some(state));
            }
        }
    }
}
