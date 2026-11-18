//! Login and reset limits: per email and per client IP, with forwarded addresses believed only
//! from a trusted proxy, and limiter keys pruned once their limit has replenished.
mod common;

use std::net::IpAddr;

use axum::http::HeaderMap;
use common::*;
use reqwest::Method;
use server::auth::{Limiter, client_ip};

#[tokio::test]
async fn pruning_drops_replenished_keys() {
    // 600 a minute replenishes one attempt every 100 ms.
    let l = Limiter::new(600);
    for i in 0..500 {
        l.check(&format!("login:user{i}@example.org")).unwrap();
    }
    assert_eq!(l.len(), 500);
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    l.prune();
    assert!(l.is_empty(), "{} keys left", l.len());
}

#[test]
fn forwarded_addresses_count_only_from_a_trusted_proxy() {
    let mut h = HeaderMap::new();
    h.insert(
        "x-forwarded-for",
        "198.51.100.7, 203.0.113.9".parse().unwrap(),
    );
    let loopback: IpAddr = "127.0.0.1".parse().unwrap();
    let stranger: IpAddr = "192.0.2.50".parse().unwrap();
    assert_eq!(client_ip(loopback, &h, &[]).to_string(), "203.0.113.9");
    assert_eq!(
        client_ip(stranger, &h, &[]),
        stranger,
        "not a trusted proxy"
    );
    assert_eq!(
        client_ip(stranger, &h, &[stranger]).to_string(),
        "203.0.113.9"
    );
    h.insert("x-real-ip", "198.51.100.1".parse().unwrap());
    assert_eq!(client_ip(loopback, &h, &[]).to_string(), "198.51.100.1");
}

async fn login_from(srv: &server::testing::TestServer, i: usize, ip: &str) -> u16 {
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/auth/login", srv.base_url))
        .header("X-Real-IP", ip)
        .json(&serde_json::json!({"email": format!("nobody{i}@example.org"), "password": "x"}))
        .send()
        .await
        .unwrap();
    resp.status().as_u16()
}

#[tokio::test(flavor = "multi_thread")]
async fn one_client_ip_is_limited_across_emails() {
    let srv = server::testing::spawn(|c| c.auth_attempts_per_minute = 2).await;
    // Ten attempts per IP per minute, each at a different email.
    let statuses: Vec<u16> =
        futures::future::join_all((0..12).map(|i| login_from(&srv, i, "203.0.113.4"))).await;
    assert_eq!(
        statuses.iter().filter(|s| **s == 401).count(),
        10,
        "{statuses:?}"
    );
    assert_eq!(
        statuses.iter().filter(|s| **s == 429).count(),
        2,
        "{statuses:?}"
    );
    // Another client behind the same (trusted, local) proxy has its own allowance.
    assert_eq!(login_from(&srv, 99, "203.0.113.5").await, 401);
    let (s, _) = call(
        &srv,
        Method::POST,
        None,
        "/auth/reset",
        Some(serde_json::json!({"email": "x@example.org"})),
    )
    .await;
    assert_eq!(s, 202);
}

#[tokio::test(flavor = "multi_thread")]
async fn registration_is_limited_per_client_ip() {
    let srv = server::testing::spawn(|c| c.auth_attempts_per_minute = 2).await;
    let mut statuses = vec![];
    for i in 0..12 {
        let resp = reqwest::Client::new()
            .post(format!("{}/v1/auth/register", srv.base_url))
            .header("X-Real-IP", "203.0.113.8")
            .json(&serde_json::json!({"email": format!("r{i}@example.org"),
                "password": "correct horse battery", "display_name": "R"}))
            .send()
            .await
            .unwrap();
        statuses.push(resp.status().as_u16());
    }
    assert_eq!(
        statuses.iter().filter(|s| **s == 200).count(),
        10,
        "{statuses:?}"
    );
    assert_eq!(
        statuses.iter().filter(|s| **s == 429).count(),
        2,
        "{statuses:?}"
    );
}
