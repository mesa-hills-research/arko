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
pub fn local_mcp_approval(root: &Path, workspace: &Path) -> Result<Option<bool>, String>;
pub fn forget_local_mcp_approval(root: &Path, workspace: &Path) -> Result<bool, String>;
pub fn remove_added(root: &Path, added: Added) -> Result<String, String>;
```

`root` is the app's Arko data folder. Pass `Some(workspace)` to list what an agent session in
that project would get. Pass `None` for what applies to every project, which lists the supplied
MCP config files, `mcp.json` in Arko's folder, and skills in Arko's `skills`, `~/.agents/skills`
and `~/.claude/skills`. A project's config and skill folders are included only with a workspace.

`Extensions` holds `servers`, `skills` and `notes`. Each server and skill names its source and
has a `skipped` reason when it would be left out. The servers come in the order a session reads
their files, which is the supplied files, the project's `.arko/mcp.json` (where agents add servers),
its `.mcp.json`, then Arko's `mcp.json`. A server's `workers` and `shared` are Arko's fields of its
entry. A skill's `models` and `workers` are its front matter's. Each has `added`, the record of
when, in which session and why an agent added it (`arko::agent_added`), or `None` when no agent did.

The built-in skills come after the folders' skills, with `builtin` set and an empty `path`, and one
is skipped when a skill of the same name is in a folder. `list` leaves out the built-in skills that
`ARKO_BUILTIN_SKILLS` and `ARKO_BUILTIN_SKILLS_OFF` leave out, and `list_with` takes the app's
choice as `Options` does, with `None` leaving it to the environment. A name that isn't a built-in
skill is skipped, as a session skips it. If `ARKO_BUILTIN_SKILLS` holds something other than 1 or
0, every built-in skill is skipped, with that as the reason. Each `ExtensionNote` has a
`file: PathBuf` and a `message: String`, so a settings page can attach a read error to its file.

`project_mcp_approval` returns the remembered answer for the current `.mcp.json` servers.
`Some(true)` means approved, `Some(false)` means declined, and `None` means there is no
answer for this config, or no config. A changed `mcpServers` value needs approval again.
Unreadable config or approval files return an error.

`local_mcp_approval` and `forget_local_mcp_approval` do the same for the project's
`.arko/mcp.json`. A server an agent adds there is remembered as approved.

`forget_project_mcp_approval` removes the project's answer, even if the config has changed or
been removed. It returns `true` when it removed an answer and `false` when there was none.
Other projects keep their answers. The next session asks again before starting that project's
servers. Servers in sessions already open keep running. Supplied MCP config files and the
`mcp.json` in Arko's folder follow their existing startup rules without project approval.

Approval writes and forgetting share a file lock and replace the saved state as a whole.
An unreadable approval file is reported rather than overwritten.

`remove_added` removes what an agent added, given as `Added::Server { file, name }` or
`Added::Skill { path }`. It takes the server's entry out of its file, or deletes the skill's folder,
and drops it from the record of what agents added (`agent-added.json` in Arko's folder). Anything
the record doesn't name is refused, so a user's own server or skill stays. The text is the line to
show, such as "Removed the MCP server github.", or why nothing was removed. Sessions open now keep
a removed server running until they close.
