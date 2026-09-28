# Deploying the broker

    sudo install -m 0755 target/release/timon /usr/local/bin/timon
    sudo install -m 0644 deploy/timon-broker.service /etc/systemd/system/
    sudo systemctl daemon-reload
    sudo systemctl enable --now timon-broker

Check it:

    systemctl status timon-broker
    curl -s http://127.0.0.1:1456/_timon/health | jq

`/usr/local/bin/timon` rather than the build directory on purpose: a `cargo
build` in the working tree must not change what the service restarts into.
Upgrading is the `install` line above, then `sudo systemctl restart
timon-broker`.

## What the supervision actually covers

| Failure | Caught by |
|---|---|
| Reboot, OOM kill, crash | `Restart=always` |
| Poisoned account lock — process alive, port open, every request 500 | `WatchdogSec`, because the broker stops checking in when a health check fails |
| Store missing at boot | `ConditionPathIsDirectory`, so it refuses to start rather than refusing every request |
| Credentials unreadable, no account usable | Health check reports unhealthy; the watchdog restarts once, and a persistent fault then shows in `systemctl status` rather than looping forever |

Not covered, and deliberately: a provider outage. The broker is healthy and
reports it; there is nowhere else to go, which is why the no-fallback decision
stands on its own.
