//! Board scripts: the escape hatch for boot configuration BenchBob has no built-in editor
//! for (a vendor tool, a merged DTB, a file format of its own). The board file names an
//! executable (`[boot] files = [{ kind = "script", path = "…" }]`) and the GPIO panel's
//! overlay routes call it. The contract is documented in backend/boards/README.md:
//!
//! ```text
//! <script> read                  {"enabled": [...], "path"?, "label"?, "prefix"?, "user"?, "revision"?}
//! <script> list                  {"installed": [{"name", "file"?, "what"?, "pins"?}]}
//! <script> stage '<change>'      {"enabled": [...after], "diff"?, "summary"?, "warnings"?}   nothing written
//! <script> apply '<change>'      {"ok": true, "message"?, "backup"?, "rebootRequired"?}
//! <change> = {"enable": [...], "disable": [...]}
//! ```
//!
//! JSON on stdout; exit status non-zero (or `{"error": "…"}`) is a refusal, with the
//! message from stderr or `error`. Environment: BENCHBOB_BOARD (the board id),
//! BENCHBOB_OVERLAY_DIRS (the board file's overlay_dirs, ':'-separated),
//! BENCHBOB_STATE_DIR. Time limits: 30 s, 120 s for apply.

use benchbob_core::run::{Opts, run};
use serde_json::Value;

pub struct Script<'a> {
    pub path: &'a str,
    pub board: &'a str,
    pub overlay_dirs: &'a [String],
}

impl Script<'_> {
    pub async fn call(&self, args: &[&str], timeout_ms: u64) -> Result<Value, String> {
        let mut cmd = vec![self.path];
        cmd.extend_from_slice(args);
        let env = vec![
            ("BENCHBOB_BOARD".to_string(), self.board.to_string()),
            ("BENCHBOB_OVERLAY_DIRS".to_string(), self.overlay_dirs.join(":")),
            ("BENCHBOB_STATE_DIR".to_string(), benchbob_core::paths::state_dir().display().to_string()),
        ];
        let r = run(&cmd, Opts { env, ..Opts::timeout_ms(timeout_ms) }).await;
        let v: Option<Value> = serde_json::from_str(&r.out).ok();
        if let Some(e) = v.as_ref().and_then(|v| v.get("error")).and_then(Value::as_str) {
            return Err(format!("{}: {e}", self.path));
        }
        if r.code != 0 {
            let why = if !r.err.is_empty() {
                r.err
            } else if !r.out.is_empty() {
                r.out
            } else {
                format!("exit {}", r.code)
            };
            return Err(format!("{} {}: {why}", self.path, args.first().unwrap_or(&"")));
        }
        v.filter(Value::is_object).ok_or_else(|| format!("{} {}: didn't print a JSON object", self.path, args.first().unwrap_or(&"")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[tokio::test]
    async fn the_contract_round_trips() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("boot");
        std::fs::write(
            &p,
            "#!/bin/sh\ncase \"$1\" in\n read) echo '{\"enabled\":[\"a\"],\"path\":\"/boot/x\"}' ;;\n list) echo \"{\\\"installed\\\":[{\\\"name\\\":\\\"$BENCHBOB_BOARD\\\"}]}\" ;;\n stage) echo \"$2\" >&2; exit 3 ;;\n apply) echo '{\"error\":\"read-only\"}' ;;\nesac\n",
        )
        .unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        let path = p.to_string_lossy().into_owned();
        let s = Script { path: &path, board: "my-board", overlay_dirs: &[] };
        assert_eq!(s.call(&["read"], 5000).await.unwrap()["enabled"][0], "a");
        assert_eq!(s.call(&["list"], 5000).await.unwrap()["installed"][0]["name"], "my-board");
        let e = s.call(&["stage", "{\"enable\":[\"b\"]}"], 5000).await.unwrap_err();
        assert!(e.ends_with("{\"enable\":[\"b\"]}"), "{e}");
        assert!(s.call(&["apply", "{}"], 5000).await.unwrap_err().ends_with("read-only"));
    }
}
