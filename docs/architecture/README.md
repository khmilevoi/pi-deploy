# Architecture

Diagrams explaining how rpi is built and how its processes flow, written for
a reader who does not read Rust. Maintained under the rules in the
`architecture-diagrams` skill (`.claude/skills/architecture-diagrams/SKILL.md`);
kept honest by the `architecture-audit` skill.

This index has no `Source anchors` section — unlike every other document
here, it describes docs, not code, so there is nothing to anchor.

## Reading order

Start with the three system documents, then dip into flows as needed.

1. [overview.md](overview.md) — what rpi is and how the CLI and the Pi-side agent split responsibilities over an SSH tunnel
2. [crates.md](crates.md) — how the codebase's four layers carry out a command such as `rpi deploy`
3. [storage.md](storage.md) — where the agent keeps its database, git checkouts, secrets vault, config, and logs on disk

## Process flows

| Flow | Answers |
|---|---|
| [deploy](flows/deploy.md) — the deploy pipeline's stages and its one-slot "latest wins" queue | what happens on `rpi deploy` |
| [connect](flows/connect.md) — picking a target, tunneling over SSH, and checking protocol compatibility | how the CLI reaches the Pi |
| [secrets](flows/secrets.md) — how a project's secrets travel from a developer's machine into a running container, including secret groups shared across environments | `rpi secrets push`/groups and injection |
| [ingress](flows/ingress.md) — routing a public hostname to a container through a Cloudflare Tunnel | publishing to the internet |
| [agent-setup](flows/agent-setup.md) — getting the `rpi` binary onto a board and turning it into a systemd service | bootstrapping a board |
| [agent-update](flows/agent-update.md) — bringing a board's agent up to a chosen version | `rpi upgrade` |
| [commands](flows/commands.md) — running a project's own declared script inside its running container | `rpi command <name>` |
| [observability](flows/observability.md) — where logs, stats, and doctor checks each get their data | logs / stats / doctor |
| [gc](flows/gc.md) — what cleanup removes, when it runs, and what it always leaves alone | what gets cleaned, when |
| [environments](flows/environments.md) — overlay resolution, deploy-time guards, `on_create`, `rpi env`, and the TTL reaper | test/preview environments, `rpi deploy --env` |
