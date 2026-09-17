# Entwicklung

## Aufbau

Ein einziges Rust-Paket:

```text
src/
  main.rs         CLI: starten, --check, --print-network-setup/-teardown, Signale
  server.rs       Listener starten, Config neu laden (SIGHUP), sauber stoppen
  intercept.rs    eine abgefangene Verbindung: Status-Ping erkennen oder durchreichen
  motd.rs         Zeile 1/2 in einer Status-Antwort ersetzen
  chat.rs         Chat-Komponenten ⇄ §-Farbcodes
  protocol.rs     VarInt, Pakete, Handshake — nur was vor dem Login nötig ist
  transparent.rs  TPROXY-Sockets: Listener und Verbindung „als Spieler"
  netsetup.rs     erzeugt die nftables-/Routing-/Firewall-Skripte aus der Config
  config.rs       Config-Datei, Standardwerte, Validierung
  observe.rs      Prometheus-Metriken
tests/            Integrationstests für intercept.rs
dev/              Dev-Lab und End-to-End-Test
deploy/           Config-Vorlage, Docker-Entrypoint, compose, systemd
```

## Wie eine Verbindung läuft

```text
1. nftables leitet neue Verbindungen auf `ports` an 127.0.0.1:25500 / [::1]:25500
2. intercept::run nimmt sie an; die lokale Adresse des Sockets ist das
   ursprüngliche Ziel (nach Dockers DNAT: der Kunden-Container)
3. transparent::connect verbindet dorthin, gebunden an die Spieler-IP
4. intercept::sniff liest die ersten Bytes des Clients:
     Handshake mit next_state=status + Status-Request  → Status
     alles andere, oder der Server sendet zuerst       → durchreichen
5. Status: Antwort des Servers lesen, motd::rewrite_status, weitergeben
6. Danach in beiden Fällen: copy_bidirectional bis zum Ende
```

Grundregel im ganzen Code: **im Zweifel durchreichen**. Kein Fehler beim
Anschauen des Verkehrs darf eine Kundenverbindung kaputtmachen; schlimmstenfalls
fehlt die Zeile.

### Die Firewall-Regeln

`netsetup.rs` erzeugt alles. Zwei conntrack-Markierungen halten die Richtungen
auseinander:

| Markierung | an | wofür |
|---|---|---|
| `0x6d63` | abgefangener Spieler-Verbindung | weitere Pakete des Spielers → Gateway |
| `0x6d65` | Verbindung Gateway → Server | Antworten des Servers → Gateway |
| `0x6d64` | Socket des Gateways (`SO_MARK`) | setzt `0x6d65` im `output` |
| `0x6d67` | Paket | Routing-Tabelle `6767`: lokal zustellen |

Server-Antworten werden an zwei Stellen abgefangen: in `prerouting` (Container
hinter einer Bridge) und in `output` (normale Prozesse und `docker-proxy`, was
Docker für IPv6 ohne Container-IPv6 nutzt).

Die TPROXY-Regel greift nur, wenn ein Socket lauscht. Ist das Gateway weg,
fließt der Verkehr normal weiter — das ist das Fail-open.

## Bauen und testen

```bash
cargo build
cargo test                    # Unit- und Integrationstests, laufen auch auf macOS
cargo clippy --all-targets
```

Die Integrationstests in `tests/intercept.rs` rufen `intercept::handle` direkt
mit der Adresse eines Fake-Servers auf. So läuft die Logik ohne TPROXY — auch
auf dem Mac. Der Fake-Server wartet wie ein echter auf Handshake **und**
Status-Request, bevor er antwortet; das war früher die Ursache eines echten Bugs.

Linux-Code auf dem Mac gegenprüfen:

```bash
rustup target add x86_64-unknown-linux-musl   # einmalig
cargo check --target x86_64-unknown-linux-musl
```

## Dev-Lab

`dev/lab.sh` baut einen simulierten Hosting-Node, gegen den du einen **echten
Minecraft-Client** verbinden kannst. Braucht nur Docker (OrbStack oder Docker
Desktop reichen).

```text
dein Rechner                 Docker
                           ┌──────────────────────────────────────────┐
Minecraft ─ localhost:35565│ Lab-Node (docker:dind)                   │
            localhost:35566│   mc-gateway      (Host-Netz des Nodes)  │
            localhost:35567│   survival :30000 ─┐                     │
                           │   creative :30001 ─┼─ Kunden-Container   │
                           │   skyblock :30002 ─┘  mit Port-Bindings  │
                           └──────────────────────────────────────────┘
```

```bash
dev/lab.sh up              # alles starten
dev/lab.sh ping 35565      # Status-Ping vom Rechner aus
dev/lab.sh reload          # nach Änderung von dev/gateway.yaml
dev/lab.sh gateway         # nach Code-Änderungen: neu bauen und neu starten
dev/lab.sh stop-gateway    # Fail-open ansehen
dev/lab.sh logs            # Gateway-Log
dev/lab.sh down            # alles entfernen
```

In Minecraft `localhost:35565` bis `35567` als Server eintragen. Die drei
Fake-Server (`dev/mc_server.py`) beantworten nur die Serverliste. Zum
Beitreten `PAPER=1 dev/lab.sh up` — dann ist `localhost:35567` ein echter
Paper-Server (der erste Start lädt ihn herunter, das dauert).

Sind die Ports belegt, verschiebt `LAB_PORT=40000 dev/lab.sh up` sie.

Die MOTD live ausprobieren: `motd.line2` in `dev/gateway.yaml` ändern,
`dev/lab.sh reload`, in Minecraft die Serverliste aktualisieren.

## End-to-End-Test

```bash
dev/check.sh
```

Baut das Image, startet einen eigenen Lab-Node und prüft auf Kernel-Ebene, über
IPv4 **und** IPv6, für drei Arten von Kundenserver (Container ohne IPv6,
Container mit IPv6, normaler Prozess):

- die Zeile wird ersetzt, Spielerzahl und erste Zeile bleiben
- der Server sieht beim Login dieselbe Adresse wie ohne Gateway
- Ports außerhalb des Bereichs bleiben unberührt, geschlossene Ports geschlossen
- Fail-open bei gestopptem Gateway
- Neustart ohne doppelte Regeln
- Host-Firewall mit `INPUT DROP` auf beiden Familien
- Teardown entfernt alles

Dauert ein paar Minuten und räumt am Ende auf. **Vor jeder Änderung an
`intercept.rs`, `transparent.rs` oder `netsetup.rs` laufen lassen** — die
Unit-Tests können die Firewall-Regeln nicht prüfen.

## Offene Punkte

- **Ausnahmen pro Kunde** — alle Server eines Nodes bekommen dieselbe Zeile. Ein
  Tarif ohne Werbung bräuchte eine Liste von Ports ohne Rewrite, per `SIGHUP`
  neu ladbar.
- **Neustart trennt Spieler** — deren Verbindung läuft durch den Prozess. Updates
  daher außerhalb der Hauptspielzeit.
- **firewalld** und **Wings auf einem echten Node** sind nicht getestet; die
  systemd-Unit ebenfalls nicht (der Docker-Weg schon).
