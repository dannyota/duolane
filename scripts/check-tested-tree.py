import json
import os
import re
import subprocess
import sys
import time

PATHS = (".github", "scripts", "Makefile", ".tool-versions", "rust-toolchain.toml", ".config")
REQUIRED_JOBS = {"gates", "rust", "deny", "gitleaks"}


def valid_sha(value):
    return isinstance(value, str) and re.fullmatch(r"[0-9a-f]{40}", value) is not None


def valid_id(value):
    return isinstance(value, int) and not isinstance(value, bool) and value > 0


def pull_head(repo, sha, parents, pulls):
    try:
        if not valid_sha(sha) or not isinstance(parents, list) or len(parents) != 2 or not all(map(valid_sha, parents)):
            return None
        if not isinstance(pulls, list) or len(pulls) != 1:
            return None
        pr = pulls[0]
        head = pr["head"]["sha"]
        if (not valid_id(pr["number"]) or not isinstance(pr["merged_at"], str) or not pr["merged_at"]
                or pr["merge_commit_sha"] != sha or pr["base"]["ref"] != "master"
                or pr["base"]["repo"]["full_name"] != repo or head != parents[1]):
            return None
        return head
    except (KeyError, TypeError):
        return None


def matching_pr_run(repo, head, number, run):
    try:
        return (isinstance(run, dict) and valid_id(run.get("id")) and run.get("status") == "completed"
                and run.get("conclusion") == "success" and run.get("head_sha") == head
                and run.get("event") == "pull_request" and run.get("repository", {}).get("full_name") == repo
                and isinstance(run.get("pull_requests"), list)
                and any(isinstance(pr, dict) and pr.get("number") == number
                        and isinstance(pr.get("base"), dict) and pr["base"].get("ref") == "master"
                        for pr in run["pull_requests"]))
    except (TypeError, AttributeError):
        return False


def successful_jobs(jobs):
    try:
        entries = jobs["jobs"]
        if not isinstance(entries, list):
            return False
        selected = [job for job in entries if isinstance(job, dict) and job.get("name") in REQUIRED_JOBS]
        return (len(selected) == len(REQUIRED_JOBS)
                and {job["name"] for job in selected} == REQUIRED_JOBS
                and all(job.get("conclusion") == "success" and valid_id(job.get("id")) for job in selected))
    except (KeyError, TypeError):
        return False


def push_proof(repo, sha, parents, before, after, pulls, run, jobs, compare, head_tree, pushed_tree, timeline):
    try:
        head = pull_head(repo, sha, parents, pulls)
        if not head or not isinstance(before, dict) or not isinstance(after, dict):
            return False
        if set(before) != set(PATHS) or set(after) != set(PATHS) or before != after:
            return False
        if any(value is not None and not valid_sha(value) for value in before.values()):
            return False
        if not matching_pr_run(repo, head, pulls[0]["number"], run) or not successful_jobs(jobs):
            return False
        if not isinstance(compare, dict) or compare.get("status") not in ("ahead", "identical") or type(compare.get("behind_by")) is not int or compare["behind_by"] != 0:
            return False
        if not isinstance(timeline, list) or any(not isinstance(event, dict) or event.get("event") == "base_ref_changed" for event in timeline):
            return False
        return valid_sha(head_tree) and head_tree == pushed_tree
    except (KeyError, TypeError):
        return False


def runs_list(value):
    if not isinstance(value, dict) or not isinstance(value.get("workflow_runs"), list):
        return []
    return value["workflow_runs"]


def matching_push(repo, sha, run):
    try:
        return (isinstance(run, dict) and run.get("head_sha") == sha
                and run.get("event") == "push" and run.get("head_branch") == "master"
                and run.get("repository", {}).get("full_name") == repo)
    except AttributeError:
        return False


def on_master(compare):
    return isinstance(compare, dict) and compare.get("status") in ("identical", "ahead")


def release_proof(repo, sha, runs, compare):
    return valid_sha(sha) and on_master(compare) and any(matching_push(repo, sha, run) and run.get("status") == "completed" and run.get("conclusion") == "success" for run in runs_list(runs))


def wait_for_release(repo, sha, compare, fetch, clock=time.monotonic, sleep=time.sleep):
    if not on_master(compare):
        return False
    deadline = clock() + 300
    while True:
        runs = fetch()
        if release_proof(repo, sha, runs, compare):
            return True
        pending = any(matching_push(repo, sha, run) and run.get("status") in ("queued", "in_progress") for run in runs_list(runs))
        remaining = deadline - clock()
        if not pending or remaining <= 0:
            return False
        sleep(min(15, remaining))


def command(args):
    return subprocess.run(args, check=True, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, timeout=30).stdout.decode("utf-8")


def api(path):
    return json.loads(command(["gh", "api", path]))


def pages(path, key=None):
    result = json.loads(command(["gh", "api", "--paginate", "--slurp", path]))
    if not isinstance(result, list) or not result:
        raise ValueError("invalid pages")
    items = []
    for page in result:
        entries = page if key is None else page[key]
        if not isinstance(entries, list):
            raise ValueError("invalid page")
        items.extend(entries)
    return items


def git(*args):
    return command(["git", *args]).strip()


def objects(commit):
    result = {}
    for path in PATHS:
        entry = git("ls-tree", "-z", commit, "--", path)
        if not entry:
            result[path] = None
        else:
            metadata, name = entry.rstrip("\0").split("\t", 1)
            mode, kind, oid = metadata.split(" ")
            if name != path or mode not in ("100644", "100755", "040000", "120000", "160000") or kind not in ("blob", "tree", "commit") or not valid_sha(oid):
                raise ValueError("invalid tree entry")
            result[path] = oid
    return result


def main():
    if len(sys.argv) != 2 or sys.argv[1] not in ("push", "release"):
        return 2
    mode = sys.argv[1]
    answer = False
    try:
        repo = os.environ["GITHUB_REPOSITORY"]
        sha = os.environ["GITHUB_SHA"]
        if not re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", repo) or not valid_sha(sha):
            raise ValueError("invalid repository or SHA")
        root = f"repos/{repo}"
        if mode == "push":
            parents = git("show", "-s", "--format=%P", sha).split()
            pulls = pages(f"{root}/commits/{sha}/pulls?per_page=100")
            head = pull_head(repo, sha, parents, pulls)
            if head:
                timeline = pages(f"{root}/issues/{pulls[0]['number']}/timeline?per_page=100")
                before, after = objects(parents[0]), objects(sha)
                compare = api(f"{root}/compare/{parents[0]}...{head}")
                head_tree = api(f"{root}/git/commits/{head}")["tree"]["sha"]
                pushed_tree = git("rev-parse", f"{sha}^{{tree}}")
                runs = pages(f"{root}/actions/workflows/ci.yml/runs?head_sha={head}&event=pull_request&per_page=100", "workflow_runs")
                for run in runs:
                    if not matching_pr_run(repo, head, pulls[0]["number"], run):
                        continue
                    jobs = {"jobs": pages(f"{root}/actions/runs/{run['id']}/jobs?per_page=100", "jobs")}
                    if push_proof(repo, sha, parents, before, after, pulls, run, jobs, compare, head_tree, pushed_tree, timeline):
                        answer = True
                        break
        else:
            compare = api(f"{root}/compare/{sha}...master")
            path = f"{root}/actions/workflows/ci.yml/runs?head_sha={sha}&event=push&branch=master&per_page=100"
            answer = wait_for_release(repo, sha, compare, lambda: {"workflow_runs": pages(path, "workflow_runs")})
    except (KeyError, ValueError, TypeError, OSError, subprocess.SubprocessError):
        pass
    try:
        with open(os.environ["GITHUB_OUTPUT"], "a", encoding="utf-8") as output:
            output.write(f"{'skip' if mode == 'push' else 'tested'}={str(answer).lower()}\n")
    except (KeyError, OSError):
        pass
    return 0


if __name__ == "__main__":
    sys.exit(main())
