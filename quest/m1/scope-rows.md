# [S] Tell apart a setting's rows for different directories

## Goal

When one setting resolves differently across groups of a profile's working
directories, each of its rows in the default `token-max doctor` report says
which group it covers, so no two rows read the same.

## Plan

Found 2026-11-18 by the 0.0.1 validation run on fedora
([kixel-ai/token-max#169](https://github.com/kixel-ai/token-max/pull/169)):
the "Everything else" list printed `Goal check-ins unknown [~/.claude]`
three times and `Prompt suggestions unknown` and `Session recaps unknown`
twice each, same value, file and setting. `--details` tells them apart only
by their evidence, "In 20 directories, such as ~, ...", "In 53
directories, ...", "In 118 directories, ...".

Give each such row a short scope label in its tag, as T3 instances get
`[T3 claudeAgent]`, from what separates the groups: name the directory when
there is one, and otherwise the reason the group resolved apart, such as
"working directory changed" for the unresolved launch scope. Merging the
rows instead would hide a group whose value differs, so keep one row per
group. Cover the background checks (prompt suggestions, session recaps, goal
check-ins, Codex recaps) and any other check that splits by directory, with
a synthetic test of two groups.
