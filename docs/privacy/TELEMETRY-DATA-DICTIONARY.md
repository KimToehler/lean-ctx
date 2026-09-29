# Telemetry Data Dictionary

Status: beschreibt den implementierten Vertrag `telemetry_v2`
(`SCHEMA_VERSION`, `deny_unknown_fields`). Serverseitige Aufbewahrung ist hier
**nicht** festgelegt; sie bedarf der Rechtsprüfung (§28).

Architekturkontext: [../architecture/telemetry.md](../architecture/telemetry.md).

Dieses Dokument definiert je Feld: Quelle, Zweck, Aufbewahrung, Kontobindung,
Optionalität, Aggregation und Löschverhalten.

## Lesehilfe

- **Aufbewahrung lokal** — die Konfiguration und das append-only Ledger auf dem
  Gerät des Nutzers; entfernbar mit `lean-ctx telemetry purge-local`.
- **Aufbewahrung remote** — durch Backend-Politik bestimmt und in diesem
  Repository nicht festgelegt. Wo unten „Backend-Politik" steht, ist die Frist
  offen und darf nicht behauptet werden.
- **Kontobindung** — ob das Feld mit einem Konto oder einer Organisation
  verknüpfbar ist.
- **Löschung** — `lean-ctx telemetry delete-remote` fordert die Löschung der
  Installation an; `reset-id` bricht die Verkettung für künftige Sendungen.

## 1. Batch

| Feld | Quelle | Zweck | Aufbewahrung | Kontobindung | Optional | Aggregation | Löschung |
|---|---|---|---|---|---|---|---|
| `schema_version` | Konstante | Vertragsversion, fail-closed | remote: Backend-Politik | nein | nein | keine | mit Installation |
| `deletion_token_hash` | `sha256` eines lokal erzeugten Geheimnisses (64 Hex) | erlaubt Löschung ohne Klaridentität | remote: Backend-Politik | nein | nein | keine | Schlüssel für Löschung |
| `events[]` | Client | Nutzlast | siehe unten | siehe unten | nein | täglich gebündelt | mit Installation |

Grenzen: höchstens `MAX_BATCH_EVENTS` Ereignisse; leerer Batch ist ungültig.

## 2. Envelope

| Feld | Quelle | Zweck | Aufbewahrung | Kontobindung | Optional | Aggregation | Löschung |
|---|---|---|---|---|---|---|---|
| `schema_version` | Konstante | Versionsprüfung | Backend-Politik | nein | nein | keine | mit Installation |
| `timestamp_bucket` | Client, Format `YYYY-MM-DD` | Tagesgenauigkeit statt Zeitpunkt | Backend-Politik | nein | nein | Tagesbucket | mit Installation |
| `installation_id` | zufällige UUID v4, lokal erzeugt | Installationen unterscheiden | lokal in Konfiguration; remote Backend-Politik | nein | nein | keine | `reset-id` bricht Verkettung; `delete-remote` löscht |
| `account_id` | `hmac-sha256:<64 Hex>` | Konto-Kohorten | Backend-Politik | **ja** | ja | keine | mit Installation/Konto |
| `organization_id` | `hmac-sha256:<64 Hex>` | Organisations-Kohorten | Backend-Politik | **ja** | ja | keine | mit Installation/Organisation |
| `app_version` | Build | Versionsverteilung | Backend-Politik | nein | nein | keine | mit Installation |
| `event` | Client | typisierte Nutzlast | siehe unten | siehe unten | nein | siehe unten | mit Installation |

Rohe Bezeichner sind als pseudonyme ID unzulässig: Nur das Präfix
`hmac-sha256:` mit 64 Kleinbuchstaben-Hexzeichen wird akzeptiert, sowohl vom
Konstruktor als auch beim Deserialisieren. Fehlermeldungen geben die abgewiesene
ID nicht wieder. Die übrigen semantischen Grenzen werden weiterhin über
`TelemetryEnvelopeV2::validate()` beziehungsweise `TelemetryBatchV2::validate()`
vor dem Versand geprüft; gültige V2-Wire-Bytes bleiben unverändert.

## 3. Ereignisfamilien

Zweiundzwanzig typisierte Varianten. Keine trägt Freitext.

Das Ereignisobjekt erlaubt ausschließlich `name` und `metrics`; zusätzliche
Felder werden bereits beim Deserialisieren abgewiesen, auch innerhalb eines
Envelopes. Sie werden nicht stillschweigend entfernt. Die gültigen V2-Wire-Bytes
bleiben unverändert; die Korrektur erweitert das erlaubte Schema nicht.

| Ereignis | Metrik-Typ |
|---|---|
| `heartbeat` | `HeartbeatMetrics` |
| `setup_profile` | `SetupProfileMetrics` |
| `setup_completed`, `integration_detected` | `OccurrenceMetrics` |
| `session_aggregate` | `SessionMetrics` |
| `tool_usage_aggregate` | `ToolUsageMetrics` |
| `tool_call_aggregate` | `ToolCallMetrics` |
| `autopilot_aggregate`, `autopilot_fallback_aggregate` | `DecisionMetrics` |
| `sync_aggregate` | `SyncMetrics` |
| `trial_started`, `upgrade_viewed`, `checkout_started`, `subscription_activated`, `subscription_cancelled`, `team_created`, `team_member_invited` | `OccurrenceMetrics` |
| `trial_ended`, `team_context_promoted` | `OutcomeMetrics` |
| `error_category_aggregate` | `ErrorMetrics` |
| `version_upgrade` | `VersionUpgradeMetrics` |
| `orchestration_aggregate` | `OrchestrationMetrics` |

## 4. Metrikfelder

Für alle Felder dieses Abschnitts gilt einheitlich: Quelle ist der lokale
Client, Aufbewahrung remote nach Backend-Politik, Kontobindung nur mittelbar
über `account_id`/`organization_id` im Envelope, Aggregation je Tagesbucket,
Löschung mit der Installation. Abweichungen sind vermerkt.

### `HeartbeatMetrics`

| Feld | Werte | Zweck | Optional |
|---|---|---|---|
| `distribution_channel` | `cargo`, `homebrew`, `npm`, `docker`, `source`, `unknown` | Kanalverteilung | nein |
| `client_family` | `claude`, `codex`, `cursor`, `gemini`, `windsurf`, `zed`, `vscode_copilot`, `kiro`, `antigravity`, `codebuddy`, `codewhale`, `other` | Client-Kohorte (aus dem MCP-Handshake, sonst Umgebungsvariablen; unbekannte Clients werden `other`, nie Klartext) | nein |
| `operating_system` | `macos`, `linux`, `windows`, `other` | Plattformverteilung | nein |
| `architecture` | `x86_64`, `aarch64`, `other` | Build-Priorisierung | nein |

Alle vier sind geschlossene Aufzählungen — keine freien Zeichenketten.

### `SetupProfileMetrics`

Einmal je Tagesbatch, Momentaufnahme der Einrichtung.

| Feld | Werte | Zweck | Optional |
|---|---|---|---|
| `integration_mode` | `default`, `mcp`, `hybrid`, `replace` | welcher Integrationsmodus genutzt wird (`default` = nie gesetzt) | nein |
| `embeddings` | `unsupported`, `disabled`, `not_installed`, `installed` | Verbreitung der semantischen Suche | nein |

Nicht enthalten: Modellname, Pfade, Konfigurationswerte, Projektanzahl.

### `ToolCallMetrics`

| Feld | Werte | Zweck | Optional |
|---|---|---|---|
| `tools[].tool` | Name eines **eingebauten** lean-ctx-Tools (`[a-z][a-z0-9_]*`, ≤ 64 Byte) | welche Tools tatsächlich genutzt werden | nein |
| `tools[].calls`, `tools[].failures` | Zähler seit dem letzten bestätigten Batch | Nutzung und Fehlerquote je Tool | nein |

Namen stammen ausschließlich aus der statischen Tool-Registry des Binaries;
Tools fremder MCP-Server, Argumente und Ergebnisse werden nie gezählt. Liste
strikt sortiert und eindeutig, höchstens `MAX_TOOL_ENTRIES` (128) Einträge
(bei Überlauf die meistgenutzten), `failures ≤ calls`, `calls ≥ 1`.

### Zählerstrukturen

| Struktur | Felder | Zweck |
|---|---|---|
| `OccurrenceMetrics` | `count` | Häufigkeit eines Funnel- oder Setup-Ereignisses |
| `OutcomeMetrics` | `accepted`, `rejected`, `unknown` | Ergebnisverteilung |
| `SessionMetrics` | `sessions`, `duration_seconds` (Histogramm) | Nutzungsintensität |
| `ToolUsageMetrics` | `calls`, `failures`, `latency_milliseconds` (Histogramm) | Zuverlässigkeit und Latenz |
| `DecisionMetrics` | `admitted`, `denied`, `fallback` | Autopilot-Entscheidungskategorien |
| `SyncMetrics` | `attempts`, `successes`, `failures` | Sync-Zuverlässigkeit |
| `ErrorMetrics` | `category`, `count` | Fehlerklassen ohne Fehlertext |
| `VersionUpgradeMetrics` | `from_major`, `to_major` | Upgrade-Pfade |

`category` ist eine geschlossene Aufzählung: `authentication`, `authorization`,
`configuration`, `network`, `provider`, `timeout`, `validation`, `internal`.
**Rohe Fehlermeldungen erscheinen nirgends.**

Konsistenz wird erzwungen: `failures ≤ calls`; `successes + failures ≤ attempts`;
`to_major ≥ from_major`. Jeder Zähler ist durch `MAX_COUNT` begrenzt.

### `OrchestrationMetrics`

| Feld | Zweck |
|---|---|
| `admitted_tasks`, `execution_plans` | Umfang bezahlter Orchestrierung |
| `retries`, `fallbacks`, `cancellations` | Robustheit |
| `lease_conflicts` | Konkurrenz um Mutationsrechte |
| `receipts` | Nachweisdichte |
| `outcomes` | eingebettete `OutcomeMetrics` |
| `node_count`, `fan_out`, `depth`, `parallel_nodes` | Histogramme der Graphform |

Nicht enthalten und ausdrücklich verboten: Aufgabentext, Prompts,
Kind-Agent-Ausgaben, Pfade, Dateinamen, Evidenz- und Kontextinhalte, exakte
Repository-Identität, Policy-Text.

### `Histogram`

`upper_bounds[]` und `counts[]`, geordnet und in Länge gekoppelt, begrenzt durch
`MAX_HISTOGRAM_BUCKETS`. Histogramme tragen Verteilungen, keine Einzelwerte —
ein einzelner Vorgang ist daraus nicht rekonstruierbar.

## 5. Lokales Ledger

Append-only auf dem Gerät, nie an das Backend gesendet.

| Feld | Zweck | Aufbewahrung | Löschung |
|---|---|---|---|
| `timestamp` | Nachvollzug der Sendung | lokal | `purge-local` |
| `installation_id`, `version`, `os`, `arch` | Zuordnung der Sendung | lokal | `purge-local` |
| `schema_version` | Vertragsversion | lokal | `purge-local` |
| `event_names[]` | welche Ereignisse gingen raus | lokal | `purge-local` |
| `payload_hash` | Integritätsnachweis **statt Inhalt** | lokal | `purge-local` |
| `endpoint`, `status` | Ziel und Ergebnis | lokal | `purge-local` |

Das Ledger enthält bewusst keine Nutzlasten, nur deren Hash.

## 6. Rechtliche Prüfliste (offen)

Vor einer öffentlichen Datenschutzaussage ist durch Rechtsberatung zu klären:

- Einstufung der Installations-ID und der HMAC-Pseudonyme nach Schweizer DSG
  und DSGVO;
- Rechtsgrundlage für Standard-an-Telemetrie je Rechtsordnung;
- konkrete serverseitige Aufbewahrungsfristen je Tabelle;
- Erfüllung von Auskunfts- und Löschbegehren über `deletion_token_hash`, ohne
  eine Klaridentität einzuführen;
- Behandlung von IP-Adressen an der Netzwerkkante — sie dürfen nicht als
  Produktanalyse-Identifikator persistiert werden;
- Auftragsverarbeitung und Drittlandtransfer;
- Aufbewahrung in Team- und Enterprise-Kontexten mit Organisationsbindung.

Bis diese Punkte geklärt sind, wird keine Frist und keine Rechtsgrundlage als
Tatsache behauptet.
