// Records which ever-node the binary is built against (version and commit, from Cargo.lock),
// so `--version` can tell a validator whether the build matches their node.
fn main() {
    println!("cargo:rerun-if-changed=Cargo.lock");
    let lock = std::fs::read_to_string("Cargo.lock").unwrap_or_default();
    let (mut version, mut commit) = ("unknown".to_owned(), "unknown".to_owned());
    for package in lock.split("[[package]]") {
        if !package.contains("\nname = \"ever-node\"\n") {
            continue;
        }
        for line in package.lines() {
            if let Some(value) = line.strip_prefix("version = \"") {
                version = value.trim_end_matches('"').to_owned();
            }
            if let Some(source) = line.strip_prefix("source = \"") {
                if let Some((_, hash)) = source.trim_end_matches('"').rsplit_once('#') {
                    commit = hash.to_owned();
                }
            }
        }
    }
    println!("cargo:rustc-env=EVER_NODE_VERSION={version}");
    println!("cargo:rustc-env=EVER_NODE_COMMIT={commit}");
}
