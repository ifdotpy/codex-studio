"""Stdlib case workers; run-guest-suite.mjs owns the deadline and process cleanup."""
from concurrent.futures import ProcessPoolExecutor
from concurrent.futures.process import BrokenProcessPool
from contextlib import redirect_stderr, redirect_stdout
import io
import multiprocessing
import os
from pathlib import Path
import sys
import tempfile
import textwrap
import time
import unittest

# Ordinary checks share at most four processes, including on large build hosts.
MAX_WORKERS = 4
PRIVILEGED_COMMANDS = {"groupadd", "useradd", "usermod", "groupmod", "groupdel", "userdel", "runuser", "sudo"}


def prepare(directory):
    sys.path.insert(0, directory)
    # Tag pool workers by cwd so the Node leak check also finds them after a timeout.
    os.chdir(tempfile.gettempdir())
    os.environ.pop("LAYR_E2E_ROOT", None)

    def guard(event, args):
        if event == "subprocess.Popen":
            executable = Path(os.fsdecode(args[0]))
            if executable.name in PRIVILEGED_COMMANDS and not executable.is_relative_to(tempfile.gettempdir()):
                raise RuntimeError(f"privileged command forbidden in guest contracts: {executable.name}")

    sys.addaudithook(guard)


def run_case(identity):
    output = io.StringIO()
    started = time.monotonic()
    with redirect_stdout(output), redirect_stderr(output):
        suite = unittest.defaultTestLoader.loadTestsFromName(identity)
        result = unittest.TextTestRunner(stream=output, verbosity=2).run(suite)
    return (result.testsRun, len(result.failures), len(result.errors) + len(result.unexpectedSuccesses),
            len(result.skipped), time.monotonic() - started, output.getvalue())


def cases(suite):
    for entry in suite:
        if isinstance(entry, unittest.TestSuite):
            yield from cases(entry)
        else:
            yield entry.id()


def main():
    directory = str(Path(sys.argv[1]).resolve()) if len(sys.argv) > 1 else str(Path(__file__).parent)
    workers = int(os.environ.get("STUDIO_GUEST_WORKERS", min(MAX_WORKERS, os.cpu_count() or 1)))
    if not 1 <= workers <= MAX_WORKERS:
        raise ValueError(f"STUDIO_GUEST_WORKERS must be between 1 and {MAX_WORKERS}")
    prepare(directory)
    suite = unittest.defaultTestLoader.discover(directory)
    if unittest.defaultTestLoader.errors:
        raise RuntimeError("\n".join(unittest.defaultTestLoader.errors))
    names = sorted(cases(suite))
    total = failures = errors = skipped = 0
    started = time.monotonic()
    with ProcessPoolExecutor(max_workers=workers, mp_context=multiprocessing.get_context("spawn"),
                             initializer=prepare, initargs=(directory,)) as pool:
        # Each case owns its fixtures; map preserves discovery order and buffered output.
        results = pool.map(run_case, names)
        for index, identity in enumerate(names):
            try:
                ran, failed, errored, skip, seconds, output = next(results)
            except BrokenProcessPool as error:
                raise RuntimeError(f"test worker crashed; unfinished: {', '.join(names[index:])}") from error
            print(f"{identity}: {ran} tests in {seconds:.3f}s", file=sys.stderr)
            if failed or errored or skip:
                print(textwrap.indent(output, "  "), file=sys.stderr, end="")
            total += ran
            failures += failed
            errors += errored
            skipped += skip
    print(f"Ran {total} tests in {time.monotonic() - started:.3f}s", file=sys.stderr)
    if failures or errors:
        print(f"FAILED (failures={failures}, errors={errors}, skipped={skipped})", file=sys.stderr)
        return 1
    print(f"OK (skipped={skipped})", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
