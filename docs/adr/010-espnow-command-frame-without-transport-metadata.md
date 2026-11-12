# ADR 010: ESP-NOW Command Frame Without Transport Metadata

## Status

Accepted

## Context

The ESP-NOW Peripheral Command Framework introduces a `CommandFrame` struct for parsing binary commands in `espnow-pure`.
A key design question was whether `CommandFrame` should include the sender MAC address or remain payload-only, with transport metadata passed separately at the dispatch site.

The sender MAC is useful for routing responses, sender allowlisting, rate limiting, and deduplication.
The question is *where* it belongs in the architecture.

### Industry survey

We surveyed how established embedded wireless protocols handle this:

**Espressif ESP-NOW framework** (`github.com/espressif/esp-now`):
The wire format (`espnow_data_t`) includes `src_addr` and `dest_addr` in the frame header.
However, application-level handler callbacks receive the MAC as a *separate first parameter* alongside the payload — not embedded in the command struct.
The low-level ESP-IDF API (`esp_now_recv_cb_t`) follows the same pattern: `esp_now_recv_info_t` (metadata sidecar) + `data` (payload buffer).

**Bluetooth Mesh (Zephyr):**
Opcode handler signature passes `bt_mesh_msg_ctx` (containing source address, RSSI, TTL, net/app index) as a separate context struct alongside the payload buffer.
Source address is never inside the parsed command.

**ZigBee ZCL:**
The ZCL layer provides an `AddressHeader` or indication struct containing source/destination addresses, endpoints, profile, and cluster as metadata.
Command payload is separate.

**General embedded protocol design** (`commschamp` guide):
Recommends "as little connection as possible between application level messages and wrapping transport data, which allows easy substitution of the latter if need arises."

The dominant pattern across all surveyed protocols is:
1. Command payload is transport-agnostic
2. Transport metadata (MAC, RSSI, channel) travels in a separate context sidecar
3. Internal queues bundle both so nothing is lost during async processing

## Decision

`CommandFrame` contains only the parsed command tag and payload body.
It does not carry the sender MAC, RSSI, or any other transport metadata.

The caller is responsible for threading transport metadata from `EspNowEvent` to the command handler at the dispatch site.

A future `ReceivedCommand` or context wrapper may bundle `CommandFrame` + sender MAC for queuing and async dispatch, but `CommandFrame` itself remains transport-independent.

## Consequences

**Positive:**
- `CommandFrame` is transport-independent — same parsing works over ESP-NOW, UART, or MQTT
- Testable on the host without mocking transport metadata
- Zero-copy: `CommandFrame<'a>` borrows directly from `&'a [u8]` with no allocation
- Consistent with the industry-standard "message context" pattern
- `espnow-pure` stays `no_std` with zero dependencies

**Negative:**
- Handlers that need the sender MAC must receive it as a separate parameter — slightly more verbose dispatch code
- If commands are queued for deferred processing, the caller must bundle the MAC alongside the `CommandFrame` (not automatic)

**Neutral:**
- Does not prevent a future `ReceivedCommand { frame: CommandFrame, sender: [u8; 6] }` wrapper if bundling proves necessary.
  Such a wrapper would live in `espnow-pure` alongside `EspNowEvent`, since the sender MAC is an ESP-NOW transport concept; reuse over UART or MQTT would define its own context type rather than borrowing one tied to MAC addressing.

## Amendment 2026-11-11: System Tags

The system range `0xF0..=0xFF` of `juggler::espnow::command` holds these tags; module tags are `0x01..=0xEF` and `0x00` is reserved.

| Tag | Name | Payload | Since |
|:----|:-----|:--------|:------|
| `0xF0` | `TAG_PING` | request empty; response `[0xF0, 0x01]` | Initial command framework |
| `0xF1` | `TAG_SELF_TEST` | request empty; response `[0xF1, 0x00]` pass or `[0xF1, 0x01]` fail | Initial command framework |
| `0xF2` | `TAG_IDENTIFY` | request empty; response `[0xF2, module_type, version]` | Initial command framework |
| `0xF3` | `TAG_PROBE` | `[0xF3, seq, tx_channel]`, the scan probe (`ProbeFrame`) | [ADR 025](025-espnow-probe-reply-carries-channel.md) |
| `0xF4` | `TAG_PROBE_REPLY` | `[0xF4, seq, tx_channel, home_channel]`, the coordinator's answer (`ProbeReply`) | [ADR 025](025-espnow-probe-reply-carries-channel.md) |

The probe and its reply follow this ADR: neither carries the sender MAC, the scan matches a reply to its probe by `seq` and `tx_channel` and takes the sender from the transport sidecar.
