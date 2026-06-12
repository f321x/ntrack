//! Generate a fresh keypair: `cargo run -p ntrack-core --example genkey`
use ntrack_core::keys;

fn main() {
    let k = keys::generate();
    println!("hex_pub: {}", k.public_key().to_hex());
    println!("npub:    {}", keys::npub(&k.public_key()));
    // printed intentionally — this tool exists to hand out fresh keys
    println!("nsec:    {}", keys::nsec(&k).expose());
}
