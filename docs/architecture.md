# Architecture

## The one design decision

The gateway is a TCP proxy that reads the first packet. Everything else follows
from that.

It parses the handshake because it has to — that is where the hostname lives. It
answers status pings because a central MOTD is the main reason to run an edge
gateway at all. It writes a PROXY header or binds a transparent socket because
the backend needs the client's IP. And then it stops, and moves bytes.

It does **not** decompress, decrypt, authenticate, or understand play-state
packets. Not because those are hard, but because doing any of them turns "add a
1.22 backend" into a protocol update, and "add NeoForge" into a compatibility
project. The parts of the protocol this gateway knows are the parts that have
not changed since 1.7.

## Layers

```text
                        MC-GATEWAY
                            │
        ┌───────────────────┼───────────────────┐
        ▼                   ▼                   ▼
    transport           protocol             routing
   (listener,          (handshake,         (matcher,
    session,            status,             registry,
    pipe)               legacy)             health)
        │                   │                   │
        └───────────────────┼───────────────────┘
                            ▼
                       forwarding
                            │
             ┌──────────────┼──────────────┐
             ▼              ▼              ▼
          TPROXY        PROXY v2        plain TCP
```

Each is a crate, and the dependencies only point one way:

| crate | knows about | deliberately does not know about |
|---|---|---|
| `mc-protocol` | bytes | config, sockets, routing |
| `mc-config` | the file format | the network |
| `mc-routing` | backends, health | clients, sessions |
| `mc-forwarding` | sockets, headers | Minecraft |
| `mc-metrics` | counters | everything else |
| `mc-gateway` | all of them | — |

`mc-forwarding` not knowing Minecraft is what keeps the forwarding modes honest:
it can only forward what it was told by `accept()`, because it has no access to
anything a client claimed.

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
   │      ├─ 0xFE ─────────────▶  legacy ping  ──▶ reply, close
   │      └─ VarInt frame ─────▶  handshake
   │
   ├─ 4. hostname ──▶ matcher ──▶ target ──▶ selection policy ──▶ backend
   │
   ├─ 5a. next_state = status
   │        ├─ motd.enabled  ──▶ answer here            ──▶ close
   │        └─ !motd.enabled ──▶ proxy the ping upstream
   │
   └─ 5b. next_state = login / transfer
            ├─ acquire the backend's session slot
            ├─ connect with the configured forwarding mode
            ├─ replay every byte read so far, verbatim
            └─ pipe until either side closes
```

Step 5b's replay is worth stating explicitly: the handshake is **not** consumed
by the gateway. The backend receives the client's original bytes, including any
`\0FML2\0` marker, so it sees exactly what a direct connection would have
delivered.

## State and reloads

All mutable state lives behind one atomic pointer:

```text
      ArcSwap<Runtime>
             │
     ┌───────┴────────┐
     ▼                ▼
  Registry         favicon
     │
  ┌──┴──────────────────┐
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

Everything afterwards is bounded too: a handshake byte budget, a frame size cap
well below the protocol's own, timeouts on the handshake, the status exchange,
the backend connect and the idle session, and a per-backend session cap. Nothing
a client sends can make the gateway allocate without limit or wait forever.
