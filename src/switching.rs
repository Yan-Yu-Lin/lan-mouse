//! Short, ordered local hooks. No input contents are logged.
use std::time::{Duration, Instant};
use tokio::process::Command;

pub(crate) async fn run_hook(command: Option<String>, state: &str) -> bool {
    let Some(command) = command else {
        return true;
    };
    let started = Instant::now();
    let mut process = Command::new("sh");
    process
        .arg("-c")
        .arg(command)
        .env("LAN_MOUSE_STATE", state)
        .kill_on_drop(true);
    let result = tokio::time::timeout(Duration::from_secs(1), process.status()).await;
    let ok = matches!(result, Ok(Ok(status)) if status.success());
    log::info!(
        "switch hook: state={state} ok={ok} elapsed_ms={}",
        started.elapsed().as_millis()
    );
    ok
}

/// Compare wrapping serials; duplicate or older entry packets never reactivate a session.
pub(crate) fn newer(serial: u32, previous: u32) -> bool {
    serial != previous && serial.wrapping_sub(previous) < (1 << 31)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn serial_order_rejects_duplicates_and_stale_packets() {
        assert!(!newer(7, 7));
        assert!(!newer(6, 7));
        assert!(newer(8, 7));
        assert!(newer(1, u32::MAX));
        assert!(!newer(u32::MAX, 1));
    }
    #[tokio::test]
    async fn hook_reports_failure() {
        assert!(run_hook(None, "connecting").await);
        assert!(!run_hook(Some("exit 1".into()), "connecting").await);
        assert!(run_hook(Some("test \"$LAN_MOUSE_STATE\" = remote".into()), "remote").await);
    }
}
