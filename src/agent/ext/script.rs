use super::*;

/// One manifest-carrying script mounted as a tool: the host runs the
/// protocol around each call (spawn, feed arguments, collect output,
/// timeout), the script itself only prints its result.
///
/// A `.rs` source cannot run through an interpreter the way `#!` scripts
/// do — the language has none — so the host compiles it first: `rustc -O`
/// into a content-hash cache under `~/.yak/tmp/rs-cache/`, rebuilt only
/// when the source changes (the cache rides the same weekly tmp sweep as
/// the editor's pasted images). The compile shares the call's timeout and
/// streams rustc's stderr into the tool log, so a broken tool fails with
/// its diagnostics, not a bare timeout.
pub(super) struct ScriptTool {
    pub(super) spec: ExecToolSpec,
    pub(super) description: String,
    /// registry name; differs from `spec.name` only on a collision
    pub(super) exposed: String,
}

impl Tool for ScriptTool {
    fn name(&self) -> &str {
        &self.exposed
    }
    fn tier(&self) -> Tier {
        self.spec.tier
    }
    fn description(&self) -> &str {
        &self.description
    }
    fn parameters(&self) -> Value {
        self.spec.schema.clone()
    }
    fn preview(&self, args: &Value) -> String {
        crate::agent::tools::args_preview(&self.spec.name, args)
    }
    fn execute(
        &self,
        args: &Value,
        cwd: &Path,
        log: &mut dyn FnMut(crate::agent::tools::ToolProgress),
    ) -> ToolOutput {
        let program = match self.resolved_program(log) {
            Ok(path) => path,
            Err(e) => return ToolOutput::err(e),
        };
        let mut command = match program {
            ResolvedProgram::Interpreted { prog } => {
                let mut c = std::process::Command::new(prog);
                c.arg(&self.spec.path);
                c
            }
            ResolvedProgram::Binary(path) => std::process::Command::new(path),
            ResolvedProgram::Source => std::process::Command::new(&self.spec.path),
        };
        command
            .current_dir(cwd)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        // argv mode: the single declared argument rides as plain argv[1],
        // so shell scripts never need to parse JSON. Picked by the declared
        // name — the model's key order (or a stray extra key) must not
        // decide what the script receives
        let stdin_payload = if self.spec.arg_mode_argv {
            let value = self
                .spec
                .schema
                .pointer("/properties")
                .and_then(Value::as_object)
                .and_then(|props| props.keys().next())
                .and_then(|name| args.get(name))
                .map(|v| match v {
                    Value::String(s) => s.clone(),
                    other => crate::jsonfmt::dumps_indent(other, 0),
                })
                .unwrap_or_default();
            command.arg(value);
            None
        } else {
            Some(format!(
                "{}\n",
                serde_json::to_string(args).expect("args serialize")
            ))
        };
        let outcome = crate::platform::run_with_progress(
            command,
            stdin_payload.as_deref(),
            self.spec.timeout,
            crate::core::http::interrupt_flag(),
            &mut |line| log(crate::agent::tools::ToolProgress::line(line)),
        );
        if outcome.interrupted {
            return ToolOutput::err("script tool interrupted");
        }
        if outcome.timed_out {
            return ToolOutput::err(format!(
                "script tool timed out after {}s: {}",
                self.spec.timeout,
                self.spec.path.display()
            ));
        }
        crate::agent::tools::finish_process_output(outcome.stdout, outcome.stderr, outcome.code)
    }
}

/// What `execute` ends up spawning for one script.
enum ResolvedProgram {
    /// the manifest's interpreter with the script path behind it
    Interpreted { prog: String },
    /// a compiled binary: the `.rs` cache hit or fresh build
    Binary(PathBuf),
    /// the source file itself (shebang / exec bit / Windows association)
    Source,
}

impl ScriptTool {
    /// Decide how this script runs. A Rust source has no interpreter to
    /// hand it to, so it goes through the compile cache instead; anything
    /// else keeps the interpreter / direct-spawn behavior.
    fn resolved_program(
        &self,
        log: &mut dyn FnMut(crate::agent::tools::ToolProgress),
    ) -> Result<ResolvedProgram, String> {
        let is_rust = self.spec.interpreter.as_deref() == Some("rust")
            || self.spec.path.extension().is_some_and(|e| e == "rs");
        if !is_rust {
            return Ok(match self.spec.interpreter.as_deref() {
                Some(prog) => ResolvedProgram::Interpreted {
                    prog: prog.to_string(),
                },
                None => ResolvedProgram::Source,
            });
        }
        self.compile_rust(log).map(ResolvedProgram::Binary)
    }

    /// `rustc -O src -o cache/<stem>-<hash>`; the hash covers the source
    /// bytes, so a cache hit means the binary already matches the file on
    /// disk. A missing rustc is the one error worth hand-holding: the
    /// install command is short and the tool cannot ever run without it.
    fn compile_rust(
        &self,
        log: &mut dyn FnMut(crate::agent::tools::ToolProgress),
    ) -> Result<PathBuf, String> {
        let source = std::fs::read(&self.spec.path)
            .map_err(|e| format!("cannot read {}: {e}", self.spec.path.display()))?;
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        use std::hash::Hasher;
        hasher.write(&source);
        let hash = format!("{:016x}", hasher.finish());
        let stem = self
            .spec
            .path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("rust-tool");
        let cache_dir = crate::core::config::user_dir().join("tmp").join("rs-cache");
        let binary = cache_dir.join(format!("{stem}-{hash}"));
        if binary.is_file() {
            return Ok(binary);
        }
        let _ = std::fs::create_dir_all(&cache_dir);
        // compile to a temp name then rename, so a cache hit can never be
        // a half-written binary from a previous interrupted compile
        let staging = cache_dir.join(format!(".{stem}-{hash}.building"));
        let _ = std::fs::remove_file(&staging);
        let mut compile = std::process::Command::new("rustc");
        compile
            .arg("--edition")
            .arg("2021")
            .arg("-O")
            .arg("-C")
            .arg("debuginfo=0")
            .arg(&self.spec.path)
            .arg("-o")
            .arg(&staging)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        log(crate::agent::tools::ToolProgress::line(format!(
            "compiling {} (first call compiles; later calls reuse the cache)",
            self.spec.path.display()
        )));
        let outcome = crate::platform::run_with_progress(
            compile,
            None,
            self.spec.timeout,
            crate::core::http::interrupt_flag(),
            &mut |line| log(crate::agent::tools::ToolProgress::line(line)),
        );
        if outcome.interrupted {
            let _ = std::fs::remove_file(&staging);
            return Err("rust compile interrupted".to_string());
        }
        if outcome.timed_out {
            let _ = std::fs::remove_file(&staging);
            return Err(format!(
                "rustc timed out after {}s compiling {}",
                self.spec.timeout,
                self.spec.path.display()
            ));
        }
        if outcome.code != 0 {
            let _ = std::fs::remove_file(&staging);
            let hint = if String::from_utf8_lossy(&outcome.stderr).contains("not found") {
                "\nRust does not appear to be installed. Install it with:\n  rustup: curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh\n  (or your system package manager, e.g. apt install rustc)\nthen retry the tool."
            } else {
                ""
            };
            return Err(format!(
                "rustc failed (exit {}) compiling {}: {}\n{}",
                outcome.code,
                self.spec.path.display(),
                hint,
                String::from_utf8_lossy(&outcome.stderr)
            ));
        }
        std::fs::rename(&staging, &binary).map_err(|e| {
            let _ = std::fs::remove_file(&staging);
            format!("cannot publish compiled tool to {}: {e}", binary.display())
        })?;
        Ok(binary)
    }
}
