use rusqlite::Connection;

#[test]
fn bundled_sqlite_contains_the_wal_reset_fix() {
    let connection = Connection::open_in_memory().expect("open bundled SQLite");
    let version: String = connection
        .query_row("SELECT sqlite_version()", [], |row| row.get(0))
        .expect("read SQLite runtime version");
    eprintln!("bundled SQLite runtime: {version}");
    let parts = version
        .split('.')
        .map(|part| part.parse::<u32>().expect("numeric SQLite version part"))
        .collect::<Vec<_>>();
    assert!(parts.len() >= 3, "unexpected SQLite version: {version}");

    let release = (parts[0], parts[1], parts[2]);
    let fixed = release >= (3, 51, 3)
        || (release.0 == 3 && release.1 == 50 && release.2 >= 7)
        || (release.0 == 3 && release.1 == 44 && release.2 >= 6);

    assert!(
        fixed,
        "bundled SQLite {version} is vulnerable to the WAL-reset corruption race"
    );
}
