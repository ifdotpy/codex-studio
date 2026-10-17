---
name: layr
description: Task recipes for layr, the local version control on btrfs. Use when you work in a layr line and need to save, inspect, recover, compare, merge, cache build output, or find out why layr refused a command.
---

# layr by task

`layr --help` and `layr <command> --help` list the commands. This skill tells which command
to use for a task.

A line is a folder: `<root>/projects/<project>/lines/<line>`. Run layr inside it.
The commands use the git verbs and print git formats. Use layr, not git, for local commits.
Git works in a line only for tools that read it.

## Save and inspect work

| Task                                   | Command                                               |
| -------------------------------------- | ----------------------------------------------------- |
| See what changed                       | `layr status`, `layr diff`                            |
| Commit                                 | `layr add -A && layr commit -m "<message>"`           |
| Save a checkpoint without a commit     | `layr save`                                           |
| Get the exact ID of the current commit | `layr rev-parse HEAD`                                 |
| Read a file of another commit or line  | `layr show <rev>:<path>`                              |
| Compare your line with another line    | `layr diff <other>...HEAD`                            |
| Search old content                     | `layr grep <pattern> <rev>`, `layr log --grep <text>` |

A revision is `HEAD`, `HEAD~2`, a line name, a tag, a state ID prefix, or `HEAD@{n}`.
A line name gives the last commit of that line, not its uncommitted files.

## Recover from a mistake

layr saves the working folder before every changing command, on `layr save`, and every
10 minutes when the content changed. These automatic states keep uncommitted work.

| Task                              | Command                                                |
| --------------------------------- | ------------------------------------------------------ |
| Undo your last layr operation     | `layr undo` (`layr undo -n` shows it first)            |
| Undo an older operation           | `layr op log`, then `layr undo <operation>`            |
| List automatic states             | `layr log --auto --format='%h %ar %s' -n 20`           |
| Check what a state holds          | `layr show --stat <state>`, `layr show <state>:<path>` |
| Get back a deleted or broken file | `layr restore --source <state> -- <path>`              |
| Make a new line from old work     | `layr branch <name> <state>`                           |

Use the state ID from the list. `HEAD@{n}` also names automatic states, newest first, but
each changing command adds a new one.
`layr undo` also undoes an undo.

## Check an exact state

`layr try <rev> -- <command>` runs the command in a temporary copy of that state.
Your folder does not change. layr records the result for that state.
Use it to test the exact state that you submit or review.

## Work in parallel

| Task                                      | Command                                                              |
| ----------------------------------------- | -------------------------------------------------------------------- |
| List lines                                | `layr branch`                                                        |
| Start a new line from your current commit | `layr branch <name>`, then `cd ../<name>`                            |
| Take the changes of another line          | `layr merge <line>`                                                  |
| Take one commit                           | `layr cherry-pick <rev>`                                             |
| Delete a line that you do not need        | `layr branch -d <line>` from another line (`-D` if it is not merged) |

A merge does not stop on a conflict. It writes conflict markers in text files.
`layr status` shows the conflicts as `UU`.
Edit the files, then run `layr add <paths> && layr commit -m "<message>"`.

## Keep build output fast

A cache is a folder of regenerable content, such as `target` or `node_modules`.
layr keeps it out of commits, diffs and backups. It stays on the same btrfs, so reflinks work.

1. Make sure that `.gitignore` or `.layrignore` ignores the folder.
2. Add `<folder> regenerable` to `.layrattributes` and commit it.
   Or run `layr cache add <folder>` (this needs the `config` permission).
3. Run `layr cache list`. The folder shows `cache`.

| Task                                             | Command              |
| ------------------------------------------------ | -------------------- |
| A tool deleted the cache and made a plain folder | `layr cache repair`  |
| Give new lines a warm copy of your caches        | `layr save --caches` |

A tool that refuses a cache inside the project can accept a folder that `layr cache list`
shows as `cache`.

## Find out why layr refused

| Message or task                      | Command                                                          |
| ------------------------------------ | ---------------------------------------------------------------- |
| `permission denied`                  | `layr access check <user> <action>`                              |
| `line '<x>' belongs to another user` | Only the owner changes a line. Work in your own line.            |
| `line '<x>' is protected`            | The line changes only with `layr merge <line> --expect <state>`. |
| Not in a line                        | `cd` into `<root>/projects/<project>/lines/<line>`               |

## Send work to the git remote

`layr push origin <branch>` exports each new commit of the line as a git commit.
`layr push --squash -m "<message>" origin <branch>` exports one commit.
Push only when the task allows it.
