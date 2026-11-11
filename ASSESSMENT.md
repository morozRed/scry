# PLAN.md assessment (arena, 8 Nov 2026)

Goal: decide whether `PLAN.md` is safe to build from, and what to change before writing code.

Four independent runners read the same brief, `PLAN.md` and the source research doc. Each wrote a ranked assessment. I scored them against a fixed rubric, picked a base, merged the best findings from the others, and re-checked the load-bearing claims on this Mac.

## Verdict

Start M0. Do not build M2 (reaping) or the MCP write tools as written. All four runners reached this verdict independently.

The architecture holds up: Rust core, CLI first, one daemon, MCP as thin wrappers, and UI last. The weak part is the part that makes Scry different, which is agent attribution and the destructive actions built on it. Every fix below is a plan edit, not a redesign.

## Scorecard

Each criterion is scored 1 to 5. The rubric is in the arena folder and the runners never saw it.

| Runner | Accuracy | Impact | Actionable | Scope | Safety | Honesty | Total |
|---|---|---|---|---|---|---|---|
| A. Opus (high) | 5 | 5 | 5 | 4 | 5 | 5 | 29 |
| B. Parent model (Opus 5.5) | 5 | 5 | 5 | 5 | 5 | 5 | **30, base** |
| C. Codex `gpt-6.1-sol` (high) | 4 | 4 | 3 | 5 | 5 | 4 | 25 |
| D. OpenCode `deepseek-v4.1-flash` (high) | 3 | 4 | 4 | 5 | 4 | 4 | 24 |

- **B is the base.** It is the only runner to find the strongest attribution signal, the agent's session ID in each child process's environment. Its leftover rule fixes both directions of the orphan problem: it stops false kills, and it catches leaked dev servers, which A's rule misses.
- **A was nearly tied.** It contributed the most grafts.
- **C caught a sequencing bug no one else did.** It also wrote the strongest privacy and protocol contract. But its proposed edits are abstract policy text rather than plan-ready lines, and it says "do not collect process environments", which would throw away the best signal.
- **D was the most practical on scope.** It proposed `doctor`, `bench`, a kill switch, an audit log, and SwiftUI for the first view. It had no web or SDK access and said so, and one of its claims is wrong (see the last verification row).

The cross-judge was skipped. Your judge pool lists Fable and Sonnet, which are both Claude models, the same family as this chat, and the arena rule bars a same-family judge. The base was picked by me alone. B ran on the same model as me, so treat the B-over-A call as one picker's judgment. The findings below don't depend on it.

## What I verified on this Mac

| Claim | Result |
|---|---|
| The orphan rule hits live agent daemons (A, B) | Confirmed. `claude daemon run`, two `claude --bg-pty-host` and `codex app-server daemon` all have ppid 1 and match the plan's patterns. |
| Agents stamp session IDs into child env (B) | Confirmed. This shell, a Claude Code child, has `CLAUDE_CODE_SESSION_ID`, `CLAUDE_PID`, `CLAUDECODE` and `AI_AGENT`. |
| Other users' listeners are visible without root (A, B) | Confirmed. `netstat -anv` shows `launchd:1`, `symptomsd:725`, `rapportd:886`, `ControlCenter:973`. `lsof` without sudo shows only my user's listeners. |
| A clone-aware size API exists (A, B) | Confirmed. `man getattrlist` documents `ATTR_CMNEXT_PRIVATESIZE`, the bytes freed immediately on delete. The plan's §7 row is wrong. |
| kqueue can watch process exit (A, B) | Confirmed. `NOTE_EXIT` is documented. `NOTE_TRACK` is absent, so it can't follow children automatically (B). |
| Codex has hooks (A) | Partly confirmed. `~/.codex/hooks.json` exists here. I did not check the payload fields. |
| `ATTR_FILE_ALLOCSIZE` is 32-bit (D) | **False.** The man page says it is an `off_t`, which is 64-bit. |

Not checked by me, but verified independently by two runners against the Claude Code hooks docs:
- SessionStart stdout is injected into the model's context.
- SessionEnd has a 1.5 s default timeout.
- Exec-form hooks run with no shell.
- `/clear`, `resume`, `compact` and `fork` start new sessions in the same process.
- A `WorktreeCreate` hook replaces git's own worktree creation.

The MCP spec calling annotations untrusted hints was verified by three runners.

## Findings

Sources are in brackets. Where all four runners agree, that is marked "all".

### Blockers

**1. The orphan rule kills agent infrastructure and misses the real leaks.** [all; base B, grafts A, C, D]

On this Mac the rule selects Claude Code's own background-session hosts and the Codex app-server. It also skips the leaked `next`/`vite` dev servers, because they don't match an agent name. Replace §4's orphan rule with a **leftover** rule:

- A process is a leftover only if all of these hold:
  - It is attributed to a session, by env stamp, hook or observed ancestry.
  - That session's agent `(pid, start_time)` is gone.
  - It is not on a never-reap list: `claude daemon`, `--bg-pty-host`, `codex app-server`, and helpers inside `*.app/Contents/`.
  - It has no live controlling tty.
  - It is older than a grace period.
- A process with ppid 1 and no attribution is *unattributed*, and is never selected by `reap`.
- `reap` acts children-first and re-checks identity before every signal. It also honours a persistent "keep running" list. [D, C]
- M2 exit: `reap --dry-run` on a machine running Claude background sessions and the Codex app selects none of their daemons.

**2. The MCP write tools are guarded by the agent itself.** [all]

`confirm: true` is filled in by the model, MCP annotations are only hints, and in bypass-permissions mode nothing prompts. The "under code roots" scope also includes every repo you own. Replace M3's write bullet:

- `stop_process` may target only processes in the calling agent's own session tree. `scry mcp` is spawned by the agent, so it knows its caller from its env and ppid. [A, B]
- Trash takes an opaque `item_id` returned by `disk_usage` or `list_worktrees`, never a free path. [B, C, D]
- Every write sends an MCP elicitation showing the exact target. Without elicitation, the tool returns the CLI command for the human to run. There is no `confirm` argument. [A, B]
- Add a global `destructive = false` kill switch in config, and an audit row for every accepted action. [D]
- Ship write tools last, after attribution has been dogfooded (see the sequence below). [B, C]

### Major

**3. Nothing installs the daemon until M6.** [all]

- Add `scry daemon install|uninstall` in M0. It writes a user LaunchAgent plist and runs `launchctl bootstrap`.
- Add a single-instance lock. [B]
- Add a protocol-version handshake, so a client refuses a daemon with a mismatched version. [A, C]
- Start the `statfs` sampler on day one, so history exists by M4. [B]
- `SMAppService` only works from inside an app bundle (verified by three runners). In M6 the app takes over the daemon and removes the CLI's plist.
- Ship one `scry` binary with a `daemon` subcommand rather than a separate `scryd`. One artifact means no version skew between brew and the app. [A]

**4. Env stamps are the best attribution signal, and the plan doesn't use them.** [B; privacy guard from C]

- Make the env stamp step 1 of resolution. Read `KERN_PROCARGS2` once per `(pid, start_time)`.
- Keep only allowlisted keys: `CLAUDE_CODE_SESSION_ID`, `CLAUDE_PID`, `CODEX_SESSION_ID`, and `SCRY_SESSION`, which our own SessionStart hook writes through `CLAUDE_ENV_FILE`. Drop the rest of the environment in memory.
- These keys are undocumented, so cover them with a fixture test per agent version.
- Use kqueue `NOTE_EXIT` on known agent PIDs to trigger rescans. Don't rely on fast polling. [A, B]

**5. The hook and session model assumes one session per process.** [A, B, C, D]

- Hooks are exec-form, print nothing to stdout, exit 0 always, and finish in under 50 ms.
- A hook sends one line to the socket, or to a spool file if the daemon is down.
- Model a session with `ended_at`/`end_reason`, plus a separate `SessionProcess` table with time ranges.
- The leftover check runs only when the agent process exits, never on `/clear` or `resume`.
- Reconcile on every scan, because SessionEnd doesn't fire on SIGKILL. [D]
- Add Codex and Gemini hooks behind one normalized `SessionEvent`. A verified both from the docs. [A]
- Before writing to `settings.json`, check for managed settings that would override it. [D]
- Never install a `WorktreeCreate` hook. [A, B]

**6. The port collector is blind to other users' sockets.** [A, B; C, D partly]

- Read the socket table from `sysctl net.inet.{tcp,udp}.pcblist_n`. This covers all users and needs no root.
- Join it with `libproc` for details on the user's own processes.
- `find_free_port` test-binds the candidate on `127.0.0.1`, `::1` and the wildcard address. It is still advisory, because the port can be taken between the check and the agent's bind. [B, C, D]
- M1 exit: `scry ports` lists everything `netstat -anv` shows as LISTEN, and may show more.

**7. Disk sizes and cleanup misreport what you get back.** [all]

- Report *allocated* (`ATTR_FILE_ALLOCSIZE`) and *reclaimable* (`ATTR_CMNEXT_PRIVATESIZE`) from one `getattrlistbulk` pass. Rank by reclaimable, because pnpm clones make allocated sizes misleading. Delete the §7 clones row.
- Dedupe hard links by `(dev, ino)`. [C, D]
- Moving to the Trash frees nothing until the Trash is emptied. After a cleanup, say "moved N GB to Trash" and show the Trash's size. [all]
- Each catalog kind declares how it is cleaned up: `trash`, `delete` (regenerable caches, typed confirm), `command` (`docker system prune`, `xcrun simctl delete unavailable`) or `never`. [B]
- Never touch `Docker.raw` directly. [C]
- Show free space using what Finder calls "available", and list snapshots and purgeable space separately. [A, D]
- Forecast only after 48 h of samples, using a robust slope. [A]
- Treat FSEvents as hints that a subtree changed, not as size deltas. Rescan the changed subtrees, and rescan everything after dropped events. [C]

**8. Worktree discovery and "creator" are wrong.** [all]

- A session's starting cwd is usually the main checkout, not the worktree it later creates.
- Codex keeps worktrees in `~/.codex/worktrees`, which is outside the default roots.
- Discover worktrees through each repo's `.git/worktrees/*/gitdir`, plus known agent directories.
- Attribute the creator by, in order:
  1. Path convention, such as `.claude/worktrees/agent-*`.
  2. Env-stamped processes whose cwd is inside the worktree.
  3. Time window, labelled *inferred*.
- Report "unknown" otherwise. [C, D]
- A `.git` file can also mean a submodule, so §4's "`.git` file means a worktree" is wrong. [C, verified against git docs]
- Use the `git` CLI as the source of truth, and `gix` only if profiling demands it. [all]
- Drop "creator" for general disk entries in v1. Show a probable owner plus the evidence for it. [D, C]

**9. M3 promises MCP tools before their collectors exist.** [C]

`disk_usage`, `list_worktrees` and `system_snapshot` are in M3, but their collectors arrive in M4 and M5. Ship each tool with its collector.

**10. The exit criteria are demos.** [all]

- Replace "kill the terminal" with a fixture agent that spawns a child that ignores SIGHUP, then exits.
- Replace "an agent calls `find_free_port`" with a contract test.
- Replace Activity Monitor's "Low" with a measured CPU/power budget (`top -stats power` over 10 idle minutes).
- Name the reference machine and corpus behind "< 30 s cold".
- Add `scry bench scan` in M0, so the cost of a scan without the daemon is measured, not assumed. [D]

### Minor

**11. Privacy and protocol contract.** [C, D]

- Return a redacted command line over MCP. Raw argv can hold tokens.
- `scry snapshot --record` redacts by default, and fixture files stay out of git.
- The socket checks the peer UID and limits frame size.
- `scry mcp` stdout carries protocol only.

**12. The trust wording is misleading.** [C, D]

"No network entitlement" means nothing for an unsandboxed Developer ID app. Say "no network code, no telemetry" instead. Record "Developer ID only, no sandbox, no Mac App Store" as a decision.

**13. Pulse shows fake zeros.** [C, D, A]

- Each metric carries an availability state.
- A one-shot CPU or network reading takes two samples, or says "warming up".
- Trim v1 Pulse to CPU, memory pressure and swap. GPU, battery and network are what Stats already does well. [A, D]

**14. MCP read tools need limits.** [D, B]

- Each read tool gets a 2 s timeout and a result cap.
- `disk_usage` is limited to configured roots and never `/`.
- `scry mcp` exits on stdin EOF or when its ppid becomes 1. Otherwise Scry would leak the very processes it reports. [B]

**15. Too many crates.** [B, C, D]

- Start with `scry-core` (with sys, index, store and proto as modules) and one `scry` binary.
- Add `shell-mac` at the UI milestone.
- Split crates out when a second consumer exists.

## Revised milestones

The runners proposed four different orders. This is the order they converge on.

| # | Content | Days |
|---|---|---|
| M0 | `scry-core` + `scry` binary, CI, `daemon install`, single-instance lock, protocol version, config file (roots, `destructive = false`), `statfs` sampler running, `snapshot --record` (redacted), `bench scan`, `doctor`. | 1 |
| M1 | Ports: pcblist + libproc, env-stamp reader, project roots, `ports`, `why`, CLI `stop` (SIGTERM, explicit escalation). MCP `who_owns_port` and `find_free_port`. | 2 |
| M2 | Agents, read-only: never-reap list, path-based detection with one fixture per install style, `SessionProcess`, silent exec-form hooks for Claude, Codex and Gemini, `agents --tree`, `leftovers`. No killing. Dogfood. | 2 to 3 |
| M3 | The rest of the MCP read tools with caps, contract tests, redaction. | 1 |
| M4 | UI spike, moved earlier [C]. Two days of Rune popover against a SwiftUI `MenuBarExtra` app target (a separate Swift project, not inside a Rust crate) [A, B, D]. Pick one. | 2 |
| M5 | `reap` (leftover rule) and the MCP write tools: caller-scoped, elicitation, `item_id`, audit log. | 2 |
| M6 | Worktrees, then disk with reclaimable sizes, per-kind cleanup, TCC-safe scanning. | 4 |
| M7 | Popover views, notifications, `SMAppService` takeover. | 1 to 2 weeks |
| M8 | Ship: signing, notarization, one Homebrew path. | 2 to 3 |

Week one ends with ports, attribution and read-only MCP in daily use, and with nothing yet able to kill or trash. Disk history is already accumulating.

## Rejected

- **No env reads at all** [C]. Rejected, because env stamps are the strongest signal. The compromise is allowlisted keys only, with the rest discarded.
- **No MCP write tools in v1** [C]. Rejected by the majority (A, B, D). Clean-up-your-own-mess is a headline use case, so write tools ship last, scoped to the caller and behind elicitation.
- **No one-shot path; clients start the daemon** [D]. Rejected. One-shot mode uses the same collectors, so it is not a second implementation. It is limited to current-state reads. [A, C]
- **Ship hooks as a Claude Code plugin by default** [A]. Deferred. The fast, silent hook matters more than the delivery channel. Plugin hooks also can't raise the 1.5 s SessionEnd budget. [B]
- **Delete the disk forecast from v1** [C]. Rejected in favour of a 48-hour minimum and a robust slope [A], with the sampler running from M0.
- **`ATTR_FILE_ALLOCSIZE` is 32-bit** [D]. False. See the verification table.

## Synthesis note

- **Base.** B, the parent-model runner.
- **Cross-judge.** Skipped, because the configured pool has no judge outside the parent's family.
- **Grafts.**
  - From A: the never-reap list, the single binary with a daemon subcommand, Codex and Gemini hooks, the 48-hour forecast guard, Finder-style "available" space, and trimming Pulse.
  - From C: the M3 sequencing bug, the privacy contract, SIGTERM-only stops, FSEvents as invalidation hints, submodule-vs-worktree, metric availability states, and moving the UI spike earlier.
  - From D: `doctor`, `bench`, the kill switch, the audit log, the managed-settings check, read-tool caps, the trust wording, and children-first reaping.
- **Convergence.** All four runners independently found five problems:
  - the daemon install gap
  - the agent-set `confirm` flag
  - the unsafe orphan rule
  - Trash not freeing space
  - worktree "creator" coming from the wrong signal
- **Dropouts.** None. Codex needed one restart, because the arena folder isn't a git repo (`--skip-git-repo-check`).
- **Files.** The raw candidates, brief and rubric are in the arena scratch folder, listed in the chat reply.
