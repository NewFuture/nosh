#[path = "../../tools/source/guard.rs"]
mod source_guard;

fn main() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(std::path::Path::parent)
        .expect("nosh-shell is inside crates");
    source_guard::check(root).unwrap_or_else(|error| panic!("{error}"));
}
