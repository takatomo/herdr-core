//! Ending everything a terminal started. Each terminal's environment carries
//! a random token that its descendants inherit, even those that leave its
//! session (agents' tool shells do), so the token finds them all. A process
//! that both leaves the session and drops its environment escapes: accepted.
//! An exec's record also names the leader of its process group, so a crashed
//! core's next start ends that group too, but only while the leader is
//! provably the process the exec started.

#[cfg(unix)]
use std::time::{Duration, Instant};

pub const VAR: &str = "HC_SCOPE";
/// Marks hc's own cores and holders: never swept, nor anything beneath them
/// (a terminal may start another session's core, which is not its own).
pub const RUNTIME: &str = "HC_RUNTIME";

/// An exec's process group leader, as its record names it: the process (its
/// pid, and its start on `processes()`'s clock) in one boot of the system.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Leader {
    pub pid: i32,
    pub started: u64,
    pub boot: String,
}

impl Leader {
    /// Its record's one line.
    pub fn line(&self) -> String {
        format!("{} {} {}", self.pid, self.started, self.boot)
    }

    /// A record's line; None for an empty or broken one.
    pub fn parse(line: &str) -> Option<Leader> {
        let mut fields = line.split_whitespace();
        let pid = fields.next()?.parse().ok().filter(|pid| *pid > 0)?;
        let started = fields.next()?.parse().ok().filter(|started| *started != u64::MAX)?;
        let boot = fields.next()?.to_string();
        fields.next().is_none().then_some(Leader { pid, started, boot })
    }
}

/// A crashed core's exec record, as read at the next start.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Record {
    Leader(Leader),
    /// Empty or broken: written before its leader was known, or by an older hc.
    NoIdentity,
    /// Could not be read: kept, to look again at the next start.
    Unreadable,
}

/// Whether a record may go: all it names was seen ended, or not (yet).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Done,
    Keep,
}

/// A fresh token for one terminal, stamped with when it was made: no process
/// started before it can carry it, so a sweep need not read those.
pub fn token() -> String {
    format!("{}-{}", now(), crate::platform::random_hex())
}

/// The oldest a process carrying one of `tokens` can be (`now()`'s clock); 0
/// when one has no stamp (made by an older hc).
#[cfg(unix)]
fn oldest(tokens: &[String]) -> u64 {
    let stamp = |t: &String| t.split_once('-').and_then(|(s, _)| s.parse::<u64>().ok());
    tokens.iter().map(stamp).min().flatten().unwrap_or(0)
}

/// Now, on a clock no process started later can show an earlier start on, even
/// when the wall clock is set back: macOS's uptime clock, which process starts
/// are kept on.
#[cfg(target_os = "macos")]
fn now() -> u64 {
    unsafe extern "C" {
        fn mach_absolute_time() -> u64;
    }
    unsafe { mach_absolute_time() }
}

/// Linux, in clock ticks: a process's start is kept on the boot clock
/// (monotonic before 5.3), and a time namespace can shift the two apart, so
/// the lower of them.
#[cfg(target_os = "linux")]
fn now() -> u64 {
    let ns = |clock| {
        let mut t: libc::timespec = unsafe { std::mem::zeroed() };
        if unsafe { libc::clock_gettime(clock, &mut t) } == 0 {
            t.tv_sec as u128 * 1_000_000_000 + t.tv_nsec as u128
        } else {
            0
        }
    };
    let ticks = unsafe { libc::sysconf(libc::_SC_CLK_TCK) }.max(1) as u128;
    (ns(libc::CLOCK_MONOTONIC).min(ns(libc::CLOCK_BOOTTIME)) * ticks / 1_000_000_000) as u64
}

#[cfg(windows)]
fn now() -> u64 {
    0
}

/// On Windows each terminal's job object ends everything it started when the
/// job closes (its program ended, or the holder died): nothing to hunt.
#[cfg(windows)]
pub fn sweep(_: &[String]) {}

#[cfg(windows)]
pub fn members(_: &[String]) -> Vec<i32> {
    Vec::new()
}

#[cfg(windows)]
pub fn leader(_: u32) -> Option<Leader> {
    None
}

#[cfg(windows)]
pub fn sweep_with(_: &[String], records: &[Record]) -> Vec<Outcome> {
    vec![Outcome::Done; records.len()]
}

/// The leader an exec just started, for its record; None when any part of
/// who it is can't be read (its record then names none).
#[cfg(unix)]
pub fn leader(pid: u32) -> Option<Leader> {
    let pid = i32::try_from(pid).ok()?;
    let group::Probe::Live { started: Some(started), .. } = group::probe(pid) else { return None };
    Some(Leader { pid, started, boot: crate::platform::boot_generation()? })
}

/// `sweep` for a crashed core's execs: also each record's group, while its
/// leader is provably itself. Says per record whether it may go.
#[cfg(unix)]
pub fn sweep_with(tokens: &[String], records: &[Record]) -> Vec<Outcome> {
    group::sweep_with(tokens, records)
}

/// Hang up, terminate, kill: each with how long it may take (ms).
#[cfg(unix)]
const STEPS: [(i32, u64); 3] = [(libc::SIGHUP, 500), (libc::SIGTERM, 500), (libc::SIGKILL, 2000)];

/// Hang up, then terminate, then kill every process carrying one of
/// `tokens`, looking again before each step (they may fork). Returns once none
/// remain or KILL had its time.
#[cfg(unix)]
pub fn sweep(tokens: &[String]) {
    for (signal, wait) in STEPS {
        let pids = members(tokens);
        if pids.is_empty() {
            return;
        }
        for pid in pids {
            unsafe { libc::kill(pid, signal) };
        }
        let deadline = Instant::now() + Duration::from_millis(wait);
        while Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(25));
            if members(tokens).is_empty() {
                return;
            }
        }
    }
}

/// Live processes (zombies, us and hc's own processes excluded) whose
/// environment holds `HC_SCOPE=<one of tokens>`, plus their descendants:
/// macOS hides the environment of its own binaries (an agent's `/bin/bash`
/// tool shell), but not their parents.
#[cfg(unix)]
pub fn members(tokens: &[String]) -> Vec<i32> {
    carriers(tokens, &processes().unwrap_or_default())
}

/// `members` among `live`.
#[cfg(unix)]
fn carriers(tokens: &[String], live: &[Proc]) -> Vec<i32> {
    if tokens.is_empty() {
        return Vec::new();
    }
    let entries: std::collections::HashSet<Vec<u8>> =
        tokens.iter().map(|t| format!("{VAR}={t}").into_bytes()).collect();
    let runtime = format!("{RUNTIME}=1").into_bytes();
    let me = std::process::id() as i32;
    let (mut set, mut hc) = (std::collections::HashSet::new(), std::collections::HashSet::new());
    let live: Vec<&Proc> = live.iter().filter(|p| p.pid != me).collect();
    let oldest = oldest(tokens);
    for p in live.iter().filter(|p| p.started >= oldest) {
        let Some(env) = environ(p.pid) else { continue };
        let mut vars = env.split(|b| *b == 0);
        if vars.clone().any(|e| e == runtime) {
            hc.insert(p.pid);
        } else if vars.any(|e| entries.contains(e)) {
            set.insert(p.pid);
        }
    }
    loop {
        let before = set.len();
        let children: Vec<i32> =
            live.iter().filter(|p| set.contains(&p.ppid) && !hc.contains(&p.pid)).map(|p| p.pid).collect();
        set.extend(children);
        if set.len() == before {
            return set.into_iter().collect();
        }
    }
}

/// One of hc's own cores or holders: never ended by a sweep.
#[cfg(unix)]
fn is_hc(pid: i32) -> bool {
    let runtime = format!("{RUNTIME}=1").into_bytes();
    environ(pid).is_some_and(|env| env.split(|b| *b == 0).any(|e| e == runtime))
}

/// A live process; `started` is on `now()`'s clock or later, `u64::MAX` when unknown.
#[cfg(unix)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Proc {
    pid: i32,
    ppid: i32,
    pgid: i32,
    started: u64,
}

/// Every live process, zombies left out; None when they can't be listed
/// (one that can't be read is left out).
#[cfg(unix)]
fn processes() -> Option<Vec<Proc>> {
    Some(
        pids()?
            .into_iter()
            .filter(|&pid| pid > 0)
            .filter_map(read)
            .filter(|(_, zombie)| !zombie)
            .map(|(p, _)| p)
            .collect(),
    )
}

#[cfg(target_os = "linux")]
fn pids() -> Option<Vec<i32>> {
    let dir = std::fs::read_dir("/proc").ok()?;
    Some(dir.flatten().filter_map(|e| e.file_name().to_str()?.parse().ok()).collect())
}

/// One process, and whether it is a zombie; None when it can't be read.
#[cfg(target_os = "linux")]
fn read(pid: i32) -> Option<(Proc, bool)> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let fields: Vec<&str> = stat.rsplit_once(')')?.1.split_whitespace().collect();
    let (state, ppid, pgid) = (*fields.first()?, fields.get(1)?.parse().ok()?, fields.get(2)?.parse().ok()?);
    let started = fields.get(19).and_then(|t| t.parse().ok()).unwrap_or(u64::MAX); // field 22, in ticks
    Some((Proc { pid, ppid, pgid, started }, state == "Z" || state == "X"))
}

/// NUL-separated `KEY=value` entries; a zombie has none.
#[cfg(target_os = "linux")]
fn environ(pid: i32) -> Option<Vec<u8>> {
    std::fs::read(format!("/proc/{pid}/environ")).ok().filter(|env| !env.is_empty())
}

#[cfg(target_os = "macos")]
fn pids() -> Option<Vec<i32>> {
    let n = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
    if n <= 0 {
        return None;
    }
    let mut pids = vec![0i32; n as usize + 64];
    let size = (pids.len() * std::mem::size_of::<i32>()) as libc::c_int;
    let got = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), size) };
    if got <= 0 {
        return None;
    }
    pids.truncate(got as usize);
    Some(pids)
}

/// One process, and whether it is a zombie; None when it can't be read.
#[cfg(target_os = "macos")]
fn read(pid: i32) -> Option<(Proc, bool)> {
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    let got = unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, (&raw mut info).cast(), size) };
    if got != size {
        return None;
    }
    const SZOMB: u32 = 5;
    let mut usage: libc::rusage_info_v0 = unsafe { std::mem::zeroed() };
    let read = unsafe { libc::proc_pid_rusage(pid, libc::RUSAGE_INFO_V0, (&raw mut usage).cast()) };
    let started = if read == 0 { usage.ri_proc_start_abstime } else { u64::MAX };
    let process = Proc { pid, ppid: info.pbi_ppid as i32, pgid: info.pbi_pgid as i32, started };
    Some((process, info.pbi_status == SZOMB))
}

#[cfg(target_os = "macos")]
fn environ(pid: i32) -> Option<Vec<u8>> {
    crate::foreground::procargs(pid).map(|(_, env)| env)
}

/// A crashed core's exec groups. A record's leader is the process only when
/// this boot, its pid, its start and its group all match; then it and its
/// group are ended, each member by its own pid and only while it is still the
/// one first seen (never `kill(-pgid)`). What can't be read is never taken
/// for gone: it is not signalled, and its record is kept for the next start.
/// Between the last look at a pid and its signal it may still end and its pid
/// be reused: as with the token's sweep, that is not ruled out.
#[cfg(unix)]
mod group {
    use super::{Leader, Outcome, Proc, Record, STEPS, carriers, is_hc, processes, read};
    use std::time::{Duration, Instant};

    /// One pid, read on its own.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum Probe {
        Live {
            pgid: i32,
            started: Option<u64>,
        },
        /// None has it (a zombie has ended too).
        Absent,
        Unreadable,
    }

    pub fn probe(pid: i32) -> Probe {
        if pid <= 0 {
            return Probe::Unreadable;
        }
        match read(pid) {
            Some((_, true)) => Probe::Absent,
            Some((p, false)) => Probe::Live { pgid: p.pgid, started: (p.started != u64::MAX).then_some(p.started) },
            None => {
                let exists = unsafe { libc::kill(pid, 0) } == 0;
                unread(if exists { None } else { std::io::Error::last_os_error().raw_os_error() })
            }
        }
    }

    /// A pid that exists for `kill(pid, 0)` or failed it with `error`: gone
    /// only when the system says no such process.
    fn unread(error: Option<i32>) -> Probe {
        if error == Some(libc::ESRCH) { Probe::Absent } else { Probe::Unreadable }
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum Verdict {
        /// Still the process the exec started, leading its group.
        Same,
        /// Shown not to be: another boot, no such pid, another start, or it left its group.
        Gone,
        /// Can't tell.
        Unknown,
    }

    fn verify(now: Probe, leader: &Leader, boot: Option<&str>) -> Verdict {
        let Some(boot) = boot else { return Verdict::Unknown };
        if boot != leader.boot {
            return Verdict::Gone;
        }
        match now {
            Probe::Absent => Verdict::Gone,
            Probe::Unreadable | Probe::Live { started: None, .. } => Verdict::Unknown,
            Probe::Live { started: Some(started), pgid } if started == leader.started && pgid == leader.pid => {
                Verdict::Same
            }
            Probe::Live { .. } => Verdict::Gone,
        }
    }

    /// Whom of `leader`'s group to look at now, as (pid, start): those found
    /// before (`kept`) and, while the leader is itself, it and its group.
    /// Never us or hc's own (`skip`).
    fn candidates(
        kept: &[(i32, u64)],
        verdict: Verdict,
        leader: &Leader,
        live: &[Proc],
        skip: impl Fn(i32) -> bool,
    ) -> Vec<(i32, u64)> {
        let mut all = kept.to_vec();
        if verdict == Verdict::Same {
            let group = live.iter().filter(|p| p.pgid == leader.pid && p.started != u64::MAX);
            for found in std::iter::once((leader.pid, leader.started)).chain(group.map(|p| (p.pid, p.started))) {
                if !all.contains(&found) {
                    all.push(found);
                }
            }
        }
        all.retain(|(pid, _)| !skip(*pid));
        all
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Member {
        /// Still the one first seen, still in the group.
        Send,
        /// Gone, or its pid is another process's now.
        Ended,
        /// Can't tell, or it left the group: not signalled, not taken for gone.
        Unconfirmed,
    }

    fn member(now: Probe, started: u64, leader: i32) -> Member {
        match now {
            Probe::Absent => Member::Ended,
            Probe::Live { started: Some(now), .. } if now != started => Member::Ended,
            Probe::Live { started: Some(_), pgid } if pgid == leader => Member::Send,
            _ => Member::Unconfirmed,
        }
    }

    /// A record may go only when its processes were seen ended: the list was
    /// read, nothing carries a token, and its leader was shown gone with none
    /// of its group left or unknown.
    fn outcome(record: &Record, verdict: Option<Verdict>, group_left: bool, listed: bool, carried: bool) -> Outcome {
        let ended = listed
            && !carried
            && match record {
                Record::Leader(_) => verdict == Some(Verdict::Gone) && !group_left,
                Record::NoIdentity => true,
                Record::Unreadable => false,
            };
        if ended { Outcome::Done } else { Outcome::Keep }
    }

    /// One look: what carries a token, and per record its leader's verdict;
    /// `kept` becomes each group's members not seen ended. With a signal, each
    /// is sent it right after its look.
    struct Look {
        listed: bool,
        carriers: Vec<i32>,
        verdicts: Vec<Option<Verdict>>,
    }

    fn look(
        tokens: &[String],
        records: &[Record],
        kept: &mut [Vec<(i32, u64)>],
        boot: Option<&str>,
        signal: Option<i32>,
    ) -> Look {
        let me = std::process::id() as i32;
        let live = processes();
        let carriers = live.as_deref().map(|live| carriers(tokens, live)).unwrap_or_default();
        let send = |pid: i32| {
            if let Some(signal) = signal {
                unsafe { libc::kill(pid, signal) };
            }
        };
        for &pid in &carriers {
            send(pid);
        }
        let mut verdicts = Vec::with_capacity(records.len());
        for (record, kept) in records.iter().zip(kept.iter_mut()) {
            let Record::Leader(leader) = record else {
                verdicts.push(None);
                continue;
            };
            let verdict = verify(probe(leader.pid), leader, boot);
            let skip = |pid| pid == me || is_hc(pid);
            let mut still = Vec::new();
            for (pid, started) in candidates(kept, verdict, leader, live.as_deref().unwrap_or_default(), skip) {
                match member(probe(pid), started, leader.pid) {
                    Member::Send => {
                        send(pid);
                        still.push((pid, started));
                    }
                    Member::Unconfirmed => still.push((pid, started)),
                    Member::Ended => {}
                }
            }
            *kept = still;
            verdicts.push(Some(verdict));
        }
        Look { listed: live.is_some(), carriers, verdicts }
    }

    /// Nothing left to signal or to make sure of.
    fn settled(seen: &Look, kept: &[Vec<(i32, u64)>]) -> bool {
        seen.listed && seen.carriers.is_empty() && kept.iter().all(Vec::is_empty)
    }

    pub fn sweep_with(tokens: &[String], records: &[Record]) -> Vec<Outcome> {
        let boot = crate::platform::boot_generation();
        let mut kept = vec![Vec::new(); records.len()];
        let mut last = None;
        for (signal, wait) in STEPS {
            let mut seen = look(tokens, records, &mut kept, boot.as_deref(), Some(signal));
            let deadline = Instant::now() + Duration::from_millis(wait);
            while !settled(&seen, &kept) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(25));
                seen = look(tokens, records, &mut kept, boot.as_deref(), None);
            }
            let done = settled(&seen, &kept);
            last = Some(seen);
            if done {
                break;
            }
        }
        let seen = last.unwrap_or_else(|| look(tokens, records, &mut kept, boot.as_deref(), None));
        let carried = !seen.carriers.is_empty();
        records
            .iter()
            .zip(&seen.verdicts)
            .zip(&kept)
            .map(|((record, verdict), group)| outcome(record, *verdict, !group.is_empty(), seen.listed, carried))
            .collect()
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        const BOOT: &str = "boot-a";

        fn leader() -> Leader {
            Leader { pid: 100, started: 7, boot: BOOT.into() }
        }

        fn live(pgid: i32, started: u64) -> Probe {
            Probe::Live { pgid, started: Some(started) }
        }

        fn proc(pid: i32, pgid: i32, started: u64) -> Proc {
            Proc { pid, ppid: 1, pgid, started }
        }

        #[test]
        fn a_leader_is_itself_only_in_this_boot_with_its_pid_start_and_group() {
            use Verdict::*;
            let unknown_start = Probe::Live { pgid: 100, started: None };
            for (now, boot, verdict) in [
                (live(100, 7), Some(BOOT), Same),
                (live(100, 7), Some("boot-b"), Gone),
                (live(100, 8), Some(BOOT), Gone),
                (Probe::Absent, Some(BOOT), Gone),
                (live(55, 7), Some(BOOT), Gone),
                (live(100, 7), None, Unknown),
                (Probe::Unreadable, Some(BOOT), Unknown),
                (unknown_start, Some(BOOT), Unknown),
            ] {
                assert_eq!(verify(now, &leader(), boot), verdict, "{now:?} {boot:?}");
            }
        }

        #[test]
        fn only_a_leader_that_is_itself_brings_in_its_group() {
            let list =
                [proc(100, 100, 7), proc(101, 100, 9), proc(102, 100, u64::MAX), proc(103, 50, 9), proc(104, 100, 9)];
            let kept = [(90, 3)];
            let skip = |pid| pid == 104;
            let same = candidates(&kept, Verdict::Same, &leader(), &list, skip);
            assert_eq!(same, [(90, 3), (100, 7), (101, 9)], "its group, never an unknown start, never ours");
            for verdict in [Verdict::Gone, Verdict::Unknown] {
                assert_eq!(candidates(&kept, verdict, &leader(), &list, skip), [(90, 3)], "only those already found");
            }
            assert_eq!(candidates(&kept, Verdict::Gone, &leader(), &[], |pid| pid == 90), [], "ours never, even kept");
        }

        #[test]
        fn a_member_is_signalled_only_while_it_is_the_one_first_seen() {
            use Member::*;
            for (now, member_now) in [
                (live(100, 9), Send),
                (live(100, 10), Ended),
                (Probe::Absent, Ended),
                (live(55, 9), Unconfirmed),
                (Probe::Unreadable, Unconfirmed),
                (Probe::Live { pgid: 100, started: None }, Unconfirmed),
            ] {
                assert_eq!(member(now, 9, 100), member_now, "{now:?}");
            }
        }

        #[test]
        fn a_pid_that_cant_be_read_is_gone_only_when_none_has_it() {
            assert_eq!(unread(Some(libc::ESRCH)), Probe::Absent);
            assert_eq!(unread(Some(libc::EPERM)), Probe::Unreadable);
            assert_eq!(unread(None), Probe::Unreadable);
            let me = std::process::id() as i32;
            assert!(matches!(probe(me), Probe::Live { started: Some(_), .. }), "{:?}", probe(me));
        }

        #[test]
        fn a_record_goes_only_once_all_it_names_was_seen_ended() {
            use Outcome::*;
            let named = Record::Leader(leader());
            let gone = Some(Verdict::Gone);
            for (record, verdict, group_left, listed, carried, outcome_now) in [
                (&named, gone, false, true, false, Done),
                (&named, gone, true, true, false, Keep),
                (&named, Some(Verdict::Same), true, true, false, Keep),
                (&named, Some(Verdict::Unknown), false, true, false, Keep),
                (&named, gone, false, false, false, Keep),
                (&named, gone, false, true, true, Keep),
                (&Record::NoIdentity, None, false, true, false, Done),
                (&Record::NoIdentity, None, false, true, true, Keep),
                (&Record::NoIdentity, None, false, false, false, Keep),
                (&Record::Unreadable, None, false, true, false, Keep),
            ] {
                let got = outcome(record, verdict, group_left, listed, carried);
                assert_eq!(
                    got, outcome_now,
                    "{record:?} {verdict:?} left {group_left} listed {listed} carried {carried}"
                );
            }
        }

        #[test]
        fn a_leader_record_reads_back_and_a_broken_one_names_none() {
            assert_eq!(Leader::parse(&leader().line()), Some(leader()));
            for broken in
                ["", "100", "100 7", "x 7 boot", "0 7 boot", "100 18446744073709551615 boot", "100 7 boot extra"]
            {
                assert_eq!(Leader::parse(broken), None, "{broken:?}");
            }
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn finds_only_processes_carrying_the_token() {
        let tokens = [token()];
        // Not macOS's own `sleep`: it hides its environment.
        let sleeper = |scope: &str| python("import time; time.sleep(30)").env(VAR, scope).spawn().unwrap();
        let (mut ours, mut other) = (sleeper(&tokens[0]), sleeper("someone-else"));
        assert_eq!(members_settled(&tokens, 1), [ours.id() as i32]);
        sweep(&tokens);
        assert!(ours.wait().unwrap().signal().is_some(), "it was ended");
        assert!(members(&tokens).is_empty());
        other.kill().unwrap();
        other.wait().unwrap();
    }

    #[test]
    fn a_child_that_dropped_the_token_still_counts_while_its_parent_does() {
        let tokens = [token()];
        // Like an agent running a tool shell that no longer shows the token.
        let child = "import os, subprocess; env = dict(os.environ); env.pop('HC_SCOPE'); subprocess.run(['sleep', '30'], env=env)";
        let mut parent = python(child).env(VAR, &tokens[0]).spawn().unwrap();
        assert_eq!(members_settled(&tokens, 2).len(), 2, "the shell and its sleep");
        sweep(&tokens);
        parent.wait().unwrap();
        assert!(members(&tokens).is_empty());
    }

    #[test]
    fn a_process_older_than_its_token_is_never_read() {
        // A process older than the token cannot carry it, so its environment need not be read.
        let later = now() * 2 + 1_000_000;
        let (tokens, found) = ([format!("{later}-{}", crate::platform::random_hex())], [token()]);
        let mut sleeper = python("import time; time.sleep(30)").env(VAR, &tokens[0]).spawn().unwrap();
        let mut seen = python("import time; time.sleep(30)").env(VAR, &found[0]).spawn().unwrap();
        assert_eq!(members_settled(&found, 1), [seen.id() as i32], "a fresh token finds its own");
        assert!(members(&tokens).is_empty(), "a token from far ahead is newer than every process");
        for child in [&mut sleeper, &mut seen] {
            child.kill().unwrap();
            child.wait().unwrap();
        }
    }

    /// Linux: the same inside a time namespace whose clocks are shifted apart.
    #[test]
    #[cfg(target_os = "linux")]
    fn a_fresh_token_finds_its_own_in_a_shifted_time_namespace() {
        let me = std::env::current_exe().unwrap();
        let test = "sweep::tests::a_fresh_token_finds_its_own";
        let inside = std::process::Command::new("unshare")
            .args(["-UrTf", "--monotonic", "1000000000", "--boottime", "0"])
            .arg(me)
            .args([test, "--exact", "--ignored"])
            .output();
        match inside {
            Ok(out) if out.status.success() => {}
            // No user or time namespaces here (an old kernel, a locked-down container).
            Ok(out) if String::from_utf8_lossy(&out.stderr).contains("unshare") => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            other => panic!("inside: {other:?}"),
        }
    }

    #[test]
    #[ignore = "run by the test above, inside a time namespace"]
    fn a_fresh_token_finds_its_own() {
        let tokens = [token()];
        let mut ours = python("import time; time.sleep(30)").env(VAR, &tokens[0]).spawn().unwrap();
        assert_eq!(members_settled(&tokens, 1), [ours.id() as i32]);
        ours.kill().unwrap();
        ours.wait().unwrap();
    }

    /// `members` once it finds `n` (5 s at most): macOS's python3 is a stub
    /// that starts the real one, and can't be read in between.
    fn members_settled(tokens: &[String], n: usize) -> Vec<i32> {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let found = members(tokens);
            if found.len() == n || Instant::now() > deadline {
                return found;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    use std::os::unix::process::ExitStatusExt;

    fn python(code: &str) -> std::process::Command {
        let mut cmd = std::process::Command::new("python3");
        cmd.args(["-c", code]);
        cmd
    }
}
