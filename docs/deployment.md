# Deployment

Das Gateway läuft auf **jedem Node**, auf dem Kundenserver laufen. Pro Node eine
Instanz mit einer Config.

## Wie es auf dem Node arbeitet

```text
Spieler ──▶ node:30123
              │
              │ nftables: Port in `ports`?
              ├── nein ─────────────────────────────▶ Kunden-Container (wie immer)
              │
              └── ja ──▶ mc-gateway ──▶ Kunden-Container
                           │               (sieht die Spieler-IP)
                           │
                           ├─ Status-Ping: Antwort des Servers holen,
                           │               Zeile 2 ersetzen, zurückgeben
                           └─ alles andere: Bytes unverändert durchreichen
```

Beim Start installiert das Gateway eine nftables-Tabelle `mcgateway`, eine
Routing-Regel (Tabelle `6767`) und eine `INPUT`-Freigabe für seine eigene
Markierung. Die Regeln erzeugt es aus der Config selbst, Ports und Firewall
können also nicht auseinanderlaufen.

## Voraussetzungen

- Linux mit nftables (jede aktuelle Distribution)
- `ip`, `nft`, `iptables`/`ip6tables` — im Docker-Image enthalten
- Kundenserver als Docker-Container mit veröffentlichten Ports (Pterodactyl,
  Pelican) oder als normale Prozesse auf dem Node

## Konfiguration

[`deploy/config.yaml`](../deploy/config.yaml) ist die Vorlage. Das Wichtigste:

| Schlüssel | Bedeutung |
|---|---|
| `ports` | Portbereich(e) eurer Kundenserver, z. B. `["25565-25665", "30000-31000"]` |
| `motd.line2` | Eure Zeile. `&`-Farbcodes und `&#rrggbb` funktionieren |
| `motd.line1` | Optional: ersetzt auch die erste Zeile (die gehört normalerweise dem Kunden) |
| `listen` / `listen_v6` | Interne Übergabe-Adressen. Nur ändern, wenn Port 25500 belegt oder im Bereich ist. `listen_v6: null` schaltet IPv6 ab |
| `timeouts.drain` | Wie lange laufende Verbindungen beim Stoppen noch bekommen |
| `metrics` | Prometheus-Endpunkt, standardmäßig aus |
| `log.client_ip` | Spieler-IPs loggen, standardmäßig aus |

Prüfen ohne zu starten:

```bash
mc-gateway --config /etc/mc-gateway/config.yaml --check
```

## Installation

### Variante A: Docker (empfohlen)

```bash
git clone <repo> /opt/mc-gateway && cd /opt/mc-gateway/deploy
nano config.yaml                      # Ports und Zeile eintragen
docker compose up -d --build
docker compose logs -f
```

[`deploy/compose.yml`](../deploy/compose.yml) setzt `network_mode: host` und
`cap_add: [NET_ADMIN]`; beides ist nötig. Der Container richtet die Regeln als
root ein und läuft danach als unprivilegierter Nutzer mit nur dieser einen
Capability.

### Variante B: systemd

```bash
cargo build --release
sudo install -m 755 target/release/mc-gateway /usr/local/bin/
sudo install -D -m 644 deploy/config.yaml /etc/mc-gateway/config.yaml
sudo useradd --system --no-create-home --shell /usr/sbin/nologin mc-gateway
sudo install -m 644 deploy/mc-gateway.service /etc/systemd/system/
sudo systemctl enable --now mc-gateway
```

Die Unit nutzt die Firewall-Werkzeuge des Hosts — sinnvoll, wenn der Host noch
iptables-legacy verwendet (das Docker-Image bringt iptables-nft mit).

## Prüfen, ob es läuft

1. **Log** — beim Start erscheinen `network setup installed` und je eine Zeile
   `intercepting` für IPv4 und IPv6.
2. **Von außen pingen** — einen Kundenserver in die Minecraft-Serverliste
   eintragen. Die zweite Zeile muss eure sein.
3. **Metriken** (falls aktiviert):

   ```bash
   curl -s localhost:9100/metrics | grep mc_gateway
   ```

   | Metrik | |
   |---|---|
   | `mc_gateway_status_requests_total` | erkannte Status-Pings |
   | `mc_gateway_motd_rewrites_total` | davon mit eurer Zeile |
   | `mc_gateway_connections_active` | Verbindungen gerade durch das Gateway |
   | `mc_gateway_server_unreachable_total` | Server hat nicht geantwortet (z. B. gestoppt) |

   Liegen `status_requests` und `motd_rewrites` dauerhaft auseinander,
   antworten Server mit etwas, das nicht umgeschrieben werden kann. Sie bekommen
   dann ihre Original-Antwort.

## Betrieb

**Zeile ändern** — `motd.line2` bearbeiten, dann neu laden. Keine Verbindung
bricht ab:

```bash
docker compose kill -s HUP          # Docker
systemctl reload mc-gateway         # systemd
```

**Ports ändern** — Config bearbeiten und **neu starten**. Die Firewall-Regeln
werden beim Start erzeugt.

**Neustart und Updates** — ⚠️ Spieler, die gerade über das Gateway verbunden
sind, werden beim Stoppen getrennt; ihre Verbindung läuft durch den Prozess.
Neue Verbindungen gehen, solange das Gateway nicht läuft, direkt zum Server.
Updates also außerhalb der Hauptspielzeit einspielen:

```bash
git pull && docker compose up -d --build
```

**Last** — der gesamte Spielverkehr intercepteter Verbindungen läuft durch das
Gateway. Das kostet pro Spieler zwei Sockets und etwas CPU fürs Kopieren.
`LimitNOFILE` in der systemd-Unit ist entsprechend hoch gesetzt.

## Was ein Kundenserver sieht

Dieselbe Absenderadresse wie ohne Gateway, nur mit anderem Quellport:

| Server | IPv4-Spieler | IPv6-Spieler |
|---|---|---|
| Container, Docker-Netz ohne IPv6 (Standard) | Spieler-IP | Docker-Bridge, z. B. `172.17.0.1` |
| Container, Docker-Netz mit IPv6 | Spieler-IP | Spieler-IPv6 |
| normaler Prozess auf dem Node | Spieler-IP | Spieler-IPv6 |

Die Bridge-Adresse in Zeile 1 kommt von Docker selbst (`docker-proxy`), nicht
vom Gateway. Mit IPv6 im Docker-Netz der Kunden verschwindet sie.

## Verhalten im Fehlerfall

| Situation | Ergebnis |
|---|---|
| Gateway gestoppt oder abgestürzt | Verbindungen gehen direkt zum Server, ohne eure Zeile |
| Kundenserver gestoppt | Verbindung wird geschlossen, wie bei einem geschlossenen Port |
| Server antwortet mit Unlesbarem | Original-Antwort geht durch |
| Kein Minecraft (RCON, HTTP, …) auf dem Port | unverändert durchgereicht |
| Host-Firewall mit `INPUT DROP` (ufw) | funktioniert, das Gateway setzt seine Freigabe selbst |
| Node ohne IPv6 | nur IPv4 wird abgefangen, Warnung im Log |

## Host-Firewall

Abgefangene Verbindungen laufen durch die `INPUT`-Chain, was veröffentlichte
Docker-Ports sonst nie tun. Das Gateway fügt deshalb beim Start in `iptables`
und `ip6tables` ein:

```text
-A INPUT -m mark --mark 0x6d67 -j ACCEPT
```

Eine Portfreigabe hilft hier nicht: nach Dockers DNAT trägt das Paket den
internen Container-Port. Getestet mit einer `INPUT DROP`-Policy. **firewalld**
verwaltet eigene Regeln, dort muss dieselbe Freigabe (fwmark `0x6d67` in input)
von Hand eingetragen werden — das ist ungetestet.

## Entfernen

```bash
docker compose down
docker run --rm --network host --cap-add NET_ADMIN --entrypoint sh mc-gateway:latest \
    -c 'mc-gateway --print-network-teardown | sh'
```

Mit systemd:

```bash
sudo systemctl disable --now mc-gateway
mc-gateway --print-network-teardown | sudo sh
```

Bleiben die Regeln nach dem Stoppen stehen, ist das unschädlich: Ohne laufendes
Gateway greifen sie nicht.

## Fehlersuche

| Symptom | Ursache |
|---|---|
| `cannot set IP_TRANSPARENT … Operation not permitted` | `NET_ADMIN` fehlt (`cap_add` bzw. `AmbientCapabilities`) |
| Keine Zeile, Verbindung klappt | Gateway läuft nicht, oder Port nicht in `ports` |
| Spieler kommen nicht mehr rein, sobald das Gateway läuft | eine Firewall verwirft `INPUT` und die Freigabe greift nicht (firewalld, iptables-legacy im Docker-Betrieb → systemd-Variante nehmen) |
| `listen port … lies inside ports` | `listen` auf einen Port außerhalb des Bereichs legen |

Aktive Regeln ansehen: `nft list table inet mcgateway`. Was installiert würde,
ohne etwas zu ändern: `mc-gateway --print-network-setup`.
