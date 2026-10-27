//! WP-51 acceptance tests: schema migration from an empty database, the
//! secret sentinel round-trip, `base_rev` conflicts (409) and reads during a
//! write.

use std::os::unix::fs::MetadataExt;
use std::time::Duration;

use axum::http::StatusCode;
use kurin_server::{ApiError, Lifecycle, Store, StoreError, SECRET_SENTINEL};

/// A realistic v1alpha2 document with a secret in every supported slot:
/// cluster sudo password, cluster SSH password, registry password, and
/// node-level password / sudo password / key passphrase.
const CONFIG: &str = r#"version = "v1alpha2"
name = "tier1"

[ssh]
become-password = "sudo-fake-secret"

[ssh.auth]
method = "password"
password = "ssh-fake-secret"

[registry]
kind = "external"
address = "registry.example:5000"
password = "registry-fake-secret"

[nodes.cp1]
address = "10.0.0.11"
role = "control-plane"

[nodes.cp1.auth]
method = "password"
password = "node-fake-secret"

[nodes.worker1]
address = "10.0.0.12"
role = "worker"
become-password = "node-sudo-fake-secret"

[nodes.worker1.auth]
method = "private_key"
key-path = "/home/kurin/.ssh/id_ed25519"
passphrase = "node-passphrase-fake"
"#;

const SECRETS: [&str; 6] = [
    "sudo-fake-secret",
    "ssh-fake-secret",
    "registry-fake-secret",
    "node-fake-secret",
    "node-sudo-fake-secret",
    "node-passphrase-fake",
];

fn open() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Store::open(dir.path()).expect("store opens");
    (dir, store)
}

#[test]
fn migrates_an_empty_database_to_the_latest_schema() {
    let (dir, store) = open();

    let version: i32 = store
        .read(|conn| {
            conn.pragma_query_value(None, "user_version", |row| row.get(0))
                .map_err(StoreError::from)
        })
        .expect("user_version");
    assert_eq!(
        version, 4,
        "v1 + v1.1 log columns + the read-only index exemption + WP-54 \
         (operations.result, snapshots.addons, packages)"
    );

    let journal: String = store
        .read(|conn| {
            conn.pragma_query_value(None, "journal_mode", |row| row.get(0))
                .map_err(StoreError::from)
        })
        .expect("journal_mode");
    assert_eq!(journal.to_lowercase(), "wal");

    let tables: Vec<String> = store
        .read(|conn| {
            let mut statement =
                conn.prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")?;
            let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()
                .map_err(StoreError::from)
        })
        .expect("table list");
    for expected in [
        "clusters",
        "config_revisions",
        "operations",
        "op_events",
        "snapshots",
        "packages",
    ] {
        assert!(
            tables.iter().any(|table| table == expected),
            "missing {expected}: {tables:?}"
        );
    }

    let columns: Vec<String> = store
        .read(|conn| {
            let mut statement = conn.prepare("PRAGMA table_info(operations)")?;
            let rows = statement.query_map([], |row| row.get::<_, String>(1))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()
                .map_err(StoreError::from)
        })
        .expect("operations columns");
    assert!(columns.iter().any(|column| column == "log_path"));
    assert!(columns.iter().any(|column| column == "log_lines"));
    assert!(columns.iter().any(|column| column == "result"));

    // Reopening an already migrated database is a no-op and keeps data.
    store
        .register_cluster("tier1", Lifecycle::Draft)
        .expect("register");
    drop(store);
    let reopened = Store::open(dir.path()).expect("reopen");
    assert_eq!(
        reopened
            .get_cluster("tier1")
            .expect("lookup")
            .expect("present")
            .name,
        "tier1"
    );
    let version: i32 = reopened
        .read(|conn| {
            conn.pragma_query_value(None, "user_version", |row| row.get(0))
                .map_err(StoreError::from)
        })
        .expect("user_version after reopen");
    assert_eq!(version, 4);
}

#[test]
fn sentinel_round_trip_keeps_secrets_and_never_echoes_them() {
    let (dir, store) = open();
    let first = store
        .import_cluster("tier1", CONFIG, Some("import".into()))
        .expect("import");
    assert_eq!(first.rev, 1);
    assert_eq!(first.sha256.len(), 64);
    assert!(
        format!("{first:?}").contains("<redacted>"),
        "Debug must not print the body"
    );
    for secret in SECRETS {
        assert!(first.toml.contains(secret), "stored secret {secret}");
        assert!(
            !format!("{first:?}").contains(secret),
            "Debug leaked {secret}"
        );
    }

    // What the API returns: every literal secret is a sentinel.
    let redacted = store
        .read_config("tier1")
        .expect("read config")
        .expect("has config");
    for secret in SECRETS {
        assert!(!redacted.contains(secret), "echoed {secret}");
    }
    assert_eq!(redacted.matches(SECRET_SENTINEL).count(), SECRETS.len());

    // "Read, edit, write back": the sentinel must restore the stored value.
    let edited = redacted.replace("10.0.0.11", "10.0.0.99");
    let second = store.save_config("tier1", &edited, 1, None).expect("save");
    assert_eq!(second.rev, 2);
    for secret in SECRETS {
        assert!(second.toml.contains(secret), "lost {secret} on save");
    }
    let redacted_again = store
        .read_config("tier1")
        .expect("read config")
        .expect("has config");
    for secret in SECRETS {
        assert!(!redacted_again.contains(secret), "echoed {secret}");
    }
    assert_eq!(
        redacted_again.matches(SECRET_SENTINEL).count(),
        SECRETS.len()
    );
    assert!(redacted_again.contains("10.0.0.99"));

    // A literal secret in the submitted document replaces the old value.
    let rotated = redacted_again.replace(SECRET_SENTINEL, "rotated-fake-secret");
    let third = store
        .save_config("tier1", &rotated, 2, None)
        .expect("save rotated");
    assert_eq!(
        third.toml.matches("rotated-fake-secret").count(),
        SECRETS.len()
    );
    for secret in SECRETS {
        assert!(!third.toml.contains(secret), "old {secret} survived");
    }

    // The mirror is plaintext (the CLI needs it) but 0600 in a 0700 dir.
    let mirror = dir.path().join("clusters/tier1/cluster.toml");
    let text = std::fs::read_to_string(&mirror).expect("mirror exists");
    assert!(text.contains("rotated-fake-secret"));
    assert_eq!(
        std::fs::metadata(&mirror).expect("mirror metadata").mode() & 0o777,
        0o600
    );
    assert_eq!(
        std::fs::metadata(mirror.parent().expect("cluster dir"))
            .expect("dir metadata")
            .mode()
            & 0o777,
        0o700
    );

    // History is immutable and complete.
    let revisions = store.list_revisions("tier1").expect("history");
    assert_eq!(revisions.len(), 3);
    assert_eq!(revisions[0].rev, 3);
    assert_eq!(revisions[2].rev, 1);
    let original = store.get_revision("tier1", 1).expect("rev 1");
    for secret in SECRETS {
        assert!(original.toml.contains(secret));
    }
}

#[test]
fn sentinel_without_a_previous_value_is_rejected_and_rolled_back() {
    let (_dir, store) = open();
    let submitted = CONFIG.replace("ssh-fake-secret", SECRET_SENTINEL);
    let error = store
        .import_cluster("fresh", &submitted, None)
        .expect_err("no previous revision to restore from");
    assert!(
        matches!(error, StoreError::MissingSecret { .. }),
        "{error:?}"
    );
    assert_eq!(error.status_code(), 400);
    assert!(
        store.get_cluster("fresh").expect("lookup").is_none(),
        "the import transaction must roll back"
    );
}

#[test]
fn inline_table_secrets_are_redacted_too() {
    let (_dir, store) = open();
    let inline = r#"version = "v1alpha2"
name = "inline"
ssh = { auth = { method = "password", password = "inline-fake-secret" } }
"#;
    store
        .import_cluster("inline", inline, None)
        .expect("import inline");
    let redacted = store
        .read_config("inline")
        .expect("read")
        .expect("has config");
    assert!(!redacted.contains("inline-fake-secret"), "{redacted}");
    assert!(redacted.contains(SECRET_SENTINEL));

    let saved = store
        .save_config("inline", &redacted, 1, None)
        .expect("save inline");
    assert!(saved.toml.contains("inline-fake-secret"));
}

#[test]
fn stale_base_rev_is_a_conflict_and_leaves_history_untouched() {
    let (_dir, store) = open();
    store.import_cluster("tier1", CONFIG, None).expect("import");

    let error = store
        .save_config("tier1", CONFIG, 0, None)
        .expect_err("stale base_rev");
    match &error {
        StoreError::RevConflict {
            cluster,
            expected,
            current,
        } => {
            assert_eq!(cluster, "tier1");
            assert_eq!(*expected, 0);
            assert_eq!(*current, 1);
        }
        other => panic!("expected RevConflict, got {other:?}"),
    }
    assert_eq!(error.status_code(), 409);
    let api: ApiError = error.into();
    assert_eq!(api.status(), StatusCode::CONFLICT);

    let cluster = store
        .get_cluster("tier1")
        .expect("lookup")
        .expect("present");
    assert_eq!(cluster.current_rev, 1);
    assert_eq!(store.list_revisions("tier1").expect("history").len(), 1);

    // A duplicate name is also a 409.
    let duplicate = store
        .register_cluster("tier1", Lifecycle::Active)
        .expect_err("duplicate");
    assert!(matches!(duplicate, StoreError::AlreadyExists(_)));
    assert_eq!(duplicate.status_code(), 409);
}

#[test]
fn reads_are_not_blocked_by_an_open_write_transaction() {
    let (_dir, store) = open();
    store
        .register_cluster("existing", Lifecycle::Active)
        .expect("register");

    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let writer = {
        let store = store.clone();
        std::thread::spawn(move || {
            store
                .write(move |conn| {
                    let transaction = conn.transaction()?;
                    transaction.execute(
                        "INSERT INTO clusters \
                         (name, lifecycle, current_rev, created_at, updated_at) \
                         VALUES ('inflight', 'draft', 0, 't', 't')",
                        [],
                    )?;
                    entered_tx.send(()).expect("signal entered");
                    release_rx
                        .recv_timeout(Duration::from_secs(5))
                        .expect("release");
                    transaction.commit()?;
                    Ok(())
                })
                .expect("write");
        })
    };
    entered_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("writer entered the transaction");

    // Four readers on the pool while the write transaction is open: WAL
    // serves the pre-commit snapshot, and the uncommitted row is invisible.
    let mut readers = Vec::new();
    for _ in 0..4 {
        let store = store.clone();
        readers.push(std::thread::spawn(move || {
            store
                .read(|conn| {
                    let count: i64 =
                        conn.query_row("SELECT COUNT(*) FROM clusters", [], |row| row.get(0))?;
                    Ok(count)
                })
                .expect("read")
        }));
    }
    for reader in readers {
        assert_eq!(reader.join().expect("reader"), 1);
    }

    release_tx.send(()).expect("release writer");
    writer.join().expect("writer");

    let count: i64 = store
        .read(|conn| {
            conn.query_row("SELECT COUNT(*) FROM clusters", [], |row| row.get(0))
                .map_err(StoreError::from)
        })
        .expect("count after commit");
    assert_eq!(count, 2);
}
