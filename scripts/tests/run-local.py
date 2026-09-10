"""Exercise launcher process cleanup without building or touching user settings."""
import os
from pathlib import Path
import signal
import subprocess
import tempfile
import time

SOURCE = Path(__file__).resolve().parents[1] / "run-local"


def exercise(fail_ui=False):
    with tempfile.TemporaryDirectory() as directory:
        root = Path(directory)
        (root / "frontend").mkdir()
        (root / "examples/standalone").mkdir(parents=True)
        (root / "examples/standalone/config.yaml").write_text("{}")
        (root / "scripts").mkdir()
        launcher = root / "scripts/run-local"
        launcher.write_bytes(SOURCE.read_bytes())
        launcher.chmod(0o755)
        mocks = root / "bin"
        mocks.mkdir()
        for name in ("pnpm", "cargo"):
            path = mocks / name
            path.write_text("#!/bin/sh\nexit 0\n")
            path.chmod(0o755)
        for profile in ("debug", "release"):
            binary = root / f"target/{profile}/agentdesktop"
            binary.parent.mkdir(parents=True)
            binary.write_text('''#!/usr/bin/env bash
printf '%s\\n' "$*" >> "$TEST_ROOT/args"
sleep 300 &
echo "$!" >> "$TEST_ROOT/children"
if [[ "$*" != *daemon* && "$FAIL_UI" == 1 ]]; then sleep 1; exit 7; fi
wait
''')
            binary.chmod(0o755)
        env = dict(os.environ, PATH=f"{mocks}:{os.environ['PATH']}",
                   TEST_ROOT=str(root), FAIL_UI=str(int(fail_ui)))
        args = [str(launcher), "--user"] + ([] if fail_ui else ["--dev"])
        with (root / "output").open("w") as output:
            process = subprocess.Popen(args, env=env, stdout=output, stderr=output)
            try:
                deadline = time.monotonic() + 10
                while time.monotonic() < deadline:
                    children = root / "children"
                    if children.exists() and len(children.read_text().splitlines()) == 2:
                        break
                    if process.poll() is not None:
                        raise AssertionError((root / "output").read_text())
                    time.sleep(0.05)
                else:
                    raise AssertionError("Components did not start")
                if not fail_ui:
                    process.send_signal(signal.SIGTERM)
                assert process.wait(timeout=10) == (7 if fail_ui else 143)
                assert "--user" in (root / "args").read_text()
                for pid in children.read_text().splitlines():
                    # A killed orphan can briefly remain as a zombie in containers.
                    result = subprocess.run(["ps", "-o", "stat=", "-p", pid],
                                            capture_output=True, text=True)
                    assert not result.stdout.strip() or result.stdout.strip().startswith("Z"), pid
            finally:
                if process.poll() is None:
                    process.kill()
                    process.wait()


exercise()
exercise(fail_ui=True)
print("run-local: signal and component-exit cleanup passed")
