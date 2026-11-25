//! TLS to Postgres by `sslmode`, against a real TLS server (`tests/tls/up.sh`). Runs only with `REBAC_TLS_IT=1` and
//! `REBAC_TLS_CA` (the CA the script prints), and says SKIPPED loudly otherwise.

use rebacd::db::pool;

fn ca() -> Option<String> {
    if std::env::var("REBAC_TLS_IT").as_deref() != Ok("1") {
        eprintln!("!!!!!!!! SKIPPED: the TLS test needs REBAC_TLS_IT=1 and REBAC_TLS_CA (tests/tls/up.sh)");
        return None;
    }
    Some(std::env::var("REBAC_TLS_CA").expect("REBAC_TLS_CA"))
}

/// Connect, and say whether the session is encrypted.
async fn encrypted(url: &str) -> anyhow::Result<bool> {
    let p = pool(url, 1)?;
    let c = p.get().await?;
    Ok(c.query_one(
        "select ssl from pg_stat_ssl where pid = pg_backend_pid()",
        &[],
    )
    .await?
    .get(0))
}

#[tokio::test]
async fn sslmode_is_honoured() {
    let Some(ca) = ca() else { return };
    let base = "postgres://postgres:rebac@HOST:55434/postgres";
    let at = |host: &str, q: &str| format!("{}?{q}", base.replace("HOST", host));

    // verify-full: the chain and the name are checked.
    assert!(encrypted(&at(
        "localhost",
        &format!("sslmode=verify-full&sslrootcert={ca}")
    ))
    .await
    .unwrap());
    let wrong_name = encrypted(&at(
        "127.0.0.1",
        &format!("sslmode=verify-full&sslrootcert={ca}"),
    ))
    .await;
    assert!(
        wrong_name.is_err(),
        "a certificate for localhost does not name 127.0.0.1"
    );
    let unknown_ca = encrypted(&at("localhost", "sslmode=verify-full")).await;
    assert!(
        unknown_ca.is_err(),
        "a private CA is not among the public roots"
    );

    // require: encrypted, nothing verified (libpq's meaning).
    assert!(encrypted(&at("127.0.0.1", "sslmode=require"))
        .await
        .unwrap());
    // prefer, the default, takes TLS when it is offered.
    assert!(encrypted(&at("127.0.0.1", "application_name=rebacd"))
        .await
        .unwrap());
    // disable: plaintext.
    assert!(!encrypted(&at("127.0.0.1", "sslmode=disable"))
        .await
        .unwrap());
}

/// TLS to SpiceDB, against a SpiceDB serving `--grpc-tls-*` with a certificate for `localhost` from the CA in
/// `REBAC_SPICEDB_TLS_CA`, at `REBAC_SPICEDB_TLS_URL` (`https://localhost:<port>`). Runs only with `REBAC_TLS_IT=1`.
#[tokio::test]
async fn spicedb_over_tls_verifies_the_server() {
    use rebacd::{
        config::Consistency,
        spicedb::{SpiceDb, Transport},
    };
    use std::time::Duration;
    if std::env::var("REBAC_TLS_IT").as_deref() != Ok("1") {
        eprintln!("!!!!!!!! SKIPPED: the SpiceDB TLS test needs REBAC_TLS_IT=1, REBAC_SPICEDB_TLS_URL and REBAC_SPICEDB_TLS_CA");
        return;
    }
    let url = std::env::var("REBAC_SPICEDB_TLS_URL").expect("REBAC_SPICEDB_TLS_URL");
    let ca = std::env::var("REBAC_SPICEDB_TLS_CA").expect("REBAC_SPICEDB_TLS_CA");
    let key = "rebac-test-key";
    let trusted = Transport {
        ca_file: Some(ca.into()),
        insecure: false,
    };
    let s = SpiceDb::connect_with(&url, key, Consistency::FullyConsistent, &trusted)
        .await
        .unwrap();
    s.write_schema("definition user {}", Duration::from_secs(5))
        .await
        .expect("the CA's certificate is trusted, and the key is accepted");
    // The public roots do not know a private CA: the handshake fails, and no key is sent.
    let untrusted = SpiceDb::connect(&url, key, Consistency::FullyConsistent)
        .await
        .unwrap();
    assert!(untrusted.read_schema(Duration::from_secs(5)).await.is_err());
    // Plaintext to a public address is refused before anything connects.
    let e = SpiceDb::connect(
        "http://203.0.113.7:50051",
        key,
        Consistency::FullyConsistent,
    )
    .await
    .err()
    .expect("refused");
    assert!(e.to_string().contains("public address"), "{e}");
}
