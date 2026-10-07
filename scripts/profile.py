#!/usr/bin/env python3
"""CPU / memory / disk-I/O profiler for filex (UI) and filex-indexd (daemon + workers). macOS only.

Samples libproc's proc_pid_rusage (the counters Activity Monitor shows: "Memory" is
phys_footprint, "Bytes Read/Written" is diskio) for every live process whose binary is
named filex or filex-indexd.

  scripts/profile.py watch [--out run.csv]     sample until Ctrl-C (drive the app yourself)
  scripts/profile.py scenario [--out run.csv]  launch target/release/filex and drive it with
                                               System Events keystrokes through scripted phases

The scenario needs Accessibility permission for the terminal running it.
"""
import argparse, csv, ctypes, os, shutil, signal, subprocess, sys, threading, time
from pathlib import Path

ROLES = {"filex": "ui", "filex-indexd": "daemon"}
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


def filex_pids():
    """{pid: role} for live filex / filex-indexd processes (any build location)."""
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


class Sampler(threading.Thread):
    """Every `interval` s, one row per role: cpu %, footprint, disk bytes/s, wakeups/s.

    ponytail: only live pids are sampled, so a worker that exits loses its last <interval of
    counters. Fine at 250 ms; use the parent's ri_child_* fields if workers get very short-lived.
    """

    def __init__(self, interval=0.25):
        super().__init__(daemon=True)
        self.interval, self.rows, self.phase, self.stop = interval, [], "start", threading.Event()
        self.t0 = time.monotonic()

    def run(self):
        prev, prev_t = {}, time.monotonic()
        while not self.stop.wait(self.interval):
            now = time.monotonic()
            dt = now - prev_t
            cur, agg = {}, {}
            for pid, role in filex_pids().items():
                r = rusage(pid)
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
                self.rows.append(dict(t=round(now - self.t0, 3), phase=self.phase, role=role,
                                      **{k: round(v, 2) for k, v in a.items()}))
            prev, prev_t = cur, now


def summarize(rows):
    phases = list(dict.fromkeys(r["phase"] for r in rows))
    hdr = f"{'phase':<22}{'role':<8}{'secs':>6}{'cpu avg%':>10}{'cpu p95%':>10}{'cpu max%':>10}" \
          f"{'mem avg MB':>12}{'mem max MB':>12}{'read MB':>9}{'write MB':>10}{'wake/s':>8}"
    print(hdr)
    print("-" * len(hdr))
    for ph in phases:
        for role in ("ui", "daemon"):
            rs = [r for r in rows if r["phase"] == ph and r["role"] == role]
            if not rs:
                continue
            ts = [r["t"] for r in rs]
            secs = max(ts) - min(ts) + 0.25
            cpu = sorted(r["cpu_pct"] for r in rs)
            mem = [r["footprint_mb"] for r in rs]
            dt = secs / len(rs)
            print(f"{ph:<22}{role:<8}{secs:>6.1f}{sum(cpu)/len(cpu):>10.1f}{cpu[int(0.95*(len(cpu)-1))]:>10.1f}"
                  f"{cpu[-1]:>10.1f}{sum(mem)/len(mem):>12.1f}{max(mem):>12.1f}"
                  f"{sum(r['read_kbs'] for r in rs)*dt/1024:>9.1f}{sum(r['write_kbs'] for r in rs)*dt/1024:>10.1f}"
                  f"{sum(r['wakeups_s'] for r in rs)/len(rs):>8.0f}")


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


def go(path):
    chord("l", "command")
    time.sleep(0.2)
    chord("a", "command")
    keys(path)
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

    if args.cold:
        s.phase = "daemon_restart"
        daemons = [pid for pid, role in filex_pids().items() if role == "daemon"]
        for pid in daemons:
            os.kill(pid, signal.SIGTERM)
        for _ in range(20):
            if not set(daemons) & set(filex_pids()):
                break
            time.sleep(0.25)
        else:  # builds predating the SIGTERM handler ignore it
            for pid in set(daemons) & set(filex_pids()):
                os.kill(pid, signal.SIGKILL)
        time.sleep(1)

    s.phase = "launch"
    env = dict(os.environ, RUST_LOG=os.environ.get("RUST_LOG", "filex=debug"))
    app = subprocess.Popen([str(ROOT / "target/release/filex")], env=env,
                           stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    time.sleep(args.settle)
    focus(app.pid)
    time.sleep(1)

    s.phase = "idle_after_launch"
    time.sleep(args.idle)

    s.phase = "navigate"
    for p in [str(Path.home()), "/Applications", "/usr/lib", "/System/Library/Frameworks",
              str(Path.home() / "Library"), "/usr/bin", str(Path.home())]:
        go(p)
        time.sleep(1.5)
    for _ in range(5):  # history
        chord("[", "command"); time.sleep(0.6)
    for _ in range(5):
        chord("]", "command"); time.sleep(0.6)

    s.phase = "huge_folder_open"
    go(str(big))
    time.sleep(4)

    s.phase = "huge_folder_scroll"
    for _ in range(60):
        chord(None, code=PGDN); time.sleep(0.1)
    for _ in range(200):
        chord(None, code=DOWN); time.sleep(0.02)
    shot(app.pid, args, "huge_folder")

    s.phase = "search_typing"
    for q in ["cargo", "readme.md", "png", "main.rs", "file_04", "zzqxj"]:
        chord("k", "command"); time.sleep(0.2)
        chord("a", "command")
        keys(q, delay=0.12)  # ~ fast typist
        time.sleep(1.5)
        if q == "png":
            shot(app.pid, args, "search_png")
        chord(None, code=ESC); time.sleep(0.4)

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
    shot(app.pid, args, "end")

    s.phase = "quit"
    chord("q", "command")
    try:
        app.wait(timeout=10)
    except subprocess.TimeoutExpired:
        app.terminate()
    time.sleep(2)


def shot(pid, args, name):
    if args.shots:
        Path(args.shots).mkdir(parents=True, exist_ok=True)
        subprocess.run(["screencapture", "-x", f"{args.shots}/{name}.png"])


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("mode", choices=["watch", "scenario"])
    ap.add_argument("--out", default="target/profile/run.csv")
    ap.add_argument("--interval", type=float, default=0.25)
    ap.add_argument("--seconds", type=float, help="watch: stop after this long")
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

    s = Sampler(args.interval)
    s.start()
    try:
        if args.mode == "scenario":
            scenario(s, args)
        else:
            s.phase = "watch"
            print("sampling… Ctrl-C to stop")
            time.sleep(args.seconds or float("inf"))
    except KeyboardInterrupt:
        pass
    finally:
        s.stop.set()
        s.join()
        summarize(s.rows)
        write_csv(s.rows, args.out)


if __name__ == "__main__":
    main()
