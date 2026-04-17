# Skills

Project-specific skills for coding agents. Each subdirectory is a
self-contained skill following the [skill-creator](https://github.com/anthropics/skills)
convention: a `SKILL.md` entry point with optional `references/`,
`scripts/`, and `assets/` siblings.

These live at a tool-agnostic path so the repo isn't coupled to any
particular agent's discovery rules.

## Available skills

- **`testing-reef/`** — Conventions, fixtures, and gotchas for writing tests in this workspace. Auto-triggers on test-related prompts.

## Installing for your agent

Skills are consumed from a tool-specific path. Install by symlinking (so
updates to the source propagate automatically) or copying.

### Claude Code (project-scoped)

```bash
mkdir -p .claude/skills
for skill in skills/*/; do
    name=$(basename "$skill")
    ln -sfn "../../skills/$name" ".claude/skills/$name"
done
```

Project `.claude/` is in `.gitignore`, so the symlink itself is local to
your working copy. Skills get auto-discovered by Claude Code when cwd is
inside this repo.

### Claude Code (user-global)

Same idea, but link into `~/.claude/skills/` instead — makes the skill
available in every Claude Code session, not just this project:

```bash
for skill in skills/*/; do
    name=$(basename "$skill")
    ln -sfn "$(pwd)/skills/$name" "$HOME/.claude/skills/$name"
done
```

### Other agents (Cursor, Aider, Continue, …)

Most coding agents now read the same Skills format. Point them at
`skills/` directly, or symlink/copy into their expected path — consult
your agent's docs for where it discovers skills from.

## Contributing a new skill

1. Create `skills/<new-skill>/SKILL.md` with YAML frontmatter (`name` + `description`)
2. Keep `SKILL.md` under ~500 lines; push details into `references/*.md`
3. Write the description "pushy" — list the specific user phrases that should trigger it
4. Update the list above in this README
