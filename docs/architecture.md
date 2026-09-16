# Architecture

## The one design decision

The gateway is a TCP proxy that reads the first packet. Everything else follows
from that.

It parses the handshake because it has to — that is where the hostname lives. It
rewrites two lines of a MOTD because that is the one thing a network wants to
say for itself. It binds a transparent socket because the backend needs the
client's IP. And then it stops, and moves bytes.

It does **not** decompress, decrypt, authenticate, or understand play-state
packets, and it does not build a status response of its own. Not because those
are hard, but because doing any of them turns "add a 1.22 backend" into a
protocol update, and "add NeoForge" into a compatibility project. The parts of
the protocol this gateway knows are the parts that have not changed since 1.7.

## Layers

```text
                        MC-GATEWAY
                            │
        ┌───────────────────┼───────────────────┐
        ▼                   ▼                   ▼
    transport           protocol             routing
   (listener,          (handshake,         (matcher,
    session,            status,             registry,
    pipe)               chat)               health)
        │                   │                   │
        └───────────────────┼───────────────────┘
                            ▼
                       forwarding
                            │
                 ┌──────────┴──────────┐
                 ▼                     ▼
            TPROXY (Linux)         plain TCP
```

Each is a crate, and the dependencies only point one way:

| crate | knows about | deliberately does not know about |
|---|---|---|
| `mc-protocol` | bytes | config, sockets, routing |
| `mc-config` | the file format | the network |
| `mc-routing` | backends, health | clients, sessions, metrics |
| `mc-forwarding` | sockets | Minecraft |
| `mc-gateway` | all of them | — |

`mc-forwarding` not knowing Minecraft is what keeps forwarding honest: it can
only use what it was told by `accept()`, because it has no access to anything a
client claimed.

`mc-routing` not knowing about metrics is why registry gauges are sampled on a
timer by the gateway rather than pushed from the health checker.

## A connection, start to finish

```text
accept()
   │
   ├─ 1. inbound PROXY header      only from listeners.proxy_protocol.trusted
   │
   ├─ 2. admission control         global cap, per-IP cap, token bucket
   │                               (before a single byte is parsed)
   │
   ├─ 3. first bytes
   │      ├─ 0xFE ─────────────▶  pre-1.7 ping ──▶ forwarded to the default target
   │      └─ VarInt frame ─────▶  handshake
   │
   ├─ 4. hostname ──▶ matcher ──▶ target ──▶ selection policy ──▶ backend
   │
   ├─ 5. connect, replay every byte read so far, verbatim
   │
   ├─ 6a. next_state = status
   │        ├─ MOTD lines configured ──▶ intercept the status response,
   │        │                            replace those lines, forward it
   │        └─ nothing configured    ──▶ forward untouched
   │
   └─ 6b. next_state = login / transfer
            └─ pipe until either side closes
```

Step 5's replay is worth stating explicitly: the handshake is **not** consumed
by the gateway. The backend receives the client's original bytes, including any
`\0FML2\0` marker, so it sees exactly what a direct connection would have
delivered.

There is exactly one response the gateway invents: when a route has no reachable
backend, there is nothing to pass through, so it answers with the configured
offline MOTD. Everything else a client ever sees came from a real server.

## Rewriting a MOTD

A status response's `description` is a chat component: an arbitrarily nested
tree of styled spans. "The second line" has no meaning inside that structure, so
the component is flattened into a legacy `§`-coded string first — a shape that
splits on `\n` and that every client back to 1.7 understands:

```text
{"extra":[{"text":"A Minecraft Server","color":"white"},
          {"text":"\n"},
          {"text":"powered by Paper","color":"gray"}]}
                            │
                            ▼  flatten
        "§r§fA Minecraft Server\n§r§7powered by Paper"
                            │
                            ▼  replace line 2
        "§r§fA Minecraft Server\n§7survival • creative"
                            │
                            ▼
              {"text":"§r§fA Minecraft Server\n§7survival • creative"}
```

Only the `description` field is touched. `version`, `players`, `sample`,
`favicon` and anything a future protocol version adds are re-serialised exactly
as they arrived. A response that cannot be parsed is forwarded unchanged — the
client may well understand something this gateway does not.

## State and reloads

All mutable state lives behind one atomic pointer:

```text
      ArcSwap<Runtime>
             │
          Registry
             │
  ┌──────────┴──────────┐
  ▼                     ▼
backends             targets
(health, active     (members,
 sessions, last      policy,
 status ping)        round-robin cursor)
```

A session takes a snapshot when it starts and keeps it until it ends. A reload
builds a whole new `Runtime` and swaps it in, so:

- in-flight sessions are never re-routed mid-connection
- health state and session counts carry over for servers whose address is
  unchanged (a moved backend starts from a clean slate — it is a different
  server now)
- a config that fails to parse or validate leaves the running one untouched

Listener definitions are the exception: rebinding sockets would drop
connections, so changes there are reported and require a restart.

## Where the limits sit

Admission control runs before parsing, which is the only place where an abusive
connection still costs nothing:

```text
accept ──▶ [global cap] ──▶ [per-IP cap] ──▶ [token bucket] ──▶ parse
```

The per-IP concurrency cap is checked before the bucket and deliberately does
not charge it: a client sitting at its connection limit is not the same thing as
a client connecting too fast, and mixing the two makes both counters unreadable.

Everything afterwards is bounded too: a handshake byte budget, a frame size cap
well below the protocol's own, timeouts on the handshake, the status exchange,
the backend connect and the idle session, and a per-backend session cap. Nothing
a client sends can make the gateway allocate without limit or wait forever.
