#!/usr/bin/env python3
"""Explicit test-session guardian. No prefix scans, no product integration.

Registration precedes launch. One detached watcher per owner process instance;
records survive both owner and watcher crashes. Only registered sessions can be
cleaned. The identical test-support copy in the core fork is independently usable.
"""
import concurrent.futures
import ctypes
import errno
import fcntl
import hashlib
import json
import os
from pathlib import Path
import select
import signal
import stat
import struct
import subprocess
import sys
import time
import uuid

MARKER = 'NEARSHELL_TEST_SESSION_ID'
BASE = Path('/var/tmp') / ('nearshell-test-guardian-' + str(os.getuid()))
LIB = ctypes.CDLL('/usr/lib/libproc.dylib' if sys.platform == 'darwin' else None, use_errno=True)


def instance(pid):
    """None means proven absence, not permission failure. Start time defeats reuse."""
    if sys.platform == 'darwin':
        buf = ctypes.create_string_buffer(136)
        n = LIB.proc_pidinfo(pid, 3, 0, buf, len(buf))  # PROC_PIDTBSDINFO
        if n == len(buf):
            fields = struct.unpack_from('12I', buf.raw)
            if fields[1] == 5:
                return None
            return [pid, fields[5], *struct.unpack_from('QQ', buf.raw, 120)]
        if ctypes.get_errno() == errno.ESRCH:
            return None
        raise OSError('process_identity_unavailable')
    try:
        fields = Path('/proc', str(pid), 'stat').read_text().rsplit(')', 1)[1].split()
        if fields[0] == 'Z':
            return None
        return [pid, Path('/proc', str(pid)).stat().st_uid, int(fields[19]), 0]
    except FileNotFoundError:
        return None


def alive(identity):
    return instance(identity[0]) == identity


def private(path, directory=False):
    flags = os.O_RDONLY | os.O_NOFOLLOW | (os.O_DIRECTORY if directory else 0)
    fd = os.open(path, flags)
    s = os.fstat(fd)
    if (s.st_uid != os.getuid() or s.st_mode & 0o077
            or not (stat.S_ISDIR(s.st_mode) if directory else stat.S_ISREG(s.st_mode) and s.st_nlink == 1)):
        os.close(fd)
        raise ValueError('unsafe_registry')
    return fd


def directory(path):
    try:
        path.mkdir(mode=0o700)
    except FileExistsError:
        pass
    os.close(private(path, True))


def write(path, value):
    temp = path.with_name('.' + uuid.uuid4().hex)
    fd = os.open(temp, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
    with os.fdopen(fd, 'w') as f:
        json.dump(value, f)
        f.flush()
        os.fsync(f.fileno())
    os.replace(temp, path)


def read(path):
    with os.fdopen(private(path)) as f:
        return json.load(f)


def locked(path, private_file=True):
    fd = os.open(path, os.O_RDWR | os.O_CREAT | os.O_NOFOLLOW, 0o600)
    s = os.fstat(fd)
    if s.st_uid != os.getuid() or s.st_mode & (0o077 if private_file else 0o022) or not stat.S_ISREG(s.st_mode) or s.st_nlink != 1:
        os.close(fd)
        raise ValueError('unsafe_lock')
    try:
        fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        return fd
    except BlockingIOError:
        os.close(fd)
        return None


def digest(path):
    with open(path, 'rb') as f:
        return hashlib.file_digest(f, 'sha256').hexdigest() if hasattr(hashlib, 'file_digest') else hashlib.sha256(f.read()).hexdigest()


def matching_processes(nonce):
    """Only a random marker installed by a registered test, never a path prefix."""
    needle = (MARKER + '=' + nonce).encode()
    if sys.platform == 'darwin':
        count = LIB.proc_listallpids(None, 0)
        array = (ctypes.c_int * (count + 256))()
        n = LIB.proc_listallpids(array, ctypes.sizeof(array))
        pids = list(array[:n])
    else:
        pids = [int(p.name) for p in Path('/proc').iterdir() if p.name.isdigit()]
    result = []
    relatives = {}
    foreign = set()
    buf = ctypes.create_string_buffer(1024 * 1024) if sys.platform == 'darwin' else None
    for pid in pids:
        if pid <= 1 or pid == os.getpid():
            continue
        try:
            ident = instance(pid)
            if not ident or ident[1] != os.getuid():
                continue
            if sys.platform == 'darwin':
                info = ctypes.create_string_buffer(136)
                if LIB.proc_pidinfo(pid, 3, 0, info, len(info)) == len(info):
                    relatives[pid] = (struct.unpack_from('I', info.raw, 16)[0], ident)
                # KERN_PROCARGS2 includes env even on macOS where ps -E omits it.
                mib = (ctypes.c_int * 3)(1, 49, pid)
                size = ctypes.c_size_t(len(buf))
                if LIB.sysctl(mib, 3, buf, ctypes.byref(size), None, 0):
                    continue
                data = buf.raw[:size.value]
                argc = struct.unpack_from('i', data)[0]
                pos = data.index(b'\0', 4)
                while pos < len(data) and data[pos] == 0:
                    pos += 1
                for _ in range(argc):
                    pos = data.index(b'\0', pos) + 1
                env = data[pos:].split(b'\0')
            else:
                parent = int(Path('/proc', str(pid), 'stat').read_text().rsplit(')', 1)[1].split()[1])
                relatives[pid] = (parent, ident)
                env = Path('/proc', str(pid), 'environ').read_bytes().split(b'\0')
            if any(e.startswith((MARKER + '=').encode()) and e != needle for e in env):
                foreign.add(pid)
            if needle in env and alive(ident):
                result.append(ident)
        except (OSError, ValueError):
            continue
    # SIP binaries (e.g. /bin/sh, /bin/sleep) hide env in KERN_PROCARGS2.
    # Follow observed descendants of a verified marker, never an unrelated PID
    # or a subtree explicitly belonging to another registered session.
    selected = {i[0]: i for i in result}
    while True:
        added = {pid: ident for pid, (parent, ident) in relatives.items()
                 if pid not in selected and pid not in foreign and parent in selected
                 and ident[2:] >= selected[parent][2:] and alive(selected[parent]) and alive(ident)}
        if not added:
            break
        selected.update(added)
    return list(selected.values())


def invoke(argv, env, timeout=6):
    try:
        return subprocess.run(argv, env=env, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                              stderr=subprocess.DEVNULL, timeout=timeout, close_fds=True).returncode == 0
    except (OSError, subprocess.TimeoutExpired):
        return False


def clean(record, allow_live=False):
    lease = locked(record.with_suffix('.lock'))
    if lease is None:
        return False
    started = time.monotonic()
    signals = 0
    owner_lease = None
    try:
        if not record.exists():
            return True
        spec = read(record)
        if alive(spec['owner']) and not allow_live:
            return False
        root = Path(spec['root'])
        nonce = spec['nonce']
        if (spec.get('schema') != 1 or spec['owner'][1] != os.getuid() or len(nonce) != 32
                or any(c not in '0123456789abcdef' for c in nonce)
                or not str(root.resolve()).startswith(str(Path('/var/tmp').resolve()) + '/')):
            raise ValueError('invalid_record')
        s = root.lstat() if root.exists() or root.is_symlink() else None
        if s and (not stat.S_ISDIR(s.st_mode) or [s.st_dev, s.st_ino, s.st_uid] != spec['rootIdentity']):
            raise ValueError('root_identity_changed')
        ok = True
        if s:
            command = spec['cleanup']
            try:
                trusted = (digest(command[0]) == spec['cleanupSHA256']
                    and all(digest(path) == expected for path, expected in spec.get('expectedFiles', {}).items()))
            except OSError:
                trusted = False
            # A rebuilt/missing helper is unavailable, never executable trust.
            # The independent nonce/owner proof still permits scoped fallback.
            ok = False
            if trusted and not spec.get('attempted', False):
                # Crash/ambiguous reply must not cause a second stop on sweep.
                spec['attempted'] = True
                write(record, spec)
                ok = invoke(command, spec['environment'])
        # Managed cleanup reuses Swift's CoreCleanupBarrier; fork cleanup first
        # uses server stop. Escalation applies ONLY to newly nonce-tagged tests.
        remaining = matching_processes(nonce) if spec.get('marked') else []
        deadline = time.monotonic() + .5
        while ok and remaining and time.monotonic() < deadline:
            time.sleep(.02)
            remaining = [i for i in remaining if alive(i)]
        if remaining and (root / 'owner.lock').exists():
            owner_lease = locked(root / 'owner.lock', private_file=False)
            if owner_lease is None:
                raise ValueError('successor_owner_active')
        for sig in (signal.SIGTERM, signal.SIGKILL):
            if not remaining:
                break
            for ident in remaining:
                if alive(ident):
                    os.kill(ident[0], sig)
                    signals += 1
            deadline = time.monotonic() + .5
            while time.monotonic() < deadline and any(alive(i) for i in remaining):
                time.sleep(.02)
            observed = {i[0]: i for i in remaining if alive(i)}
            observed.update({i[0]: i for i in matching_processes(nonce)})
            remaining = list(observed.values())
        # No recursive deletion here: fixtures/Swift barrier own data removal.
        if not remaining and (ok or spec.get('marked')):
            write(record.with_suffix('.outcome'), dict(result='stopped', elapsedMs=round((time.monotonic()-started)*1000), signals=signals))
            record.unlink()
            # Normal teardown must still fail if core's stop leaked a process.
            # Emergency owner-death cleanup is expected to use escalation.
            return not allow_live or signals == 0
        raise ValueError('shutdown_incomplete')
    except (OSError, ValueError, KeyError, TypeError, IndexError):
        print('test_guardian_cleanup_unconfirmed', file=sys.stderr)
        return False
    finally:
        if owner_lease is not None:
            os.close(owner_lease)
        os.close(lease)


def sweep():
    # No scan of arbitrary scratch roots, and no legacy import.
    for group in BASE.iterdir():
        if not group.is_dir() or group.is_symlink():
            continue
        try:
            os.close(private(group, True))
        except (OSError, ValueError):
            continue
        for record in group.glob('*.json'):
            try:
                if not alive(read(record)['owner']):
                    clean(record)
            except (OSError, ValueError, KeyError, TypeError, IndexError):
                pass


def watch(group, owner):
    lease = locked(group / 'watch.lock')
    if lease is None:
        print('READY', flush=True)
        return
    # Register NOTE_EXIT before reporting readiness. Polling also notices already
    # dead/PID-reused owners, so registration/death has no unobserved gap.
    queue = None
    if sys.platform == 'darwin' and alive(owner):
        queue = select.kqueue()
        try:
            queue.control([select.kevent(owner[0], filter=select.KQ_FILTER_PROC,
                flags=select.KQ_EV_ADD | select.KQ_EV_ONESHOT, fflags=select.KQ_NOTE_EXIT)], 0, 0)
        except ProcessLookupError:
            pass
    write(group / 'watcher', instance(os.getpid()))
    print('READY', flush=True)
    sys.stdout.close()
    try:
        while alive(owner):
            if queue:
                queue.control(None, 1, .5)
            else:
                time.sleep(.1)
        records = list(group.glob('*.json'))
        with concurrent.futures.ThreadPoolExecutor(max_workers=8) as pool:
            list(pool.map(clean, records))
    finally:
        if queue:
            queue.close()
        os.close(lease)


def register(spec):
    owner = instance(int(spec['owner']))
    if not owner or owner[1] != os.getuid() or owner[0] != os.getppid():
        raise ValueError('owner_identity_invalid')
    root = Path(spec['root'])
    # All callers opt in before launch. No root beneath Application Support.
    if (not root.is_absolute() or '..' in root.parts or not str(root).startswith('/var/tmp/')
            or not str(root.resolve()).startswith(str(Path('/var/tmp').resolve()) + '/')):
        raise ValueError('not_test_root')
    root.mkdir(mode=0o700, parents=True, exist_ok=True)
    s = root.lstat()
    if not stat.S_ISDIR(s.st_mode) or s.st_uid != os.getuid() or s.st_mode & 0o022:
        raise ValueError('unsafe_root')
    directory(BASE)
    sweep()
    group = BASE / hashlib.sha256(json.dumps(owner).encode()).hexdigest()[:24]
    directory(group)
    record = group / (hashlib.sha256(str(root).encode()).hexdigest()[:24] + '.json')
    if record.exists():
        previous = read(record)
        if previous['rootIdentity'] != [s.st_dev, s.st_ino, s.st_uid]:
            raise ValueError('root_replaced')
        nonce = previous['nonce']
        if previous.get('attempted'):
            spec['attempted'] = True
    else:
        nonce = uuid.uuid4().hex
    spec['expectedFiles'] = {path: digest(path) for path in spec.pop('verifyFiles', [])}
    spec['environment'][MARKER] = nonce
    spec.update(schema=1, owner=owner, nonce=nonce, rootIdentity=[s.st_dev, s.st_ino, s.st_uid],
                cleanupSHA256=digest(spec['cleanup'][0]))
    write(record, spec)
    child = subprocess.Popen([sys.executable, '-B', __file__, 'watch', str(group), json.dumps(owner)],
        stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
        close_fds=True, start_new_session=True)
    ready = select.select([child.stdout], [], [], 5)[0]
    if not ready or child.stdout.readline().strip() != b'READY':
        raise ValueError('guardian_not_ready')
    print('READY ' + nonce, flush=True)


def main():
    action = sys.argv[1]
    if action == 'run':
        # Explicit wrapper for diagnostic Apps/scripts; never installed or auto-used.
        import argparse
        parser = argparse.ArgumentParser()
        parser.add_argument('--root', required=True)
        parser.add_argument('--cleanup', required=True)
        parser.add_argument('--artifact', required=True, action='append')
        parser.add_argument('command', nargs=argparse.REMAINDER)
        args = parser.parse_args(sys.argv[2:])
        command = args.command[1:] if args.command[:1] == ['--'] else args.command
        if not command:
            raise ValueError('missing_test_command')
        spec = dict(root=args.root, owner=os.getpid(), cleanup=[args.cleanup, args.root] + args.artifact,
                    environment={'PATH':'/usr/bin:/bin'})
        result = subprocess.run([sys.executable, '-B', __file__, 'register'], input=json.dumps(spec),
                                text=True, stdout=subprocess.PIPE, check=True)
        if not result.stdout.startswith('READY '):
            raise ValueError('guardian_not_ready')
        sys.exit(subprocess.call(command))
    elif action == 'register':
        register(json.load(sys.stdin))
    elif action == 'watch':
        watch(Path(sys.argv[2]), json.loads(sys.argv[3]))
    elif action == 'finish':
        owner = instance(os.getppid())
        group = BASE / hashlib.sha256(json.dumps(owner).encode()).hexdigest()[:24]
        record = group / (hashlib.sha256(sys.argv[2].encode()).hexdigest()[:24] + '.json')
        if read(record)['owner'] != owner or not clean(record, allow_live=True):
            raise ValueError('finish_unconfirmed')
    elif action == 'upstream-stop':
        root, binary = Path(sys.argv[2]), sys.argv[3]
        env = dict(PATH=os.environ.get('PATH', '/usr/bin:/bin'), HOME=str(root), HC_SESSION='test',
                   XDG_CONFIG_HOME=str(root / 'config'), XDG_STATE_HOME=str(root / 'state'), XDG_RUNTIME_DIR=str(root / 'run'))
        if MARKER in os.environ:
            env[MARKER] = os.environ[MARKER]
        # Cover extra sessions/custom sockets created by tests, not just default.
        sockets = [p for p in root.rglob('*.sock') if '.holder' not in p.name and not p.is_symlink()
                   and stat.S_ISSOCK(p.lstat().st_mode) and p.resolve().is_relative_to(root.resolve())]
        if not sockets:
            sockets = [root / 'run/herdr-core/test.sock']
        ok = True
        for sock in sockets:
            scoped = dict(env, HC_SOCKET=str(sock))
            if not invoke([binary, 'server', 'status'], scoped, 2):
                ok = False
            if not invoke([binary, 'server', 'stop'], scoped, 3):
                ok = False
        if not ok:
            raise ValueError('stop_failed')
    elif action == 'managed-stop':
        root, binary, token = Path(sys.argv[2]), sys.argv[3], sys.argv[4]
        lease = locked(root / 'owner.lock', private_file=False)
        if lease is None:
            raise ValueError('owner_still_holds_lease')
        try:
            env = dict(PATH='/usr/bin:/bin', HOME=str(root), NEARSHELL_HC_TEST_ROOT=str(root))
            if (root / 'config/herdr-core/config.toml').exists():
                if not invoke([binary, '--owner-token', token, 'server', 'stop'], env, 5):
                    raise ValueError('stop_failed')
        finally:
            os.close(lease)
    elif action == 'sweep':
        directory(BASE)
        sweep()
    else:
        raise ValueError('unknown_action')


if __name__ == '__main__':
    try:
        main()
    except (OSError, ValueError, KeyError, TypeError, IndexError):
        print('test_guardian_failed', file=sys.stderr)
        sys.exit(1)
