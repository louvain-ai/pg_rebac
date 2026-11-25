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
