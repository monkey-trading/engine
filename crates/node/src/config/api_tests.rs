use super::*;

use crate::test_support::DEV_CONFIG as DEV;

#[test]
fn a_signed_stream_heartbeat_is_five_seconds_unless_set_and_never_under_one() {
    let dev = std::fs::read_to_string(DEV).unwrap();
    let interval = |text: &str| {
        let cfg: NodeConfig = toml::from_str(text).unwrap();
        cfg.validate().map(|()| cfg.api.heartbeat_interval)
    };
    let unset = dev.replace("heartbeat_interval = \"5s\"\n", "");
    assert!(!unset.contains("heartbeat_interval"));
    assert_eq!(interval(&unset).unwrap(), Duration::from_secs(5));
    let set = |value: &str| dev.replace("heartbeat_interval = \"5s\"", &format!("heartbeat_interval = \"{value}\""));
    assert_eq!(interval(&set("1s")).unwrap(), Duration::from_secs(1));
    for short in ["999ms", "0s"] {
        let err = interval(&set(short)).unwrap_err();
        assert!(format!("{err:#}").contains("api.heartbeat_interval must be at least 1s"), "{short}: {err:#}");
    }
}
