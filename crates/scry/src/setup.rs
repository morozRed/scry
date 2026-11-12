//! `scry setup`: teaches coding agents to use Scry by installing a skill that explains the
//! CLI. See `scry_core::skill` for what the skill says and where each agent reads it.

use std::io::{BufRead, IsTerminal, Write};
use std::path::Path;

use anyhow::{Result, bail};
use clap::{Args, ValueEnum};
use scry_core::skill::{self, Agent, Env, Install, Remove};

/// Teach your coding agents to use Scry: installs a skill that tells them when to reach for
/// it and how to call its CLI safely. Without agents named, asks which in a terminal.
#[derive(Args)]
pub struct SetupArgs {
    /// The agents to install the skill for.
    #[arg(value_enum)]
    agents: Vec<SkillAgent>,
    /// Every agent found on this Mac.
    #[arg(long, conflicts_with = "agents")]
    all: bool,
    /// Remove Scry's skill instead. A skill of the same name that Scry did not write stays.
    #[arg(long)]
    remove: bool,
    /// Say what would change and write nothing.
    #[arg(long)]
    dry_run: bool,
    /// Print the skill and exit.
    #[arg(long, conflicts_with_all = ["agents", "all", "remove", "dry_run"])]
    print: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum SkillAgent {
    Claude,
    Codex,
    Opencode,
    Gemini,
}

impl From<SkillAgent> for Agent {
    fn from(a: SkillAgent) -> Agent {
        match a {
            SkillAgent::Claude => Agent::Claude,
            SkillAgent::Codex => Agent::Codex,
            SkillAgent::Opencode => Agent::OpenCode,
            SkillAgent::Gemini => Agent::Gemini,
        }
    }
}

pub fn run(a: SetupArgs) -> Result<()> {
    if a.print {
        print!("{}", skill::text());
        return Ok(());
    }
    let env = Env::read()?;
    let agents: Vec<Agent> = if a.all {
        let found: Vec<Agent> = Agent::ALL.into_iter().filter(|g| g.present(&env)).collect();
        if found.is_empty() {
            println!(
                "No agent found on this Mac: none of {} has its folder yet.",
                names()
            );
            return Ok(());
        }
        found
    } else if !a.agents.is_empty() {
        let mut v: Vec<Agent> = a.agents.iter().map(|&g| g.into()).collect();
        v.dedup();
        v
    } else if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
        let picked = ask(&env, a.remove)?;
        if picked.is_empty() {
            println!("Nothing changed.");
            return Ok(());
        }
        picked
    } else {
        bail!(
            "Name the agents, as in `scry setup claude codex`, or pass --all for every agent \
             found. The choices are claude, codex, opencode and gemini."
        );
    };

    let (mut refused, mut wrote) = (false, false);
    for agent in agents {
        let file = agent.skill_file(&env);
        let shown = tilde(&file, &env.home);
        let name = agent.name();
        if a.remove {
            match skill::plan_remove(&file)? {
                Remove::Missing => println!("  {name}: no Scry skill at {shown}"),
                Remove::Foreign => {
                    refused = true;
                    println!("  {name}: left {shown} alone; Scry did not write it");
                }
                Remove::Remove if a.dry_run => println!("  {name}: would remove {shown}"),
                Remove::Remove => {
                    skill::remove(&file)?;
                    println!("✓ {name}: removed {shown}");
                }
            }
            continue;
        }
        match skill::plan_install(&file)? {
            Install::Current => println!("✓ {name}: {shown} is current"),
            Install::Foreign => {
                refused = true;
                println!(
                    "  {name}: left {shown} alone; a skill named scry that Scry did not write \
                     is there"
                );
            }
            plan if a.dry_run => {
                let what = if plan == Install::Create {
                    "write"
                } else {
                    "update"
                };
                println!("  {name}: would {what} {shown}");
            }
            plan => {
                skill::install(&file)?;
                wrote = true;
                let what = if plan == Install::Create {
                    "wrote"
                } else {
                    "updated"
                };
                println!("✓ {name}: {what} {shown}");
            }
        }
    }
    if wrote {
        println!(
            "A new session of each agent loads the skill. `scry setup --remove` takes it out."
        );
    }
    if refused {
        std::process::exit(1);
    }
    Ok(())
}

/// Lists the agents with whether each is on this Mac, and reads the numbers picked. Enter
/// alone picks every agent found.
fn ask(env: &Env, remove: bool) -> Result<Vec<Agent>> {
    let verb = if remove { "forget" } else { "learn to use" };
    println!("Which agents should {verb} Scry?");
    for (i, agent) in Agent::ALL.iter().enumerate() {
        let state = match (
            agent.present(env),
            skill::plan_install(&agent.skill_file(env))?,
        ) {
            (_, Install::Current) => "skill installed",
            (_, Install::Update) => "skill out of date",
            (true, _) => "found",
            (false, _) => "not found",
        };
        println!("  {}  {:<12} {state}", i + 1, agent.name());
    }
    print!("Numbers, separated by spaces, or enter for every agent found: ");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    pick(line.trim(), env)
}

fn pick(answer: &str, env: &Env) -> Result<Vec<Agent>> {
    if answer.is_empty() {
        return Ok(Agent::ALL.into_iter().filter(|g| g.present(env)).collect());
    }
    let mut out = Vec::new();
    for word in answer.split(|c: char| c.is_whitespace() || c == ',') {
        if word.is_empty() {
            continue;
        }
        let agent = word
            .parse::<usize>()
            .ok()
            .and_then(|n| n.checked_sub(1))
            .and_then(|i| Agent::ALL.get(i));
        match agent {
            Some(a) if !out.contains(a) => out.push(*a),
            Some(_) => {}
            None => bail!(
                "`{word}` is not one of the numbers 1 to {}.",
                Agent::ALL.len()
            ),
        }
    }
    Ok(out)
}

fn names() -> String {
    Agent::ALL.map(Agent::name).join(", ")
}

fn tilde(path: &Path, home: &Path) -> String {
    match path.strip_prefix(home) {
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => path.display().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_by_number_and_enter_takes_every_agent_found() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".codex")).unwrap();
        let env = Env {
            home: dir.path().to_path_buf(),
            ..Env::default()
        };
        assert_eq!(pick("", &env).unwrap(), [Agent::Codex]);
        assert_eq!(
            pick("1 3,1", &env).unwrap(),
            [Agent::Claude, Agent::OpenCode]
        );
        assert!(pick("5", &env).is_err());
        assert!(pick("claude", &env).is_err());
    }

    /// Every `scry <command>` the skill tells an agent to run is a real command, so the
    /// skill cannot drift from the CLI.
    #[test]
    fn the_skill_names_only_real_commands_and_flags() {
        use clap::CommandFactory;
        let cli = crate::Cli::command();
        let text = skill::text();
        let mut seen = 0;
        for line in text.lines() {
            for part in line.split('`').skip(1).step_by(2) {
                let Some(rest) = part.strip_prefix("scry ") else {
                    continue;
                };
                let mut words = rest.split_whitespace();
                let name = words.next().unwrap();
                let Some(sub) = cli.find_subcommand(name) else {
                    panic!("`scry {name}` is not a command: {line}");
                };
                for flag in words.filter(|w| w.starts_with("--")) {
                    let long = flag.trim_start_matches("--");
                    assert!(
                        sub.get_arguments().any(|a| a.get_long() == Some(long)),
                        "`scry {name} {flag}` is not a flag: {line}"
                    );
                }
                seen += 1;
            }
        }
        assert!(seen >= 15, "found {seen} commands in the skill");
    }
}
