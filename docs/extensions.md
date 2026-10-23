# Listing extensions and managing project approvals

`arko::extensions` reads MCP server configs and skill folders without starting servers or
opening a session. Run these functions off the UI thread because they read files.

```rust
pub fn list(root: &Path, workspace: Option<&Path>, mcp_config: &[PathBuf]) -> Extensions;
pub fn list_with(
    root: &Path,
    workspace: Option<&Path>,
    mcp_config: &[PathBuf],
    builtin_skills: Option<bool>,
    builtin_skills_off: Option<&[String]>,
) -> Extensions;
pub fn project_mcp_approval(root: &Path, workspace: &Path) -> Result<Option<bool>, String>;
pub fn forget_project_mcp_approval(root: &Path, workspace: &Path) -> Result<bool, String>;
```

`root` is the app's Arko data folder. Pass `Some(workspace)` to list what an agent session in
that project would get. Pass `None` for shared settings, which lists the supplied MCP config
files, `mcp.json` in Arko's folder, and skills in Arko's `skills`, `~/.agents/skills` and
`~/.claude/skills`. A project's config and skill folders are included only with a workspace.

`Extensions` holds `servers`, `skills` and `notes`. Each server and skill names its source and
has a `skipped` reason when it would be left out. The built-in skills come after the folders'
skills, with `builtin` set and an empty `path`, and one is skipped when a skill of the same name is
in a folder. `list` leaves out the built-in skills that `ARKO_BUILTIN_SKILLS` and
`ARKO_BUILTIN_SKILLS_OFF` leave out, and `list_with` takes the app's choice as `Options` does, with
`None` leaving it to the environment. A name that isn't a built-in skill is skipped, as a session
skips it. If `ARKO_BUILTIN_SKILLS` holds something other than 1 or 0, every built-in skill is skipped,
with that as the reason. Each `ExtensionNote` has a `file: PathBuf` and a `message: String`, so a settings page
can attach a read error to its file.

`project_mcp_approval` returns the remembered answer for the current `.mcp.json` servers.
`Some(true)` means approved, `Some(false)` means declined, and `None` means there is no
answer for this config, or no config. A changed `mcpServers` value needs approval again.
Unreadable config or approval files return an error.

`forget_project_mcp_approval` removes the project's answer, even if the config has changed or
been removed. It returns `true` when it removed an answer and `false` when there was none.
Other projects keep their answers. The next session asks again before starting that project's
servers. Servers in sessions already open keep running. Supplied MCP config files and Arko's
shared `mcp.json` follow their existing startup rules without project approval.

Approval writes and forgetting share a file lock and replace the saved state as a whole.
An unreadable approval file is reported rather than overwritten.
