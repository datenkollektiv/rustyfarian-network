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
| `ddi-idf` | `idf_c3_ddi` | DDI OTA against a lab server: forced update, `up_to_date` and chip gate (1007), cancel and reset mid-download, rollback (1004) and its sticky refusal, wrong sha256 (1002), each row probed on the server | lab DDI server (`DDI_MGMT_*`), local broker, seven bundles published with `just ddi-publish`, see header |
| `ddi-hal` | `hal_c3_ddi` | the `ddi-idf` chain step for step on the esp-hal tier, same patterns, only the example names differ | as `ddi-idf`, with `hal_*` bundles |

The scenario checks and the default-scenario checks (`espnow-power-cycle`, `espnow-peer-session`, `espnow-soak`) need different `ESPNOW_SCENARIO` settings; see the headers.
Holding BOOT through a reset enters download mode; press RST again without BOOT.
The `ddi-idf` and `ddi-hal` checks of the C3 and the C6 can run at the same time because they share the lab proxy; the first run to end leaves it up for the other, and a run that died without its teardown leaves a file in `tmp/bench-ddi-proxy.users/`, so delete that directory; a proxy that was already running when the first run started is borrowed and never stopped by a run.
