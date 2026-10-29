// Keeps a worktree's branch current with local main. Arm it in each worktree
// session. See CONTRIBUTING.md, "Keeping a worktree current".
let meta = #{
    name: "sync-main",
    description: "Keep this worktree current with a local base branch at the next safe point",
    triggers: [
        #{ kind: "schedule", every: "15m" },
        #{ kind: "armed" },
        #{ kind: "idle" },
    ],
    limits: #{ max_per_hour: 6 },
    args: #{
        base: #{ type: "string", default_value: "main", description: "Local branch to merge into this worktree" },
        check: #{ type: "string", default_value: "make check", description: "Verification command after the merge" },
        brief_over: #{ type: "int", default_value: 5, min: 0, max: 10000, description: "Delegate reading when more than this many commits arrived" },
        notice_hours: #{ type: "int", default_value: 12, min: 1, max: 168, description: "Hours to wait for a safe point before notifying once" },
    },
};

let comparison = ();
try {
    comparison = git_compare(args.base);
} catch (err) {
    if err.kind in ["timeout", "unavailable", "invalid_argument"] {
        skip("git comparison failed: " + err.kind);
    }
    throw err;
}
if comparison == () { skip("no repository or local base branch"); }
if comparison.branch == () { skip("HEAD is detached"); }
if comparison.branch == args.base { skip("this checkout has the base checked out"); }
if comparison.operation != () { skip("a git " + comparison.operation + " is in progress"); }

let base_head = comparison.base_head;
let t = now().unix;
let hour = 3600;
if event.trigger == "idle" && state.sync_fire != () && event.started_by.fire_id == state.sync_fire {
    state.sync_fire = ();
    if event.outcome != "completed" && event.error_kind in ["rate_limit", "overloaded", "timeout", "network"] {
        skip("the sync turn hit a passing error, the next check asks again");
    }
    let reply = event.last_response;
    reply.trim();
    if last_line(reply, "SYNC-MAIN: BLOCKED") {
        if state.blocked == base_head { skip("blocked on this base commit"); }
        state.blocked = base_head;
        notify("sync-main is blocked on " + args.base + " at " + base_head + ". The transcript says what a person must decide. Arm sync-main again after resolving it.");
        return;
    }
    if last_line(reply, "SYNC-MAIN: WAIT") {
        if state.waiting_head != base_head {
            state.waiting_head = base_head;
            state.waiting_since = t;
        }
    }
}
if comparison.behind == 0 {
    state.clear();
    skip("current with " + args.base);
}
if event.trigger == "armed" && event.reason == "manual" && state.blocked != () {
    state.blocked = ();
    state.asked = ();
    state.nudged = ();
}
if state.blocked == base_head { skip("blocked on this base commit"); }
if state.waiting_head == base_head && state.waited_notice != base_head && t - state.waiting_since >= args.notice_hours * hour {
    notify("sync-main has waited " + args.notice_hours + " hours for a safe point to sync with " + args.base + " at " + base_head + ". Check the session's current work.");
    state.waited_notice = base_head;
}

let policy = sync_policy(args);
if event.session.status == "idle" {
    if state.asked == base_head && t - state.asked_at < hour { skip("already asked for this base commit within the last hour"); }
    message("Sync this worktree with " + args.base + ": " + comparison.behind + " commits arrived (now " + base_head + ").\n" + policy);
    state.asked = base_head;
    state.asked_at = t;
    state.sync_fire = event.fire_id;
} else {
    if state.nudged == base_head { skip("already added a sync todo for this base commit"); }
    message(args.base + " gained " + comparison.behind + " commits (now " + base_head + "). Add the todo `Sync with " + args.base + "` right after your current item, and run it at the next safe point under the sync policy below. Do not stop running work for it.\n" + policy, #{ delivery: "guide" });
    state.nudged = base_head;
}

fn last_line(reply, marker) {
    reply == marker || reply.ends_with("\n" + marker)
}

fn sync_policy(a) {
    "Sync policy:\n"
        + "1. Work only at a safe point. No background job, subagent, or workflow you started is running, and you are not mid-edit. Otherwise add the todo `Sync with " + a.base + "` after the current item and end with SYNC-MAIN: WAIT.\n"
        + "2. Read what arrived. Run `git log --reverse --format=fuller HEAD.." + a.base + "` with full messages, read `git diff HEAD..." + a.base + " -- release-notes/`, and list the files both sides changed. When more than " + a.brief_over + " commits arrived or both sides changed the same files, delegate the reading to one read-only subagent. It returns each relevant change's motivation, its release-notes line, and what it implements that this branch also implements, works around, or depends on, with file:line pointers.\n"
        + "3. Commit this branch's finished, verified work first. Leave unfinished work uncommitted only when `git merge-tree --write-tree HEAD " + a.base + "` reports no conflicts and no file that " + a.base + " changed is dirty here. Otherwise finish the unit first and end with SYNC-MAIN: WAIT.\n"
        + "4. Run `git merge " + a.base + "`. Never rebase, push, or touch the main checkout. Resolve conflicts with both intents kept. Where " + a.base + " now implements what this branch implements or works around, adopt " + a.base + "'s version and drop the duplicate when it fits in the merge, and add a todo when it does not. Move release-notes bullets to the next version's file when `git tag -l v<version>` shows their version shipped. After a Cargo.toml or Cargo.lock conflict, run `cargo hakari generate` and `cargo hakari manage-deps`.\n"
        + "5. Run `" + a.check + "` and fix what the merge broke.\n"
        + "6. Reply with what arrived, what you adopted, the todos you added, and the conflicts you resolved. Explain any wait or what a person must decide before the marker. End with exactly one marker on the last line, in plain text without backticks or bold: SYNC-MAIN: DONE, SYNC-MAIN: WAIT, or SYNC-MAIN: BLOCKED."
}
