use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::Notify;
use websrt::ingest::SrtListenerService;
use websrt::StreamRegistry;

fn free_port() -> u16 {
    let socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let port = socket.local_addr().unwrap().port();
    drop(socket);
    port
}

async fn wait_until_alive(registry: &StreamRegistry, name: &str, timeout: Duration) -> bool {
    let start = Instant::now();
    loop {
        if registry.is_alive(name) {
            return true;
        }
        if start.elapsed() > timeout {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
#[ignore = "real SRT loopback — run locally: cargo test -- --ignored"]
async fn multi_publisher_accepts_distinct_streamids() {
    let registry = Arc::new(StreamRegistry::new(16, 128));
    let shutdown = Arc::new(Notify::new());
    let port = free_port();
    let listener = SrtListenerService::bind(
        format!("127.0.0.1:{port}"),
        Duration::from_millis(120),
        None,
    )
    .await
    .expect("bind");

    let serve_registry = registry.clone();
    let serve_shutdown = shutdown.clone();
    tokio::spawn(async move {
        listener
            .serve(serve_registry, serve_shutdown, |_name, conn| conn)
            .await;
    });

    tokio::time::sleep(Duration::from_millis(100)).await;

    let addr: srt_protocol::options::SocketAddress =
        format!("127.0.0.1:{port}").try_into().unwrap();
    let _pub_foo = srt_tokio::SrtSocket::builder()
        .latency(Duration::from_millis(120))
        .call(addr.clone(), Some("foo"))
        .await
        .expect("connect foo");
    let _pub_bar = srt_tokio::SrtSocket::builder()
        .latency(Duration::from_millis(120))
        .call(addr.clone(), Some("bar"))
        .await
        .expect("connect bar");

    assert!(
        wait_until_alive(&registry, "foo", Duration::from_secs(5)).await,
        "foo not alive"
    );
    assert!(
        wait_until_alive(&registry, "bar", Duration::from_secs(5)).await,
        "bar not alive"
    );

    assert!(registry.subscribe("foo").is_some(), "subscribe foo");
    assert!(registry.subscribe("bar").is_some(), "subscribe bar");

    let snap = registry.snapshot_streams();
    let names: Vec<&str> = snap.iter().map(|s| s.name.as_str()).collect();
    assert!(names.contains(&"foo"), "snapshot has foo");
    assert!(names.contains(&"bar"), "snapshot has bar");

    let _pub_foo2 = srt_tokio::SrtSocket::builder()
        .latency(Duration::from_millis(120))
        .call(addr, Some("foo"))
        .await
        .expect("connect foo2");

    tokio::time::sleep(Duration::from_millis(500)).await;

    assert!(registry.is_alive("foo"), "foo still alive after duplicate");

    let snap2 = registry.snapshot_streams();
    let alive_count = snap2.iter().filter(|s| s.alive).count();
    assert_eq!(alive_count, 2, "still exactly 2 alive streams after duplicate");

    shutdown.notify_one();
}
