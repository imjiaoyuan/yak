// --- yak-tool: rustgreet
// description: greet a name from a single-file Rust script tool
// args: name (string) the name to greet
// arg-mode: argv
// timeout: 60
//
// A Rust "script tool": the manifest header above is the whole integration.
// Copy this file into ~/.yak/extensions/ (or the project's .yak/extensions/)
// and /reload — the host compiles it with rustc on first call (content-hash
// cached under ~/.yak/tmp/rs-cache/, rebuilt only when the source changes)
// and runs the binary. Needs a Rust toolchain installed; without rustc the
// tool call returns an error with install instructions.
//
// arg-mode: argv passes the single declared argument as plain argv[1].
// Without that line the arguments object arrives as one JSON line on stdin.

fn main() {
    let name = std::env::args().nth(1).unwrap_or_default();
    println!("hello, {name}, from a rust script tool");
}
