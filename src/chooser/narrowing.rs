//! The skills and MCP servers a worker gets.
//!
//! An agent's set of skills, and its set of servers, is everything or a
//! fixed list. The main agent has everything. A worker started with neither
//! `skills` nor `mcp` in the call or its profile has what its starter has,
//! without the skills and servers marked `workers: false`. One started with
//! names has exactly those, and each must be in its starter's set and not
//! marked `workers: false`, or the start is refused with the starter's set
//! named. The call's names win over the profile's. A profile serves many
//! starters, so its names are crossed with the starter's set, and the ones
//! the starter can't give are left out and said in the start result.
//!
//! Each start records the sets with the run, and a worker that starts
//! workers passes on its own, so a set carries through every depth.

use std::path::PathBuf;

use arko_agents::mcp::ServerFacts;
use arko_agents::skill::{self, SkillsConfig};

use super::{Basis, Call};
use crate::crew::{self, WorkerSetup};
use crate::profiles::Profile;

/// The skills the session's agents have, which a start can narrow a worker's
/// to. (The MCP servers are the hub's, which the start hook reads at each
/// start.)
#[derive(Debug, Clone, Default)]
pub struct Available {
    /// How the agents load skills (none without skill folders or built-in
    /// skills). The folders are read at each start that names skills, so a
    /// skill saved since is there.
    pub skills: Option<SkillsConfig>,
    /// The project, where the skill folders that are relative to it are.
    pub workspace: PathBuf,
}

/// A worker's skills and servers, as a start works them out.
pub(super) struct Narrowed {
    pub skills: Option<Vec<String>>,
    pub mcp: Option<Vec<String>>,
    /// The names the profile asked for that the starter couldn't give.
    pub left_out: Option<String>,
}

/// What a starter can give a worker of one kind.
#[derive(Debug, Default)]
struct Pool {
    /// The names a worker may have.
    give: Vec<String>,
    /// The names the starter has that are for the main agent alone.
    main_only: Vec<String>,
}

/// The skills and servers of the worker that `call` and `profile` ask for,
/// started by the agent whose recorded setup is `starter` (none: the main
/// agent, or a worker with no record). `worker` says the starter is a worker.
pub(super) fn narrow(
    basis: &Basis,
    call: &Call,
    profile: Option<&Profile>,
    starter: Option<&WorkerSetup>,
    worker: bool,
    servers: &[ServerFacts],
) -> Result<Narrowed, String> {
    let has_skills = starter.and_then(|starter| starter.skills.clone());
    let has_mcp = starter.and_then(|starter| starter.mcp.clone());
    let (skills, skills_left) = settle(
        "skill",
        call.skills.as_ref(),
        profile.and_then(|profile| profile.skills.as_ref()),
        &has_skills,
        || skill_pool(basis, &has_skills, worker),
    )?;
    let (mcp, mcp_left) = settle(
        "MCP server",
        call.mcp.as_ref(),
        profile.and_then(|profile| profile.mcp.as_ref()),
        &has_mcp,
        || server_pool(servers, &has_mcp),
    )?;
    let left_out: Vec<String> = [("skill", skills_left), ("MCP server", mcp_left)]
        .into_iter()
        .filter(|(_, names)| !names.is_empty())
        .map(|(noun, names)| {
            format!(
                "The profile also names {}, which this worker can't get.",
                names_phrase(noun, &names)
            )
        })
        .collect();
    Ok(Narrowed {
        skills,
        mcp,
        left_out: (!left_out.is_empty()).then(|| left_out.join(" ")),
    })
}

/// One kind's set for a worker: the call's names, which must all be in the
/// starter's `pool`, else the profile's that are, else what the starter has
/// (`has`: none is everything). Also the profile's names left out. The pool
/// is worked out only when names are asked for.
fn settle(
    noun: &str,
    asked: Option<&Vec<String>>,
    profile: Option<&Vec<String>>,
    has: &Option<Vec<String>>,
    pool: impl FnOnce() -> Pool,
) -> Result<(Option<Vec<String>>, Vec<String>), String> {
    // (No names, `none`, needs no pool.)
    if let Some(names) = asked {
        if !names.is_empty() {
            check(&pool(), noun, names)?;
        }
        return Ok((Some(names.clone()), Vec::new()));
    }
    if let Some(names) = profile.filter(|names| !names.is_empty()) {
        let pool = pool();
        let (kept, left) = names
            .iter()
            .cloned()
            .partition(|name| pool.give.contains(name));
        return Ok((Some(kept), left));
    }
    Ok((profile.or(has.as_ref()).cloned(), Vec::new()))
}

/// "the skill `a`" or "the skills `a` and `b`".
fn names_phrase(noun: &str, names: &[String]) -> String {
    let ticked: Vec<String> = names.iter().map(|name| format!("`{name}`")).collect();
    match ticked.as_slice() {
        [one] => format!("the {noun} {one}"),
        many => format!("the {noun}s {}", crew::names_words(many)),
    }
}

/// Whether the starter's `pool` can give all of `names`, else why not,
/// naming what it can give.
fn check(pool: &Pool, noun: &str, names: &[String]) -> Result<(), String> {
    let refused: Vec<String> = names
        .iter()
        .filter(|name| !pool.give.contains(name))
        .cloned()
        .collect();
    if refused.is_empty() {
        return Ok(());
    }
    let mut text = format!("A worker can't get {}.", names_phrase(noun, &refused));
    let alone: Vec<String> = refused
        .iter()
        .filter(|name| pool.main_only.contains(name))
        .map(|name| format!("`{name}`"))
        .collect();
    match alone.as_slice() {
        [] => {}
        [one] => text.push_str(&format!(" {one} is marked for the main agent alone.")),
        many => text.push_str(&format!(
            " {} are marked for the main agent alone.",
            crate::and_list(many)
        )),
    }
    if pool.give.is_empty() {
        text.push_str(&format!(" You have no {noun}s to give."));
    } else {
        text.push_str(&format!(" You can give {}.", crew::names_words(&pool.give)));
    }
    Err(text)
}

/// The skills the starter has, as the agents load them now. The starter's
/// own list narrows them (`has`), and a starter that is a worker has none
/// that are for the main agent alone.
fn skill_pool(basis: &Basis, has: &Option<Vec<String>>, worker: bool) -> Pool {
    let Some(config) = &basis.available.skills else {
        return Pool::default();
    };
    let config = SkillsConfig {
        subagent: worker,
        only: has.clone(),
        ..config.clone()
    };
    let mut pool = Pool::default();
    for scanned in skill::scan(&config, &basis.available.workspace)
        .into_iter()
        .filter(|scanned| scanned.skipped.is_none())
    {
        if scanned.workers {
            pool.give.push(scanned.name);
        } else {
            pool.main_only.push(scanned.name);
        }
    }
    pool
}

/// The MCP servers the starter has, by name. The starter's own list is
/// what it has when it is a list, else the hub's servers now. Those marked
/// `workers: false` are not given, and a refusal says why.
fn server_pool(servers: &[ServerFacts], has: &Option<Vec<String>>) -> Pool {
    let alone = |name: &str| servers.iter().any(|s| s.name == name && !s.workers);
    let names = match has {
        Some(list) => list.clone(),
        None => servers.iter().map(|server| server.name.clone()).collect(),
    };
    let (main_only, give) = names.into_iter().partition(|name| alone(name));
    Pool { give, main_only }
}

/// Whether a worker with the MCP servers `mcp` (none: its starter's) reaches
/// at least one of `servers`. A worker that reaches none isn't offered the
/// `mcp` tool.
pub(super) fn reaches_a_server(mcp: &Option<Vec<String>>, servers: &[ServerFacts]) -> bool {
    servers
        .iter()
        .any(|server| server.workers && mcp.as_ref().is_none_or(|list| list.contains(&server.name)))
}

/// Whether the names `profile` holds are ones a worker of the main agent can
/// get now, else why not. `save_worker_profile` checks them before it asks
/// the user, so no one is asked about names that no start would give.
pub fn check_profile_names(
    basis: &Basis,
    profile: &Profile,
    servers: &[ServerFacts],
) -> Result<(), String> {
    if let Some(names) = &profile.skills {
        check(&skill_pool(basis, &None, false), "skill", names)?;
    }
    if let Some(names) = &profile.mcp {
        check(&server_pool(servers, &None), "MCP server", names)?;
    }
    Ok(())
}
