use anyhow::{bail, Result};
use interfaces::Interface;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::net::IpAddr;
use std::os::unix::process::CommandExt;
use std::time::Duration;
use std::{env, path, process::Command, str};
use sysctl::Sysctl;
use tokio::time::{sleep, interval};
use tokio::sync::mpsc;

mod config;

#[derive(Debug, Deserialize)]
struct ServerList {
    groups: HashMap<String, Vec<GroupDetails>>,
    regions: Vec<Region>,
}

#[derive(Debug, Deserialize)]
struct GroupDetails {
    ports: Vec<i32>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Region {
    id: String,
    name: String,
    // dns: String,
    port_forward: bool,
    offline: bool,
    servers: HashMap<String, Vec<ServerDetails>>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ServerDetails {
    ip: IpAddr,
    cn: String,
}

#[derive(Debug, Deserialize)]
struct Token {
    token: String,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
struct Signature {
    payload: String,
    signature: String,
    status: String,
    message: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Payload {
    // token: String,
    port: i32,
    expires_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Deserialize)]
struct BindPort {
    status: String,
    message: String,
}

const CONFIG_PATH: &str = "/config";
const HEALTH_CHECK_INTERVAL_SECS: u64 = 60;
const MAX_RECONNECT_ATTEMPTS: u32 = 5;
const RECONNECT_DELAY_SECS: u64 = 5;

#[derive(Clone)]
struct VpnState {
    region_id: String,
    token: String,
    server_cn: String,
    server_ip: IpAddr,
    port: i32,
    conf_api: String,
    forward_port: bool,
    payload_port: Option<i32>,
    sig: Option<Signature>,
}

async fn check_vpn_connection(expected_ip: &str) -> Result<bool> {
    let current_ip = reqwest::Client::new()
        .get("https://icanhazip.com")
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .ok();

    let current_ip = match current_ip {
        Some(response) => response.text().await.ok(),
        None => None,
    };

    match current_ip {
        Some(ip) => {
            if ip.trim() == expected_ip.trim() {
                Ok(true)
            } else {
                println!("[WARN] IP changed! Expected: {}, Current: {}", expected_ip, ip);
                Ok(false)
            }
        }
        None => {
            println!("[WARN] Could not fetch current IP - connection may be down");
            Ok(false)
        }
    }
}

async fn check_wireguard_interface() -> bool {
    match Interface::get_by_name("wg0") {
        Ok(Some(iface)) => iface.is_up(),
        Ok(None) => {
            println!("[WARN] WireGuard interface wg0 not found");
            false
        }
        Err(e) => {
            println!("[WARN] Error checking wg0 interface: {}", e);
            false
        }
    }
}

fn stop_wireguard() {
    println!("[INFO] Stopping WireGuard interface");
    Command::new("wg-quick")
        .args(["down", &format!("{}/wg0.conf", CONFIG_PATH)])
        .output()
        .ok();
}

async fn reconnect_vpn(state: &VpnState) -> Result<String> {
    println!("[INFO] Attempting VPN reconnection...");

    stop_wireguard();
    sleep(Duration::from_secs(2)).await;

    let data = reqwest::Client::new()
        .get("https://raw.githubusercontent.com/pia-foss/manual-connections/master/ca.rsa.4096.crt")
        .send()
        .await?
        .bytes()
        .await?;

    let pia_client = reqwest::Client::builder()
        .resolve(
            &state.server_cn,
            format!("{}:{}", state.server_ip, state.port).parse().unwrap(),
        )
        .add_root_certificate(reqwest::Certificate::from_pem(&data)?)
        .build()?;

    let conf = config::Config::new(&state.server_cn, &state.token, state.port, &pia_client)
        .await?;
    conf.write(format!("{}/wg0.conf", CONFIG_PATH).parse()?)
        .await;

    Command::new("wg-quick")
        .args(["up", &format!("{}/wg0.conf", CONFIG_PATH)])
        .status()
        .expect("[ERROR] Wireguard failed to start");

    for i in 1..=30 {
        if check_wireguard_interface().await {
            println!("[INFO] WireGuard interface is up");
            break;
        }
        if i % 5 == 0 {
            println!("[INFO] Waiting for WireGuard interface... ({}/30)", i);
        }
        sleep(Duration::from_secs(1)).await;
    }

    sleep(Duration::from_secs(3)).await;

    let new_ip = reqwest::Client::new()
        .get("https://icanhazip.com")
        .timeout(Duration::from_secs(10))
        .send()
        .await?
        .text()
        .await?
        .trim_end()
        .to_owned();

    println!("[INFO] Reconnected to PIA. New IP: {}", new_ip);
    Ok(new_ip)
}

#[tokio::main]
async fn main() -> Result<()> {
    println!("----------------------------------------------------------------------\nENVIRONMENT\n----------------------------------------------------------------------");
    for (key, value) in env::vars() {
        println!("{key}: {value}");
    }
    println!("----------------------------------------------------------------------");

    if let Ok(Some(_)) = Interface::get_by_name("docker0") {
        bail!("[ERROR] Docker network mode 'host' is not supported")
    }
    if sysctl::Ctl::new("net.ipv4.conf.all.src_valid_mark")?.value_string()? != "1" {
        bail!("[ERROR] net.ipv4.conf.all.src_valid_mark is not set to 1")
    }

    if path::Path::new(&format!("{}/wg0.conf", CONFIG_PATH)).exists() {
        Command::new("wg-quick")
            .args(["down", &format!("{}/wg0.conf", CONFIG_PATH)])
            .output()
            .ok();
        println!("[INFO] Stopped previous wireguard interface")
    }

    println!("[INFO] Removing src_valid_mark=1 from wg-quick");
    Command::new("sed")
        .args([
            "-i",
            r#"/net\.ipv4\.conf\.all\.src_valid_mark/d"#,
            "/usr/bin/wg-quick",
        ])
        .spawn()?;

    let region_id =
        env::var("PIA_REGION_ID").expect("[ERROR] Missing PIA_REGION_ID in environment variables");
    println!("[INFO] Fetching PIA server list");

    let list: ServerList = {
        let list_raw = reqwest::Client::new()
            .get("https://serverlist.piaservers.net/vpninfo/servers/v6")
            .send()
            .await?
            .text()
            .await?;
        serde_json::from_str(list_raw.split_once('\n').unwrap().0)?
    };
    let region = list
        .regions
        .iter()
        .find(|r| r.id == region_id)
        .expect("[ERROR] Could not locate region");
    if region.offline {
        bail!("[ERROR] Selected server is offline")
    }

    let forward_port = env::var("PORT_FORWARDING")
        .unwrap_or_else(|_| "false".to_string())
        .parse::<bool>()
        .unwrap();

    if !region.port_forward && forward_port {
        bail!("[ERROR] Selected server doesn't support port forwarding but PORT_FORWARDING is set to true")
    }

    println!("[INFO] Region {} selected", region.name);

    let mut login = HashMap::new();
    login.insert(
        "username",
        env::var("PIA_USER").expect("[ERROR] Missing PIA_USER in environment variables"),
    );
    login.insert(
        "password",
        env::var("PIA_PASS").expect("[ERROR] Missing PIA_PASS in environment variables"),
    );
    let token: Token = reqwest::Client::new()
        .post("https://www.privateinternetaccess.com/api/client/v2/token")
        .form(&login)
        .send()
        .await?
        .json()
        .await
        .expect("[ERROR] Failed to login");
    println!("[INFO] Successfully logged in and created token");

    let data = reqwest::Client::new()
        .get("https://raw.githubusercontent.com/pia-foss/manual-connections/master/ca.rsa.4096.crt")
        .send()
        .await?
        .bytes()
        .await?;
    println!("[INFO] Fetched PIA certificate");

    let server = region.servers.get("wg").unwrap().first().unwrap();
    let port = list
        .groups
        .get("wg")
        .unwrap()
        .first()
        .unwrap()
        .ports
        .first()
        .unwrap();

    let pia_client = reqwest::Client::builder()
        .resolve(
            &server.cn,
            format!("{}:{}", server.ip, port).parse().unwrap(),
        )
        .add_root_certificate(reqwest::Certificate::from_pem(&data)?)
        .build()?;

    let conf = config::Config::new(&server.cn, &token.token, *port, &pia_client)
        .await
        .expect("[ERROR] Failed to generate wireguard configuration");
    let conf_api = conf.api.clone();
    conf.write(format!("{}/wg0.conf", CONFIG_PATH).parse()?)
        .await;

    let old_ip = reqwest::Client::new()
        .get("https://icanhazip.com")
        .send()
        .await?
        .text()
        .await?;

    Command::new("wg-quick")
        .args(["up", &format!("{}/wg0.conf", CONFIG_PATH)])
        .status()
        .expect("[ERROR] Wireguard failed to start");

    loop {
        println!("[INFO] Waiting for wireguard interface to go up");
        let interface =
            Interface::get_by_name("wg0")?.expect("[ERROR] failed to find wireguard interface");
        if interface.is_up() {
            break;
        }
        sleep(Duration::from_secs(1)).await;
    }
    println!("[INFO] Wireguard interface up");

    let default_route = String::from_utf8_lossy(
        &Command::new("ip")
            .args(["-o", "-4", "route", "show", "to", "default"])
            .output()
            .unwrap()
            .stdout,
    )
    .into_owned();

    let interface_name = default_route
        .split(' ')
        .nth(4)
        .expect("[ERROR] Failed to find the default interface");

    let gateway = default_route.split(' ').nth(2).unwrap();

    let network_cidr = {
        let inet_cmd = String::from_utf8_lossy(
            &Command::new("ip")
                .args(["-o", "-f", "inet", "addr", "show", interface_name])
                .output()
                .unwrap()
                .stdout,
        )
        .into_owned();
        let cidr_re = Regex::new(r#"(?:[0-9]{1,3}\.){3}[0-9]{1,3}/\d{1,2}"#).unwrap();
        let cidr = cidr_re
            .find(&inet_cmd)
            .expect("Failed to find inet CIDR")
            .as_str();
        let ipcalc_cmd =
            String::from_utf8_lossy(&Command::new("ipcalc").args([cidr]).output().unwrap().stdout)
                .into_owned();
        cidr_re
            .find(ipcalc_cmd.split('\n').nth(1).unwrap())
            .expect("[ERROR] Failed to calculate inet CIDR")
            .as_str()
            .to_owned()
    };

    if let Ok(networks) = env::var("VPN_LAN_NETWORKS") {
        for lan_network in networks.split(',') {
            println!("[INFO] Adding {lan_network} as route via interface {interface_name}");
            Command::new("ip")
                .args([
                    "route",
                    "add",
                    lan_network,
                    "via",
                    gateway,
                    "dev",
                    interface_name,
                ])
                .spawn()
                .expect("[ERROR] Failed to add VPN_LAN_NETWORKS");
        }
    }

    let ipt = iptables::new(false).unwrap();

    ipt.set_policy("filter", "FORWARD", "DROP").unwrap();
    ipt.set_policy("filter", "INPUT", "DROP").unwrap();
    ipt.append("filter", "INPUT", "-i wg0 -p udp -j ACCEPT")
        .unwrap();
    ipt.append("filter", "INPUT", "-i wg0 -p tcp -j ACCEPT")
        .unwrap();
    ipt.append(
        "filter",
        "INPUT",
        &format!("-s {network_cidr} -d {network_cidr} -j ACCEPT"),
    )
    .unwrap();
    ipt.append(
        "filter",
        "INPUT",
        &format!("-i {interface_name} -p udp --sport {port} -j ACCEPT"),
    )
    .unwrap();
    ipt.append(
        "filter",
        "INPUT",
        "-p icmp --icmp-type echo-reply -j ACCEPT",
    )
    .unwrap();
    ipt.append("filter", "INPUT", "-i lo -j ACCEPT").unwrap();
    ipt.set_policy("filter", "OUTPUT", "DROP").unwrap();
    ipt.append("filter", "OUTPUT", "-o wg0 -p udp -j ACCEPT")
        .unwrap();
    ipt.append("filter", "OUTPUT", "-o wg0 -p tcp -j ACCEPT")
        .unwrap();
    ipt.append(
        "filter",
        "OUTPUT",
        &format!("-s {network_cidr} -d {network_cidr} -j ACCEPT"),
    )
    .unwrap();
    ipt.append(
        "filter",
        "OUTPUT",
        &format!("-o {interface_name} -p udp --dport {port} -j ACCEPT"),
    )
    .unwrap();
    ipt.append(
        "filter",
        "OUTPUT",
        "-p icmp --icmp-type echo-request -j ACCEPT",
    )
    .unwrap();
    ipt.append("filter", "OUTPUT", "-o lo -j ACCEPT").unwrap();

    if let Ok(ports) = env::var("BYPASS_PORTS") {
        for port in ports.split(",") {
            let (port_num, protocol) = port.split_once("/").unwrap();
            println!("[INFO] Bypassing {port}");
            ipt.insert(
                "filter",
                "INPUT",
                &format!("-i wg0 -p {protocol} --dport {port_num} -j DROP"),
                1,
            )
            .unwrap();
            ipt.append(
                "filter",
                "INPUT",
                &format!("-i {interface_name} -p {protocol} --dport {port_num} -j ACCEPT"),
            )
            .unwrap();
            ipt.insert(
                "filter",
                "OUTPUT",
                &format!("-o wg0 -p {protocol} --sport {port_num} -j DROP"),
                1,
            )
            .unwrap();
            ipt.append(
                "filter",
                "OUTPUT",
                &format!("-o {interface_name} -p {protocol} --sport {port_num} -j ACCEPT"),
            )
            .unwrap();
        }
    }

    println!("[INFO] iptables modified",);

    if let Ok(delay) = env::var("VPN_IP_CHECK_DELAY") {
        println!("[INFO] Delaying IP check by {delay} seconds");
        sleep(Duration::from_secs(delay.parse::<u64>().unwrap())).await;
    }

    let new_ip = reqwest::Client::new()
        .get("https://icanhazip.com")
        .send()
        .await?
        .text()
        .await?
        .trim_end()
        .to_owned();
    println!(
        "[INFO] Successfully connected to PIA\n----------------------------------------------------------------------\nOld IP: {}\nNew IP: {}\n----------------------------------------------------------------------",
        old_ip, new_ip
    );

    let vpn_state = VpnState {
        region_id: region_id.clone(),
        token: token.token.clone(),
        server_cn: server.cn.clone(),
        server_ip: server.ip,
        port: *port,
        conf_api: conf_api.clone(),
        forward_port,
        payload_port: None,
        sig: None,
    };

    // Channel pour les événements de health check
    let (tx, mut rx) = mpsc::channel::<bool>(10);

    // Health check task
    let health_check_ip = new_ip.clone();
    let health_tx = tx.clone();
    tokio::spawn(async move {
        let mut interval = interval(Duration::from_secs(HEALTH_CHECK_INTERVAL_SECS));
        loop {
            interval.tick().await;

            let wg_up = check_wireguard_interface().await;
            let vpn_ok = check_vpn_connection(&health_check_ip).await.unwrap_or(false);

            if !wg_up || !vpn_ok {
                println!("[ALERT] VPN connection issue detected!");
                println!("  - WireGuard up: {}", wg_up);
                println!("  - VPN IP correct: {}", vpn_ok);
                let _ = health_tx.send(false).await;
            } else {
                let _ = health_tx.send(true).await;
            }
        }
    });

    // Port forwarding task
    let mut current_ip = new_ip.clone();
    if forward_port {
        let persist_port = env::var("PERSIST_PORT")
            .unwrap_or_else(|_| "false".to_string())
            .parse::<bool>()
            .unwrap();

        println!("[INFO] Port forwarding is enabled");
        let api_client = reqwest::Client::builder()
            .resolve(&server.cn, format!("{}:19999", conf.api).parse().unwrap())
            .add_root_certificate(reqwest::Certificate::from_pem(&data)?)
            .build()?;

        let mut sig: Signature = {
            if persist_port && path::Path::new(&format!("{}/signature.json", CONFIG_PATH)).exists()
            {
                println!("[INFO] Persisted port is being used");
                serde_json::from_str(
                    &tokio::fs::read_to_string(format!("{}/signature.json", CONFIG_PATH))
                        .await
                        .unwrap(),
                )
                .unwrap()
            } else {
                println!("[INFO] New port signature is being fetched");
                api_client
                    .get(format!("https://{}:19999/getSignature", server.cn))
                    .query(&[("token", token.token)])
                    .send()
                    .await?
                    .json()
                    .await?
            }
        };

        if sig.status != "OK" {
            bail!("[ERROR] Failed to get signature: {}", sig.message.unwrap())
        }
        let mut payload: Payload =
            serde_json::from_str(std::str::from_utf8(&base64::decode(&sig.payload)?).unwrap())?;
        println!(
            "[INFO] PIA signature received. It expires at: {} UTC",
            payload.expires_at
        );

        if env::var("CONNECTION_FILE")
            .unwrap_or_else(|_| "false".to_string())
            .parse::<bool>()
            .unwrap()
        {
            tokio::fs::write(
                format!("{}/connection.json", CONFIG_PATH),
                serde_json::json!({
                    "port": payload.port,
                    "ip": current_ip
                })
                .to_string(),
            )
            .await
            .expect("[ERROR] failed to save connection data to file");
            println!(
                "[INFO] Connection data saved to {}/connection.json",
                CONFIG_PATH
            );
        }
        if env::var("PERSIST_PORT")
            .unwrap_or_else(|_| "false".to_string())
            .parse::<bool>()
            .unwrap()
        {
            tokio::fs::write(
                format!("{}/signature.json", CONFIG_PATH),
                serde_json::to_string(&sig).unwrap(),
            )
            .await
            .expect("[ERROR] failed to save port signature to file");
            println!(
                "[INFO] Port signature saved to {}/signature.json",
                CONFIG_PATH
            );
        }

        if env::var("GLUETUN_QBITTORRENT_PORT_MANAGER")
            .unwrap_or_else(|_| "false".to_string())
            .parse::<bool>()
            .unwrap()
        {
            let port_file_path = env::var("FORWARDED_PORT_FILE_PATH")
                .unwrap_or_else(|_| "/tmp/pia/forwarded_port".to_string());
            let port_dir = std::path::Path::new(&port_file_path).parent().unwrap_or(std::path::Path::new("/tmp"));
            tokio::fs::create_dir_all(port_dir).await.ok();
            tokio::fs::write(
                &port_file_path,
                payload.port.to_string(),
            )
            .await
            .expect("[ERROR] failed to write forwarded port file");
            println!("[INFO] Forwarded port written to {}", port_file_path);
        }

        println!("[INFO] Binding to port {}. Health monitoring active", payload.port);

        let mut reconnect_attempts = 0;

        loop {
            tokio::select! {
                // Port refresh every 15 minutes
                _ = sleep(Duration::from_secs(900)) => {
                    if payload.expires_at.timestamp() < chrono::Utc::now().timestamp() {
                        println!("[INFO] Port signature expired, fetching new signature");

                        let new_sig_response = api_client
                            .get(format!("https://{}:19999/getSignature", server.cn))
                            .query(&[("token", vpn_state.token.clone())])
                            .send()
                            .await
                            .ok();

                        let new_sig: Option<Signature> = match new_sig_response {
                            Some(response) => response.json().await.ok(),
                            None => None,
                        };

                        if let Some(new_sig) = new_sig {
                            if new_sig.status == "OK" {
                                sig = new_sig;
                                let new_payload: Payload =
                                    serde_json::from_str(std::str::from_utf8(&base64::decode(&sig.payload).unwrap()).unwrap()).unwrap();
                                payload = new_payload;
                                println!("[INFO] New signature received. Port: {}", payload.port);

                                // Update forwarded port file
                                if env::var("GLUETUN_QBITTORRENT_PORT_MANAGER")
                                    .unwrap_or_else(|_| "false".to_string())
                                    .parse::<bool>()
                                    .unwrap()
                                {
                                    tokio::fs::write(
                                        "/tmp/gluetun/forwarded_port",
                                        payload.port.to_string(),
                                    )
                                    .await.ok();
                                }

                                reconnect_attempts = 0;
                            }
                        }

                        if reconnect_attempts >= MAX_RECONNECT_ATTEMPTS {
                            println!("[ERROR] Max reconnection attempts reached, restarting container");
                            Command::new("/proc/self/exe").exec();
                            break;
                        }
                    }

                    let pf_bind_response = api_client
                        .get(format!("https://{}:19999/bindPort", server.cn))
                        .query(&[("payload", &sig.payload), ("signature", &sig.signature)])
                        .send()
                        .await
                        .ok();

                    let pf_bind: Option<BindPort> = match pf_bind_response {
                        Some(response) => response.json().await.ok(),
                        None => None,
                    };

                    match pf_bind {
                        Some(bind) if bind.status == "OK" => {
                            println!("[INFO] Port {} bound successfully", payload.port);
                            reconnect_attempts = 0;

                        }
                        Some(bind) => {
                            println!("[WARN] Failed to bind port: {}", bind.message);
                            reconnect_attempts += 1;
                        }
                        None => {
                            println!("[WARN] Could not reach PIA API for port binding");
                            reconnect_attempts += 1;
                        }
                    }
                }

                // Health check events
                healthy = rx.recv() => {
                    if healthy == Some(false) {
                        reconnect_attempts += 1;
                        println!("[WARN] Reconnect attempt {}/{}", reconnect_attempts, MAX_RECONNECT_ATTEMPTS);

                        if reconnect_attempts >= MAX_RECONNECT_ATTEMPTS {
                            println!("[ERROR] Max reconnection attempts reached, restarting container");
                            Command::new("/proc/self/exe").exec();
                            break;
                        }

                        match reconnect_vpn(&vpn_state).await {
                            Ok(ip) => {
                                current_ip = ip;
                                reconnect_attempts = 0;
                                println!("[INFO] VPN reconnected successfully");

                                // Update connection file if enabled
                                if env::var("CONNECTION_FILE")
                                    .unwrap_or_else(|_| "false".to_string())
                                    .parse::<bool>()
                                    .unwrap()
                                {
                                    tokio::fs::write(
                                        format!("{}/connection.json", CONFIG_PATH),
                                        serde_json::json!({
                                            "port": payload.port,
                                            "ip": current_ip
                                        })
                                        .to_string(),
                                    )
                                    .await.ok();
                                }
                            }
                            Err(e) => {
                                println!("[ERROR] Reconnection failed: {}", e);
                            }
                        }
                    }
                }
            }
        }
    } else {
        // No port forwarding - just health monitoring
        println!("[INFO] Health monitoring active (no port forwarding)");

        loop {
            if let Some(healthy) = rx.recv().await {
                if !healthy {
                    println!("[WARN] VPN connection issue, attempting reconnection...");

                    match reconnect_vpn(&vpn_state).await {
                        Ok(ip) => {
                            println!("[INFO] VPN reconnected successfully. New IP: {}", ip);
                        }
                        Err(e) => {
                            println!("[ERROR] Reconnection failed: {}", e);
                            println!("[INFO] Restarting container...");
                            Command::new("/proc/self/exe").exec();
                            break;
                        }
                    }
                }
            }
        }
    }

    Ok(())
}
