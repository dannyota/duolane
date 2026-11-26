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


def successful_ci(run, jobs):
    if not isinstance(jobs, dict) or not isinstance(jobs.get("jobs"), list):
        return False
    entries = jobs["jobs"]
    if "total_count" in jobs and (type(jobs["total_count"]) is not int or jobs["total_count"] != len(entries)):
        return False
    if any(not isinstance(job, dict) or not isinstance(job.get("name"), str) for job in entries):
        return False
    ci = [job for job in entries if job.get("name") == "ci"]
    return (len(ci) == 1 and ci[0].get("status") == "completed"
            and ci[0].get("conclusion") == "success" and valid_id(ci[0].get("run_id"))
            and ci[0]["run_id"] == run["id"] and valid_id(ci[0].get("run_attempt"))
            and ci[0]["run_attempt"] == run["run_attempt"])


def master_sha(ref):
    try:
        obj = ref["object"]
        sha = obj["sha"]
        return sha if ref["ref"] == "refs/heads/master" and obj["type"] == "commit" and valid_sha(sha) else None
    except (KeyError, TypeError):
        return None


def valid_compare(sha, compare):
    try:
        return (isinstance(compare, dict) and compare.get("status") in ("ahead", "identical")
                and type(compare.get("behind_by")) is int and compare["behind_by"] == 0
                and compare["merge_base_commit"]["sha"] == sha)
    except (KeyError, TypeError):
        return False


def release_proof(repo, sha, run, jobs, ref, compare):
    return (matching_run(repo, sha, run) and run.get("status") == "completed"
            and run.get("conclusion") == "success" and successful_ci(run, jobs)
            and master_sha(ref) is not None and valid_compare(sha, compare))


def wait_for_release(repo, sha, fetch_runs, fetch_jobs, fetch_run, clock=time.monotonic, sleep=time.sleep, deadline=None):
    deadline = min(clock() + 300, deadline if deadline is not None else float("inf"))
    while True:
        if clock() >= deadline:
            return False
        response = fetch_runs()
        if not isinstance(response, dict) or not isinstance(response.get("workflow_runs"), list):
            return False
        runs = response["workflow_runs"]
        if any(not isinstance(run, dict) for run in runs):
            return False
        for run in runs:
            if clock() >= deadline:
                return False
            if (matching_run(repo, sha, run) and run.get("status") == "completed"
                    and run.get("conclusion") == "success" and successful_ci(run, fetch_jobs(run))):
                latest = fetch_run(run)
                if (matching_run(repo, sha, latest) and latest["id"] == run["id"]
                        and latest["run_attempt"] == run["run_attempt"]
                        and latest.get("status") == "completed" and latest.get("conclusion") == "success"):
                    return clock() < deadline
        pending = any(matching_run(repo, sha, run) and run.get("status") in ("queued", "in_progress") for run in runs)
        remaining = deadline - clock()
        if not pending or remaining <= 0:
            return False
        sleep(min(15, remaining))


def check_release(repo, sha, fetch_runs, fetch_jobs, fetch_run, fetch_ref, fetch_compare, deadline=None):
    clock = time.monotonic
    if deadline is None:
        deadline = clock() + 300
    def call(fetch, *args):
        if clock() >= deadline:
            raise TimeoutError("proof deadline reached")
        result = fetch(*args)
        if clock() >= deadline:
            raise TimeoutError("proof deadline reached")
        return result
    try:
        if not wait_for_release(repo, sha, lambda: call(fetch_runs), lambda run: call(fetch_jobs, run), lambda run: call(fetch_run, run), clock=clock, deadline=deadline):
            return False
        master = master_sha(call(fetch_ref))
        if not master:
            return False
        return valid_compare(sha, call(fetch_compare, master))
    except (KeyError, ValueError, TypeError, OSError, subprocess.SubprocessError):
        return False


def command(args, deadline=None):
    remaining = 30 if deadline is None else min(30, deadline - time.monotonic())
    if remaining <= 0:
        raise TimeoutError("proof deadline reached")
    return subprocess.run(args, check=True, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, timeout=remaining).stdout.decode("utf-8")


def api(path, deadline=None):
    return json.loads(command(["gh", "api", path], deadline))


def pages(path, key, deadline=None):
    items = []
    totals = []
    page_number = 1
    while True:
        separator = "&" if "?" in path else "?"
        page = api(f"{path}{separator}page={page_number}", deadline)
        entries = page[key]
        if not isinstance(entries, list) or len(entries) > 100:
            raise ValueError("invalid page")
        if key == "jobs":
            total = page.get("total_count")
            if type(total) is not int or total < 0:
                raise ValueError("invalid total count")
            totals.append(total)
        items.extend(entries)
        if len(entries) < 100:
            break
        page_number += 1
    if totals and any(total != len(items) for total in totals):
        raise ValueError("incomplete job pages")
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
        deadline = time.monotonic() + 300
        answer = check_release(
            repo, sha,
            lambda: {"workflow_runs": pages(runs_path, "workflow_runs", deadline)},
            lambda run: {"jobs": pages(f"{root}/actions/runs/{run['id']}/attempts/{run['run_attempt']}/jobs?per_page=100", "jobs", deadline)},
            lambda run: api(f"{root}/actions/runs/{run['id']}", deadline),
            lambda: api(f"{root}/git/ref/heads/master", deadline),
            lambda master: api(f"{root}/compare/{sha}...{master}", deadline),
            deadline=deadline,
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
