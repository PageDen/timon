# Timon on a Mac

Your code and your editing stay on the Mac. Timon runs there too: workers run in
worktrees of your local repositories. Only the model calls leave the Mac, through
an SSH tunnel to the broker on the workbench VM, which pays from the pooled
accounts and keeps their credentials. Nothing holding a pooled credential is ever
copied to the Mac.

```
Mac   timon run "goal" --execute   →  workers in your local repo
        model calls → 127.0.0.1:1456 ──ssh tunnel──▶ VM broker → acct2 / acct3
```

## One-time setup

**1. Tools.** Rust and the Codex CLI:

```sh
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
npm install -g @openai/codex          # or: brew install codex
codex login                           # your own login; see "Why log in" below
```

**2. Timon.**

```sh
cargo install --git https://github.com/PageDen/timon --locked timon
timon --version
```

**3. SSH to the workbench.** This must work without a password prompt:

```sh
ssh workbench@100.104.193.31 true && echo ok
```

**4. Config.**

```sh
timon config init
```

Then edit `~/.config/timon/config.toml` and uncomment:

```toml
strong_model = "gpt-5.6-luna"
cheap_model = "gpt-5.5"
budget_secs = 900
allow_planner = true
```

Leave `broker` alone: the default `127.0.0.1:1456` is where the tunnel arrives.

**5. Qualify this Mac's sandbox.** Free, no model call, about ten seconds:

```sh
timon qualify probe
```

A Mac sandboxes with Seatbelt, not what the VM uses, so the VM's result says
nothing about it. All nine checks should say `ok`. Until they do, workers run
read-only and Timon says so.

## Every session

Open the tunnel in its own terminal and leave it running:

```sh
ssh -N -L 1456:127.0.0.1:1456 workbench@100.104.193.31
```

Then, in another terminal:

```sh
timon status        # should end with "Ready"
```

## Using it

Exactly as on the VM, in any local repository:

```sh
cd ~/Projects/your-repo
timon run "your goal"               # routes it, spends nothing
timon run "your goal" --execute     # does it, on a branch
timon runs diff <id>
timon runs accept <id>
```

## Why log in, if the broker pays

The broker's upstream is the ChatGPT backend, which expects the request shape a
logged-in Codex sends. Your worker sends your own login; the broker strips it and
substitutes a pooled account's before anything leaves the VM. Your login never
pays for a hand-off and never reaches the provider through the broker.

## When something is wrong

| `timon status` says | Meaning |
|---|---|
| broker UNREACHABLE | the tunnel is not open, or ended |
| writing NOT permitted | run `timon qualify probe` and read what failed |
| 0 accounts usable | the VM broker has a problem; check it there |
