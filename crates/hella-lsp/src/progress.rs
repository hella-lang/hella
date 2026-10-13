//! Server-initiated work-done progress.
//!
//! These are the messages editors render as a progress indicator at the
//! bottom of the window (e.g. VS Code's status-area progress row): the
//! server sends `window/workDoneProgress/create` once, then `$/progress`
//! `Begin`/`Report` notifications, and a final `End` which dismisses the
//! indicator.
//!
//! The `create` handshake is asynchronous — progress is only reported after
//! the client acknowledges our token (tracked with [`ProgressState`]).
//! Clients without progress support reject `create`, after which the server
//! stays silent.

use lsp_server::{Notification, Request, RequestId};
use lsp_types::notification::Notification as _;
use lsp_types::notification::Progress as ProgressNotif;
use lsp_types::request::Request as _;
use lsp_types::request::WorkDoneProgressCreate;
use lsp_types::{
    NumberOrString, ProgressParams, ProgressParamsValue, WorkDoneProgress,
    WorkDoneProgressBegin, WorkDoneProgressCreateParams, WorkDoneProgressEnd,
    WorkDoneProgressReport,
};

/// Token identifying Hella's workspace progress session. Created once per
/// server lifetime and reused for every begin/end cycle.
pub fn token() -> NumberOrString {
    NumberOrString::String("hella/workspace".to_string())
}

/// Lifecycle of our server-initiated progress token.
#[derive(Debug, Default)]
pub enum ProgressState {
    /// No `create` request sent yet (or nothing to report with).
    #[default]
    Idle,
    /// `create` sent; waiting for the client response before reporting.
    Creating(RequestId),
    /// Client acknowledged the token; progress may be reported.
    Ready,
    /// Client rejected `create` (no progress support); stay silent.
    Unsupported,
}

impl ProgressState {
    pub fn is_ready(&self) -> bool {
        matches!(self, ProgressState::Ready)
    }
}

/// `window/workDoneProgress/create` request for our token.
pub fn create_request(id: RequestId) -> Request {
    Request {
        id,
        method: WorkDoneProgressCreate::METHOD.to_string(),
        params: serde_json::to_value(WorkDoneProgressCreateParams {
            token: token(),
        })
        .expect("progress create params serialize"),
    }
}

fn progress_notification(value: WorkDoneProgress) -> Notification {
    let params = ProgressParams {
        token: token(),
        value: ProgressParamsValue::WorkDone(value),
    };
    Notification {
        method: ProgressNotif::METHOD.to_string(),
        params: serde_json::to_value(params)
            .expect("progress params serialize"),
    }
}

/// `$/progress` `Begin`: shows the indicator with `title`.
pub fn begin(
    title: &str,
    message: Option<&str>,
    percentage: Option<u32>,
) -> Notification {
    progress_notification(WorkDoneProgress::Begin(WorkDoneProgressBegin {
        title: title.to_string(),
        cancellable: Some(false),
        message: message.map(str::to_string),
        percentage,
    }))
}

/// `$/progress` `Report`: updates message/percentage on the indicator.
pub fn report(message: Option<&str>, percentage: Option<u32>) -> Notification {
    progress_notification(WorkDoneProgress::Report(WorkDoneProgressReport {
        cancellable: Some(false),
        message: message.map(str::to_string),
        percentage,
    }))
}

/// `$/progress` `End`: dismisses the indicator.
pub fn end(message: Option<&str>) -> Notification {
    progress_notification(WorkDoneProgress::End(WorkDoneProgressEnd {
        message: message.map(str::to_string),
    }))
}

/// Count `.hll` files under `roots` (bounded, fast, no behavior change).
///
/// Skips hidden directories as well as common build/output trees
/// (`target`, `out`, `node_modules`) and stops descending after a budget of
/// visited entries so startup stays snappy on huge checkouts.
pub fn count_hella_files(roots: &[std::path::PathBuf]) -> usize {
    const BUDGET: usize = 20_000;
    let mut count = 0;
    let mut visited = 0;
    let mut stack: Vec<std::path::PathBuf> = roots.to_vec();
    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            visited += 1;
            if visited > BUDGET {
                return count;
            }
            let path = entry.path();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with('.') {
                continue;
            }
            let ft = match entry.file_type() {
                Ok(t) => t,
                Err(_) => continue,
            };
            if ft.is_dir() {
                if name == "target" || name == "out" || name == "node_modules" {
                    continue;
                }
                stack.push(path);
            } else if ft.is_file()
                && path.extension().is_some_and(|e| e == "hll")
            {
                count += 1;
            }
        }
    }
    count
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_request_shape() {
        let req =
            create_request(RequestId::from("hella/progress/1".to_string()));
        assert_eq!(req.method, "window/workDoneProgress/create");
        assert_eq!(
            req.params,
            serde_json::json!({ "token": "hella/workspace" })
        );
    }

    #[test]
    fn begin_report_end_shapes() {
        let b = begin("Indexing Hella workspace", Some("Scanning"), Some(0));
        assert_eq!(b.method, "$/progress");
        assert_eq!(
            b.params,
            serde_json::json!({
                "token": "hella/workspace",
                "value": {
                    "kind": "begin",
                    "title": "Indexing Hella workspace",
                    "cancellable": false,
                    "message": "Scanning",
                    "percentage": 0
                }
            })
        );

        let r = report(Some("12 files"), Some(100));
        assert_eq!(
            r.params,
            serde_json::json!({
                "token": "hella/workspace",
                "value": {
                    "kind": "report",
                    "cancellable": false,
                    "message": "12 files",
                    "percentage": 100
                }
            })
        );

        let e = end(Some("Indexed 12 Hella files"));
        assert_eq!(
            e.params,
            serde_json::json!({
                "token": "hella/workspace",
                "value": { "kind": "end", "message": "Indexed 12 Hella files" }
            })
        );
    }

    #[test]
    fn progress_state_defaults_idle() {
        let s = ProgressState::default();
        assert!(!s.is_ready());
        assert!(ProgressState::Ready.is_ready());
        assert!(!ProgressState::Unsupported.is_ready());
    }

    #[test]
    fn count_hella_files_finds_nested_sources() {
        let root = std::env::temp_dir()
            .join(format!("hella-lsp-progress-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src/nested")).unwrap();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::create_dir_all(root.join("target")).unwrap();
        std::fs::write(root.join("main.hll"), "int main() do\nend\n").unwrap();
        std::fs::write(root.join("src/nested/mod.hll"), "").unwrap();
        std::fs::write(root.join("notes.txt"), "").unwrap();
        std::fs::write(root.join(".git/hidden.hll"), "").unwrap();
        std::fs::write(root.join("target/built.hll"), "").unwrap();

        assert_eq!(count_hella_files(&[root.clone()]), 2);
        assert_eq!(count_hella_files(&[]), 0);
        assert_eq!(count_hella_files(&[root.join("does-not-exist")]), 0);
        let _ = std::fs::remove_dir_all(&root);
    }
}
