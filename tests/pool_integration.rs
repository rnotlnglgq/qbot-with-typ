use std::path::PathBuf;
use std::time::Duration;

use qbot_with_typ::worker::{PoolConfig, WorkerError, WorkerPool};

fn mock_worker_path() -> String {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("tests/fixtures/mock_worker.py");
    p.to_string_lossy().into_owned()
}

fn test_config(pool_size: usize) -> PoolConfig {
    PoolConfig {
        pool_size,
        render_timeout: Duration::from_secs(5),
        spawn_timeout: Duration::from_secs(10),
        memory_limit_bytes: 256 * 1024 * 1024,
        cpu_lifetime_secs: 60,
        fsize_limit_bytes: Some(0),
        extra_args: Vec::new(),
        worker_bin: mock_worker_path(),
    }
}

#[tokio::test]
async fn pool_spawn_and_render() {
    let pool = WorkerPool::new(test_config(1)).await.unwrap();
    let png = pool.render("hello world", "default").await.unwrap();
    assert!(!png.is_empty());
    // PNG magic bytes
    assert_eq!(&png[..4], b"\x89PNG");

    let stats = pool.stats().await;
    assert_eq!(stats.alive, 1);
    assert_eq!(stats.total_renders, 1);
    assert_eq!(stats.total_deaths, 0);

    pool.shutdown().await;
}

#[tokio::test]
async fn pool_concurrent_renders() {
    let pool = std::sync::Arc::new(WorkerPool::new(test_config(2)).await.unwrap());

    let mut handles = Vec::new();
    for i in 0..6 {
        let p = pool.clone();
        handles.push(tokio::spawn(async move {
            let source = format!("concurrent request {i}");
            p.render(&source, "default").await
        }));
    }

    for handle in handles {
        let result = handle.await.unwrap();
        assert!(result.is_ok(), "render failed: {:?}", result.err());
    }

    let stats = pool.stats().await;
    assert_eq!(stats.total_renders, 6);
    assert_eq!(stats.alive, 2);

    let Ok(pool) = std::sync::Arc::try_unwrap(pool) else {
        panic!("Arc still shared");
    };
    pool.shutdown().await;
}

#[tokio::test]
async fn pool_compile_error() {
    let pool = WorkerPool::new(test_config(1)).await.unwrap();

    let result = pool.render("__error__", "default").await;
    assert!(result.is_err());
    match result.unwrap_err() {
        WorkerError::Compile(msg) => {
            assert!(msg.contains("mock compile error"), "unexpected: {msg}");
        }
        other => panic!("expected Compile error, got: {other}"),
    }

    // Worker should still be alive after a compile error
    let stats = pool.stats().await;
    assert_eq!(stats.alive, 1);
    assert_eq!(stats.total_deaths, 0);

    // Next render should succeed
    let png = pool.render("ok after error", "default").await.unwrap();
    assert_eq!(&png[..4], b"\x89PNG");

    pool.shutdown().await;
}

#[tokio::test]
async fn pool_timeout_kills_and_respawns() {
    let mut config = test_config(1);
    config.render_timeout = Duration::from_secs(2);

    let pool = WorkerPool::new(config).await.unwrap();

    let result = pool.render("__timeout__", "default").await;
    assert!(matches!(result, Err(WorkerError::Timeout(_))));

    // Give the background respawn some time
    tokio::time::sleep(Duration::from_secs(3)).await;

    let stats = pool.stats().await;
    assert_eq!(stats.total_deaths, 1);
    assert_eq!(stats.alive, 1, "worker should have respawned: {stats}");

    // Should work again
    let png = pool.render("after timeout", "default").await.unwrap();
    assert_eq!(&png[..4], b"\x89PNG");

    pool.shutdown().await;
}

#[tokio::test]
async fn pool_crash_recovery() {
    let pool = WorkerPool::new(test_config(1)).await.unwrap();

    let result = pool.render("__crash__", "default").await;
    assert!(matches!(result, Err(WorkerError::Exited)));

    // Give the background respawn some time
    tokio::time::sleep(Duration::from_secs(3)).await;

    let stats = pool.stats().await;
    assert_eq!(stats.total_deaths, 1);
    assert_eq!(stats.alive, 1, "worker should have respawned: {stats}");

    // Should work again
    let png = pool.render("after crash", "default").await.unwrap();
    assert_eq!(&png[..4], b"\x89PNG");

    pool.shutdown().await;
}

#[tokio::test]
async fn pool_stats_display() {
    let pool = WorkerPool::new(test_config(2)).await.unwrap();
    let stats = pool.stats().await;
    let display = format!("{stats}");
    assert!(display.contains("2/2 alive"));
    assert!(display.contains("renders: 0"));
    pool.shutdown().await;
}
