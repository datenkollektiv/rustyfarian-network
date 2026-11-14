# Bench — ESP32-C3 SuperMini

| Check | Example(s) | What it proves | Prerequisites |
|:--|:--|:--|:--|
| `connect` | `hal_c3_connect_async` | DHCP lease, 20 s stable, relink after a router block | |
| `mqtt` | `idf_c3_mqtt` | retained `online`, heartbeats, reconnect after a router block | |
| `mqtt-lwt` | `hal_c3_mqtt_lwt` | connect, ticks probed at the broker, reconnect after a router block | |
| `soak` | `hal_c3_mqtt_lwt` | 10 min MQTT run, heap trend | |
| `reentry` | `idf_c3_provision_mqtt` | BOOT hook: short press reopens the portal, long press erases | provisioned board |
| `espnow-cancel` | `idf_c3_espnow_scout`, `hal_c3_espnow_peer` | cancelled scans refind the coordinator on one channel | coordinator board, `ESPNOW_SCENARIO=cancel`, MACs |
| `espnow-flood` | `idf_c3_espnow_scout`, `hal_c3_espnow_peer` | floods of `send_acked` end, each followed by a good send | coordinator board, `ESPNOW_SCENARIO=flood`, MACs |
| `espnow-shutdown` | `idf_c3_espnow_scout`, `hal_c3_espnow_peer` | shutdown mid-scan, no reboot | coordinator board, `ESPNOW_SCENARIO=shutdown`, MACs |
| `espnow-shutdown-queued` | `idf_c3_espnow_scout`, `hal_c3_espnow_peer` | shutdown with queued sends, all `Shutdown` | coordinator board, `ESPNOW_SCENARIO=shutdown-queued`, MACs |
| `espnow-txpower` | `idf_c3_espnow_scout`, `hal_c3_espnow_peer` | scans leave the TX power unchanged | coordinator board, `ESPNOW_SCENARIO=txpower`, MACs |
| `espnow-power-cycle` | `idf_c3_espnow_scout` | scout survives a coordinator power cycle | `idf_c6_espnow_coordinator`, `COORDINATOR_MAC` |
| `espnow-soak` | `hal_c3_espnow_peer` | 30 min at 100 ms, failures below 1 %, rescans across channels | coordinator board, `ESPNOW_PEER_MAC` |
| `espnow-peer-session` | `hal_c3_espnow_peer`, `idf_c3_espnow_peer` | `PeerSession` relinks after a coordinator power cycle | C6 coordinator (IDF or esp-hal), `ESPNOW_PEER_MAC` |
| `ota` | `hal_c3_ota` | download, verify, swap, boot marked valid | `just bench-ota c3 ota …` |
| `ota-rollback` | `hal_c3_ota` | unhealthy image rolls back, no retry | `just bench-ota c3 ota-rollback …` |
| `ota-runtime` | `idf_c3_ota_runtime` | MQTT-triggered OTA rows P2, P7, P13, P14, P17 | local broker, six served images, see header |

The scenario checks and the default-scenario checks (`espnow-power-cycle`, `espnow-peer-session`, `espnow-soak`) need different `ESPNOW_SCENARIO` settings; see the headers.
Holding BOOT through a reset enters download mode; press RST again without BOOT.
