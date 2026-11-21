#!/usr/bin/env python3
"""Benchmark and evidence driver for CI (Linux + Docker). Methodology: bench/README.md.

    bench.py parity                      scorer-flag parity with scripts/docker_run.sh
    bench.py dataset DIR                 bytes, file counts and digest of a dataset (any OS)
    bench.py correctness ...             36 fixtures, extra fixtures, dev answer
    bench.py bench20m ...                diagnostics + timed pipeline runs at practice scale
    bench.py bench60m ...                one run at final scale with memory and disk use

Every command writes one JSON document (--out-json); bench/report.py renders it.
"""

from __future__ import annotations

import argparse
import calendar
import concurrent.futures
import ctypes
import hashlib
import json
import os
import platform
import re
import shlex
import shutil
import statistics
import subprocess
import sys
import threading
import time
import traceback
import uuid
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
PLATFORM = "linux/amd64"
SCORER_CPUS = 8
SCORER_MEM_G = 24
HARD_CAP_S = 180.0
READTHROUGH_MODES = ["mmap-scan", "mmap-populate-scan", "mmap-touch", "read-scan", "read"]


# --------------------------------------------------------------------------
# small helpers
# --------------------------------------------------------------------------

def log(message=""):
    print(message, flush=True)


class group:
    """A collapsible section in the Actions log."""

    def __init__(self, title):
        self.title = title

    def __enter__(self):
        log(f"::group::{self.title}")

    def __exit__(self, *_):
        log("::endgroup::")


def run_cmd(cmd, *, timeout=None, env=None, cwd=None):
    return subprocess.run([str(part) for part in cmd], stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                          text=True, errors="replace", timeout=timeout, env=env, cwd=cwd)


def output_of(cmd, default=""):
    try:
        done = run_cmd(cmd, timeout=60)
    except (OSError, subprocess.SubprocessError):
        return default
    return done.stdout.strip() if done.returncode == 0 else default


def stream(cmd, *, env=None, cwd=None, timeout=None):
    """Runs a command, echoing its combined output to the log. Returns (exit code, text, seconds)."""
    log("$ " + " ".join(shlex.quote(str(part)) for part in cmd))
    started = time.perf_counter()
    process = subprocess.Popen([str(part) for part in cmd], stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                               text=True, errors="replace", env=env, cwd=cwd)
    lines = []
    timer = None
    if timeout:
        timer = threading.Timer(timeout, process.kill)
        timer.start()
    try:
        for line in process.stdout:
            lines.append(line)
            sys.stdout.write(line)
            sys.stdout.flush()
        code = process.wait()
    finally:
        if timer:
            timer.cancel()
    return code, "".join(lines), time.perf_counter() - started


def read_text(path, default=""):
    try:
        return Path(path).read_text(errors="replace")
    except OSError:
        return default


def tail(text, limit=3000):
    return text if len(text) <= limit else "..." + text[-limit:]


def median(values):
    values = [value for value in values if value is not None]
    return statistics.median(values) if values else None


def remove_tree(path):
    """Deletes a directory even when a container wrote into it as uid 1000."""
    path = Path(path)
    if not path.exists():
        return
    shutil.rmtree(path, ignore_errors=True)
    if path.exists():
        subprocess.run(["sudo", "-n", "rm", "-rf", str(path)], check=False)


def fresh_dir(path):
    """A new, empty, world-writable directory (the container writes as uid 1000)."""
    path = Path(path)
    remove_tree(path)
    path.mkdir(parents=True)
    os.chmod(path, 0o777)
    return path


def tree_usage(path):
    """(bytes, files) under a directory; unreadable entries are skipped."""
    total = files = 0
    stack = [str(path)]
    while stack:
        try:
            with os.scandir(stack.pop()) as entries:
                for entry in entries:
                    try:
                        if entry.is_dir(follow_symlinks=False):
                            stack.append(entry.path)
                        elif entry.is_file(follow_symlinks=False):
                            total += entry.stat(follow_symlinks=False).st_size
                            files += 1
                    except OSError:
                        pass
        except OSError:
            pass
    return total, files


def parse_cpu_list(text):
    cpus = []
    for part in text.strip().split(","):
        if not part:
            continue
        low, _, high = part.partition("-")
        cpus.extend(range(int(low), int(high or low) + 1))
    return cpus


def format_cpu_list(cpus):
    cpus = sorted(set(cpus))
    parts, at = [], 0
    while at < len(cpus):
        end = at
        while end + 1 < len(cpus) and cpus[end + 1] == cpus[end] + 1:
            end += 1
        parts.append(str(cpus[at]) if end == at else f"{cpus[at]}-{cpus[end]}")
        at = end + 1
    return ",".join(parts)


def pick_cpuset(count):
    """`count` logical CPUs as whole cores (every hyperthread of each core), lowest IDs first:
    the shape of the scorer's eight pinned hyperthreads."""
    allowed = sorted(os.sched_getaffinity(0))
    cores = {}
    for cpu in allowed:
        siblings = read_text(f"/sys/devices/system/cpu/cpu{cpu}/topology/thread_siblings_list")
        members = tuple(c for c in parse_cpu_list(siblings) if c in allowed) or (cpu,)
        cores[members] = True
    chosen = []
    for members in sorted(cores):
        for cpu in members:
            if len(chosen) < count and cpu not in chosen:
                chosen.append(cpu)
    return sorted(chosen)


# --------------------------------------------------------------------------
# hardware
# --------------------------------------------------------------------------

def meminfo():
    values = {}
    for line in read_text("/proc/meminfo").splitlines():
        key, _, rest = line.partition(":")
        fields = rest.split()
        if fields and fields[0].isdigit():
            values[key] = int(fields[0])  # kB
    return values


def hardware(data_dir):
    cpuinfo = read_text("/proc/cpuinfo")
    model = next((line.split(":", 1)[1].strip() for line in cpuinfo.splitlines() if line.startswith("model name")),
                 platform.processor() or "unknown")
    flags = next((line.split(":", 1)[1].split() for line in cpuinfo.splitlines() if line.startswith("flags")), [])
    wanted = ["sse4_2", "avx", "avx2", "bmi2", "pclmulqdq", "avx512f", "avx512bw", "avx512vl", "avx512_vbmi",
              "avx512_vbmi2", "gfni", "vpclmulqdq"]
    lscpu = {}
    for line in output_of(["lscpu"]).splitlines():
        key, _, value = line.partition(":")
        lscpu[key.strip()] = value.strip()
    memory = meminfo()
    os_release = dict(line.split("=", 1) for line in read_text("/etc/os-release").splitlines() if "=" in line)
    info = {
        "cpu_model": model,
        "nproc": len(os.sched_getaffinity(0)) if hasattr(os, "sched_getaffinity") else os.cpu_count(),
        "sockets": lscpu.get("Socket(s)"),
        "cores_per_socket": lscpu.get("Core(s) per socket"),
        "threads_per_core": lscpu.get("Thread(s) per core"),
        "cache_l1d": lscpu.get("L1d cache"),
        "cache_l2": lscpu.get("L2 cache"),
        "cache_l3": lscpu.get("L3 cache"),
        "hypervisor": lscpu.get("Hypervisor vendor"),
        "cpu_flags_present": [flag for flag in wanted if flag in flags],
        "cpu_flags_absent": [flag for flag in wanted if flag not in flags],
        "mem_total_kb": memory.get("MemTotal"),
        "mem_available_kb": memory.get("MemAvailable"),
        "swap_total_kb": memory.get("SwapTotal"),
        "kernel": platform.release(),
        "os": os_release.get("PRETTY_NAME", "").strip('"'),
        "runner_image": " ".join(filter(None, [os.environ.get("ImageOS"), os.environ.get("ImageVersion")])),
        "runner_name": os.environ.get("RUNNER_NAME", ""),
        "thp_enabled": read_text("/sys/kernel/mm/transparent_hugepage/enabled").strip(),
        "thp_defrag": read_text("/sys/kernel/mm/transparent_hugepage/defrag").strip(),
        "python": platform.python_version(),
    }
    try:
        docker = json.loads(output_of(["docker", "info", "--format", "{{json .}}"], "{}") or "{}")
    except ValueError:
        docker = {}
    info["docker"] = {
        "server_version": docker.get("ServerVersion") or output_of(["docker", "version", "--format", "{{.Server.Version}}"]),
        "client_version": output_of(["docker", "version", "--format", "{{.Client.Version}}"]),
        "storage_driver": docker.get("Driver"),
        "cgroup_driver": docker.get("CgroupDriver"),
        "cgroup_version": docker.get("CgroupVersion"),
        "default_runtime": docker.get("DefaultRuntime"),
        "runc": output_of(["runc", "--version"]).splitlines()[0] if output_of(["runc", "--version"]) else "",
        "containerd": output_of(["containerd", "--version"]),
        "security_options": docker.get("SecurityOptions"),
        "docker_root_dir": docker.get("DockerRootDir"),
    }
    data_dir = Path(data_dir)
    data_dir.mkdir(parents=True, exist_ok=True)
    findmnt = output_of(["findmnt", "-n", "-o", "FSTYPE,SOURCE,TARGET,OPTIONS", "-T", str(data_dir)]).split(None, 3)
    usage = shutil.disk_usage(data_dir)
    info["data_dir"] = {
        "path": str(data_dir),
        "fstype": findmnt[0] if findmnt else "",
        "source": findmnt[1] if len(findmnt) > 1 else "",
        "mount": findmnt[2] if len(findmnt) > 2 else "",
        "options": findmnt[3] if len(findmnt) > 3 else "",
        "size_bytes": usage.total,
        "free_bytes": usage.free,
    }
    return info


def resource_limits(hw):
    """--cpus = min(8, nproc); --memory = min(24 GB, what the runner can give: RAM minus 2 GB, whole GB)."""
    cpus = min(SCORER_CPUS, hw["nproc"])
    total_gib = (hw.get("mem_total_kb") or 0) // (1024 * 1024)
    mem_g = min(SCORER_MEM_G, max(1, total_gib - 2))
    return cpus, mem_g


# --------------------------------------------------------------------------
# datasets
# --------------------------------------------------------------------------

def file_sha256(path):
    digest = hashlib.sha256()
    with open(path, "rb") as handle:
        while True:
            block = handle.read(8 << 20)
            if not block:
                break
            digest.update(block)
    return digest.hexdigest()


def dataset_stats(root, with_digest=True):
    """Actual bytes and file counts per folder, plus one digest over every file's path, size and SHA-256."""
    root = Path(root)
    folders = {}
    for name in ("events", "clicks", "links"):
        files = sorted(path for path in (root / name).glob("*.csv") if path.is_file())
        sizes = [path.stat().st_size for path in files]
        header_only = 0
        for path, size in zip(files, sizes):
            if size < 256 and path.read_bytes().count(b"\n") <= 1:
                header_only += 1
        folders[name] = {
            "files": len(files),
            "bytes": sum(sizes),
            "header_only_files": header_only,
            "largest_file_bytes": max(sizes, default=0),
        }
    every = sorted(path for path in root.rglob("*") if path.is_file())
    stats = {
        "path": str(root),
        "folders": folders,
        "lookup_bytes": {name: (root / name).stat().st_size for name in ("products.csv", "campaigns.csv", "manifest.json")
                         if (root / name).is_file()},
        "total_files": len(every),
        "total_bytes": sum(path.stat().st_size for path in every),
    }
    try:
        stats["manifest"] = json.loads((root / "manifest.json").read_text())
    except (OSError, ValueError):
        stats["manifest"] = None
    if with_digest:
        started = time.perf_counter()
        with concurrent.futures.ThreadPoolExecutor(max_workers=min(8, os.cpu_count() or 1)) as pool:
            hashes = list(pool.map(file_sha256, every))
        combined = hashlib.sha256()
        for path, file_hash in zip(every, hashes):
            combined.update(f"{path.relative_to(root).as_posix()}\t{path.stat().st_size}\t{file_hash}\n".encode())
        stats["digest"] = "sha256:" + combined.hexdigest()
        stats["digest_seconds"] = round(time.perf_counter() - started, 3)
    return stats


def generate(generator, seed, events, clicks, out):
    remove_tree(out)
    cmd = [generator, "--seed", seed, "--events", events, "--clicks", clicks, "--output", out]
    code, _, seconds = stream(cmd)
    if code != 0:
        raise RuntimeError(f"generator exited {code}")
    return {"command": "generate " + " ".join(str(part) for part in cmd[1:-1]) + " DIR", "seconds": round(seconds, 3)}


def data_files(root):
    return sorted(str(path) for path in Path(root).rglob("*") if path.is_file())


def warm_page_cache(root):
    """Reads every input file once so the timed runs start from a warm page cache."""
    def read(path):
        with open(path, "rb") as handle:
            while handle.read(8 << 20):
                pass
    started = time.perf_counter()
    with concurrent.futures.ThreadPoolExecutor(max_workers=4) as pool:
        list(pool.map(read, data_files(root)))
    return round(time.perf_counter() - started, 3)


def page_cache_state(root):
    """How much of the input is resident in the host page cache right now (mincore over every file)."""
    try:
        libc = ctypes.CDLL(None, use_errno=True)
        libc.mmap.restype = ctypes.c_void_p
        libc.mmap.argtypes = [ctypes.c_void_p, ctypes.c_size_t, ctypes.c_int, ctypes.c_int, ctypes.c_int, ctypes.c_long]
        libc.munmap.argtypes = [ctypes.c_void_p, ctypes.c_size_t]
        libc.mincore.argtypes = [ctypes.c_void_p, ctypes.c_size_t, ctypes.c_void_p]
        page = os.sysconf("SC_PAGESIZE")
        total = resident = 0
        for path in data_files(root):
            size = os.path.getsize(path)
            if size == 0:
                continue
            fd = os.open(path, os.O_RDONLY)
            try:
                address = libc.mmap(None, size, 1, 1, fd, 0)  # PROT_READ, MAP_SHARED
                if address in (None, ctypes.c_void_p(-1).value):
                    return None
                pages = (size + page - 1) // page
                vector = (ctypes.c_ubyte * pages)()
                if libc.mincore(address, size, vector) != 0:
                    libc.munmap(address, size)
                    return None
                in_core = sum(value & 1 for value in bytes(vector))
                libc.munmap(address, size)
            finally:
                os.close(fd)
            total += size
            resident += min(size, in_core * page)
        return {"resident_bytes": resident, "total_bytes": total,
                "resident_pct": round(100.0 * resident / total, 2) if total else 100.0}
    except (OSError, AttributeError, ValueError):
        return None


def drop_page_cache():
    done = run_cmd(["sudo", "-n", "sh", "-c", "sync; echo 3 > /proc/sys/vm/drop_caches"], timeout=300)
    return done.returncode == 0


# --------------------------------------------------------------------------
# the scorer's docker flags
# --------------------------------------------------------------------------

def scorer_args(*, cpus, mem, data, out, scratch, platform_=PLATFORM, ncpu=None, mem_gb=None):
    """The flags of scripts/docker_run.sh, in its order. `cpus` like "8", `mem` like "24g"."""
    ncpu = cpus.rsplit(".", 1)[0] if ncpu is None else ncpu          # ${CPUS%.*}
    mem_gb = mem.removesuffix("g") if mem_gb is None else mem_gb     # ${MEM%g}
    return [
        "--platform", platform_,
        "--network", "none", "--read-only", "--tmpfs", "/tmp:rw,size=64m,mode=1777",
        "--cap-drop", "ALL", "--security-opt", "no-new-privileges",
        "--pids-limit", "4096", "--cpus", cpus, "--memory", mem, "--memory-swap", mem,
        "--user", "1000:1000",
        "-v", f"{data}:/data:ro,z", "-v", f"{out}:/output:z", "-v", f"{scratch}:/scratch:z",
        "-e", "DATA_DIR=/data", "-e", "OUTPUT_DIR=/output", "-e", "SCRATCH_DIR=/scratch", "-e", "HOME=/scratch",
        "-e", f"NCPU={ncpu}", "-e", f"MEM_GB={mem_gb}",
    ]


def docker_run_sh_args(script):
    """The flags the organizers' script passes to its scored `docker run`, with shell variables as @NAME@."""
    text = Path(script).read_text()
    command = text[text.rindex("docker run --rm"):].replace("\\\n", " ").strip().splitlines()[0]
    for shell, placeholder in {
        '"$PLATFORM"': "@PLATFORM@", '"$CPUS"': "@CPUS@", '"$MEM"': "@MEM@", '"$DATA"': "@DATA@", '"$OUT"': "@OUT@",
        '"$SCRATCH"': "@SCRATCH@", '"${CPUS%.*}"': "@NCPU@", '"${MEM%g}"': "@MEM_GB@", '"$IMAGE"': "@IMAGE@",
    }.items():
        command = command.replace(shell, placeholder)
    if "$" in command:
        raise ValueError(f"unexpected shell variable left in: {command}")
    tokens = shlex.split(command)
    if tokens[:3] != ["docker", "run", "--rm"] or tokens[-1] != "@IMAGE@":
        raise ValueError(f"unexpected command shape: {command}")
    return tokens[3:-1]


def flag_parity():
    """Checks that the harness passes exactly the flags of scripts/docker_run.sh (it only swaps --rm
    for --name, so the container can be inspected, and adds --cpuset-cpus when pinning)."""
    script = REPO / "scripts" / "docker_run.sh"
    mine = scorer_args(cpus="@CPUS@", mem="@MEM@", data="@DATA@", out="@OUT@", scratch="@SCRATCH@",
                       platform_="@PLATFORM@", ncpu="@NCPU@", mem_gb="@MEM_GB@")
    try:
        theirs = docker_run_sh_args(script)
    except (OSError, ValueError) as error:
        return {"ok": False, "detail": f"could not read {script.name}: {error}"}
    result = {"ok": mine == theirs, "script_sha256": file_sha256(script), "flags": " ".join(theirs)}
    if not result["ok"]:
        result["detail"] = f"harness: {' '.join(mine)} | script: {' '.join(theirs)}"
    return result


# --------------------------------------------------------------------------
# running containers
# --------------------------------------------------------------------------

def docker_time_ns(text):
    """Docker's RFC 3339 timestamp with nanoseconds -> integer nanoseconds since the epoch."""
    match = re.fullmatch(r"(\d{4})-(\d\d)-(\d\d)T(\d\d):(\d\d):(\d\d)(?:\.(\d+))?(Z|[+-]\d\d:\d\d)", text or "")
    if not match or match.group(1) == "0001":
        return None
    year, month, day, hour, minute, second = (int(match.group(index)) for index in range(1, 7))
    seconds = calendar.timegm((year, month, day, hour, minute, second))
    zone = match.group(8)
    if zone != "Z":
        sign = 1 if zone[0] == "+" else -1
        seconds -= sign * (int(zone[1:3]) * 3600 + int(zone[4:6]) * 60)
    fraction = (match.group(7) or "0")[:9].ljust(9, "0")
    return seconds * 1_000_000_000 + int(fraction)


def parse_ss_timing(text):
    """`SS_TIMING stage=<name> ms=<float>` lines, in the order printed. Stage names are not interpreted."""
    stages = []
    for line in text.splitlines():
        match = re.match(r"^\s*SS_TIMING\s+stage=(\S+)\s+ms=([-+0-9.eE]+)\s*$", line)
        if match:
            try:
                stages.append([match.group(1), float(match.group(2))])
            except ValueError:
                pass
    return stages


def parse_diag(text):
    """The `DIAG readthrough key=value ...` line of bench/diag."""
    for line in text.splitlines():
        if line.startswith("DIAG readthrough "):
            fields = dict(part.split("=", 1) for part in line.split()[2:] if "=" in part)
            for key in ("threads", "chunk_kb", "files", "bytes", "lines", "populate_failures"):
                if key in fields:
                    fields[key] = int(fields[key])
            if "ms" in fields:
                fields["ms"] = float(fields["ms"])
            return fields
    return None


class Monitor(threading.Thread):
    """Samples a running container's cgroup memory and the size of its scratch and output folders."""

    def __init__(self, container_id, scratch, out, interval=0.005):
        super().__init__(daemon=True)
        self.candidates = [f"/sys/fs/cgroup/system.slice/docker-{container_id}.scope",
                           f"/sys/fs/cgroup/docker/{container_id}"]
        self.scratch, self.out, self.interval = scratch, out, interval
        self.halt = threading.Event()
        self.memory_peak = self.max_current = self.max_anon = self.max_file = None
        self.max_scratch = self.max_out = 0
        self.samples = 0
        self.cgroup = None

    @staticmethod
    def _bigger(old, new):
        return new if old is None or new > old else old

    def run(self):
        last_disk = 0.0
        while not self.halt.is_set():
            if self.cgroup is None:
                self.cgroup = next((path for path in self.candidates if os.path.exists(path + "/memory.current")), None)
            if self.cgroup is not None:
                try:
                    self.max_current = self._bigger(self.max_current, int(read_file(self.cgroup + "/memory.current")))
                    self.samples += 1
                    for line in read_file(self.cgroup + "/memory.stat").splitlines()[:2]:
                        key, _, value = line.partition(" ")
                        if key == "anon":
                            self.max_anon = self._bigger(self.max_anon, int(value))
                        elif key == "file":
                            self.max_file = self._bigger(self.max_file, int(value))
                    self.memory_peak = self._bigger(self.memory_peak, int(read_file(self.cgroup + "/memory.peak")))
                except (OSError, ValueError):
                    pass
            now = time.monotonic()
            if now - last_disk >= 0.1:
                last_disk = now
                self.max_scratch = max(self.max_scratch, tree_usage(self.scratch)[0])
                self.max_out = max(self.max_out, tree_usage(self.out)[0])
            self.halt.wait(self.interval)

    def result(self):
        return {
            "memory_peak_bytes": self.memory_peak,
            "max_memory_current_bytes": self.max_current,
            "max_anon_bytes": self.max_anon,
            "max_file_bytes": self.max_file,
            "peak_scratch_bytes": self.max_scratch,
            "peak_output_bytes": self.max_out,
            "samples": self.samples,
            "cgroup": self.cgroup,
        }


def read_file(path):
    with open(path) as handle:
        return handle.read()


class Harness:
    """Runs fresh containers with the scorer's flags and measures them."""

    def __init__(self, work, cpus, mem_g, cpuset=None):
        self.work = Path(work)
        self.cpus, self.mem_g, self.cpuset = cpus, mem_g, cpuset
        self.work.mkdir(parents=True, exist_ok=True)

    def describe(self):
        text = f"--cpus {self.cpus} --memory {self.mem_g}g --memory-swap {self.mem_g}g"
        return text + (f" --cpuset-cpus {self.cpuset}" if self.cpuset else "")

    def probe_cpuset(self, image, command):
        """Keeps --cpuset-cpus only if Docker accepts it here."""
        if not self.cpuset:
            return
        done = run_cmd(["docker", "run", "--rm", "--network", "none", "--cpuset-cpus", self.cpuset, image, *command])
        if done.returncode != 0:
            log(f"::warning::--cpuset-cpus {self.cpuset} is not usable here ({tail(done.stderr, 300).strip()}); "
                "running with the --cpus quota only")
            self.cpuset = None

    def run(self, image, label, data, *, command=(), entrypoint=None, env=None, out_dir=None, split=False,
            monitor=False, timeout=900):
        """One fresh container. Timed two ways: the Docker client's wall clock around `docker run`, and the
        container's own State.StartedAt -> State.FinishedAt. Everything else happens outside both."""
        out = fresh_dir(out_dir or self.work / "out-discard")
        scratch = fresh_dir(self.work / "scratch")
        name = "ss-" + re.sub(r"[^A-Za-z0-9_.-]+", "-", label) + "-" + uuid.uuid4().hex[:8]
        args = ["--name", name] + scorer_args(cpus=str(self.cpus), mem=f"{self.mem_g}g", data=data, out=out,
                                               scratch=scratch)
        if self.cpuset:
            args += ["--cpuset-cpus", self.cpuset]
        for key, value in (env or {}).items():
            args += ["-e", f"{key}={value}"]
        if entrypoint:
            args += ["--entrypoint", entrypoint]
        args += [image, *command]
        result = {"label": label, "image": image, "command": " ".join([entrypoint or "", *command]).strip() or "(image default)",
                  "env": dict(env or {})}
        subprocess.run(["sync"], check=False)  # nothing left to write back from earlier steps
        sampler = None
        try:
            if split or monitor:
                started = time.perf_counter_ns()
                created = run_cmd(["docker", "create", *args], timeout=120)
                result["create_s"] = (time.perf_counter_ns() - started) / 1e9
                if created.returncode != 0:
                    raise RuntimeError(f"docker create failed: {tail(created.stderr, 500)}")
                if monitor:
                    sampler = Monitor(created.stdout.strip(), scratch, out)
                    sampler.start()
                started = time.perf_counter_ns()
                done = run_cmd(["docker", "start", "-a", name], timeout=timeout)
                result["start_s"] = (time.perf_counter_ns() - started) / 1e9
                result["wall_s"] = result["create_s"] + result["start_s"]
            else:
                started = time.perf_counter_ns()
                done = run_cmd(["docker", "run", *args], timeout=timeout)
                result["wall_s"] = (time.perf_counter_ns() - started) / 1e9
            stdout, stderr = done.stdout, done.stderr
        except subprocess.TimeoutExpired as expired:
            subprocess.run(["docker", "kill", name], check=False, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            result["timed_out_after_s"] = timeout
            stdout = expired.stdout if isinstance(expired.stdout, str) else ""
            stderr = expired.stderr if isinstance(expired.stderr, str) else ""
        except RuntimeError as error:
            result["error"] = str(error)
            stdout = stderr = ""
        finally:
            if sampler:
                sampler.halt.set()
                sampler.join()
        # Outside the timed interval: the container's own timestamps, exit status and what it wrote.
        inspected = run_cmd(["docker", "inspect", "--format", "{{json .State}}", name])
        state = {}
        if inspected.returncode == 0:
            try:
                state = json.loads(inspected.stdout)
            except ValueError:
                pass
        subprocess.run(["docker", "rm", "-f", name], check=False, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        begin, end = docker_time_ns(state.get("StartedAt")), docker_time_ns(state.get("FinishedAt"))
        result.update({
            "started_at": state.get("StartedAt"),
            "finished_at": state.get("FinishedAt"),
            "docker_s": (end - begin) / 1e9 if begin is not None and end is not None else None,
            "exit_code": state.get("ExitCode") if state else None,
            "oom_killed": state.get("OOMKilled"),
            "docker_error": state.get("Error") or None,
            "stdout_tail": tail(stdout, 20000),
            "stderr_tail": tail(stderr),
        })
        timing = parse_ss_timing(stderr)
        if timing:
            result["ss_timing"] = timing
        diag = parse_diag(stdout)
        if diag:
            result["diag"] = diag
        if sampler:
            result["monitor"] = sampler.result()
        out_bytes, out_files = tree_usage(out)
        scratch_bytes, scratch_files = tree_usage(scratch)
        result["output_bytes"], result["output_files"] = out_bytes, out_files
        result["scratch_bytes_at_exit"], result["scratch_files_at_exit"] = scratch_bytes, scratch_files
        result["ok"] = result.get("exit_code") == 0 and "error" not in result and "timed_out_after_s" not in result
        log(f"  {label}: container {fmt_s(result.get('docker_s'))}  client wall {fmt_s(result.get('wall_s'))}  "
            f"exit {result.get('exit_code')}" + ("  OOM-KILLED" if result.get("oom_killed") else ""))
        if not result["ok"]:
            log("    stderr: " + tail(stderr, 800).strip().replace("\n", "\n    "))
        remove_tree(scratch)
        if out_dir is None:
            remove_tree(out)
        return result

    def series(self, image, label, data, count, **options):
        runs = [self.run(image, f"{label}-{index + 1}", data, **options) for index in range(count)]
        return summarize(runs)


def fmt_s(value):
    return "n/a" if value is None else f"{value:.3f} s"


def summarize(runs):
    summary = {
        "runs": runs,
        "median_docker_s": median([run.get("docker_s") for run in runs]),
        "median_wall_s": median([run.get("wall_s") for run in runs]),
        "ok": all(run.get("ok") for run in runs),
    }
    inner = [run["diag"]["ms"] for run in runs if run.get("diag") and "ms" in run["diag"]]
    if inner:
        summary["median_inprocess_ms"] = median(inner)
    return summary


def image_info(tag):
    done = run_cmd(["docker", "image", "inspect", "--format", "{{json .}}", tag])
    if done.returncode != 0:
        return None
    try:
        data = json.loads(done.stdout)
    except ValueError:
        return None
    return {"tag": tag, "id": data.get("Id"), "size_bytes": data.get("Size"), "created": data.get("Created")}


# --------------------------------------------------------------------------
# checking outputs
# --------------------------------------------------------------------------

def compare_outputs(left, right):
    """scripts/compare.py: exact, order-insensitive diff of both tables. MATCH or DIFFER."""
    done = run_cmd([sys.executable, REPO / "scripts" / "compare.py", left, right], timeout=3600)
    lines = [line for line in done.stdout.strip().splitlines() if line.strip()]
    verdict = lines[-1].strip() if lines else ""
    return {
        "match": done.returncode == 0 and verdict == "MATCH",
        "exit_code": done.returncode,
        "output": tail(done.stdout.strip(), 2500),
        "stderr_tail": tail(done.stderr.strip(), 1500) if done.returncode not in (0, 1) else "",
    }


def parquet_files(folder):
    folder = Path(folder)
    return sorted(str(path) for path in folder.glob("*.parquet")
                  if path.is_file() and not path.is_symlink() and not path.name.startswith((".", "_")))


def self_check(out):
    """The rules' quick self-check on one output folder (not a substitute for the full diff)."""
    import polars as pl
    try:
        users = pl.concat([pl.read_parquet(path) for path in parquet_files(Path(out) / "user_daily")])
        campaigns = pl.concat([pl.read_parquet(path) for path in parquet_files(Path(out) / "campaign_daily")])
    except Exception as error:  # noqa: BLE001 - any unreadable output fails the check
        return {"ok": False, "error": f"{type(error).__name__}: {error}"}
    credited = campaigns.filter(pl.col("campaign_id") != "organic")
    values = {
        "user_daily_rows": users.height,
        "campaign_daily_rows": campaigns.height,
        "user_sessions": int(users["sessions"].sum()),
        "campaign_sessions": int(campaigns["sessions"].sum()),
        "user_revenue_cents": int(users["revenue_cents"].sum()),
        "campaign_revenue_cents": int(campaigns["revenue_cents"].sum()),
        "assisted_sessions": int(campaigns["assisted_sessions"].sum()),
        "credited_non_organic_sessions": int(credited["sessions"].sum()),
    }
    values["ok"] = (values["user_sessions"] == values["campaign_sessions"]
                    and values["user_revenue_cents"] == values["campaign_revenue_cents"]
                    and values["assisted_sessions"] >= values["credited_non_organic_sessions"])
    return values


def person_skew(out):
    """How uneven the people are, read from a trusted user_daily: surviving events per person."""
    import polars as pl
    try:
        users = pl.concat([pl.read_parquet(path, columns=["user_id", "events", "sessions"])
                           for path in parquet_files(Path(out) / "user_daily")])
    except Exception as error:  # noqa: BLE001
        return {"error": f"{type(error).__name__}: {error}"}
    people = users.group_by("user_id").agg(pl.col("events").sum(), pl.col("sessions").sum(), pl.len().alias("days"))
    if people.height == 0:
        return {"people": 0}
    total = int(people["events"].sum())
    top = people.sort(["events", "user_id"], descending=[True, False]).head(5)
    return {
        "people": people.height,
        "surviving_events": total,
        "sessions": int(people["sessions"].sum()),
        "events_per_person_median": float(people["events"].median()),
        "events_per_person_p99": float(people["events"].quantile(0.99)),
        "events_per_person_max": int(people["events"].max()),
        "top5_events": [int(value) for value in top["events"]],
        "top5_share_pct": [round(100.0 * int(value) / total, 3) for value in top["events"]],
        "top5_sessions": [int(value) for value in top["sessions"]],
    }


def output_summary(out):
    return {table: {"files": len(parquet_files(Path(out) / table)),
                    "bytes": sum(os.path.getsize(path) for path in parquet_files(Path(out) / table))}
            for table in ("user_daily", "campaign_daily")}


# --------------------------------------------------------------------------
# commands
# --------------------------------------------------------------------------

def base_result(job):
    server = os.environ.get("GITHUB_SERVER_URL", "https://github.com")
    repository = os.environ.get("GITHUB_REPOSITORY", "")
    run_id = os.environ.get("GITHUB_RUN_ID", "")
    return {
        "schema": 1,
        "job": job,
        "sha": os.environ.get("BENCH_SHA") or os.environ.get("GITHUB_SHA") or output_of(["git", "-C", REPO, "rev-parse", "HEAD"]),
        "ref": os.environ.get("GITHUB_REF_NAME", ""),
        "event": os.environ.get("GITHUB_EVENT_NAME", ""),
        "run_url": f"{server}/{repository}/actions/runs/{run_id}" if repository and run_id else "",
        "run_attempt": os.environ.get("GITHUB_RUN_ATTEMPT", ""),
        "utc": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        "errors": [],
        "warnings": [],
    }


def write_json(path, result):
    Path(path).parent.mkdir(parents=True, exist_ok=True)
    Path(path).write_text(json.dumps(result, indent=1) + "\n")
    log(f"wrote {path}")


def finish(args, result):
    result["ok"] = not result["errors"]
    write_json(args.out_json, result)
    for error in result["errors"]:
        log(f"::error::{error}")
    for warning in result["warnings"]:
        log(f"::warning::{warning}")
    return 0 if result["ok"] else 1


def cmd_parity(_args):
    parity = flag_parity()
    log(json.dumps(parity, indent=1))
    return 0 if parity["ok"] else 1


def cmd_dataset(args):
    log(json.dumps(dataset_stats(args.dir), indent=1))
    return 0


def cmd_correctness(args):
    result = base_result("correctness")
    work = Path(args.work)
    work.mkdir(parents=True, exist_ok=True)
    try:
        hw = hardware(work)
        cpus, mem_g = resource_limits(hw)
        result.update({"hardware": hw, "limits": {"cpus": cpus, "memory_g": mem_g}, "mode": "pipeline" if args.image else "stand-in",
                       "flag_parity": flag_parity(), "python_polars": output_of([sys.executable, "-c", "import polars; print(polars.__version__)"])})
        if not result["flag_parity"]["ok"]:
            result["errors"].append("the harness's docker flags differ from scripts/docker_run.sh: " + result["flag_parity"].get("detail", ""))
        if not args.image:
            result["skipped"] = "no root Dockerfile at this commit: the fixture and dev checks were skipped"
            return finish(args, result)
        result["image"] = image_info(args.image)
        # The organizers' own commands, run their way; the scripts read these from the environment.
        env = dict(os.environ, IMAGE=args.image, CPUS=str(cpus), MEM=f"{mem_g}g", PLATFORM=PLATFORM)
        result["script_env"] = f"IMAGE={args.image} CPUS={cpus} MEM={mem_g}g"
        result["fixtures"] = []
        for name in ("fixtures", "extra_fixtures"):
            if not (REPO / name).is_dir():
                if name == "fixtures":
                    result["errors"].append("fixtures/ is missing")
                continue
            with group(f"{name}: run_fixtures.py {name} -- scripts/docker_run.sh"):
                command = [sys.executable, "scripts/run_fixtures.py", name, "--", "scripts/docker_run.sh"]
                code, text, seconds = stream(command, env=env, cwd=REPO, timeout=3600)
            counts = re.search(r"(\d+)/(\d+) fixtures passed", text)
            entry = {
                "set": name,
                "command": f"python3 scripts/run_fixtures.py {name} -- scripts/docker_run.sh",
                "exit_code": code,
                "passed": int(counts.group(1)) if counts else None,
                "total": int(counts.group(2)) if counts else None,
                "seconds": round(seconds, 1),
                "failures": [line for line in text.splitlines() if line.startswith("FAIL")][:40],
            }
            entry["ok"] = code == 0 and counts is not None and entry["passed"] == entry["total"]
            result["fixtures"].append(entry)
            if not entry["ok"]:
                result["errors"].append(f"{name}: {entry['passed']}/{entry['total']} passed (exit {code})")
        # The dev check: seed 1, 1M events, 250k clicks against the published answer.
        dev, out, answer = work / "dev", work / "dev-out", Path(args.tools) / "dev-answer"
        with group("dev check: generate seed 1, docker_run.sh, compare.py against the published answer"):
            generation = generate(Path(args.tools) / "generate", 1, 1_000_000, 250_000, dev)
            stats = dataset_stats(dev)
            remove_tree(out)
            code, text, seconds = stream(["scripts/docker_run.sh", dev, out], env=env, cwd=REPO, timeout=1800)
            comparison = compare_outputs(out, answer) if code == 0 else {"match": False, "output": "(run failed)", "exit_code": None}
            log(comparison["output"])
        result["dev"] = {
            "generation": generation,
            "dataset": stats,
            "run_command": "scripts/docker_run.sh dev out",
            "run_exit_code": code,
            "run_script_seconds": round(seconds, 3),
            "run_output_tail": tail(text, 1500),
            "compare_command": "python3 scripts/compare.py out dev-answer",
            "compare": comparison,
            "ok": code == 0 and comparison["match"],
        }
        if code != 0:
            result["errors"].append(f"dev run: scripts/docker_run.sh exited {code}")
        elif not comparison["match"]:
            result["errors"].append("dev check: compare.py did not print MATCH against the published dev answer")
        remove_tree(out)
        remove_tree(dev)
    except Exception as error:  # noqa: BLE001 - always leave a report behind
        traceback.print_exc()
        result["errors"].append(f"harness error: {type(error).__name__}: {error}")
    return finish(args, result)


def prepare_dataset(args, seed, events, clicks, data):
    with group(f"seed {seed}: generate {events:,} events / {clicks:,} clicks"):
        generation = generate(args.generator, seed, events, clicks, data)
        stats = dataset_stats(data)
        warm_seconds = warm_page_cache(data)
        cache = page_cache_state(data)
        log(json.dumps({"generation": generation, "folders": stats["folders"], "total_bytes": stats["total_bytes"],
                        "digest": stats.get("digest"), "page_cache": cache}, indent=1))
    return {"seed": seed, "events": events, "clicks": clicks, "generation": generation, "dataset": stats,
            "warm_read_seconds": warm_seconds, "page_cache_after_warm": cache}


def cache_label(before, after):
    def pct(state):
        return None if not state else state.get("resident_pct")
    return {"condition": "warm", "resident_pct_before": pct(before), "resident_pct_after": pct(after)}


def setup_harness(args, result):
    work = Path(args.work)
    hw = hardware(work)
    cpus, mem_g = resource_limits(hw)
    cpuset = format_cpu_list(pick_cpuset(cpus)) if not args.no_cpuset else None
    harness = Harness(work, cpus, mem_g, cpuset)
    harness.probe_cpuset(args.diag_image, ["noop"])
    result.update({
        "hardware": hw,
        "limits": {"cpus": cpus, "memory_g": mem_g, "cpuset": harness.cpuset, "flags": harness.describe(),
                   "scorer": f"--cpus {SCORER_CPUS} --memory {SCORER_MEM_G}g, eight pinned hyperthreads"},
        "flag_parity": flag_parity(),
        "mode": "pipeline" if args.image else "stand-in",
        "images": {"pipeline": image_info(args.image) if args.image else None, "diag": image_info(args.diag_image)},
    })
    if not result["flag_parity"]["ok"]:
        result["errors"].append("the harness's docker flags differ from scripts/docker_run.sh: " + result["flag_parity"].get("detail", ""))
    return harness


def diagnostics(harness, args, data, runs):
    """Container start-up cost and the read-through floor, each from fresh containers."""
    diag = {}
    before = page_cache_state(data)
    with group("diagnostic (i): container start-up, `noop` in a fresh container"):
        diag["noop"] = harness.series(args.diag_image, "noop", data, runs, command=["noop"])
        diag["noop_split"] = harness.series(args.diag_image, "noop-split", data, runs, command=["noop"], split=True)
    with group("diagnostic (ii): read-through of every input byte"):
        diag["readthrough"] = {}
        for mode in READTHROUGH_MODES:
            diag["readthrough"][mode] = harness.series(args.diag_image, f"readthrough-{mode}", data, runs,
                                                       command=["readthrough", "--mode", mode])
    with group("what a container sees (sysinfo)"):
        info = harness.run(args.diag_image, "sysinfo", data, command=["sysinfo"])
        diag["sysinfo"] = [line[len("SYSINFO "):] for line in info.get("stdout_tail", "").splitlines() if line.startswith("SYSINFO ")]
        log("\n".join(diag["sysinfo"]))
    diag["cache"] = cache_label(before, page_cache_state(data))
    return diag


def cmd_bench20m(args):
    result = base_result("bench-20m")
    result["seeds"] = []
    try:
        harness = setup_harness(args, result)
        work = Path(args.work)
        prior_image = args.prior_image or None
        result["prior"] = {"sha": args.prior_sha or None, "status": args.prior_status or ("built" if prior_image else "absent")}
        if prior_image:
            result["images"]["prior"] = image_info(prior_image)
        for seed in [int(token) for token in re.split(r"[\s,]+", args.seeds.strip()) if token]:
            data = work / f"data-s{seed}"
            entry = prepare_dataset(args, seed, args.events, args.clicks, data)
            result["seeds"].append(entry)
            entry["diag"] = diagnostics(harness, args, data, args.runs)
            if not all(series["ok"] for series in [entry["diag"]["noop"], *entry["diag"]["readthrough"].values()]):
                result["errors"].append(f"seed {seed}: a diagnostics container failed")
            if args.image:
                bench_pipeline(harness, args, entry, data, prior_image, result)
            else:
                entry["pipeline"] = None
            remove_tree(data)
        if not args.image:
            result["skipped"] = "no root Dockerfile at this commit: only the diagnostics ran"
    except Exception as error:  # noqa: BLE001 - always leave a report behind
        traceback.print_exc()
        result["errors"].append(f"harness error: {type(error).__name__}: {error}")
    return finish(args, result)


def bench_pipeline(harness, args, entry, data, prior_image, result):
    """(iii) timed fresh-container runs, balanced with the prior best when there is one; (iv) every
    output diffed against /reference on the same input; (v) one extra run with SS_TIMING=1."""
    seed, work, runs = entry["seed"], harness.work, args.runs
    images = {"new": args.image}
    if prior_image:
        images["prior"] = prior_image
    outputs = {}  # label -> (which, folder)
    samples = {which: [] for which in images}
    order = []
    cache_before = page_cache_state(data)
    with group(f"seed {seed} (iii): {runs} timed fresh-container runs" + (" each, prior and new in balanced order" if prior_image else "")):
        for index in range(runs):
            pair = ["new"] if not prior_image else (["prior", "new"] if index % 2 == 0 else ["new", "prior"])
            for which in pair:
                label = f"{which}-{index + 1}"
                out = work / f"out-{label}"
                run = harness.run(images[which], f"s{seed}-{label}", data, out_dir=out)
                run["sequence"] = len(order) + 1
                run["outputs"] = output_summary(out)
                order.append(which)
                samples[which].append(run)
                outputs[label] = (which, out)
    pipeline = summarize(samples["new"])
    pipeline["cache"] = cache_label(cache_before, page_cache_state(data))
    pipeline["order"] = order
    entry["pipeline"] = pipeline
    if prior_image:
        entry["prior"] = summarize(samples["prior"])
        entry["prior"]["cache"] = pipeline["cache"]
    with group(f"seed {seed} (v): one extra run with SS_TIMING=1"):
        for which, image in images.items():
            label = f"{which}-timing"
            out = work / f"out-{label}"
            run = harness.run(image, f"s{seed}-{label}", data, env={"SS_TIMING": "1"}, out_dir=out)
            run["outputs"] = output_summary(out)
            (pipeline if which == "new" else entry["prior"])["timing_run"] = run
            outputs[label] = (which, out)
    with group(f"seed {seed}: one extra run for peak memory and disk use"):
        label = "new-memory"
        out = work / f"out-{label}"
        run = harness.run(args.image, f"s{seed}-{label}", data, out_dir=out, monitor=True)
        run["outputs"] = output_summary(out)
        pipeline["memory_run"] = run
        outputs[label] = ("new", out)
    with group(f"seed {seed} (iv): /reference on the same input, then compare.py on both tables of every run"):
        reference_out = work / "out-reference"
        reference = harness.run(args.image, f"s{seed}-reference", data, entrypoint="/reference", out_dir=reference_out,
                                timeout=3600)
        reference["outputs"] = output_summary(reference_out)
        entry["reference"] = reference
        entry["compare"] = []
        if reference["ok"]:
            entry["person_skew"] = person_skew(reference_out)
            entry["reference_self_check"] = self_check(reference_out)
            for label, (which, out) in outputs.items():
                comparison = compare_outputs(out, reference_out)
                comparison.update({"run": label, "which": which})
                entry["compare"].append(comparison)
                log(f"  {label} vs /reference: {'MATCH' if comparison['match'] else 'DIFFER'}")
                if not comparison["match"]:
                    log(comparison["output"])
        else:
            result["errors"].append(f"seed {seed}: /reference failed (exit {reference.get('exit_code')}), so the benchmark has no correctness check and is void")
    if args.cold_run:
        with group(f"seed {seed}: one run from a cold page cache (labelled, never mixed into the medians)"):
            if drop_page_cache():
                before = page_cache_state(data)
                run = harness.run(args.image, f"s{seed}-new-cold", data, out_dir=work / "out-new-cold")
                run["cache"] = {"condition": "cold (sync; echo 3 > /proc/sys/vm/drop_caches just before)",
                                "resident_pct_before": before and before.get("resident_pct"),
                                "resident_pct_after": (page_cache_state(data) or {}).get("resident_pct")}
                pipeline["cold_run"] = run
                remove_tree(work / "out-new-cold")
            else:
                result["warnings"].append("could not drop the page cache (no sudo): the cold run was skipped")
    # Verdicts: the new image must exit 0 and match on every run; the prior image is only reported.
    new_runs = samples["new"] + [pipeline.get("timing_run"), pipeline.get("memory_run")]
    for run in new_runs:
        if run and not run["ok"]:
            result["errors"].append(f"seed {seed}: pipeline run {run['label']} failed (exit {run.get('exit_code')}"
                                    + (", OOM-killed" if run.get("oom_killed") else "") + ")")
    for comparison in entry["compare"]:
        if not comparison["match"]:
            message = f"seed {seed}: output of {comparison['run']} DIFFERS from /reference"
            (result["errors"] if comparison["which"] == "new" else result["warnings"]).append(message)
    for run in samples.get("prior", []):
        if not run["ok"]:
            result["warnings"].append(f"seed {seed}: prior-best run {run['label']} failed (exit {run.get('exit_code')})")
    for _, out in outputs.values():
        remove_tree(out)
    remove_tree(work / "out-reference")


def cmd_bench60m(args):
    result = base_result("bench-60m")
    try:
        harness = setup_harness(args, result)
        work = Path(args.work)
        data = work / f"data-60m-s{args.seed}"
        result["disk_before"] = {"free_bytes": shutil.disk_usage(work).free}
        entry = prepare_dataset(args, args.seed, args.events, args.clicks, data)
        result["seeds"] = [entry]
        result["disk_after_generation"] = {"free_bytes": shutil.disk_usage(work).free}
        if not args.image:
            result["skipped"] = "no root Dockerfile at this commit: only the dataset was generated"
            remove_tree(data)
            return finish(args, result)
        cache_before = page_cache_state(data)
        with group("60M: one pipeline run with the scorer's flags (memory and disk sampled)"):
            out = work / "out-60m"
            run = harness.run(args.image, "60m-pipeline", data, out_dir=out, monitor=True, timeout=1800)
            run["outputs"] = output_summary(out)
            run["cache"] = cache_label(cache_before, page_cache_state(data))
            entry["pipeline_run"] = run
        if not run["ok"]:
            result["errors"].append(f"60M pipeline run failed (exit {run.get('exit_code')}"
                                    + (", OOM-killed" if run.get("oom_killed") else "") + ")")
        else:
            seconds = max(value for value in (run.get("docker_s"), run.get("wall_s")) if value is not None)
            if seconds >= HARD_CAP_S:
                result["errors"].append(f"60M pipeline run took {seconds:.1f} s: over the {HARD_CAP_S:.0f} s cap")
            entry["self_check"] = self_check(out)
            if not entry["self_check"].get("ok"):
                result["errors"].append("60M: the rules' self-check failed on the pipeline output")
            with group("60M: /reference on the same input (if the runner's memory allows), then compare.py"):
                reference_out = work / "out-60m-reference"
                reference = harness.run(args.image, "60m-reference", data, entrypoint="/reference",
                                        out_dir=reference_out, monitor=True, timeout=3000)
                reference["outputs"] = output_summary(reference_out)
                entry["reference"] = reference
                if reference["ok"]:
                    entry["person_skew"] = person_skew(reference_out)
                    comparison = compare_outputs(out, reference_out)
                    comparison.update({"run": "60m-pipeline", "which": "new"})
                    entry["compare"] = [comparison]
                    log(comparison["output"])
                    if not comparison["match"]:
                        result["errors"].append("60M: pipeline output DIFFERS from /reference")
                else:
                    entry["compare"] = []
                    result["warnings"].append(
                        f"60M: /reference did not finish on this runner (exit {reference.get('exit_code')}"
                        + (", OOM-killed" if reference.get("oom_killed") else "")
                        + "); correctness rests on the rules' self-check only")
                remove_tree(reference_out)
        result["disk_after_runs"] = {"free_bytes": shutil.disk_usage(work).free}
        remove_tree(work / "out-60m")
        remove_tree(data)
    except Exception as error:  # noqa: BLE001 - always leave a report behind
        traceback.print_exc()
        result["errors"].append(f"harness error: {type(error).__name__}: {error}")
    return finish(args, result)


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    commands = parser.add_subparsers(dest="command", required=True)

    commands.add_parser("parity").set_defaults(run=cmd_parity)

    dataset = commands.add_parser("dataset")
    dataset.add_argument("dir")
    dataset.set_defaults(run=cmd_dataset)

    correctness = commands.add_parser("correctness")
    correctness.add_argument("--image", default="", help="pipeline image tag; empty when there is no root Dockerfile")
    correctness.add_argument("--tools", required=True, help="folder with generate and dev-answer/ (bench/fetch_tools.sh)")
    correctness.add_argument("--work", required=True)
    correctness.add_argument("--out-json", required=True)
    correctness.set_defaults(run=cmd_correctness)

    def common(sub, events, clicks):
        sub.add_argument("--image", default="", help="pipeline image tag; empty when there is no root Dockerfile")
        sub.add_argument("--diag-image", required=True)
        sub.add_argument("--generator", required=True)
        sub.add_argument("--work", required=True)
        sub.add_argument("--out-json", required=True)
        sub.add_argument("--events", type=int, default=events)
        sub.add_argument("--clicks", type=int, default=clicks)
        sub.add_argument("--no-cpuset", action="store_true", help="do not pin with --cpuset-cpus")

    bench20 = commands.add_parser("bench20m")
    common(bench20, 20_000_000, 5_000_000)
    bench20.add_argument("--seeds", default="2", help="seed list, separated by spaces or commas")
    bench20.add_argument("--runs", type=int, default=3)
    bench20.add_argument("--prior-image", default="")
    bench20.add_argument("--prior-sha", default="")
    bench20.add_argument("--prior-status", default="")
    bench20.add_argument("--cold-run", action="store_true", help="add one labelled run from a cold page cache")
    bench20.set_defaults(run=cmd_bench20m)

    bench60 = commands.add_parser("bench60m")
    common(bench60, 60_000_000, 15_000_000)
    bench60.add_argument("--seed", type=int, default=2)
    bench60.set_defaults(run=cmd_bench60m)

    args = parser.parse_args()
    sys.exit(args.run(args))


if __name__ == "__main__":
    main()
