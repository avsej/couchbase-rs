use couchbase::Cluster;
use serde_derive::Serialize;

#[derive(Debug, Serialize)]
struct Airport {
    airportname: String,
    icao: String,
    iata: String,
}

fn main() {
    let mut cluster = Cluster::connect("couchbase://127.0.0.1", "Administrator", "password")
        .expect("Could not create Cluster reference!");
    let bucket = cluster
        .bucket("travel-sample")
        .expect("Could not open bucket");
    let collection = bucket.default_collection();

    let airport = Airport {
        airportname: "Vienna Airport".into(),
        icao: "LOWW".into(),
        iata: "VIE".into(),
    };
    collection
        .upsert("airport_999", airport, None)
        .expect("could not upsert airport!");

    cluster.disconnect().expect("Failure while disconnecting!");
}