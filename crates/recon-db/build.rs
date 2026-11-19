// `sqlx::migrate!` reads ./migrations while it expands, but the expansion's dep
// graph reaches only the files it already saw — a NEW migration changes no input
// cargo tracks, so it will happily link a stale `recon-db` rlib. That bit for
// real: a suite build once ran a migrator missing 0010 (the database ledger had
// 1..=9 applied and no match_lines_by_line) while a `-p recon-db` build of the
// same tree had it. Declaring the directory makes any added or edited .sql
// rebuild the library — the only way the migrator can never go stale.
fn main() {
    println!("cargo:rerun-if-changed=migrations");
}
