//! What uses the CPU and memory: the viewer's processes grouped by the app or program they
//! belong to, ranked by load, and one line for the rest of the machine.
//!
//! Without root the kernel gives CPU time and footprint for the viewer's own processes
//! only, so other users' processes (macOS's own daemons, WindowServer, the kernel) are one
//! line: the machine's totals minus what the viewer's processes account for.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use super::{Detail, Fact, State, Target, human_age, human_bytes};
use crate::attribution::Lineage;
use crate::home::Home;
use crate::model::{ProcId, Process, Reading, Snapshot};

/// The id of the line for everything Scry cannot read.
pub const REST_ID: &str = "rest";

/// One reading of the Top view.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct UsageView {
    /// Logical cores. A share of 100% is every one of them busy.
    pub cores: u32,
    /// The whole machine's CPU load, percent of all cores; `None` until two readings exist.
    pub cpu: Option<f32>,
    /// Memory in use and installed, in bytes.
    pub ram_used: Option<u64>,
    pub ram_total: Option<u64>,
    /// Groups and their processes, flattened depth first, heaviest first.
    pub rows: Vec<UsageRow>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UsageRow {
    pub id: String,
    /// 0 for a group, 1 for a process in it.
    pub depth: u8,
    pub label: String,
    /// The project it runs in, when one is known.
    pub context: Option<String>,
    /// Percent of the whole machine (all cores) since the previous reading.
    pub cpu: Option<f32>,
    pub ram: Option<u64>,
    /// Processes this line stands for.
    pub count: usize,
    pub pid: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_us: Option<i64>,
    pub age_s: Option<u64>,
    pub state: Option<State>,
    pub detail: Detail,
}

impl UsageRow {
    /// What `x` stops: one of the viewer's processes, never a group or the rest.
    pub fn stop_target(&self) -> Option<Target> {
        let pid = self.pid?;
        self.start_us?;
        (self.state != Some(State::System)).then(|| Target {
            pid,
            start_us: self.start_us,
            command: self.label.clone(),
            session: None,
            session_id: None,
            evidence: None,
            age_s: self.age_s.unwrap_or(0),
            frees: self.ram,
            keep: None,
        })
    }
}

/// Turns CPU time into shares: the difference between two snapshots over the wall time
/// between them. Keeps the previous reading's times.
#[derive(Debug, Default)]
pub struct CpuMeter {
    prev: HashMap<ProcId, u64>,
    prev_ms: Option<i64>,
}

impl CpuMeter {
    pub fn new() -> CpuMeter {
        CpuMeter::default()
    }

    /// Each readable process's share of the machine, percent of all `cores`, since the last
    /// call. `None` on the first call, when there is nothing to compare with. A process
    /// that started since then is measured from zero, which its start bounds.
    pub fn shares(&mut self, snap: &Snapshot, cores: u32) -> Option<HashMap<ProcId, f32>> {
        let now: HashMap<ProcId, u64> = snap
            .processes
            .iter()
            .filter_map(|p| Some((p.id, p.cpu_ns?)))
            .collect();
        let prev_ms = self.prev_ms.replace(snap.taken_at_ms);
        let prev = std::mem::replace(&mut self.prev, now);
        let elapsed_ms = snap.taken_at_ms - prev_ms?;
        if elapsed_ms <= 0 {
            return None;
        }
        let budget = elapsed_ms as f64 * 1e6 * f64::from(cores.max(1));
        Some(
            self.prev
                .iter()
                .map(|(id, &ns)| {
                    let since = match prev.get(id) {
                        Some(&before) => ns.saturating_sub(before),
                        None => ns,
                    };
                    (*id, (since as f64 * 100.0 / budget).min(100.0) as f32)
                })
                .collect(),
        )
    }
}

/// The view from one snapshot. `shares` comes from a [`CpuMeter`]; without it the rows
/// rank by memory and show no CPU.
pub fn build(
    snap: &Snapshot,
    lineage: &Lineage,
    home: Option<&Home>,
    cores: u32,
    shares: Option<&HashMap<ProcId, f32>>,
) -> UsageView {
    let cpu = match &snap.pulse.cpu_percent {
        Reading::Ready(v) if shares.is_some() => Some(*v),
        _ => None,
    };
    let (ram_used, ram_total) = match &snap.pulse.ram {
        Reading::Ready(r) => (Some(r.used), Some(r.total)),
        _ => (None, None),
    };
    let share = |p: &Process| shares.and_then(|s| s.get(&p.id).copied());

    let mut groups: HashMap<String, Vec<&Process>> = HashMap::new();
    for p in snap.own_processes() {
        groups.entry(group_of(p)).or_default().push(p);
    }
    let mut built: Vec<(UsageRow, Vec<UsageRow>)> = groups
        .into_iter()
        .map(|(name, mut procs)| {
            procs.sort_by(|a, b| {
                weigh(share(b), b.footprint).total_cmp(&weigh(share(a), a.footprint))
            });
            let children: Vec<UsageRow> = procs
                .iter()
                .map(|p| process_row(snap, lineage, home, p, share(p), 1))
                .collect();
            let head = if procs.len() == 1 {
                UsageRow {
                    id: format!("g-{name}"),
                    depth: 0,
                    ..children[0].clone()
                }
            } else {
                group_row(&name, &children, cores, shares.is_some())
            };
            let children = if procs.len() == 1 {
                Vec::new()
            } else {
                children
            };
            (head, children)
        })
        .collect();

    let own_cpu: f32 = built.iter().filter_map(|(h, _)| h.cpu).sum();
    let own_ram: u64 = built.iter().filter_map(|(h, _)| h.ram).sum();
    let others = snap.processes.len() - snap.own_processes().count();
    built.push((
        rest_row(others, cpu, own_cpu, ram_used, own_ram),
        Vec::new(),
    ));

    built.sort_by(|a, b| {
        weigh(b.0.cpu, b.0.ram)
            .total_cmp(&weigh(a.0.cpu, a.0.ram))
            .then_with(|| a.0.label.cmp(&b.0.label))
    });
    let rows = built
        .into_iter()
        .flat_map(|(head, children)| std::iter::once(head).chain(children))
        .collect();
    UsageView {
        cores,
        cpu,
        ram_used,
        ram_total,
        rows,
    }
}

/// How heavy a line is: CPU first, memory to break ties and to rank without CPU. A
/// gigabyte weighs as much as a tenth of a core's share.
fn weigh(cpu: Option<f32>, ram: Option<u64>) -> f64 {
    f64::from(cpu.unwrap_or(0.0)) + ram.unwrap_or(0) as f64 / (1u64 << 30) as f64 * 0.1
}

/// What a process belongs to: the outermost app bundle it runs from, so Chrome's helpers
/// count as Chrome, else its own name. Tools inside Xcode's developer folder, such as
/// clang, keep their own name.
pub fn group_of(p: &Process) -> String {
    if let Some(exe) = p.exe.as_deref()
        && !exe.contains("/Contents/Developer/")
        && let Some(app) = exe
            .split('/')
            .find(|part| part.len() > 4 && part.ends_with(".app"))
    {
        return app.trim_end_matches(".app").to_string();
    }
    p.name.clone()
}

fn process_row(
    snap: &Snapshot,
    lineage: &Lineage,
    home: Option<&Home>,
    p: &Process,
    cpu: Option<f32>,
    depth: u8,
) -> UsageRow {
    let label = snap.worker_parent(p).unwrap_or(p).label();
    let project = p.project.as_ref().map(|r| short_dir(home, &r.root));
    let session = lineage.session_of(p.id.pid).map(|(s, _)| s);
    let state = if lineage.is_leftover(p.id.pid) {
        Some(State::Leftover)
    } else {
        None
    };
    let age = ((snap.taken_at_ms * 1000 - p.id.start_us).max(0) / 1_000_000) as u64;
    let mut facts = vec![fact("pid", p.id.pid.to_string())];
    if let Some(c) = cpu {
        facts.push(fact("cpu", percent(c)));
    }
    if let Some(r) = p.footprint {
        facts.push(fact("memory", human_bytes(r)));
    }
    facts.push(fact("running", human_age(age)));
    if let Some(c) = &p.cwd {
        facts.push(fact("cwd", shown(home, c)));
    }
    let load = load_words(cpu, p.footprint);
    let sentence = match (session, state) {
        (Some(s), Some(State::Leftover)) => format!(
            "{label} {load}. It is a leftover: the {} session that started it has ended.",
            s.key.agent.title()
        ),
        (Some(s), _) => format!("{label} {load}, under a {} session.", s.key.agent.title()),
        (None, _) => format!("{label} {load}."),
    };
    let mut evidence = Vec::new();
    if let Some(exe) = &p.exe {
        evidence.push(format!("Runs {}", shown(home, exe)));
    }
    if let Some(parent) = snap.process(p.ppid) {
        evidence.push(format!(
            "Started by {} (pid {})",
            parent.name, parent.id.pid
        ));
    }
    UsageRow {
        id: format!("p-{}", p.id.pid),
        depth,
        label: label.clone(),
        context: project,
        cpu,
        ram: p.footprint,
        count: 1,
        pid: Some(p.id.pid),
        start_us: crate::control::guard(p, snap)
            .is_ok()
            .then_some(p.id.start_us),
        age_s: Some(age),
        state,
        detail: Detail {
            title: label,
            facts,
            sentence,
            evidence,
        },
    }
}

fn group_row(name: &str, children: &[UsageRow], cores: u32, measured: bool) -> UsageRow {
    let n = children.len();
    let cpu = measured.then(|| children.iter().filter_map(|c| c.cpu).sum::<f32>());
    let ram_known = children.iter().any(|c| c.ram.is_some());
    let ram = ram_known.then(|| children.iter().filter_map(|c| c.ram).sum::<u64>());
    let mut projects: Vec<&str> = children
        .iter()
        .filter_map(|c| c.context.as_deref())
        .collect();
    projects.sort_unstable();
    projects.dedup();
    let context = match projects.as_slice() {
        [] => None,
        [one] => Some((*one).to_string()),
        many => Some(format!("{} projects", many.len())),
    };
    let leftovers = children
        .iter()
        .filter(|c| c.state == Some(State::Leftover))
        .count();
    let heaviest = &children[0];
    let mut facts = vec![fact("processes", n.to_string())];
    if let Some(c) = cpu {
        facts.push(fact("cpu", percent(c)));
        facts.push(fact(
            "cores busy",
            format!("{:.1} of {cores}", c * cores as f32 / 100.0),
        ));
    }
    if let Some(r) = ram {
        facts.push(fact("memory", human_bytes(r)));
    }
    let mut sentence = format!(
        "{n} {name} processes {} together.",
        load_words(cpu, ram)
            .replacen("uses", "use", 1)
            .replacen("runs", "run", 1)
    );
    sentence.push_str(&format!(
        " The heaviest is {} (pid {}).",
        heaviest.label,
        heaviest.pid.unwrap_or_default()
    ));
    if leftovers > 0 {
        sentence.push_str(&format!(
            " {leftovers} of them {} left over from an ended agent session.",
            if leftovers == 1 { "is" } else { "are" }
        ));
    }
    UsageRow {
        id: format!("g-{name}"),
        depth: 0,
        label: name.to_string(),
        context,
        cpu,
        ram,
        count: n,
        pid: None,
        start_us: None,
        age_s: children.iter().filter_map(|c| c.age_s).max(),
        state: (leftovers > 0).then_some(State::Leftover),
        detail: Detail {
            title: format!("{name} ×{n}"),
            facts,
            sentence,
            evidence: children
                .iter()
                .take(5)
                .map(|c| {
                    format!(
                        "{} (pid {}): {}",
                        c.label,
                        c.pid.unwrap_or_default(),
                        load_words(c.cpu, c.ram)
                    )
                })
                .collect(),
        },
    }
}

fn rest_row(
    count: usize,
    cpu: Option<f32>,
    own_cpu: f32,
    ram_used: Option<u64>,
    own_ram: u64,
) -> UsageRow {
    let cpu = cpu.map(|c| (c - own_cpu).max(0.0));
    // Footprints count shared and compressed pages in each process, so yours can add up
    // to more than the machine has in use. The difference then says nothing.
    let ram = ram_used.filter(|&u| u > own_ram).map(|u| u - own_ram);
    let mut facts = vec![fact("processes", count.to_string())];
    if let Some(c) = cpu {
        facts.push(fact("cpu", percent(c)));
    }
    if let Some(r) = ram {
        facts.push(fact("memory", human_bytes(r)));
    }
    UsageRow {
        id: REST_ID.into(),
        depth: 0,
        label: "macOS and other users".into(),
        context: None,
        cpu,
        ram,
        count,
        pid: None,
        start_us: None,
        age_s: None,
        state: Some(State::System),
        detail: Detail {
            title: "macOS and other users".into(),
            facts,
            sentence: format!(
                "Everything that is not yours: the kernel, WindowServer and macOS's other \
                 services, and processes of other accounts. Together they {}.",
                load_words(cpu, ram)
                    .replacen("uses", "use", 1)
                    .replacen("runs", "run", 1)
            ),
            evidence: vec![
                "Without root, macOS reads CPU time and memory only for your own processes.".into(),
                "This line is the machine's total less what your processes account for. Your \
                 processes' footprints overlap where they share memory, so when they add up \
                 to more than is in use, its memory is not shown."
                    .into(),
                "Activity Monitor or `sudo top -o cpu` breaks it down further.".into(),
            ],
        },
    }
}

/// "uses 41% of the CPU and 1.2G of memory", or the half that is known.
fn load_words(cpu: Option<f32>, ram: Option<u64>) -> String {
    match (cpu, ram) {
        (Some(c), Some(r)) => format!(
            "uses {} of the CPU and {} of memory",
            percent(c),
            human_bytes(r)
        ),
        (Some(c), None) => format!("uses {} of the CPU", percent(c)),
        (None, Some(r)) => format!("uses {} of memory", human_bytes(r)),
        (None, None) => "runs".into(),
    }
}

/// A share as the list shows it: `41%`, `2.5%`, `0.3%`, `0%`.
pub fn percent(v: f32) -> String {
    if v >= 9.95 {
        format!("{v:.0}%")
    } else if v >= 0.05 {
        format!("{v:.1}%")
    } else {
        "0%".into()
    }
}

fn fact(label: &str, value: String) -> Fact {
    Fact {
        label: label.into(),
        value,
    }
}

fn shown(home: Option<&Home>, path: &str) -> String {
    home.map_or_else(|| path.to_string(), |h| h.redact(path))
}

fn short_dir(home: Option<&Home>, path: &str) -> String {
    let p = shown(home, path);
    match p.rsplit_once('/') {
        Some((_, last)) if !last.is_empty() => last.to_string(),
        _ => p,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attribution::{Known, Policy};
    use crate::model::{Ram, SNAPSHOT_SCHEMA};

    fn p(pid: i32, name: &str, exe: &str, ram_mb: u64, cpu_ms: u64) -> Process {
        Process {
            id: ProcId {
                pid,
                start_us: 1_000_000,
            },
            ppid: 1,
            uid: 501,
            name: name.into(),
            exe: Some(exe.into()),
            footprint: Some(ram_mb << 20),
            cpu_ns: Some(cpu_ms * 1_000_000),
            command: None,
            cwd: None,
            project: None,
            stamps: Default::default(),
            worker: false,
            tty: None,
            role: None,
            script: None,
            file_args: false,
        }
    }

    fn snap(at_ms: i64, procs: Vec<Process>) -> Snapshot {
        let mut s = Snapshot {
            schema: SNAPSHOT_SCHEMA,
            taken_at_ms: at_ms,
            viewer_uid: 501,
            processes: procs,
            ..Snapshot::default()
        };
        s.pulse.cpu_percent = Reading::Ready(50.0);
        s.pulse.ram = Reading::Ready(Ram {
            used: 8 << 30,
            total: 16 << 30,
        });
        s
    }

    #[test]
    fn apps_group_by_their_outermost_bundle_and_tools_by_name() {
        let chrome = "/Applications/Google Chrome.app/Contents/Frameworks/Google Chrome Framework.framework/Helpers/Google Chrome Helper (Renderer).app/Contents/MacOS/Google Chrome Helper (Renderer)";
        assert_eq!(
            group_of(&p(10, "Google Chrome He", chrome, 1, 0)),
            "Google Chrome"
        );
        let clang = "/Applications/Xcode.app/Contents/Developer/Toolchains/XcodeDefault.xctoolchain/usr/bin/clang";
        assert_eq!(group_of(&p(11, "clang", clang, 1, 0)), "clang");
        assert_eq!(
            group_of(&p(12, "rustc", "/Users/me/.rustup/bin/rustc", 1, 0)),
            "rustc"
        );
    }

    #[test]
    fn shares_are_the_cpu_time_over_the_wall_time_of_all_cores() {
        let mut m = CpuMeter::new();
        let a = snap(
            1_000,
            vec![p(10, "a", "/bin/a", 1, 100), p(11, "b", "/bin/b", 1, 0)],
        );
        assert!(
            m.shares(&a, 4).is_none(),
            "the first reading has nothing to compare"
        );
        // One second later, a used 2 s of CPU (two cores of four): 50%.
        let b = snap(
            2_000,
            vec![p(10, "a", "/bin/a", 1, 2_100), p(12, "c", "/bin/c", 1, 400)],
        );
        let s = m.shares(&b, 4).unwrap();
        let of = |pid| s.iter().find(|(id, _)| id.pid == pid).map(|(_, v)| *v);
        assert_eq!(of(10), Some(50.0));
        assert_eq!(of(12), Some(10.0), "a new process counts from zero");
        assert_eq!(of(11), None, "a process that exited is gone");
    }

    #[test]
    fn groups_rank_by_load_and_the_rest_is_the_total_less_yours() {
        let procs = vec![
            p(4010, "rustc", "/x/rustc", 300, 0),
            p(4011, "rustc", "/x/rustc", 200, 0),
            p(4012, "zsh", "/bin/zsh", 5, 0),
        ];
        let mut s = snap(2_000, procs);
        let mut root = p(1, "launchd", "/sbin/launchd", 0, 0);
        root.uid = 0;
        root.footprint = None;
        root.cpu_ns = None;
        s.processes.push(root);
        let shares: HashMap<ProcId, f32> = s
            .processes
            .iter()
            .filter(|p| p.uid == 501)
            .map(|p| (p.id, if p.name == "rustc" { 10.0 } else { 1.0 }))
            .collect();
        let lineage = Lineage::build(&s, &Known::default(), &Policy::default());
        let v = build(&s, &lineage, None, 8, Some(&shares));
        let labels: Vec<(u8, &str)> = v.rows.iter().map(|r| (r.depth, r.label.as_str())).collect();
        assert_eq!(
            labels,
            [
                (0, "macOS and other users"),
                (0, "rustc"),
                (1, "rustc"),
                (1, "rustc"),
                (0, "zsh"),
            ]
        );
        let rustc = &v.rows[1];
        assert_eq!(
            (rustc.count, rustc.cpu, rustc.ram),
            (2, Some(20.0), Some(500 << 20))
        );
        let rest = &v.rows[0];
        assert_eq!(rest.cpu, Some(29.0), "50% total less 21% of yours");
        assert_eq!(rest.ram, Some((8 << 30) - (505 << 20)));
        assert_eq!(rest.state, Some(State::System));
        assert!(rest.stop_target().is_none() && rustc.stop_target().is_none());
        assert!(
            v.rows[2].stop_target().is_some(),
            "one of your processes can be stopped"
        );
    }

    #[test]
    fn without_shares_rows_rank_by_memory_and_show_no_cpu() {
        let s = snap(
            2_000,
            vec![p(10, "a", "/bin/a", 10, 0), p(11, "b", "/bin/b", 900, 0)],
        );
        let lineage = Lineage::build(&s, &Known::default(), &Policy::default());
        let v = build(&s, &lineage, None, 8, None);
        assert_eq!(v.cpu, None);
        assert!(v.rows.iter().all(|r| r.cpu.is_none()));
        assert_eq!(v.rows[0].label, "macOS and other users");
        assert_eq!(v.rows[1].label, "b");
    }

    #[test]
    fn percent_keeps_two_figures() {
        assert_eq!(percent(41.4), "41%");
        assert_eq!(percent(2.46), "2.5%");
        assert_eq!(percent(0.01), "0%");
    }
}
