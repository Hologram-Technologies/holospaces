//! Print the κ-label (the substrate's content address) of each file argument —
//! `blake3:<hex>  <path>`. Used to pin imported artifacts by κ so a peer can
//! verify them by re-derivation on load (Law L5).
fn main() {
    for path in std::env::args().skip(1) {
        let bytes = std::fs::read(&path).expect("read file");
        println!("{}  {}", holospaces::address(&bytes).as_str(), path);
    }
}
