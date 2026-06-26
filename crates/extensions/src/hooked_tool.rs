//! `HookedTool`: a `Tool` decorator that fires matched `PreToolUse` hooks before the inner call
//! (a `PreToolUse` hook may BLOCK by exiting 2) and `PostToolUse` hooks after (observe-only). It
//! wraps the inner tool only when at least one hook matches the tool's name — otherwise `wrap`
//! returns the inner `Arc` unchanged, so un-hooked tools pay zero overhead. The decorator's `call`
//! wraps the inner tool's `call`; `ToolRegistry::call` dispatches to the decorator only after the
//! permission gate allows — so a hook can deny an allowed call but never widen a denied one.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use otto_engine_core::tool::Tool;
use serde_json::{Value, json};

use crate::hook_def::{HookCommand, HookSet};
use crate::hook_exec::{HookEvent, HookExecutor};

/// Default per-hook timeout when a `HookCommand` does not specify one.
const DEFAULT_HOOK_TIMEOUT_SECS: u64 = 60;

/// A `PreToolUse` hook blocks the tool call by exiting with this code (Claude Code convention).
const BLOCK_EXIT_CODE: i32 = 2;

pub struct HookedTool {
    inner: Arc<dyn Tool>,
    pre: Vec<HookCommand>,
    post: Vec<HookCommand>,
    executor: Arc<dyn HookExecutor>,
}

impl HookedTool {
    /// Wrap `inner` with the hooks matching `inner.name()`. Returns the inner `Arc` unchanged
    /// when no pre/post hook matches.
    pub fn wrap(
        inner: Arc<dyn Tool>,
        hooks: &HookSet,
        executor: Arc<dyn HookExecutor>,
    ) -> Arc<dyn Tool> {
        let pre = hooks.matched(HookEvent::PreToolUse, inner.name());
        let post = hooks.matched(HookEvent::PostToolUse, inner.name());
        if pre.is_empty() && post.is_empty() {
            return inner;
        }
        Arc::new(HookedTool {
            inner,
            pre,
            post,
            executor,
        })
    }

    /// Run a list of hooks against `input`. When `blocking`, an exit code of 2 aborts with an
    /// error (used for `PreToolUse`). Otherwise — and for any other nonzero exit, an executor
    /// error, or a timeout — the hook is a non-blocking warning and execution proceeds.
    async fn fire(&self, hooks: &[HookCommand], input: &str, blocking: bool) -> anyhow::Result<()> {
        let name = self.inner.name();
        for hook in hooks {
            let timeout = Duration::from_secs(hook.timeout.unwrap_or(DEFAULT_HOOK_TIMEOUT_SECS));
            match self.executor.run(&hook.command, input, timeout).await {
                Ok(out) if blocking && out.exit_code == Some(BLOCK_EXIT_CODE) => {
                    anyhow::bail!(
                        "tool '{name}' blocked by PreToolUse hook ({}): {}",
                        hook.command,
                        out.stderr.trim()
                    );
                }
                Ok(out) if out.exit_code != Some(0) => {
                    if blocking {
                        eprintln!(
                            "warning: PreToolUse gate hook ({}) for '{name}' exited {:?} without \
                             blocking (only exit {BLOCK_EXIT_CODE} blocks); tool will PROCEED \
                             unguarded: {}",
                            hook.command,
                            out.exit_code,
                            out.stderr.trim()
                        );
                    } else {
                        eprintln!(
                            "warning: PostToolUse hook ({}) for '{name}' exited {:?}: {}",
                            hook.command,
                            out.exit_code,
                            out.stderr.trim()
                        );
                    }
                }
                Ok(_) => {}
                Err(e) => {
                    if blocking {
                        eprintln!(
                            "warning: PreToolUse gate hook ({}) for '{name}' failed to run; tool \
                             will PROCEED unguarded: {e}",
                            hook.command
                        );
                    } else {
                        eprintln!(
                            "warning: PostToolUse hook ({}) for '{name}' failed to run: {e}",
                            hook.command
                        );
                    }
                }
            }
        }
        Ok(())
    }
}

#[async_trait]
impl Tool for HookedTool {
    fn name(&self) -> &str {
        self.inner.name()
    }

    async fn call(&self, args: Value) -> anyhow::Result<Value> {
        let name = self.inner.name();
        if !self.pre.is_empty() {
            let input = json!({
                "hook_event_name": "PreToolUse",
                "tool_name": name,
                "tool_input": &args,
            })
            .to_string();
            self.fire(&self.pre, &input, true).await?;
        }

        // Clone the args for the PostToolUse payload only when a post hook will consume them.
        let args_for_post = if self.post.is_empty() {
            None
        } else {
            Some(args.clone())
        };
        // PostToolUse hooks do not fire when the inner tool fails (the `?` returns early here).
        let result = self.inner.call(args).await?;

        if let Some(args) = args_for_post {
            let input = json!({
                "hook_event_name": "PostToolUse",
                "tool_name": name,
                "tool_input": args,
                "tool_response": &result,
            })
            .to_string();
            // `fire(.., blocking=false)` never returns Err; the discard is for clarity.
            let _ = self.fire(&self.post, &input, false).await;
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hook_def::{HookMatcher, HookSet};
    use crate::hook_exec::HookOutcome;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    struct SpyTool {
        called: Mutex<u32>,
    }
    #[async_trait]
    impl Tool for SpyTool {
        fn name(&self) -> &str {
            "fs.read"
        }
        async fn call(&self, _args: Value) -> anyhow::Result<Value> {
            *self.called.lock().unwrap() += 1;
            Ok(json!({ "ok": true }))
        }
    }

    struct FakeExec {
        exit: Option<i32>,
        seen: Mutex<Vec<(String, String)>>,
    }
    #[async_trait]
    impl HookExecutor for FakeExec {
        async fn run(
            &self,
            command: &str,
            stdin_json: &str,
            _timeout: Duration,
        ) -> anyhow::Result<HookOutcome> {
            self.seen
                .lock()
                .unwrap()
                .push((command.to_string(), stdin_json.to_string()));
            Ok(HookOutcome {
                exit_code: self.exit,
                stdout: String::new(),
                stderr: "nope".to_string(),
            })
        }
    }

    struct ErrExec;
    #[async_trait]
    impl HookExecutor for ErrExec {
        async fn run(&self, _c: &str, _s: &str, _t: Duration) -> anyhow::Result<HookOutcome> {
            Err(anyhow::anyhow!("spawn failed"))
        }
    }

    fn pre_set(matcher: &str, command: &str) -> HookSet {
        HookSet {
            pre_tool_use: vec![HookMatcher {
                matcher: Some(matcher.to_string()),
                hooks: vec![HookCommand {
                    command: command.to_string(),
                    timeout: None,
                }],
            }],
            post_tool_use: vec![],
        }
    }

    #[tokio::test]
    async fn wrap_returns_inner_when_no_hooks_match() {
        let inner: Arc<dyn Tool> = Arc::new(SpyTool {
            called: Mutex::new(0),
        });
        let exec = Arc::new(FakeExec {
            exit: Some(0),
            seen: Mutex::new(vec![]),
        });
        let wrapped = HookedTool::wrap(inner.clone(), &pre_set("bash", "x.sh"), exec);
        assert!(Arc::ptr_eq(&inner, &wrapped), "should return the same Arc");
    }

    #[tokio::test]
    async fn pre_exit_2_blocks_and_inner_not_called() {
        let spy = Arc::new(SpyTool {
            called: Mutex::new(0),
        });
        let exec = Arc::new(FakeExec {
            exit: Some(2),
            seen: Mutex::new(vec![]),
        });
        let wrapped = HookedTool::wrap(spy.clone(), &pre_set("fs.read", "block.sh"), exec.clone());
        let err = wrapped.call(json!({ "path": "a" })).await.unwrap_err();
        assert!(err.to_string().contains("blocked by PreToolUse hook"));
        assert_eq!(
            *spy.called.lock().unwrap(),
            0,
            "inner must not run when blocked"
        );
        let seen = exec.seen.lock().unwrap();
        assert_eq!(seen[0].0, "block.sh");
        assert!(seen[0].1.contains("\"hook_event_name\":\"PreToolUse\""));
        assert!(seen[0].1.contains("\"tool_name\":\"fs.read\""));
    }

    #[tokio::test]
    async fn pre_exit_0_allows_inner_to_run() {
        let spy = Arc::new(SpyTool {
            called: Mutex::new(0),
        });
        let exec = Arc::new(FakeExec {
            exit: Some(0),
            seen: Mutex::new(vec![]),
        });
        let wrapped = HookedTool::wrap(spy.clone(), &pre_set("fs.read", "ok.sh"), exec);
        let out = wrapped.call(json!({ "path": "a" })).await.unwrap();
        assert_eq!(out, json!({ "ok": true }));
        assert_eq!(*spy.called.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn pre_other_nonzero_is_non_blocking() {
        let spy = Arc::new(SpyTool {
            called: Mutex::new(0),
        });
        let exec = Arc::new(FakeExec {
            exit: Some(1),
            seen: Mutex::new(vec![]),
        });
        let wrapped = HookedTool::wrap(spy.clone(), &pre_set("*", "warn.sh"), exec);
        assert!(wrapped.call(json!({})).await.is_ok());
        assert_eq!(*spy.called.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn pre_executor_error_is_non_blocking() {
        let spy = Arc::new(SpyTool {
            called: Mutex::new(0),
        });
        let wrapped = HookedTool::wrap(
            spy.clone(),
            &pre_set("fs.read", "boom.sh"),
            Arc::new(ErrExec),
        );
        assert!(wrapped.call(json!({ "path": "a" })).await.is_ok());
        assert_eq!(*spy.called.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn post_runs_after_inner_with_tool_response() {
        let spy = Arc::new(SpyTool {
            called: Mutex::new(0),
        });
        let exec = Arc::new(FakeExec {
            exit: Some(2),
            seen: Mutex::new(vec![]),
        });
        let set = HookSet {
            pre_tool_use: vec![],
            post_tool_use: vec![HookMatcher {
                matcher: None,
                hooks: vec![HookCommand {
                    command: "log.sh".to_string(),
                    timeout: None,
                }],
            }],
        };
        let wrapped = HookedTool::wrap(spy.clone(), &set, exec.clone());
        let out = wrapped.call(json!({ "path": "a" })).await.unwrap();
        assert_eq!(out, json!({ "ok": true }));
        assert_eq!(*spy.called.lock().unwrap(), 1);
        let seen = exec.seen.lock().unwrap();
        assert!(seen[0].1.contains("\"hook_event_name\":\"PostToolUse\""));
        assert!(seen[0].1.contains("\"tool_response\":{\"ok\":true}"));
    }

    struct FailTool;
    #[async_trait]
    impl Tool for FailTool {
        fn name(&self) -> &str {
            "fs.read"
        }
        async fn call(&self, _args: Value) -> anyhow::Result<Value> {
            anyhow::bail!("inner boom")
        }
    }

    #[tokio::test]
    async fn post_does_not_fire_when_inner_tool_errors() {
        let exec = Arc::new(FakeExec {
            exit: Some(0),
            seen: Mutex::new(vec![]),
        });
        let set = HookSet {
            pre_tool_use: vec![],
            post_tool_use: vec![HookMatcher {
                matcher: None,
                hooks: vec![HookCommand {
                    command: "log.sh".to_string(),
                    timeout: None,
                }],
            }],
        };
        let wrapped = HookedTool::wrap(Arc::new(FailTool), &set, exec.clone());
        assert!(
            wrapped.call(json!({})).await.is_err(),
            "inner error must propagate"
        );
        assert!(
            exec.seen.lock().unwrap().is_empty(),
            "PostToolUse must not fire when the inner tool errors"
        );
    }

    /// Returns queued exit codes in order, recording each command it ran.
    struct SeqExec {
        exits: Mutex<VecDeque<i32>>,
        seen: Mutex<Vec<String>>,
    }
    #[async_trait]
    impl HookExecutor for SeqExec {
        async fn run(
            &self,
            command: &str,
            _stdin: &str,
            _t: Duration,
        ) -> anyhow::Result<HookOutcome> {
            self.seen.lock().unwrap().push(command.to_string());
            let code = self.exits.lock().unwrap().pop_front().unwrap_or(0);
            Ok(HookOutcome {
                exit_code: Some(code),
                stdout: String::new(),
                stderr: String::new(),
            })
        }
    }

    fn two_pre(c1: &str, c2: &str) -> HookSet {
        HookSet {
            pre_tool_use: vec![HookMatcher {
                matcher: Some("fs.read".to_string()),
                hooks: vec![
                    HookCommand {
                        command: c1.to_string(),
                        timeout: None,
                    },
                    HookCommand {
                        command: c2.to_string(),
                        timeout: None,
                    },
                ],
            }],
            post_tool_use: vec![],
        }
    }

    #[tokio::test]
    async fn all_nonblocking_pre_hooks_run_in_order() {
        let spy = Arc::new(SpyTool {
            called: Mutex::new(0),
        });
        let exec = Arc::new(SeqExec {
            exits: Mutex::new(VecDeque::from(vec![0, 0])),
            seen: Mutex::new(vec![]),
        });
        let wrapped = HookedTool::wrap(spy.clone(), &two_pre("a.sh", "b.sh"), exec.clone());
        assert!(wrapped.call(json!({})).await.is_ok());
        assert_eq!(*spy.called.lock().unwrap(), 1);
        assert_eq!(
            &*exec.seen.lock().unwrap(),
            &["a.sh".to_string(), "b.sh".to_string()]
        );
    }

    #[tokio::test]
    async fn blocking_pre_hook_short_circuits_later_hooks() {
        let spy = Arc::new(SpyTool {
            called: Mutex::new(0),
        });
        // first hook exits 2 (block) → second hook must NOT run, inner must NOT run
        let exec = Arc::new(SeqExec {
            exits: Mutex::new(VecDeque::from(vec![2, 0])),
            seen: Mutex::new(vec![]),
        });
        let wrapped = HookedTool::wrap(spy.clone(), &two_pre("block.sh", "later.sh"), exec.clone());
        assert!(wrapped.call(json!({})).await.is_err());
        assert_eq!(
            *spy.called.lock().unwrap(),
            0,
            "inner must not run after a block"
        );
        assert_eq!(
            &*exec.seen.lock().unwrap(),
            &["block.sh".to_string()],
            "later hook must not run after a block"
        );
    }
}
