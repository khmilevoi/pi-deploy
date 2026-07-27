# `rpi` plugin

Two portable skills that teach an AI coding agent this project's surface, so it stops
inventing `rpi` flags and `rpi.toml` keys:

| Skill | Covers |
| --- | --- |
| [`rpi-cli`](skills/rpi-cli/SKILL.md) | deploys, secrets and secret groups, environment overlays, logs and stats, agent setup, `rpi upgrade`, CLI-to-agent troubleshooting |
| [`rpi-toml`](skills/rpi-toml/SKILL.md) | schema 1 fields, `[project]`/`[source]`/`[build]`/`[ingress]`/`[healthcheck]`/`[secrets]`, Compose service and port mapping, `rpi.<env>.toml` overlays, configuration variables and the `RPI_*` runtime set |

## Claude Code

The repository root is a plugin marketplace (`.claude-plugin/marketplace.json`), so no checkout
is needed:

```bash
claude plugin marketplace add khmilevoi/rpi-deploy
claude plugin install rpi@rpi
```

`claude plugin update rpi@rpi` pulls later versions; `claude plugin details rpi@rpi` shows what
the plugin contributes. Do **not** also copy the skills into `~/.claude/skills` with the install
script below — the plugin and a manual copy would shadow each other.

## OpenAI Codex

Codex reads skills from `~/.codex/skills` (or `$CODEX_HOME/skills`) rather than from a
marketplace, so install from a checkout:

```sh
sh scripts/install-skills.sh codex
```

```powershell
powershell -File scripts\install-skills.ps1 -Target codex
```

Both scripts enumerate `skills/` dynamically and replace whatever is already installed, so
re-running one after a `git pull` is the refresh path.

## Layout

```
plugins/rpi/
├── .claude-plugin/plugin.json   # Claude Code manifest
├── .codex-plugin/plugin.json    # Codex manifest
├── scripts/install-skills.{sh,ps1}
└── skills/
    ├── rpi-cli/{SKILL.md,agents/openai.yaml}
    └── rpi-toml/{SKILL.md,agents/openai.yaml}
```

Bump `version` in **both** manifests and in the matching entry of the root
`.claude-plugin/marketplace.json` when a skill changes — Claude Code keys plugin updates off the
marketplace entry's version.
