pub mod cli;
pub mod commands;
pub mod config;
pub mod network;
pub mod output;
pub mod profiles;
pub mod supervisor;

/// Errors are plain strings: every failure is printed for a human, usually
/// alongside the stderr of whatever tool produced it.
pub type Error = String;

pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_parse_link_valid() {
        let link_str = "vless://abc-123@myhost.com:443?type=ws&security=reality&pbk=1234567890123456789012345678901234567890123&sid=ab12cd34&path=%2Ftest%#myprofile";
        let parsed = config::parse_link(link_str).unwrap();
        assert_eq!(parsed.id, "abc-123");
        assert_eq!(parsed.host, "myhost.com");
        assert_eq!(parsed.port, 443);
        assert_eq!(parsed.transport, config::Transport::Ws);
        assert_eq!(parsed.security, config::Security::Reality);
        assert_eq!(parsed.param("path"), Some("/test%"));
    }

    #[test]
    fn test_normalize_app() {
        assert_eq!(network::normalize_app("Telegram.app").unwrap(), "Telegram");
        assert_eq!(
            network::normalize_app("/Applications/Telegram.app/").unwrap(),
            "Telegram"
        );
        assert_eq!(network::normalize_app("Safari").unwrap(), "Safari");
        assert!(network::normalize_app("").is_err());
    }

    #[test]
    fn test_parse_link_strips_fragment() {
        let link_str = "vless://abc-123@myhost.com:443?type=tcp&security=reality&pbk=1234567890123456789012345678901234567890123&sid=ab12cd34#tag";
        let parsed = config::parse_link(link_str).unwrap();
        assert_eq!(parsed.param("sid"), Some("ab12cd34"));
        assert_eq!(
            parsed.param("pbk"),
            Some("1234567890123456789012345678901234567890123")
        );
    }

    #[test]
    fn test_parse_link_invalid() {
        let link_str =
            "vless://abc-123@myhost.com:443?type=ws&security=reality&pbk=short_pbk&sid=ab12cd34";
        assert!(config::parse_link(link_str).is_err());
    }

    #[test]
    fn test_normalize_site() {
        assert_eq!(
            network::normalize_site("www.netflix.com").unwrap(),
            "netflix.com"
        );
        assert_eq!(
            network::normalize_site("NETFLIX.COM.").unwrap(),
            "netflix.com"
        );
        assert!(network::normalize_site("http://netflix.com").is_err());
        assert!(network::normalize_site("netflix.com/path").is_err());
    }

    #[test]
    fn test_migrate_singbox() {
        let existing = json!({
            "route": {
                "rules": [
                    { "domain_suffix": ["netflix.com"], "outbound": "xray" },
                    { "process_name": ["curl"], "outbound": "direct" }
                ]
            }
        });
        let migrated =
            config::migrate_singbox(&existing, config::Scope::Selective, config::Ingress::Tun);
        let rules = migrated["route"]["rules"].as_array().unwrap();
        // The user rule netflix.com should survive
        let has_user_rule = rules.iter().any(|r| {
            r.get("domain_suffix")
                .and_then(|v| v.as_array())
                .map(|arr| arr.iter().any(|d| d.as_str() == Some("netflix.com")))
                .unwrap_or(false)
        });
        assert!(has_user_rule);
    }

    #[test]
    fn test_is_running_detection() {
        let tmp = std::env::temp_dir().join(format!("xvpn-test-lock-{}", std::process::id()));
        std::env::set_var("XVPN_LOCK", &tmp);

        // Before any lock is held
        assert!(!supervisor::is_running());

        // Hold the lock
        let file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&tmp)
            .unwrap();
        let rc = unsafe {
            libc::flock(
                std::os::unix::io::AsRawFd::as_raw_fd(&file),
                libc::LOCK_EX | libc::LOCK_NB,
            )
        };
        assert_eq!(rc, 0);

        // Now supervisor::is_running() must return true
        assert!(supervisor::is_running());

        // Release the lock
        unsafe {
            libc::flock(std::os::unix::io::AsRawFd::as_raw_fd(&file), libc::LOCK_UN);
        }
        assert!(!supervisor::is_running());

        let _ = std::fs::remove_file(&tmp);
        std::env::remove_var("XVPN_LOCK");
    }

    #[test]
    fn test_plan_key_tracks_mode_mtime() {
        let dir = std::env::temp_dir().join(format!("xvpn-test-dir-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let mode_file = dir.join(config::MODE);
        std::fs::write(&mode_file, "off\n").unwrap();

        let mut cache = None;
        let plan1 = supervisor::plan(&dir, &mut cache);

        // Sleep briefly to ensure mtime timestamp differences if resolution is coarse
        std::thread::sleep(std::time::Duration::from_millis(1100));
        std::fs::write(&mode_file, "off\n").unwrap();

        let plan2 = supervisor::plan(&dir, &mut cache);
        assert_ne!(
            plan1.key, plan2.key,
            "key must change when mode file mtime changes"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
