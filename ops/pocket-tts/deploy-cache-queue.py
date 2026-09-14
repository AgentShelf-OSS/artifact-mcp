"""One-time VM310 Pocket cache/queue cutover; run only after operator approval."""
import pathlib
import shutil
import subprocess
import time

STACK = pathlib.Path("/opt/docker/artifact-pocket-tts")
OLD_TAG = "homelab/artifact-pocket:3.1.0-timestamped-1"
NEW_TAG = "homelab/artifact-pocket:3.1.0-cache-queue-20260913"
OLD_ID = "sha256:d26ef8020fc5b8c57e91df985c0282d0cbde7cfb8089275711f728597ea45b55"
NEW_ID = "sha256:4c5c3ea88aa3b2b684839a2f11ccf968503640a7f46f95521290a0c29bf16fa0"


def output(*args):
    return subprocess.check_output(args, cwd=STACK, text=True).strip()


def recreate():
    subprocess.check_call(["docker", "compose", "up", "-d", "--no-build", "speech"], cwd=STACK)


def main():
    compose = STACK / "compose.yml"
    current = output("docker", "compose", "ps", "-q", "speech")
    if not current or output("docker", "inspect", current, "--format", "{{.Image}}") != OLD_ID:
        raise SystemExit("The running speech image differs from the reviewed production image")
    if output("docker", "image", "inspect", NEW_TAG, "--format", "{{.Id}}") != NEW_ID:
        raise SystemExit("The candidate image differs from the tested image")
    old_line, new_line = "image: " + OLD_TAG, "image: " + NEW_TAG
    original = compose.read_text()
    if original.count(old_line) != 1:
        raise SystemExit("Expected production image line missing or ambiguous")
    backup = STACK / ("compose.yml.pre-cache-queue-" + time.strftime("%Y%m%d-%H%M%S"))
    if backup.exists():
        raise SystemExit("Backup already exists")
    shutil.copy2(compose, backup)
    try:
        compose.write_text(original.replace(old_line, new_line, 1))
        subprocess.check_call(["docker", "compose", "config", "--quiet"], cwd=STACK)
        recreate()
        for _ in range(90):
            current = output("docker", "compose", "ps", "-q", "speech")
            if current and output("docker", "inspect", current, "--format", "{{.State.Health.Status}}") == "healthy":
                if output("docker", "inspect", current, "--format", "{{.Image}}") != NEW_ID:
                    raise RuntimeError("Running candidate digest mismatch")
                print("Speech is healthy. Compose backup:", backup)
                return
            time.sleep(2)
        raise RuntimeError("Speech did not become healthy within 180 seconds")
    except BaseException:
        shutil.copy2(backup, compose)
        recreate()
        raise


if __name__ == "__main__":
    main()
