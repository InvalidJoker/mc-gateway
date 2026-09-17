# mc-gateway

Schreibt eure Zeile in die MOTD jedes Minecraft-Servers auf einem Hosting-Node —
und ist ansonsten unsichtbar.

```text
Serverliste des Spielers, vorher:          nachher:
  Steve's SMP                                Steve's SMP
  whitelist on                               Hosted by example.net
```

Zeile 1, Spielerzahl, Version und Icon bleiben die des Kunden. Logins, Gameplay
und alles, was kein Status-Ping ist, gehen unverändert durch. Der Kundenserver
sieht die echte Spieler-IP. Läuft das Gateway nicht, gehen alle Verbindungen
direkt zum Server.

## Konfiguration

```yaml
ports: ["25565-25665"]                  # euer Allocation-Bereich
motd:
  line2: "&7Hosted by &bexample.net"
```

Vollständiges Beispiel mit allen Optionen: [`deploy/config.yaml`](deploy/config.yaml).

## Dokumentation

- **[Deployment](docs/deployment.md)** — auf einem Node installieren, betreiben, entfernen
- **[Entwicklung](docs/development.md)** — Aufbau des Codes, Tests, Dev-Lab mit echtem Minecraft-Client

## Kurzfassung

```bash
# Node (Linux, als root)
cp deploy/config.yaml /etc/mc-gateway/config.yaml   # Ports und Zeile anpassen
cd deploy && docker compose up -d --build

# Entwicklung
cargo test
dev/lab.sh up        # Test-Node mit Servern, erreichbar unter localhost:35565
dev/check.sh         # vollständiger End-to-End-Test
```
