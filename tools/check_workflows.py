"""Sanity-check the GitHub Actions workflows without GitHub's own validator.

Catches what actually costs a CI cycle: YAML that does not parse, a job with no
`runs-on`, a step that has neither `run` nor `uses` (or both), an unknown
top-level key, and a workflow that would never trigger.

    python tools/check_workflows.py .github/workflows
"""

import sys
from pathlib import Path

import yaml

# GitHub's own keys. `on` arrives as True under YAML 1.1, hence the mapping.
TOP_LEVEL = {"name", "run-name", "on", True, "env", "defaults", "concurrency", "permissions", "jobs"}
JOB_KEYS = {
    "name", "runs-on", "needs", "if", "permissions", "environment", "concurrency",
    "outputs", "env", "defaults", "steps", "timeout-minutes", "strategy",
    "continue-on-error", "container", "services", "uses", "with", "secrets",
}
STEP_KEYS = {
    "id", "if", "name", "uses", "run", "working-directory", "shell", "with", "env",
    "continue-on-error", "timeout-minutes",
}

problems = []


def fail(path, message):
    problems.append(f"{path}: {message}")


def check_step(path, job, index, step):
    where = f"jobs.{job}.steps[{index}]"
    if not isinstance(step, dict):
        fail(path, f"{where} is not a mapping")
        return
    for key in step:
        if key not in STEP_KEYS:
            fail(path, f"{where} has unknown key '{key}'")
    has_run, has_uses = "run" in step, "uses" in step
    if not has_run and not has_uses:
        fail(path, f"{where} has neither 'run' nor 'uses'")
    if has_run and has_uses:
        fail(path, f"{where} has both 'run' and 'uses'")
    if has_uses and "@" not in str(step["uses"]):
        fail(path, f"{where} uses '{step['uses']}' without a version")
    if has_run and "name" not in step:
        fail(path, f"{where} has no 'name' (a run step should be labelled)")
    if not has_run and not has_uses:
        return
    label = step.get("name", step.get("uses", "<unnamed>"))
    return label


def check_workflow(path):
    text = path.read_text(encoding="utf-8")
    try:
        doc = yaml.safe_load(text)
    except yaml.YAMLError as error:
        fail(path, f"does not parse: {error}")
        return

    if not isinstance(doc, dict):
        fail(path, "top level is not a mapping")
        return

    for key in doc:
        if key not in TOP_LEVEL:
            fail(path, f"unknown top-level key '{key}'")

    triggers = doc.get("on", doc.get(True))
    if not triggers:
        fail(path, "has no 'on:' trigger, so it would never run")

    jobs = doc.get("jobs")
    if not isinstance(jobs, dict) or not jobs:
        fail(path, "has no jobs")
        return

    for job_name, job in jobs.items():
        if not isinstance(job, dict):
            fail(path, f"job '{job_name}' is not a mapping")
            continue
        for key in job:
            if key not in JOB_KEYS:
                fail(path, f"job '{job_name}' has unknown key '{key}'")
        # A reusable-workflow call has `uses` instead of `runs-on`.
        if "uses" not in job and "runs-on" not in job:
            fail(path, f"job '{job_name}' has no 'runs-on'")
        steps = job.get("steps")
        if steps is None:
            continue
        if not isinstance(steps, list):
            fail(path, f"job '{job_name}' has non-list 'steps'")
            continue
        for index, step in enumerate(steps):
            check_step(path, job_name, index, step)
        print(f"  {job_name}: {len(steps)} steps on {job.get('runs-on', '(reusable)')}")


def main():
    root = Path(sys.argv[1] if len(sys.argv) > 1 else ".github/workflows")
    files = sorted(root.glob("*.y*ml"))
    if not files:
        print(f"no workflows under {root}")
        return 1
    for path in files:
        print(f"{path}")
        check_workflow(path)
    print()
    if problems:
        print(f"{len(problems)} problem(s):")
        for problem in problems:
            print(f"  - {problem}")
        return 1
    print(f"OK: {len(files)} workflow(s) look well-formed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
