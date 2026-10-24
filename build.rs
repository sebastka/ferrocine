//! Rebuild when a migration is added or changed: `sqlx::migrate!` embeds
//! them, and cargo does not watch that directory by itself.
fn main() {
    println!("cargo:rerun-if-changed=migrations");
}
