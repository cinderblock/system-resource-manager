//! Several real daemons talking over loopback, plus small fakes for the devices they
//! probe. Nothing here leaves the machine.

mod common;

use common::Env;
use serde_json::Value;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::Stdio;
use std::thread;
use std::time::Duration;

const TOKEN: &str = "test-token";

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// A node that serves on loopback, with a token shared by the test's fleet.
fn node(port: u16, name: &str, extra: &str) -> Env {
    let env = Env::new(&format!(
        r#"
        [node]
        name = "{name}"
        [resources.cpu]
        capacity = 2
        [network]
        enabled = true
        listen = "127.0.0.1:{port}"
        mdns = false
        poll_secs = 1
        {extra}
        "#
    ));
    std::fs::write(env.home.join("network-token"), TOKEN).unwrap();
    env
}

/// What a node with no network config looks like.
fn watcher(extra: &str) -> Env {
    Env::new(&format!("[network]\npoll_secs = 1\n{extra}\n"))
}

fn peer_config(port: u16) -> String {
    format!("[[peer]]\nurl = \"http://127.0.0.1:{port}\"\ntoken = \"{TOKEN}\"\n")
}

fn get(port: u16, path: &str, headers: &[&str]) -> (u16, String) {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    let mut request = format!("GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n");
    for header in headers {
        request.push_str(header);
        request.push_str("\r\n");
    }
    request.push_str("\r\n");
    stream.write_all(request.as_bytes()).unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    let status = response.split_whitespace().nth(1).unwrap().parse().unwrap();
    let body = response.split_once("\r\n\r\n").map(|(_, b)| b.to_string());
    (status, body.unwrap_or_default())
}

fn wait_served(port: u16) {
    for _ in 0..200 {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return;
        }
        thread::sleep(Duration::from_millis(25));
    }
    panic!("nothing listening on {port}");
}

/// Answer every request on `port` with a fixed response.
fn fake_device(status_line: &str, body: &str) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let response = format!(
        "HTTP/1.1 {status_line}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let response = response.clone();
            thread::spawn(move || {
                let mut stream = stream;
                let mut request = [0u8; 2048];
                let _ = stream.read(&mut request);
                let _ = stream.write_all(response.as_bytes());
            });
        }
    });
    port
}

fn asset<'a>(status: &'a Value, id: &str) -> &'a Value {
    status["assets"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["id"] == id)
        .unwrap_or_else(|| panic!("no asset {id} in {status:#}"))
}

fn has_asset(status: &Value, id: &str) -> bool {
    status["assets"]
        .as_array()
        .unwrap()
        .iter()
        .any(|a| a["id"] == id)
}

#[test]
fn the_network_face_is_off_unless_asked_for() {
    let port = free_port();
    let env = Env::new(&format!("[network]\nlisten = \"127.0.0.1:{port}\"\n"));
    env.status();
    thread::sleep(Duration::from_millis(300));
    assert!(TcpStream::connect(("127.0.0.1", port)).is_err());
    assert!(!env.home.join("network-token").exists());
}

#[test]
fn a_token_is_required_for_everything() {
    let port = free_port();
    let env = node(port, "a", "");
    env.status();
    wait_served(port);

    assert_eq!(get(port, "/v1/snapshot", &[]).0, 401);
    assert_eq!(get(port, "/metrics", &[]).0, 401);
    assert_eq!(get(port, "/v1/snapshot", &["X-Srm-Token: wrong"]).0, 401);
    assert_eq!(
        get(port, "/v1/snapshot", &["X-Srm-Token: test-token"]).0,
        200
    );
    assert_eq!(
        get(port, "/metrics", &["Authorization: Bearer test-token"]).0,
        200
    );
}

#[test]
fn the_network_face_is_read_only() {
    let port = free_port();
    let env = node(port, "a", "");
    env.status();
    wait_served(port);

    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    let request = "POST /v1/snapshot HTTP/1.1\r\nHost: x\r\nX-Srm-Token: test-token\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
    stream.write_all(request.as_bytes()).unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    assert!(response.starts_with("HTTP/1.1 405"), "{response}");
}

#[test]
fn metrics_describe_the_pools() {
    let port = free_port();
    let env = node(port, "a", "");
    env.status();
    wait_served(port);
    let (_, body) = get(port, "/metrics", &["X-Srm-Token: test-token"]);
    assert!(
        body.contains(r#"srm_pool_capacity{pool="cpu"} 2"#),
        "{body}"
    );
    assert!(body.contains("srm_cpu_percent"), "{body}");
}

#[test]
fn one_node_sees_the_work_on_another() {
    let port = free_port();
    let a = node(port, "alpha", "");
    let mut build = a.start(&[
        "run",
        "--slots",
        "2",
        "--label",
        "remote build",
        "--",
        "node",
        "-e",
        "setTimeout(()=>{},15000)",
    ]);
    a.wait_for_pool("the build", |p| p["used"] == 2);
    wait_served(port);

    let b = watcher(&peer_config(port));
    let status = b.wait_for("the peer", |s| s["peers"][0]["state"] == "online");
    let remote = &status["peers"][0]["snapshot"];
    assert_eq!(remote["node"]["name"], "alpha");
    assert_eq!(remote["pools"][0]["holders"][0]["label"], "remote build");

    build.kill().unwrap();
    build.wait().unwrap();
}

#[test]
fn a_wrong_token_is_told_apart_from_a_dead_peer() {
    let port = free_port();
    let a = node(port, "alpha", "");
    a.status();
    wait_served(port);

    let dead = free_port();
    let b = watcher(&format!(
        "[[peer]]\nurl = \"http://127.0.0.1:{port}\"\ntoken = \"wrong\"\n[[peer]]\nurl = \"http://127.0.0.1:{dead}\"\n"
    ));
    let status = b.wait_for("both peers", |s| {
        s["peers"]
            .as_array()
            .unwrap()
            .iter()
            .all(|p| p["state"] != "online")
            && s["peers"].as_array().unwrap().len() == 2
    });
    let states: Vec<&str> = status["peers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["state"].as_str().unwrap())
        .collect();
    assert!(states.contains(&"unauthorized"), "{states:?}");
    assert!(states.contains(&"unreachable"), "{states:?}");
}

#[test]
fn a_peer_that_goes_away_stays_listed_with_its_last_state() {
    let port = free_port();
    let a = node(port, "alpha", "");
    a.status();
    wait_served(port);

    let b = watcher(&peer_config(port));
    b.wait_for("the peer", |s| s["peers"][0]["state"] == "online");

    a.run(&["daemon", "stop"]);
    let status = b.wait_for("the peer lost", |s| s["peers"][0]["state"] == "unreachable");
    assert_eq!(status["peers"][0]["snapshot"]["node"]["name"], "alpha");
    assert!(status["peers"][0]["last_seen"].is_i64());
}

#[test]
fn a_shared_peer_list_doesnt_list_ourselves() {
    let port = free_port();
    let a = node(port, "alpha", &peer_config(port));
    a.status();
    wait_served(port);
    // Two polls: long enough to have looked at itself.
    std::thread::sleep(Duration::from_secs(2));
    let status = a.status();
    assert_eq!(status["peers"].as_array().unwrap().len(), 0, "{status:#}");
}

#[test]
fn peers_added_by_reload_need_no_restart() {
    let port = free_port();
    let a = node(port, "alpha", "");
    a.status();
    wait_served(port);

    let b = watcher("");
    b.status();
    std::fs::write(
        b.home.join("config.toml"),
        format!("[network]\npoll_secs = 1\n{}", peer_config(port)),
    )
    .unwrap();
    b.run(&["daemon", "reload"]);
    b.wait_for("the new peer", |s| s["peers"][0]["state"] == "online");
}

#[test]
fn tcp_probes_tell_up_from_down() {
    let open = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = open.local_addr().unwrap().port();
    let closed = free_port();
    let env = watcher(&format!(
        r#"
        [[asset]]
        id = "cam-up"
        name = "Warehouse camera"
        kind = "camera"
        site = "TSL"
        probe = {{ type = "tcp", host = "127.0.0.1", port = {port} }}
        [[asset]]
        id = "cam-down"
        kind = "camera"
        probe = {{ type = "tcp", host = "127.0.0.1", port = {closed} }}
        "#
    ));
    let status = env.wait_for("both probes", |s| {
        has_asset(s, "cam-up") && has_asset(s, "cam-down")
    });
    let up = asset(&status, "cam-up");
    assert_eq!(up["health"], "up");
    assert_eq!(up["name"], "Warehouse camera");
    assert_eq!(up["kind"], "camera");
    assert_eq!(up["site"], "TSL");
    assert!(up["latency_ms"].is_f64());
    let down = asset(&status, "cam-down");
    assert_eq!(down["health"], "down");
    assert!(down["detail"].is_string());
    assert_eq!(down["name"], "cam-down", "falls back to the id");
}

#[test]
fn http_probes_count_refusal_as_alive_and_errors_as_degraded() {
    let fine = fake_device("200 OK", "ok");
    let locked = fake_device("401 Unauthorized", "");
    let broken = fake_device("503 Service Unavailable", "");
    let env = watcher(&format!(
        r#"
        [[asset]]
        id = "fine"
        probe = {{ type = "http", url = "http://127.0.0.1:{fine}/" }}
        [[asset]]
        id = "nas"
        kind = "nas"
        probe = {{ type = "http", url = "http://127.0.0.1:{locked}/" }}
        [[asset]]
        id = "broken"
        probe = {{ type = "http", url = "http://127.0.0.1:{broken}/" }}
        "#
    ));
    let status = env.wait_for("the probes", |s| has_asset(s, "broken"));
    assert_eq!(asset(&status, "fine")["health"], "up");
    assert_eq!(asset(&status, "nas")["health"], "up");
    assert_eq!(asset(&status, "broken")["health"], "degraded");
    assert_eq!(asset(&status, "broken")["detail"], "HTTP 503");
}

#[test]
fn node_exporter_probes_read_a_server_with_nothing_installed() {
    let body = "node_memory_MemAvailable_bytes 1e+09\nnode_memory_MemTotal_bytes 1e+10\n\
                node_filesystem_avail_bytes{mountpoint=\"/\"} 5e+09\n\
                node_filesystem_size_bytes{mountpoint=\"/\"} 1e+11\nnode_load1 0.5\n";
    let port = fake_device("200 OK", body);
    let env = watcher(&format!(
        r#"
        [[asset]]
        id = "storage"
        kind = "server"
        probe = {{ type = "node_exporter", url = "http://127.0.0.1:{port}/metrics" }}
        "#
    ));
    let status = env.wait_for("the probe", |s| has_asset(s, "storage"));
    let server = asset(&status, "storage");
    // 90% memory is fine, 95% disk isn't.
    assert_eq!(server["health"], "degraded");
    assert_eq!(server["detail"], "disk 95% full");
    assert_eq!(server["metrics"]["memory_percent"], 90.0);
    assert_eq!(server["metrics"]["load1"], 0.5);
}

#[test]
fn an_asset_watched_from_two_nodes_appears_once_and_up_wins() {
    let device = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = device.local_addr().unwrap().port();
    let closed = free_port();

    // Alpha can't reach it (the wrong port stands in for a broken path); beta can.
    let beta_port = free_port();
    let beta = node(
        beta_port,
        "beta",
        &format!(
            "[[asset]]\nid = \"nas\"\nprobe = {{ type = \"tcp\", host = \"127.0.0.1\", port = {port} }}\n"
        ),
    );
    beta.status();
    wait_served(beta_port);

    let alpha = watcher(&format!(
        "{}[[asset]]\nid = \"nas\"\nprobe = {{ type = \"tcp\", host = \"127.0.0.1\", port = {closed} }}\n",
        peer_config(beta_port)
    ));
    let status = alpha.wait_for("beta's view", |s| {
        s["peers"][0]["state"] == "online"
            && !s["peers"][0]["snapshot"]["assets"]
                .as_array()
                .unwrap()
                .is_empty()
    });
    assert_eq!(status["assets"].as_array().unwrap().len(), 1);
    assert_eq!(asset(&status, "nas")["health"], "up");
    assert_eq!(asset(&status, "nas")["observer"], "beta");
}

#[test]
fn a_status_print_covers_the_whole_fleet() {
    let port = free_port();
    let a = node(port, "alpha", "");
    a.status();
    wait_served(port);
    let device = TcpListener::bind("127.0.0.1:0").unwrap();
    let device_port = device.local_addr().unwrap().port();
    let b = watcher(&format!(
        "{}[[asset]]\nid = \"cam\"\nname = \"Front camera\"\nprobe = {{ type = \"tcp\", host = \"127.0.0.1\", port = {device_port} }}\n",
        peer_config(port)
    ));
    b.wait_for("everything", |s| {
        s["peers"][0]["state"] == "online" && has_asset(s, "cam")
    });
    let output = b
        .ctl()
        .args(["status"])
        .stdout(Stdio::piped())
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("alpha: CPU"), "{text}");
    assert!(text.contains("Front camera"), "{text}");
}
