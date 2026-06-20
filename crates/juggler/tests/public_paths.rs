// public_paths.rs — downstream-style public-path smoke tests for `juggler`.
//
// Each `#[test]` fn imports and references the principal public items via the
// EXTERNAL `juggler::<domain>::` paths exactly as a downstream crates.io
// consumer would write them.  The goal is to lock the re-export surface so
// that a future refactor that accidentally breaks a public path shows up here
// as a compile error, not a surprise for a downstream user.
//
// These tests are lightweight path checks, not behaviour re-tests.  For
// behavioural coverage see the `#[cfg(test)]` modules inline in each source
// file.
//
// Feature-gate alignment with the existing `just` recipes (all run on the host
// target, no ESP toolchain needed):
//
//   just test-wifi         → -p juggler --features mock
//   just test-lora         → -p juggler --features lora,mock
//   just test-espnow       → -p juggler --features mock           (mock implies espnow)
//   just test-mqtt         → -p juggler --features std mqtt        (std implies mqtt)
//   just test-ota          → -p juggler --features ota
//   just test-provisioning → -p juggler --features provisioning

// ── wifi ─────────────────────────────────────────────────────────────────────

#[cfg(feature = "wifi")]
#[test]
fn wifi_public_paths() {
    use juggler::wifi::{
        validate_ap_config, validate_password, validate_ssid, wifi_disconnect_reason_name,
        ApConfig, ConnectMode, TxPowerLevel, WiFiConfig, WifiDriver, WifiPowerSave, AP_CHANNEL_MAX,
        AP_CHANNEL_MIN, AP_MAX_CONNECTIONS_DEFAULT, AP_PASSWORD_MIN_LEN, DEFAULT_TIMEOUT_SECS,
        PASSWORD_MAX_LEN, POLL_INTERVAL_MS, SSID_MAX_LEN,
    };

    // Constants are reachable.
    let _: usize = SSID_MAX_LEN;
    let _: usize = PASSWORD_MAX_LEN;
    let _: usize = AP_PASSWORD_MIN_LEN;
    let _: u8 = AP_CHANNEL_MIN;
    let _: u8 = AP_CHANNEL_MAX;
    let _: u8 = AP_MAX_CONNECTIONS_DEFAULT;
    let _: u64 = DEFAULT_TIMEOUT_SECS;
    let _: u64 = POLL_INTERVAL_MS;

    // Validator functions are callable.
    assert!(validate_ssid("TestNet").is_ok());
    assert!(validate_password("hunter2").is_ok());
    assert_eq!(wifi_disconnect_reason_name(201), Some("NO_AP_FOUND"));
    assert_eq!(wifi_disconnect_reason_name(0), None);

    // WiFiConfig builder chain.
    let cfg = WiFiConfig::new("TestNet", "hunter2")
        .with_timeout(60)
        .with_power_save(WifiPowerSave::MinModem)
        .with_tx_power(TxPowerLevel::Low);
    assert_eq!(cfg.ssid, "TestNet");
    assert!(matches!(
        cfg.connect_mode,
        ConnectMode::Blocking { timeout_secs: 60 }
    ));

    let cfg_nb = WiFiConfig::new("Net", "pw").connect_nonblocking();
    assert!(matches!(cfg_nb.connect_mode, ConnectMode::NonBlocking));

    // TxPowerLevel::to_quarter_dbm is reachable.
    assert_eq!(TxPowerLevel::Medium.to_quarter_dbm(), 52i8);

    // ApConfig builder chain.
    let ap = ApConfig::wpa2("RustyNet", "password12")
        .with_channel(6)
        .with_max_connections(2)
        .with_tx_power(TxPowerLevel::Low);
    assert!(validate_ap_config(&ap).is_ok());

    let ap_open = ApConfig::open("OpenNet");
    assert!(validate_ap_config(&ap_open).is_ok());

    // WifiDriver trait is in scope (turbofish would require a concrete type;
    // just confirm it names a trait by using it as a bound in a local fn).
    fn _accepts_driver<D: WifiDriver>(_: &D) {}
}

// ── wifi::mock ────────────────────────────────────────────────────────────────

#[cfg(all(feature = "wifi", feature = "mock"))]
#[test]
fn wifi_mock_public_paths() {
    use juggler::wifi::mock::{MockWifiDriver, MockWifiError};
    use juggler::wifi::WifiDriver;

    let mut drv = MockWifiDriver::new();
    drv.configure("ssid", "psk").unwrap();
    drv.start().unwrap();
    drv.connect().unwrap();
    assert!(drv.is_connected().unwrap());
    drv.disconnect().unwrap();
    assert!(!drv.is_connected().unwrap());

    // MockWifiError is reachable.
    let _: MockWifiError = MockWifiError::ConnectFailed;
}

// ── mqtt ──────────────────────────────────────────────────────────────────────

#[cfg(any(feature = "mqtt", feature = "std"))]
#[test]
fn mqtt_public_paths() {
    use juggler::mqtt::{
        connection_wait_iterations, next_state, topic_matches_filter, validate_broker_host,
        validate_broker_port, validate_client_id, validate_publish_topic,
        validate_subscribe_filter, validate_topic, MqttConnectionState, MqttEvent,
        CLIENT_ID_MAX_LEN, TOPIC_MAX_LEN,
    };

    // Constants.
    let _: usize = CLIENT_ID_MAX_LEN;
    let _: usize = TOPIC_MAX_LEN;

    // Validation functions.
    assert!(validate_topic("sensors/temperature").is_ok());
    assert!(validate_topic("").is_err());
    assert!(validate_publish_topic("sensors/temp").is_ok());
    assert!(validate_publish_topic("sensors/+/temp").is_err());
    assert!(validate_subscribe_filter("sensors/#").is_ok());
    assert!(validate_subscribe_filter("sport+").is_err());
    assert!(validate_client_id("my-device-01").is_ok());
    assert!(validate_client_id("").is_err());
    assert!(validate_broker_host("192.168.1.1").is_ok());
    assert!(validate_broker_host("").is_err());
    assert!(validate_broker_port(1883).is_ok());
    assert!(validate_broker_port(0).is_err());

    // Topic matching.
    assert!(topic_matches_filter("sensors/temp", "sensors/+"));
    assert!(!topic_matches_filter("$SYS/monitor", "#"));

    // Poll-iterations helper.
    assert_eq!(connection_wait_iterations(5050), 51);

    // State machine.
    assert_eq!(
        next_state(MqttConnectionState::Connecting, MqttEvent::Connected),
        Some(MqttConnectionState::Connected)
    );
    assert_eq!(
        next_state(MqttConnectionState::Connected, MqttEvent::Connected),
        None
    );
}

// ── mqtt std helpers ──────────────────────────────────────────────────────────

#[cfg(feature = "std")]
#[test]
fn mqtt_std_public_paths() {
    use juggler::mqtt::{format_broker_url, spawn_subscriber_thread, QoS, SubscribeClient};
    use std::sync::{Arc, Mutex};

    // QoS enum is reachable.
    let _: QoS = QoS::AtLeastOnce;
    let _: QoS = QoS::AtMostOnce;
    let _: QoS = QoS::ExactlyOnce;

    // format_broker_url produces a usable URL.
    let url = format_broker_url("broker.local", 1883);
    assert_eq!(url, "mqtt://broker.local:1883");

    // SubscribeClient trait in scope: verify with a minimal impl.
    struct NoopClient;
    impl SubscribeClient for NoopClient {
        fn subscribe_topic(&mut self, _topic: &str, _qos: QoS) -> anyhow::Result<()> {
            Ok(())
        }
    }

    // spawn_subscriber_thread is callable (returns immediately; the thread may
    // still be running when the assertion below executes, but that is fine for
    // a path-smoke test).
    let client = Arc::new(Mutex::new(NoopClient));
    spawn_subscriber_thread(client, vec![("sensors/#".to_string(), QoS::AtLeastOnce)], 0);
}

// ── lora ──────────────────────────────────────────────────────────────────────

#[cfg(feature = "lora")]
#[test]
fn lora_public_paths() {
    use juggler::lora::{
        Bandwidth, CodingRate, Downlink, HeltecV3Pins, LoraConfig, LoraRadio, LorawanDevice,
        LorawanError, LorawanResponse, LorawanSessionData, LorawanState, Region, RxConfig,
        RxQuality, RxWindow, SpreadingFactor, TxConfig, RX_WINDOW_DURATION_MS, RX_WINDOW_OFFSET_MS,
    };

    // Constants.
    let _: i32 = RX_WINDOW_OFFSET_MS;
    let _: u32 = RX_WINDOW_DURATION_MS;

    // Region and LoraConfig.
    let _: Region = Region::EU868;
    let _: Region = Region::US915;
    let cfg = LoraConfig::default();
    assert_eq!(cfg.region, Region::EU868);

    let parsed = LoraConfig::from_hex_strings(
        Region::EU868,
        "0000000000000001",
        "0000000000000002",
        "00000000000000000000000000000003",
    );
    assert!(parsed.is_some());

    // HeltecV3Pins.
    let pins = HeltecV3Pins::default_pins();
    assert_eq!(pins.nss, 8);

    // SpreadingFactor, Bandwidth, CodingRate.
    let _: SpreadingFactor = SpreadingFactor::SF7;
    let _: Bandwidth = Bandwidth::BW125;
    let _: CodingRate = CodingRate::Cr45;

    // TxConfig / RxConfig / RxWindow / RxQuality.
    let _tx = TxConfig {
        freq_hz: 868_100_000,
        sf: SpreadingFactor::SF7,
        bw: Bandwidth::BW125,
        cr: CodingRate::Cr45,
        power_dbm: 14,
    };
    let _rx = RxConfig {
        freq_hz: 869_525_000,
        sf: SpreadingFactor::SF12,
        bw: Bandwidth::BW125,
        cr: CodingRate::Cr45,
    };
    let _: RxWindow = RxWindow::Rx1;
    let _: RxWindow = RxWindow::Rx2;
    let q = RxQuality::default();
    assert_eq!(q.rssi, 0);

    // LorawanState.
    let _: LorawanState = LorawanState::Idle;
    let _: LorawanState = LorawanState::Joined;

    // LorawanSessionData.
    let sess = LorawanSessionData::empty();
    assert_eq!(sess.valid, 0);

    // LorawanResponse variants.
    let _: LorawanResponse = LorawanResponse::NoUpdate;
    let _: LorawanResponse = LorawanResponse::JoinSuccess;
    let _: LorawanResponse = LorawanResponse::JoinFailed;
    let _: LorawanResponse = LorawanResponse::TimeoutRequest(100);

    // LoraRadio trait in scope.
    fn _accepts_radio<R: LoraRadio>(_: &R) {}

    // LorawanError<_> is constructible.
    fn _makes_error<E: core::fmt::Debug>() -> LorawanError<E> {
        LorawanError::JoinFailed
    }

    // Downlink struct is constructible (fields are pub).
    let _dl = Downlink {
        port: 10,
        data: heapless::Vec::new(),
        rssi: -80,
    };

    // LorawanDevice can be named and constructed (requires mock or a real radio).
    // We name the type in a phantom way to confirm the import compiles.
    fn _accepts_device<R: LoraRadio>(_: &LorawanDevice<R>) {}
}

// ── lora::mock ────────────────────────────────────────────────────────────────

#[cfg(all(feature = "lora", feature = "mock"))]
#[test]
fn lora_mock_public_paths() {
    use juggler::lora::mock::{MockLoraRadio, RecordedTx, RxResponse};
    use juggler::lora::{LoraConfig, LorawanDevice, RxQuality};

    let radio = MockLoraRadio::new();

    // RecordedTx and RxResponse names are reachable (used by callers inspecting
    // the mock after driving it in tests).
    fn _uses_recorded(_: &RecordedTx) {}
    fn _uses_rx_response(_: &RxResponse) {}

    // LorawanDevice<MockLoraRadio> — the primary use case for the mock.
    let cfg = LoraConfig::default();
    let _device: LorawanDevice<MockLoraRadio> = LorawanDevice::new(radio, cfg);

    // RxQuality::default is reachable via the lora path (it lives in lora::mod).
    let _: RxQuality = RxQuality::default();
}

// ── espnow ────────────────────────────────────────────────────────────────────

#[cfg(feature = "espnow")]
#[test]
fn espnow_public_paths() {
    use juggler::espnow::{
        parse_frame, parse_system_command, validate_payload, CommandFrame, EspNowDriver,
        EspNowEvent, MacAddress, PeerConfig, ScanConfig, ScanResult, SystemCommand, WifiInterface,
        BROADCAST_MAC, DEFAULT_BURST_TIMEOUT, DEFAULT_CONFIRMATION_GAP,
        DEFAULT_PROBE_CONFIRMATIONS, DEFAULT_PROBE_TIMEOUT, DEFAULT_RX_CHANNEL_CAPACITY,
        DEFAULT_SCAN_CHANNELS, MAX_DATA_LEN, TAG_IDENTIFY, TAG_PING, TAG_SELF_TEST,
    };

    // Constants.
    let _: usize = MAX_DATA_LEN;
    let _: usize = DEFAULT_RX_CHANNEL_CAPACITY;
    let _: [u8; 13] = DEFAULT_SCAN_CHANNELS;
    let _: core::time::Duration = DEFAULT_PROBE_TIMEOUT;
    let _: core::time::Duration = DEFAULT_BURST_TIMEOUT;
    let _: u8 = DEFAULT_PROBE_CONFIRMATIONS;
    let _: core::time::Duration = DEFAULT_CONFIRMATION_GAP;
    let _: u8 = TAG_PING;
    let _: u8 = TAG_IDENTIFY;
    let _: u8 = TAG_SELF_TEST;

    // MacAddress / BROADCAST_MAC.
    let _: MacAddress = BROADCAST_MAC;
    assert_eq!(BROADCAST_MAC, [0xFF; 6]);

    // WifiInterface.
    let _: WifiInterface = WifiInterface::Sta;
    let _: WifiInterface = WifiInterface::Ap;
    assert_eq!(WifiInterface::default(), WifiInterface::Sta);

    // validate_payload.
    assert!(validate_payload(b"hello").is_ok());
    assert!(validate_payload(&[0u8; 251]).is_err());

    // EspNowEvent.
    let mac: MacAddress = [0x01, 0x02, 0x03, 0x04, 0x05, 0x06];
    let ev = EspNowEvent::new(mac, b"ping");
    assert_eq!(ev.payload(), b"ping");
    assert_eq!(ev.mac, mac);

    // PeerConfig builder.
    let peer = PeerConfig::new(mac).with_ap_interface();
    assert_eq!(peer.interface, WifiInterface::Ap);

    // ScanConfig builder.
    let scan = ScanConfig::new(b"probe")
        .with_channels(&[1, 6, 11])
        .with_probe_confirmations(2)
        .with_confirmation_gap(core::time::Duration::from_millis(200))
        .with_probe_timeout(core::time::Duration::from_millis(50))
        .with_burst_timeout(core::time::Duration::from_secs(5));
    assert_eq!(scan.channels, &[1, 6, 11]);
    assert_eq!(scan.probe_confirmations, 2);

    // ScanResult.
    let _: ScanResult = ScanResult { channel: 6 };

    // EspNowDriver trait is in scope.
    fn _accepts_driver<D: EspNowDriver>(_: &D) {}

    // parse_frame / parse_system_command / CommandFrame / SystemCommand.
    // Call the functions directly to confirm the paths resolve.
    let raw = &[TAG_PING, 0x00u8][..];
    let frame: CommandFrame<'_> =
        parse_frame(raw).expect("parse_frame should succeed on non-empty input");
    assert_eq!(frame.tag, TAG_PING);
    let cmd = parse_system_command(&frame).expect("TAG_PING should parse as a system command");
    assert_eq!(cmd, SystemCommand::Ping);
}

// ── espnow::mock ──────────────────────────────────────────────────────────────

#[cfg(all(feature = "espnow", feature = "mock"))]
#[test]
fn espnow_mock_public_paths() {
    use juggler::espnow::mock::{MockEspNowDriver, MockEspNowError};
    use juggler::espnow::{EspNowDriver, EspNowEvent, MacAddress, PeerConfig};

    let drv = MockEspNowDriver::new();
    let mac: MacAddress = [0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF];
    drv.add_peer(&PeerConfig::new(mac)).unwrap();
    drv.send(&mac, b"hello").unwrap();
    assert_eq!(drv.sent_count(), 1);

    let ev = EspNowEvent::new(mac, b"incoming");
    drv.queue_rx_event(ev);
    assert!(drv.try_recv().is_some());

    // MockEspNowError variant reachable.
    let _: MockEspNowError = MockEspNowError::SendFailed;
}

// ── ota ───────────────────────────────────────────────────────────────────────

#[cfg(feature = "ota")]
#[test]
fn ota_public_paths() {
    use juggler::ota::{
        bytes_to_hex, hex_to_bytes, ImageMetadata, OtaError, OtaState, StreamingVerifier, Version,
    };

    // Version::new / parse / comparison.
    let v = Version::new(1, 2, 3);
    assert_eq!(Version::parse("1.2.3").unwrap(), v);
    assert!(Version::parse("1.2.3.4").is_err());
    assert_eq!(Version::parse("1.2.3"), Ok(Version::new(1, 2, 3)));
    assert!(Version::new(1, 0, 0) > Version::new(0, 9, 9));

    // OtaError variants.
    let _: OtaError = OtaError::ServerUnreachable;
    let _: OtaError = OtaError::DownloadFailed { status: 404 };
    let _: OtaError = OtaError::DownloadTimeout;
    let _: OtaError = OtaError::ChecksumMismatch;
    let _: OtaError = OtaError::VersionInvalid;
    let _: OtaError = OtaError::FlashWriteFailed;
    let _: OtaError = OtaError::PartitionNotFound;
    let _: OtaError = OtaError::InsufficientSpace;

    // OtaState linear progression.
    let s = OtaState::Idle;
    assert_eq!(s.next_state(), Some(OtaState::Downloading));
    assert_eq!(OtaState::Booted.next_state(), None);

    // StreamingVerifier.
    let mut sv = StreamingVerifier::new();
    sv.update(b"hello");
    let digest = sv.finalize();
    assert_eq!(digest.len(), 32);

    // bytes_to_hex / hex_to_bytes round-trip.
    let hex = bytes_to_hex(&digest);
    assert_eq!(hex.len(), 64);
    let back = hex_to_bytes(&hex).unwrap();
    assert_eq!(back, digest);

    // ImageMetadata is reachable (construction requires heapless types; just
    // confirm the import resolves — the name alone is the path test).
    fn _uses_metadata(_: &ImageMetadata) {}
}

// ── provisioning ─────────────────────────────────────────────────────────────

#[cfg(feature = "provisioning")]
#[test]
fn provisioning_public_paths() {
    use juggler::provisioning::{
        derive_softap_ssid, parse_form, ExtraField, Field, FieldError, FieldErrors,
        InvalidTransition, LoraFields, MqttFields, ProvisioningConfig, ProvisioningInput,
        ProvisioningState, SchemaProfile, ValidationError, DEVICE_NAME_MAX_LEN, EXTRA_FIELDS_MAX,
        EXTRA_KEY_MAX_LEN, EXTRA_VALUE_MAX_LEN, MAX_FIELD_ERRORS, MQTT_HOST_MAX_LEN,
        MQTT_PASS_MAX_LEN, MQTT_USER_MAX_LEN, OTA_URL_MAX_LEN,
    };

    // Constants.
    let _: usize = DEVICE_NAME_MAX_LEN;
    let _: usize = OTA_URL_MAX_LEN;
    let _: usize = EXTRA_FIELDS_MAX;
    let _: usize = EXTRA_KEY_MAX_LEN;
    let _: usize = EXTRA_VALUE_MAX_LEN;
    let _: usize = MAX_FIELD_ERRORS;
    let _: usize = MQTT_HOST_MAX_LEN;
    let _: usize = MQTT_PASS_MAX_LEN;
    let _: usize = MQTT_USER_MAX_LEN;

    // SchemaProfile.
    let _: SchemaProfile = SchemaProfile::LorawanFieldDevice;
    let _: SchemaProfile = SchemaProfile::WifiMqttDevice;
    assert_eq!(SchemaProfile::LorawanFieldDevice.as_str(), "lorawan");
    assert_eq!(SchemaProfile::WifiMqttDevice.as_str(), "wifi_mqtt");
    assert_eq!(
        SchemaProfile::from_nvs_str("lorawan"),
        Some(SchemaProfile::LorawanFieldDevice)
    );
    assert!(SchemaProfile::from_nvs_str("unknown").is_none());

    // ProvisioningState machine.
    let s0 = ProvisioningState::AwaitingSubmission;
    let s1 = s0.apply(ProvisioningInput::ValidSubmission).unwrap();
    assert_eq!(s1, ProvisioningState::Persisting);
    let s2 = s1.apply(ProvisioningInput::PersistOk).unwrap();
    assert_eq!(s2, ProvisioningState::Committed);
    assert_eq!(s2.as_str(), "committed");

    // InvalidTransition is reachable.
    let bad = ProvisioningState::Committed.apply(ProvisioningInput::ValidSubmission);
    assert!(bad.is_err());
    let _: InvalidTransition = bad.unwrap_err();

    // ProvisioningInput variants are reachable.
    let _: ProvisioningInput = ProvisioningInput::ValidSubmission;
    let _: ProvisioningInput = ProvisioningInput::PersistFailed;
    let _: ProvisioningInput = ProvisioningInput::FactoryReset;

    // derive_softap_ssid.
    let mac = [0xDE, 0xAD, 0xBE, 0xEF, 0xAB, 0x12];
    let ssid = derive_softap_ssid("RustyFarian", &mac);
    assert!(ssid.as_str().starts_with("RustyFarian-"));
    assert!(ssid.as_str().ends_with("AB12"));

    // parse_form returns a typed result; field-error types are reachable from it.
    let body = "wifi_ssid=MyNet&wifi_password=hunter2&\
                dev_eui=0000000000000001&join_eui=0000000000000002&\
                app_key=00000000000000000000000000000003&\
                ota_url=http%3A%2F%2Fota.local%2Ffirmware.bin&device_name=node01";
    let result = parse_form(body, SchemaProfile::LorawanFieldDevice);
    // Either Ok(ProvisioningConfig) or Err(FieldErrors) — both types must be reachable.
    fn _uses_config(_: &ProvisioningConfig) {}
    fn _uses_field_errors(_: &FieldErrors) {}
    match &result {
        Ok(cfg) => _uses_config(cfg),
        Err(errs) => _uses_field_errors(errs),
    }

    // Field variants are reachable.
    let _: Field = Field::WifiSsid;
    let _: Field = Field::OtaUrl;
    assert_eq!(Field::WifiSsid.form_name(), "wifi_ssid");

    // ValidationError variants are reachable.
    let _: ValidationError = ValidationError::Missing;
    let _: ValidationError = ValidationError::TooLong { max: 32 };

    // FieldError is a struct (field + error), not an enum.
    let _fe = FieldError {
        field: Field::WifiSsid,
        error: ValidationError::Missing,
    };

    // ExtraField struct is reachable (name-import check).
    fn _uses_extra_field(_: &ExtraField) {}

    // LoraFields and MqttFields are exported (path-import check).
    fn _uses_lora_fields(_: &LoraFields) {}
    fn _uses_mqtt_fields(_: &MqttFields) {}
}
