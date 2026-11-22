"""Times several images on one runner, fresh container per run, in shuffled rounds.

python3 calib.py DATA_DIR ROUNDS IMAGE...   (prints one line per run and a median table)

Container time is State.StartedAt -> State.FinishedAt from `docker inspect`, as in
bench/bench.py. Flags as in the CI benchmark: 4 CPUs pinned, 13 GB, no network,
input read-only. Nothing here is ranked; it is calibration data only.
"""
import json
import os
import random
import statistics
import subprocess
import sys
import tempfile
from datetime import datetime


def stamp(text):
    # 2026-11-21T21:54:00.929097722Z -> seconds; Python takes microseconds only.
    head, frac = text.rstrip('Z').split('.')
    return datetime.fromisoformat(head).timestamp() + float('0.' + frac)


def run(image, data):
    out = tempfile.mkdtemp(prefix='calib-out-')
    os.chmod(out, 0o777)
    name = 'calib-%d' % random.randrange(1 << 30)
    create = ['docker', 'create', '--name', name, '--network', 'none', '--cpus', '4', '--cpuset-cpus', '0-3',
              '--memory', '13g', '--memory-swap', '13g', '-e', 'NCPU=4',
              '-v', '%s:/data:ro' % data, '-v', '%s:/output' % out, '-e', 'DATA_DIR=/data', '-e', 'OUTPUT_DIR=/output',
              image]
    subprocess.run(create, check=True, capture_output=True)
    subprocess.run(['docker', 'start', '-a', name], capture_output=True)
    state = json.loads(subprocess.run(['docker', 'inspect', name], check=True, capture_output=True, text=True).stdout)[0]['State']
    subprocess.run(['docker', 'rm', name], capture_output=True)
    subprocess.run(['sudo', 'rm', '-rf', out], capture_output=True)
    return (stamp(state['FinishedAt']) - stamp(state['StartedAt'])) * 1e3, state['ExitCode']


def main():
    data, rounds, images = sys.argv[1], int(sys.argv[2]), sys.argv[3:]
    times = {image: [] for image in images}
    for image in images:  # one warm-up run each, not counted
        run(image, data)
    for round_ in range(rounds):
        order = images[:]
        random.shuffle(order)
        for image in order:
            ms, code = run(image, data)
            times[image].append(ms)
            print('round %d %s %.1f ms exit %d' % (round_ + 1, image, ms, code), flush=True)
    print('\n| image | median ms | min ms | runs |')
    print('|---|---:|---:|---:|')
    for image in images:
        print('| %s | %.1f | %.1f | %d |' % (image, statistics.median(times[image]), min(times[image]), len(times[image])))


main()
