# workcell-mcp-shell

`workcell-mcp-shell` implements Workcell's `shell` MCP tool. It starts commands in a canonical
workdir beneath the configured root, applies immutable deny-first command policy, streams ordered
progress, applies command, time, and output bounds, and terminates process groups on cancellation where
supported. Policy denials and invalid arguments are returned as actionable MCP tool errors.

The crate does not sandbox commands. A command can change directories, use absolute paths, access the
network, and reach any resource visible to the server process. Deploy Workcell inside the operating
system boundary intended for shell execution.

## Verification

```bash
cargo clippy -p workcell-mcp-shell --all-targets -- -D warnings
cargo test -p workcell-mcp-shell
```
