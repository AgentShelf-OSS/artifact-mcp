# Issue tracker: GitHub

Issues and specs for `AgentShelf-OSS/artifact-mcp` live as GitHub issues. Use the `gh` CLI with the `neilcorp2kx` account for all operations.

## Conventions

- **Create an issue**: `gh issue create --title "..." --body "..."`. Use a heredoc for multi-line bodies.
- **Read an issue**: `gh issue view <number> --comments`, filtering comments with `jq` and also fetching labels.
- **List issues**: `gh issue list --state open --json number,title,body,labels,comments --jq '[.[] | {number, title, body, labels: [.labels[].name], comments: [.comments[].body]}]'` with appropriate `--label` and `--state` filters.
- **Comment on an issue**: `gh issue comment <number> --body "..."`
- **Apply or remove labels**: `gh issue edit <number> --add-label "..."` or `--remove-label "..."`
- **Close an issue**: `gh issue close <number> --comment "..."`

Infer the repository from `git remote -v`; `gh` does this automatically when run inside this clone.

## Pull requests as a triage surface

**PRs as a request surface: no.** Set this to `yes` if the repository starts treating external pull requests as feature requests. The triage skill reads this flag.

When set to `yes`, pull requests use the same labels and states as issues through the `gh pr` commands:

- **Read a pull request**: `gh pr view <number> --comments` and `gh pr diff <number>`.
- **List external pull requests for triage**: `gh pr list --state open --json number,title,body,labels,author,authorAssociation,comments`, then keep only `CONTRIBUTOR`, `FIRST_TIME_CONTRIBUTOR`, or `NONE` author associations.
- **Comment, label, or close**: use `gh pr comment`, `gh pr edit --add-label` or `--remove-label`, and `gh pr close`.

GitHub shares one number space across issues and pull requests. Resolve a bare `#42` with `gh pr view 42`, then fall back to `gh issue view 42`.

## When a skill says "publish to the issue tracker"

Create a GitHub issue.

## When a skill says "fetch the relevant ticket"

Run `gh issue view <number> --comments`.

## Wayfinding operations

The Wayfinder map is a single issue with child issues as tickets.

- **Map**: create one issue labelled `wayfinder:map` that holds Notes, Decisions-so-far, and Fog. Use `gh issue create --label wayfinder:map`.
- **Child ticket**: link an issue to the map as a GitHub sub-issue through `gh api`. If sub-issues are unavailable, add the child to a task list in the map and put `Part of #<map>` at the top of the child body. Apply one `wayfinder:<type>` label from `research`, `prototype`, `grilling`, or `task`. Assign a claimed ticket to the driving developer.
- **Blocking**: use GitHub's native issue dependencies. Add an edge with `gh api --method POST repos/<owner>/<repo>/issues/<child>/dependencies/blocked_by -F issue_id=<blocker-db-id>`. The blocker database ID comes from `gh api repos/<owner>/<repo>/issues/<number> --jq .id`; it is not the issue number or `node_id`. If dependencies are unavailable, put `Blocked by: #<number>` at the top of the child body. A ticket is unblocked when every blocker is closed.
- **Frontier query**: list the map's open children, remove assigned tickets and those with an open blocker, then take the first ticket in map order.
- **Claim**: run `gh issue edit <number> --add-assignee @me`. This is the session's first write.
- **Resolve**: comment with the answer, close the child issue, then append a context pointer and link to the map's Decisions-so-far section.
