# Running on a Linux VM, controlled from the Mac

## Topology

```
 Mac                                   Linux VM (always on)
 ┌──────────────────────────┐          ┌──────────────────────────────┐
 │ Speedcraft.app           │          │ systemd: speedcraft-agent    │
 │  speedcraft ui :7700     │  HTTP +  │  speedcraft agent :7777      │
 │  ├─ local engine         │  bearer  │  └─ engine (paper or live)   │
 │  └─ remote agent proxy ──┼─token───▶│     config /etc/speedcraft   │
 │ browser → 127.0.0.1:7700 │          │     ledger /var/lib/speedcraft│
 └──────────────────────────┘          └──────────────────────────────┘
```

The browser only ever talks to the Mac app on localhost. The Mac app holds
each agent's token (in `agents.json`, mode 0600) and forwards commands. The
same binary runs on both sides; `ui` and `agent` are just two subcommands.

## Install the agent

On the VM (Ubuntu/Debian/Fedora with systemd):

```sh
curl -fsSL https://raw.githubusercontent.com/cjsmitty/speedcraft/main/install/install-agent.sh | bash
```

What it does: installs Rust if needed, builds the release binary, creates a
`speedcraft` system user, writes `/etc/speedcraft/{config.toml,agent.env,agent.token}`,
installs and starts a hardened systemd unit that restarts on failure and on
reboot, and prints the token and the URL to add in the Mac app. The agent
autostarts in **paper** mode. Re-running the script upgrades the binary and
keeps your config.

Useful commands on the VM:

```sh
sudo systemctl status speedcraft-agent
sudo journalctl -u speedcraft-agent -f
sudo nano /etc/speedcraft/config.toml && sudo systemctl restart speedcraft-agent
sudo cat /etc/speedcraft/agent.token
```

## Reach the agent from the Mac

The agent binds to `127.0.0.1:7777` by default, so nothing is exposed. Pick one:

- **SSH tunnel** (simplest, no extra software):
  `ssh -N -L 7777:127.0.0.1:7777 user@vm` in a Terminal tab, then add the
  agent in the app as `http://127.0.0.1:7777`. Use a different local port for
  each VM (`-L 7778:127.0.0.1:7777`).
- **Tailscale or a private network**: install with
  `SPEEDCRAFT_LISTEN=0.0.0.0:7777` and add the agent as `http://<vm-ip>:7777`.
  The token is mandatory when not bound to localhost; the agent refuses to
  start without one.

Never port-forward an agent to the public internet. The token is strong but
the API has start-live and config-write endpoints.

## Live mode on the agent

Three gates, all required, same as the CLI:

1. `live = true` in `/etc/speedcraft/config.toml`
2. The loss acknowledgement: tick it in the Mac app's Settings → Live trading
   while the agent's tab is selected (it is saved on the agent), or set
   `SPEEDCRAFT_I_ACCEPT_LOSSES=yes` in `/etc/speedcraft/agent.env`. Exchange
   keys likewise: Settings → Exchanges & keys on the agent's tab, or the env file.
3. Typing `I ACCEPT LOSSES` in the Mac app's live dialog (or
   `SPEEDCRAFT_AUTOSTART=live` at install time, which counts as the explicit confirmation
   for unattended restarts)

The live dialog shows which gates are satisfied before you type.

## The Mac app

```sh
./install/make-mac-app.sh     # produces dist/Speedcraft.app
```

Double-click it. It runs in the background and the UI opens in your default
browser at `http://127.0.0.1:7700`. Data (config, env, ledger, agents) lives
in `~/Library/Application Support/Speedcraft`; logs in
`~/Library/Logs/Speedcraft`. The panel's Quit button stops the local engine
cleanly and exits.

Without the bundle: `speedcraft ui` does the same thing from any directory
holding a `config.toml`.

## API (for scripting)

All under `/api`, JSON, `Authorization: Bearer <token>` when a token is set.

| Method | Path | Purpose |
|---|---|---|
| GET | `/status` | agent, engine state, config summary, live snapshot (books, edges, balances, PnL) |
| GET | `/events?since=N&limit=M` | ledger events after sequence N |
| POST | `/engine/start` | `{"mode":"paper"}` or `{"mode":"live","confirm":"I ACCEPT LOSSES"}` |
| POST | `/engine/stop` | graceful stop |
| POST | `/engine/halt` | `{"reason":"..."}` stop sending orders, keep watching |
| POST | `/engine/resume` | clear a halt |
| POST | `/stopfile` | `{"present":true}` create/remove the STOP file |
| GET / PUT | `/config` | read / validate-and-write `config.toml` |
| GET / PUT | `/settings` | structured config plus masked secrets; PUT takes `{"config": {...patch}, "secrets": {...}}` |
| GET / PUT | `/coins` | catalog and selected coins; PUT `{"bases": ["BTC","SOL"]}` rewrites the pairs |
| POST | `/exchanges/{id}/test` | read fees, balances and market rules with the saved keys; never trades |
| POST | `/quit` | UI host only: stop the engine and exit the process |
| POST | `/record` | `{"on":true}` start saving every quote the running engine sees to `quotes-<date>.jsonl`; `{"on":false}` stop |
| POST | `/scan` | `{"file":"..."}` optional; ranks the newest recording at your fee floors plus volume and zero tiers |
| GET / POST / DELETE | `/agents`, `/agents/{id}` | UI host only: remote agent registry |
| ANY | `/agents/{id}/...` | UI host only: proxied to that agent |

Example from the Mac to a VM through a tunnel:

```sh
curl -s -H "Authorization: Bearer $(cat token)" http://127.0.0.1:7777/api/status | jq .engine
curl -s -X POST -H "Authorization: Bearer $TOKEN" -H 'content-type: application/json' \
     -d '{"reason":"market looks weird"}' http://127.0.0.1:7777/api/engine/halt
```
