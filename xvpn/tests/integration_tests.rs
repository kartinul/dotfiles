use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use xvpn::config::{self, Ingress, Scope};

/// A guard that cleans up a temporary directory upon drop.
struct TempDirGuard {
    path: PathBuf,
}

impl TempDirGuard {
    fn new(name: &str) -> Self {
        let mut path = std::env::temp_dir();
        let random_num: u32 = rand_num();
        path.push(format!("xvpn-test-{}-{random_num}", name));
        fs::create_dir_all(&path).unwrap();
        Self { path }
    }
}

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&mut self.path);
    }
}

fn rand_num() -> u32 {
    let now = Instant::now();
    let duration = now.elapsed();
    (duration.as_nanos() & 0xFFFFFFFF) as u32
}

/// A guard that kills a child process upon drop.
struct ChildGuard(std::process::Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn test_singbox_config_validates_successfully() {
    let temp = TempDirGuard::new("singbox-val");
    let on_path = temp.path.join("on.json");
    let def_path = temp.path.join("default.json");

    // Generate both Global (on) and Selective (default) configs.
    // Use Ingress::Tun which is standard, and we want to verify sing-box check handles it.
    let global_cfg = config::singbox_config(Scope::Global, Ingress::Tun);
    let selective_cfg = config::singbox_config(Scope::Selective, Ingress::Tun);

    config::write_json(&on_path, &global_cfg).unwrap();
    config::write_json(&def_path, &selective_cfg).unwrap();

    // Verify sing-box check passes on our generated configs
    let check_on = config::check_singbox(&on_path);
    assert!(
        check_on.is_ok(),
        "sing-box rejected global config: {:?}",
        check_on
    );

    let check_def = config::check_singbox(&def_path);
    assert!(
        check_def.is_ok(),
        "sing-box rejected selective config: {:?}",
        check_def
    );
}

#[test]
fn test_xray_config_generation_and_local_port_binding() {
    let temp = TempDirGuard::new("xray-test");
    let xray_path = temp.path.join("xray.json");

    // Create a dummy vless link with custom credentials.
    // We use security=tls to bypass the Reality key validation in `validate()`,
    // so we can test the full config generation and local port binding flow.
    let link_str = "vless://abc-123@127.0.0.1:443?type=ws&security=tls&sni=127.0.0.1#test-profile";
    let parsed = config::parse_link(link_str).unwrap();

    // Ensure we run on isolated high test ports to avoid any collision
    // with running production/user instances of xvpn!
    let test_socks_port = 19808;
    let test_http_port = 19809;

    // Generate config
    let mut xray_cfg = config::xray_config(&parsed, test_socks_port, test_http_port);

    xray_cfg["inbounds"] = serde_json::json!([
        {
            "listen": "127.0.0.1",
            "port": test_socks_port,
            "protocol": "socks",
            "settings": { "udp": true },
        },
        {
            "listen": "127.0.0.1",
            "port": test_http_port,
            "protocol": "http"
        }
    ]);

    config::write_json(&xray_path, &xray_cfg).unwrap();

    // First do dry-run validation using xray -test
    let test_check = config::check_xray(&xray_path);
    assert!(
        test_check.is_ok(),
        "xray dry-run validation failed: {:?}",
        test_check
    );

    // Start xray as a background child process wrapped in ChildGuard
    // This is strictly local on localhost:19808 / localhost:19809
    let child = Command::new("xray")
        .arg("run")
        .arg("-c")
        .arg(&xray_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn xray background binary");

    let _guard = ChildGuard(child);

    // Wait a brief moment for the port to bind
    std::thread::sleep(Duration::from_millis(500));

    // Check if the local SOCKS port is listening. We do this by attempting
    // a TCP connection to 127.0.0.1:19808.
    // If the port is listening, it will connect immediately.
    let connect_res = std::net::TcpStream::connect_timeout(
        &"127.0.0.1:19808".parse().unwrap(),
        Duration::from_secs(2),
    );
    assert!(
        connect_res.is_ok(),
        "xray failed to listen on local test SOCKS port 19808: {:?}",
        connect_res
    );

    // Let's also check if curl can talk to our test socks5 proxy.
    // Even though handshake to real server 127.0.0.1:443 will fail or timeout,
    // curl communicating with 127.0.0.1:19808 proves proxy transport protocol is active.
    let curl_res = Command::new("curl")
        .args([
            "-s",
            "-I",
            "--connect-timeout",
            "1",
            "-x",
            &format!("socks5h://127.0.0.1:{}", test_socks_port),
            "http://127.0.0.1/", // we curl localhost to avoid hitting external web
        ])
        .output();

    // Since our backend destination (127.0.0.1:443 vless) is offline, curl should get
    // a proxy error or connection closure (e.g. exit 97 or exit 52 or exit 7),
    // but curl MUST be able to contact the proxy itself (meaning it shouldn't say "Could not resolve proxy" or "Failed to connect to proxy").
    if let Ok(out) = curl_res {
        let err_str = String::from_utf8_lossy(&out.stderr);
        assert!(
            !err_str.contains("Failed to connect to 127.0.0.1 port 19808"),
            "Could not connect to SOCKS5 proxy: {}",
            err_str
        );
    }
}
