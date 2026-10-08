#!/usr/bin/env python3
"""CPU / memory / disk-I/O profiler for filex (UI) and filex-indexd (daemon + workers), or
Finder for comparison. macOS only.

Samples libproc's proc_pid_rusage (the counters Activity Monitor shows: "Memory" is
phys_footprint, "Bytes Read/Written" is diskio) for every live process of the chosen app,
plus Spotlight (mds*, mdworker_shared, corespotlightd) as its own role: Finder's index lives
there, and filex's file churn wakes it too.

  scripts/profile.py watch [--app finder]       sample until Ctrl-C (drive the app yourself)
  scripts/profile.py scenario [--app finder]    launch the app and drive it with System Events
                                                keystrokes through scripted phases

Full disk I/O including root-owned Spotlight (two terminals, same --app and --phase-file):
  sudo python3 scripts/profile.py watch --app finder --phase-file /tmp/fx-phase --out /tmp/finder-io.csv
  python3 scripts/profile.py scenario --app finder --phase-file /tmp/fx-phase

The scenario needs Accessibility (+ Screen Recording for screenshots) for the terminal.
Without sudo, root-owned mds/mds_stores report CPU and RSS only (via ps), no disk I/O.
"""
import argparse, csv, ctypes, os, shutil, signal, subprocess, sys, threading, time
from pathlib import Path

APPS = {
    "filex": {"filex": "ui", "filex-indexd": "daemon"},
    "finder": {"Finder": "ui", "QuickLookUIService": "ui", "com.apple.quicklook.ThumbnailsAgent": "ui"},
}
SPOTLIGHT = {n: "spotlight" for n in ("mds", "mds_stores", "mdworker_shared", "corespotlightd")}
ROLES = {**APPS["filex"], **SPOTLIGHT}
ROOT = Path(__file__).resolve().parent.parent

lib = ctypes.CDLL("/usr/lib/libSystem.B.dylib")
u64 = ctypes.c_uint64


class RusageV4(ctypes.Structure):
    _fields_ = [("uuid", ctypes.c_uint8 * 16)] + [(n, u64) for n in (
        "user_time system_time pkg_idle_wkups interrupt_wkups pageins wired_size resident_size "
        "phys_footprint proc_start_abstime proc_exit_abstime child_user_time child_system_time "
        "child_pkg_idle_wkups child_interrupt_wkups child_pageins child_elapsed_abstime "
        "diskio_bytesread diskio_byteswritten qos_default qos_maintenance qos_background "
        "qos_utility qos_legacy qos_user_initiated qos_user_interactive billed_system_time "
        "serviced_system_time logical_writes lifetime_max_phys_footprint instructions cycles "
        "billed_energy serviced_energy interval_max_phys_footprint runnable_time").split()]


class Timebase(ctypes.Structure):
    _fields_ = [("numer", ctypes.c_uint32), ("denom", ctypes.c_uint32)]


_tb = Timebase()
lib.mach_timebase_info(ctypes.byref(_tb))
TICK_NS = _tb.numer / _tb.denom  # rusage CPU times are mach ticks (41.67 ns on Apple Silicon)


def target_pids():
    """{pid: role} for live processes whose binary name is in ROLES (any build location)."""
    buf = (ctypes.c_int * 8192)()
    n = lib.proc_listallpids(buf, ctypes.sizeof(buf))
    path = ctypes.create_string_buffer(4096)
    out = {}
    for pid in buf[:n]:
        if pid > 0 and lib.proc_pidpath(pid, path, 4096) > 0:
            role = ROLES.get(os.path.basename(path.value.decode(errors="replace")))
            if role:
                out[pid] = role
    return out


def rusage(pid):
    r = RusageV4()
    return r if lib.proc_pid_rusage(pid, 4, ctypes.byref(r)) == 0 else None


def ps_usage(pids):
    """Root-owned processes (mds, mds_stores) refuse proc_pid_rusage without sudo, but ps still
    reports CPU time (10 ms resolution) and RSS. Disk I/O and wakeups stay 0 for these."""
    out = subprocess.run(["ps", "-o", "pid=,time=,rss=", "-p", ",".join(map(str, pids))],
                         capture_output=True, text=True).stdout
    res = {}
    for line in out.splitlines():
        pid, cpu, rss = line.split()
        r = RusageV4()
        r.user_time = int(sum(float(x) * 60**i for i, x in enumerate(reversed(cpu.split(":")))) * 1e9 / TICK_NS)
        r.phys_footprint = r.resident_size = int(rss) * 1024
        res[int(pid)] = r
    return res


class Sampler(threading.Thread):
    """Every `interval` s, one row per role: cpu %, footprint, disk bytes/s, wakeups/s.

    ponytail: only live pids are sampled, so a worker that exits loses its last <interval of
    counters. Fine at 250 ms; use the parent's ri_child_* fields if workers get very short-lived.
    """

    def __init__(self, interval=0.25, phase_file=None, follow=False):
        super().__init__(daemon=True)
        self.phase_file, self.follow = phase_file and Path(phase_file), follow
        self.interval, self.rows, self.stop = interval, [], threading.Event()
        self.phase = "start"

    # With --phase-file, the scenario (as you) publishes its phase and a `sudo … watch` sampler
    # follows it, so root-owned mds/mds_stores get real disk I/O without driving apps as root.
    @property
    def phase(self):
        if self.follow:
            try:
                return self.phase_file.read_text().strip() or "waiting"
            except OSError:
                return "waiting"
        return self._phase

    @phase.setter
    def phase(self, value):
        self._phase = value
        if self.phase_file and not self.follow:
            self.phase_file.write_text(value)
        self.t0 = time.monotonic()

    def run(self):
        prev, prev_t = {}, time.monotonic()
        while not self.stop.wait(self.interval):
            cur, agg = {}, {}
            pids = target_pids()
            usage = {pid: rusage(pid) for pid in pids}
            denied = [pid for pid, r in usage.items() if r is None]
            if denied:  # ps can take seconds under load: re-read the rest after it, then stamp
                usage = {**ps_usage(denied), **{pid: rusage(pid) for pid in pids if pid not in denied}}
            now = time.monotonic()
            dt = now - prev_t
            for pid, role in pids.items():
                r = usage.get(pid)
                if r is None:
                    continue
                cur[pid] = r
                a = agg.setdefault(role, dict(procs=0, cpu_pct=0.0, footprint_mb=0.0, rss_mb=0.0,
                                              read_kbs=0.0, write_kbs=0.0, logical_write_kbs=0.0,
                                              wakeups_s=0.0, peak_footprint_mb=0.0))
                a["procs"] += 1
                a["footprint_mb"] += r.phys_footprint / 2**20
                a["rss_mb"] += r.resident_size / 2**20
                a["peak_footprint_mb"] += r.lifetime_max_phys_footprint / 2**20
                p = prev.get(pid)
                if p is not None:  # first sight of a pid only sets its baseline
                    a["cpu_pct"] += ((r.user_time + r.system_time) - (p.user_time + p.system_time)) * TICK_NS / 1e9 / dt * 100
                    a["read_kbs"] += (r.diskio_bytesread - p.diskio_bytesread) / 1024 / dt
                    a["write_kbs"] += (r.diskio_byteswritten - p.diskio_byteswritten) / 1024 / dt
                    a["logical_write_kbs"] += (r.logical_writes - p.logical_writes) / 1024 / dt
                    a["wakeups_s"] += ((r.pkg_idle_wkups + r.interrupt_wkups) - (p.pkg_idle_wkups + p.interrupt_wkups)) / dt
            for role, a in agg.items():
                self.rows.append(dict(t=round(now - self.t0, 3), dt=round(dt, 3), phase=self.phase, role=role,
                                      **{k: round(v, 2) for k, v in a.items()}))
            prev, prev_t = cur, now


def summarize(rows):
    phases = list(dict.fromkeys(r["phase"] for r in rows))
    hdr = f"{'phase':<22}{'role':<11}{'secs':>6}{'cpu avg%':>10}{'cpu p95%':>10}{'cpu max%':>10}" \
          f"{'mem avg MB':>12}{'mem max MB':>12}{'read MB':>9}{'write MB':>10}{'wake/s':>8}"
    print(hdr)
    print("-" * len(hdr))
    for ph in phases:
        for role in ("ui", "daemon", "spotlight"):
            rs = [r for r in rows if r["phase"] == ph and r["role"] == role]
            if not rs:
                continue
            secs = sum(float(r["dt"]) for r in rs)  # rows are rates; weight by real interval
            avg = lambda k: sum(float(r[k]) * float(r["dt"]) for r in rs) / secs
            total_mb = lambda k: sum(float(r[k]) * float(r["dt"]) for r in rs) / 1024
            cpu = sorted(float(r["cpu_pct"]) for r in rs)
            mem = [float(r["footprint_mb"]) for r in rs]
            print(f"{ph:<22}{role:<11}{secs:>6.1f}{avg('cpu_pct'):>10.1f}{cpu[int(0.95*(len(cpu)-1))]:>10.1f}"
                  f"{cpu[-1]:>10.1f}{avg('footprint_mb'):>12.1f}{max(mem):>12.1f}"
                  f"{total_mb('read_kbs'):>9.1f}{total_mb('write_kbs'):>10.1f}{avg('wakeups_s'):>8.0f}")


def write_csv(rows, out):
    if rows:
        with open(out, "w", newline="") as f:
            w = csv.DictWriter(f, fieldnames=list(rows[0]))
            w.writeheader()
            w.writerows(rows)
        print(f"\n{len(rows)} samples -> {out}")


# ---------------------------------------------------------------- scenario driving

def osa(script):
    subprocess.run(["osascript", "-e", script], check=True, capture_output=True)


def keys(text, delay=0.0):
    """Type `text` char by char (search-as-you-type), `delay` s between chars."""
    for ch in text:
        osa(f'tell application "System Events" to keystroke "{ch}"')
        time.sleep(delay)


def chord(key, *mods, code=None):
    using = f" using {{{', '.join(m + ' down' for m in mods)}}}" if mods else ""
    what = f"key code {code}" if code is not None else f'keystroke "{key}"'
    osa(f'tell application "System Events" to {what}{using}')


ENTER, ESC, DOWN, PGDN = 36, 53, 125, 121


class FilexDriver:
    location, search = ("l", "command"), ("k", "command")

    def launch(self):
        env = dict(os.environ, RUST_LOG=os.environ.get("RUST_LOG", "filex=debug"))
        self.app = subprocess.Popen([str(ROOT / "target/release/filex")], env=env,
                                    stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        return self.app.pid

    def quit(self):
        chord("q", "command")
        try:
            self.app.wait(timeout=10)
        except subprocess.TimeoutExpired:
            self.app.terminate()


class FinderDriver:
    """Same keystrokes, Finder's bindings: Go to Folder (⇧⌘G) and ⌘F, which searches This Mac
    through Spotlight (names *and* contents, so not identical work to filex's name search)."""
    location, search = ("g", "command", "shift"), ("f", "command")

    def launch(self):  # relaunch so launch cost and memory start fresh, like filex
        subprocess.run(["killall", "Finder"], capture_output=True)
        time.sleep(1)
        subprocess.run(["open", "-a", "Finder", str(Path.home())], check=True)
        for _ in range(40):
            pid = next((p for p, r in target_pids().items() if r == "ui"), None)
            if pid:
                return pid
            time.sleep(0.25)
        raise RuntimeError("Finder did not relaunch")

    def quit(self):  # Finder has no Quit; close its windows (and any live search with them)
        osa('tell application "Finder" to close every window')


def go(driver, path):
    chord(*driver.location)
    time.sleep(0.4)
    chord("a", "command")
    keys(path)
    time.sleep(0.3)
    chord(None, code=ENTER)


def focus(pid):
    osa(f'tell application "System Events" to set frontmost of (first process whose unix id is {pid}) to true')


def make_tree(root, n_files, n_dirs=0):
    root.mkdir(parents=True, exist_ok=True)
    for i in range(n_files):
        (root / f"file_{i:06d}.txt").touch()
    for i in range(n_dirs):
        (root / f"dir_{i:04d}").mkdir(exist_ok=True)


def scenario(s, args):
    big = Path(args.big_dir)
    churn = Path(args.churn_dir)
    shutil.rmtree(churn, ignore_errors=True)
    if not (big.is_dir() and len(os.listdir(big)) >= args.big_files):
        print(f"creating {args.big_files} files in {big} …")
        make_tree(big, args.big_files, 200)

    if args.cold and args.app == "filex":
        s.phase = "daemon_restart"
        daemons = [pid for pid, role in target_pids().items() if role == "daemon"]
        for pid in daemons:
            os.kill(pid, signal.SIGTERM)
        for _ in range(20):
            if not set(daemons) & set(target_pids()):
                break
            time.sleep(0.25)
        else:  # builds predating the SIGTERM handler ignore it
            for pid in set(daemons) & set(target_pids()):
                os.kill(pid, signal.SIGKILL)
        time.sleep(1)

    driver = FinderDriver() if args.app == "finder" else FilexDriver()
    s.phase = "launch"
    pid = driver.launch()
    time.sleep(args.settle)
    focus(pid)
    time.sleep(1)

    s.phase = "idle_after_launch"
    time.sleep(args.idle)

    s.phase = "navigate"
    for p in [str(Path.home()), "/Applications", "/usr/lib", "/System/Library/Frameworks",
              str(Path.home() / "Library"), "/usr/bin", str(Path.home())]:
        go(driver, p)
        time.sleep(1.5)
    for _ in range(5):  # history
        chord("[", "command"); time.sleep(0.6)
    for _ in range(5):
        chord("]", "command"); time.sleep(0.6)

    s.phase = "huge_folder_open"
    go(driver, str(big))
    time.sleep(4)

    s.phase = "huge_folder_scroll"
    for _ in range(60):
        chord(None, code=PGDN); time.sleep(0.1)
    for _ in range(200):
        chord(None, code=DOWN); time.sleep(0.02)
    shot(args, "huge_folder")

    s.phase = "search_typing"
    # Search from home in both apps: Finder's ⌘F honours FXDefaultSearchScope, and on "current
    # folder" an unindexed folder (like big/ in /tmp) turns into an mds crawl that pins every core
    # for as long as the search stays open.
    go(driver, str(Path.home()))
    time.sleep(1.5)
    for q in ["cargo", "readme.md", "png", "main.rs", "file_04", "zzqxj"]:
        chord(*driver.search); time.sleep(0.2)
        chord("a", "command")
        keys(q, delay=0.12)  # ~ fast typist
        time.sleep(1.5)
        if q == "png":
            shot(args, "search_png")
        chord(None, code=ESC); time.sleep(0.4)
    go(driver, str(Path.home()))  # leave search mode: an open Finder search is a live mds query
    time.sleep(1)

    s.phase = "fs_churn"
    make_tree(churn, args.churn_files)
    time.sleep(1)
    shutil.rmtree(churn)
    time.sleep(1)
    make_tree(churn, args.churn_files)
    shutil.rmtree(churn)

    s.phase = "after_churn_settle"
    time.sleep(args.idle)

    s.phase = "idle_end"
    time.sleep(args.idle)
    shot(args, "end")

    s.phase = "quit"
    driver.quit()
    time.sleep(2)
    s.phase = "done"


def shot(args, name):
    if args.shots:
        Path(args.shots).mkdir(parents=True, exist_ok=True)
        subprocess.run(["screencapture", "-x", f"{args.shots}/{args.app}_{name}.png"])


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("mode", choices=["watch", "scenario"])
    ap.add_argument("--app", choices=list(APPS), default="filex")
    ap.add_argument("--out", default="target/profile/run.csv")
    ap.add_argument("--interval", type=float, default=0.25)
    ap.add_argument("--seconds", type=float, help="watch: stop after this long")
    ap.add_argument("--phase-file", help="scenario writes its phase here; watch labels rows from it "
                                         "and stops at 'done' (run watch under sudo for mds disk I/O)")
    ap.add_argument("--cold", action="store_true", help="SIGTERM the daemon first (cold start)")
    ap.add_argument("--settle", type=float, default=6)
    ap.add_argument("--idle", type=float, default=15)
    ap.add_argument("--big-dir", default="/tmp/filex-profile/big")
    ap.add_argument("--big-files", type=int, default=100_000)
    ap.add_argument("--churn-dir", default=str(Path.home() / "filex-profile-churn"))
    ap.add_argument("--churn-files", type=int, default=20_000)
    ap.add_argument("--shots", default="target/profile/shots")
    args = ap.parse_args()
    Path(args.out).parent.mkdir(parents=True, exist_ok=True)
    ROLES.clear()
    ROLES.update(APPS[args.app], **SPOTLIGHT)

    follow = args.mode == "watch" and args.phase_file
    if follow:
        Path(args.phase_file).unlink(missing_ok=True)  # a stale "done" would end the watch at once
    s = Sampler(args.interval, args.phase_file, follow)
    s.start()
    try:
        if args.mode == "scenario":
            scenario(s, args)
        else:
            if not follow:
                s.phase = "watch"
            print("sampling… Ctrl-C to stop" + (f"; following {args.phase_file} until 'done'" if follow else ""))
            end = time.monotonic() + (args.seconds or float("inf"))
            while time.monotonic() < end and s.phase != "done":
                time.sleep(0.5)
    except KeyboardInterrupt:
        pass
    finally:
        s.stop.set()
        s.join()
        summarize(s.rows)
        write_csv(s.rows, args.out)


if __name__ == "__main__":
    main()
