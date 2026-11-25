import json
import os
import re
import subprocess
import sys
import time


def valid_sha(value):
    return isinstance(value, str) and re.fullmatch(r"[0-9a-f]{40}", value) is not None


def valid_id(value):
    return isinstance(value, int) and not isinstance(value, bool) and value > 0


def matching_run(repo, sha, run):
    try:
        return (valid_sha(sha) and isinstance(run, dict) and valid_id(run.get("id"))
                and valid_id(run.get("run_attempt")) and run.get("head_sha") == sha
                and run.get("event") == "push" and run.get("repository", {}).get("full_name") == repo)
    except AttributeError:
        return False


def successful_rust(run, jobs):
    if not isinstance(jobs, dict) or not isinstance(jobs.get("jobs"), list):
        return False
    entries = jobs["jobs"]
    if any(not isinstance(job, dict) or not isinstance(job.get("name"), str) for job in entries):
        return False
    rust = [job for job in entries if job.get("name") == "rust"]
    return (len(rust) == 1 and rust[0].get("status") == "completed"
            and rust[0].get("conclusion") == "success" and valid_id(rust[0].get("run_id"))
            and rust[0]["run_id"] == run["id"] and valid_id(rust[0].get("run_attempt"))
            and rust[0]["run_attempt"] == run["run_attempt"])


def master_sha(ref):
    try:
        obj = ref["object"]
        sha = obj["sha"]
        return sha if ref["ref"] == "refs/heads/master" and obj["type"] == "commit" and valid_sha(sha) else None
    except (KeyError, TypeError):
        return None


def release_proof(repo, sha, run, jobs, ref, compare):
    return (matching_run(repo, sha, run) and run.get("status") == "completed"
            and run.get("conclusion") == "success" and successful_rust(run, jobs)
            and master_sha(ref) is not None and isinstance(compare, dict)
            and compare.get("status") in ("ahead", "identical"))


def wait_for_release(repo, sha, fetch_runs, fetch_jobs, clock=time.monotonic, sleep=time.sleep):
    deadline = clock() + 300
    while True:
        response = fetch_runs()
        if not isinstance(response, dict) or not isinstance(response.get("workflow_runs"), list):
            return False
        runs = response["workflow_runs"]
        if any(not isinstance(run, dict) for run in runs):
            return False
        for run in runs:
            if (matching_run(repo, sha, run) and run.get("status") == "completed"
                    and run.get("conclusion") == "success" and successful_rust(run, fetch_jobs(run))):
                return True
        pending = any(matching_run(repo, sha, run) and run.get("status") in ("queued", "in_progress") for run in runs)
        remaining = deadline - clock()
        if not pending or remaining <= 0:
            return False
        sleep(min(15, remaining))


def check_release(repo, sha, fetch_runs, fetch_jobs, fetch_ref, fetch_compare):
    try:
        if not wait_for_release(repo, sha, fetch_runs, fetch_jobs):
            return False
        master = master_sha(fetch_ref())
        if not master:
            return False
        compare = fetch_compare(master)
        return isinstance(compare, dict) and compare.get("status") in ("ahead", "identical")
    except (KeyError, ValueError, TypeError, OSError, subprocess.SubprocessError):
        return False


def command(args):
    return subprocess.run(args, check=True, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, timeout=30).stdout.decode("utf-8")


def api(path):
    return json.loads(command(["gh", "api", path]))


def pages(path, key):
    result = json.loads(command(["gh", "api", "--paginate", "--slurp", path]))
    if not isinstance(result, list) or not result:
        raise ValueError("invalid pages")
    items = []
    for page in result:
        entries = page[key]
        if not isinstance(entries, list):
            raise ValueError("invalid page")
        items.extend(entries)
    return items


def main():
    if len(sys.argv) != 2 or sys.argv[1] != "release":
        return 2
    answer = False
    try:
        repo = os.environ["GITHUB_REPOSITORY"]
        sha = os.environ["GITHUB_SHA"]
        if not re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", repo) or not valid_sha(sha):
            raise ValueError("invalid repository or SHA")
        root = f"repos/{repo}"
        runs_path = f"{root}/actions/workflows/ci.yml/runs?head_sha={sha}&event=push&per_page=100"
        answer = check_release(
            repo, sha,
            lambda: {"workflow_runs": pages(runs_path, "workflow_runs")},
            lambda run: {"jobs": pages(f"{root}/actions/runs/{run['id']}/attempts/{run['run_attempt']}/jobs?per_page=100", "jobs")},
            lambda: api(f"{root}/git/ref/heads/master"),
            lambda master: api(f"{root}/compare/{sha}...{master}"),
        )
    except (KeyError, ValueError, TypeError, OSError, subprocess.SubprocessError):
        pass
    try:
        with open(os.environ["GITHUB_OUTPUT"], "a", encoding="utf-8") as output:
            output.write(f"tested={str(answer).lower()}\n")
    except (KeyError, OSError):
        pass
    return 0


if __name__ == "__main__":
    sys.exit(main())
