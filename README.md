# Couchbase Rust SDK
A brand new `libcouchbase`-based binding for [Rust](https://www.rust-lang.org).

# Example

Run with `cargo run --example=helloworld`.

```rust
extern crate couchbase;
extern crate futures;

use couchbase::Cluster;
use futures::Future;

fn main() {
    // Initialize the Cluster
    let cluster = Cluster::new("localhost").expect("Could not initialize Cluster");

    // Open the travel-sample bucket
    let bucket = cluster.open_bucket("travel-sample", "").expect("Could not open Bucket");

    // Load an airline, wait for it to load and print it out
    let document = bucket.get("airline_10123").wait().expect("Could not load Document");
    println!("{:?}", document);
}
```
