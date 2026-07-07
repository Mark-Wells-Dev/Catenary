fn main() {
    // Intentional diagnostic: E0308 mismatched types — a &str where a u32 is
    // declared. rust-analyzer publishes this from native type-check, no flycheck
    // required.
    let _answer: u32 = "not a number";
}
